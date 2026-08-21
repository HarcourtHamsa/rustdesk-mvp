//! Hosting logic shared between the two GUI-ish front ends: `gui.rs` (the
//! visible window with a Start/Stop button) and `tray.rs` (the default,
//! auto-starting, tray-icon-only mode). Neither of those files talks to
//! sockets or ngrok directly - they both just call `start_hosting()` here
//! and render/react to the result.

use rd_protocol::{hash_password, DEFAULT_PORT};
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Live status of the accept loop, shared with whichever UI so it can show
/// "waiting" vs "connected: <peer>" without polling the socket itself.
#[derive(Default)]
pub struct Status {
    pub connected_peer: Option<String>,
}

/// Decodes the shared branding logo (baked in at compile time), letterboxes
/// it onto a transparent square canvas so the whole mark survives instead
/// of being cropped, and resizes to `size`x`size`. Used for both the
/// window icon (`gui.rs`) and the tray icon (`tray.rs`), so it lives here
/// rather than duplicated in both.
///
/// Returns `None` only if the embedded PNG somehow fails to decode - it's
/// baked in at compile time, so that would mean the file itself is corrupt.
pub fn load_logo_rgba(size: u32) -> Option<Vec<u8>> {
    let bytes = include_bytes!("../../assets/logo.png");
    let img = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    let side = w.max(h);
    let mut canvas = image::RgbaImage::new(side, side);
    image::imageops::overlay(&mut canvas, &img, ((side - w) / 2) as i64, ((side - h) / 2) as i64);
    let resized = image::imageops::resize(&canvas, size, size, image::imageops::FilterType::Lanczos3);
    Some(resized.into_raw())
}

/// How the ngrok tunnel is coming along, polled from its local API.
pub enum NgrokState {
    Starting,
    Ready(String),
    Unavailable(String),
}

/// Handles to the background hosting thread, kept around so powering off
/// can actually stop things rather than just changing what the UI shows.
pub struct HostControl {
    pub running: Arc<AtomicBool>,
    /// A clone of whatever client socket is currently being served, if any -
    /// powering off shuts this down so an in-progress session actually ends
    /// immediately instead of running until the client happens to disconnect.
    pub active_stream: Arc<Mutex<Option<TcpStream>>>,
    pub status: Arc<Mutex<Status>>,
    pub ngrok: Arc<Mutex<NgrokState>>,
    /// The running `ngrok` child process, if it started successfully - kept
    /// so powering off can kill it instead of leaving it running orphaned.
    pub ngrok_child: Arc<Mutex<Option<Child>>>,
}

/// Start listening on the default port with a fresh random password, and
/// spawn the background thread that accepts connections. Mirrors `rd-host
/// listen`'s behavior, just wired up to report status back to a UI instead
/// of printing to a terminal.
pub fn start_hosting() -> std::io::Result<(String, String, HostControl)> {
    let password = crate::generate_random_password();
    let password_hash = hash_password(&password);

    let listener = TcpListener::bind(("0.0.0.0", DEFAULT_PORT))?;
    listener.set_nonblocking(true)?;

    let local_ip = detect_local_ip().unwrap_or_else(|| "<this machine's IP>".to_string());
    let address = format!("{local_ip}:{DEFAULT_PORT}");

    let running = Arc::new(AtomicBool::new(true));
    let active_stream: Arc<Mutex<Option<TcpStream>>> = Arc::new(Mutex::new(None));
    let status = Arc::new(Mutex::new(Status::default()));
    let ngrok = Arc::new(Mutex::new(NgrokState::Starting));
    let ngrok_child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));

    spawn_ngrok(DEFAULT_PORT, ngrok.clone(), ngrok_child.clone(), running.clone());

    let running_t = running.clone();
    let active_stream_t = active_stream.clone();
    let status_t = status.clone();
    std::thread::spawn(move || {
        while running_t.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, addr)) => {
                    // On Windows, a socket returned by accept() inherits the
                    // *listener's* non-blocking flag - without resetting it
                    // here, every read/write in handle_client (which assume
                    // a blocking socket) fails immediately with WouldBlock.
                    if let Err(e) = stream.set_nonblocking(false) {
                        eprintln!("Failed to reset accepted socket to blocking mode: {e}");
                        continue;
                    }

                    // Keep a clone so the UI thread can force this session
                    // closed on power-off, even while `handle_client` below
                    // is blocked reading from it on this thread.
                    match stream.try_clone() {
                        Ok(clone) => *active_stream_t.lock().unwrap() = Some(clone),
                        Err(_) => continue,
                    }
                    status_t.lock().unwrap().connected_peer = Some(addr.to_string());

                    if let Err(e) = crate::handle_client(stream, &password_hash) {
                        eprintln!("Session ended: {e}");
                    }

                    status_t.lock().unwrap().connected_peer = None;
                    *active_stream_t.lock().unwrap() = None;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(_) => break,
            }
        }
    });

    Ok((
        address,
        password,
        HostControl { running, active_stream, status, ngrok, ngrok_child },
    ))
}

/// Stop accepting new connections, forcibly end whatever session is
/// currently in progress, and kill the ngrok tunnel - so "off" is an
/// immediate, complete stop rather than something that trails off in the
/// background.
pub fn stop_hosting(control: &HostControl) {
    control.running.store(false, Ordering::Relaxed);
    if let Some(stream) = control.active_stream.lock().unwrap().take() {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    if let Some(mut child) = control.ngrok_child.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Launch `ngrok tcp <port>` and poll its local API (the same one the
/// `ngrok` CLI itself uses to report the tunnel) until the public address
/// shows up, updating `ngrok_state` as it goes. ngrok can take anywhere
/// from a couple seconds to (per one real observed run) over a minute to
/// establish a session, so this is generous about how long it waits rather
/// than failing fast.
fn spawn_ngrok(
    port: u16,
    ngrok_state: Arc<Mutex<NgrokState>>,
    ngrok_child: Arc<Mutex<Option<Child>>>,
    running: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let child = Command::new("ngrok")
            .args(["tcp", &port.to_string(), "--log=stdout"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        let child = match child {
            Ok(c) => c,
            Err(e) => {
                let msg = if e.kind() == std::io::ErrorKind::NotFound {
                    "ngrok not found - install it from ngrok.com to tunnel from anywhere".to_string()
                } else {
                    format!("failed to start ngrok: {e}")
                };
                *ngrok_state.lock().unwrap() = NgrokState::Unavailable(msg);
                return;
            }
        };
        *ngrok_child.lock().unwrap() = Some(child);

        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if !running.load(Ordering::Relaxed) {
                return;
            }
            if let Some(addr) = query_ngrok_tunnel() {
                *ngrok_state.lock().unwrap() = NgrokState::Ready(addr);
                return;
            }
            if Instant::now() > deadline {
                *ngrok_state.lock().unwrap() =
                    NgrokState::Unavailable("ngrok tunnel didn't start in time".to_string());
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}

/// Ask ngrok's local API (started alongside the `ngrok` CLI on
/// 127.0.0.1:4040) for the public address of the tunnel we just asked it to
/// open. This is the same source of truth the `ngrok` CLI's own log line
/// comes from - just read directly instead of scraping stdout text.
fn query_ngrok_tunnel() -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", 4040)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
    stream
        .write_all(b"GET /api/tunnels HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut body = String::new();
    stream.read_to_string(&mut body).ok()?;

    let needle = "\"public_url\":\"tcp://";
    let start = body.find(needle)? + needle.len();
    let rest = &body[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Best-effort guess at the address other machines on this network would
/// use to reach us: the local IP of whichever interface routes outbound
/// traffic. This doesn't send any packets - `UdpSocket::connect` on a
/// connectionless socket just picks a route and records the local endpoint.
fn detect_local_ip() -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|addr| addr.ip().to_string())
}

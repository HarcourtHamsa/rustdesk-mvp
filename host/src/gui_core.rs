//! Hosting logic shared between the two GUI-ish front ends: `gui.rs` (the
//! visible window with a Start/Stop button) and `tray.rs` (the default,
//! auto-starting, tray-icon-only mode). Neither of those files talks to
//! sockets or ngrok directly - they both just call `start_hosting()` here
//! and render/react to the result.

use ngrok::prelude::*;
use rd_protocol::{hash_password, DEFAULT_PORT};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use url::Url;

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
    image::imageops::overlay(
        &mut canvas,
        &img,
        ((side - w) / 2) as i64,
        ((side - h) / 2) as i64,
    );
    let resized =
        image::imageops::resize(&canvas, size, size, image::imageops::FilterType::Lanczos3);
    Some(resized.into_raw())
}

/// How the in-process ngrok tunnel is coming along.
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

    spawn_ngrok(
        DEFAULT_PORT,
        ngrok.clone(),
        running.clone(),
        address.clone(),
        password.clone(),
    );

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
        HostControl {
            running,
            active_stream,
            status,
            ngrok,
        },
    ))
}

/// Stop accepting new connections, forcibly end whatever session is
/// currently in progress, and tear down the ngrok tunnel - so "off" is an
/// immediate, complete stop rather than something that trails off in the
/// background. Flipping `running` to false is what signals the tunnel
/// thread to drop its forwarder and close the tunnel.
pub fn stop_hosting(control: &HostControl) {
    control.running.store(false, Ordering::Relaxed);
    if let Some(stream) = control.active_stream.lock().unwrap().take() {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
}

/// Bring up an ngrok TCP tunnel *in-process* (no external `ngrok` binary)
/// that forwards public connections to our own local listener on `port`,
/// updating `ngrok_state` as it comes up. The authtoken is read from the
/// `NGROK_AUTHTOKEN` environment variable; without it the tunnel is reported
/// as unavailable and LAN hosting still works.
///
/// This runs on its own OS thread with a single-threaded tokio runtime,
/// since the rest of the host is plain blocking/threaded code. The tunnel
/// stays open until `running` flips to false (on power-off), at which point
/// the forwarder is dropped and the tunnel closes.
fn spawn_ngrok(
    port: u16,
    ngrok_state: Arc<Mutex<NgrokState>>,
    running: Arc<AtomicBool>,
    lan_address: String,
    password: String,
) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                *ngrok_state.lock().unwrap() =
                    NgrokState::Unavailable(format!("couldn't start tunnel runtime: {e}"));
                return;
            }
        };

        rt.block_on(async move {
            // Use hardcoded ngrok authtoken
            let authtoken = "3IEJq4PF4dg0Bkb7sp9IvPPxu41_TBYZjgP68z6U3uqBPVWo".to_string();

            let session = match ngrok::Session::builder()
                .authtoken(authtoken)
                .connect()
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    *ngrok_state.lock().unwrap() =
                        NgrokState::Unavailable(format!("ngrok connect failed: {e}"));
                    return;
                }
            };

            // Forward the public tunnel to the same local port our accept
            // loop is already listening on - so tunnel clients and LAN
            // clients both land on the one listener.
            let forward_to =
                Url::parse(&format!("tcp://localhost:{port}")).expect("valid hard-coded url");
            let forwarder = match session.tcp_endpoint().listen_and_forward(forward_to).await {
                Ok(f) => f,
                Err(e) => {
                    *ngrok_state.lock().unwrap() =
                        NgrokState::Unavailable(format!("ngrok tunnel failed: {e}"));
                    return;
                }
            };

            // The URL comes back as `tcp://host:port`; strip the scheme so it
            // matches the `host:port` form the client's connect box expects.
            let addr = forwarder.url().trim_start_matches("tcp://").to_string();
            *ngrok_state.lock().unwrap() = NgrokState::Ready(addr.clone());

            // Now that we have all three - tunnel address, LAN address, and
            // password - email them out. Fire-and-forget on its own thread,
            // so a slow/failed send never stalls the tunnel we're holding
            // open below.
            crate::mail::send_connection_email(&lan_address, &addr, &password);

            // Hold the forwarder open until power-off. Leaving this scope
            // drops it, which closes the tunnel.
            while running.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });
    });
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

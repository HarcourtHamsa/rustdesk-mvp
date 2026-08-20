//! The GUI entry point for `rd-host`: a small window with a power button.
//!
//! Click it to start listening for a connection (same as `rd-host listen`
//! under the hood, on the default port) - the window then shows the
//! address and password to give whoever's connecting, each with a copy
//! button. Click the power button again to stop.
//!
//! This is purely additive - `rd-host listen`/`relay`/`with-relay` still
//! work exactly as before for scripted or headless use (e.g. running
//! `rd-relay`/`rd-host relay` on a VPS with no display). The GUI is just
//! what you get when you run `rd-host` with no arguments.

use eframe::egui;
use rd_protocol::{hash_password, DEFAULT_PORT};
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([360.0, 440.0])
            .with_min_inner_size([320.0, 400.0])
            .with_resizable(false),
        ..Default::default()
    };
    eframe::run_native(
        "rd-host",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(App::default()))
        }),
    )
}

/// Live status of the accept loop, shared with the UI thread so it can show
/// "waiting" vs "connected: <peer>" without polling the socket itself.
#[derive(Default)]
struct Status {
    connected_peer: Option<String>,
}

/// How the ngrok tunnel is coming along, polled from its local API.
enum NgrokState {
    Starting,
    Ready(String),
    Unavailable(String),
}

/// Handles to the background hosting thread, kept around so powering off
/// can actually stop things rather than just changing what the window shows.
struct HostControl {
    running: Arc<AtomicBool>,
    /// A clone of whatever client socket is currently being served, if any -
    /// powering off shuts this down so an in-progress session actually ends
    /// immediately instead of running until the client happens to disconnect.
    active_stream: Arc<Mutex<Option<TcpStream>>>,
    status: Arc<Mutex<Status>>,
    ngrok: Arc<Mutex<NgrokState>>,
    /// The running `ngrok` child process, if it started successfully - kept
    /// so powering off can kill it instead of leaving it running orphaned.
    ngrok_child: Arc<Mutex<Option<Child>>>,
}

enum PowerState {
    Off,
    On {
        address: String,
        password: String,
        control: HostControl,
    },
}

struct App {
    state: PowerState,
    error: Option<String>,
}

impl Default for App {
    fn default() -> Self {
        Self { state: PowerState::Off, error: None }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        egui::CentralPanel::default().show(ui, |ui| match &mut self.state {
            PowerState::Off => {
                if show_off(ui, &self.error) {
                    match start_hosting() {
                        Ok((address, password, control)) => {
                            self.error = None;
                            self.state = PowerState::On { address, password, control };
                        }
                        Err(e) => self.error = Some(e.to_string()),
                    }
                }
            }
            PowerState::On { address, password, control } => {
                if show_on(ui, address, password, control) {
                    stop_hosting(control);
                    self.state = PowerState::Off;
                }
            }
        });

        // Cheap to redraw a few times a second - keeps "waiting..." vs
        // "connected: ..." current without needing input to refresh.
        ctx.request_repaint_after(Duration::from_millis(200));
    }
}

fn show_off(ui: &mut egui::Ui, error: &Option<String>) -> bool {
    let mut clicked = false;
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() / 2.0 - 100.0);
        ui.heading("rd-host");
        ui.add_space(28.0);

        // A literal power-symbol glyph isn't in egui's bundled font (renders
        // as an empty box), so this spells out what clicking it does instead.
        let button = egui::Button::new(egui::RichText::new("START").size(20.0).strong())
            .min_size(egui::vec2(120.0, 120.0))
            .corner_radius(60.0);
        if ui.add(button).on_hover_text("Start hosting").clicked() {
            clicked = true;
        }

        ui.add_space(16.0);
        ui.label(egui::RichText::new("Click to start hosting").weak());

        if let Some(e) = error {
            ui.add_space(12.0);
            ui.colored_label(egui::Color32::LIGHT_RED, e);
        }
    });
    clicked
}

/// Returns true if the power button was clicked (caller should stop hosting).
fn show_on(ui: &mut egui::Ui, address: &str, password: &str, control: &HostControl) -> bool {
    let mut turn_off = false;
    let peer = control.status.lock().unwrap().connected_peer.clone();

    ui.vertical_centered(|ui| {
        ui.add_space(20.0);
        ui.heading("rd-host");
        ui.add_space(20.0);

        let button = egui::Button::new(
            egui::RichText::new("STOP").size(16.0).strong().color(egui::Color32::from_rgb(90, 220, 120)),
        )
        .min_size(egui::vec2(84.0, 84.0))
        .corner_radius(42.0);
        if ui.add(button).on_hover_text("Stop hosting").clicked() {
            turn_off = true;
        }

        ui.add_space(14.0);
        match &peer {
            Some(p) => {
                ui.colored_label(egui::Color32::from_rgb(90, 220, 120), format!("Connected: {p}"));
            }
            None => {
                ui.label(egui::RichText::new("Waiting for a connection...").weak());
            }
        }

        ui.add_space(24.0);
        copy_row(ui, "LAN address", address);

        ui.add_space(10.0);
        match &*control.ngrok.lock().unwrap() {
            NgrokState::Starting => {
                ui.horizontal(|ui| {
                    ui.add_sized([72.0, 20.0], egui::Label::new("Tunnel"));
                    ui.label(egui::RichText::new("starting ngrok tunnel...").weak());
                });
            }
            NgrokState::Ready(tunnel_addr) => copy_row(ui, "Tunnel", tunnel_addr),
            NgrokState::Unavailable(reason) => {
                ui.horizontal(|ui| {
                    ui.add_sized([72.0, 20.0], egui::Label::new("Tunnel"));
                    ui.label(egui::RichText::new(reason).weak().small());
                });
            }
        }

        ui.add_space(10.0);
        copy_row(ui, "Password", password);

        ui.add_space(20.0);
        ui.label(
            egui::RichText::new(
                "LAN address only works on this network. The tunnel address\nworks from anywhere, once ngrok finishes starting.",
            )
            .weak()
            .small(),
        );
    });

    turn_off
}

/// One labeled, monospaced, copyable value - used for both Address and Password.
fn copy_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.add_sized([72.0, 20.0], egui::Label::new(label));
        ui.add(
            egui::Label::new(egui::RichText::new(value).monospace())
                .selectable(true),
        );
        if ui.button("Copy").on_hover_text(format!("Copy {label}")).clicked() {
            ui.ctx().copy_text(value.to_string());
        }
    });
}

/// Start listening on the default port with a fresh random password, and
/// spawn the background thread that accepts connections. Mirrors `rd-host
/// listen`'s behavior, just wired up to report status back to the GUI
/// instead of printing to a terminal.
fn start_hosting() -> std::io::Result<(String, String, HostControl)> {
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
/// currently in progress, and kill the ngrok tunnel - so the power button
/// reads as an immediate, complete "off" rather than something that trails
/// off in the background.
fn stop_hosting(control: &HostControl) {
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

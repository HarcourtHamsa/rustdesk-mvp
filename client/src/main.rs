//! rd-client: run this on the machine you have with you (e.g. a borrowed
//! laptop while traveling). It's a small GUI app - point it at a host's
//! address and password, and it shows you the remote screen and forwards
//! your mouse/keyboard.

use eframe::egui;
use rd_protocol::{hash_password, read_message, write_message, ClientMessage, HostMessage, MouseButton};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([640.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "rd-client",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(App::default()))
        }),
    )
}

/// A decoded frame, shared between the network receiver thread and the UI
/// thread. `version` is bumped on every new frame so the UI only re-uploads
/// the texture when there's actually something new, instead of every repaint.
struct SharedFrame {
    width: usize,
    height: usize,
    rgb: Vec<u8>,
    version: u64,
}

/// State of the host-side "blank host screen" privacy toggle, updated by
/// the receiver thread whenever a `BlankScreenResult` arrives, read by the
/// UI thread to draw the toggle button.
struct BlankScreenStatus {
    enabled: bool,
    /// True from the moment we send a toggle request until the host's
    /// reply arrives - the button disables itself meanwhile so a slow
    /// reply can't be raced by a second click.
    pending: bool,
    error: Option<String>,
}

/// Everything that exists only while connected: the socket, the frame feed,
/// and the background thread reading it.
struct Session {
    stream: TcpStream,
    remote_w: usize,
    remote_h: usize,
    shared: Arc<Mutex<SharedFrame>>,
    blank_screen: Arc<Mutex<BlankScreenStatus>>,
    running: Arc<AtomicBool>,
    receiver: Option<std::thread::JoinHandle<()>>,
    texture: Option<egui::TextureHandle>,
    uploaded_version: u64,
    last_mouse: Option<(i32, i32)>,
    mouse_down: [bool; 3], // left, right, middle
    keys_down: std::collections::HashSet<egui::Key>,
    modifiers_down: egui::Modifiers,
    error: Option<String>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        let _ = write_message(&mut self.stream, &ClientMessage::Disconnect);
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        if let Some(h) = self.receiver.take() {
            let _ = h.join();
        }
    }
}

/// Result of a background connection attempt, sent back to the UI thread.
enum ConnectOutcome {
    Ok(Session),
    Err(String),
}

enum View {
    Form,
    Connecting,
    Connected(Session),
}

struct App {
    view: View,
    address: String,
    password: String,
    show_password: bool,
    error: Option<String>,
    connect_rx: Option<mpsc::Receiver<ConnectOutcome>>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            view: View::Form,
            address: String::new(),
            password: String::new(),
            show_password: false,
            error: None,
            connect_rx: None,
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // egui::Context is a cheap Arc handle - clone it once so it can be
        // used freely below without fighting the `ui: &mut Ui` borrow.
        let ctx = ui.ctx().clone();

        // Poll for a finished connection attempt, if one's in flight.
        if let Some(rx) = &self.connect_rx {
            if let Ok(outcome) = rx.try_recv() {
                self.connect_rx = None;
                match outcome {
                    ConnectOutcome::Ok(session) => {
                        self.error = None;
                        self.view = View::Connected(session);
                    }
                    ConnectOutcome::Err(e) => {
                        self.error = Some(e);
                        self.view = View::Form;
                    }
                }
            }
        }

        // Empty sidebar, reserved for future use.
        egui::Panel::left("sidebar")
            .resizable(false)
            .exact_size(220.0)
            .show(ui, |_ui| {});

        egui::CentralPanel::default().show(ui, |ui| match &mut self.view {
            View::Form | View::Connecting => {
                let connecting = matches!(self.view, View::Connecting);
                let clicked_connect = show_connect_form(
                    ui,
                    &mut self.address,
                    &mut self.password,
                    &mut self.show_password,
                    connecting,
                    &self.error,
                );
                if clicked_connect {
                    let (tx, rx) = mpsc::channel();
                    self.connect_rx = Some(rx);
                    spawn_connect(sanitize_address(&self.address), self.password.trim().to_string(), tx);
                    self.view = View::Connecting;
                }
            }
            View::Connected(session) => {
                if !show_connected(ui, &ctx, session) {
                    self.view = View::Form;
                }
            }
        });

        // Keep polling for network frames / the connect result even with no
        // user input.
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }
}

/// Draws the connect form. Returns true the frame the user clicks Connect
/// with valid input.
fn show_connect_form(
    ui: &mut egui::Ui,
    address: &mut String,
    password: &mut String,
    show_password: &mut bool,
    connecting: bool,
    error: &Option<String>,
) -> bool {
    let mut clicked_connect = false;
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() / 2.0 - 90.0);
        ui.heading("rd-client");
        ui.add_space(16.0);

        // `egui::Grid` doesn't behave like a single sized widget the way
        // `heading`/`button` do, so `vertical_centered` alone won't center
        // it - it anchors to the panel's left edge instead. Wrapping it in
        // a fixed-size `allocate_ui` gives it a known size to be centered
        // as a block, same as everything else in this column.
        ui.allocate_ui(egui::vec2(390.0, 80.0), |ui| {
            egui::Grid::new("connect_form")
                .num_columns(2)
                .spacing([8.0, 10.0])
                .show(ui, |ui| {
                    ui.label("Address");
                    ui.add_enabled(
                        !connecting,
                        egui::TextEdit::singleline(address)
                            .hint_text("host:port, e.g. 2.tcp.eu.ngrok.io:18802")
                            .desired_width(300.0),
                    );
                    ui.end_row();

                    ui.label("Password");
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        ui.add_enabled(
                            !connecting,
                            egui::TextEdit::singleline(password)
                                .password(!*show_password)
                                .desired_width(250.0),
                        );
                        // Text label, not an eye emoji - egui's bundled font
                        // doesn't have emoji glyphs, so those render as an
                        // empty box instead of an icon.
                        let label = if *show_password { "Hide" } else { "Show" };
                        if ui
                            .add_enabled(!connecting, egui::Button::new(label).min_size([44.0, 20.0].into()))
                            .on_hover_text(if *show_password { "Hide password" } else { "Show password" })
                            .clicked()
                        {
                            *show_password = !*show_password;
                        }
                    });
                    ui.end_row();
                });
        });

        ui.add_space(12.0);

        let can_connect = !connecting && !address.trim().is_empty() && !password.trim().is_empty();
        let button_label = if connecting { "Connecting..." } else { "Connect" };
        if ui
            .add_enabled(can_connect, egui::Button::new(button_label).min_size([120.0, 32.0].into()))
            .clicked()
        {
            clicked_connect = true;
        }

        if let Some(e) = error {
            ui.add_space(10.0);
            ui.colored_label(egui::Color32::LIGHT_RED, e);
        }
    });
    clicked_connect
}

/// Renders the connected view (remote screen + Disconnect button).
/// Returns false when the caller should drop back to the connect form
/// (user clicked Disconnect, or the connection died).
fn show_connected(ui: &mut egui::Ui, ctx: &egui::Context, session: &mut Session) -> bool {
    if !session.running.load(Ordering::Relaxed) {
        return false; // receiver thread ended - connection dropped
    }

    let mut keep_going = true;
    ui.horizontal(|ui| {
        if ui.button("Disconnect").clicked() {
            keep_going = false;
        }
        ui.label(format!("Remote: {}x{}", session.remote_w, session.remote_h));
        if let Some(e) = &session.error {
            ui.colored_label(egui::Color32::LIGHT_RED, e);
        }
    });

    // "Blank host screen" toggle: hides the real desktop from anyone
    // physically at the host machine (shows "Welcome to Onboarding"
    // there instead) while this client keeps seeing/controlling it.
    // Host-side support is Windows-only; other hosts reply with an error,
    // shown below rather than the toggle just silently doing nothing.
    ui.horizontal(|ui| {
        let (label, pending, enabled, err) = {
            let status = session.blank_screen.lock().unwrap();
            let label = if status.pending {
                "Blank host screen: ..."
            } else if status.enabled {
                "Blank host screen: On"
            } else {
                "Blank host screen: Off"
            };
            (label, status.pending, status.enabled, status.error.clone())
        };

        if ui.add_enabled(!pending, egui::Button::new(label)).clicked() {
            session.blank_screen.lock().unwrap().pending = true;
            let _ = write_message(
                &mut session.stream,
                &ClientMessage::ToggleBlankScreen { enabled: !enabled },
            );
        }
        if let Some(e) = err {
            ui.colored_label(egui::Color32::LIGHT_RED, e);
        }
    });

    // Upload the latest decoded frame to a GPU texture, only if it changed.
    let (w, h, version) = {
        let guard = session.shared.lock().unwrap();
        (guard.width, guard.height, guard.version)
    };
    if w > 0 && h > 0 && version != session.uploaded_version {
        let rgb = session.shared.lock().unwrap().rgb.clone();
        let image = egui::ColorImage::from_rgb([w, h], &rgb);
        match &mut session.texture {
            Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
            None => {
                session.texture =
                    Some(ctx.load_texture("remote-frame", image, egui::TextureOptions::LINEAR));
            }
        }
        session.uploaded_version = version;
    }

    let avail = ui.available_size();
    if let Some(tex) = &session.texture {
        let tex_size = tex.size_vec2();
        let scale = (avail.x / tex_size.x).min(avail.y / tex_size.y).min(1.0).max(0.01);
        let draw_size = tex_size * scale;

        let (rect, response) =
            ui.allocate_exact_size(draw_size, egui::Sense::click_and_drag());
        egui::Image::new((tex.id(), draw_size)).paint_at(ui, rect);

        handle_input(ui, ctx, session, rect, response);
    } else {
        ui.centered_and_justified(|ui| ui.label("Waiting for first frame..."));
    }

    keep_going
}

/// Translate egui input (within `rect`, mapped to the remote's pixel space)
/// into ClientMessages and send them to the host.
fn handle_input(
    ui: &egui::Ui,
    ctx: &egui::Context,
    session: &mut Session,
    rect: egui::Rect,
    response: egui::Response,
) {
    let _ = ui;
    let (remote_w, remote_h) = (session.remote_w as f32, session.remote_h as f32);

    // Mouse movement, scaled from the drawn rect into remote screen coords.
    if let Some(pos) = response.hover_pos().or(response.interact_pointer_pos()) {
        if rect.width() > 0.0 && rect.height() > 0.0 {
            let rel_x = (pos.x - rect.left()) / rect.width();
            let rel_y = (pos.y - rect.top()) / rect.height();
            let x = (rel_x.clamp(0.0, 1.0) * remote_w) as i32;
            let y = (rel_y.clamp(0.0, 1.0) * remote_h) as i32;
            if session.last_mouse != Some((x, y)) {
                session.last_mouse = Some((x, y));
                let _ = write_message(&mut session.stream, &ClientMessage::MouseMove { x, y });
            }
        }
    }

    // Mouse buttons. Gated to the remote-view widget specifically - egui's
    // `i.pointer.button_down()` reflects the raw OS button state regardless
    // of which widget is involved, so without this check, clicking *any*
    // other UI element (the Blank host screen toggle, Disconnect, ...)
    // would also fire a phantom click into the host at whatever stale
    // position the pointer last was inside the remote view.
    let over_remote_view =
        response.hovered() || response.dragged() || response.is_pointer_button_down_on();
    if over_remote_view {
        ctx.input(|i| {
            for (idx, (btn, proto_btn)) in [
                (egui::PointerButton::Primary, MouseButton::Left),
                (egui::PointerButton::Secondary, MouseButton::Right),
                (egui::PointerButton::Middle, MouseButton::Middle),
            ]
            .into_iter()
            .enumerate()
            {
                let down = i.pointer.button_down(btn);
                if down != session.mouse_down[idx] {
                    session.mouse_down[idx] = down;
                    let _ = write_message(
                        &mut session.stream,
                        &ClientMessage::MouseButton { button: proto_btn, pressed: down },
                    );
                }
            }
        });
    }

    ctx.input(|i| {

        // Scroll wheel (only forward while hovering the remote view).
        if response.hovered() && i.smooth_scroll_delta.y.abs() > 0.01 {
            let _ = write_message(
                &mut session.stream,
                &ClientMessage::MouseScroll { delta: i.smooth_scroll_delta.y as i32 },
            );
        }

        // Modifier keys: egui reports these via Modifiers, not as Key
        // events, so diff the modifiers state itself frame to frame.
        let mods = i.modifiers;
        for (down, was_down, name) in [
            (mods.shift, session.modifiers_down.shift, "Shift"),
            (mods.ctrl, session.modifiers_down.ctrl, "Control"),
            (mods.alt, session.modifiers_down.alt, "Alt"),
            (mods.mac_cmd, session.modifiers_down.mac_cmd, "Meta"),
        ] {
            if down != was_down {
                let _ = write_message(
                    &mut session.stream,
                    &ClientMessage::Key { key: name.to_string(), pressed: down },
                );
            }
        }
        session.modifiers_down = mods;

        // Regular keys: diff the down-set like the old minifb loop did.
        let now_down = i.keys_down.clone();
        for key in now_down.difference(&session.keys_down) {
            if let Some(name) = key_to_name(*key) {
                let _ = write_message(
                    &mut session.stream,
                    &ClientMessage::Key { key: name, pressed: true },
                );
            }
        }
        for key in session.keys_down.difference(&now_down) {
            if let Some(name) = key_to_name(*key) {
                let _ = write_message(
                    &mut session.stream,
                    &ClientMessage::Key { key: name, pressed: false },
                );
            }
        }
        session.keys_down = now_down;
    });
}

/// Kick off a connection attempt on a background thread so the UI stays
/// responsive; the result comes back over `tx`.
fn spawn_connect(address: String, password: String, tx: mpsc::Sender<ConnectOutcome>) {
    std::thread::spawn(move || {
        let outcome = connect(&address, &password)
            .map(|(stream, w, h)| {
                let shared = Arc::new(Mutex::new(SharedFrame {
                    width: 0,
                    height: 0,
                    rgb: Vec::new(),
                    version: 0,
                }));
                let blank_screen = Arc::new(Mutex::new(BlankScreenStatus {
                    enabled: false,
                    pending: false,
                    error: None,
                }));
                let running = Arc::new(AtomicBool::new(true));
                let receiver = spawn_frame_receiver(
                    &stream,
                    shared.clone(),
                    blank_screen.clone(),
                    running.clone(),
                );
                Session {
                    stream,
                    remote_w: w,
                    remote_h: h,
                    shared,
                    blank_screen,
                    running,
                    receiver: Some(receiver),
                    texture: None,
                    uploaded_version: 0,
                    last_mouse: None,
                    mouse_down: [false; 3],
                    keys_down: Default::default(),
                    modifiers_down: Default::default(),
                    error: None,
                }
            })
            .map(ConnectOutcome::Ok)
            .unwrap_or_else(|e| ConnectOutcome::Err(e.to_string()));
        let _ = tx.send(outcome);
    });
}

fn spawn_frame_receiver(
    stream: &TcpStream,
    shared: Arc<Mutex<SharedFrame>>,
    blank_screen: Arc<Mutex<BlankScreenStatus>>,
    running: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    let mut reader_stream = stream.try_clone().expect("clone socket");
    let version_counter = Arc::new(AtomicU64::new(0));
    std::thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            let msg: Result<HostMessage, _> = read_message(&mut reader_stream);
            match msg {
                Ok(HostMessage::Frame { width, height, jpeg_bytes }) => {
                    if let Ok(img) = image::load_from_memory(&jpeg_bytes) {
                        let rgb = img.to_rgb8().into_raw();
                        let v = version_counter.fetch_add(1, Ordering::Relaxed) + 1;
                        let mut guard = shared.lock().unwrap();
                        guard.width = width as usize;
                        guard.height = height as usize;
                        guard.rgb = rgb;
                        guard.version = v;
                    }
                }
                Ok(HostMessage::BlankScreenResult { enabled, ok, reason }) => {
                    let mut status = blank_screen.lock().unwrap();
                    status.pending = false;
                    status.enabled = enabled;
                    status.error = if ok { None } else { reason };
                }
                Ok(HostMessage::Error { message }) => {
                    eprintln!("Host error: {message}");
                }
                Ok(_) => {}
                Err(_) => break, // connection lost
            }
        }
        running.store(false, Ordering::Relaxed);
    })
}

/// Clean up an address the user typed or pasted before handing it to
/// `TcpStream::connect`. In particular, tools like ngrok print their
/// forwarding address as `tcp://host:port` - pasting that whole line in
/// (scheme included) makes the OS try to resolve a hostname literally
/// containing "tcp://", which fails as "no such host is known" and is
/// confusing to debug. Strip a leading scheme and any stray whitespace.
fn sanitize_address(input: &str) -> String {
    let trimmed = input.trim();
    trimmed
        .split_once("://")
        .map(|(_scheme, rest)| rest)
        .unwrap_or(trimmed)
        .trim_end_matches('/')
        .to_string()
}

/// Connect straight to a host's address:port and run the auth handshake.
fn connect(addr: &str, password: &str) -> std::io::Result<(TcpStream, usize, usize)> {
    let mut stream = TcpStream::connect(addr).map_err(|e| {
        // "No such host is known" (Windows) / "Name or service not known"
        // (Linux) almost always means the Address field has a typo or a
        // stray scheme/path in it rather than a real network problem.
        if e.raw_os_error() == Some(11001) || e.to_string().contains("known") {
            std::io::Error::new(
                e.kind(),
                format!("can't resolve \"{addr}\" - check the Address field (host:port, no \"tcp://\" or trailing slash): {e}"),
            )
        } else {
            e
        }
    })?;
    stream.set_nodelay(true).ok();

    write_message(
        &mut stream,
        &ClientMessage::Auth { password_hash: hash_password(password) },
    )?;

    let auth_result: HostMessage = read_message(&mut stream)?;
    match auth_result {
        HostMessage::AuthResult { ok: true, screen_width, screen_height, .. } => {
            Ok((stream, screen_width as usize, screen_height as usize))
        }
        HostMessage::AuthResult { ok: false, reason, .. } => Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("authentication failed: {}", reason.unwrap_or_default()),
        )),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "unexpected response from host",
        )),
    }
}

/// Map an egui key to the portable key name used on the wire (see host's map_key).
fn key_to_name(key: egui::Key) -> Option<String> {
    use egui::Key::*;
    Some(match key {
        A => "a".into(), B => "b".into(), C => "c".into(), D => "d".into(),
        E => "e".into(), F => "f".into(), G => "g".into(), H => "h".into(),
        I => "i".into(), J => "j".into(), K => "k".into(), L => "l".into(),
        M => "m".into(), N => "n".into(), O => "o".into(), P => "p".into(),
        Q => "q".into(), R => "r".into(), S => "s".into(), T => "t".into(),
        U => "u".into(), V => "v".into(), W => "w".into(), X => "x".into(),
        Y => "y".into(), Z => "z".into(),
        Num0 => "0".into(), Num1 => "1".into(), Num2 => "2".into(), Num3 => "3".into(),
        Num4 => "4".into(), Num5 => "5".into(), Num6 => "6".into(), Num7 => "7".into(),
        Num8 => "8".into(), Num9 => "9".into(),
        Enter => "Enter".into(),
        Backspace => "Backspace".into(),
        Tab => "Tab".into(),
        Space => "Space".into(),
        Escape => "Escape".into(),
        ArrowUp => "ArrowUp".into(),
        ArrowDown => "ArrowDown".into(),
        ArrowLeft => "ArrowLeft".into(),
        ArrowRight => "ArrowRight".into(),
        Delete => "Delete".into(),
        Home => "Home".into(),
        End => "End".into(),
        PageUp => "PageUp".into(),
        PageDown => "PageDown".into(),
        F1 => "F1".into(), F2 => "F2".into(), F3 => "F3".into(), F4 => "F4".into(),
        F5 => "F5".into(), F6 => "F6".into(), F7 => "F7".into(), F8 => "F8".into(),
        F9 => "F9".into(), F10 => "F10".into(), F11 => "F11".into(), F12 => "F12".into(),
        Period => ".".into(),
        Comma => ",".into(),
        Minus => "-".into(),
        Equals => "=".into(),
        Slash => "/".into(),
        Backslash => "\\".into(),
        Semicolon => ";".into(),
        Quote => "'".into(),
        OpenBracket => "[".into(),
        CloseBracket => "]".into(),
        _ => return None,
    })
}

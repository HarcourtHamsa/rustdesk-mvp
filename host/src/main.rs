//! rd-host: runs on the machine you want to control remotely.
//!
//! Listens on a TCP port, authenticates a single controlling client with a
//! shared password, streams JPEG screen frames to it, and injects the
//! mouse/keyboard events it receives.
//!
//! SECURITY: the connection itself is NOT encrypted. Anyone who can see the
//! raw TCP traffic can see your screen and inject input if they guess the
//! password hash. Do not expose this port directly to the open internet -
//! tunnel it over SSH or a VPN. See the top-level README.

// Suppresses the console window that would otherwise pop up when
// double-clicking the exe (the default, tray-only mode has no use for
// one). Only in release builds, so `eprintln!` diagnostics stay visible
// while developing. CLI modes (`listen`/`relay`/`with-relay`) invoked from
// an existing terminal are unaffected either way - they inherit that
// terminal's console rather than needing one auto-created.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod blank_screen;
mod clipboard_sync;
mod gui_core;
mod keylogger;
mod lock_host;
mod mail;
mod tray;

use blank_screen::BlankScreenOverlay;
use clipboard_sync::ClipboardSync;
use keylogger::KeyLogger;
use enigo::{Enigo, Key as EnigoKey, KeyboardControllable, MouseButton as EnigoMouseButton, MouseControllable};
use rd_protocol::{
    hash_password, hashes_match, read_message, write_message, ClientMessage, HostMessage,
    MouseButton, RelayHello, RelayToHost,
};
use scrap::{Capturer, Display};
use std::io::ErrorKind;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

fn generate_random_password() -> String {
    use rand::Rng;
    const CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut rng = rand::thread_rng();
    (0..10)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

fn print_help() {
    println!("rd-host [mode] [options]");
    println!();
    println!("With no arguments, runs in the system tray (no window). Right-click the");
    println!("tray icon to copy the address/password, open the connection log, or quit.");
    println!("The address and password are also appended to rd-host-connections.log next");
    println!("to the executable once the ngrok tunnel finishes starting (or gives up).");
    println!();
    println!("  --tray");
    println!("      Explicitly select tray mode. Same as passing no arguments; accepted");
    println!("      so existing auto-start entries that pass --tray keep working.");
    println!();
    println!("  --setup-autostart");
    println!("      Configure the app to start automatically in system tray mode");
    println!("      when the computer boots.");
    println!();
    println!("  --remove-autostart");
    println!("      Remove the app from auto-start.");
    println!();
    println!("Modes (for scripted/headless use, e.g. running rd-relay on a VPS):");
    println!("  listen [--port <port>] [--password <password>]");
    println!("      Listen directly for incoming connections. The other side needs");
    println!("      this machine's IP:port (and a router port-forward if remote).");
    println!();
    println!("  relay <relay-address:port> [--password <password>]");
    println!("      Register with a relay server you run (rd-relay) and get back a");
    println!("      short code. The other side connects using just that code - no");
    println!("      IP address or port forwarding needed on this end.");
    println!();
    println!("  with-relay [--port <port>] [--password <password>]");
    println!("      Run the relay in this same process and register with it, so one");
    println!("      command gets you both. Only useful when the relay and the host");
    println!("      are the same machine - the other side still needs this machine's");
    println!("      IP address, so this is not a way to avoid port forwarding. Handy");
    println!("      for testing the relay flow on one box.");
    println!();
    println!("If --password is omitted in any mode, a random one is generated and printed.");
}

fn main() {
    // Install rustls's crypto provider before anything touches TLS (the
    // ngrok tunnel does, via hyper-rustls/tokio-rustls). Both the `ring`
    // and `aws-lc-rs` backends end up compiled in here - ngrok itself
    // enables `aws-lc-rs` as a default feature, while another dependency
    // in the tree pulls in `ring` - so rustls can't auto-select one and
    // panics at first use ("Could not automatically determine the
    // process-level CryptoProvider") unless we pick one explicitly, up
    // front, before any TLS handshake can start.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Make the whole process DPI-aware before anything captures the screen.
    // On a display with scaling (e.g. 125%/150%), a DPI-unaware process sees
    // the virtualized *logical* resolution (e.g. 1280x720) while the capture
    // API hands back the *physical* frame buffer (e.g. 1920x1080). That
    // width/height mismatch makes `bgra_to_jpeg` read only the top-left
    // corner, so the client shows a cropped, zoomed-in view instead of the
    // whole screen. Setting this here - before any mode creates a Capturer -
    // makes the reported dimensions and the frame buffer agree on physical
    // pixels. (blank_screen.rs also set it, but only late and only when the
    // blank overlay was used; this covers every mode from the start.)
    #[cfg(windows)]
    unsafe {
        winapi::um::winuser::SetProcessDPIAware();
    }

    // Load NGROK_AUTHTOKEN (and anything else) from a .env file, if present.
    // Missing file is fine - the token can also come from the real
    // environment, and LAN hosting works without it either way.
    //
    // `dotenvy::dotenv()` only looks relative to the current working
    // directory, which is wrong when the exe is launched from a shortcut,
    // a double-click, or a copied/packaged location (the CWD is then
    // unrelated to where the .env lives, so the token isn't found and the
    // ngrok tunnel is reported unavailable). So also search next to the
    // executable and up its parent directories, which is where the .env
    // actually ships. Real environment variables still win either way -
    // dotenvy never overrides a variable that's already set.
    let _ = dotenvy::dotenv();
    load_dotenv_beside_exe();

    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_default();
    match mode.as_str() {
        // No arguments runs the tray (the only interactive mode now); --tray
        // is kept as an explicit alias so existing auto-start registry entries
        // that pass it keep working.
        "" | "--tray" => {
            // Register for launch-on-boot every time the app starts in tray
            // mode, so just running it once is enough - no manual
            // --setup-autostart step needed. Idempotent (it overwrites the
            // Run entry with the current exe path, self-healing if the exe
            // moved) and best-effort: a failure here must not stop the app
            // from running now. `--remove-autostart` still deletes the entry,
            // but note the next tray launch re-adds it.
            if let Err(e) = setup_autostart() {
                eprintln!("Could not register auto-start: {e}");
            }
            if let Err(e) = tray::run() {
                eprintln!("Failed to start: {e}");
                std::process::exit(1);
            }
        }
        "--setup-autostart" => {
            match setup_autostart() {
                Ok(msg) => println!("{msg}"),
                Err(e) => {
                    eprintln!("Failed to set up auto-start: {e}");
                    std::process::exit(1);
                }
            }
        }
        "--remove-autostart" => {
            match remove_autostart() {
                Ok(msg) => println!("{msg}"),
                Err(e) => {
                    eprintln!("Failed to remove auto-start: {e}");
                    std::process::exit(1);
                }
            }
        }
        "listen" => run_listen_mode(args),
        "relay" => run_relay_mode(args),
        "with-relay" => run_with_relay_mode(args),
        "--help" | "-h" => print_help(),
        other => {
            eprintln!("Unknown mode '{other}'.");
            print_help();
            std::process::exit(1);
        }
    }
}

/// Load a `.env` file found next to the executable, or in any of its parent
/// directories, so the ngrok token is picked up regardless of the working
/// directory the process was launched with. This complements
/// `dotenvy::dotenv()` (which only searches from the current directory);
/// values already set - by the real environment or by that earlier call -
/// are left untouched.
fn load_dotenv_beside_exe() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut dir = exe.parent();
    while let Some(d) = dir {
        let candidate = d.join(".env");
        if candidate.is_file() {
            let _ = dotenvy::from_path(&candidate);
            return;
        }
        dir = d.parent();
    }
}

// ------------------------- listen mode (direct, LAN or port-forwarded) -------------------------

fn run_listen_mode(mut args: impl Iterator<Item = String>) {
    let mut port = rd_protocol::DEFAULT_PORT;
    let mut password: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                if let Some(v) = args.next() {
                    port = v.parse().unwrap_or(port);
                }
            }
            "--password" => password = args.next(),
            other => eprintln!("Unknown argument: {other}"),
        }
    }

    let password = password.unwrap_or_else(generate_random_password);
    let password_hash = hash_password(&password);

    println!("=================================================");
    println!(" rd-host is running (direct listen mode)");
    println!(" Port:     {port}");
    println!(" Password: {password}");
    println!("=================================================");
    println!(" Give the client the password above plus this machine's");
    println!(" IP address (and forward the port on your router if");
    println!(" connecting from outside your home network).");
    println!();

    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind port {port}: {e}");
            std::process::exit(1);
        }
    };

    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let hash = password_hash.clone();
                if let Err(e) = handle_client(stream, &hash) {
                    eprintln!("Session ended: {e}");
                }
                println!("Waiting for next connection...");
            }
            Err(e) => eprintln!("Failed to accept connection: {e}"),
        }
    }
}

// ------------------------- relay mode (AnyDesk-style code) -------------------------

fn run_relay_mode(mut args: impl Iterator<Item = String>) {
    let mut relay_addr: Option<String> = None;
    let mut password: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--password" => password = args.next(),
            other => {
                if relay_addr.is_none() {
                    relay_addr = Some(other.to_string());
                } else {
                    eprintln!("Unknown argument: {other}");
                }
            }
        }
    }

    let relay_addr = relay_addr.unwrap_or_else(|| {
        eprintln!("Usage: rd-host relay <relay-address:port> [--password <password>]");
        std::process::exit(1);
    });

    serve_via_relay(&relay_addr, password, None);
}

// ------------------------- with-relay mode (relay + host, one process) -------------------------

/// Start the relay on a background thread inside this process, then register
/// with it over loopback. One command, one terminal, and one Ctrl-C takes
/// down both halves - which is the whole point of this mode versus running
/// `rd-relay` and `rd-host relay` side by side.
///
/// Note this does NOT avoid port forwarding the way a VPS relay does: the
/// relay is listening here, so the connecting side still has to reach this
/// machine on `--port`. The banner says as much.
fn run_with_relay_mode(mut args: impl Iterator<Item = String>) {
    let mut port = rd_protocol::DEFAULT_RELAY_PORT;
    let mut password: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                if let Some(v) = args.next() {
                    port = v.parse().unwrap_or(port);
                }
            }
            "--password" => password = args.next(),
            other => eprintln!("Unknown argument: {other}"),
        }
    }

    // Bind before spawning, so "port already in use" is reported here and
    // now rather than from a detached thread after the host has started up.
    let listener = match rd_relay::bind(port) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind relay port {port}: {e}");
            eprintln!("(Is an rd-relay already running? Pass --port to pick another.)");
            std::process::exit(1);
        }
    };
    println!("[relay] listening on 0.0.0.0:{port} (in-process)");
    std::thread::spawn(move || rd_relay::serve(listener));

    // The listener is already bound, so registering over loopback cannot
    // race the relay's accept loop starting up - no sleep needed here.
    serve_via_relay(&format!("127.0.0.1:{port}"), password, Some(port));
}

// ------------------------- shared relay plumbing -------------------------

/// Register with a relay and serve sessions from it forever, reconnecting if
/// the control connection drops. `local_relay_port` is `Some` when the relay
/// is the one we started ourselves, which only changes what the banner tells
/// the user to do.
fn serve_via_relay(
    relay_addr: &str,
    password: Option<String>,
    local_relay_port: Option<u16>,
) -> ! {
    let password = password.unwrap_or_else(generate_random_password);
    let password_hash = hash_password(&password);

    let mut printed_banner = false;
    loop {
        match connect_and_serve_relay(
            relay_addr,
            &password_hash,
            &password,
            local_relay_port,
            &mut printed_banner,
        ) {
            Ok(()) => {}
            Err(e) => eprintln!("Relay connection error: {e}."),
        }
        eprintln!("Reconnecting to relay in 5s...");
        std::thread::sleep(Duration::from_secs(5));
    }
}

fn connect_and_serve_relay(
    relay_addr: &str,
    password_hash: &str,
    password_plain: &str,
    local_relay_port: Option<u16>,
    printed_banner: &mut bool,
) -> std::io::Result<()> {
    let mut control = TcpStream::connect(relay_addr)?;
    control.set_nodelay(true).ok();
    write_message(&mut control, &RelayHello::RegisterHost)?;

    let reply: RelayToHost = read_message(&mut control)?;
    let code = match reply {
        RelayToHost::Registered { code } => code,
        _ => {
            return Err(std::io::Error::new(
                ErrorKind::Other,
                "unexpected reply from relay during registration",
            ))
        }
    };

    if !*printed_banner {
        println!("=================================================");
        match local_relay_port {
            Some(port) => {
                println!(" rd-host is running (relay + host on this machine)");
                println!(" Relay:    this machine, port {port}");
            }
            None => println!(" rd-host is registered with the relay"),
        }
        println!(" Code:     {code}");
        println!(" Password: {password_plain}");
        println!("=================================================");
        match local_relay_port {
            Some(port) => {
                println!(" The relay is running here rather than on a separate box,");
                println!(" so the connecting side needs this machine's IP too:");
                println!("   rd-client relay <this-machine-ip>:{port} {code} {password_plain}");
                println!(" Port {port} has to be reachable from their side (firewall,");
                println!(" and a router forward if they're off this network).");
            }
            None => {
                println!(" Give whoever is connecting the CODE and PASSWORD above.");
                println!(" They don't need this machine's IP address.");
            }
        }
        println!();
        *printed_banner = true;
    } else {
        println!("Re-registered with relay. Code: {code}");
    }

    loop {
        let msg: RelayToHost = read_message(&mut control)?;
        if let RelayToHost::IncomingSession { session_id } = msg {
            let relay_addr = relay_addr.to_string();
            let password_hash = password_hash.to_string();
            std::thread::spawn(move || {
                match TcpStream::connect(&relay_addr) {
                    Ok(mut data_stream) => {
                        data_stream.set_nodelay(true).ok();
                        if write_message(&mut data_stream, &RelayHello::SessionData { session_id })
                            .is_err()
                        {
                            return;
                        }
                        if let Err(e) = handle_client(data_stream, &password_hash) {
                            eprintln!("Relay session ended: {e}");
                        }
                    }
                    Err(e) => eprintln!("Failed to open relay data connection: {e}"),
                }
                println!("Waiting for next connection...");
            });
        }
    }
}

fn handle_client(mut stream: TcpStream, expected_hash: &str) -> std::io::Result<()> {
    stream.set_nodelay(true).ok();
    let peer = stream.peer_addr().ok();
    println!("Incoming connection from {peer:?}");

    // --- Auth ---
    let auth: ClientMessage = read_message(&mut stream)?;
    let provided_hash = match auth {
        ClientMessage::Auth { password_hash } => password_hash,
        _ => {
            write_message(
                &mut stream,
                &HostMessage::AuthResult {
                    ok: false,
                    reason: Some("expected auth message first".into()),
                    screen_width: 0,
                    screen_height: 0,
                },
            )?;
            return Ok(());
        }
    };

    let display = Display::primary().map_err(|e| std::io::Error::new(ErrorKind::Other, e))?;
    let (screen_w, screen_h) = (display.width() as u32, display.height() as u32);

    if !hashes_match(&provided_hash, expected_hash) {
        println!("Rejected connection from {peer:?}: bad password");
        write_message(
            &mut stream,
            &HostMessage::AuthResult {
                ok: false,
                reason: Some("incorrect password".into()),
                screen_width: 0,
                screen_height: 0,
            },
        )?;
        return Ok(());
    }

    // Every write from here on goes through this one shared, locked clone -
    // `stream` itself is used for reading only from this point on. Before
    // this, `stream` was also used directly for a couple of ad-hoc replies
    // (RequestScreenInfo, and now ToggleBlankScreen) *while* the frame
    // sender thread wrote to its own separate clone concurrently -  two
    // unsynchronized writers to the same socket, which can interleave
    // partial messages (a length prefix from one write landing between
    // another write's length prefix and payload) and corrupt the framing.
    // Routing every writer through the same Mutex makes each `write_message`
    // call atomic with respect to all the others.
    let writer: Arc<Mutex<TcpStream>> = Arc::new(Mutex::new(stream.try_clone()?));

    write_message(
        &mut *writer.lock().unwrap(),
        &HostMessage::AuthResult {
            ok: true,
            reason: None,
            screen_width: screen_w,
            screen_height: screen_h,
        },
    )?;
    println!("Client authenticated from {peer:?}. Streaming...");

    let running = Arc::new(AtomicBool::new(true));

    // --- Frame capture + send, decoupled by a single-slot "latest frame
    // wins" handoff ---
    //
    // Capture and network send used to happen in the same loop, writing
    // each frame straight to the socket. That's fine on a fast link, but
    // over anything slower (a phone hotspot, a tunnel like ngrok, plain
    // congestion) the OS/tunnel buffers in front of the socket absorb
    // frames faster than they can actually leave the machine, so a backlog
    // silently piles up - the client ends up displaying frames that were
    // captured a minute or more ago, and it only gets worse over time.
    //
    // Splitting this into two threads sharing one `Mutex<Option<Frame>>`
    // slot fixes that: the capture thread just overwrites the slot with
    // whatever it captured most recently (discarding a not-yet-sent older
    // frame outright), and the sender thread always transmits whatever's
    // currently in the slot. If the network falls behind, frames get
    // dropped here instead of queueing - latency stays bounded by roughly
    // one frame's transmit time instead of growing without limit.
    let latest_frame: Arc<Mutex<Option<HostMessage>>> = Arc::new(Mutex::new(None));

    // Lets `ClientMessage::RefreshScreen` ask this thread to recreate its
    // Capturer: each request carries a reply channel, so the requester
    // gets a real ok/err back rather than a blind "sure, requested".
    // Recreating the DXGI Desktop Duplication session is the standard fix
    // for one that's gotten stuck returning stale/no frames after certain
    // host-side desktop transitions - see `ClientMessage::RefreshScreen`'s
    // doc comment.
    let (refresh_tx, refresh_rx) = mpsc::channel::<mpsc::Sender<Result<(), String>>>();

    let running_capture = running.clone();
    let latest_frame_writer = latest_frame.clone();
    let capture_thread = std::thread::spawn(move || {
        // Lazily created, and recreated on demand: both a refresh request and
        // a capture error drop the old duplication and build a fresh one.
        // `None` means "needs (re)creating on the next tick".
        let mut capturer: Option<Capturer> = None;
        let (mut w, mut h) = (0usize, 0usize);
        let target_frame_time = Duration::from_millis(1000 / 12); // ~12 fps

        while running_capture.load(Ordering::Relaxed) {
            let frame_start = std::time::Instant::now();

            // A refresh request recreates the Capturer from scratch and reports
            // the outcome back to whoever asked (the RefreshScreen handler in
            // the input loop). Recreation is the standard fix for a DXGI
            // duplication that's gotten stuck after a desktop transition - but
            // the loop below now also recreates on its own when a capture error
            // signals the same thing, so a refresh is just a manual trigger of
            // recovery that already happens automatically.
            if let Ok(reply_tx) = refresh_rx.try_recv() {
                // Drop the current duplication *before* creating its
                // replacement - Windows allows only one live
                // IDXGIOutputDuplication per output at a time.
                capturer = None;
                let reply = match make_capturer(&running_capture) {
                    Ok((c, nw, nh)) => {
                        w = nw;
                        h = nh;
                        capturer = Some(c);
                        Ok(())
                    }
                    Err(e) => Err(e),
                };
                let _ = reply_tx.send(reply);
            }

            // Ensure we have a working capturer, (re)creating it if a prior
            // error or a failed refresh left us without one. If creation is
            // still failing (e.g. we're sitting on the secure desktop), back
            // off and retry next tick rather than spinning - the thread stays
            // alive and responsive to `running` either way, so teardown and
            // future refreshes still work.
            if capturer.is_none() {
                match make_capturer(&running_capture) {
                    Ok((c, nw, nh)) => {
                        w = nw;
                        h = nh;
                        capturer = Some(c);
                    }
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(200));
                        continue;
                    }
                }
            }
            let Some(cap) = capturer.as_mut() else { continue };

            match cap.frame() {
                Ok(frame) => {
                    if let Some(jpeg) = bgra_to_jpeg(&frame, w, h) {
                        let msg = HostMessage::Frame {
                            width: w as u32,
                            height: h as u32,
                            jpeg_bytes: jpeg,
                        };
                        *latest_frame_writer.lock().unwrap() = Some(msg);
                    }
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                    // frame not ready yet, just retry
                }
                Err(e) => {
                    // A real capture error - almost always DXGI_ERROR_ACCESS_LOST
                    // after a desktop switch (lock/unlock, UAC's secure desktop,
                    // a resolution change), which permanently invalidates this
                    // duplication. Drop it and recreate on the next tick rather
                    // than killing the capture thread (the old behavior, which
                    // froze the client on the last frame and left the thread
                    // unable to serve the refresh meant to recover from exactly
                    // this).
                    eprintln!("Capture error, recreating capturer: {e}");
                    capturer = None;
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            let elapsed = frame_start.elapsed();
            if elapsed < target_frame_time {
                std::thread::sleep(target_frame_time - elapsed);
            }
        }
    });

    let writer_for_sender = writer.clone();
    let running_sender = running.clone();
    let sender_thread = std::thread::spawn(move || {
        while running_sender.load(Ordering::Relaxed) {
            let frame = latest_frame.lock().unwrap().take();
            match frame {
                Some(msg) => {
                    let wrote = write_message(&mut *writer_for_sender.lock().unwrap(), &msg);
                    if wrote.is_err() {
                        running_sender.store(false, Ordering::Relaxed);
                        break; // client disconnected
                    }
                }
                None => std::thread::sleep(Duration::from_millis(5)), // nothing new yet
            }
        }
    });

    // --- Input receive loop (runs on this thread) ---
    let mut enigo = Enigo::new();
    // Dropped whenever this function returns, by any path (clean
    // disconnect, read error, early return) - which tears the overlay
    // down via its Drop impl. A dead session must never leave the host's
    // screen permanently blanked.
    let mut blank_screen = BlankScreenOverlay::new();
    // While true, mouse/keyboard messages below are read (so the socket
    // doesn't back up) but not applied - frames keep streaming as normal.
    // Reset implicitly on every new connection since this is a fresh local
    // variable each time `handle_client` runs.
    let mut guest_input_blocked = false;

    // Same lifetime/safety story as `blank_screen`: a local variable here,
    // torn down (unhooking the keyboard) on every exit path via Drop.
    let mut keylogger = KeyLogger::new();
    // Captured keystrokes land here (from the hook's own dedicated
    // thread) and get forwarded to the client by the thread just below -
    // never written to the socket directly from the hook callback, which
    // has to return quickly.
    let (key_tx, key_rx) = mpsc::channel::<keylogger::Keystroke>();
    let writer_for_keys = writer.clone();
    let running_keys = running.clone();
    let key_forward_thread = std::thread::spawn(move || {
        while running_keys.load(Ordering::Relaxed) {
            match key_rx.recv_timeout(Duration::from_millis(200)) {
                Ok((key, timestamp_ms)) => {
                    let msg = HostMessage::Keystroke { key, timestamp_ms };
                    if write_message(&mut *writer_for_keys.lock().unwrap(), &msg).is_err() {
                        break; // client disconnected
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {} // just recheck `running`
                Err(mpsc::RecvTimeoutError::Disconnected) => break, // `key_tx` dropped
            }
        }
    });

    // Same lifetime/safety story again: a local variable here, its poll
    // thread torn down on every exit path via Drop.
    let mut clipboard_sync = ClipboardSync::new();
    // Host clipboard changes land here (from the poll thread) and get
    // forwarded to the client by the thread just below - same
    // detect-there/send-here split as the keystroke log above.
    let (clip_tx, clip_rx) = mpsc::channel::<String>();
    let writer_for_clip = writer.clone();
    let running_clip = running.clone();
    let clip_forward_thread = std::thread::spawn(move || {
        while running_clip.load(Ordering::Relaxed) {
            match clip_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(text) => {
                    let msg = HostMessage::ClipboardUpdate { text };
                    if write_message(&mut *writer_for_clip.lock().unwrap(), &msg).is_err() {
                        break; // client disconnected
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {} // just recheck `running`
                Err(mpsc::RecvTimeoutError::Disconnected) => break, // `clip_tx` dropped
            }
        }
    });


    loop {
        let msg: Result<ClientMessage, _> = read_message(&mut stream);
        let msg = match msg {
            Ok(m) => m,
            Err(_) => break, // client disconnected
        };
        match msg {
            ClientMessage::MouseMove { x, y } => {
                if !guest_input_blocked {
                    enigo.mouse_move_to(x, y);
                }
            }
            ClientMessage::MouseButton { button, pressed } => {
                if !guest_input_blocked {
                    let btn = match button {
                        MouseButton::Left => EnigoMouseButton::Left,
                        MouseButton::Right => EnigoMouseButton::Right,
                        MouseButton::Middle => EnigoMouseButton::Middle,
                    };
                    if pressed {
                        enigo.mouse_down(btn);
                    } else {
                        enigo.mouse_up(btn);
                    }
                }
            }
            ClientMessage::MouseScroll { delta } => {
                if !guest_input_blocked {
                    enigo.mouse_scroll_y(delta);
                }
            }
            ClientMessage::Key { key, pressed } => {
                if !guest_input_blocked {
                    if let Some(k) = map_key(&key) {
                        if pressed {
                            enigo.key_down(k);
                        } else {
                            enigo.key_up(k);
                        }
                    }
                }
            }
            ClientMessage::RequestScreenInfo => {
                let msg = HostMessage::AuthResult {
                    ok: true,
                    reason: None,
                    screen_width: screen_w,
                    screen_height: screen_h,
                };
                let _ = write_message(&mut *writer.lock().unwrap(), &msg);
            }
            ClientMessage::ToggleBlankScreen { enabled } => {
                let (ok, reason) = if enabled {
                    match blank_screen.enable() {
                        Ok(()) => (true, None),
                        Err(e) => (false, Some(e)),
                    }
                } else {
                    blank_screen.disable();
                    (true, None)
                };
                let msg = HostMessage::BlankScreenResult {
                    enabled: blank_screen.is_enabled(),
                    ok,
                    reason,
                };
                let _ = write_message(&mut *writer.lock().unwrap(), &msg);
            }
            ClientMessage::BlockGuestInput { blocked } => {
                guest_input_blocked = blocked;
                let msg = HostMessage::GuestInputBlockedResult { blocked: guest_input_blocked };
                let _ = write_message(&mut *writer.lock().unwrap(), &msg);
            }
            ClientMessage::ToggleKeystrokeLog { enabled } => {
                let (ok, reason) = if enabled {
                    match keylogger.enable(key_tx.clone()) {
                        Ok(()) => (true, None),
                        Err(e) => (false, Some(e)),
                    }
                } else {
                    keylogger.disable();
                    (true, None)
                };
                let msg = HostMessage::KeystrokeLogResult {
                    enabled: keylogger.is_enabled(),
                    ok,
                    reason,
                };
                let _ = write_message(&mut *writer.lock().unwrap(), &msg);
            }
            ClientMessage::ToggleClipboardSync { enabled } => {
                let (ok, reason) = if enabled {
                    match clipboard_sync.enable(clip_tx.clone()) {
                        Ok(()) => (true, None),
                        Err(e) => (false, Some(e)),
                    }
                } else {
                    clipboard_sync.disable();
                    (true, None)
                };
                let msg = HostMessage::ClipboardSyncResult {
                    enabled: clipboard_sync.is_enabled(),
                    ok,
                    reason,
                };
                let _ = write_message(&mut *writer.lock().unwrap(), &msg);
            }
            ClientMessage::ClipboardUpdate { text } => {
                if clipboard_sync.is_enabled() {
                    clipboard_sync.set_local(&text);
                }
            }
            ClientMessage::LockHost => {
                let (ok, reason) = match lock_host::lock() {
                    Ok(()) => (true, None),
                    Err(e) => (false, Some(e)),
                };
                let msg = HostMessage::LockHostResult { ok, reason };
                let _ = write_message(&mut *writer.lock().unwrap(), &msg);
            }
            ClientMessage::RefreshScreen => {
                let (reply_tx, reply_rx) = mpsc::channel();
                let (ok, reason) = if refresh_tx.send(reply_tx).is_ok() {
                    match reply_rx.recv_timeout(Duration::from_secs(3)) {
                        Ok(Ok(())) => (true, None),
                        Ok(Err(e)) => (false, Some(e)),
                        Err(_) => (false, Some("timed out refreshing the screen capture".to_string())),
                    }
                } else {
                    (false, Some("capture thread isn't running".to_string()))
                };
                let msg = HostMessage::RefreshScreenResult { ok, reason };
                let _ = write_message(&mut *writer.lock().unwrap(), &msg);
            }
            ClientMessage::Disconnect => break,
            ClientMessage::Auth { .. } => {} // ignore repeated auth
        }
    }

    running.store(false, Ordering::Relaxed);
    keylogger.disable(); // unhook before the channel it feeds is torn down
    clipboard_sync.disable(); // stop polling before the channel it feeds is torn down
    let _ = capture_thread.join();
    let _ = sender_thread.join();
    let _ = key_forward_thread.join();
    let _ = clip_forward_thread.join();
    println!("Client {peer:?} disconnected.");
    Ok(())
}

/// Create a fresh screen `Capturer`, retrying briefly before giving up.
///
/// A single attempt is unreliable on Windows: right after an old DXGI
/// duplication is dropped, the OS may not have released it yet, and certain
/// desktop transitions (lock/unlock, UAC's secure desktop, a resolution
/// change) leave duplication momentarily unavailable - so a creation that
/// fails now often succeeds a fraction of a second later. Retries for ~2s,
/// bailing out early if `running` goes false so a closing session tears down
/// promptly. Returns the capturer with its width/height, or the last error.
fn make_capturer(running: &AtomicBool) -> Result<(Capturer, usize, usize), String> {
    let mut last_err = "screen capture unavailable".to_string();
    for _ in 0..20 {
        if !running.load(Ordering::Relaxed) {
            return Err("connection closing".to_string());
        }
        match Display::primary().and_then(Capturer::new) {
            Ok(c) => {
                let (w, h) = (c.width(), c.height());
                return Ok((c, w, h));
            }
            Err(e) => {
                last_err = e.to_string();
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    Err(last_err)
}


/// Convert a captured BGRA frame (which may have row padding) into JPEG bytes.
fn bgra_to_jpeg(frame: &[u8], width: usize, height: usize) -> Option<Vec<u8>> {
    let stride = frame.len() / height;
    let mut rgb = vec![0u8; width * height * 3];
    for y in 0..height {
        let row_start = y * stride;
        for x in 0..width {
            let i = row_start + x * 4;
            if i + 2 >= frame.len() {
                continue;
            }
            let b = frame[i];
            let g = frame[i + 1];
            let r = frame[i + 2];
            let out = (y * width + x) * 3;
            rgb[out] = r;
            rgb[out + 1] = g;
            rgb[out + 2] = b;
        }
    }

    let img = image::RgbImage::from_raw(width as u32, height as u32, rgb)?;
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut buf, image::ImageOutputFormat::Jpeg(60))
        .ok()?;
    Some(buf.into_inner())
}

/// Map the portable key names sent over the wire to enigo's Key enum.
fn map_key(name: &str) -> Option<EnigoKey> {
    if name.chars().count() == 1 {
        return Some(EnigoKey::Layout(name.chars().next().unwrap()));
    }
    Some(match name {
        "Enter" => EnigoKey::Return,
        "Backspace" => EnigoKey::Backspace,
        "Tab" => EnigoKey::Tab,
        "Space" => EnigoKey::Space,
        "Escape" => EnigoKey::Escape,
        "Shift" => EnigoKey::Shift,
        "Control" => EnigoKey::Control,
        "Alt" => EnigoKey::Alt,
        "Meta" | "Super" | "Cmd" => EnigoKey::Meta,
        "CapsLock" => EnigoKey::CapsLock,
        "ArrowUp" => EnigoKey::UpArrow,
        "ArrowDown" => EnigoKey::DownArrow,
        "ArrowLeft" => EnigoKey::LeftArrow,
        "ArrowRight" => EnigoKey::RightArrow,
        "Delete" => EnigoKey::Delete,
        "Home" => EnigoKey::Home,
        "End" => EnigoKey::End,
        "PageUp" => EnigoKey::PageUp,
        "PageDown" => EnigoKey::PageDown,
        "F1" => EnigoKey::F1,
        "F2" => EnigoKey::F2,
        "F3" => EnigoKey::F3,
        "F4" => EnigoKey::F4,
        "F5" => EnigoKey::F5,
        "F6" => EnigoKey::F6,
        "F7" => EnigoKey::F7,
        "F8" => EnigoKey::F8,
        "F9" => EnigoKey::F9,
        "F10" => EnigoKey::F10,
        "F11" => EnigoKey::F11,
        "F12" => EnigoKey::F12,
        _ => return None,
    })
}

/// Convert a Rust string to a wide UTF-16 string for Windows APIs.
fn wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// Set up auto-start by adding a Windows registry entry to run the app in tray mode on boot.
#[cfg(windows)]
fn setup_autostart() -> Result<String, String> {
    use winapi::um::winreg::{RegOpenKeyExW, RegSetValueExW, HKEY_CURRENT_USER};

    let subkey = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    let app_name = wide("RD Authenticator");

    // Get the executable path
    let exe_path = std::env::current_exe()
        .map_err(|e| format!("Failed to get executable path: {e}"))?;

    // Append --tray to the command
    let exe_path_str = format!("\"{}\" --tray", exe_path.display());
    let exe_path_wide = wide(&exe_path_str);

    unsafe {
        let mut hkey = std::ptr::null_mut();
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            0x0002, // KEY_WRITE
            &mut hkey,
        );

        if result != 0 {
            return Err(format!("Failed to open registry key (error code: {result})"));
        }

        let result = RegSetValueExW(
            hkey,
            app_name.as_ptr(),
            0,
            1, // REG_SZ
            exe_path_wide.as_ptr() as *const u8,
            (exe_path_wide.len() * std::mem::size_of::<u16>()) as u32,
        );

        winapi::um::winreg::RegCloseKey(hkey);

        if result != 0 {
            return Err(format!("Failed to set registry value (error code: {result})"));
        }
    }

    Ok("Auto-start enabled. The app will start in system tray mode when you boot.".to_string())
}

#[cfg(not(windows))]
fn setup_autostart() -> Result<String, String> {
    Err("Auto-start is only supported on Windows".to_string())
}

/// Remove auto-start by deleting the Windows registry entry.
#[cfg(windows)]
fn remove_autostart() -> Result<String, String> {
    use winapi::um::winreg::{RegOpenKeyExW, RegDeleteValueW, HKEY_CURRENT_USER};

    let subkey = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    let app_name = wide("RD Authenticator");

    unsafe {
        let mut hkey = std::ptr::null_mut();
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            0x0002, // KEY_WRITE
            &mut hkey,
        );

        if result != 0 {
            return Err(format!("Failed to open registry key (error code: {result})"));
        }

        let result = RegDeleteValueW(hkey, app_name.as_ptr());

        winapi::um::winreg::RegCloseKey(hkey);

        if result != 0 && result != 2 {
            // Error code 2 means "not found", which is fine
            return Err(format!("Failed to delete registry value (error code: {result})"));
        }
    }

    Ok("Auto-start disabled.".to_string())
}

#[cfg(not(windows))]
fn remove_autostart() -> Result<String, String> {
    Err("Auto-start removal is only supported on Windows".to_string())
}

//! rd-client: run this on the machine you have with you (e.g. a borrowed
//! laptop while traveling). It's a small GUI app - point it at a host's
//! address and password, and it shows you the remote screen and forwards
//! your mouse/keyboard.

use eframe::egui;
use rd_protocol::{hash_password, read_message, write_message, ClientMessage, HostMessage, MouseButton};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ICON_SIZE: u32 = 256;

/// Decodes the shared branding logo (baked in at compile time), letterboxes
/// it onto a transparent square canvas so the whole mark survives instead
/// of being cropped, and resizes to `size`x`size`.
fn load_logo_rgba(size: u32) -> Option<Vec<u8>> {
    let bytes = include_bytes!("../../assets/logo.png");
    let img = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    let side = w.max(h);
    let mut canvas = image::RgbaImage::new(side, side);
    image::imageops::overlay(&mut canvas, &img, ((side - w) / 2) as i64, ((side - h) / 2) as i64);
    let resized = image::imageops::resize(&canvas, size, size, image::imageops::FilterType::Lanczos3);
    Some(resized.into_raw())
}

fn main() -> eframe::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1100.0, 720.0])
        .with_min_inner_size([640.0, 420.0]);
    if let Some(rgba) = load_logo_rgba(ICON_SIZE) {
        viewport = viewport.with_icon(egui::IconData { width: ICON_SIZE, height: ICON_SIZE, rgba });
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
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

/// The host's screen-capture target (see rd-host's `target_frame_time`,
/// ~12 fps). Receiving frames at roughly this rate is treated as "100%".
const TARGET_FPS: f32 = 12.0;
/// How far back the strength readout looks when computing the rate.
const NET_WINDOW: Duration = Duration::from_secs(3);

/// Rolling record of recent frame-arrival times, updated by the receiver
/// thread and read by the UI thread to gauge how well the stream is keeping
/// up - the basis for the network-strength readout. A strong link delivers
/// the host's target rate steadily; a weak one arrives slower and in bursts.
struct NetworkStats {
    /// When the session started, so the first few seconds after connecting
    /// measure against how long we've actually been receiving rather than a
    /// full window we haven't had time to fill (which would read red on a
    /// perfectly good link).
    started: Instant,
    /// Arrival instants of recent frames, oldest first. Pruned to the
    /// trailing `NET_WINDOW` on every push, so it never grows without bound.
    arrivals: VecDeque<Instant>,
}

/// A computed network-strength readout for display: a 0-100 score, the
/// color to draw it in, a one-word label, and the raw fps behind it.
struct NetworkQuality {
    percent: u8,
    color: egui::Color32,
    label: &'static str,
    fps: f32,
}

impl NetworkStats {
    fn new() -> Self {
        Self { started: Instant::now(), arrivals: VecDeque::new() }
    }

    /// Record a frame arriving `now`, dropping anything older than the window.
    fn record(&mut self, now: Instant) {
        self.arrivals.push_back(now);
        while let Some(&front) = self.arrivals.front() {
            if now.duration_since(front) > NET_WINDOW {
                self.arrivals.pop_front();
            } else {
                break;
            }
        }
    }

    /// Turn the recent arrivals into a strength readout as of `now`. Combines
    /// two things a laggy link degrades: throughput (frames/sec against the
    /// host's target) and the worst recent gap between frames (a single long
    /// gap is a visible stutter even when the average rate looks fine).
    fn quality(&self, now: Instant) -> NetworkQuality {
        const GREEN: egui::Color32 = egui::Color32::from_rgb(0x2e, 0xcc, 0x71);
        const YELLOW: egui::Color32 = egui::Color32::from_rgb(0xf1, 0xc4, 0x0f);
        const RED: egui::Color32 = egui::Color32::from_rgb(0xe7, 0x4c, 0x3c);

        let count = self.arrivals.len();
        if count == 0 {
            // No frames anywhere in the window - the stream has stalled.
            return NetworkQuality { percent: 0, color: RED, label: "Stalled", fps: 0.0 };
        }

        // Measure against how long we've actually been connected, capped at
        // the window, so a fresh connection isn't punished for not yet having
        // a full window of history.
        let span = now.duration_since(self.started).min(NET_WINDOW).as_secs_f32().max(0.5);
        let fps = count as f32 / span;
        let throughput = (fps / TARGET_FPS).clamp(0.0, 1.0);

        // Worst gap between consecutive frames, including the time since the
        // most recent one (so an in-progress stall counts right away).
        let mut worst_gap_ms =
            now.duration_since(*self.arrivals.back().unwrap()).as_secs_f32() * 1000.0;
        for (a, b) in self.arrivals.iter().zip(self.arrivals.iter().skip(1)) {
            worst_gap_ms = worst_gap_ms.max(b.duration_since(*a).as_secs_f32() * 1000.0);
        }
        // Up to ~2 frame-times is still smooth; 750ms+ is a clear stall.
        const GOOD_GAP_MS: f32 = 166.0;
        const STALL_GAP_MS: f32 = 750.0;
        let smoothness =
            ((STALL_GAP_MS - worst_gap_ms) / (STALL_GAP_MS - GOOD_GAP_MS)).clamp(0.0, 1.0);

        // Mostly throughput, with jitter shaving off up to 30%.
        let score = 0.7 * throughput + 0.3 * smoothness;
        let percent = (score * 100.0).round().clamp(0.0, 100.0) as u8;

        let (color, label) = if percent >= 67 {
            (GREEN, "Strong")
        } else if percent >= 34 {
            (YELLOW, "Fair")
        } else {
            (RED, "Weak")
        };
        NetworkQuality { percent, color, label, fps }
    }
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

/// State of the "block guest input" toggle - i.e. whether *this client's*
/// mouse/keyboard input is currently being applied at the host or silently
/// dropped there (frames keep streaming either way). Same
/// pending-until-acknowledged shape as `BlankScreenStatus`, but this one
/// can't fail on the host side, so there's no error to track.
struct GuestInputStatus {
    blocked: bool,
    pending: bool,
}

/// State of the "get keyboard keystrokes" toggle - whether the host is
/// currently forwarding its own physical keystrokes to us. Same
/// pending-until-acknowledged shape as `BlankScreenStatus` (this genuinely
/// can fail: non-Windows host, or the hook failing to install there).
struct KeystrokeLogStatus {
    enabled: bool,
    pending: bool,
    error: Option<String>,
}

/// State of the "share clipboard" toggle - whether this client and the
/// host are currently mirroring clipboard changes onto each other. Same
/// pending-until-acknowledged shape as `KeystrokeLogStatus` (this can
/// genuinely fail on the host side too: no display server, a locked
/// session, etc.).
struct ClipboardSyncStatus {
    enabled: bool,
    pending: bool,
    error: Option<String>,
}

/// State of the one-shot "lock host computer" action - `pending` disables
/// the button while waiting on the host's acknowledgment (so a slow reply
/// can't be double-fired), `error` carries the reason if the last attempt
/// failed. No `enabled` field, unlike the toggles above - locking isn't a
/// state to be on or off, it's a single action.
struct LockHostStatus {
    pending: bool,
    error: Option<String>,
}

/// State of the one-shot "refresh" action - same pending/error shape as
/// `LockHostStatus`. Recreates the host's screen-capture session, for
/// when the view gets stuck showing a screen the host has since moved
/// past (see `ClientMessage::RefreshScreen`'s doc comment for why that
/// happens).
struct RefreshStatus {
    pending: bool,
    error: Option<String>,
}

/// One logged keystroke, persisted to disk. `host` is the session's
/// address (stable even if the session gets renamed later) - the log
/// panel filters the shared, on-disk history down to entries for
/// whichever host is currently being viewed.
#[derive(Clone, Serialize, Deserialize)]
struct KeystrokeEntry {
    timestamp_ms: u64,
    host: String,
    key: String,
}

/// Everything that exists only while connected: the socket, the frame feed,
/// and the background thread reading it.
struct Session {
    stream: TcpStream,
    remote_w: usize,
    remote_h: usize,
    shared: Arc<Mutex<SharedFrame>>,
    /// Frame-arrival timing, updated by the receiver thread, read each
    /// repaint to draw the network-strength readout.
    network: Arc<Mutex<NetworkStats>>,
    blank_screen: Arc<Mutex<BlankScreenStatus>>,
    guest_input: Arc<Mutex<GuestInputStatus>>,
    keystroke_log: Arc<Mutex<KeystrokeLogStatus>>,
    clipboard_sync: Arc<Mutex<ClipboardSyncStatus>>,
    /// Newly-detected local clipboard text waiting to be sent to the host -
    /// written by the clipboard-poll thread below, drained and actually
    /// sent by the UI thread each frame (see `show_connected`). Only the
    /// UI thread writes to `stream`, same single-writer design the rest of
    /// this file uses, so this indirection is what lets a second thread
    /// (the poller) hand off a value to send without needing a mutex
    /// around the socket itself.
    clipboard_outgoing: Arc<Mutex<Option<String>>>,
    lock_host: Arc<Mutex<LockHostStatus>>,
    refresh: Arc<Mutex<RefreshStatus>>,
    /// This session's address, used to tag/filter keystroke log entries -
    /// kept alongside the connection since `Session` itself doesn't know
    /// about `SessionEntry`'s `address` field.
    host_key: String,
    running: Arc<AtomicBool>,
    receiver: Option<std::thread::JoinHandle<()>>,
    /// Polls this machine's clipboard while sync is enabled - see
    /// `spawn_clipboard_poller`. Joined in `Drop` alongside `receiver`.
    clipboard_poll: Option<std::thread::JoinHandle<()>>,
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
        if let Some(h) = self.clipboard_poll.take() {
            let _ = h.join();
        }
    }
}

/// Result of a background connection attempt, sent back to the UI thread.
/// `Session` is boxed since it's much larger than `Err`'s `String` - keeps
/// this enum (and `EntryState` below, which stores the same `Session`)
/// from ballooning to the size of its biggest variant.
enum ConnectOutcome {
    Ok(Box<Session>),
    Err(String),
}

/// One saved session in the sidebar list. The address/password are kept
/// around even while disconnected so "Connect" on a previously-used entry
/// is a single click, not a re-typed form.
struct SessionEntry {
    id: u64,
    /// Optional user-chosen label, e.g. "Mom's laptop" - empty means
    /// unnamed, in which case the address is shown instead.
    name: String,
    address: String,
    password: String,
    state: EntryState,
}

/// What to show for an entry: its name if it has one, the address otherwise.
fn display_name(entry: &SessionEntry) -> &str {
    if entry.name.trim().is_empty() {
        &entry.address
    } else {
        &entry.name
    }
}

enum EntryState {
    Disconnected,
    Connecting(mpsc::Receiver<ConnectOutcome>),
    Connected(Box<Session>),
    Error(String),
}

/// An action requested from a sidebar row this frame. Collected while
/// drawing the (immutable) list, then applied afterwards so we don't need
/// to fight the borrow checker over `&self.sessions` vs `&mut self.sessions`.
enum RowAction {
    NewSession,
    Connect(u64),
    Switch(u64),
    Disconnect(u64),
    Delete(u64),
    StartRename(u64),
    CancelRename,
    SaveRename(u64, String),
}

/// The on-disk form of a `SessionEntry` - just the fields needed to
/// reconnect. Connection state never gets this far: every session comes
/// back as `Disconnected` on the next launch, ready for a one-click
/// reconnect rather than auto-reconnecting on startup.
#[derive(Serialize, Deserialize)]
struct SavedSession {
    /// Missing in files saved before naming existed - defaults to empty,
    /// same as an unnamed session.
    #[serde(default)]
    name: String,
    address: String,
    password: String,
}

/// `...\target\release\rd-client-sessions.json`, next to the executable -
/// same convention as rd-host's connection log. Falls back to a relative
/// path if the executable's own location can't be determined.
///
/// This file holds saved passwords in plain text. That matches the rest of
/// this project (the wire protocol itself is unencrypted - see README) but
/// is worth knowing before treating it as a real secrets store.
fn sessions_file_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("rd-client-sessions.json")))
        .unwrap_or_else(|| PathBuf::from("rd-client-sessions.json"))
}

/// Best-effort load: a missing or corrupt file just means "no saved
/// sessions" rather than a startup error.
fn load_saved_sessions() -> Vec<SavedSession> {
    std::fs::read_to_string(sessions_file_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Best-effort save: if it fails (read-only install location, etc.) the
/// app keeps working for this run, it just won't remember next time.
fn save_sessions(sessions: &[SessionEntry]) {
    let saved: Vec<SavedSession> = sessions
        .iter()
        .map(|e| SavedSession {
            name: e.name.clone(),
            address: e.address.clone(),
            password: e.password.clone(),
        })
        .collect();
    if let Ok(json) = serde_json::to_string_pretty(&saved) {
        if let Err(e) = std::fs::write(sessions_file_path(), json) {
            eprintln!("couldn't save sessions: {e}");
        }
    }
}

/// How long a logged keystroke sticks around before it's dropped for good.
const KEYSTROKE_RETENTION_MS: u64 = 24 * 60 * 60 * 1000; // 1 day

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `...\target\release\rd-client-keystrokes.json`, next to the executable -
/// same convention as the sessions file. Holds logged host keystrokes in
/// plain text, same caveat as the sessions file above.
fn keystrokes_file_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("rd-client-keystrokes.json")))
        .unwrap_or_else(|| PathBuf::from("rd-client-keystrokes.json"))
}

/// Drops entries older than the 1-day retention window. Returns true if
/// anything was actually removed, so callers know whether the on-disk copy
/// needs rewriting.
fn prune_keystrokes(entries: &mut Vec<KeystrokeEntry>) -> bool {
    let cutoff = now_ms().saturating_sub(KEYSTROKE_RETENTION_MS);
    let before = entries.len();
    entries.retain(|e| e.timestamp_ms >= cutoff);
    entries.len() != before
}

/// Best-effort load: a missing or corrupt file just means "no logged
/// keystrokes" rather than a startup error. Prunes on the way in, so a
/// file that sat untouched past the retention window shrinks back down
/// the moment the app next runs, not just while it's open.
fn load_keystrokes() -> Vec<KeystrokeEntry> {
    let mut entries: Vec<KeystrokeEntry> = std::fs::read_to_string(keystrokes_file_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    prune_keystrokes(&mut entries);
    entries
}

/// Best-effort save: if it fails, this run keeps working, it just won't
/// remember next time.
fn save_keystrokes(entries: &[KeystrokeEntry]) {
    if let Ok(json) = serde_json::to_string_pretty(entries) {
        if let Err(e) = std::fs::write(keystrokes_file_path(), json) {
            eprintln!("couldn't save keystroke log: {e}");
        }
    }
}

struct App {
    /// Every session the user has added this run, connected or not.
    sessions: Vec<SessionEntry>,
    next_id: u64,
    /// Which session's view is shown in the central panel. `None` means
    /// "show the new-session form" (either nothing's been added yet, or
    /// the user clicked "+ New session").
    active_id: Option<u64>,
    new_name: String,
    new_address: String,
    new_password: String,
    show_password: bool,
    /// Id + live edit buffer of the sidebar entry currently being renamed,
    /// if any.
    renaming: Option<(u64, String)>,
    /// Every logged host keystroke across all sessions, newest last -
    /// persisted to disk and pruned to the last 24 hours. Shared with each
    /// session's receiver thread (which appends to it) and the log panel
    /// (which reads it, filtered to whichever host is on screen).
    keystroke_log: Arc<Mutex<Vec<KeystrokeEntry>>>,
    /// Which session's keystroke log window is open, if any. Independent
    /// of `active_id` - the log window stays open even if you switch to
    /// viewing a different session (or that session disconnects), since
    /// the log itself doesn't depend on a live connection.
    viewing_log_id: Option<u64>,
}

impl Default for App {
    fn default() -> Self {
        let saved = load_saved_sessions();
        let sessions: Vec<SessionEntry> = saved
            .into_iter()
            .enumerate()
            .map(|(i, s)| SessionEntry {
                id: i as u64 + 1,
                name: s.name,
                address: s.address,
                password: s.password,
                state: EntryState::Disconnected,
            })
            .collect();
        let next_id = sessions.len() as u64 + 1;
        Self {
            sessions,
            next_id,
            active_id: None,
            new_name: String::new(),
            new_address: String::new(),
            new_password: String::new(),
            show_password: false,
            renaming: None,
            keystroke_log: Arc::new(Mutex::new(load_keystrokes())),
            viewing_log_id: None,
        }
    }
}

impl App {
    fn apply_action(&mut self, action: RowAction) {
        match action {
            RowAction::NewSession => {
                self.active_id = None;
                self.new_name.clear();
                self.new_address.clear();
                self.new_password.clear();
            }
            RowAction::Connect(id) => {
                if let Some(entry) = self.sessions.iter_mut().find(|e| e.id == id) {
                    let (tx, rx) = mpsc::channel();
                    spawn_connect(
                        sanitize_address(&entry.address),
                        entry.address.clone(),
                        entry.password.clone(),
                        self.keystroke_log.clone(),
                        tx,
                    );
                    entry.state = EntryState::Connecting(rx);
                }
                self.active_id = Some(id);
            }
            RowAction::Switch(id) => {
                self.active_id = Some(id);
            }
            RowAction::Disconnect(id) => {
                if let Some(entry) = self.sessions.iter_mut().find(|e| e.id == id) {
                    entry.state = EntryState::Disconnected; // drops Session -> sends Disconnect, joins thread
                }
            }
            RowAction::Delete(id) => {
                self.sessions.retain(|e| e.id != id); // Drop runs here for a Connected entry
                if self.active_id == Some(id) {
                    self.active_id = self.sessions.first().map(|e| e.id);
                }
                save_sessions(&self.sessions);
            }
            RowAction::StartRename(id) => {
                let current = self
                    .sessions
                    .iter()
                    .find(|e| e.id == id)
                    .map(|e| e.name.clone())
                    .unwrap_or_default();
                self.renaming = Some((id, current));
            }
            RowAction::CancelRename => {
                self.renaming = None;
            }
            RowAction::SaveRename(id, name) => {
                if let Some(entry) = self.sessions.iter_mut().find(|e| e.id == id) {
                    entry.name = name.trim().to_string();
                }
                self.renaming = None;
                save_sessions(&self.sessions);
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // egui::Context is a cheap Arc handle - clone it once so it can be
        // used freely below without fighting the `ui: &mut Ui` borrow.
        let ctx = ui.ctx().clone();

        // Poll every in-flight connection attempt, not just the active
        // one - a session being connected in the background should finish
        // connecting even while some other session is on screen.
        for entry in &mut self.sessions {
            if let EntryState::Connecting(rx) = &entry.state {
                if let Ok(outcome) = rx.try_recv() {
                    entry.state = match outcome {
                        ConnectOutcome::Ok(session) => EntryState::Connected(session),
                        ConnectOutcome::Err(e) => EntryState::Error(e),
                    };
                }
            }
        }

        // Same for connections that die on their own (host closed the
        // socket, network dropped, etc.) - the receiver thread notices via
        // `running` regardless of whether this session is the one on
        // screen right now.
        for entry in &mut self.sessions {
            if let EntryState::Connected(session) = &entry.state {
                if !session.running.load(Ordering::Relaxed) {
                    entry.state = EntryState::Disconnected;
                }
            }
        }

        // Age out keystroke log entries past the 1-day retention window.
        // Cheap enough to just do every frame (a retain() over what's
        // realistically at most a few thousand entries) rather than
        // throttling it - only actually rewrites the file when something
        // was removed.
        {
            let mut log = self.keystroke_log.lock().unwrap();
            if prune_keystrokes(&mut log) {
                save_keystrokes(&log);
            }
        }

        let mut action = None;
        egui::Panel::left("sidebar")
            .resizable(false)
            .exact_size(240.0)
            .show(ui, |ui| {
                action = show_sidebar(ui, &self.sessions, self.active_id, &mut self.renaming);
            });
        if let Some(action) = action {
            self.apply_action(action);
        }

        // Look up the active entry's index (a plain usize, not a borrow)
        // before deciding what to draw in the central panel - keeps the
        // borrow of `self.sessions` from spanning both match arms below.
        let active_idx = self
            .active_id
            .and_then(|id| self.sessions.iter().position(|e| e.id == id));

        egui::CentralPanel::default().show(ui, |ui| match active_idx {
            Some(idx) => {
                let entry = &mut self.sessions[idx];
                // Computed up front, before matching &mut entry.state below -
                // display_name() needs a shared borrow of the whole entry,
                // which would conflict with a match arm still holding a
                // mutable borrow into entry.state (e.g. EntryState::Error's
                // bound message).
                let title = display_name(entry).to_string();
                let entry_id = entry.id;
                match &mut entry.state {
                    EntryState::Disconnected => {
                        let mut reconnect = false;
                        ui.vertical_centered(|ui| {
                            ui.add_space(ui.available_height() / 2.0 - 40.0);
                            ui.label(format!("Disconnected from {title}"));
                            ui.add_space(10.0);
                            if ui
                                .add(egui::Button::new("Reconnect").min_size([120.0, 32.0].into()))
                                .clicked()
                            {
                                reconnect = true;
                            }
                        });
                        if reconnect {
                            let (tx, rx) = mpsc::channel();
                            spawn_connect(
                                sanitize_address(&entry.address),
                                entry.address.clone(),
                                entry.password.clone(),
                                self.keystroke_log.clone(),
                                tx,
                            );
                            entry.state = EntryState::Connecting(rx);
                        }
                    }
                    EntryState::Connecting(_) => {
                        ui.centered_and_justified(|ui| ui.label("Connecting..."));
                    }
                    EntryState::Connected(session) => {
                        if !show_connected(
                            ui,
                            &ctx,
                            &title,
                            &self.keystroke_log,
                            &mut self.viewing_log_id,
                            entry_id,
                            session,
                        ) {
                            entry.state = EntryState::Disconnected;
                        }
                    }
                    EntryState::Error(e) => {
                        // Clone the error text out before the closure below
                        // borrows it - the closure also needs to reassign
                        // `entry.state` on Retry, which it couldn't do while
                        // still holding a borrow into that same state.
                        let message = e.clone();
                        let mut retry = false;
                        ui.vertical_centered(|ui| {
                            ui.add_space(ui.available_height() / 2.0 - 50.0);
                            ui.label(format!("Couldn't connect to {title}"));
                            ui.add_space(6.0);
                            ui.colored_label(egui::Color32::LIGHT_RED, message);
                            ui.add_space(10.0);
                            if ui
                                .add(egui::Button::new("Retry").min_size([120.0, 32.0].into()))
                                .clicked()
                            {
                                retry = true;
                            }
                        });
                        if retry {
                            let (tx, rx) = mpsc::channel();
                            spawn_connect(
                                sanitize_address(&entry.address),
                                entry.address.clone(),
                                entry.password.clone(),
                                self.keystroke_log.clone(),
                                tx,
                            );
                            entry.state = EntryState::Connecting(rx);
                        }
                    }
                }
            }
            None => {
                let clicked_connect = show_connect_form(
                    ui,
                    &mut self.new_name,
                    &mut self.new_address,
                    &mut self.new_password,
                    &mut self.show_password,
                    false,
                    &None,
                );
                if clicked_connect {
                    let name = self.new_name.trim().to_string();
                    let address = sanitize_address(&self.new_address);
                    let password = self.new_password.trim().to_string();
                    let (tx, rx) = mpsc::channel();
                    spawn_connect(
                        address.clone(),
                        address.clone(),
                        password.clone(),
                        self.keystroke_log.clone(),
                        tx,
                    );
                    let id = self.next_id;
                    self.next_id += 1;
                    self.sessions.push(SessionEntry {
                        id,
                        name,
                        address,
                        password,
                        state: EntryState::Connecting(rx),
                    });
                    self.active_id = Some(id);
                    save_sessions(&self.sessions);
                }
            }
        });

        // The keystroke log window floats independent of the central
        // panel - it stays open even if you switch to a different
        // session, or the one it's showing disconnects, since the log
        // itself doesn't depend on a live connection.
        show_keystroke_log_window(&ctx, &self.sessions, &self.keystroke_log, &mut self.viewing_log_id);

        // Keep polling for network frames / connect results even with no
        // user input.
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }
}

/// Renders the floating "keystroke log" window for whichever session
/// `viewing_log_id` points at, if any - closes itself (clearing
/// `viewing_log_id`) via its own titlebar close button, or if that
/// session gets deleted out from under it.
fn show_keystroke_log_window(
    ctx: &egui::Context,
    sessions: &[SessionEntry],
    keystroke_log: &Arc<Mutex<Vec<KeystrokeEntry>>>,
    viewing_log_id: &mut Option<u64>,
) {
    let Some(id) = *viewing_log_id else { return };
    let Some(entry) = sessions.iter().find(|e| e.id == id) else {
        *viewing_log_id = None;
        return;
    };
    let title = display_name(entry).to_string();
    let host_key = entry.address.clone();

    let mut open = true;
    egui::Window::new(format!("Keystroke log - {title}"))
        .id(egui::Id::new("keystroke_log_window"))
        .open(&mut open)
        .default_size([440.0, 340.0])
        .collapsible(false)
        .show(ctx, |ui| {
            ui.label(egui::RichText::new("Last 24 hours").weak());
            ui.add_space(4.0);
            egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(
                ui,
                |ui| {
                    let log = keystroke_log.lock().unwrap();
                    let mut entries: Vec<&KeystrokeEntry> =
                        log.iter().filter(|e| e.host == host_key).collect();
                    entries.sort_by_key(|e| e.timestamp_ms);
                    if entries.is_empty() {
                        ui.weak("No keystrokes logged for this host in the last 24 hours.");
                    } else {
                        for entry in entries {
                            ui.monospace(format!(
                                "{}  {}",
                                format_log_time(entry.timestamp_ms),
                                entry.key
                            ));
                        }
                    }
                },
            );
        });
    if !open {
        *viewing_log_id = None;
    }
}

/// Draws the sidebar's session list and returns the action (if any) the
/// user requested this frame by clicking a row's button. `renaming` is the
/// live edit buffer for whichever entry is mid-rename, if any - it's
/// mutated directly here (as the user types) rather than via `RowAction`,
/// which only fires once, on Save/Cancel/click.
fn show_sidebar(
    ui: &mut egui::Ui,
    sessions: &[SessionEntry],
    active_id: Option<u64>,
    renaming: &mut Option<(u64, String)>,
) -> Option<RowAction> {
    let mut action = None;

    ui.add_space(6.0);
    ui.heading("Sessions");
    ui.add_space(6.0);
    if ui
        .add(egui::Button::new("+ New session").min_size([0.0, 26.0].into()))
        .clicked()
    {
        action = Some(RowAction::NewSession);
    }
    ui.add_space(8.0);
    ui.separator();

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if sessions.is_empty() {
            ui.add_space(12.0);
            ui.weak("No sessions yet.");
        }

        for entry in sessions {
            let is_active = active_id == Some(entry.id);
            ui.add_space(6.0);

            egui::Frame::default()
                .fill(if is_active {
                    ui.visuals().selection.bg_fill
                } else {
                    egui::Color32::TRANSPARENT
                })
                .inner_margin(6.0)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());

                    let is_renaming = renaming.as_ref().map(|(id, _)| *id) == Some(entry.id);
                    if is_renaming {
                        let (_, buf) = renaming.as_mut().unwrap();
                        ui.add(
                            egui::TextEdit::singleline(buf)
                                .hint_text("Name (optional)")
                                .desired_width(ui.available_width()),
                        );
                        ui.horizontal(|ui| {
                            if ui.small_button("Save").clicked() {
                                action = Some(RowAction::SaveRename(entry.id, buf.clone()));
                            }
                            if ui.small_button("Cancel").clicked() {
                                action = Some(RowAction::CancelRename);
                            }
                        });
                    } else {
                        ui.label(egui::RichText::new(display_name(entry)).strong());
                        // If it's got a name, show the address too, smaller -
                        // otherwise display_name() already showed the address
                        // above, so repeating it here would be redundant.
                        if !entry.name.trim().is_empty() {
                            ui.weak(&entry.address);
                        }

                        let (status_text, status_color) = match &entry.state {
                            EntryState::Disconnected => ("Disconnected", egui::Color32::GRAY),
                            EntryState::Connecting(_) => ("Connecting...", egui::Color32::YELLOW),
                            EntryState::Connected(_) => ("Connected", egui::Color32::LIGHT_GREEN),
                            EntryState::Error(_) => ("Error", egui::Color32::LIGHT_RED),
                        };
                        ui.colored_label(status_color, status_text);

                        // One button per row rather than packed side by
                        // side - this sidebar is narrow enough that a
                        // horizontal row of 3+ buttons got cramped/wrapped.
                        ui.vertical(|ui| {
                            ui.spacing_mut().button_padding = egui::vec2(10.0, 6.0);
                            ui.spacing_mut().item_spacing.y = 4.0;
                            let full_width = egui::vec2(ui.available_width(), 0.0);

                            match &entry.state {
                                EntryState::Disconnected | EntryState::Error(_) => {
                                    if ui.add(egui::Button::new("Connect").min_size(full_width)).clicked() {
                                        action = Some(RowAction::Connect(entry.id));
                                    }
                                }
                                EntryState::Connecting(_) => {
                                    ui.add_enabled(
                                        false,
                                        egui::Button::new("Connect").min_size(full_width),
                                    );
                                }
                                EntryState::Connected(_) => {
                                    if !is_active
                                        && ui.add(egui::Button::new("Switch").min_size(full_width)).clicked()
                                    {
                                        action = Some(RowAction::Switch(entry.id));
                                    }
                                    if ui.add(egui::Button::new("Disconnect").min_size(full_width)).clicked() {
                                        action = Some(RowAction::Disconnect(entry.id));
                                    }
                                }
                            }
                            if ui.add(egui::Button::new("Rename").min_size(full_width)).clicked() {
                                action = Some(RowAction::StartRename(entry.id));
                            }
                            if ui.add(egui::Button::new("Delete").min_size(full_width)).clicked() {
                                action = Some(RowAction::Delete(entry.id));
                            }
                        });
                    }
                });
        }
    });

    action
}

/// Draws the connect form. Returns true the frame the user clicks Connect
/// with valid input.
fn show_connect_form(
    ui: &mut egui::Ui,
    name: &mut String,
    address: &mut String,
    password: &mut String,
    show_password: &mut bool,
    connecting: bool,
    error: &Option<String>,
) -> bool {
    let mut clicked_connect = false;
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() / 2.0 - 100.0);
        ui.heading("rd-client");
        ui.add_space(16.0);

        // `egui::Grid` doesn't behave like a single sized widget the way
        // `heading`/`button` do, so `vertical_centered` alone won't center
        // it - it anchors to the panel's left edge instead. Wrapping it in
        // a fixed-size `allocate_ui` gives it a known size to be centered
        // as a block, same as everything else in this column.
        ui.allocate_ui(egui::vec2(390.0, 110.0), |ui| {
            egui::Grid::new("connect_form")
                .num_columns(2)
                .spacing([8.0, 10.0])
                .show(ui, |ui| {
                    ui.label("Name");
                    ui.add_enabled(
                        !connecting,
                        egui::TextEdit::singleline(name)
                            .hint_text("optional, e.g. Mom's laptop")
                            .desired_width(300.0),
                    );
                    ui.end_row();

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

/// Renders a logged keystroke's timestamp as a local `HH:MM:SS`, or a
/// placeholder if the stored millisecond value is somehow out of range.
fn format_log_time(timestamp_ms: u64) -> String {
    let secs = (timestamp_ms / 1000) as i64;
    let nsecs = ((timestamp_ms % 1000) * 1_000_000) as u32;
    match chrono::DateTime::from_timestamp(secs, nsecs) {
        Some(utc) => {
            let local: chrono::DateTime<chrono::Local> = utc.into();
            local.format("%H:%M:%S").to_string()
        }
        None => "??:??:??".to_string(),
    }
}

/// Renders the connected view (remote screen + controls).
/// Returns false when the caller should drop back to the connect form
/// (user clicked Disconnect, or the connection died).
#[allow(clippy::too_many_arguments)]
fn show_connected(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    title: &str,
    keystroke_log: &Arc<Mutex<Vec<KeystrokeEntry>>>,
    viewing_log_id: &mut Option<u64>,
    entry_id: u64,
    session: &mut Session,
) -> bool {
    if !session.running.load(Ordering::Relaxed) {
        return false; // receiver thread ended - connection dropped
    }

    // If the clipboard poller (its own thread - see `spawn_clipboard_poller`)
    // noticed a local clipboard change since last frame, send it now. Only
    // the UI thread writes to `session.stream`, so the poller can't just
    // send it directly - it hands the value off here instead.
    if let Some(text) = session.clipboard_outgoing.lock().unwrap().take() {
        let _ = write_message(&mut session.stream, &ClientMessage::ClipboardUpdate { text });
    }

    let mut keep_going = true;

    // Collects errors from the toggles below (any of the fallible ones can
    // report one) so they're shown together underneath the button row,
    // rather than each toggle needing its own space to grow into.
    let mut errors: Vec<String> = Vec::new();

    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 6.0;

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(title).strong());
            ui.label(format!("Remote: {}x{}", session.remote_w, session.remote_h));

            // Network-strength readout, pushed to the right edge of the row.
            // Green/yellow/red with a percentage, indicating how smoothly the
            // remote screen is keeping up (higher = smoother).
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let q = session.network.lock().unwrap().quality(Instant::now());
                ui.label(
                    egui::RichText::new(format!("\u{25cf} {}%  {}", q.percent, q.label))
                        .color(q.color),
                )
                .on_hover_text(format!(
                    "Network strength - how smoothly the remote screen is keeping up.\n\
                     Receiving {:.0} fps (the host streams ~{:.0}).\n\
                     Green = strong, yellow = fair, red = weak.",
                    q.fps, TARGET_FPS,
                ));
            });
        });
        if let Some(e) = &session.error {
            ui.colored_label(egui::Color32::LIGHT_RED, e);
        }
        ui.add_space(4.0);

        // All the session action buttons in one row - wraps to a second
        // line rather than clipping if the window's too narrow to fit
        // them all.
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().button_padding = egui::vec2(12.0, 8.0);
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);

            if ui.button("Disconnect").clicked() {
                keep_going = false;
            }

            // "Refresh": one-shot action that asks the host to recreate
            // its screen-capture session. For when the view gets stuck
            // showing a screen the host has since moved past - a known
            // limitation of the underlying capture API (DXGI Desktop
            // Duplication on Windows), which can stop producing new
            // frames after certain host-side desktop transitions (a
            // lock/unlock, the blank-screen overlay, a UAC prompt) even
            // though the real screen has moved on.
            let (pending, err) = {
                let status = session.refresh.lock().unwrap();
                (status.pending, status.error.clone())
            };
            let label = if pending { "Refresh: ..." } else { "Refresh" };
            if ui.add_enabled(!pending, egui::Button::new(label)).clicked() {
                session.refresh.lock().unwrap().pending = true;
                let _ = write_message(&mut session.stream, &ClientMessage::RefreshScreen);
            }
            if let Some(e) = err {
                errors.push(format!("Refresh: {e}"));
            }

            // "Blank host screen" toggle: hides the real desktop from
            // anyone physically at the host machine (shows a fake
            // "Working on updates" screen there instead) while this
            // client keeps seeing/controlling it. Host-side support is
            // Windows-only; other hosts reply with an error, shown below
            // the row rather than the toggle just silently doing nothing.
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
                errors.push(format!("Blank host screen: {e}"));
            }

            // "Block guest input" toggle: while on, this client's mouse/
            // keyboard stop reaching the host (it keeps streaming frames
            // as normal) - a "view only" mode for when you just want to
            // watch without risking an accidental click or keystroke
            // landing on the host.
            let (label, pending, blocked) = {
                let status = session.guest_input.lock().unwrap();
                let label = if status.pending {
                    "Block guest input: ..."
                } else if status.blocked {
                    "Block guest input: On"
                } else {
                    "Block guest input: Off"
                };
                (label, status.pending, status.blocked)
            };
            if ui.add_enabled(!pending, egui::Button::new(label)).clicked() {
                session.guest_input.lock().unwrap().pending = true;
                let _ = write_message(
                    &mut session.stream,
                    &ClientMessage::BlockGuestInput { blocked: !blocked },
                );
            }

            // "Get keyboard keystrokes" toggle: while on, the host
            // forwards its own physical keystrokes here, persisted to
            // disk and kept for 1 day. Host-side support is Windows-only,
            // same caveat as blank screen. The log itself lives in a
            // separate window (see "View Logs" below) rather than inline
            // here, so it doesn't push the remote view around.
            let (label, pending, enabled, err) = {
                let status = session.keystroke_log.lock().unwrap();
                let label = if status.pending {
                    "Get keyboard keystrokes: ..."
                } else if status.enabled {
                    "Get keyboard keystrokes: On"
                } else {
                    "Get keyboard keystrokes: Off"
                };
                (label, status.pending, status.enabled, status.error.clone())
            };
            if ui.add_enabled(!pending, egui::Button::new(label)).clicked() {
                session.keystroke_log.lock().unwrap().pending = true;
                let _ = write_message(
                    &mut session.stream,
                    &ClientMessage::ToggleKeystrokeLog { enabled: !enabled },
                );
            }
            if let Some(e) = err {
                errors.push(format!("Get keyboard keystrokes: {e}"));
            }

            // "Share clipboard" toggle: while on, whichever side's
            // clipboard changes gets mirrored onto the other's - lets a
            // paste on one end pick up something just copied on the
            // other. Cross-platform on the host side (unlike blank
            // screen/keystrokes), but can still fail there (no display
            // server, a locked session, etc.), same reply shape.
            let (label, pending, enabled, err) = {
                let status = session.clipboard_sync.lock().unwrap();
                let label = if status.pending {
                    "Share clipboard: ..."
                } else if status.enabled {
                    "Share clipboard: On"
                } else {
                    "Share clipboard: Off"
                };
                (label, status.pending, status.enabled, status.error.clone())
            };
            if ui.add_enabled(!pending, egui::Button::new(label)).clicked() {
                session.clipboard_sync.lock().unwrap().pending = true;
                let _ = write_message(
                    &mut session.stream,
                    &ClientMessage::ToggleClipboardSync { enabled: !enabled },
                );
            }
            if let Some(e) = err {
                errors.push(format!("Share clipboard: {e}"));
            }

            // "Lock host computer": one-shot action (not a toggle, unlike
            // everything above) that immediately locks the host's desktop
            // session - the same effect as pressing Win+L there. Meant for
            // e.g. locking your own home laptop back up once you're done
            // working on it remotely. Windows-only on the host side.
            let (pending, err) = {
                let status = session.lock_host.lock().unwrap();
                (status.pending, status.error.clone())
            };
            let label = if pending { "Lock host computer: ..." } else { "Lock host computer" };
            if ui.add_enabled(!pending, egui::Button::new(label)).clicked() {
                session.lock_host.lock().unwrap().pending = true;
                let _ = write_message(&mut session.stream, &ClientMessage::LockHost);
            }
            if let Some(e) = err {
                errors.push(format!("Lock host computer: {e}"));
            }

            let log_count = keystroke_log
                .lock()
                .unwrap()
                .iter()
                .filter(|e| e.host == session.host_key)
                .count();
            if ui.button(format!("View Logs ({log_count})")).clicked() {
                *viewing_log_id = Some(entry_id);
            }
        });

        for e in &errors {
            ui.colored_label(egui::Color32::LIGHT_RED, e);
        }

        ui.add_space(4.0);
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
/// responsive; the result comes back over `tx`. `host_key` tags this
/// session's keystroke log entries (its address, stable across renames).
fn spawn_connect(
    address: String,
    host_key: String,
    password: String,
    keystroke_log: Arc<Mutex<Vec<KeystrokeEntry>>>,
    tx: mpsc::Sender<ConnectOutcome>,
) {
    std::thread::spawn(move || {
        let outcome = connect(&address, &password)
            .map(|(stream, w, h)| {
                let shared = Arc::new(Mutex::new(SharedFrame {
                    width: 0,
                    height: 0,
                    rgb: Vec::new(),
                    version: 0,
                }));
                let network = Arc::new(Mutex::new(NetworkStats::new()));
                let blank_screen = Arc::new(Mutex::new(BlankScreenStatus {
                    enabled: false,
                    pending: false,
                    error: None,
                }));
                let guest_input = Arc::new(Mutex::new(GuestInputStatus {
                    blocked: false,
                    pending: false,
                }));
                let keystroke_status = Arc::new(Mutex::new(KeystrokeLogStatus {
                    enabled: false,
                    pending: false,
                    error: None,
                }));
                let clipboard_status = Arc::new(Mutex::new(ClipboardSyncStatus {
                    enabled: false,
                    pending: false,
                    error: None,
                }));
                let clipboard_outgoing: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
                // Not a `Session` field - only the poller (below) and the
                // receiver thread (applying incoming updates) need it, to
                // keep each other from re-detecting/echoing the other's
                // change back. See `SessionHandles::clipboard_last_known`.
                let clipboard_last_known: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
                let lock_host_status = Arc::new(Mutex::new(LockHostStatus { pending: false, error: None }));
                let refresh_status = Arc::new(Mutex::new(RefreshStatus { pending: false, error: None }));
                let running = Arc::new(AtomicBool::new(true));
                let receiver = spawn_frame_receiver(
                    &stream,
                    SessionHandles {
                        shared: shared.clone(),
                        network: network.clone(),
                        blank_screen: blank_screen.clone(),
                        guest_input: guest_input.clone(),
                        keystroke_status: keystroke_status.clone(),
                        keystroke_log,
                        host_key: host_key.clone(),
                        clipboard_sync: clipboard_status.clone(),
                        clipboard_last_known: clipboard_last_known.clone(),
                        lock_host: lock_host_status.clone(),
                        refresh: refresh_status.clone(),
                    },
                    running.clone(),
                );
                let clipboard_poll = spawn_clipboard_poller(
                    clipboard_status.clone(),
                    clipboard_outgoing.clone(),
                    clipboard_last_known,
                    running.clone(),
                );
                Session {
                    stream,
                    remote_w: w,
                    remote_h: h,
                    shared,
                    network,
                    blank_screen,
                    guest_input,
                    keystroke_log: keystroke_status,
                    clipboard_sync: clipboard_status,
                    clipboard_outgoing,
                    lock_host: lock_host_status,
                    refresh: refresh_status,
                    host_key,
                    running,
                    receiver: Some(receiver),
                    clipboard_poll: Some(clipboard_poll),
                    texture: None,
                    uploaded_version: 0,
                    last_mouse: None,
                    mouse_down: [false; 3],
                    keys_down: Default::default(),
                    modifiers_down: Default::default(),
                    error: None,
                }
            })
            .map(|session| ConnectOutcome::Ok(Box::new(session)))
            .unwrap_or_else(|e| ConnectOutcome::Err(e.to_string()));
        let _ = tx.send(outcome);
    });
}

/// The shared state a session's frame-receiver thread updates as
/// `HostMessage`s arrive - bundled into one struct so `spawn_frame_receiver`
/// takes a handful of arguments instead of one per status handle.
struct SessionHandles {
    shared: Arc<Mutex<SharedFrame>>,
    network: Arc<Mutex<NetworkStats>>,
    blank_screen: Arc<Mutex<BlankScreenStatus>>,
    guest_input: Arc<Mutex<GuestInputStatus>>,
    keystroke_status: Arc<Mutex<KeystrokeLogStatus>>,
    /// Shared across all sessions (not just this one) - filtered by
    /// `host_key` when read, same as everywhere else this log is used.
    keystroke_log: Arc<Mutex<Vec<KeystrokeEntry>>>,
    host_key: String,
    clipboard_sync: Arc<Mutex<ClipboardSyncStatus>>,
    /// Shared with `spawn_clipboard_poller` - see that fn's comment and
    /// `spawn_connect`'s local of the same name for why.
    clipboard_last_known: Arc<Mutex<Option<String>>>,
    lock_host: Arc<Mutex<LockHostStatus>>,
    refresh: Arc<Mutex<RefreshStatus>>,
}

fn spawn_frame_receiver(
    stream: &TcpStream,
    handles: SessionHandles,
    running: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    let SessionHandles {
        shared,
        network,
        blank_screen,
        guest_input,
        keystroke_status,
        keystroke_log,
        host_key,
        clipboard_sync,
        clipboard_last_known,
        lock_host,
        refresh,
    } = handles;
    let mut reader_stream = stream.try_clone().expect("clone socket");
    let version_counter = Arc::new(AtomicU64::new(0));
    std::thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            let msg: Result<HostMessage, _> = read_message(&mut reader_stream);
            match msg {
                Ok(HostMessage::Frame { width, height, jpeg_bytes }) => {
                    // Timestamp every frame that arrives (even if it fails to
                    // decode below) - arrival timing is what the strength
                    // readout is measuring, independent of decode success.
                    network.lock().unwrap().record(Instant::now());
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
                Ok(HostMessage::GuestInputBlockedResult { blocked }) => {
                    let mut status = guest_input.lock().unwrap();
                    status.pending = false;
                    status.blocked = blocked;
                }
                Ok(HostMessage::KeystrokeLogResult { enabled, ok, reason }) => {
                    let mut status = keystroke_status.lock().unwrap();
                    status.pending = false;
                    status.enabled = enabled;
                    status.error = if ok { None } else { reason };
                }
                Ok(HostMessage::Keystroke { key, timestamp_ms }) => {
                    let mut log = keystroke_log.lock().unwrap();
                    log.push(KeystrokeEntry { timestamp_ms, host: host_key.clone(), key });
                    save_keystrokes(&log);
                }
                Ok(HostMessage::ClipboardSyncResult { enabled, ok, reason }) => {
                    let mut status = clipboard_sync.lock().unwrap();
                    status.pending = false;
                    status.enabled = enabled;
                    status.error = if ok { None } else { reason };
                }
                Ok(HostMessage::ClipboardUpdate { text }) => {
                    if clipboard_sync.lock().unwrap().enabled {
                        // Mark it known *before* applying it locally, so
                        // the poll thread's next tick sees its own read
                        // already matches and doesn't echo it straight
                        // back to the host.
                        *clipboard_last_known.lock().unwrap() = Some(text.clone());
                        if let Ok(mut clipboard) = arboard::Clipboard::new() {
                            let _ = clipboard.set_text(text);
                        }
                    }
                }
                Ok(HostMessage::LockHostResult { ok, reason }) => {
                    let mut status = lock_host.lock().unwrap();
                    status.pending = false;
                    status.error = if ok { None } else { reason };
                }
                Ok(HostMessage::RefreshScreenResult { ok, reason }) => {
                    let mut status = refresh.lock().unwrap();
                    status.pending = false;
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

/// Polls this client's OS clipboard while `clipboard_sync.enabled` is set,
/// handing each newly-detected value off via `outgoing` for the UI thread
/// to actually send (see `Session::clipboard_outgoing` and
/// `show_connected`, which drains it). A single thread runs for the whole
/// life of the session regardless of the toggle's state - flipping the
/// toggle just changes whether this loop's tick does anything, rather than
/// starting/stopping a thread each time (there's no OS resource being
/// held here the way `blank_screen`'s overlay window or the host's
/// keyboard hook are, so there's nothing to actually tear down between
/// toggles).
///
/// `last_known` is shared with the receiver thread applying incoming
/// updates (see `spawn_frame_receiver`'s `ClipboardUpdate` handling) so
/// this poller doesn't immediately re-detect a remote update just applied
/// to this machine's clipboard and echo it straight back to the host.
fn spawn_clipboard_poller(
    clipboard_sync: Arc<Mutex<ClipboardSyncStatus>>,
    outgoing: Arc<Mutex<Option<String>>>,
    last_known: Arc<Mutex<Option<String>>>,
    running: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            if clipboard_sync.lock().unwrap().enabled {
                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                    // Errors here just mean "nothing text-shaped to sync
                    // this tick" (empty clipboard, or a non-text format
                    // like an image) - not a real failure.
                    if let Ok(text) = clipboard.get_text() {
                        let mut last = last_known.lock().unwrap();
                        if last.as_deref() != Some(text.as_str()) {
                            *last = Some(text.clone());
                            drop(last);
                            *outgoing.lock().unwrap() = Some(text);
                        }
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
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

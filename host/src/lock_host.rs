//! The "lock host computer" feature: a one-shot action - unlike everything
//! else in this app - that locks the host machine's desktop session, the
//! same effect as physically pressing Win+L there. Meant for locking your
//! own machine back up once you're done working on it remotely, without
//! needing to be standing in front of it.
//!
//! Nothing to enable/disable or tear down here (unlike `blank_screen.rs`,
//! `keylogger.rs`, `clipboard_sync.rs`): it's a single call, not a
//! background resource, so `main.rs` just calls `lock()` directly from
//! `handle_client` when a `ClientMessage::LockHost` arrives.
//!
//! Windows-only: there's no equivalent "lock this session" call wired up
//! here for macOS/Linux.
//!
//! Once locked, mouse/keyboard input from the client generally stops
//! having any effect - Windows switches to the secure desktop for the
//! lock screen, which (by design, the same protection that stops
//! screen-scraping a password prompt) isn't reachable by simulated input
//! from a background process. That's expected, not a bug to work around:
//! the whole point of locking is that nobody (including this app) can get
//! back in without the real credentials.

#[cfg(windows)]
pub fn lock() -> Result<(), String> {
    // The exact same call physically pressing Win+L makes. Locks whichever
    // desktop session this process is running in - requires an interactive
    // session (true for rd-host's normal tray/GUI modes; a session-0
    // service has none, which this reports rather than silently no-op'ing).
    let ok = unsafe { winapi::um::winuser::LockWorkStation() };
    if ok == 0 {
        let err = unsafe { winapi::um::errhandlingapi::GetLastError() };
        Err(format!("LockWorkStation failed (error {err})"))
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
pub fn lock() -> Result<(), String> {
    Err("locking the host isn't supported on this platform yet (Windows-only)".to_string())
}

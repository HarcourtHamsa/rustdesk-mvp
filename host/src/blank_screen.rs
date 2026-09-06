//! The "blank host screen" privacy feature: while toggled on, anyone
//! physically at the host machine sees a full-screen fake "Working on
//! updates" Windows Update screen (spinner, slow-crawling percentage,
//! "Don't turn off your computer") instead of the real desktop, while the
//! connected client keeps seeing and controlling the real thing.
//!
//! This works by creating a topmost, fullscreen local window and marking
//! it with `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` - a Windows
//! API (10, version 2004+) that excludes a specific window from anything
//! that captures the screen, including the DXGI Desktop Duplication API
//! our own `scrap`-based capture thread uses. So our capture keeps seeing
//! the real desktop underneath, while a person looking at the physical
//! monitor sees this window on top of it.
//!
//! It also hides the real cursor while active (via a temporary system
//! cursor swap, not just a window-local one) - otherwise the cursor
//! moving around on its own, driven by the remote client's input, would
//! be the one visible giveaway on an otherwise blanked screen.
//!
//! Crucially, this window is also mouse- and keyboard-transparent
//! (`WS_EX_TRANSPARENT` + returning `HTTRANSPARENT` from `WM_NCHITTEST`,
//! `WS_EX_NOACTIVATE` + `SW_SHOWNOACTIVATE`) - without that, a topmost
//! window covering the whole screen would swallow every click before it
//! reaches the real desktop, and would steal keyboard focus from whatever
//! the client was typing into, breaking remote control entirely while
//! blanked (exactly the failure mode this was fixed to avoid).
//!
//! (An alternative approach - actually powering the monitor off via
//! SC_MONITORPOWER instead of drawing an overlay - was tried and reverted:
//! it eliminates the click-passthrough question entirely since there's no
//! window in the way, but empirically it also stops our own DXGI capture
//! from producing new frames at all once the display really sleeps, which
//! breaks the client's video feed - a worse regression than the one it
//! was meant to fix.)
//!
//! Windows-only: there's no equivalent capture-exclusion mechanism wired
//! up for macOS/Linux here, so `enable()` just reports it's unsupported
//! on other platforms.

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use winapi::shared::minwindef::{LPARAM, LRESULT, UINT, WPARAM};
    use winapi::shared::windef::{HBRUSH, HDC, HWND, RECT};
    use winapi::shared::winerror::ERROR_CLASS_ALREADY_EXISTS;
    use winapi::um::errhandlingapi::GetLastError;
    use winapi::um::libloaderapi::GetModuleHandleW;
    use winapi::um::wingdi::{
        CreateFontW, CreateSolidBrush, DeleteObject, Ellipse, GetStockObject, SelectObject,
        SetBkMode, SetTextColor, RGB, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
        DEFAULT_PITCH, FF_SWISS, FW_NORMAL, NULL_PEN, OUT_DEFAULT_PRECIS, TRANSPARENT,
    };
    use winapi::shared::windef::HCURSOR;
    use winapi::um::winuser::{
        BeginPaint, CreateCursor, DefWindowProcW, DispatchMessageW, DrawTextW, EndPaint,
        GetClientRect, GetMessageW, GetSystemMetrics, InvalidateRect, KillTimer, LoadCursorW,
        PostMessageW, PostQuitMessage, RegisterClassW, SetCursor, SetProcessDPIAware,
        SetSystemCursor, SetTimer, ShowCursor, ShowWindow, SystemParametersInfoW,
        TranslateMessage, UpdateWindow, CW_USEDEFAULT, DT_CENTER, DT_SINGLELINE, DT_VCENTER,
        HTTRANSPARENT, IDC_ARROW, MSG, SM_CXSCREEN, SM_CYSCREEN, SPI_SETCURSORS,
        SW_SHOWNOACTIVATE, WM_CLOSE, WM_DESTROY, WM_NCHITTEST, WM_PAINT, WM_SETCURSOR, WM_TIMER,
        WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    };
    use winapi::um::winuser::{CreateWindowExW, SetWindowDisplayAffinity};

    /// Windows Update's signature blue (#0078D7), used as this overlay's
    /// background - the whole point is to read as a real system screen.
    const BACKGROUND_COLOR: (u8, u8, u8) = (0, 120, 215);

    /// Id for the periodic repaint timer (only one timer on this window,
    /// so any nonzero value works).
    const ANIMATION_TIMER_ID: usize = 1;
    /// ~20fps - smooth enough for the spinner, cheap enough not to matter.
    const ANIMATION_TIMER_MS: u32 = 50;

    /// How long the fake progress percentage sits on each number - "very
    /// slow" is the point, so anyone glancing at it reads it as a real,
    /// unremarkable update rather than a prop counting up suspiciously
    /// fast. Caps at 99% (see `fake_percent`) rather than looping back to
    /// 0, matching how real Windows updates are broadly remembered for
    /// sitting at 99% seemingly forever.
    const SECONDS_PER_PERCENT: f64 = 5.0;

    /// When the currently-showing overlay's animation/percent clock
    /// started - reset every time a new overlay window is created, so each
    /// activation starts its own fake update back at 0%. A `Mutex` guards
    /// it since `window_proc` (reading it, on the overlay's own thread) and
    /// `create_overlay_window` (setting it) both touch it, same reasoning
    /// as `blank_screen.rs`'s sibling modules use their own statics for.
    static ANIMATION_START: Mutex<Option<Instant>> = Mutex::new(None);

    /// The fake progress percentage at `elapsed_secs` into this
    /// activation - see `SECONDS_PER_PERCENT`.
    fn fake_percent(elapsed_secs: f64) -> u32 {
        ((elapsed_secs / SECONDS_PER_PERCENT) as u32).min(99)
    }

    /// Added in Windows 10 2004 (SDK 10.0.19041) - not yet in winapi 0.3.9's
    /// constant list, though the function that takes it is. Just a number.
    const WDA_EXCLUDEFROMCAPTURE: u32 = 0x00000011;

    /// The system's default arrow cursor slot - also not in winapi 0.3.9's
    /// constant list (it predates the crate; these OCR_* IDs are ancient).
    const OCR_NORMAL: u32 = 32512;

    const LINE_1: &str = "Working on updates";
    const LINE_3: &str = "Don't turn off your computer";

    fn wide(s: &str) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    /// A 32x32 cursor that draws nothing: AND mask all 1s (fully
    /// transparent), XOR mask all 0s. `SetSystemCursor` takes ownership of
    /// whatever handle it's given (and destroys it), so this needs to be
    /// called fresh each time rather than reusing one handle.
    fn create_blank_cursor(hinstance: winapi::shared::minwindef::HINSTANCE) -> HCURSOR {
        let and_mask = [0xFFu8; 32 * 32 / 8];
        let xor_mask = [0x00u8; 32 * 32 / 8];
        unsafe {
            CreateCursor(
                hinstance,
                0,
                0,
                32,
                32,
                and_mask.as_ptr() as *const _,
                xor_mask.as_ptr() as *const _,
            )
        }
    }

    /// Draws the six-dot "ring" spinner Windows uses on its own update/
    /// reset screens: dots evenly spaced around a circle, graduated in
    /// size from a small "head" to a large "tail" so the ring reads as a
    /// moving comet rather than six static dots, continuously rotating
    /// via `elapsed_secs`.
    fn draw_spinner(hdc: HDC, cx: i32, cy: i32, elapsed_secs: f64) {
        const NUM_DOTS: i32 = 6;
        const ORBIT_RADIUS: f64 = 22.0;
        const MIN_DOT_RADIUS: f64 = 3.0;
        const MAX_DOT_RADIUS: f64 = 7.0;
        const REVOLUTIONS_PER_SEC: f64 = 0.6;

        unsafe {
            let brush = CreateSolidBrush(RGB(255, 255, 255));
            let old_brush = SelectObject(hdc, brush as _);
            let old_pen = SelectObject(hdc, GetStockObject(NULL_PEN as i32) as _);

            let base_angle = elapsed_secs * REVOLUTIONS_PER_SEC * std::f64::consts::TAU;
            for i in 0..NUM_DOTS {
                let angle = base_angle + (i as f64 / NUM_DOTS as f64) * std::f64::consts::TAU;
                let x = cx as f64 + ORBIT_RADIUS * angle.cos();
                let y = cy as f64 + ORBIT_RADIUS * angle.sin();
                // i=0 is the comet's "head" (biggest dot); size tapers
                // down around the ring from there.
                let taper = i as f64 / (NUM_DOTS - 1) as f64;
                let r = MAX_DOT_RADIUS - (MAX_DOT_RADIUS - MIN_DOT_RADIUS) * taper;
                Ellipse(hdc, (x - r) as i32, (y - r) as i32, (x + r) as i32, (y + r) as i32);
            }

            SelectObject(hdc, old_brush);
            SelectObject(hdc, old_pen);
            DeleteObject(brush as _);
        }
    }

    /// Draws one line of centered white Segoe UI text within `rect`.
    fn draw_text_line(hdc: HDC, text: &str, rect: &mut RECT, point_size: i32) {
        unsafe {
            let font_name = wide("Segoe UI");
            let font = CreateFontW(
                -point_size,
                0,
                0,
                0,
                FW_NORMAL as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                CLEARTYPE_QUALITY as u32,
                (DEFAULT_PITCH | FF_SWISS) as u32,
                font_name.as_ptr(),
            );
            let old_font = SelectObject(hdc, font as _);
            SetBkMode(hdc, TRANSPARENT as i32);
            SetTextColor(hdc, RGB(255, 255, 255));

            let wide_text = wide(text);
            DrawTextW(hdc, wide_text.as_ptr(), -1, rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE);

            SelectObject(hdc, old_font);
            DeleteObject(font as _);
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        msg: UINT,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_PAINT => {
                let mut ps: winapi::um::winuser::PAINTSTRUCT = std::mem::zeroed();
                let hdc = BeginPaint(hwnd, &mut ps);

                let mut rect: RECT = std::mem::zeroed();
                GetClientRect(hwnd, &mut rect);

                let (r, g, b) = BACKGROUND_COLOR;
                let bg_brush = CreateSolidBrush(RGB(r, g, b));
                winapi::um::winuser::FillRect(hdc, &rect, bg_brush as HBRUSH);
                DeleteObject(bg_brush as _);

                let elapsed_secs = ANIMATION_START
                    .lock()
                    .unwrap()
                    .map(|start| start.elapsed().as_secs_f64())
                    .unwrap_or(0.0);
                let percent = fake_percent(elapsed_secs);

                let center_x = (rect.left + rect.right) / 2;
                let center_y = (rect.top + rect.bottom) / 2;

                // Spinner, then three lines of text stacked below it - laid
                // out as fixed offsets from center (like the reference
                // Windows Update screen), biased upward a bit so the group
                // as a whole reads as centered rather than the spinner
                // alone.
                draw_spinner(hdc, center_x, center_y - 70, elapsed_secs);

                let mut line1_rect =
                    RECT { left: rect.left, right: rect.right, top: center_y - 6, bottom: center_y + 34 };
                draw_text_line(hdc, LINE_1, &mut line1_rect, 24);

                let mut line2_rect =
                    RECT { left: rect.left, right: rect.right, top: center_y + 34, bottom: center_y + 64 };
                draw_text_line(hdc, &format!("{percent}% complete"), &mut line2_rect, 20);

                let mut line3_rect =
                    RECT { left: rect.left, right: rect.right, top: center_y + 64, bottom: center_y + 94 };
                draw_text_line(hdc, LINE_3, &mut line3_rect, 20);

                EndPaint(hwnd, &ps);
                0
            }
            WM_TIMER => {
                // Nothing to draw here directly - just forces a WM_PAINT so
                // the spinner keeps rotating and the percentage keeps
                // ticking over. 0 (don't erase background) avoids a flicker
                // between this and the FillRect WM_PAINT already does.
                InvalidateRect(hwnd, std::ptr::null(), 0);
                0
            }
            WM_NCHITTEST => {
                // The reliable way to make a window click-through: tell
                // Windows to hit-test as if we're not here at all, so
                // mouse messages (including the clicks the remote client
                // injects) go straight to whatever's underneath instead
                // of being swallowed by this window. WS_EX_TRANSPARENT
                // alone turned out not to be sufficient on its own.
                HTTRANSPARENT
            }
            WM_SETCURSOR => {
                // The remote client is still moving the real cursor via
                // injected input (that's the whole point - it keeps
                // controlling the real desktop underneath). Without this,
                // that moving cursor is the one visible giveaway on an
                // otherwise blanked screen. Our window covers the entire
                // screen and is topmost, so the cursor is always over it;
                // setting it to nothing here hides it completely.
                SetCursor(std::ptr::null_mut());
                1
            }
            WM_CLOSE => {
                winapi::um::winuser::DestroyWindow(hwnd);
                0
            }
            WM_DESTROY => {
                KillTimer(hwnd, ANIMATION_TIMER_ID);
                // Cleared so the *next* activation's WM_PAINT (before its
                // own create_overlay_window has had a chance to set a new
                // start time) reads elapsed=0 instead of a stale clock.
                *ANIMATION_START.lock().unwrap() = None;
                // Balances the ShowCursor(FALSE) from create_overlay_window.
                ShowCursor(1);
                // Reload the real system cursors, undoing the blank one
                // create_overlay_window swapped in - this is the mechanism
                // that actually matters (see the comment there); ShowCursor
                // is kept as a harmless secondary attempt.
                SystemParametersInfoW(SPI_SETCURSORS, 0, std::ptr::null_mut(), 0);
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    fn create_overlay_window() -> Result<HWND, String> {
        unsafe {
            // In `rd-host`'s GUI mode, eframe/winit already declares DPI
            // awareness at startup, so GetSystemMetrics below already
            // returns true physical pixels. But CLI mode (`listen`/`relay`)
            // never touches DPI awareness at all - without this call,
            // GetSystemMetrics returns DPI-*virtualized* (scaled-down)
            // dimensions there, sizing this window smaller than the real
            // screen and leaving a border of real desktop (and its normal
            // cursor) visible around the edges. Harmless if it's already
            // been set (by eframe, or a previous call here) - Windows only
            // honors the first attempt per process either way.
            SetProcessDPIAware();

            let hinstance = GetModuleHandleW(std::ptr::null());
            let class_name = wide("RdHostBlankOverlay");

            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(window_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance,
                hIcon: std::ptr::null_mut(),
                hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class_name.as_ptr(),
            };
            // Re-enabling after a previous disable re-registers the same
            // class - that's expected and harmless, ignore just that error.
            if RegisterClassW(&wc) == 0 {
                let err = GetLastError();
                if err != ERROR_CLASS_ALREADY_EXISTS {
                    return Err(format!("RegisterClassW failed (error {err})"));
                }
            }

            let w = GetSystemMetrics(SM_CXSCREEN);
            let h = GetSystemMetrics(SM_CYSCREEN);

            let title = wide("RD Authenticator blank screen");
            let hwnd = CreateWindowExW(
                // WS_EX_TRANSPARENT is the important one here: without it,
                // this topmost, full-screen window swallows every mouse
                // message before it can reach whatever's underneath -
                // including the clicks the remote client is injecting, so
                // remote control effectively stops working while blanked.
                // With it, we still render on top and stay excluded from
                // capture, but mouse hit-testing skips straight past us to
                // the real desktop below. WS_EX_NOACTIVATE matters just as
                // much: without it, showing this window steals *keyboard*
                // focus from whatever the remote client was just typing
                // into, so even a fully click-transparent overlay would
                // still swallow every injected keystroke.
                // (WS_EX_LAYERED was tried here too, on the theory that it's
                // the more "standard" click-through combo - measurement
                // showed it actually regresses this specific case, so it's
                // deliberately left out. Trust the click-through probe over
                // received wisdom about which flags "should" work.)
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                0,
                0,
                w,
                h,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null_mut(),
            );
            if hwnd.is_null() {
                return Err(format!("CreateWindowExW failed (error {})", GetLastError()));
            }

            if SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) == 0 {
                let err = GetLastError();
                winapi::um::winuser::DestroyWindow(hwnd);
                return Err(format!(
                    "SetWindowDisplayAffinity failed (error {err}) - this needs Windows 10 version 2004 or later"
                ));
            }

            // SW_SHOWNOACTIVATE, not SW_SHOW: belt-and-suspenders alongside
            // WS_EX_NOACTIVATE above - ShowWindow's own activation behavior
            // has been inconsistent with that style alone across Windows
            // versions, and stealing focus here is what breaks remote
            // keyboard input while blanked.
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            UpdateWindow(hwnd);

            // Starts this activation's fake update clock at 0% and kicks
            // off the repaint timer that animates the spinner and advances
            // the percentage - see ANIMATION_START and WM_TIMER.
            *ANIMATION_START.lock().unwrap() = Some(Instant::now());
            SetTimer(hwnd, ANIMATION_TIMER_ID, ANIMATION_TIMER_MS, None);

            // The actual mechanism that hides the cursor: swap the real
            // system arrow-cursor resource for a blank one. This is a
            // per-session resource swap, not tied to any particular
            // thread or window, so it works regardless of which thread
            // (ours, or the one injecting mouse input via Enigo) is
            // "moving" the cursor at any given moment - unlike ShowCursor,
            // whose hide/show effect turned out not to reach a cursor
            // being moved by a different thread's SendInput calls.
            // Undone by SystemParametersInfoW(SPI_SETCURSORS, ...) in
            // WM_DESTROY.
            let blank_cursor = create_blank_cursor(hinstance);
            if !blank_cursor.is_null() {
                SetSystemCursor(blank_cursor, OCR_NORMAL);
            }

            // Kept as a harmless secondary attempt - see WM_DESTROY.
            ShowCursor(0);

            let _ = CW_USEDEFAULT; // (unused constant kept only for readers scanning imports)
            Ok(hwnd)
        }
    }

    fn run_message_loop() {
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /// Owns the overlay's dedicated window-and-message-loop thread. Dropping
    /// this (or calling `disable`) tears the overlay down.
    pub struct BlankScreenOverlay {
        hwnd: Arc<AtomicIsize>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl BlankScreenOverlay {
        pub fn new() -> Self {
            Self { hwnd: Arc::new(AtomicIsize::new(0)), thread: None }
        }

        pub fn is_enabled(&self) -> bool {
            self.thread.is_some()
        }

        /// Turn the overlay on. A no-op if it's already on.
        pub fn enable(&mut self) -> Result<(), String> {
            if self.thread.is_some() {
                return Ok(());
            }

            let hwnd_slot = self.hwnd.clone();
            let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

            let handle = std::thread::spawn(move || match create_overlay_window() {
                Ok(hwnd) => {
                    hwnd_slot.store(hwnd as isize, Ordering::SeqCst);
                    let _ = ready_tx.send(Ok(()));
                    run_message_loop();
                    hwnd_slot.store(0, Ordering::SeqCst);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            });

            match ready_rx.recv_timeout(Duration::from_secs(3)) {
                Ok(Ok(())) => {
                    self.thread = Some(handle);
                    Ok(())
                }
                Ok(Err(e)) => {
                    let _ = handle.join();
                    Err(e)
                }
                Err(_) => {
                    let _ = handle.join();
                    Err("timed out creating the blank-screen overlay window".to_string())
                }
            }
        }

        /// Turn the overlay off. A no-op if it's already off.
        pub fn disable(&mut self) {
            let hwnd = self.hwnd.load(Ordering::SeqCst);
            if hwnd != 0 {
                unsafe {
                    PostMessageW(hwnd as HWND, WM_CLOSE, 0, 0);
                }
            }
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    impl Drop for BlankScreenOverlay {
        fn drop(&mut self) {
            // A dropped session (disconnect, error, force-close on power-off)
            // must never leave the host's screen permanently blanked.
            self.disable();
        }
    }
}

#[cfg(not(windows))]
mod imp {
    /// Non-Windows stub: there's no capture-exclusion mechanism wired up
    /// here for macOS/Linux, so this always reports itself as off and
    /// fails clearly rather than silently doing nothing.
    pub struct BlankScreenOverlay;

    impl BlankScreenOverlay {
        pub fn new() -> Self {
            Self
        }
        pub fn is_enabled(&self) -> bool {
            false
        }
        pub fn enable(&mut self) -> Result<(), String> {
            Err("blank host screen isn't supported on this platform yet (Windows-only)".to_string())
        }
        pub fn disable(&mut self) {}
    }
}

pub use imp::BlankScreenOverlay;

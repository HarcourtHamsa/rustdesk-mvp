//! The default mode when you run `rd-host` with no arguments: it starts
//! hosting immediately (no button to click) and stays out of the way as a
//! small icon in the system tray - no window at all. Right-click the icon
//! for Copy Address / Copy Password / Open Log File / Quit.
//!
//! Once the ngrok tunnel finishes starting (or fails to), the address and
//! password get appended to a log file next to the executable, so they're
//! not lost just because there's no window showing them.
//!
//! Windows-only, same as `blank_screen.rs` - there's no tray-icon
//! implementation wired up here for macOS/Linux, so `run()` just returns
//! an error there telling you to use `rd-host listen`/`--window` instead.

#[cfg(windows)]
mod imp {
    use crate::gui_core::{start_hosting, stop_hosting, HostControl, NgrokState};
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Duration;

    use winapi::shared::minwindef::{LPARAM, LRESULT, UINT, WPARAM};
    use winapi::shared::windef::{HBITMAP, HICON, HWND, POINT};
    use winapi::um::libloaderapi::GetModuleHandleW;
    use winapi::um::minwinbase::SYSTEMTIME;
    use winapi::um::shellapi::{
        Shell_NotifyIconW, ShellExecuteW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
        NOTIFYICONDATAW,
    };
    use winapi::um::sysinfoapi::GetLocalTime;
    use winapi::um::wingdi::{
        CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        DIB_RGB_COLORS,
    };
    use winapi::um::winuser::{
        AppendMenuW, CreatePopupMenu, DefWindowProcW, DestroyMenu, DispatchMessageW, GetCursorPos,
        GetDC, GetMessageW, LoadIconW, MessageBoxW, PostMessageW, PostQuitMessage,
        RegisterClassW, ReleaseDC, SetForegroundWindow, TrackPopupMenu, TranslateMessage,
        CreateWindowExW, CW_USEDEFAULT, IDI_APPLICATION, MB_ICONINFORMATION, MB_OK, MF_SEPARATOR,
        MF_STRING, MSG, TPM_RIGHTBUTTON, WM_APP, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY,
        WM_LBUTTONDBLCLK, WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPEDWINDOW,
    };
    use winapi::um::winuser::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData, CF_UNICODETEXT,
    };
    use winapi::um::winbase::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

    const WM_TRAYICON: UINT = WM_APP + 1;
    const ID_COPY_ADDRESS: UINT = 1001;
    const ID_COPY_PASSWORD: UINT = 1002;
    const ID_OPEN_LOG: UINT = 1003;
    const ID_QUIT: UINT = 1004;
    const TRAY_UID: UINT = 1;

    /// Everything the window proc's menu handlers need. There's only ever
    /// one tray icon per process, so a global behind a mutex is simpler and
    /// safer here than threading a pointer through GWLP_USERDATA.
    struct TrayContext {
        control: HostControl,
        lan_address: String,
        password: String,
        log_path: PathBuf,
    }

    static TRAY_CTX: Mutex<Option<TrayContext>> = Mutex::new(None);

    fn wide(s: &str) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    pub fn run() -> std::io::Result<()> {
        let (lan_address, password, control) = start_hosting()?;

        let log_path = log_file_path();
        spawn_log_writer(control.ngrok.clone(), lan_address.clone(), password.clone(), log_path.clone());

        *TRAY_CTX.lock().unwrap() =
            Some(TrayContext { control, lan_address, password, log_path });

        unsafe { run_message_loop() }
        Ok(())
    }

    /// Next to the running executable, e.g.
    /// `...\target\release\rd-host-connections.log`. Falls back to the
    /// current directory if the exe's own path can't be determined.
    fn log_file_path() -> PathBuf {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("rd-host-connections.log")))
            .unwrap_or_else(|| PathBuf::from("rd-host-connections.log"))
    }

    /// Waits for the ngrok tunnel to leave "Starting" (either ready or
    /// given up), then appends one entry to the log with whatever we
    /// learned - the tunnel address if it came up, or a note that it
    /// didn't along with the LAN address as a fallback.
    fn spawn_log_writer(
        ngrok: std::sync::Arc<Mutex<NgrokState>>,
        lan_address: String,
        password: String,
        log_path: PathBuf,
    ) {
        std::thread::spawn(move || loop {
            let outcome = match &*ngrok.lock().unwrap() {
                NgrokState::Starting => None,
                NgrokState::Ready(addr) => Some(Ok(addr.clone())),
                NgrokState::Unavailable(reason) => Some(Err(reason.clone())),
            };
            match outcome {
                None => std::thread::sleep(Duration::from_millis(300)),
                Some(result) => {
                    append_log_entry(&log_path, &lan_address, &password, result.as_ref());
                    return;
                }
            }
        });
    }

    fn timestamp() -> String {
        unsafe {
            let mut st: SYSTEMTIME = std::mem::zeroed();
            GetLocalTime(&mut st);
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
            )
        }
    }

    fn append_log_entry(
        path: &std::path::Path,
        lan_address: &str,
        password: &str,
        tunnel: Result<&String, &String>,
    ) {
        let mut entry = format!("[{}] rd-host is running\n", timestamp());
        entry += &format!("  LAN address: {lan_address}\n");
        match tunnel {
            Ok(addr) => entry += &format!("  Tunnel:      {addr}\n"),
            Err(reason) => entry += &format!("  Tunnel:      unavailable ({reason})\n"),
        }
        entry += &format!("  Password:    {password}\n\n");

        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = file.write_all(entry.as_bytes());
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        msg: UINT,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_TRAYICON => {
                match lparam as u32 {
                    WM_RBUTTONUP | WM_CONTEXTMENU => show_context_menu(hwnd),
                    WM_LBUTTONDBLCLK => show_status(),
                    _ => {}
                }
                0
            }
            WM_COMMAND => {
                match (wparam & 0xffff) as UINT {
                    ID_COPY_ADDRESS => copy_current(hwnd, true),
                    ID_COPY_PASSWORD => copy_current(hwnd, false),
                    ID_OPEN_LOG => open_log(),
                    ID_QUIT => {
                        if let Some(ctx) = TRAY_CTX.lock().unwrap().take() {
                            stop_hosting(&ctx.control);
                        }
                        let mut nid = tray_icon_data(hwnd, None);
                        Shell_NotifyIconW(NIM_DELETE, &mut nid);
                        winapi::um::winuser::DestroyWindow(hwnd);
                    }
                    _ => {}
                }
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    fn show_context_menu(hwnd: HWND) {
        unsafe {
            let mut pt: POINT = std::mem::zeroed();
            GetCursorPos(&mut pt);

            let menu = CreatePopupMenu();
            let items = [
                (ID_COPY_ADDRESS, "Copy Address"),
                (ID_COPY_PASSWORD, "Copy Password"),
                (ID_OPEN_LOG, "Open Log File"),
            ];
            for (id, label) in items {
                let text = wide(label);
                AppendMenuW(menu, MF_STRING, id as usize, text.as_ptr());
            }
            AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            let quit_text = wide("Quit");
            AppendMenuW(menu, MF_STRING, ID_QUIT as usize, quit_text.as_ptr());

            // Required so the menu reliably closes if the user clicks
            // elsewhere instead of choosing an item - documented Win32 idiom.
            SetForegroundWindow(hwnd);
            TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, std::ptr::null());
            PostMessageW(hwnd, WM_NULL, 0, 0);
            DestroyMenu(menu);
        }
    }

    fn show_status() {
        let guard = TRAY_CTX.lock().unwrap();
        let Some(ctx) = guard.as_ref() else { return };
        let tunnel_line = match &*ctx.control.ngrok.lock().unwrap() {
            NgrokState::Starting => "Tunnel: starting...".to_string(),
            NgrokState::Ready(addr) => format!("Tunnel: {addr}"),
            NgrokState::Unavailable(reason) => format!("Tunnel unavailable: {reason}"),
        };
        let text = format!(
            "rd-host is running.\n\nLAN address: {}\n{}\nPassword: {}",
            ctx.lan_address, tunnel_line, ctx.password
        );
        drop(guard);
        unsafe {
            let title = wide("rd-host");
            let body = wide(&text);
            MessageBoxW(std::ptr::null_mut(), body.as_ptr(), title.as_ptr(), MB_OK | MB_ICONINFORMATION);
        }
    }

    fn copy_current(hwnd: HWND, address: bool) {
        let guard = TRAY_CTX.lock().unwrap();
        let Some(ctx) = guard.as_ref() else { return };
        let text = if address {
            match &*ctx.control.ngrok.lock().unwrap() {
                NgrokState::Ready(addr) => addr.clone(),
                _ => ctx.lan_address.clone(),
            }
        } else {
            ctx.password.clone()
        };
        drop(guard);
        copy_to_clipboard(hwnd, &text);
    }

    fn copy_to_clipboard(hwnd: HWND, text: &str) {
        unsafe {
            if OpenClipboard(hwnd) == 0 {
                return;
            }
            EmptyClipboard();
            let wide_text = wide(text);
            let bytes = wide_text.len() * std::mem::size_of::<u16>();
            let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if !hmem.is_null() {
                let ptr = GlobalLock(hmem) as *mut u16;
                if !ptr.is_null() {
                    std::ptr::copy_nonoverlapping(wide_text.as_ptr(), ptr, wide_text.len());
                    GlobalUnlock(hmem);
                    // The system owns hmem once this succeeds - don't free it ourselves.
                    SetClipboardData(CF_UNICODETEXT, hmem as _);
                }
            }
            CloseClipboard();
        }
    }

    fn open_log() {
        let path = {
            let guard = TRAY_CTX.lock().unwrap();
            guard.as_ref().map(|c| c.log_path.clone())
        };
        let Some(path) = path else { return };
        // Create it if it doesn't exist yet (e.g. tunnel still starting),
        // so there's always something for the default handler to open.
        if !path.exists() {
            let _ = std::fs::write(&path, "");
        }
        unsafe {
            let op = wide("open");
            let file = wide(&path.to_string_lossy());
            ShellExecuteW(
                std::ptr::null_mut(),
                op.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1, // SW_SHOWNORMAL
            );
        }
    }

    /// A small solid-green dot, drawn with real per-pixel alpha so it reads
    /// as a circle against the tray background rather than a solid square.
    fn create_tray_hicon() -> HICON {
        unsafe {
            let size: i32 = 16;
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = size;
            bmi.bmiHeader.biHeight = size;
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB;

            let hdc = GetDC(std::ptr::null_mut());
            let mut bits_ptr: *mut winapi::ctypes::c_void = std::ptr::null_mut();
            let color_bmp: HBITMAP = CreateDIBSection(
                hdc,
                &bmi,
                DIB_RGB_COLORS,
                &mut bits_ptr,
                std::ptr::null_mut(),
                0,
            );
            ReleaseDC(std::ptr::null_mut(), hdc);

            if color_bmp.is_null() || bits_ptr.is_null() {
                return LoadIconW(std::ptr::null_mut(), IDI_APPLICATION);
            }

            let pixels =
                std::slice::from_raw_parts_mut(bits_ptr as *mut u32, (size * size) as usize);
            let center = size as f32 / 2.0;
            let radius = size as f32 / 2.0 - 1.0;
            for y in 0..size {
                for x in 0..size {
                    let dx = x as f32 + 0.5 - center;
                    let dy = y as f32 + 0.5 - center;
                    let idx = (y * size + x) as usize;
                    // 0xAARRGGBB as a little-endian u32 lands in memory as
                    // [B, G, R, A], matching the BGRA layout a 32bpp DIB
                    // expects.
                    pixels[idx] = if dx * dx + dy * dy <= radius * radius {
                        0xFF5ADC78
                    } else {
                        0x00000000
                    };
                }
            }

            // Windows requires a mask bitmap even for a 32bpp DIB with its
            // own alpha channel; content doesn't matter here since the
            // per-pixel alpha above is what actually gets honored.
            let mask_bmp = CreateBitmap(size, size, 1, 1, std::ptr::null());
            let mut icon_info = winapi::um::winuser::ICONINFO {
                fIcon: 1,
                xHotspot: 0,
                yHotspot: 0,
                hbmMask: mask_bmp,
                hbmColor: color_bmp,
            };
            let hicon = winapi::um::winuser::CreateIconIndirect(&mut icon_info);
            DeleteObject(mask_bmp as _);
            DeleteObject(color_bmp as _);

            if hicon.is_null() {
                LoadIconW(std::ptr::null_mut(), IDI_APPLICATION)
            } else {
                hicon
            }
        }
    }

    fn tray_icon_data(hwnd: HWND, icon: Option<HICON>) -> NOTIFYICONDATAW {
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_UID;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAYICON;
        if let Some(icon) = icon {
            nid.hIcon = icon;
        }
        let tip = wide("rd-host - running (right-click for options)");
        let len = tip.len().min(nid.szTip.len() - 1);
        nid.szTip[..len].copy_from_slice(&tip[..len]);
        nid
    }

    unsafe fn run_message_loop() {
        let hinstance = GetModuleHandleW(std::ptr::null());
        let class_name = wide("RdHostTrayWindow");

        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        RegisterClassW(&wc);

        // Never shown - it only exists to own the tray icon and receive its
        // messages, so it doesn't need any particular size or position.
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null_mut(),
        );
        if hwnd.is_null() {
            return;
        }

        let icon = create_tray_hicon();
        let mut nid = tray_icon_data(hwnd, Some(icon));
        Shell_NotifyIconW(NIM_ADD, &mut nid);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn run() -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the tray/background mode is Windows-only for now - use `rd-host listen` or `rd-host --window` instead",
        ))
    }
}

pub use imp::run;

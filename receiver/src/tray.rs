//! Notification-area (system tray) icon, so the receiver can keep running
//! with its window closed. Left click opens the window; right click gives
//! a small menu.

use std::sync::atomic::{AtomicU8, Ordering};

pub const NONE: u8 = 0;
pub const OPEN: u8 = 1;
pub const QUIT: u8 = 2;
static ACTION: AtomicU8 = AtomicU8::new(NONE);

/// What the user last asked for from the tray, if anything.
pub fn take_action() -> u8 {
    ACTION.swap(NONE, Ordering::Relaxed)
}

#[cfg(not(windows))]
pub fn start() -> bool {
    false
}
#[cfg(not(windows))]
pub fn stop() {}

#[cfg(windows)]
pub use win::{start, stop};

#[cfg(windows)]
mod win {
    use super::*;
    use std::sync::atomic::AtomicIsize;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
    };
    use windows::Win32::UI::WindowsAndMessaging::*;

    const WM_TRAY: u32 = WM_APP + 1;
    const ID_OPEN: usize = 1;
    const ID_QUIT: usize = 2;
    static WINDOW: AtomicIsize = AtomicIsize::new(0);
    static TASKBAR_CREATED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    unsafe fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            ..Default::default()
        };
        // Icon resource 1 is the program's own icon; fall back to the stock one.
        let module = GetModuleHandleW(None).unwrap_or_default();
        nid.hIcon = LoadIconW(module, PCWSTR(1 as *const u16))
            .or_else(|_| LoadIconW(None, IDI_APPLICATION))
            .unwrap_or_default();
        for (i, c) in "AllyPad Receiver".encode_utf16().enumerate() {
            nid.szTip[i] = c;
        }
        nid
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            WM_TRAY => {
                match lparam.0 as u32 {
                    WM_LBUTTONUP | WM_LBUTTONDBLCLK => ACTION.store(OPEN, Ordering::Relaxed),
                    WM_RBUTTONUP | WM_CONTEXTMENU => {
                        if let Ok(menu) = CreatePopupMenu() {
                            let _ = AppendMenuW(menu, MF_STRING, ID_OPEN, w!("Open AllyPad Receiver"));
                            let _ = AppendMenuW(menu, MF_STRING, ID_QUIT, w!("Quit"));
                            let mut pt = POINT::default();
                            let _ = GetCursorPos(&mut pt);
                            // Needed so the menu closes when you click elsewhere.
                            let _ = SetForegroundWindow(hwnd);
                            let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
                            let _ = DestroyMenu(menu);
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                match wparam.0 & 0xFFFF {
                    ID_OPEN => ACTION.store(OPEN, Ordering::Relaxed),
                    ID_QUIT => ACTION.store(QUIT, Ordering::Relaxed),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd));
                PostQuitMessage(0);
                LRESULT(0)
            }
            // Explorer restarted: the tray is empty again, so put the icon back.
            m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
                let _ = Shell_NotifyIconW(NIM_ADD, &icon_data(hwnd));
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    /// Adds the tray icon. Returns false if Windows would not take it.
    pub fn start() -> bool {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || unsafe {
            let module = GetModuleHandleW(None).unwrap_or_default();
            let class = w!("AllyPadReceiverTray");
            let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: module.into(), lpszClassName: class, ..Default::default() };
            RegisterClassW(&wc);
            TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);
            // A hidden top-level window: it only exists to receive tray messages.
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!("AllyPad Receiver"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                module,
                None,
            );
            let Ok(hwnd) = hwnd else {
                let _ = tx.send(false);
                return;
            };
            let ok = Shell_NotifyIconW(NIM_ADD, &icon_data(hwnd)).as_bool();
            let _ = tx.send(ok);
            if !ok {
                return;
            }
            WINDOW.store(hwnd.0 as isize, Ordering::Relaxed);
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        });
        rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap_or(false)
    }

    /// Removes the tray icon.
    pub fn stop() {
        let h = WINDOW.swap(0, Ordering::Relaxed);
        if h != 0 {
            unsafe {
                let _ = PostMessageW(HWND(h as *mut _), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
    }
}

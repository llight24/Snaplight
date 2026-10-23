use snaplight_domain::ImageDescriptor;
use std::path::PathBuf;

/// Owner-drawn popup menu used by the tray context menu.
#[cfg(windows)]
pub mod popup_menu;

/// Owner-drawn confirmation prompt with an opt-out checkbox.
#[cfg(windows)]
pub mod confirm;

mod storage;
pub use storage::{
    atomic_write, capture_dir, capture_files, data_dir, list_captures, load_config,
    load_window_state, read_pixels, save_config, save_window_state, write_pixels, CaptureEntry,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct Capabilities {
    pub capture: bool,
    pub global_hotkeys: bool,
    pub positioning: bool,
    pub topmost: bool,
    pub click_through: bool,
}
pub trait CaptureService {
    type Frame;
    type Error;
    fn capture(&mut self) -> Result<Vec<Self::Frame>, Self::Error>;
}
pub trait WindowService {
    type Window;
    type Error;
    fn capabilities(&self) -> Capabilities;
    fn set_click_through(&self, window: &Self::Window, enabled: bool) -> Result<(), Self::Error>;
}
pub trait HotkeyService {
    type Error;
    fn register(&mut self, id: i32, modifiers: u32, key: u32) -> Result<(), Self::Error>;
}
pub trait ClipboardService {
    type Error;
    fn write_image(&self, image: &ImageDescriptor, bytes: &[u8]) -> Result<(), Self::Error>;
}

pub fn sibling(name: &str) -> std::io::Result<PathBuf> {
    Ok(std::env::current_exe()?
        .parent()
        .ok_or_else(|| std::io::Error::other("missing executable directory"))?
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use snaplight_domain::*;
    #[cfg(windows)]
    #[test]
    fn hotkeys_map_to_win32_flags_and_virtual_keys() {
        use windows::Win32::UI::Input::KeyboardAndMouse::*;
        let hotkey = snaplight_domain::parse_hotkey(snaplight_domain::DEFAULT_CAPTURE_HOTKEY)
            .expect("default capture hotkey");
        let (modifiers, key) = native::hotkey_registration(&hotkey);
        assert_eq!(modifiers, MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT);
        assert_eq!(key, 'A' as u32);
        let hotkey = snaplight_domain::parse_hotkey("Alt+Win+F12").expect("function hotkey");
        let (modifiers, key) = native::hotkey_registration(&hotkey);
        assert_eq!(modifiers, MOD_ALT | MOD_WIN | MOD_NOREPEAT);
        assert_eq!(key, 0x7b);
        let hotkey = snaplight_domain::parse_hotkey("Ctrl+7").expect("digit hotkey");
        assert_eq!(native::hotkey_registration(&hotkey).1, '7' as u32);
    }
    #[cfg(windows)]
    #[test]
    fn held_shortcuts_map_to_both_modifier_variants() {
        use windows::Win32::UI::Input::KeyboardAndMouse::*;
        let held = |text: &str| {
            snaplight_domain::parse_held_shortcut(text)
                .expect("held shortcut")
                .expect("enabled")
        };
        // A lone modifier is watched through its left and right variants.
        assert_eq!(
            native::held_shortcut_keys(&held(DEFAULT_THROUGH_HOTKEY)),
            vec![VK_LCONTROL, VK_RCONTROL]
        );
        assert_eq!(
            native::held_shortcut_keys(&held("Ctrl+Shift")),
            vec![VK_LCONTROL, VK_RCONTROL, VK_LSHIFT, VK_RSHIFT]
        );
        // A lone key adds no modifier at all.
        assert_eq!(
            native::held_shortcut_keys(&held("a")),
            vec![VIRTUAL_KEY('A' as u16)]
        );
        assert_eq!(
            native::held_shortcut_keys(&held("Win+Alt+F5")),
            vec![VK_LMENU, VK_RMENU, VK_LWIN, VK_RWIN, VIRTUAL_KEY(0x74)]
        );
        // Nothing is down in a test process, so the check reports "not held".
        assert!(!native::held_shortcut_down(&held("Ctrl")));
    }
    #[cfg(windows)]
    #[test]
    fn probe_policy_keeps_controls_inside_their_window() {
        use native::{pick_bounds, ElementBounds};
        let entry =
            |control_type: i32, class_name: &str, rect: (i32, i32, i32, i32)| ElementBounds {
                control_type,
                class_name: class_name.into(),
                rect: snaplight_domain::ScreenRect {
                    x: rect.0,
                    y: rect.1,
                    width: rect.2,
                    height: rect.3,
                },
            };
        let window = 50032;
        let group = 50026;
        let desktop = 50033;
        // A control inside a window is captured as-is.
        let chain = vec![
            entry(group, "", (300, 220, 400, 300)),
            entry(window, "Notepad", (100, 100, 800, 600)),
            entry(desktop, "#32769", (0, 0, 2560, 1440)),
        ];
        assert_eq!(
            pick_bounds(&chain),
            Some(snaplight_domain::ScreenRect {
                x: 300,
                y: 220,
                width: 400,
                height: 300
            })
        );
        // A control that overflows its window is clamped to the window.
        let chain = vec![
            entry(group, "", (90, 90, 900, 700)),
            entry(window, "Notepad", (100, 100, 800, 600)),
            entry(desktop, "#32769", (0, 0, 2560, 1440)),
        ];
        assert_eq!(
            pick_bounds(&chain).map(|r| (r.x, r.y, r.width, r.height)),
            Some((100, 100, 800, 600))
        );
        // Zero-sized elements climb to the nearest usable ancestor.
        let chain = vec![
            entry(50034, "", (100, 100, 0, 40)),
            entry(50034, "", (100, 100, 800, 40)),
            entry(window, "Notepad", (100, 100, 800, 600)),
            entry(desktop, "#32769", (0, 0, 2560, 1440)),
        ];
        assert_eq!(
            pick_bounds(&chain).map(|r| (r.x, r.y, r.width, r.height)),
            Some((100, 100, 800, 40))
        );
        // A window reported without the window control type is still usable as
        // long as it is not shell chrome.
        let chain = vec![
            entry(group, "", (120, 140, 300, 200)),
            entry(desktop, "#32769", (0, 0, 2560, 1440)),
        ];
        assert_eq!(
            pick_bounds(&chain).map(|r| (r.width, r.height)),
            Some((300, 200))
        );
    }
    #[cfg(windows)]
    #[test]
    fn probe_policy_rejects_desktop_and_taskbar() {
        use native::{pick_bounds, ElementBounds};
        let entry =
            |control_type: i32, class_name: &str, rect: (i32, i32, i32, i32)| ElementBounds {
                control_type,
                class_name: class_name.into(),
                rect: snaplight_domain::ScreenRect {
                    x: rect.0,
                    y: rect.1,
                    width: rect.2,
                    height: rect.3,
                },
            };
        let desktop = 50033;
        // Wallpaper icon: the chain ends in the shell's desktop container.
        let chain = vec![
            entry(50007, "", (0, 5, 74, 72)),
            entry(50008, "SysListView32", (0, 0, 2560, 1440)),
            entry(desktop, "Progman", (0, 0, 2560, 1440)),
            entry(desktop, "#32769", (0, 0, 2560, 1440)),
        ];
        assert_eq!(pick_bounds(&chain), None);
        // Taskbar button: no window ancestor, and the outermost element is the
        // taskbar itself.
        let chain = vec![
            entry(50000, "Button", (1225, 1400, 56, 32)),
            entry(50000, "ToggleButton", (1061, 1400, 220, 32)),
            entry(desktop, "Shell_TrayWnd", (0, 1392, 2560, 48)),
            entry(desktop, "#32769", (0, 0, 2560, 1440)),
        ];
        assert_eq!(pick_bounds(&chain), None);
        // The desktop root itself, and an empty chain, report nothing.
        assert_eq!(
            pick_bounds(&[entry(desktop, "#32769", (0, 0, 2560, 1440))]),
            None
        );
        assert_eq!(pick_bounds(&[]), None);
        // A degenerate window cannot produce a capture region.
        let chain = vec![
            entry(desktop, "SomePane", (10, 10, 0, 0)),
            entry(desktop, "#32769", (0, 0, 2560, 1440)),
        ];
        assert_eq!(pick_bounds(&chain), None);
    }
    #[cfg(windows)]
    #[test]
    fn shared_pixels_are_read_only_in_receiver_and_size_checked() {
        use native::SharedPixels;
        let name = format!("Local\\Snaplight.Test.{}", std::process::id());
        let writer = SharedPixels::create(name.clone(), &[1, 2, 3, 4]).unwrap();
        let reader = SharedPixels::open(name.clone(), 4).unwrap();
        assert_eq!(reader.bytes(), writer.bytes());
        assert!(SharedPixels::open(name, 0).is_err());
    }
}

#[cfg(windows)]
pub mod native {
    mod shared_pixels;
    pub use shared_pixels::SharedPixels;
    mod probe;
    pub use probe::{pick_bounds, ElementBounds, ElementProbe, PointHole};

    use snaplight_domain::{HeldShortcut, Hotkey, HotkeyKey, ScreenPoint};
    use std::path::PathBuf;
    use windows::{
        core::*,
        Win32::{
            Foundation::*,
            System::{LibraryLoader::GetModuleHandleW, Threading::*},
            UI::{HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
        },
    };
    /// Resource id of the icon group every Snaplight executable embeds through
    /// `scripts/build/app_icon.rs`; keep both in sync.
    const APP_ICON_RESOURCE: u16 = 1;
    /// Loads the embedded application icon at the size Windows asks for: small
    /// icons for title bars and the notification area, large ones for the
    /// taskbar and Alt-Tab. The caller owns the handle; passing it to a window
    /// through `WM_SETICON` or to a window class transfers that ownership.
    pub fn app_icon(small: bool) -> Result<HICON> {
        unsafe {
            let (cx, cy) = if small {
                (SM_CXSMICON, SM_CYSMICON)
            } else {
                (SM_CXICON, SM_CYICON)
            };
            let instance = HINSTANCE(GetModuleHandleW(None)?.0);
            let image = LoadImageW(
                instance,
                PCWSTR(APP_ICON_RESOURCE as *const u16),
                IMAGE_ICON,
                GetSystemMetrics(cx),
                GetSystemMetrics(cy),
                LR_DEFAULTCOLOR,
            )?;
            Ok(HICON(image.0))
        }
    }
    /// Posted by the settings window after `config.json` changes. The core
    /// re-registers the capture shortcut and forwards a reload to the pin
    /// process, so shortcut changes apply without restarting either process.
    pub const RELOAD_CONFIG: u32 = WM_APP + 0x51;
    /// Asks the running core to reload the configuration. A setting window
    /// launched without a core (or after it exited) finds no window and does
    /// nothing; the file is still saved and the next start reads it.
    pub fn notify_config_reload() {
        unsafe {
            if let Ok(hwnd) = FindWindowW(w!("Snaplight.Core"), PCWSTR::null()) {
                let _ = PostMessageW(hwnd, RELOAD_CONFIG, WPARAM(0), LPARAM(0));
            }
        }
    }
    /// Translates a parsed hotkey into Win32 registration flags (including
    /// `MOD_NOREPEAT`) and a virtual-key code.
    pub fn hotkey_registration(hotkey: &Hotkey) -> (HOT_KEY_MODIFIERS, u32) {
        let mut modifiers = MOD_NOREPEAT;
        if hotkey.ctrl {
            modifiers |= MOD_CONTROL;
        }
        if hotkey.shift {
            modifiers |= MOD_SHIFT;
        }
        if hotkey.alt {
            modifiers |= MOD_ALT;
        }
        if hotkey.win {
            modifiers |= MOD_WIN;
        }
        let key = match hotkey.key {
            HotkeyKey::Letter(c) | HotkeyKey::Digit(c) => c as u32,
            HotkeyKey::Function(n) => 0x70 + u32::from(n) - 1,
        };
        (modifiers, key)
    }
    /// Virtual keys to watch for a held shortcut. Modifiers are listed as their
    /// left and right variants because `GetAsyncKeyState(VK_CONTROL)` does not
    /// reliably report the generic key, while the individual ones do.
    pub fn held_shortcut_keys(shortcut: &HeldShortcut) -> Vec<VIRTUAL_KEY> {
        let mut keys = Vec::new();
        for (enabled, left, right) in [
            (shortcut.ctrl, VK_LCONTROL, VK_RCONTROL),
            (shortcut.shift, VK_LSHIFT, VK_RSHIFT),
            (shortcut.alt, VK_LMENU, VK_RMENU),
        ] {
            if enabled {
                keys.push(left);
                keys.push(right);
            }
        }
        if shortcut.win {
            keys.push(VK_LWIN);
            keys.push(VK_RWIN);
        }
        if let Some(key) = shortcut.key {
            let code = match key {
                HotkeyKey::Letter(c) | HotkeyKey::Digit(c) => c as u16,
                HotkeyKey::Function(n) => 0x70 + u16::from(n) - 1,
            };
            keys.push(VIRTUAL_KEY(code));
        }
        keys
    }
    /// True while every part of a held shortcut is down. A modifier pair counts
    /// as one part: either side satisfies it.
    pub fn held_shortcut_down(shortcut: &HeldShortcut) -> bool {
        let either = |left: VIRTUAL_KEY, right: VIRTUAL_KEY| key_down(left) || key_down(right);
        let modifiers = [
            (shortcut.ctrl, VK_LCONTROL, VK_RCONTROL),
            (shortcut.shift, VK_LSHIFT, VK_RSHIFT),
            (shortcut.alt, VK_LMENU, VK_RMENU),
            (shortcut.win, VK_LWIN, VK_RWIN),
        ];
        let mut any = false;
        for (enabled, left, right) in modifiers {
            if enabled {
                any = true;
                if !either(left, right) {
                    return false;
                }
            }
        }
        if let Some(key) = shortcut.key {
            any = true;
            let code = match key {
                HotkeyKey::Letter(c) | HotkeyKey::Digit(c) => c as u16,
                HotkeyKey::Function(n) => 0x70 + u16::from(n) - 1,
            };
            if !key_down(VIRTUAL_KEY(code)) {
                return false;
            }
        }
        // A shortcut without any part is never held.
        any
    }
    /// Moves files to the recycle bin, so a mistaken deletion in the capture
    /// list can be undone. Returns an error instead of silently deleting when
    /// the shell refuses the operation.
    pub fn delete_to_recycle_bin(paths: &[PathBuf]) -> Result<()> {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::UI::Shell::*;
        if paths.is_empty() {
            return Ok(());
        }
        unsafe {
            let mut from: Vec<u16> = Vec::new();
            for path in paths {
                from.extend(path.as_os_str().encode_wide());
                from.push(0);
            }
            from.push(0);
            let mut operation = SHFILEOPSTRUCTW {
                wFunc: FO_DELETE,
                pFrom: PCWSTR(from.as_ptr()),
                fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI).0 as u16,
                ..Default::default()
            };
            let result = SHFileOperationW(&mut operation);
            if result != 0 {
                return Err(Error::from_hresult(HRESULT::from_win32(result as u32)));
            }
            if operation.fAnyOperationsAborted.as_bool() {
                return Err(Error::new(E_ABORT, "recycle bin operation aborted"));
            }
        }
        Ok(())
    }
    /// Formats a file timestamp as local `YYYY-MM-DD HH:MM` for the capture list.
    pub fn format_local_time(time: std::time::SystemTime) -> String {
        use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
        use windows::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
        use windows::Win32::System::Time::FileTimeToSystemTime;
        let since_epoch = time
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        // 100-nanosecond ticks since 1601-01-01 plus the Unix epoch offset.
        const EPOCH_OFFSET_TICKS: u64 = 116_444_736_000_000_000;
        let ticks = EPOCH_OFFSET_TICKS + since_epoch.as_nanos() as u64 / 100;
        let file_time = FILETIME {
            dwLowDateTime: ticks as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
        unsafe {
            let mut local = FILETIME::default();
            let mut system = SYSTEMTIME::default();
            if FileTimeToLocalFileTime(&file_time, &mut local).is_err()
                || FileTimeToSystemTime(&local, &mut system).is_err()
            {
                return "unknown time".into();
            }
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}",
                system.wYear, system.wMonth, system.wDay, system.wHour, system.wMinute
            )
        }
    }
    pub fn dpi_aware() {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
    }
    pub fn show_error(text: &str) {
        if std::env::var_os("SNAPLIGHT_NONINTERACTIVE").is_some() {
            eprintln!("{text}");
            return;
        }
        unsafe {
            MessageBoxW(
                None,
                &HSTRING::from(text),
                w!("Snaplight"),
                MB_OK | MB_ICONERROR,
            );
        }
    }
    /// Shows an informational message. Same suppression switch as `show_error`,
    /// so non-interactive runs never block on a dialog.
    pub fn show_notice(text: &str) {
        if std::env::var_os("SNAPLIGHT_NONINTERACTIVE").is_some() {
            eprintln!("{text}");
            return;
        }
        unsafe {
            MessageBoxW(
                None,
                &HSTRING::from(text),
                w!("Snaplight"),
                MB_OK | MB_ICONINFORMATION,
            );
        }
    }
    /// Cursor position in physical screen pixels.
    pub fn cursor_position() -> Option<ScreenPoint> {
        unsafe {
            let mut point = POINT::default();
            GetCursorPos(&mut point).ok()?;
            Some(ScreenPoint {
                x: point.x,
                y: point.y,
            })
        }
    }
    /// Moves the cursor, in physical screen pixels.
    pub fn set_cursor_position(point: ScreenPoint) -> bool {
        unsafe { SetCursorPos(point.x, point.y).is_ok() }
    }
    /// Sends mouse-wheel notches at the current cursor position. Positive scrolls
    /// up, negative down; one notch is `WHEEL_DELTA` (120).
    ///
    /// The wheel goes to the window under the cursor, which is why long capture
    /// parks the cursor inside the region it scrolls and restores it afterwards.
    pub fn scroll_wheel(notches: i32) -> bool {
        const WHEEL_DELTA: i32 = 120;
        unsafe {
            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        mouseData: (notches * WHEEL_DELTA) as u32,
                        dwFlags: MOUSEEVENTF_WHEEL,
                        ..Default::default()
                    },
                },
            };
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32) == 1
        }
    }
    /// True while the key is down, whichever window owns the focus. Long capture
    /// watches `VK_ESCAPE` this way because it runs with its own windows hidden.
    pub fn key_down(key: VIRTUAL_KEY) -> bool {
        unsafe { (GetAsyncKeyState(key.0 as i32) as u16 & 0x8000) != 0 }
    }
    /// Top-level window under a screen point.
    pub fn root_window_at(point: ScreenPoint) -> Option<HWND> {
        unsafe {
            let window = WindowFromPoint(POINT {
                x: point.x,
                y: point.y,
            });
            if window.is_invalid() {
                return None;
            }
            let root = GetAncestor(window, GA_ROOT);
            Some(if root.is_invalid() { window } else { root })
        }
    }
    pub fn message_loop() -> Result<()> {
        unsafe {
            let mut msg = MSG::default();
            loop {
                let r = GetMessageW(&mut msg, None, 0, 0).0;
                if r == -1 {
                    return Err(Error::from_win32());
                }
                if r == 0 {
                    return Ok(());
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
    pub fn singleton(name: &str) -> Result<Option<HANDLE>> {
        unsafe {
            let handle = CreateMutexW(None, false, &HSTRING::from(name))?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(handle)?;
                Ok(None)
            } else {
                Ok(Some(handle))
            }
        }
    }
    /// Makes `hwnd` the foreground window. Windows refuses foreground changes
    /// requested by a background process, so a plain `SetForegroundWindow` is
    /// followed by the usual input-queue attachment and, as a last resort, the
    /// topmost round-trip. The window is never left topmost.
    unsafe fn raise_now(hwnd: HWND) {
        if SetForegroundWindow(hwnd).as_bool() && GetForegroundWindow() == hwnd {
            return;
        }
        // AttachThreadInput only works for threads that own an input queue. The
        // core runs this on a worker thread, so make sure the queue exists.
        let mut message = MSG::default();
        let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
        let foreground = GetForegroundWindow();
        let foreground_thread = if foreground.0.is_null() {
            0
        } else {
            GetWindowThreadProcessId(foreground, None)
        };
        let current_thread = GetCurrentThreadId();
        let attached = foreground_thread != 0
            && foreground_thread != current_thread
            && AttachThreadInput(foreground_thread, current_thread, true).as_bool();
        let _ = SetForegroundWindow(hwnd);
        if attached {
            let _ = AttachThreadInput(foreground_thread, current_thread, false);
        }
        if GetForegroundWindow() != hwnd {
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            let _ = SetWindowPos(
                hwnd,
                HWND_NOTOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
            );
        }
    }
    /// How long a repeated settings request shakes the open page.
    pub const ALERT_DURATION: std::time::Duration = std::time::Duration::from_secs(1);
    /// Brings a window to the front and shakes it horizontally for `duration`,
    /// then restores its exact position. This is how the application points at
    /// the settings page when it is already open. Returns whether the window
    /// could be moved; a maximized window is only brought forward.
    pub fn alert_window(hwnd: HWND, duration: std::time::Duration) -> bool {
        unsafe {
            if hwnd.0.is_null() || !IsWindow(hwnd).as_bool() {
                return false;
            }
            // A minimized page is restored first, otherwise the shake would be
            // invisible.
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            let _ = SetWindowPos(
                hwnd,
                HWND_TOP,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
            );
            raise_now(hwnd);
            // A maximized window has no room to move; bringing it forward is
            // the whole reminder.
            if IsZoomed(hwnd).as_bool() {
                return true;
            }
            let mut rect = RECT::default();
            if GetWindowRect(hwnd, &mut rect).is_err() {
                return false;
            }
            let (x, y) = (rect.left, rect.top);
            // 8 logical pixels at 7.5 shakes per second reads as a quick nudge
            // without pulling the page away from where the user left it.
            let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
            let amplitude = (8.0 * scale).round().max(4.0) as i32;
            let start = std::time::Instant::now();
            let mut offset = 0;
            while start.elapsed() < duration {
                if !IsWindow(hwnd).as_bool() {
                    return false;
                }
                let phase = start.elapsed().as_secs_f32() * std::f32::consts::TAU * 7.5;
                let dx = (phase.sin() * amplitude as f32).round() as i32;
                if dx != offset {
                    let _ = SetWindowPos(
                        hwnd,
                        HWND_TOP,
                        x + dx,
                        y,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
                    );
                    offset = dx;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let _ = SetWindowPos(
                hwnd,
                HWND_TOP,
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            );
            true
        }
    }
}

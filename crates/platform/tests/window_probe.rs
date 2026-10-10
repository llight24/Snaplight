//! Integration checks for the UI Automation probe: a full-screen overlay must
//! not hide the window below it once a one-pixel hole is punched, and the hole
//! must be gone again after the guard is dropped.
//!
//! These checks need a real desktop (a visible topmost window and a window
//! manager that hit-tests); they do not depend on what the user has open, since
//! the windows under test are created last and therefore above other topmost
//! windows.
#![cfg(windows)]

use snaplight_domain::{ScreenPoint, ScreenRect};
use snaplight_platform::native::{ElementProbe, PointHole};
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, msg, w, l)
}

unsafe fn rect_of(hwnd: HWND) -> ScreenRect {
    let mut rect = RECT::default();
    GetWindowRect(hwnd, &mut rect).expect("window rect");
    ScreenRect {
        x: rect.left,
        y: rect.top,
        width: rect.right - rect.left,
        height: rect.bottom - rect.top,
    }
}

struct Window(HWND);
impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}

#[test]
fn probe_sees_through_the_overlay_hole_and_restores_it() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance = GetModuleHandleW(None).expect("module");
        let class = w!("Snaplight.Test.HoverTarget");
        let wc = WNDCLASSW {
            hInstance: instance.into(),
            lpszClassName: class,
            lpfnWndProc: Some(window_proc),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        // A window with one classic Win32 child control: the child has a UI
        // Automation element whose rectangle equals its own window rectangle,
        // which makes it an independent oracle for the detected region.
        let target = Window(
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                class,
                w!("Snaplight Hover Target"),
                WS_POPUP,
                left + width / 2 - 160,
                top + height / 2 - 120,
                320,
                240,
                None,
                None,
                instance,
                None,
            )
            .expect("target window"),
        );
        let button = Window(
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("Button"),
                w!("Probe"),
                WS_CHILD | WS_VISIBLE,
                40,
                40,
                200,
                120,
                target.0,
                None,
                instance,
                None,
            )
            .expect("child button"),
        );
        let _ = ShowWindow(target.0, SW_SHOWNOACTIVATE);
        let _ = ShowWindow(button.0, SW_SHOW);
        let button_rect = rect_of(button.0);
        let point = ScreenPoint {
            x: button_rect.x + button_rect.width / 2,
            y: button_rect.y + button_rect.height / 2,
        };

        let overlay = Window(
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOREDIRECTIONBITMAP,
                class,
                w!("Snaplight Test Overlay"),
                WS_POPUP,
                left,
                top,
                width,
                height,
                None,
                None,
                instance,
                None,
            )
            .expect("overlay window"),
        );
        let _ = ShowWindow(overlay.0, SW_SHOWNOACTIVATE);

        let probe = ElementProbe::new().expect("UI Automation probe");
        let covered = probe.bounds_at(point);
        assert_ne!(
            covered,
            Some(button_rect),
            "the overlay must hide the control below it"
        );

        let hole = PointHole::punch(overlay.0, point).expect("hole");
        let detected = probe.bounds_at(point);
        drop(hole);
        assert_eq!(
            detected,
            Some(button_rect),
            "the hole must expose the control below the overlay"
        );
        assert_ne!(
            probe.bounds_at(point),
            Some(button_rect),
            "dropping the guard must restore the overlay's shape"
        );
    }
}

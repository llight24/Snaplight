use snaplight_domain::ScreenPoint;
use snaplight_platform::{load_window_state, save_window_state};
use snaplight_ui::PhysicalPosition;
use windows::Win32::{
    Foundation::*, Graphics::Gdi::*, System::Threading::*, UI::WindowsAndMessaging::*,
};

/// Places the tool options next to the toolbar: above it when the captured
/// desktop has room, otherwise below, always clamped to the capture.
pub(super) fn options_position(
    screen: RECT,
    x: i32,
    toolbar_y: i32,
    toolbar_height: i32,
    size: (i32, i32),
) -> (i32, i32) {
    const MARGIN: i32 = 8;
    let x = x.clamp(screen.left, (screen.right - size.0).max(screen.left));
    let above = toolbar_y - size.1 - MARGIN;
    let y = if above >= screen.top {
        above
    } else {
        toolbar_y + toolbar_height + MARGIN
    };
    (x, y)
}
/// Finds a top-level window of this process by title. Slint owns its windows, so
/// the editor only learns about the handle after the window has been created; the
/// title is applied with the rest of the window properties, hence the search.
pub(super) fn find_process_window(title: &str) -> Option<HWND> {
    unsafe extern "system" fn visit(hwnd: HWND, param: LPARAM) -> BOOL {
        // SAFETY: `param` points to the `(title, result)` tuple owned by the
        // caller below, which outlives the enumeration.
        let state = unsafe { &mut *(param.0 as *mut (String, Option<HWND>)) };
        let mut pid = 0u32;
        let mut text = [0u16; 128];
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid != GetCurrentProcessId() {
                return BOOL(1);
            }
            let length = GetWindowTextW(hwnd, &mut text) as usize;
            if title_matches(&text[..length], &state.0) {
                state.1 = Some(hwnd);
                return BOOL(0);
            }
        }
        BOOL(1)
    }
    let mut state: (String, Option<HWND>) = (title.to_string(), None);
    unsafe {
        let _ = EnumWindows(
            Some(visit),
            LPARAM((&mut state as *mut (String, Option<HWND>)) as isize),
        );
    }
    state.1
}
/// Compares a window title (UTF-16, without terminator) with `wanted`.
fn title_matches(text: &[u16], wanted: &str) -> bool {
    text.len() == wanted.encode_utf16().count() && text.iter().copied().eq(wanted.encode_utf16())
}
/// Gives `hwnd` the application icon embedded in this executable. Windows takes
/// the title-bar and taskbar icon from `WM_SETICON`, not from the executable
/// resource, and the window owns the handles afterwards, so this runs once per
/// window.
unsafe fn apply_window_icon(hwnd: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{ICON_BIG, ICON_SMALL};
    // An icon was set before (or the class provides one): leave it alone.
    let existing =
        unsafe { SendMessageW(hwnd, WM_GETICON, WPARAM(ICON_SMALL as usize), LPARAM(0)) };
    if existing.0 != 0 {
        return;
    }
    for (flag, small) in [(ICON_SMALL, true), (ICON_BIG, false)] {
        if let Ok(icon) = snaplight_platform::native::app_icon(small) {
            unsafe {
                SendMessageW(
                    hwnd,
                    WM_SETICON,
                    WPARAM(flag as usize),
                    LPARAM(icon.0 as isize),
                )
            };
        }
    }
}
/// Applies the app icon to a Slint window once it exists. Slint creates its
/// windows on the event-loop thread, so poll from a helper thread and only touch
/// the window from the event loop.
pub(super) fn apply_icon_when_ready(title: &'static str) {
    std::thread::spawn(move || {
        for _ in 0..100 {
            if find_process_window(title).is_some() {
                snaplight_ui::invoke(move || unsafe {
                    if let Some(hwnd) = find_process_window(title) {
                        apply_window_icon(hwnd);
                    }
                });
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
}
/// Rounds off a frameless Slint window and keeps it out of the taskbar, so the
/// floater sits over the capture instead of looking like a document window. The
/// window is created asynchronously by Slint, so poll for it in the background.
fn round_window_when_ready(title: &'static str, radius: f32) {
    std::thread::spawn(move || {
        for _ in 0..100 {
            if find_process_window(title).is_some() {
                snaplight_ui::invoke(move || unsafe {
                    if let Some(hwnd) = find_process_window(title) {
                        round_window(hwnd, radius);
                        raise_window(hwnd);
                    }
                });
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
}
/// Corner radius of a floating window, in logical pixels: the toolbar and the
/// colour picker are cards, the options panel and the paint bar are capsules of
/// their own height.
fn window_radius(title: &str, logical_size: (u32, u32)) -> f32 {
    match title {
        snaplight_ui::TOOLBAR_TITLE => TOOLBAR_CORNER,
        snaplight_ui::PICKER_TITLE => PICKER_CORNER,
        _ => logical_size.1 as f32 / 2.0,
    }
}
/// Rounds off a window once it really has the size it was asked for, then raises
/// it. Changing the width of a shown Slint window takes an event-loop turn, and a
/// region built before that would clip the window to the old size, so the size is
/// polled in the background the same way window creation is.
pub(super) fn reshape_when_sized(title: &'static str, logical_size: (u32, u32), dpi: u32) {
    let expected = snaplight_ui::scale_to_physical(logical_size, dpi);
    let radius = window_radius(title, logical_size);
    std::thread::spawn(move || {
        for _ in 0..50 {
            if let Some(hwnd) = find_process_window(title) {
                let mut rect = RECT::default();
                let sized = unsafe {
                    GetWindowRect(hwnd, &mut rect).is_ok()
                        && (rect.right - rect.left, rect.bottom - rect.top) == expected
                };
                if sized {
                    snaplight_ui::invoke(move || unsafe {
                        if let Some(hwnd) = find_process_window(title) {
                            round_window(hwnd, radius);
                            raise_window(hwnd);
                        }
                    });
                    return;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
}
/// Raises `hwnd` to the top of the topmost band. The capture overlay takes the
/// mouse while a selection is dragged, which also puts it above the pill; a
/// window left below it would be dimmed by the overlay and receive no clicks.
unsafe fn raise_window(hwnd: HWND) {
    let _ = SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
}
/// Corner radius of the toolbar and the colour picker, in logical pixels; the
/// options panel and the paint bar are capsules of their own height instead.
const TOOLBAR_CORNER: f32 = 10.0;
const PICKER_CORNER: f32 = 12.0;
/// Re-applies the rounded shape and raises a Slint window that was just shown.
/// Showing an already composited window brings the system title bar back, so the
/// frame has to be dropped again after every show, not only at creation.
pub(super) fn reshape_and_raise(title: &'static str, logical_size: (u32, u32)) {
    let radius = window_radius(title, logical_size);
    match find_process_window(title) {
        Some(hwnd) => unsafe {
            round_window(hwnd, radius);
            raise_window(hwnd);
        },
        None => round_window_when_ready(title, radius),
    }
}
const TOOL_WINDOW_SUBCLASS: usize = 0x534c5442;
pub(super) fn tool_window_style(style: isize) -> isize {
    style
        & !((WS_CAPTION | WS_THICKFRAME | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX).0 as isize)
}
/// Winit may apply window attributes again during activation and UI updates.
/// Keep the native frame contract in force instead of repairing it after paint.
unsafe extern "system" fn tool_window_proc(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    id: usize,
    _data: usize,
) -> LRESULT {
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass};
    match msg {
        WM_STYLECHANGING if w.0 as i32 == GWL_STYLE.0 => {
            if l.0 != 0 {
                let style = &mut *(l.0 as *mut STYLESTRUCT);
                style.styleNew = tool_window_style(style.styleNew as isize) as u32;
            }
        }
        WM_NCCALCSIZE => return LRESULT(0),
        WM_NCPAINT => return LRESULT(0),
        WM_NCACTIVATE => return LRESULT(1),
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(hwnd, Some(tool_window_proc), id);
        }
        _ => {}
    }
    DefSubclassProc(hwnd, msg, w, l)
}
/// Clips `hwnd` to a stadium shaped region and drops it from the taskbar.
unsafe fn round_window(hwnd: HWND, radius: f32) {
    let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96);
    let mut rect = RECT::default();
    // The region is rebuilt every time: the window keeps it across a hide/show
    // cycle, but a window that changed size needs a region that matches. A
    // successful SetWindowRgn hands the region over to the system.
    if GetWindowRect(hwnd, &mut rect).is_ok() {
        let diameter = ((radius * dpi as f32 / 96.0).round() as i32 * 2).max(2);
        let region = CreateRoundRectRgn(
            0,
            0,
            rect.right - rect.left + 1,
            rect.bottom - rect.top + 1,
            diameter,
            diameter,
        );
        if SetWindowRgn(hwnd, region, true) == 0 {
            let _ = DeleteObject(HGDIOBJ(region.0));
        }
    }
    // Subclass installation and style changes run on the Slint window thread.
    let _ = windows::Win32::UI::Shell::SetWindowSubclass(
        hwnd,
        Some(tool_window_proc),
        TOOL_WINDOW_SUBCLASS,
        0,
    );
    let frame = GetWindowLongPtrW(hwnd, GWL_STYLE);
    let wanted_frame = tool_window_style(frame);
    let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
    let wanted_style = (style | WS_EX_TOOLWINDOW.0 as isize) & !(WS_EX_APPWINDOW.0 as isize);
    if frame != wanted_frame || style != wanted_style {
        SetWindowLongPtrW(hwnd, GWL_STYLE, wanted_frame);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, wanted_style);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}
/// Places the toolbar inside the captured region when the user has never moved
/// it: bottom-left, so it stays visible on a full-screen capture.
/// Off-screen home of the pill windows: they are shown from the start (Slint
/// keeps rendering them, and showing a window again reinstates the system title
/// bar) but stay parked outside the virtual screen until a selection places them.
pub(super) fn parked_position(size: (i32, i32)) -> (i32, i32) {
    let left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    (left - size.0 - 16, top - size.1 - 16)
}
pub(super) fn initial_toolbar_position(bounds: RECT, size: (i32, i32)) -> (i32, i32) {
    const MARGIN: i32 = 16;
    let x = bounds.left + MARGIN;
    let y = (bounds.bottom - size.1 - MARGIN).max(bounds.top + MARGIN);
    (x, y)
}
/// Places the toolbar next to a screen-space rectangle, below it when there is
/// room and above it otherwise, clamped to the captured screen.
pub(super) fn follow_position(screen: RECT, anchor: RECT, size: (i32, i32)) -> (i32, i32) {
    const MARGIN: i32 = 16;
    let max_x = (screen.right - size.0 - MARGIN).max(screen.left + MARGIN);
    let x = anchor.left.clamp(screen.left + MARGIN, max_x);
    let below = anchor.bottom + MARGIN;
    let y = if below + size.1 + MARGIN <= screen.bottom {
        below
    } else {
        (anchor.top - size.1 - MARGIN).max(screen.top + MARGIN)
    };
    (x, y)
}
/// Work area of the monitor that shows `anchor`, in physical screen
/// coordinates. The pinned image spans its monitor, so the two floating windows
/// are kept off the taskbar and off neighbouring monitors. When the monitor
/// lookup fails the whole virtual screen is used instead: an oversized bound is
/// a safer fallback than a zero-size one.
pub(super) fn monitor_work_area(anchor: RECT) -> RECT {
    unsafe {
        let monitor = MonitorFromRect(&anchor, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            return info.rcWork;
        }
        let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        RECT {
            left,
            top,
            right: left + width,
            bottom: top + height,
        }
    }
}
/// Places the toolbar and the paint bar as one pair around a pinned image, so
/// the two floating windows never land on top of each other.
///
/// Both are stacked below the pin when the monitor's work area has room for the
/// pair, otherwise above it. The toolbar, the gap and the paint bar are then
/// clamped to the work area as one stack, and both y coordinates are derived
/// from that single position, so a pin that fills or overflows the work area
/// still yields two ordered, non-overlapping windows there. A window wider than
/// the pin keeps its own width instead of being squeezed into the picture, and
/// each is clamped horizontally on its own. Only a work area too short to hold
/// the stack can push it outside.
pub(super) fn pin_floating_positions(
    pin_rect: RECT,
    toolbar_size: (i32, i32),
    paint_size: (i32, i32),
    work_area: RECT,
) -> ((i32, i32), (i32, i32)) {
    const GAP: i32 = 8;
    const EDGE: i32 = 8;
    let clamp_x = |width: i32| {
        let max = (work_area.right - width - EDGE).max(work_area.left + EDGE);
        pin_rect.left.clamp(work_area.left + EDGE, max)
    };
    // The toolbar, the gap and the paint bar are clamped as a single stack, so
    // the two y coordinates come from one position and the windows keep their
    // order instead of collapsing onto the same row when neither side fits.
    let stack_height = toolbar_size.1 + GAP + paint_size.1;
    // Below the pin the toolbar comes first, the paint bar under it.
    let below = pin_rect.bottom + EDGE;
    // Above the pin the toolbar sits over the paint bar, and the paint bar is
    // the one left adjacent to the pin.
    let above = pin_rect.top - EDGE - stack_height;
    let stack_top = if below + stack_height + EDGE <= work_area.bottom {
        below
    } else if above - EDGE >= work_area.top {
        above
    } else {
        below
    };
    let max_top = (work_area.bottom - stack_height - EDGE).max(work_area.top + EDGE);
    let stack_top = stack_top.clamp(work_area.top + EDGE, max_top);
    let paint_top = stack_top + toolbar_size.1 + GAP;
    (
        (clamp_x(toolbar_size.0), stack_top),
        (clamp_x(paint_size.0), paint_top),
    )
}
/// Bottom centre of the captured desktop: under the drawing hand but clear of
/// the toolbar, which parks in the bottom left corner.
pub(super) fn paint_bar_position(screen: RECT, size: (i32, i32)) -> (i32, i32) {
    const MARGIN: i32 = 16;
    let centered = screen.left + (screen.right - screen.left - size.0) / 2;
    (
        centered.clamp(
            screen.left + MARGIN,
            (screen.right - size.0 - MARGIN).max(screen.left + MARGIN),
        ),
        (screen.bottom - size.1 - MARGIN).max(screen.top + MARGIN),
    )
}
/// Puts the floating windows back in front of the capture overlay. Clicking the
/// capture activates it, and an activated topmost window takes the top of the
/// topmost band, which would leave the toolbar and the paint bar behind the
/// full-screen overlay: visible to Slint, unreachable for the pointer.
pub(super) fn raise_floating_windows() {
    for title in [
        snaplight_ui::TOOLBAR_TITLE,
        snaplight_ui::PAINT_BAR_TITLE,
        snaplight_ui::OPTIONS_TITLE,
        snaplight_ui::PICKER_TITLE,
    ] {
        if let Some(hwnd) = find_process_window(title) {
            unsafe { raise_window(hwnd) };
        }
    }
}
/// Restores a saved window position, in screen physical coordinates.
pub(super) fn restore_position(window: &snaplight_ui::Window, saved: Option<ScreenPoint>) {
    if let Some(p) = saved {
        window.set_position(PhysicalPosition::new(p.x, p.y));
    }
}
/// Persists a window position without disturbing the other window's entry.
pub(super) fn remember_position(settings_window: bool, window: &snaplight_ui::Window) {
    let mut state = load_window_state();
    let position = window.position();
    // A minimized window reports the (-32000, -32000) sentinel; persisting it
    // would make the next launch restore a window that is off-screen.
    if position.x <= -30000 && position.y <= -30000 {
        return;
    }
    let point = ScreenPoint {
        x: position.x,
        y: position.y,
    };
    if settings_window {
        state.settings = Some(point);
    } else {
        state.toolbar = Some(point);
    }
    let _ = save_window_state(&state);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }
    #[test]
    fn tool_windows_reject_frame_bits_without_losing_visibility() {
        let original = (WS_VISIBLE
            | WS_POPUP
            | WS_CAPTION
            | WS_THICKFRAME
            | WS_SYSMENU
            | WS_MINIMIZEBOX
            | WS_MAXIMIZEBOX)
            .0 as isize;
        let clean = tool_window_style(original);
        assert_eq!(clean, (WS_VISIBLE | WS_POPUP).0 as isize);
        assert_eq!(tool_window_style(clean), clean);
    }
    #[test]
    fn paint_bar_sits_at_the_bottom_centre() {
        let screen = rect(0, 0, 3840, 2160);
        assert_eq!(
            paint_bar_position(screen, (700, 52)),
            ((3840 - 700) / 2, 2160 - 52 - 16)
        );
        // A capture narrower than the bar still shows its left edge.
        assert_eq!(
            paint_bar_position(rect(0, 0, 600, 400), (700, 52)),
            (16, 400 - 52 - 16)
        );
        // Negative monitor origins are honoured.
        assert_eq!(
            paint_bar_position(rect(-1920, -200, 0, 880), (700, 52)),
            (-1920 + (1920 - 700) / 2, 880 - 52 - 16)
        );
    }
    #[test]
    fn toolbar_placement_stays_inside_the_capture() {
        let screen = rect(0, 0, 3840, 2160);
        // Without a selection the toolbar sits in the capture's bottom-left corner.
        assert_eq!(
            initial_toolbar_position(screen, (1110, 264)),
            (16, 2160 - 264 - 16)
        );
        // A selection with room below it keeps the toolbar underneath.
        assert_eq!(
            follow_position(screen, rect(100, 100, 500, 300), (1110, 264)),
            (100, 316)
        );
        // A selection at the bottom pushes the toolbar above it.
        assert_eq!(
            follow_position(screen, rect(100, 1800, 500, 2100), (1110, 264)),
            (100, 1800 - 264 - 16)
        );
        // A selection at the right edge keeps the whole toolbar on screen.
        assert_eq!(
            follow_position(screen, rect(3800, 100, 3840, 300), (1110, 264)),
            (3840 - 1110 - 16, 316)
        );
        // Negative monitor origins are honored.
        assert_eq!(
            initial_toolbar_position(rect(-1920, -200, 0, 880), (400, 200)),
            (-1904, 880 - 200 - 16)
        );
        // A capture smaller than the toolbar still shows its top-left corner.
        assert_eq!(
            initial_toolbar_position(rect(0, 0, 300, 200), (400, 300)),
            (16, 16)
        );
    }
    /// A window rectangle from its top-left and size.
    fn window_at(pos: (i32, i32), size: (i32, i32)) -> RECT {
        rect(pos.0, pos.1, pos.0 + size.0, pos.1 + size.1)
    }
    fn intersect(a: RECT, b: RECT) -> bool {
        a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
    }
    fn contains(outer: RECT, inner: RECT) -> bool {
        inner.left >= outer.left
            && inner.top >= outer.top
            && inner.right <= outer.right
            && inner.bottom <= outer.bottom
    }
    /// Asserts the pair stays on the work area and never overlaps. The toolbar
    /// is the capture's eight-command pill, the paint bar is one tool row.
    fn assert_pair(pin: RECT, toolbar: (i32, i32), paint: (i32, i32), work: RECT) {
        let (toolbar_pos, paint_pos) = pin_floating_positions(pin, toolbar, paint, work);
        let toolbar_rect = window_at(toolbar_pos, toolbar);
        let paint_rect = window_at(paint_pos, paint);
        assert!(contains(work, toolbar_rect), "toolbar {toolbar_rect:?}");
        assert!(contains(work, paint_rect), "paint {paint_rect:?}");
        assert!(!intersect(toolbar_rect, paint_rect));
    }
    #[test]
    fn pin_floating_pair_stacks_below_a_small_picture() {
        // The Q24 picture: wider tools must not be squeezed into the pin.
        let work = rect(0, 0, 2560, 1440);
        assert_pair(rect(300, 250, 620, 430), (758, 52), (430, 52), work);
        // A tiny pin high on the screen still gets both windows, below it.
        assert_pair(rect(100, 40, 180, 140), (758, 52), (730, 52), work);
    }
    #[test]
    fn pin_floating_pair_handles_screen_edges_and_origins() {
        // Bottom-right corner: the pair moves above the pin and shifts left.
        assert_pair(
            rect(2200, 1180, 2520, 1360),
            (758, 52),
            (730, 52),
            rect(0, 0, 2560, 1440),
        );
        // Negative monitor origin.
        assert_pair(
            rect(-1820, -160, -1500, 20),
            (758, 52),
            (730, 52),
            rect(-1920, -200, 0, 880),
        );
        // A pin near the left edge keeps the wider paint bar on screen.
        assert_pair(
            rect(-1900, 300, -1580, 480),
            (758, 52),
            (730, 52),
            rect(-1920, -200, 0, 880),
        );
    }
    /// The work area always has room for the stack here, so no pin rectangle
    /// may leave a window off it or put the two windows on the same row.
    #[test]
    fn pin_floating_pair_survives_a_pin_that_leaves_no_room() {
        // A pin filling the whole work area: neither side is free.
        let work = rect(0, 0, 2560, 1440);
        assert_pair(work, (758, 52), (730, 52), work);
        // A pin covering most of the work area, overflowing its sides, with too
        // little room both above and below for the pair.
        assert_pair(rect(50, 40, 2500, 1400), (758, 52), (730, 52), work);
        // A pin dragged past the bottom of the work area.
        assert_pair(rect(0, 1500, 2560, 1700), (758, 52), (730, 52), work);
        // A pin dragged past the top of the work area.
        assert_pair(rect(0, -400, 2560, -200), (758, 52), (730, 52), work);
    }
    #[test]
    fn pin_floating_pair_carries_every_tool_width() {
        let work = rect(0, 0, 2560, 1440);
        let pin = rect(1200, 600, 1520, 780);
        for paint in [(255, 52), (430, 52), (730, 52), (758, 52)] {
            assert_pair(pin, (758, 52), paint, work);
        }
    }
}

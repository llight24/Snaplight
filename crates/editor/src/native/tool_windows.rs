use snaplight_domain::ScreenPoint;
use snaplight_platform::{load_window_state, save_window_state};
use snaplight_ui::PhysicalPosition;
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use windows::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::Threading::*,
    UI::{Input::KeyboardAndMouse::VK_ESCAPE, WindowsAndMessaging::*},
};

/// Places the colour picker next to the window it was opened from: above that
/// window when the captured desktop has room, otherwise below it, always
/// clamped to the capture. The style windows are placed by `toolbar_column`.
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
                        round_window(hwnd, radius, window_kind(title));
                        raise_window(hwnd);
                    }
                });
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
}
/// What a tool window's subclass watches for, from its title.
fn window_kind(title: &str) -> usize {
    match title {
        snaplight_ui::SHAPE_MENU_TITLE => WINDOW_SHAPE_MENU,
        snaplight_ui::TOOLBAR_TITLE => WINDOW_TOOLBAR,
        _ => WINDOW_PLAIN,
    }
}
/// Corner radius of a floating window, in logical pixels: the toolbar and the
/// colour picker are cards, the options panel and the paint bar are capsules of
/// their own height.
fn window_radius(title: &str, logical_size: (u32, u32)) -> f32 {
    match title {
        snaplight_ui::TOOLBAR_TITLE => TOOLBAR_CORNER,
        snaplight_ui::PICKER_TITLE => PICKER_CORNER,
        // The shape menu is a small card, not a capsule of its own height.
        snaplight_ui::SHAPE_MENU_TITLE => MENU_CORNER,
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
                            round_window(hwnd, radius, window_kind(title));
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
/// Gives a tool window the activation. The shape menu is a drop-down: it only
/// receives Escape and reports focus loss through `WM_ACTIVATE` once it holds
/// the foreground, so the editor focuses it when it opens and hands the
/// activation back to the toolbar when it closes.
pub(super) fn focus_window(title: &'static str) {
    if let Some(hwnd) = find_process_window(title) {
        unsafe {
            let _ = SetForegroundWindow(hwnd);
        }
    }
}
/// Corner radius of the toolbar and the colour picker, in logical pixels; the
/// options panel and the paint bar are capsules of their own height instead.
const TOOLBAR_CORNER: f32 = 10.0;
const PICKER_CORNER: f32 = 12.0;
const MENU_CORNER: f32 = 8.0;
/// Re-applies the rounded shape and raises a Slint window that was just shown.
/// Showing an already composited window brings the system title bar back, so the
/// frame has to be dropped again after every show, not only at creation.
pub(super) fn reshape_and_raise(title: &'static str, logical_size: (u32, u32)) {
    let radius = window_radius(title, logical_size);
    match find_process_window(title) {
        Some(hwnd) => unsafe {
            round_window(hwnd, radius, window_kind(title));
            raise_window(hwnd);
        },
        None => round_window_when_ready(title, radius),
    }
}
const TOOL_WINDOW_SUBCLASS: usize = 0x534c5442;
/// What the subclass of a tool window watches for, carried in its reference
/// data. Every window gets the frameless treatment; only the transient shape
/// menu and the toolbar need the extra events.
const WINDOW_PLAIN: usize = 0;
const WINDOW_SHAPE_MENU: usize = 1;
const WINDOW_TOOLBAR: usize = 2;
type Hook = Arc<dyn Fn() + Send + Sync + 'static>;
thread_local! {
    /// Closes the shape menu when the user dismisses it. Set by the editor on
    /// the window thread, which is the thread that owns the menu window. The
    /// editor closes the menu without moving the focus, so losing the
    /// activation - clicking another application - never steals it back.
    static MENU_DISMISSED: RefCell<Option<Hook>> = const { RefCell::new(None) };
    /// Closes the shape menu on Escape and hands the focus back to the toolbar,
    /// which is where the keyboard was before the drop-down opened.
    static MENU_ESCAPE: RefCell<Option<Hook>> = const { RefCell::new(None) };
    /// Reports a finished toolbar drag, so the editor can close the transient
    /// menu and move the style windows with the toolbar.
    static TOOLBAR_MOVED: RefCell<Option<Hook>> = const { RefCell::new(None) };
    /// Whether the shape menu held the activation; a menu that never had it
    /// must not treat its first `WM_ACTIVATE` as a focus loss.
    static MENU_ACTIVE: Cell<bool> = const { Cell::new(false) };
}
/// Registers the handler that closes the shape menu on Escape or focus loss.
pub(super) fn set_menu_dismissed(handler: Hook) {
    MENU_DISMISSED.with(|cell| *cell.borrow_mut() = Some(handler));
}
/// Registers the handler that closes the shape menu on Escape and returns the
/// focus to the toolbar.
pub(super) fn set_menu_escape(handler: Hook) {
    MENU_ESCAPE.with(|cell| *cell.borrow_mut() = Some(handler));
}
/// Registers the handler that follows a user drag of the toolbar.
pub(super) fn set_toolbar_moved(handler: Hook) {
    TOOLBAR_MOVED.with(|cell| *cell.borrow_mut() = Some(handler));
}
/// Runs a registered handler from the event loop. The handlers move Slint
/// windows, so they must not run inside the window procedure that reported the
/// event; `invoke` also keeps a hide from re-entering the same procedure.
fn notify(cell: &'static std::thread::LocalKey<RefCell<Option<Hook>>>) {
    let handler = cell.with(|cell| cell.borrow().clone());
    if let Some(handler) = handler {
        snaplight_ui::invoke(move || handler());
    }
}
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
    data: usize,
) -> LRESULT {
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass};
    match msg {
        WM_STYLECHANGING if w.0 as i32 == GWL_STYLE.0 => {
            if l.0 != 0 {
                let style = &mut *(l.0 as *mut STYLESTRUCT);
                style.styleNew = tool_window_style(style.styleNew as isize) as u32;
            }
        }
        // Escape closes the shape menu, the way a drop-down behaves, and puts the
        // focus back on the toolbar. The menu is shown activated, so it receives
        // the key.
        WM_KEYDOWN if data == WINDOW_SHAPE_MENU && w.0 as u32 == VK_ESCAPE.0 as u32 => {
            notify(&MENU_ESCAPE);
        }
        // Clicking anywhere else takes the activation away; a drop-down closes
        // with it and must not take the focus back from the window the user
        // clicked. `MENU_ACTIVE` ignores the `WM_ACTIVATE` a window gets while
        // it is still being created or shown, which is not a focus loss.
        WM_ACTIVATE if data == WINDOW_SHAPE_MENU => {
            if (w.0 & 0xffff) as u32 == WA_INACTIVE {
                if MENU_ACTIVE.with(|active| active.replace(false)) {
                    notify(&MENU_DISMISSED);
                }
            } else {
                MENU_ACTIVE.with(|active| active.set(true));
            }
        }
        // A native hide of the toolbar - not routed through the Slint callback -
        // still has to take the transient menu down with it. `l` is zero on
        // WM_SHOWWINDOW when the window is being hidden.
        WM_SHOWWINDOW if data == WINDOW_TOOLBAR && l.0 == 0 => notify(&MENU_DISMISSED),
        // A user drag moves the pill, and the style windows hang off it; the
        // move is reported once the drag loop has finished.
        WM_EXITSIZEMOVE if data == WINDOW_TOOLBAR => notify(&TOOLBAR_MOVED),
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
unsafe fn round_window(hwnd: HWND, radius: f32, kind: usize) {
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
        kind,
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
/// Gap between the toolbar and the windows anchored to it: the shape menu sits
/// this far above it, the style windows this far below it and this far apart.
pub(super) const ANCHOR_GAP: i32 = 8;
/// Inset kept between an anchored window and the monitor work area edge.
const ANCHOR_EDGE: i32 = 8;
/// Clamps a window's left edge to the work area, leaving `ANCHOR_EDGE` free at
/// both sides. Negative monitor origins go through the same clamp.
fn anchored_left(work_area: RECT, left: i32, width: i32) -> i32 {
    let max = (work_area.right - width - ANCHOR_EDGE).max(work_area.left + ANCHOR_EDGE);
    left.clamp(work_area.left + ANCHOR_EDGE, max)
}
/// Stacks `sizes` under `top`, each `ANCHOR_GAP` below the previous one and all
/// left edges on `left`, clamped to the work area as one column. Only a work
/// area too short to hold the column pushes it outside.
fn stack_below(top: i32, left: i32, sizes: &[(i32, i32)], work_area: RECT) -> Vec<(i32, i32)> {
    let gaps = ANCHOR_GAP * sizes.len().saturating_sub(1) as i32;
    let total: i32 = sizes.iter().map(|size| size.1).sum::<i32>() + gaps;
    let min_top = work_area.top + ANCHOR_EDGE;
    let max_top = (work_area.bottom - total - ANCHOR_EDGE).max(min_top);
    let mut y = top.clamp(min_top, max_top);
    sizes
        .iter()
        .map(|size| {
            let position = (anchored_left(work_area, left, size.0), y);
            y += size.1 + ANCHOR_GAP;
            position
        })
        .collect()
}
/// The toolbar and the style windows hanging off it, laid out as one column.
pub(super) struct ToolbarColumn {
    pub(super) toolbar: (i32, i32),
    pub(super) style: Vec<(i32, i32)>,
}
/// Lays out the toolbar and the style windows below it as one column inside the
/// monitor work area: the windows sit `ANCHOR_GAP` apart, each is clamped
/// horizontally on its own, and the toolbar moves the few pixels that keep the
/// style windows on the work area instead of letting them overlap the toolbar.
/// As the style windows change with the tool, this is what makes the toolbar
/// step aside for the paint bar and the options panel. A pinned image reuses it
/// once its pair position has placed the toolbar.
pub(super) fn toolbar_column(
    toolbar: RECT,
    style: &[(i32, i32)],
    work_area: RECT,
) -> ToolbarColumn {
    let toolbar_size = (toolbar.right - toolbar.left, toolbar.bottom - toolbar.top);
    let gaps = ANCHOR_GAP * style.len().saturating_sub(1) as i32;
    let style_height: i32 = style.iter().map(|size| size.1).sum::<i32>() + gaps;
    let below = if style.is_empty() {
        0
    } else {
        ANCHOR_GAP + style_height
    };
    let min_top = work_area.top + ANCHOR_EDGE;
    let max_top = (work_area.bottom - ANCHOR_EDGE - toolbar_size.1 - below).max(min_top);
    let toolbar_y = toolbar.top.clamp(min_top, max_top);
    let toolbar_x = anchored_left(work_area, toolbar.left, toolbar_size.0);
    let mut y = toolbar_y + toolbar_size.1 + ANCHOR_GAP;
    let mut positions = Vec::with_capacity(style.len());
    for size in style {
        positions.push((anchored_left(work_area, toolbar_x, size.0), y));
        y += size.1 + ANCHOR_GAP;
    }
    ToolbarColumn {
        toolbar: (toolbar_x, toolbar_y),
        style: positions,
    }
}
/// Where the shape menu opened, and whether the style windows had to move.
pub(super) struct ShapeMenuPlacement {
    pub(super) menu: (i32, i32),
    /// New positions for the style windows when the menu had to open under the
    /// toolbar; `None` leaves them where the toolbar column put them.
    pub(super) style: Option<Vec<(i32, i32)>>,
}
/// Places the shape menu above the toolbar, `ANCHOR_GAP` clear of it and
/// anchored at `anchor_x`, the toolbar's shape entry. Both edges are clamped to
/// the monitor work area, negative monitor origins included. When the work area
/// has no room above the toolbar - a selection against the top edge - the menu
/// opens `ANCHOR_GAP` below it instead and the style windows move below the
/// menu, so neither the toolbar nor the style windows end up covered.
pub(super) fn shape_menu_column(
    toolbar: RECT,
    anchor_x: i32,
    size: (i32, i32),
    style: &[(i32, i32)],
    work_area: RECT,
) -> ShapeMenuPlacement {
    let menu_x = anchored_left(work_area, anchor_x, size.0);
    let above = toolbar.top - ANCHOR_GAP - size.1;
    if above >= work_area.top {
        return ShapeMenuPlacement {
            menu: (menu_x, above),
            style: None,
        };
    }
    let lowest = (work_area.bottom - size.1 - ANCHOR_EDGE).max(work_area.top + ANCHOR_EDGE);
    let menu_y = (toolbar.bottom + ANCHOR_GAP).min(lowest);
    let style_positions = stack_below(menu_y + size.1 + ANCHOR_GAP, toolbar.left, style, work_area);
    ShapeMenuPlacement {
        menu: (menu_x, menu_y),
        style: Some(style_positions),
    }
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
    /// The toolbar and the style windows below it stay ordered, inside the work
    /// area, and never overlap. `toolbar` is the pill's physical size, `style`
    /// one entry per window hanging under it.
    fn assert_column(toolbar: (i32, i32), style: &[(i32, i32)], work: RECT) -> ToolbarColumn {
        let placed = toolbar_column(window_at((200, 300), toolbar), style, work);
        let toolbar_rect = window_at(placed.toolbar, toolbar);
        assert!(contains(work, toolbar_rect), "toolbar {toolbar_rect:?}");
        let mut previous = toolbar_rect;
        for (position, size) in placed.style.iter().zip(style) {
            let style_rect = window_at(*position, *size);
            assert!(contains(work, style_rect), "style {style_rect:?}");
            assert!(!intersect(previous, style_rect));
            assert_eq!(style_rect.top, previous.bottom + ANCHOR_GAP);
            previous = style_rect;
        }
        placed
    }
    #[test]
    fn style_windows_stack_under_the_toolbar() {
        // The toolbar and the style windows of a capture session: the bar first,
        // the options panel under it, all clamped to the same work area.
        let work = rect(0, 0, 2560, 1440);
        assert_column((706, 52), &[(730, 52)], work);
        assert_column((706, 52), &[(730, 52), (780, 52)], work);
        // A toolbar already at the bottom edge steps up instead of letting the
        // style windows overlap it.
        let placed = toolbar_column(
            window_at((100, work.bottom - 52 - 4), (706, 52)),
            &[(730, 52)],
            work,
        );
        assert_eq!(placed.style[0].1, placed.toolbar.1 + 52 + ANCHOR_GAP);
        assert!(contains(work, window_at(placed.style[0], (730, 52))));
        // Negative monitor origins and a work area with a taskbar at the top.
        let negative = rect(-1920, -200, 0, 880);
        assert_column((706, 52), &[(730, 52)], negative);
        let shifted = toolbar_column(window_at((-1900, 300), (706, 52)), &[(730, 52)], negative);
        assert!(shifted.style[0].0 >= negative.left + ANCHOR_EDGE);
        // A window wider than the work area keeps its left edge on the work area
        // instead of disappearing to the right.
        let narrow = rect(0, 0, 600, 400);
        let placed = toolbar_column(window_at((0, 0), (706, 52)), &[(730, 52)], narrow);
        assert_eq!(placed.toolbar.0, ANCHOR_EDGE);
        assert_eq!(placed.style[0].0, ANCHOR_EDGE);
    }
    #[test]
    fn scaled_dpi_columns_stay_ordered_and_clamped() {
        // 144 dpi scales every window by 1.5: the toolbar 706x52 -> 1059x78, the
        // paint bar 730x52 -> 1095x78 and the shape menu 100x52 -> 150x78. The
        // column has to stay ordered and inside a monitor whose origin is
        // negative, exactly as it does at 96 dpi.
        let work = rect(-2560, -200, 0, 1440);
        let toolbar = (1059, 78);
        let bar = (1095, 78);
        let placed = assert_column(toolbar, &[bar], work);
        // The bar is wider than the toolbar, so its own clamp decides the shared
        // left edge rather than the toolbar's.
        assert!(contains(work, window_at(placed.style[0], bar)));
        let menu = (150, 78);
        let above = shape_menu_column(
            window_at((work.left + 100, 300), toolbar),
            work.left + 100 + 108,
            menu,
            &[bar],
            work,
        );
        assert_eq!(above.menu.1, 300 - ANCHOR_GAP - 78);
        assert!(contains(work, window_at(above.menu, menu)));
        assert!(above.style.is_none());
        // With the scaled toolbar against the top edge the menu flips below the
        // pill and the bar moves under the menu, still inside the work area.
        let low = window_at((work.left + 100, work.top), toolbar);
        let flipped = shape_menu_column(low, work.left + 100 + 108, menu, &[bar], work);
        let menu_rect = window_at(flipped.menu, menu);
        assert_eq!(menu_rect.top, low.bottom + ANCHOR_GAP);
        assert!(contains(work, menu_rect));
        let moved = flipped
            .style
            .expect("the style windows move below the menu");
        let bar_rect = window_at(moved[0], bar);
        assert_eq!(bar_rect.top, menu_rect.bottom + ANCHOR_GAP);
        assert!(contains(work, bar_rect));
        assert!(!intersect(bar_rect, low));
    }
    #[test]
    fn shape_menu_opens_above_the_toolbar() {
        let work = rect(0, 0, 2560, 1440);
        let toolbar = window_at((200, 500), (706, 52));
        let placed = shape_menu_column(toolbar, 230, (100, 52), &[(730, 52)], work);
        assert_eq!(placed.menu, (230, 500 - ANCHOR_GAP - 52));
        assert!(contains(work, window_at(placed.menu, (100, 52))));
        // Above the toolbar the style windows stay where the toolbar column put
        // them; the menu never moves them.
        assert!(placed.style.is_none());
        // The menu is anchored at the shape entry but clamped to the work area,
        // incl. a monitor whose origin is negative.
        let wide = shape_menu_column(
            window_at((2400, 500), (706, 52)),
            2600,
            (100, 52),
            &[],
            work,
        );
        assert_eq!(wide.menu.0, work.right - 100 - ANCHOR_EDGE);
        let negative = rect(-1920, -200, 0, 880);
        let clamped = shape_menu_column(
            window_at((-1900, 300), (706, 52)),
            -1890,
            (100, 52),
            &[],
            negative,
        );
        assert!(contains(negative, window_at(clamped.menu, (100, 52))));
    }
    #[test]
    fn shape_menu_flips_under_a_toolbar_with_no_room_above() {
        let work = rect(0, 0, 2560, 1440);
        // A selection against the top edge leaves no room above the toolbar.
        let toolbar = window_at((200, 0), (706, 52));
        let style = [(730, 52), (780, 52)];
        let placed = shape_menu_column(toolbar, 230, (100, 52), &style, work);
        let menu = window_at(placed.menu, (100, 52));
        assert!(contains(work, menu), "menu {menu:?}");
        assert_eq!(menu.top, toolbar.bottom + ANCHOR_GAP);
        assert!(!intersect(menu, toolbar));
        // The style windows move below the menu, still ordered and clear of it.
        let moved = placed.style.expect("the style windows move below the menu");
        let mut previous = menu;
        for (position, size) in moved.iter().zip(style) {
            let window = window_at(*position, size);
            assert!(contains(work, window), "style {window:?}");
            assert!(!intersect(previous, window));
            previous = window;
        }
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

use parking_lot::Mutex;
use snaplight_domain::*;
use snaplight_platform::{
    atomic_write, capture_dir, data_dir, load_config, load_window_state, native::PointHole,
    save_config, save_window_state, write_pixels, CaptureService,
};
use snaplight_render_win::*;
use snaplight_ui::{ComponentHandle, ModelRc, OptionsPanel, PhysicalPosition, Toolbar, VecModel};
use std::{
    io::Write,
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{Com::*, DataExchange::*, LibraryLoader::*, Memory::*, Threading::*},
        UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
};

/// The interactive long capture: a timer samples the selected region while the
/// user scrolls the content, and the stitched result is trimmed and finished
/// from a bird's eye panel.
mod scroll_capture;
use scroll_capture::{parse_long_capture_demo, LongCaptureSession};

const COMMAND: u32 = WM_APP + 42;
/// Posted to the overlay window when a hover query should run.
const HOVER_PROBE: u32 = WM_APP + 43;
/// A cursor movement of at least this many physical pixels, together with
/// `HOVER_INTERVAL`, bounds how often UI Automation is queried.
const HOVER_MOVE_PX: i32 = 3;
const HOVER_INTERVAL: Duration = Duration::from_millis(33);
/// Documented value (0x02A3); the Windows bindings only expose the non-client
/// variant `WM_NCMOUSELEAVE`.
const WM_MOUSELEAVE: u32 = 0x02a3;
/// Movement, in logical pixels, below which a press is a click rather than a
/// drag and therefore adopts the detected region.
const CLICK_SLOP: f32 = 4.0;
/// Frames rendered by the `--stress` 4K annotation loop.
const STRESS_FRAMES: usize = 240;
/// Zoom limits for the canvas view. The lower bound keeps a very long capture
/// legible at least as a thumbnail; the upper bound keeps annotation handles
/// usable.
const MIN_VIEW_SCALE: f32 = 0.02;
const MAX_VIEW_SCALE: f32 = 8.0;
/// One wheel notch is this fraction of the current zoom.
const ZOOM_STEP: f32 = 1.1;
/// View pixels panned per wheel notch.
const PAN_STEP: f32 = 96.0;
/// Timer that samples the selected region while a long capture runs.
const LONG_CAPTURE_TIMER: usize = 1;
#[derive(Clone)]
enum UiCommand {
    Action(i32),
    Properties {
        width: i32,
        block: i32,
        text: String,
        swatch: i32,
    },
}
/// Grid indices match the eight handles drawn by the renderer (no centre).
#[derive(Clone, Copy, Debug, PartialEq)]
struct SelectionHandle {
    col: u8,
    row: u8,
}
/// Hover detection state of one overlay canvas.
#[derive(Default)]
struct HoverProbe {
    /// UI Automation client, created on first use on the thread that owns the
    /// overlay window. Queries run on that thread: UI Automation needs the
    /// apartment that owns the overlay to keep pumping, so waiting for another
    /// thread while blocking this one only serializes the query behind the wait.
    probe: Option<snaplight_platform::native::ElementProbe>,
    /// Screen point and time of the last query, used to bound the query rate.
    last: Option<(ScreenPoint, Instant)>,
    /// Cursor position waiting to be probed, in image pixels.
    pending: Option<ImagePoint>,
}
/// True when the pointer moved far enough, and enough time passed since the
/// previous query, to justify another one. A pointer that stands still never
/// re-queries, and a fast pointer cannot outrun the query rate.
fn probe_due(last: Option<(ScreenPoint, Instant)>, point: ScreenPoint, now: Instant) -> bool {
    match last {
        None => true,
        Some((previous, at)) => {
            (previous.x - point.x)
                .abs()
                .max((previous.y - point.y).abs())
                >= HOVER_MOVE_PX
                && now.duration_since(at) >= HOVER_INTERVAL
        }
    }
}
/// Maps a detected screen rectangle into image pixels, clipped to the overlay.
/// A region that lies entirely on another monitor yields nothing.
fn hover_region(screen: RECT, image: &ImageDescriptor, rect: ScreenRect) -> Option<ImageRect> {
    let overlay = ScreenRect {
        x: screen.left,
        y: screen.top,
        width: screen.right - screen.left,
        height: screen.bottom - screen.top,
    };
    let visible = rect.intersect(overlay)?;
    Some(
        ImageRect {
            x: (visible.x - screen.left) as f32,
            y: (visible.y - screen.top) as f32,
            width: visible.width as f32,
            height: visible.height as f32,
        }
        .clamp(image.width, image.height),
    )
}
/// Hit-tests the eight selection handles. `view_scale` is the zoom the document
/// is drawn at, so the grab radius stays a constant number of screen pixels.
fn selection_handle(
    rect: ImageRect,
    point: ImagePoint,
    dpi: u32,
    view_scale: f32,
) -> Option<SelectionHandle> {
    let radius = 8.0 * (dpi as f32 / 96.0).clamp(1.0, 4.0) / view_scale.max(MIN_VIEW_SCALE);
    // Corners take priority when a tiny selection has overlapping hit regions.
    let handles = [
        (0, 0),
        (2, 0),
        (0, 2),
        (2, 2),
        (1, 0),
        (0, 1),
        (2, 1),
        (1, 2),
    ];
    handles.into_iter().find_map(|(col, row)| {
        let x = rect.x + rect.width * col as f32 / 2.0;
        let y = rect.y + rect.height * row as f32 / 2.0;
        ((point.x - x).powi(2) + (point.y - y).powi(2) <= radius * radius)
            .then_some(SelectionHandle { col, row })
    })
}
fn resize_selection(
    rect: ImageRect,
    handle: SelectionHandle,
    point: ImagePoint,
    width: u32,
    height: u32,
) -> ImageRect {
    let (mut left, mut top, mut right, mut bottom) =
        (rect.x, rect.y, rect.x + rect.width, rect.y + rect.height);
    match handle.col {
        0 => left = point.x.clamp(0.0, right - 1.0),
        2 => right = point.x.clamp(left + 1.0, width as f32),
        _ => {}
    }
    match handle.row {
        0 => top = point.y.clamp(0.0, bottom - 1.0),
        2 => bottom = point.y.clamp(top + 1.0, height as f32),
        _ => {}
    }
    ImageRect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }
}
/// How the document is shown inside the overlay window: zoom plus pan, in view
/// pixels. A normal capture opens 1:1; a long capture opens shrunk to fit.
#[derive(Clone, Copy)]
struct ViewTransform {
    scale: f32,
    x: f32,
    y: f32,
}
impl ViewTransform {
    /// 1:1, with the document origin at the window corner.
    fn plain() -> Self {
        Self {
            scale: 1.0,
            x: 0.0,
            y: 0.0,
        }
    }
    /// Shrinks `image` to fit `view` (never magnifying) and centers it.
    fn fit(image: &ImageDescriptor, view: (u32, u32)) -> Self {
        let mut transform = Self::plain();
        transform.scale = (view.0 as f32 / image.width.max(1) as f32)
            .min(view.1 as f32 / image.height.max(1) as f32);
        transform.clamp(image, view);
        transform
    }
    /// Keeps the document covering the view while it is larger, and centered
    /// while it is smaller, so panning can never expose empty space.
    fn clamp(&mut self, image: &ImageDescriptor, view: (u32, u32)) {
        self.scale = self.scale.clamp(MIN_VIEW_SCALE, MAX_VIEW_SCALE);
        let width = image.width as f32 * self.scale;
        let height = image.height as f32 * self.scale;
        let (view_width, view_height) = (view.0 as f32, view.1 as f32);
        self.x = if width <= view_width {
            (view_width - width) / 2.0
        } else {
            self.x.clamp(view_width - width, 0.0)
        };
        self.y = if height <= view_height {
            (view_height - height) / 2.0
        } else {
            self.y.clamp(view_height - height, 0.0)
        };
    }
    fn to_view(self, p: ImagePoint) -> (f32, f32) {
        (p.x * self.scale + self.x, p.y * self.scale + self.y)
    }
    fn to_image(self, x: f32, y: f32) -> (f32, f32) {
        let scale = if self.scale > 0.0 { self.scale } else { 1.0 };
        ((x - self.x) / scale, (y - self.y) / scale)
    }
}
struct Canvas {
    hwnd: HWND,
    renderer: OverlayRenderer,
    doc: Document,
    history: History,
    interaction: Interaction,
    tool: Tool,
    style: Style,
    block: u32,
    text: String,
    start: Option<ImagePoint>,
    resize: Option<(ImageRect, SelectionHandle)>,
    active: Arc<Mutex<usize>>,
    error: Option<String>,
    // Immutable base image pixels (BGRA8, `doc.image.stride`) so GPU resources can
    // be rebuilt after device loss without changing the captured content.
    base: Vec<u8>,
    screen: RECT,
    /// Zoom and pan of the document inside this window.
    view: ViewTransform,
    device_rebuilds: u32,
    capture_dir: PathBuf,
    ui_slots: Arc<UiSlots>,
    dpi: u32,
    /// Configured detection mode; `Off` never queries UI Automation.
    detect: WindowDetect,
    hover_probe: HoverProbe,
    /// The running long-capture session, if any. While it is set, the window is
    /// hidden and the ring and panel windows stand in for it.
    long_capture: Option<LongCaptureSession>,
    /// Set by `--long-capture-demo`: the session runs against a fixed region and
    /// reports JSON instead of a notice.
    demo: bool,
}
impl Canvas {
    fn draw(&mut self) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let viewport = self.viewport();
        match self.renderer.render(&self.doc, viewport, &self.interaction) {
            Ok(()) => Ok(()),
            Err(error) if is_recoverable_device_error(&error) || self.renderer.device_lost() => {
                self.rebuild_device()?;
                self.renderer
                    .render(&self.doc, viewport, &self.interaction)?;
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }
    fn viewport(&self) -> Viewport {
        Viewport {
            scale: self.view.scale,
            offset_x: self.view.x,
            offset_y: self.view.y,
            ..Viewport::plain(
                self.view_size().0,
                self.view_size().1,
                self.dpi as f32,
                ScreenPoint {
                    x: self.screen.left,
                    y: self.screen.top,
                },
            )
        }
    }
    /// Client area of the overlay window, in physical pixels: the size of the
    /// view the document is drawn into.
    fn view_size(&self) -> (u32, u32) {
        (
            (self.screen.right - self.screen.left).max(1) as u32,
            (self.screen.bottom - self.screen.top).max(1) as u32,
        )
    }
    fn rebuild_device(&mut self) -> std::result::Result<(), Box<dyn std::error::Error>> {
        self.renderer
            .rebuild(&self.base, &self.doc.image, self.screen, self.view_size())?;
        self.device_rebuilds += 1;
        Ok(())
    }
}
impl Canvas {
    unsafe fn invalidate(&self) {
        let _ = InvalidateRect(self.hwnd, None, false);
    }
    /// True while a detected region may be shown: the select tool, nothing
    /// committed yet, and no drag in progress.
    fn hover_eligible(&self) -> bool {
        self.detect == WindowDetect::Ui
            && self.tool == Tool::Select
            && self.doc.selection.is_none()
            && self.interaction.selection_draft.is_none()
            && self.start.is_none()
            && !self.interaction.dragging
    }
    fn screen_point(&self, p: ImagePoint) -> ScreenPoint {
        let (x, y) = self.view.to_view(p);
        ScreenPoint {
            x: self.screen.left + x.round() as i32,
            y: self.screen.top + y.round() as i32,
        }
    }
    /// Replaces the preview, repainting only when it actually changed.
    fn set_hover(&mut self, region: Option<ImageRect>) {
        if self.interaction.hover != region {
            self.interaction.hover = region;
            unsafe { self.invalidate() };
        }
    }
    /// Queues a detection query for a cursor position. Movement and time
    /// thresholds keep a moving pointer from querying on every mouse message.
    unsafe fn request_hover(&mut self, p: ImagePoint) {
        if !self.hover_eligible() {
            return;
        }
        if self.hover_probe.pending.is_some() {
            // A query is already queued. Move it to the newest position instead
            // of dropping it, so the preview follows the pointer.
            self.hover_probe.pending = Some(p);
            return;
        }
        let point = self.screen_point(p);
        if !probe_due(self.hover_probe.last, point, Instant::now()) {
            return;
        }
        self.hover_probe.pending = Some(p);
        let _ = PostMessageW(self.hwnd, HOVER_PROBE, WPARAM(0), LPARAM(0));
    }
    /// Runs the queued query: a one-pixel hole in the overlay's window region
    /// lets UI Automation see the window below it, and the hole is closed as
    /// soon as the answer is in.
    unsafe fn run_hover(&mut self) {
        let Some(p) = self.hover_probe.pending.take() else {
            return;
        };
        if !self.hover_eligible() {
            return;
        }
        let point = self.screen_point(p);
        self.hover_probe.last = Some((point, Instant::now()));
        if self.hover_probe.probe.is_none() {
            // Created on first use: a session that never hovers loads no client.
            match snaplight_platform::native::ElementProbe::new() {
                Ok(probe) => self.hover_probe.probe = Some(probe),
                Err(error) => {
                    eprintln!("window detection unavailable: {error}");
                    return;
                }
            }
        }
        let Some(probe) = &self.hover_probe.probe else {
            return;
        };
        let hole = match PointHole::punch(self.hwnd, point) {
            Ok(hole) => hole,
            Err(_) => return,
        };
        let bounds = probe.bounds_at(point);
        drop(hole);
        let region = bounds.and_then(|rect| hover_region(self.screen, &self.doc.image, rect));
        self.set_hover(region);
    }
    fn position(&self, l: LPARAM) -> ImagePoint {
        let (x, y) = self
            .view
            .to_image(l.0 as u16 as i16 as f32, (l.0 >> 16) as u16 as i16 as f32);
        ImagePoint {
            x: x.clamp(0.0, self.doc.image.width as f32),
            y: y.clamp(0.0, self.doc.image.height as f32),
        }
    }
    /// `Ctrl` zooms around the cursor, otherwise the view pans. `delta` is a
    /// wheel rotation in `WM_MOUSEWHEEL` units (multiples of 120).
    fn wheel(&mut self, delta: i32, p: ImagePoint, ctrl: bool, shift: bool) {
        if delta == 0 {
            return;
        }
        if ctrl {
            let factor = if delta > 0 {
                ZOOM_STEP
            } else {
                1.0 / ZOOM_STEP
            };
            let (x, y) = self.view.to_view(p);
            self.view.scale *= factor;
            // Keep the image point under the cursor in place.
            self.view.x = x - p.x * self.view.scale;
            self.view.y = y - p.y * self.view.scale;
        } else {
            let step = delta as f32 / 120.0 * PAN_STEP;
            if shift {
                self.view.x += step;
            } else {
                self.view.y += step;
            }
        }
        let view = self.view_size();
        self.view.clamp(&self.doc.image, view);
        unsafe { self.invalidate() };
    }
    fn update_drag(&mut self, p: ImagePoint) {
        let Some(start) = self.start else {
            return;
        };
        if let Some((rect, handle)) = self.resize {
            let adjusted = ImagePoint {
                x: rect.x + rect.width * handle.col as f32 / 2.0 + p.x - start.x,
                y: rect.y + rect.height * handle.row as f32 / 2.0 + p.y - start.y,
            };
            self.interaction.selection_draft = Some(resize_selection(
                rect,
                handle,
                adjusted,
                self.doc.image.width,
                self.doc.image.height,
            ));
            return;
        }
        let r = ImageRect::between(start, p).clamp(self.doc.image.width, self.doc.image.height);
        if self.tool == Tool::Select {
            self.interaction.selection_draft = Some(r);
            return;
        }
        let shape = match self.tool {
            Tool::Rectangle => Shape::Rectangle(r),
            Tool::Ellipse => Shape::Ellipse(r),
            Tool::Arrow => Shape::Arrow { from: start, to: p },
            Tool::Mosaic => Shape::Mosaic {
                rect: r,
                block: self.block,
            },
            Tool::Pen => {
                if let Some(Annotation {
                    shape: Shape::Pen(points),
                    ..
                }) = &mut self.interaction.draft
                {
                    if points.last() != Some(&p) {
                        points.push(p);
                    }
                    return;
                }
                Shape::Pen(vec![start, p])
            }
            _ => return,
        };
        self.interaction.draft = Some(Annotation {
            shape,
            style: self.style,
        });
    }
    fn action(&mut self, id: i32) -> std::result::Result<(), Box<dyn std::error::Error>> {
        // A running long capture owns the toolbar: the four action buttons act on
        // the stitch instead of the hidden overlay document, and the drawing
        // tools, the history and the tool options do nothing. Save, copy, pin and
        // close therefore mean the same thing here as in a normal capture.
        if self.long_capture.is_some() {
            match id {
                12 => self.copy_long_capture()?,
                13 => self.finish_long_capture(),
                14 => self.pin_long_capture()?,
                15 => self.cancel_long_capture(),
                _ => {}
            }
            return Ok(());
        }
        match id {
            0..=6 => {
                self.tool = [
                    Tool::Select,
                    Tool::Rectangle,
                    Tool::Ellipse,
                    Tool::Arrow,
                    Tool::Pen,
                    Tool::Mosaic,
                    Tool::Text,
                ][id as usize];
                // Only the select tool adopts a detected region.
                self.set_hover(None);
            }
            10 => {
                self.history.undo(&mut self.doc);
                self.renderer.invalidate_annotations();
                self.set_hover(None);
            }
            11 => {
                self.history.redo(&mut self.doc);
                self.renderer.invalidate_annotations();
                self.set_hover(None);
            }
            12..=14 => {
                let (image, bytes) = self.renderer.export(&self.doc)?;
                if id == 12 {
                    clipboard(self.hwnd, &image, &bytes)?;
                } else {
                    let original = self.renderer.original(&self.doc)?;
                    if id == 13 {
                        let folder = self.capture_dir.clone();
                        std::fs::create_dir_all(&folder)?;
                        let key = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
                        let stem = folder.join(format!("{key}"));
                        write_capture_source(&stem, &self.doc, &original)?;
                        write_png(&stem.with_extension("png"), &image, &bytes)?;
                    } else {
                        let origin = self.renderer.export_origin(&self.doc);
                        let screen = ScreenPoint {
                            x: self.screen.left + origin.x as i32,
                            y: self.screen.top + origin.y as i32,
                        };
                        write_pin_bundle(
                            &self.capture_dir,
                            &self.doc,
                            &original,
                            image,
                            &bytes,
                            screen,
                        )?;
                    }
                }
            }
            15 => unsafe {
                PostQuitMessage(0);
            },
            // Long capture (plan B09): samples the selected region while the user
            // scrolls, stitches the frames into one tall image and opens that image
            // as the document so it can be annotated further.
            17 => {
                if let Err(error) = self.start_long_capture() {
                    snaplight_platform::native::show_error(&error.to_string());
                }
            }
            _ => {}
        }
        unsafe {
            self.invalidate();
        }
        Ok(())
    }
}
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Canvas;
    if !ptr.is_null() {
        let state = &mut *ptr;
        match msg {
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut paint);
                if let Err(e) = state.draw() {
                    state.error = Some(e.to_string());
                    PostQuitMessage(1);
                }
                let _ = EndPaint(hwnd, &paint);
                return LRESULT(0);
            }
            WM_ERASEBKGND => return LRESULT(1),
            WM_LBUTTONDOWN => {
                *state.active.lock() = hwnd.0 as usize;
                let p = state.position(l);
                state.interaction.cursor = Some(p);
                let handle = state.doc.selection.and_then(|r| {
                    selection_handle(r, p, state.dpi, state.view.scale).map(|h| (r, h))
                });
                if let Some(resize) = handle {
                    park_toolbar(state);
                    state.resize = Some(resize);
                    state.start = Some(p);
                    state.interaction.dragging = true;
                    SetCapture(hwnd);
                    state.update_drag(p);
                    state.invalidate();
                    return LRESULT(0);
                }
                if state.tool == Tool::Text {
                    if !state.text.is_empty() {
                        state.history.execute(
                            &mut state.doc,
                            Edit::Add(Annotation {
                                shape: Shape::Text {
                                    at: p,
                                    text: state.text.clone(),
                                    size: state.style.width * 4.0 + 12.0,
                                },
                                style: state.style,
                            }),
                        );
                        state.renderer.invalidate_annotations();
                        state.invalidate();
                    }
                } else {
                    if state.tool == Tool::Select {
                        park_toolbar(state);
                    }
                    SetCapture(hwnd);
                    state.start = Some(p);
                    state.interaction.dragging = true;
                    state.update_drag(p);
                    state.invalidate();
                }
                return LRESULT(0);
            }
            WM_SETCURSOR => {
                if (l.0 as u32 & 0xffff) == HTCLIENT {
                    let mut point = POINT::default();
                    if GetCursorPos(&mut point).is_ok()
                        && ScreenToClient(hwnd, &mut point).as_bool()
                    {
                        let (px, py) = state.view.to_image(point.x as f32, point.y as f32);
                        let p = ImagePoint { x: px, y: py };
                        let handle = state.resize.map(|(_, h)| h).or_else(|| {
                            state
                                .doc
                                .selection
                                .and_then(|r| selection_handle(r, p, state.dpi, state.view.scale))
                        });
                        let cursor = match handle {
                            Some(SelectionHandle { col: 1, .. }) => IDC_SIZENS,
                            Some(SelectionHandle { row: 1, .. }) => IDC_SIZEWE,
                            Some(SelectionHandle { col, row }) if col == row => IDC_SIZENWSE,
                            Some(_) => IDC_SIZENESW,
                            None => IDC_CROSS,
                        };
                        if let Ok(cursor) = LoadCursorW(None, cursor) {
                            SetCursor(cursor);
                        }
                        return LRESULT(1);
                    }
                }
            }
            WM_MOUSEMOVE => {
                let p = state.position(l);
                state.interaction.cursor = Some(p);
                state.update_drag(p);
                state.request_hover(p);
                // The preview must not outlive the pointer leaving this monitor.
                let mut track = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut track);
                state.invalidate();
                return LRESULT(0);
            }
            WM_MOUSELEAVE => {
                state.hover_probe.pending = None;
                state.hover_probe.last = None;
                state.set_hover(None);
                return LRESULT(0);
            }
            WM_MOUSEWHEEL => {
                // `l` carries screen coordinates and `w` the rotation plus the
                // modifier keys for this message.
                let delta = (w.0 >> 16) as u16 as i16 as i32;
                let flags = w.0 as u32;
                let (x, y) = (
                    l.0 as u16 as i16 as f32 - state.screen.left as f32,
                    (l.0 >> 16) as u16 as i16 as f32 - state.screen.top as f32,
                );
                let (px, py) = state.view.to_image(x, y);
                let p = ImagePoint { x: px, y: py };
                state.wheel(delta, p, flags & 0x0008 != 0, flags & 0x0004 != 0);
                return LRESULT(0);
            }
            HOVER_PROBE => {
                state.run_hover();
                return LRESULT(0);
            }
            WM_LBUTTONUP => {
                if let Some(start) = state.start {
                    let up = state.position(l);
                    state.update_drag(up);
                    let slop = CLICK_SLOP * (state.dpi as f32 / 96.0);
                    let clicked = state.resize.is_none()
                        && (up.x - start.x).abs().max((up.y - start.y).abs()) <= slop;
                    if state.tool == Tool::Select && clicked && state.detect == WindowDetect::Ui {
                        // A click adopts the detected region; without one nothing is
                        // committed, so a stray click cannot leave a one-pixel
                        // selection. With detection off the previous behaviour
                        // (a tiny hand-drawn region) is kept.
                        if let Some(region) = state.interaction.hover {
                            state.interaction.selection_draft = None;
                            if state.doc.selection != Some(region) {
                                state
                                    .history
                                    .execute(&mut state.doc, Edit::Selection(Some(region)));
                            }
                        } else {
                            state.interaction.selection_draft = None;
                        }
                    } else if let Some(r) = state.interaction.selection_draft.take() {
                        if r.width >= 1.0 && r.height >= 1.0 && state.doc.selection != Some(r) {
                            state
                                .history
                                .execute(&mut state.doc, Edit::Selection(Some(r)));
                        }
                    } else if let Some(a) = state.interaction.draft.take() {
                        state.history.execute(&mut state.doc, Edit::Add(a));
                        state.renderer.invalidate_annotations();
                    }
                }
                state.start = None;
                state.resize = None;
                state.interaction.dragging = false;
                // The preview belongs to the pointer, not to a finished gesture.
                state.hover_probe.pending = None;
                state.interaction.hover = None;
                let _ = ReleaseCapture();
                publish_history(state);
                state.invalidate();
                return LRESULT(0);
            }
            WM_CAPTURECHANGED => {
                let interrupted = state.start.is_some();
                state.start = None;
                state.resize = None;
                state.interaction.draft = None;
                state.interaction.selection_draft = None;
                state.interaction.dragging = false;
                state.interaction.hover = None;
                state.hover_probe.pending = None;
                if interrupted {
                    publish_history(state);
                }
                state.invalidate();
                return LRESULT(0);
            }
            WM_KEYDOWN => {
                let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
                let id = match w.0 as u16 {
                    0x1b => Some(15),
                    0x0d => Some(14),
                    0x5a if ctrl => Some(10),
                    0x59 if ctrl => Some(11),
                    0x43 if ctrl => Some(12),
                    0x53 if ctrl => Some(13),
                    _ => None,
                };
                if let Some(id) = id {
                    if let Err(e) = state.action(id) {
                        state.error = Some(e.to_string());
                    }
                }
                return LRESULT(0);
            }
            WM_RBUTTONUP | WM_CLOSE => {
                PostQuitMessage(0);
                return LRESULT(0);
            }
            WM_DISPLAYCHANGE => {
                // Display configuration changes invalidate device resources but not the
                // frozen base image; rebuild instead of discarding the document.
                if let Err(e) = state.rebuild_device() {
                    state.error = Some(e.to_string());
                    PostQuitMessage(1);
                } else {
                    state.invalidate();
                }
                return LRESULT(0);
            }
            WM_DPICHANGED => return LRESULT(0),
            // Long capture: the session samples the selected region on a timer
            // and is driven by the toolbar's buttons through `action`.
            WM_TIMER if w.0 == LONG_CAPTURE_TIMER => {
                state.long_capture_tick();
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, w, l)
}
/// Captures the desktop with a small, bounded number of attempts.
///
/// Capture access can transiently fail while the desktop is locked or a display is
/// reconfigured. Retrying a fixed number of times keeps the failure bounded and still
/// reports a clear error instead of retrying forever or showing stale pixels.
fn capture_with_retry() -> std::result::Result<Vec<FrozenFrame>, Box<dyn std::error::Error>> {
    const ATTEMPTS: u32 = 3;
    let mut last = None;
    for attempt in 0..ATTEMPTS {
        match DesktopCapture.capture() {
            Ok(frames) => return Ok(frames),
            Err(e) => {
                last = Some(e.to_string());
                if attempt + 1 < ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| "capture failed".into()).into())
}
/// Builds a large, deterministic annotation set for the `--stress` frame loop.
fn stress_annotations(width: u32, height: u32) -> Vec<Annotation> {
    const COUNT: usize = 1000;
    let max_x = (width as f32 - 200.0).max(1.0);
    let max_y = (height as f32 - 160.0).max(1.0);
    (0..COUNT)
        .map(|i| {
            let x = (i as f32 * 37.0) % max_x;
            let y = (i as f32 * 91.0) % max_y;
            let shape = match i % 5 {
                0 => Shape::Rectangle(ImageRect {
                    x,
                    y,
                    width: 120.0,
                    height: 60.0,
                }),
                1 => Shape::Ellipse(ImageRect {
                    x,
                    y,
                    width: 90.0,
                    height: 70.0,
                }),
                2 => Shape::Arrow {
                    from: ImagePoint { x, y },
                    to: ImagePoint {
                        x: x + 140.0,
                        y: y + 90.0,
                    },
                },
                3 => Shape::Pen(vec![
                    ImagePoint { x, y },
                    ImagePoint {
                        x: x + 30.0,
                        y: y + 20.0,
                    },
                    ImagePoint {
                        x: x + 60.0,
                        y: y + 5.0,
                    },
                ]),
                _ => Shape::Mosaic {
                    rect: ImageRect {
                        x,
                        y,
                        width: 128.0,
                        height: 96.0,
                    },
                    block: 12,
                },
            };
            Annotation {
                shape,
                style: Style::default(),
            }
        })
        .collect()
}
fn canvas_thread(
    config: Config,
    queue: Arc<Mutex<Vec<UiCommand>>>,
    ready: std::sync::mpsc::Sender<std::result::Result<CaptureReady, String>>,
    ui_slots: Arc<UiSlots>,
    smoke: bool,
    stress: bool,
    demo: Option<RECT>,
) -> std::result::Result<(), String> {
    let result = (|| -> std::result::Result<(), Box<dyn std::error::Error>> {
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
            let capture_started = std::time::Instant::now();
            let mut frames = capture_with_retry()?;
            let capture_us = capture_started.elapsed().as_micros() as u64;
            if smoke {
                let width = 640;
                let height = 400;
                let mut pixels = vec![0; 640 * 400 * 4];
                for y in 0..height {
                    for x in 0..width {
                        let n = (y * width + x) as usize * 4;
                        pixels[n..n + 4].copy_from_slice(&[
                            ((x * 3 + y * 5) % 256) as u8,
                            ((x + y * 2) % 256) as u8,
                            ((x * 2 + y) % 256) as u8,
                            255,
                        ]);
                    }
                }
                let image = ImageDescriptor {
                    id: ImageId(999),
                    width,
                    height,
                    stride: width * 4,
                    format: PixelFormat::Bgra8,
                    alpha: AlphaMode::Opaque,
                    color_space: ColorSpace::Srgb,
                };
                frames.push(Gpu::new(None)?.frame_from_bgra(
                    image,
                    &pixels,
                    RECT {
                        left: 0,
                        top: 0,
                        right: 640,
                        bottom: 400,
                    },
                )?);
            }
            let instance = GetModuleHandleW(None)?;
            let class = w!("Snaplight.Overlay");
            // The window-class icon covers the frames this process draws, which
            // is what Alt-Tab peeks at. `WNDCLASSW` has no separate small icon;
            // the system derives it from this one.
            let class_icon = snaplight_platform::native::app_icon(false).unwrap_or_default();
            let wc = WNDCLASSW {
                hInstance: instance.into(),
                lpszClassName: class,
                lpfnWndProc: Some(wndproc),
                hCursor: LoadCursorW(None, IDC_CROSS)?,
                hIcon: class_icon,
                ..Default::default()
            };
            if RegisterClassW(&wc) == 0 {
                return Err(Error::from_win32().into());
            }
            let active = Arc::new(Mutex::new(0usize));
            let mut canvases = Vec::new();
            // The toolbar is placed against the captured desktop bounds, so
            // compute them before the frames are consumed.
            let bounds = frames.iter().fold(
                RECT {
                    left: i32::MAX,
                    top: i32::MAX,
                    right: i32::MIN,
                    bottom: i32::MIN,
                },
                |acc, frame| RECT {
                    left: acc.left.min(frame.screen.left),
                    top: acc.top.min(frame.screen.top),
                    right: acc.right.max(frame.screen.right),
                    bottom: acc.bottom.max(frame.screen.bottom),
                },
            );
            let mut dpi = 96u32;
            for frame in frames {
                let s = frame.screen;
                let hwnd = CreateWindowExW(
                    WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOREDIRECTIONBITMAP,
                    class,
                    w!("Snaplight Capture"),
                    WS_POPUP,
                    s.left,
                    s.top,
                    s.right - s.left,
                    s.bottom - s.top,
                    None,
                    None,
                    instance,
                    None,
                )?;
                let renderer = match OverlayRenderer::new(hwnd, &frame) {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = DestroyWindow(hwnd);
                        return Err(e.into());
                    }
                };
                let doc = Document {
                    version: 1,
                    image: frame.descriptor.clone(),
                    selection: None,
                    annotations: vec![],
                };
                // Retain the immutable base image (BGRA8, `doc.image.stride`) so a lost
                // device can be rebuilt without re-capturing or losing the document.
                let base = renderer.original(&doc)?;
                let window_dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd);
                if window_dpi > 0 {
                    dpi = window_dpi;
                }
                let mut canvas = Box::new(Canvas {
                    hwnd,
                    renderer,
                    doc,
                    history: History::default(),
                    interaction: Interaction::default(),
                    tool: Tool::Select,
                    style: Style {
                        width: config.stroke_width,
                        ..Style::default()
                    },
                    block: config.mosaic_block,
                    text: String::new(),
                    start: None,
                    resize: None,
                    active: active.clone(),
                    error: None,
                    base,
                    screen: s,
                    view: ViewTransform::plain(),
                    device_rebuilds: 0,
                    capture_dir: capture_dir(&config),
                    ui_slots: ui_slots.clone(),
                    dpi: window_dpi.max(96),
                    detect: config.window_detect,
                    hover_probe: HoverProbe::default(),
                    long_capture: None,
                    demo: demo.is_some(),
                });
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&mut *canvas as *mut Canvas) as isize);
                if *active.lock() == 0 {
                    *active.lock() = hwnd.0 as usize;
                }
                if smoke {
                    // A captured frame that carries no desktop image (an empty
                    // duplication surface, seen as all black) is invisible to the
                    // synthetic-frame checks below, so compare the real frame with
                    // an independent GDI capture of the same screen.
                    let mut agreement: Option<f32> = None;
                    if !stress && frame.descriptor.id != ImageId(999) {
                        if let Some(value) =
                            snaplight_render_win::capture_agreement(&frame, &canvas.base)?
                        {
                            agreement = Some(value);
                            if value < 0.8 {
                                return Err(format!(
                                    "captured frame agrees with an independent GDI capture on only {:.0}% of samples",
                                    value * 100.0
                                )
                                .into());
                            }
                        }
                    }
                    canvas.renderer.enable_profiling();
                    canvas.doc.selection = Some(ImageRect {
                        x: 20.0,
                        y: 20.0,
                        width: frame.descriptor.width.min(640).saturating_sub(20) as f32,
                        height: frame.descriptor.height.min(400).saturating_sub(20) as f32,
                    });
                    canvas.doc.annotations = if stress {
                        stress_annotations(frame.descriptor.width, frame.descriptor.height)
                    } else {
                        vec![
                            Annotation {
                                shape: Shape::Rectangle(ImageRect {
                                    x: 40.0,
                                    y: 40.0,
                                    width: 200.0,
                                    height: 100.0,
                                }),
                                style: Style::default(),
                            },
                            Annotation {
                                shape: Shape::Mosaic {
                                    rect: ImageRect {
                                        x: 60.0,
                                        y: 180.0,
                                        width: 180.0,
                                        height: 80.0,
                                    },
                                    block: 12,
                                },
                                style: Style::default(),
                            },
                            Annotation {
                                shape: Shape::Text {
                                    at: ImagePoint { x: 280.0, y: 60.0 },
                                    text: "Snaplight GPU smoke".into(),
                                    size: 24.0,
                                },
                                style: Style::default(),
                            },
                        ]
                    };
                    canvas.renderer.record_composed(!stress);
                    canvas.renderer.render(
                        &canvas.doc,
                        Viewport::plain(
                            frame.descriptor.width,
                            frame.descriptor.height,
                            canvas.dpi as f32,
                            ScreenPoint {
                                x: canvas.screen.left,
                                y: canvas.screen.top,
                            },
                        ),
                        &Interaction::default(),
                    )?;
                    if !stress {
                        // The selection chrome is drawn into the composed frame only, so
                        // check it before the export strips it back down to the selected
                        // pixels. The same check runs for the synthetic frame and for a
                        // real capture.
                        let composed = canvas
                            .renderer
                            .composed()
                            .ok_or("the overlay did not record a composed frame")?;
                        let pixel = |x: usize, y: usize| -> [u8; 4] {
                            let n = y * composed.width as usize * 4 + x * 4;
                            [
                                composed.pixels[n],
                                composed.pixels[n + 1],
                                composed.pixels[n + 2],
                                composed.pixels[n + 3],
                            ]
                        };
                        let captured = |x: usize, y: usize| -> [u8; 4] {
                            let n = y * canvas.doc.image.stride as usize + x * 4;
                            [
                                canvas.base[n],
                                canvas.base[n + 1],
                                canvas.base[n + 2],
                                canvas.base[n + 3],
                            ]
                        };
                        let within = |got: [u8; 4], want: [u8; 4], tolerance: i32| {
                            (0..3).all(|i| (got[i] as i32 - want[i] as i32).abs() <= tolerance)
                        };
                        // BGRA8 render target: blue outline, blue handle fill, dark chip.
                        for (x, y, want, what) in [
                            (400usize, 20usize, [255u8, 136, 51, 255], "outline"),
                            (330, 20, [255, 136, 51, 255], "top handle"),
                            (20, 210, [255, 136, 51, 255], "side handle"),
                            (26, 20, [56, 49, 40, 255], "size chip"),
                        ] {
                            let got = pixel(x, y);
                            if !within(got, want, 24) {
                                return Err(format!(
                                    "selection {what} at {x},{y} is {got:?}, expected {want:?}"
                                )
                                .into());
                            }
                        }
                        // Selected pixels keep the captured value.
                        let inside = pixel(350, 100);
                        let wanted = captured(350, 100);
                        if !within(inside, wanted, 6) {
                            return Err(format!(
                                "selected pixel is {inside:?}, expected the undimmed capture {wanted:?}"
                            )
                            .into());
                        }
                        // Everything outside the selection is dimmed by the mask, so a
                        // bright enough capture pixel must come back at about 55%.
                        let outside = pixel(500, 12);
                        let reference = captured(500, 12);
                        if reference[..3].iter().copied().max().unwrap_or(0) > 40 {
                            for i in 0..3 {
                                let want = reference[i] as f32 * 0.55;
                                if (outside[i] as f32 - want).abs() > 8.0 {
                                    return Err(format!(
                                        "pixel outside the selection is {outside:?}, expected about 55% of {reference:?}"
                                    )
                                    .into());
                                }
                            }
                        }
                        // The preview for a detected region is an outline without
                        // handles, so a click is distinguishable from a committed
                        // selection. It is checked on its own frame, with no
                        // selection in the document: everything outside the
                        // preview is dimmed, and its corner stays blue instead of
                        // carrying a handle.
                        let hover = ImageRect {
                            x: 60.0,
                            y: 300.0,
                            width: 140.0,
                            height: 80.0,
                        };
                        let selection = canvas.doc.selection.take();
                        canvas.renderer.record_composed(true);
                        canvas.renderer.render(
                            &canvas.doc,
                            Viewport::plain(
                                frame.descriptor.width,
                                frame.descriptor.height,
                                canvas.dpi as f32,
                                ScreenPoint {
                                    x: canvas.screen.left,
                                    y: canvas.screen.top,
                                },
                            ),
                            &Interaction {
                                hover: Some(hover),
                                ..Default::default()
                            },
                        )?;
                        let preview = canvas
                            .renderer
                            .composed()
                            .ok_or("the overlay did not record the preview frame")?;
                        let preview_pixel = |x: usize, y: usize| -> [u8; 4] {
                            let n = y * preview.width as usize * 4 + x * 4;
                            [
                                preview.pixels[n],
                                preview.pixels[n + 1],
                                preview.pixels[n + 2],
                                preview.pixels[n + 3],
                            ]
                        };
                        let corner = preview_pixel(60, 300);
                        if !within(corner, [255, 136, 51, 255], 24) {
                            return Err(format!(
                                "preview corner is {corner:?}, expected the outline colour without a handle"
                            )
                            .into());
                        }
                        let inside = preview_pixel(120, 340);
                        let wanted = captured(120, 340);
                        if !within(inside, wanted, 6) {
                            return Err(format!(
                                "previewed pixel is {inside:?}, expected the undimmed capture {wanted:?}"
                            )
                            .into());
                        }
                        let outside = preview_pixel(400, 340);
                        let reference = captured(400, 340);
                        if reference[..3].iter().copied().max().unwrap_or(0) > 40 {
                            for i in 0..3 {
                                let want = reference[i] as f32 * 0.55;
                                if (outside[i] as f32 - want).abs() > 8.0 {
                                    return Err(format!(
                                        "pixel outside the preview is {outside:?}, expected about 55% of {reference:?}"
                                    )
                                    .into());
                                }
                            }
                        }
                        canvas.doc.selection = selection;
                        canvas.renderer.record_composed(false);
                    }
                    let (image, bytes) = canvas.renderer.export(&canvas.doc)?;
                    if image.id == ImageId(999) && !stress {
                        if bytes[..4] != [160, 60, 60, 255] {
                            return Err("GPU source/crop pixel check failed".into());
                        }
                        let at = |x: usize, y: usize| -> &[u8] {
                            let n = y * image.stride as usize + x * 4;
                            &bytes[n..n + 4]
                        };
                        if at(42, 162) != at(45, 165) {
                            return Err("mosaic block anchoring check failed".into());
                        }
                    }
                    canvas.renderer.collect_gpu_samples();
                    // Device-loss recovery: rebuild every GPU resource from the retained
                    // base image and verify the document and rendered output survive.
                    canvas.rebuild_device()?;
                    let rebuilt = canvas.renderer.export(&canvas.doc)?.1;
                    if rebuilt != bytes {
                        return Err("device rebuild changed the rendered output".into());
                    }
                    if stress {
                        // Sustained 4K frame loop over the full annotation set, with the
                        // magnifier and a long live stroke redrawn every frame. The
                        // resulting arrays carry per-frame CPU submission, blocking
                        // Present and GPU timestamp durations for the stress report.
                        let mut live = Interaction {
                            cursor: Some(ImagePoint { x: 400.0, y: 300.0 }),
                            dragging: true,
                            draft: Some(Annotation {
                                shape: Shape::Pen(
                                    (0..600)
                                        .map(|i| ImagePoint {
                                            x: 120.0 + i as f32 * 5.0,
                                            y: 1200.0 + (i as f32 * 0.08).sin() * 220.0,
                                        })
                                        .collect(),
                                ),
                                style: Style::default(),
                            }),
                            ..Default::default()
                        };
                        for frame_index in 0..STRESS_FRAMES {
                            // Move the magnifier so its source rect changes every frame.
                            live.cursor = Some(ImagePoint {
                                x: 400.0 + (frame_index as f32 * 7.0) % 2600.0,
                                y: 300.0 + (frame_index as f32 * 3.0) % 1400.0,
                            });
                            canvas.renderer.render(
                                &canvas.doc,
                                Viewport::plain(
                                    frame.descriptor.width,
                                    frame.descriptor.height,
                                    canvas.dpi as f32,
                                    ScreenPoint {
                                        x: canvas.screen.left,
                                        y: canvas.screen.top,
                                    },
                                ),
                                &live,
                            )?;
                            canvas.renderer.collect_gpu_samples();
                        }
                    }
                    let p = data_dir().join(format!("smoke-{}.png", image.id.0));
                    std::fs::create_dir_all(data_dir())?;
                    write_png(&p, &image, &bytes)?;
                    let telemetry = serde_json::json!({"width":frame.descriptor.width,"height":frame.descriptor.height,"capture_fallback":frame.fallback,"capture_agreement":agreement,"software_gpu":frame.gpu.software,"capture_us":capture_us,"frames":canvas.renderer.frames,"device_rebuilds":canvas.device_rebuilds,"cpu_us":canvas.renderer.cpu_us,"present_wait_us":canvas.renderer.present_us,"gpu_us":canvas.renderer.gpu_us,"window_dpi":windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd),"system_dpi":windows::Win32::UI::HiDpi::GetDpiForSystem()});
                    atomic_write(
                        &data_dir().join(format!("smoke-{}.json", image.id.0)),
                        &serde_json::to_vec_pretty(&telemetry)?,
                    )?;
                } else {
                    let _ = ShowWindow(hwnd, SW_SHOW);
                    canvas.invalidate();
                    // Show the region under the pointer right away instead of
                    // waiting for the first mouse move.
                    let mut cursor = POINT::default();
                    if canvas.detect == WindowDetect::Ui && GetCursorPos(&mut cursor).is_ok() {
                        canvas.request_hover(ImagePoint {
                            x: (cursor.x - s.left) as f32,
                            y: (cursor.y - s.top) as f32,
                        });
                    }
                }
                canvases.push(canvas);
            }
            let thread_id = GetCurrentThreadId();
            let _ = ready.send(Ok(CaptureReady {
                thread_id,
                bounds,
                dpi: dpi.max(96),
            }));
            if let Some(region) = demo {
                let canvas = canvases
                    .iter_mut()
                    .find(|c| {
                        region.left >= c.screen.left
                            && region.top >= c.screen.top
                            && region.right <= c.screen.right
                            && region.bottom <= c.screen.bottom
                    })
                    .ok_or("the demo rectangle is not inside any captured screen")?;
                canvas.doc.selection = Some(ImageRect {
                    x: (region.left - canvas.screen.left) as f32,
                    y: (region.top - canvas.screen.top) as f32,
                    width: (region.right - region.left) as f32,
                    height: (region.bottom - region.top) as f32,
                });
                match canvas.start_long_capture() {
                    Ok(()) => {}
                    Err(error) => return Err(format!("long capture failed: {error}").into()),
                }
            }
            // The demo needs the message loop too: the session advances on a timer
            // and finishes when the driver sends Enter.
            if !smoke || demo.is_some() {
                let mut msg = MSG::default();
                loop {
                    let result = GetMessageW(&mut msg, None, 0, 0).0;
                    if result == -1 {
                        return Err(Error::from_win32().into());
                    }
                    if result == 0 {
                        break;
                    }
                    if msg.message == COMMAND {
                        let commands = std::mem::take(&mut *queue.lock());
                        for command in commands {
                            match command {
                                UiCommand::Action(id) => {
                                    // A running long capture owns the toolbar:
                                    // its canvas maps the action buttons onto the
                                    // session and ignores everything else.
                                    if let Some(c) =
                                        canvases.iter_mut().find(|c| c.long_capture.is_some())
                                    {
                                        if let Err(e) = c.action(id) {
                                            snaplight_platform::native::show_error(&e.to_string());
                                        }
                                    } else if id <= 6 {
                                        for c in &mut canvases {
                                            let _ = c.action(id);
                                        }
                                    } else if let Some(c) = canvases
                                        .iter_mut()
                                        .find(|c| c.hwnd.0 as usize == *active.lock())
                                    {
                                        if let Err(e) = c.action(id) {
                                            snaplight_platform::native::show_error(&e.to_string());
                                        }
                                    }
                                }
                                UiCommand::Properties {
                                    width,
                                    block,
                                    text,
                                    swatch,
                                } => {
                                    let colors = [
                                        [0.95, 0.2, 0.25, 1.0],
                                        [0.2, 0.79, 0.63, 1.0],
                                        [1.0, 0.79, 0.29, 1.0],
                                        [1.0, 1.0, 1.0, 1.0],
                                    ];
                                    for c in &mut canvases {
                                        c.style.width = width.clamp(1, 64) as f32;
                                        c.style.rgba = colors[swatch.clamp(0, 3) as usize];
                                        c.block = block.clamp(2, 128) as u32;
                                        c.text = text.clone();
                                    }
                                }
                            }
                        }
                        // Undo and redo availability follows the edit history.
                        if let Some(c) = canvases
                            .iter()
                            .find(|c| c.hwnd.0 as usize == *active.lock())
                        {
                            publish_history(c);
                        }
                    } else {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            }
            let error = canvases.iter().find_map(|c| c.error.clone());
            for canvas in &canvases {
                SetWindowLongPtrW(canvas.hwnd, GWLP_USERDATA, 0);
                DestroyWindow(canvas.hwnd)?;
            }
            drop(canvases);
            CoUninitialize();
            if let Some(e) = error {
                return Err(e.into());
            }
            Ok(())
        }
    })();
    result.map_err(|e| e.to_string())
}
fn write_png(
    path: &std::path::Path,
    image: &ImageDescriptor,
    bytes: &[u8],
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut rgba = bytes.to_vec();
    for p in rgba.as_chunks_mut::<4>().0.iter_mut() {
        p.swap(0, 2);
        p[3] = 255;
    }
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(file, image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&rgba)?;
    Ok(())
}
/// Writes a Slint window snapshot, which is already RGBA8.
fn write_rgba_png(
    path: &std::path::Path,
    width: u32,
    height: u32,
    bytes: &[u8],
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(bytes)?;
    Ok(())
}
fn clipboard(hwnd: HWND, image: &ImageDescriptor, bytes: &[u8]) -> Result<()> {
    unsafe {
        OpenClipboard(hwnd)?;
        let result = (|| {
            EmptyClipboard()?;
            let size = std::mem::size_of::<BITMAPINFOHEADER>();
            let memory = GlobalAlloc(GMEM_MOVEABLE, size + bytes.len())?;
            let ptr = GlobalLock(memory);
            if ptr.is_null() {
                let _ = GlobalFree(memory);
                return Err(Error::from_win32());
            }
            let header = BITMAPINFOHEADER {
                biSize: size as u32,
                biWidth: image.width as i32,
                biHeight: -(image.height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: bytes.len() as u32,
                ..Default::default()
            };
            std::ptr::copy_nonoverlapping(
                (&header as *const BITMAPINFOHEADER).cast::<u8>(),
                ptr.cast(),
                size,
            );
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>().add(size), bytes.len());
            let _ = GlobalUnlock(memory);
            if let Err(e) = SetClipboardData(8, HANDLE(memory.0)) {
                let _ = GlobalFree(memory);
                return Err(e);
            }
            Ok(())
        })();
        let closed = CloseClipboard();
        result?;
        closed
    }
}
/// Writes the two files every saved capture shares: the pristine pixels and the
/// document that describes them.
fn write_capture_source(
    stem: &std::path::Path,
    document: &Document,
    original: &[u8],
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    write_pixels(
        &stem.with_extension("source.bgra"),
        &document.image,
        original,
    )?;
    atomic_write(
        &stem.with_extension("document.json"),
        &serde_json::to_vec_pretty(document)?,
    )?;
    Ok(())
}

/// Writes a pin bundle into `folder` — the pristine pixels, the document, the
/// flattened image and the pin document — and hands the pin to the pins process,
/// which takes over and quits this editor. A normal capture and a finished long
/// capture both go through here, so the two leave the same library behind.
fn write_pin_bundle(
    folder: &std::path::Path,
    document: &Document,
    original: &[u8],
    image: ImageDescriptor,
    bytes: &[u8],
    screen: ScreenPoint,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(folder)?;
    let key = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let stem = folder.join(format!("{key}"));
    write_capture_source(&stem, document, original)?;
    let path = stem.with_extension("bgra");
    write_pixels(&path, &image, bytes)?;
    let flattened = Document {
        version: 1,
        image,
        selection: None,
        annotations: vec![],
    };
    let doc_path = stem.with_extension("pin.json");
    atomic_write(&doc_path, &serde_json::to_vec(&flattened)?)?;
    println!(
        "{}",
        serde_json::to_string(&Envelope::new(PinCommand::Open {
            image: path,
            document: doc_path,
            screen,
        }))?
    );
    std::io::stdout().flush()?;
    unsafe {
        PostQuitMessage(0);
    }
    Ok(())
}

/// Toolbar and tool options handles shared with the canvas thread, which moves
/// the toolbar after a selection and closes options that no longer belong to it.
#[derive(Default)]
struct UiSlots {
    toolbar: OnceLock<snaplight_ui::ToolbarWeak>,
    panel: OnceLock<snaplight_ui::PanelWeak>,
    toolbar_size: OnceLock<(u32, u32)>,
    toolbar_selection: Mutex<Option<ImageRect>>,
    /// Physical screen rectangle of the placed toolbar, so a long capture can
    /// keep its own pill out of the region it stitches.
    toolbar_rect: Mutex<Option<RECT>>,
}
/// Captured desktop bounds, monitor DPI, and the canvas thread id.
#[derive(Clone, Copy)]
struct CaptureReady {
    thread_id: u32,
    bounds: RECT,
    dpi: u32,
}
/// Places the tool options next to the toolbar: above it when the captured
/// desktop has room, otherwise below, always clamped to the capture.
fn options_position(
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
fn find_process_window(title: &str) -> Option<HWND> {
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
fn apply_icon_when_ready(title: &'static str) {
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
/// pill floats over the capture instead of looking like a document window. The
/// window is created asynchronously by Slint, so poll for it in the background.
fn round_pill_window(title: &'static str, logical_size: (u32, u32)) {
    let radius = if title == snaplight_ui::TOOLBAR_TITLE {
        10.0
    } else {
        logical_size.1 as f32 / 2.0
    };
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
/// True when `hwnd` already carries a window region, so the pill shape was
/// applied before.
unsafe fn has_region(hwnd: HWND) -> bool {
    let probe = CreateRectRgn(0, 0, 1, 1);
    let kind = GetWindowRgn(hwnd, probe);
    let _ = DeleteObject(HGDIOBJ(probe.0));
    // ERROR (0) means the window has no region at all.
    kind.0 != 0
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
/// Re-applies the pill shape and raises a Slint window that was just shown.
/// Showing an already composited window brings the system title bar back, so the
/// frame has to be dropped again after every show, not only at creation.
fn reshape_and_raise(title: &'static str, logical_size: (u32, u32)) {
    match find_process_window(title) {
        Some(hwnd) => unsafe {
            round_window(
                hwnd,
                if title == snaplight_ui::TOOLBAR_TITLE {
                    10.0
                } else {
                    logical_size.1 as f32 / 2.0
                },
            );
            raise_window(hwnd);
        },
        None => round_pill_window(title, logical_size),
    }
}
const TOOL_WINDOW_SUBCLASS: usize = 0x534c5442;
fn tool_window_style(style: isize) -> isize {
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
    // The region survives a hide/show cycle, so it only has to be built once;
    // the frame change below has to run again after every show.
    if GetWindowRect(hwnd, &mut rect).is_ok() && !has_region(hwnd) {
        let diameter = ((radius * dpi as f32 / 96.0).round() as i32 * 2).max(2);
        let region = CreateRoundRectRgn(
            0,
            0,
            rect.right - rect.left + 1,
            rect.bottom - rect.top + 1,
            diameter,
            diameter,
        );
        // A successful SetWindowRgn hands the region over to the system.
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
fn parked_position(size: (i32, i32)) -> (i32, i32) {
    let left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    (left - size.0 - 16, top - size.1 - 16)
}
fn initial_toolbar_position(bounds: RECT, size: (i32, i32)) -> (i32, i32) {
    const MARGIN: i32 = 16;
    let x = bounds.left + MARGIN;
    let y = (bounds.bottom - size.1 - MARGIN).max(bounds.top + MARGIN);
    (x, y)
}
/// Places the toolbar next to a screen-space rectangle, below it when there is
/// room and above it otherwise, clamped to the captured screen.
fn follow_position(screen: RECT, anchor: RECT, size: (i32, i32)) -> (i32, i32) {
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
/// Reflects the active document's history in the toolbar, so undo and redo only
/// look available when there is something to reverse.
fn publish_history(canvas: &Canvas) {
    let Some(weak) = canvas.ui_slots.toolbar.get() else {
        return;
    };
    snaplight_ui::set_toolbar_history(weak, canvas.history.can_undo(), canvas.history.can_redo());
    // The toolbar acts on the selection, so it only exists while there is one:
    // undoing the selection brings the capture back to the empty state.
    if let Some(selection) = canvas.doc.selection {
        publish_selection(canvas, selection);
    } else {
        park_toolbar(canvas);
    }
}
fn park_toolbar(canvas: &Canvas) {
    *canvas.ui_slots.toolbar_selection.lock() = None;
    // Parked windows sit outside the virtual screen, so they cannot sit over a
    // region either.
    *canvas.ui_slots.toolbar_rect.lock() = None;
    if let Some(weak) = canvas.ui_slots.toolbar.get() {
        let size = snaplight_ui::scale_to_physical(
            *canvas.ui_slots.toolbar_size.get().unwrap(),
            canvas.dpi,
        );
        let (x, y) = parked_position(size);
        snaplight_ui::move_toolbar(weak, PhysicalPosition::new(x, y));
    }
    if let Some(panel) = canvas.ui_slots.panel.get() {
        snaplight_ui::hide_panel(panel);
    }
}
/// Moves the toolbar under the completed selection and reports its pixel size.
fn publish_selection(canvas: &Canvas, selection: ImageRect) {
    let mut displayed = canvas.ui_slots.toolbar_selection.lock();
    if *displayed == Some(selection) {
        return;
    }
    *displayed = Some(selection);
    drop(displayed);
    // Quick settings belong to the previous toolbar position.
    if let Some(panel) = canvas.ui_slots.panel.get() {
        snaplight_ui::hide_panel(panel);
    }
    let Some(weak) = canvas.ui_slots.toolbar.get() else {
        return;
    };
    let anchor = RECT {
        left: canvas.screen.left + selection.x as i32,
        top: canvas.screen.top + selection.y as i32,
        right: canvas.screen.left + (selection.x + selection.width) as i32,
        bottom: canvas.screen.top + (selection.y + selection.height) as i32,
    };
    let size =
        snaplight_ui::scale_to_physical(*canvas.ui_slots.toolbar_size.get().unwrap(), canvas.dpi);
    let (x, y) = follow_position(canvas.screen, anchor, size);
    *canvas.ui_slots.toolbar_rect.lock() = Some(RECT {
        left: x,
        top: y,
        right: x + size.0,
        bottom: y + size.1,
    });
    snaplight_ui::set_toolbar_status(
        weak,
        format!(
            "{} × {}",
            selection.width.round() as u32,
            selection.height.round() as u32
        ),
    );
    // The bar belongs to a finished selection: place it next to the selection and
    // put it back above the overlay, which took the mouse - and with it the top of
    // the topmost band - while the selection was dragged.
    snaplight_ui::move_toolbar(weak, PhysicalPosition::new(x, y));
    let logical_size = *canvas.ui_slots.toolbar_size.get().unwrap();
    snaplight_ui::invoke(move || reshape_and_raise(snaplight_ui::TOOLBAR_TITLE, logical_size));
}
/// Directory the settings page acts on: the typed folder, or the default one
/// when the field is empty.
fn capture_dir_from_field(value: &str) -> PathBuf {
    let value = value.trim();
    if value.is_empty() {
        data_dir().join("captures")
    } else {
        PathBuf::from(value)
    }
}
/// Builds the capture list rows: local time, saved kind, and total size.
fn capture_items(dir: &std::path::Path) -> Vec<snaplight_ui::CaptureItem> {
    snaplight_platform::list_captures(dir)
        .into_iter()
        .map(|entry| snaplight_ui::CaptureItem {
            label: format!(
                "{} · {} · {}",
                entry
                    .modified
                    .map(snaplight_platform::native::format_local_time)
                    .unwrap_or_else(|| entry.id.clone()),
                if entry.png { "PNG" } else { "BGRA" },
                human_bytes(entry.bytes)
            )
            .into(),
            id: entry.id.into(),
        })
        .collect()
}
fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if value >= KIB * KIB {
        format!("{:.1} MiB", value / (KIB * KIB))
    } else if value >= KIB {
        format!("{:.0} KiB", value / KIB)
    } else {
        format!("{bytes} B")
    }
}
/// Opens a file or folder with the shell's default handler.
fn shell_open(path: &std::path::Path) -> windows::core::Result<()> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    unsafe {
        let result = ShellExecuteW(
            None,
            w!("open"),
            &HSTRING::from(path.as_os_str()),
            None,
            None,
            SW_SHOWNORMAL,
        );
        // ShellExecute returns a value greater than 32 on success.
        if result.0 as usize <= 32 {
            return Err(Error::from_win32());
        }
    }
    Ok(())
}
/// Restores a saved window position, in screen physical coordinates.
fn restore_position(window: &snaplight_ui::Window, saved: Option<ScreenPoint>) {
    if let Some(p) = saved {
        window.set_position(PhysicalPosition::new(p.x, p.y));
    }
}
/// Persists a window position without disturbing the other window's entry.
fn remember_position(settings_window: bool, window: &snaplight_ui::Window) {
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
/// Folder picker for the capture directory. Returns `None` when the user
/// cancels or the shell dialog is unavailable.
fn pick_folder() -> Option<PathBuf> {
    use windows::Win32::{
        System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER},
        UI::Shell::{
            FileOpenDialog, IFileOpenDialog, FILEOPENDIALOGOPTIONS, FOS_FORCEFILESYSTEM,
            FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
        },
    };
    unsafe {
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        let options = FILEOPENDIALOGOPTIONS(options.0 | FOS_PICKFOLDERS.0 | FOS_FORCEFILESYSTEM.0);
        dialog.SetOptions(options).ok()?;
        // Without an owner the dialog is not modal to the Slint window, but the
        // winit backend does not expose its HWND.
        dialog.Show(HWND::default()).ok()?;
        let item = dialog.GetResult().ok()?;
        let wide = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = wide.to_string().ok()?;
        CoTaskMemFree(Some(wide.0 as *const std::ffi::c_void));
        Some(PathBuf::from(path))
    }
}
fn settings(config: Config) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let ui = snaplight_ui::settings(&config)?;
    restore_position(ui.window(), load_window_state().settings);
    ui.set_capture_dir_value(capture_dir(&config).to_string_lossy().into_owned().into());
    ui.set_config_path(
        snaplight_platform::data_dir()
            .join("config.json")
            .to_string_lossy()
            .into_owned()
            .into(),
    );
    ui.set_data_dir(
        snaplight_platform::data_dir()
            .to_string_lossy()
            .into_owned()
            .into(),
    );
    ui.set_captures(ModelRc::new(VecModel::from(capture_items(
        &capture_dir_from_field(&ui.get_capture_dir_value()),
    ))));
    let weak = ui.as_weak();
    ui.on_open_data_dir(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let folder = snaplight_platform::data_dir();
        let _ = std::fs::create_dir_all(&folder);
        if let Err(e) = shell_open(&folder) {
            ui.set_status(e.to_string().into());
        }
    });
    let weak = ui.as_weak();
    ui.on_open_capture_folder(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let folder = capture_dir_from_field(&ui.get_capture_dir_value());
        let _ = std::fs::create_dir_all(&folder);
        if let Err(e) = shell_open(&folder) {
            ui.set_status(e.to_string().into());
        }
    });
    let weak = ui.as_weak();
    ui.on_open_capture(move |id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let folder = capture_dir_from_field(&ui.get_capture_dir_value());
        let png = folder.join(format!("{id}.png"));
        let path = if png.exists() {
            png
        } else {
            folder.join(format!("{id}.bgra"))
        };
        if let Err(e) = shell_open(&path) {
            ui.set_status(e.to_string().into());
        }
    });
    let weak = ui.as_weak();
    ui.on_delete_capture(move |id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        // Deleting needs a second click, so a stray click cannot remove a capture.
        if ui.get_pending_delete().as_str() != id.as_str() {
            ui.set_pending_delete(id.clone());
            ui.set_status(format!("再次点击「确认删除」可把 {id} 移入回收站").into());
            return;
        }
        ui.set_pending_delete(Default::default());
        let folder = capture_dir_from_field(&ui.get_capture_dir_value());
        let files = snaplight_platform::capture_files(&folder, &id);
        let removed = files.len();
        match snaplight_platform::native::delete_to_recycle_bin(&files) {
            Ok(()) => {
                ui.set_captures(ModelRc::new(VecModel::from(capture_items(&folder))));
                ui.set_status(format!("已把 {removed} 个文件移入回收站").into());
            }
            Err(e) => ui.set_status(e.to_string().into()),
        }
    });
    let weak = ui.as_weak();
    ui.on_move_tool(move |index, delta| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if let Some(items) = snaplight_ui::move_tool(&ui.get_tools(), index, delta) {
            ui.set_tools(ModelRc::new(VecModel::from(items)));
        }
    });
    let weak = ui.as_weak();
    ui.on_browse_capture_dir(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if let Some(folder) = pick_folder() {
            ui.set_capture_dir_value(folder.to_string_lossy().into_owned().into());
        }
    });
    let weak = ui.as_weak();
    ui.on_save(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let mut c = config.clone();
        c.stroke_width = ui.get_stroke() as f32;
        c.mosaic_block = ui.get_block() as u32;
        c.gpu_pins = ui.get_gpu_pins();
        c.theme = Theme {
            background: ui.get_background_value().into(),
            foreground: ui.get_foreground_value().into(),
            accent: ui.get_accent_value().into(),
        };
        snaplight_ui::apply_tool_order(&mut c, &ui.get_tools());
        c.window_detect = snaplight_ui::window_detect_from_index(ui.get_detect_mode());
        // Normalize and validate the shortcuts before anything is written.
        let hotkeys = (|| -> std::result::Result<(), &'static str> {
            let capture = parse_hotkey(ui.get_capture_hotkey_value().trim())
                .map_err(|_| "截图快捷键格式无效")?
                .canonical();
            let restore = parse_hotkey(ui.get_restore_hotkey_value().trim())
                .map_err(|_| "恢复贴图快捷键格式无效")?
                .canonical();
            // The held shortcut is not registered with the shell, so it accepts
            // a lone modifier or a lone key as well.
            let through = parse_held_shortcut(ui.get_through_hotkey_value().trim())
                .map_err(|_| "穿透临时交互按键格式无效")?
                .ok_or("穿透临时交互按键不能为空")?
                .canonical();
            ui.set_capture_hotkey_value(capture.clone().into());
            ui.set_restore_hotkey_value(restore.clone().into());
            ui.set_through_hotkey_value(through.clone().into());
            c.capture_hotkey = capture;
            c.restore_hotkey = restore;
            c.through_hotkey = through;
            Ok(())
        })();
        if let Err(message) = hotkeys {
            ui.set_status(format!("{message}，未保存任何更改").into());
            return;
        }
        let folder = ui.get_capture_dir_value().trim().to_string();
        c.capture_dir = (!folder.is_empty()).then(|| folder.clone());
        let saved = (|| -> std::result::Result<(), Box<dyn std::error::Error>> {
            if !folder.is_empty() {
                // Fail loudly instead of saving a directory that cannot be created.
                std::fs::create_dir_all(&folder)?;
            }
            save_config(&c)?;
            Ok(())
        })();
        match saved {
            Ok(()) => {
                // The list follows the folder that was just saved.
                ui.set_captures(ModelRc::new(VecModel::from(capture_items(
                    &capture_dir_from_field(&ui.get_capture_dir_value()),
                ))));
                ui.set_status("已保存".into())
            }
            Err(e) => ui.set_status(e.to_string().into()),
        }
    });
    // The settings window keeps its system frame, so it needs the icon itself.
    apply_icon_when_ready(snaplight_ui::SETTINGS_TITLE);
    ui.run()?;
    remember_position(true, ui.window());
    Ok(())
}
pub fn run() -> std::result::Result<(), Box<dyn std::error::Error>> {
    unsafe {
        // The settings folder picker needs a COM apartment on this thread; Slint
        // and the shell tolerate an already-initialized apartment.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    snaplight_platform::native::dpi_aware();
    let config = load_config()?;
    if std::env::args().any(|a| a == "--settings") {
        return settings(config);
    }
    let stress = std::env::args().any(|a| a == "--stress");
    let smoke = std::env::args().any(|a| a == "--smoke") || stress;
    // `--long-capture-demo=x,y,w,h` runs one real long capture session against a
    // screen region and prints its result, so scrolling and stitching can be
    // verified end to end against a scrollable window without a human clicking.
    let demo = std::env::args().find_map(|a| parse_long_capture_demo(&a));
    // The demo runs without the Slint toolbar: it drives the canvas directly and
    // exits when it is done.
    let interactive = !smoke && demo.is_none();
    let queue = Arc::new(Mutex::new(vec![]));
    let thread_queue = queue.clone();
    let ui_slots = Arc::new(UiSlots::default());
    let thread_slots = ui_slots.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let result = canvas_thread(
            config,
            thread_queue,
            tx.clone(),
            thread_slots,
            smoke,
            stress,
            demo,
        );
        if let Err(e) = &result {
            let _ = tx.send(Err(e.clone()));
        }
        if interactive {
            snaplight_ui::invoke(snaplight_ui::quit);
        }
        result
    });
    let ready = rx.recv()??;
    let id = ready.thread_id;
    if interactive {
        let config = load_config()?;
        let ui: Toolbar = snaplight_ui::toolbar(&config)?;
        let _ = ui_slots.toolbar.set(ui.as_weak());
        let _ = ui_slots
            .toolbar_size
            .set(snaplight_ui::toolbar_logical_size(&config.tools));
        let size = snaplight_ui::scale_to_physical(
            snaplight_ui::toolbar_logical_size(&config.tools),
            ready.dpi,
        );
        let parked = parked_position(size);
        ui.window()
            .set_position(PhysicalPosition::new(parked.0, parked.1));
        ui.show()?;
        // The window has its final size once it is shown, so the pill shape and
        // the tool window style are applied now and never again.
        reshape_and_raise(
            snaplight_ui::TOOLBAR_TITLE,
            snaplight_ui::toolbar_logical_size(&config.tools),
        );
        let panel: OptionsPanel = snaplight_ui::options_panel(&config)?;
        let _ = ui_slots.panel.set(panel.as_weak());
        round_pill_window(
            snaplight_ui::OPTIONS_TITLE,
            snaplight_ui::OPTIONS_LOGICAL_SIZE,
        );
        let q = queue.clone();
        panel.on_properties_changed(move |width, block, text, swatch| {
            q.lock().push(UiCommand::Properties {
                width,
                block,
                text: text.into(),
                swatch,
            });
            unsafe {
                let _ = PostThreadMessageW(id, COMMAND, WPARAM(0), LPARAM(0));
            }
        });
        let dismissed = panel.as_weak();
        panel.on_dismissed(move || {
            if let Some(panel) = dismissed.upgrade() {
                let _ = panel.hide();
            }
        });
        let toolbar_ui = ui.as_weak();
        let panel_ui = panel.as_weak();
        let panel_dpi = ready.dpi;
        let bounds = ready.bounds;
        ui.on_options_toggled(move |tool| {
            let (Some(toolbar), Some(panel)) = (toolbar_ui.upgrade(), panel_ui.upgrade()) else {
                return;
            };
            // The select tool has nothing to configure.
            if !(1..=6).contains(&tool) {
                return;
            }
            let open = panel.window().is_visible() && panel.get_tool() == tool;
            if open {
                let _ = panel.hide();
                return;
            }
            panel.set_tool(tool);
            panel.set_stroke(toolbar.get_stroke());
            panel.set_block(toolbar.get_block());
            panel.set_text_value(toolbar.get_text_value());
            panel.set_swatch(toolbar.get_swatch());
            let panel_size =
                snaplight_ui::scale_to_physical(snaplight_ui::OPTIONS_LOGICAL_SIZE, panel_dpi);
            let bar_size = snaplight_ui::scale_to_physical(
                snaplight_ui::toolbar_logical_size(&config.tools),
                panel_dpi,
            );
            let position = toolbar.window().position();
            let (x, y) = options_position(bounds, position.x, position.y, bar_size.1, panel_size);
            panel.window().set_position(PhysicalPosition::new(x, y));
            let _ = panel.show();
            reshape_and_raise(
                snaplight_ui::OPTIONS_TITLE,
                snaplight_ui::OPTIONS_LOGICAL_SIZE,
            );
        });
        let q = queue.clone();
        ui.on_command(move |action| {
            q.lock().push(UiCommand::Action(action));
            unsafe {
                let _ = PostThreadMessageW(id, COMMAND, WPARAM(0), LPARAM(0));
            }
        });
        let q = queue.clone();
        ui.on_properties_changed(move |width, block, text, swatch| {
            q.lock().push(UiCommand::Properties {
                width,
                block,
                text: text.into(),
                swatch,
            });
            unsafe {
                let _ = PostThreadMessageW(id, COMMAND, WPARAM(0), LPARAM(0));
            }
        });
        // The panel is created hidden and only shown when a tool asks for it.
        snaplight_ui::hide_panel(&panel.as_weak());
        let result = snaplight_ui::run_event_loop();
        // A parked toolbar was never placed by a selection, so its position is not
        // worth remembering.
        let position = ui.window().position();
        if (position.x, position.y) != parked {
            remember_position(false, ui.window());
        }
        queue.lock().push(UiCommand::Action(15));
        unsafe {
            let _ = PostThreadMessageW(id, COMMAND, WPARAM(0), LPARAM(0));
        }
        result?;
    }
    thread.join().map_err(|_| "canvas thread panicked")??;
    if smoke && !stress {
        let config = load_config()?;
        let ui = snaplight_ui::toolbar(&config)?;
        ui.show()?;
        reshape_and_raise(
            snaplight_ui::TOOLBAR_TITLE,
            snaplight_ui::toolbar_logical_size(&config.tools),
        );
        // Without a remembered position the toolbar must be placed inside the
        // captured desktop, so it stays reachable on a full-screen capture.
        let size = ui.window().size();
        // The selection readout is published through the same helper the canvas
        // thread uses; running the loop applies it and creates the window.
        snaplight_ui::set_toolbar_status(&ui.as_weak(), "1920 × 1080".into());
        snaplight_ui::run_event_loop_for(std::time::Duration::from_millis(120));
        let physical = ui.window().size();
        let physical = (physical.width as i32, physical.height as i32);
        // The placement math uses the toolbar's logical size scaled by the
        // monitor DPI; the real window must match it.
        let expected = snaplight_ui::scale_to_physical(
            snaplight_ui::toolbar_logical_size(&config.tools),
            ready.dpi,
        );
        if physical != expected {
            return Err(format!(
                "toolbar physical size {physical:?} does not match dpi-scaled {expected:?} at {} dpi",
                ready.dpi
            )
            .into());
        }
        // Permanent regression: attribute updates must never restore a system
        // frame, even after the toolbar has been shown and rendered.
        let hwnd = find_process_window(snaplight_ui::TOOLBAR_TITLE)
            .ok_or("toolbar native window missing")?;
        unsafe {
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            SetWindowLongPtrW(
                hwnd,
                GWL_STYLE,
                style | (WS_CAPTION | WS_THICKFRAME | WS_SYSMENU).0 as isize,
            );
            let applied = GetWindowLongPtrW(hwnd, GWL_STYLE);
            if applied != tool_window_style(applied) {
                return Err("toolbar allowed system window decorations to return".into());
            }
            let mut outer = RECT::default();
            let mut client = RECT::default();
            GetWindowRect(hwnd, &mut outer)?;
            GetClientRect(hwnd, &mut client)?;
            if outer.right - outer.left != client.right - client.left
                || outer.bottom - outer.top != client.bottom - client.top
            {
                return Err("toolbar has an unexpected nonclient frame".into());
            }
        }
        atomic_write(
            &data_dir().join("toolbar-window.json"),
            &serde_json::to_vec_pretty(&serde_json::json!({
                "size_logical": [size.width, size.height],
                "size_physical": [physical.0, physical.1],
                "dpi": ready.dpi,
                "scale": ui.window().scale_factor(),
                "position": [ui.window().position().x, ui.window().position().y],
            }))?,
        )?;
        let size = physical;
        let bounds = ready.bounds;
        let (x, y) = initial_toolbar_position(bounds, size);
        ui.window().set_position(PhysicalPosition::new(x, y));
        let placed = ui.window().position();
        let inside = placed.x >= bounds.left
            && placed.y >= bounds.top
            && placed.x + size.0 <= bounds.right
            && placed.y + size.1 <= bounds.bottom;
        if !inside {
            return Err(format!(
                "toolbar placed outside the capture: {:?} {size:?} bounds {:?}",
                placed, bounds
            )
            .into());
        }
        let snapshot = ui.window().take_snapshot()?;
        write_rgba_png(
            &data_dir().join("toolbar-smoke.png"),
            snapshot.width(),
            snapshot.height(),
            snapshot.as_bytes(),
        )?;
        ui.hide()?;
        // Window position memory must survive a real save/load round trip.
        remember_position(false, ui.window());
        let position = ui.window().position();
        if load_window_state().toolbar
            != Some(ScreenPoint {
                x: position.x,
                y: position.y,
            })
        {
            return Err("toolbar position did not round-trip through ui-state.json".into());
        }
        // Image management: the settings page must list saved captures, and
        // deletion must move a capture's files to the recycle bin.
        let captures_dir = capture_dir(&config);
        std::fs::create_dir_all(&captures_dir)?;
        for id in ["1700000000000000000", "1700000001000000000"] {
            atomic_write(&captures_dir.join(format!("{id}.png")), &[0u8; 16])?;
            atomic_write(&captures_dir.join(format!("{id}.document.json")), b"{}")?;
        }
        if snaplight_platform::list_captures(&captures_dir).len() < 2 {
            return Err("capture list did not include the smoke captures".into());
        }
        let doomed = captures_dir.join("1700000000000000000.png");
        let files = snaplight_platform::capture_files(&captures_dir, "1700000000000000000");
        if files.len() != 2 {
            return Err(format!("expected 2 files for the capture, found {}", files.len()).into());
        }
        snaplight_platform::native::delete_to_recycle_bin(&files)?;
        if doomed.exists() {
            return Err("deleting a capture did not remove its files".into());
        }
        // The settings page is part of the UI surface check as well: the window
        // is a navigation pane plus one options page, so every page has to
        // render without clipping. `settings-smoke.png` keeps showing the page
        // with the capture list.
        let ui = snaplight_ui::settings(&config)?;
        ui.set_capture_dir_value(capture_dir(&config).to_string_lossy().into_owned().into());
        ui.set_config_path(
            snaplight_platform::data_dir()
                .join("config.json")
                .to_string_lossy()
                .into_owned()
                .into(),
        );
        ui.set_data_dir(
            snaplight_platform::data_dir()
                .to_string_lossy()
                .into_owned()
                .into(),
        );
        ui.set_captures(ModelRc::new(VecModel::from(capture_items(
            &capture_dir_from_field(&ui.get_capture_dir_value()),
        ))));
        ui.show()?;
        for (page, slug) in snaplight_ui::SETTINGS_PAGES.iter().enumerate() {
            ui.set_page(page as i32);
            let snapshot = ui.window().take_snapshot()?;
            let bytes = snapshot.as_bytes();
            write_rgba_png(
                &data_dir().join(format!("settings-smoke-{slug}.png")),
                snapshot.width(),
                snapshot.height(),
                bytes,
            )?;
            if *slug == "capture" {
                write_rgba_png(
                    &data_dir().join("settings-smoke.png"),
                    snapshot.width(),
                    snapshot.height(),
                    bytes,
                )?;
            }
        }
        ui.hide()?;
        remember_position(true, ui.window());
        let position = ui.window().position();
        if load_window_state().settings
            != Some(ScreenPoint {
                x: position.x,
                y: position.y,
            })
        {
            return Err("settings position did not round-trip through ui-state.json".into());
        }
        // The quick tool options share the settings wording, so snapshot each
        // variant: pen (stroke width), mosaic (block size) and text.
        let panel = snaplight_ui::options_panel(&config)?;
        for (tool, slug) in [(4, "pen"), (5, "mosaic"), (6, "text")] {
            panel.set_tool(tool);
            panel.show()?;
            let snapshot = panel.window().take_snapshot()?;
            let bytes = snapshot.as_bytes();
            write_rgba_png(
                &data_dir().join(format!("options-smoke-{slug}.png")),
                snapshot.width(),
                snapshot.height(),
                bytes,
            )?;
            if tool == 4 {
                write_rgba_png(
                    &data_dir().join("options-smoke.png"),
                    snapshot.width(),
                    snapshot.height(),
                    bytes,
                )?;
            }
        }
        panel.hide()?;
    }
    Ok(())
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
    fn hover_queries_need_movement_and_time() {
        let start = Instant::now();
        let point = ScreenPoint { x: 100, y: 100 };
        assert!(probe_due(None, point, start));
        let last = Some((point, start));
        // Standing still never re-queries, however long the pointer waits.
        assert!(!probe_due(last, point, start + Duration::from_secs(5)));
        // A one-pixel jitter is not worth a query either.
        assert!(!probe_due(
            last,
            ScreenPoint { x: 102, y: 100 },
            start + Duration::from_secs(1)
        ));
        // Movement inside the interval is throttled.
        assert!(!probe_due(
            last,
            ScreenPoint { x: 140, y: 100 },
            start + Duration::from_millis(1)
        ));
        // Enough movement after enough time queries again.
        assert!(probe_due(
            last,
            ScreenPoint { x: 103, y: 100 },
            start + HOVER_INTERVAL
        ));
        assert!(probe_due(
            last,
            ScreenPoint { x: 100, y: 97 },
            start + Duration::from_secs(1)
        ));
    }
    #[test]
    fn detected_regions_map_to_the_overlay_image() {
        let image = ImageDescriptor {
            id: ImageId(1),
            width: 2560,
            height: 1440,
            stride: 2560 * 4,
            format: PixelFormat::Bgra8,
            alpha: AlphaMode::Opaque,
            color_space: ColorSpace::Srgb,
        };
        let screen = rect(0, 0, 2560, 1440);
        // A region on this monitor keeps its offset and size.
        assert_eq!(
            hover_region(
                screen,
                &image,
                ScreenRect {
                    x: 100,
                    y: 200,
                    width: 800,
                    height: 600
                }
            ),
            Some(ImageRect {
                x: 100.0,
                y: 200.0,
                width: 800.0,
                height: 600.0
            })
        );
        // A second monitor to the left reports negative screen coordinates.
        let left_screen = rect(-1920, 0, 0, 1080);
        assert_eq!(
            hover_region(
                left_screen,
                &image,
                ScreenRect {
                    x: -1900,
                    y: 40,
                    width: 600,
                    height: 400
                }
            ),
            Some(ImageRect {
                x: 20.0,
                y: 40.0,
                width: 600.0,
                height: 400.0
            })
        );
        // A region that reaches past the overlay's edge is clipped to it.
        assert_eq!(
            hover_region(
                left_screen,
                &image,
                ScreenRect {
                    x: -100,
                    y: 0,
                    width: 300,
                    height: 200
                }
            ),
            Some(ImageRect {
                x: 1820.0,
                y: 0.0,
                width: 100.0,
                height: 200.0
            })
        );
        // A region entirely on another monitor has nothing to show here.
        assert_eq!(
            hover_region(
                screen,
                &image,
                ScreenRect {
                    x: -500,
                    y: 100,
                    width: 400,
                    height: 300
                }
            ),
            None
        );
        assert_eq!(
            hover_region(
                screen,
                &image,
                ScreenRect {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0
                }
            ),
            None
        );
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
    fn selection_handles_hit_all_eight_at_scaled_dpi() {
        let r = ImageRect {
            x: 100.0,
            y: 100.0,
            width: 200.0,
            height: 100.0,
        };
        for row in 0..3 {
            for col in 0..3 {
                let p = ImagePoint {
                    x: 100.0 + 100.0 * col as f32,
                    y: 100.0 + 50.0 * row as f32,
                };
                let expected = (col != 1 || row != 1).then_some(SelectionHandle { col, row });
                assert_eq!(selection_handle(r, p, 144, 1.0), expected);
            }
        }
        assert_eq!(
            selection_handle(r, ImagePoint { x: 89.0, y: 100.0 }, 144, 1.0),
            Some(SelectionHandle { col: 0, row: 0 })
        );
        assert_eq!(
            selection_handle(r, ImagePoint { x: 89.0, y: 100.0 }, 96, 1.0),
            None
        );
        // A zoomed-out view widens the grab radius in image pixels so the handles
        // stay reachable on screen: at 0.2 the radius is 12 / 0.2 = 60 image
        // pixels, so a point 50 away still grabs where it would not at 1.0.
        assert_eq!(
            selection_handle(r, ImagePoint { x: 50.0, y: 100.0 }, 144, 0.2),
            Some(SelectionHandle { col: 0, row: 0 })
        );
        assert_eq!(
            selection_handle(r, ImagePoint { x: 50.0, y: 100.0 }, 144, 1.0),
            None
        );
    }
    #[test]
    fn resizing_preserves_opposite_edges_and_clamps_to_image() {
        let r = ImageRect {
            x: 100.0,
            y: 100.0,
            width: 200.0,
            height: 100.0,
        };
        for row in 0..3 {
            for col in 0..3 {
                if col == 1 && row == 1 {
                    continue;
                }
                let resized = resize_selection(
                    r,
                    SelectionHandle { col, row },
                    ImagePoint { x: 150.0, y: 150.0 },
                    500,
                    400,
                );
                assert_eq!(resized.x, if col == 0 { 150.0 } else { 100.0 });
                assert_eq!(resized.y, if row == 0 { 150.0 } else { 100.0 });
                assert_eq!(
                    resized.x + resized.width,
                    if col == 2 { 150.0 } else { 300.0 }
                );
                assert_eq!(
                    resized.y + resized.height,
                    if row == 2 { 150.0 } else { 200.0 }
                );
            }
        }
        let h = SelectionHandle { col: 0, row: 0 };
        assert_eq!(
            resize_selection(r, h, ImagePoint { x: -50.0, y: -50.0 }, 500, 400),
            ImageRect {
                x: 0.0,
                y: 0.0,
                width: 300.0,
                height: 200.0
            }
        );
        assert_eq!(
            resize_selection(r, h, ImagePoint { x: 450.0, y: 350.0 }, 500, 400),
            ImageRect {
                x: 299.0,
                y: 199.0,
                width: 1.0,
                height: 1.0
            }
        );
        let enlarged = resize_selection(
            r,
            SelectionHandle { col: 2, row: 2 },
            ImagePoint { x: 900.0, y: 900.0 },
            500,
            400,
        );
        assert_eq!(
            enlarged,
            ImageRect {
                x: 100.0,
                y: 100.0,
                width: 400.0,
                height: 300.0
            }
        );
        let mut doc = Document {
            version: 1,
            image: ImageDescriptor {
                id: ImageId(1),
                width: 500,
                height: 400,
                stride: 2000,
                format: PixelFormat::Bgra8,
                alpha: AlphaMode::Opaque,
                color_space: ColorSpace::Srgb,
            },
            selection: Some(r),
            annotations: vec![],
        };
        let mut history = History::default();
        history.execute(&mut doc, Edit::Selection(Some(enlarged)));
        history.undo(&mut doc);
        assert_eq!(doc.selection, Some(r));
        history.redo(&mut doc);
        assert_eq!(doc.selection, Some(enlarged));
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
}

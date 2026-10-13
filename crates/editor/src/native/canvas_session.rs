//! 编辑器会话边界：画布窗口、交互状态与绘制窗口的发布逻辑。
//!
//! 这里拥有画布会话的全部状态（`Canvas` 私有）、覆盖窗口过程、会话线程入口
//! 以及浮窗的位置/历史发布；`native` 只负责组装 UI、启动会话和 smoke 验收。
//! 长截图作为画布的内部子模块留在同一实现里，只通过本模块的公开接口被协调层
//! 使用。

use parking_lot::Mutex;
use snaplight_domain::*;
use snaplight_platform::{
    atomic_write, capture_dir, data_dir, native::PointHole, write_pixels, CaptureService,
};
use snaplight_render_win::*;
use snaplight_ui::{ComponentHandle, PhysicalPosition};
use std::{
    io::Write,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{Com::*, LibraryLoader::*, Threading::*},
        UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
};

/// The interactive long capture: a timer samples the selected region while the
/// user scrolls the content, and the stitched result is trimmed and finished
/// from a bird's eye panel.
#[path = "scroll_capture.rs"]
mod scroll_capture;
use scroll_capture::LongCaptureSession;

use super::output::{clipboard, write_capture_source, write_pin_bundle, write_png};
use super::pin_edit::PinEdit;
use super::selection::{
    move_selection, move_target, resize_selection, selection_handle, SelectionHandle,
};
use super::smoke::{stress_annotations, FILL, FILLED, OUTLINED, SMOKE_TEXT, STRESS_FRAMES};
use super::tool_windows::{
    follow_position, monitor_work_area, paint_bar_position, parked_position,
    pin_floating_positions, raise_floating_windows, reshape_and_raise, reshape_when_sized,
};
use super::ui_bridge::{UiCommand, UiSlots};

pub(super) const COMMAND: u32 = WM_APP + 42;
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
/// Timer that takes the value readout away again after a scrub stops moving.
const SCRUB_TIMER: usize = 2;
/// How long a scrub readout stays on screen after the last change, in
/// milliseconds. Long enough to read while the pointer pauses, short enough to
/// disappear on its own when the key is released and the pointer stops.
const SCRUB_HUD_MS: u32 = 900;
/// Paint a session starts with: the configured colours, sizes and opacity, with
/// the two paint switches taken per tool from the configured tool lists.
fn configured_styles(config: &Config) -> [Style; 7] {
    let defaults = Style::default();
    let stroke = parse_hex_color(&config.stroke_color).unwrap_or(defaults.stroke);
    let fill = parse_hex_color(&config.fill_color).unwrap_or(defaults.fill);
    let opacity = config.fill_opacity.min(100) as f32 / 100.0;
    Tool::ALL.map(|tool| Style {
        stroke,
        fill: [fill[0], fill[1], fill[2], opacity],
        width: config.stroke_width,
        stroke_on: tool.can_stroke() && config.stroke_tools.contains(&tool),
        fill_on: tool.can_fill() && config.fill_tools.contains(&tool),
        font_size: config.font_size as f32,
    })
}
/// One drawing value the number keys adjust, Photoshop style: hold the key and
/// slide the pointer sideways.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScrubSlot {
    StrokeWidth,
    FillOpacity,
    FontSize,
}
impl ScrubSlot {
    /// Slots in key order: 1 adjusts the outline width, 2 the fill opacity and
    /// 3 the text size.
    const KEYS: [(ScrubSlot, i32, i32, &'static str); 3] = [
        (ScrubSlot::StrokeWidth, 0x31, 0x61, "描边宽度"),
        (ScrubSlot::FillOpacity, 0x32, 0x62, "填充不透明度"),
        (ScrubSlot::FontSize, 0x33, 0x63, "字号"),
    ];
    fn label(self) -> &'static str {
        Self::KEYS
            .iter()
            .find(|(slot, ..)| *slot == self)
            .map(|(_, _, _, label)| *label)
            .expect("every slot has a label")
    }
    /// Bounds of the value, and how many screen pixels one unit of movement
    /// covers. A wider range takes more pixels per unit so every value stays
    /// reachable in one short swipe.
    fn range(self) -> (f32, f32, f32) {
        match self {
            ScrubSlot::StrokeWidth => (1.0, 64.0, 2.0),
            ScrubSlot::FillOpacity => (0.0, 100.0, 2.5),
            ScrubSlot::FontSize => (MIN_FONT_SIZE as f32, MAX_FONT_SIZE as f32, 1.5),
        }
    }
    /// Value the slot currently carries in `style`.
    fn value(self, style: Style) -> f32 {
        match self {
            ScrubSlot::StrokeWidth => style.width,
            ScrubSlot::FillOpacity => style.fill[3] * 100.0,
            ScrubSlot::FontSize => style.font_size,
        }
    }
    /// Writes one value into a style.
    fn set(self, style: &mut Style, value: f32) {
        match self {
            ScrubSlot::StrokeWidth => style.width = value,
            ScrubSlot::FillOpacity => style.fill[3] = value / 100.0,
            ScrubSlot::FontSize => style.font_size = value,
        }
    }
    /// Readout shown over the canvas while the value moves.
    fn hud(self, value: f32) -> String {
        match self {
            ScrubSlot::FillOpacity => format!("{} {}%", self.label(), value.round() as i32),
            _ => format!("{} {} px", self.label(), value.round() as i32),
        }
    }
}
/// Value a scrub produces after moving `pixels` horizontally from where the key
/// was pressed.
fn scrub_value(slot: ScrubSlot, start: f32, pixels: f32) -> f32 {
    let (min, max, per_unit) = slot.range();
    (start + pixels / per_unit).clamp(min, max)
}
/// A scrub in progress: which value is moving and where it started.
#[derive(Clone, Copy, Debug)]
struct Scrub {
    slot: ScrubSlot,
    start_x: i32,
    start: f32,
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
/// True while the physical key with this virtual-key code is held. Scrubbing
/// reads the key state instead of key messages so the gesture does not depend on
/// which Snaplight window owns the keyboard focus.
fn key_down(virtual_key: i32) -> bool {
    unsafe { (GetAsyncKeyState(virtual_key) as i16) < 0 }
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
    /// Last tool that paints, so Space can return from the select tool to
    /// drawing with the paint bar.
    last_draw_tool: Tool,
    /// Paint per tool, indexed by `Tool::id`. Colours, widths, opacity and text
    /// size are shared defaults; the two paint switches belong to each tool, so
    /// text can fill its glyphs where a rectangle draws an outline.
    styles: [Style; 7],
    /// The value the number keys are adjusting right now, if any.
    scrub: Option<Scrub>,
    block: u32,
    text: String,
    start: Option<ImagePoint>,
    resize: Option<(ImageRect, SelectionHandle)>,
    /// The committed selection a press is moving, captured when the gesture
    /// started so the drag stays a translation from there.
    moving: Option<ImageRect>,
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
    /// Set when this session annotates a pinned image (`--edit-pin=<image>`):
    /// the document is a fixed picture, the window is the pin's, and 「钉住」
    /// writes the result back into the pin's files instead of opening a pin.
    pin_edit: Option<PinEdit>,
}
impl Canvas {
    /// True while the document is a fixed picture re-opened for annotation:
    /// there is no capture region to choose, so a press never resizes or moves
    /// one, the pointer is never detected, and the renderer draws no chrome.
    fn fixed_image(&self) -> bool {
        self.pin_edit.is_some()
    }
    fn draw(&mut self) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let viewport = self.viewport();
        // A fixed picture is drawn without a capture region's dimming and chrome,
        // even if the loaded document still carries a selection.
        self.interaction.fixed_image = self.fixed_image();
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
    /// Paint of the tool that is currently drawing.
    fn style(&self) -> Style {
        self.style_of(self.tool)
    }
    fn style_of(&self, tool: Tool) -> Style {
        self.styles[tool.id() as usize]
    }
    /// Snapshot the options panel and the bottom bar edit and show.
    fn paint(&self) -> snaplight_ui::ToolPaint {
        let style = self.style();
        snaplight_ui::ToolPaint {
            tool: self.tool.id(),
            stroke_width: style.width.round().clamp(1.0, 64.0) as i32,
            fill_opacity: (style.fill[3] * 100.0).round().clamp(0.0, 100.0) as i32,
            font_size: style
                .font_size
                .round()
                .clamp(MIN_FONT_SIZE as f32, MAX_FONT_SIZE as f32) as i32,
            block: self.block as i32,
            stroke_color: snaplight_ui::slint_color(style.stroke),
            fill_color: snaplight_ui::slint_color([
                style.fill[0],
                style.fill[1],
                style.fill[2],
                1.0,
            ]),
            stroke_on: style.stroke_on,
            fill_on: style.fill_on,
            text: self.text.clone().into(),
        }
    }
    /// Applies a snapshot from the UI. Colours, sizes and opacity are the
    /// defaults every tool shares; the two paint switches belong to the tool the
    /// window was opened for, so turning off a fill for rectangles leaves the
    /// text fill alone.
    fn apply_paint(&mut self, paint: &snaplight_ui::ToolPaint) {
        let stroke = snaplight_ui::rgba_of(paint.stroke_color);
        let fill = snaplight_ui::rgba_of(paint.fill_color);
        let opacity = paint.fill_opacity.clamp(0, 100) as f32 / 100.0;
        let width = paint.stroke_width.clamp(1, 64) as f32;
        let font_size = paint
            .font_size
            .clamp(MIN_FONT_SIZE as i32, MAX_FONT_SIZE as i32) as f32;
        for style in self.styles.iter_mut() {
            style.stroke = stroke;
            style.fill = [fill[0], fill[1], fill[2], opacity];
            style.width = width;
            style.font_size = font_size;
        }
        if let Some(tool) = Tool::from_id(paint.tool) {
            let style = &mut self.styles[tool.id() as usize];
            style.stroke_on = paint.stroke_on;
            style.fill_on = paint.fill_on;
        }
        self.block = paint.block.clamp(2, 128) as u32;
        self.text = paint.text.to_string();
    }
    /// Photoshop-style scrubbing: while one of the number keys is held, sideways
    /// pointer movement moves that value. The key state is read here instead of
    /// from key messages, so the gesture works no matter which Snaplight window
    /// holds the keyboard focus.
    fn scrub_move(&mut self, x: i32) {
        let held = ScrubSlot::KEYS
            .iter()
            .find(|(_, digit, numpad, _)| key_down(*digit) || key_down(*numpad));
        match (self.scrub, held) {
            (None, None) => {}
            (None, Some((slot, ..))) => {
                let slot = *slot;
                self.scrub = Some(Scrub {
                    slot,
                    start_x: x,
                    start: slot.value(self.style()),
                });
                self.show_scrub(slot, slot.value(self.style()));
            }
            (Some(_), None) => {
                // The key is up: the readout goes away with the gesture.
                self.scrub = None;
                self.clear_hud();
            }
            (Some(scrub), Some((slot, ..))) if scrub.slot == *slot => {
                let slot = scrub.slot;
                let value = scrub_value(slot, scrub.start, (x - scrub.start_x) as f32);
                for style in self.styles.iter_mut() {
                    slot.set(style, value);
                }
                self.show_scrub(slot, value);
            }
            // A different digit took over: the gesture restarts from here.
            (Some(_), Some((slot, ..))) => {
                let slot = *slot;
                self.scrub = Some(Scrub {
                    slot,
                    start_x: x,
                    start: slot.value(self.style()),
                });
                self.show_scrub(slot, slot.value(self.style()));
            }
        }
    }
    /// Space returns to drawing: the last drawing tool comes back when the select
    /// tool is active, and the paint bar is shown and raised, so its controls are
    /// reachable again after any gesture.
    fn show_paint_bar(&mut self) {
        if self.tool == Tool::Select {
            self.tool = self.last_draw_tool;
            if let Some(weak) = self.ui_slots.toolbar() {
                snaplight_ui::set_toolbar_active(weak, self.tool.id());
            }
        }
        publish_paint(self);
        raise_floating_windows();
        unsafe { self.invalidate() };
    }
    /// Restores a pinned image session's editing surface once its Slint windows
    /// exist. The pin has no selection for the toolbar to follow, so it is placed
    /// against the whole picture; the session then starts as if Space had been
    /// pressed, bringing the last drawing tool and its bar back. The canvas takes
    /// the foreground so keyboard gestures reach it.
    fn resume_pin_editing(&mut self) {
        place_toolbar_for_pin(self);
        self.show_paint_bar();
        publish_history(self);
        unsafe {
            let _ = SetForegroundWindow(self.hwnd);
            let _ = SetFocus(self.hwnd);
        }
    }
    /// Shows the value readout, restarts the timer that takes it away, and keeps
    /// the floating windows in step with the canvas.
    fn show_scrub(&mut self, slot: ScrubSlot, value: f32) {
        self.interaction.hud = Some(slot.hud(value));
        unsafe {
            let _ = SetTimer(self.hwnd, SCRUB_TIMER, SCRUB_HUD_MS, None);
        }
        publish_paint(self);
        unsafe { self.invalidate() };
    }
    fn clear_hud(&mut self) {
        if self.interaction.hud.take().is_some() {
            unsafe {
                let _ = KillTimer(self.hwnd, SCRUB_TIMER);
                self.invalidate();
            }
        }
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
                x: rect.x + rect.width * handle.column() as f32 / 2.0 + p.x - start.x,
                y: rect.y + rect.height * handle.row() as f32 / 2.0 + p.y - start.y,
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
        if let Some(rect) = self.moving {
            self.interaction.selection_draft = Some(move_selection(
                rect,
                p.x - start.x,
                p.y - start.y,
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
            style: self.style(),
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
                self.tool = Tool::from_id(id).expect("tool ids 0..=6 exist");
                if self.tool != Tool::Select {
                    self.last_draw_tool = self.tool;
                }
                // Only the select tool adopts a detected region.
                self.set_hover(None);
                // The options panel and the bottom paint bar follow the tool.
                publish_paint(self);
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
            // In a pinned image session, 「钉住」 means "keep these annotations":
            // the annotated picture goes back into the pin's files and the pin
            // reloads it, so the pin keeps its place and its zoom. Closing the
            // session (disk 15) leaves the pin untouched.
            14 if self.pin_edit.is_some() => self.finish_pin_edit()?,
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
    /// 「钉住」 in a pinned image session: the annotated picture goes back into
    /// the pin's files, and the pin process reloads it when the session ends, so
    /// the pin keeps its place and its zoom. The session edits the whole
    /// picture, so a selection left in the document must not crop the result.
    fn finish_pin_edit(&mut self) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let edit = self.pin_edit.take();
        self.doc.selection = None;
        let (image, bytes) = self.renderer.export(&self.doc)?;
        if let Some(edit) = edit {
            edit.write_back(&image, &bytes, &self.doc, &self.base)?;
        }
        unsafe {
            PostQuitMessage(0);
        }
        Ok(())
    }
    /// `--edit-pin-demo`: annotates one scripted rectangle, checks that the
    /// session draws the picture undimmed — a capture session would mask
    /// everything outside its selection — writes the result back the way
    /// 「钉住」 does, and reports what it found. The whole round trip of a pinned
    /// image can therefore be verified without a user clicking anything.
    fn pin_edit_demo(
        &mut self,
    ) -> std::result::Result<serde_json::Value, Box<dyn std::error::Error>> {
        const FILL: [f32; 4] = [0.1, 0.8, 0.3, 1.0];
        let edit = self.pin_edit.as_ref().ok_or("not a pinned image session")?;
        let (image_path, document_path) = (edit.image_path(), edit.document_path());
        let rect = ImageRect {
            x: (self.doc.image.width / 4) as f32,
            y: (self.doc.image.height / 4) as f32,
            width: 40.0,
            height: 30.0,
        };
        self.history.execute(
            &mut self.doc,
            Edit::Add(Annotation {
                shape: Shape::Rectangle(rect),
                style: Style {
                    fill: FILL,
                    fill_on: true,
                    stroke_on: false,
                    ..Style::default()
                },
            }),
        );
        self.renderer.invalidate_annotations();
        self.renderer.record_composed(true);
        self.draw()?;
        let composed = self
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
        let source = |x: usize, y: usize| -> [u8; 4] {
            let n = y * self.doc.image.stride as usize + x * 4;
            [
                self.base[n],
                self.base[n + 1],
                self.base[n + 2],
                self.base[n + 3],
            ]
        };
        let within = |got: [u8; 4], want: [u8; 4], tolerance: i32| {
            (0..3).all(|i| (got[i] as i32 - want[i] as i32).abs() <= tolerance)
        };
        // The composed frame is the window (view pixels) while the annotation is
        // placed in image pixels, so a session on a pin that is scaled shows the
        // picture at that scale and every read-back coordinate is scaled to it.
        let zoom = self.view.scale.max(0.0001);
        let (cx, cy) = (
            ((rect.x + rect.width / 2.0) * zoom) as usize,
            ((rect.y + rect.height / 2.0) * zoom) as usize,
        );
        let painted = pixel(cx, cy);
        let wanted = [
            (FILL[2] * 255.0).round() as u8,
            (FILL[1] * 255.0).round() as u8,
            (FILL[0] * 255.0).round() as u8,
            255,
        ];
        if !within(painted, wanted, 8) {
            return Err(
                format!("the scripted annotation is {painted:?}, expected {wanted:?}").into(),
            );
        }
        // Bottom right corner: painted by neither the annotation nor any chrome,
        // so it shows exactly what the picture holds when nothing is dimmed.
        let (ax, ay) = (
            (self.doc.image.width as f32 * zoom) as usize - 6,
            (self.doc.image.height as f32 * zoom) as usize - 6,
        );
        let shown = pixel(ax, ay);
        // A scaled view samples the source at a slightly different pixel, so any
        // of the three-by-three around the mapped one is the same picture.
        let (sx, sy) = (
            ((ax as f32 + 0.5) / zoom) as i32,
            ((ay as f32 + 0.5) / zoom) as i32,
        );
        let undimmed = (-1i32..=1).any(|dx| {
            (-1i32..=1).any(|dy| {
                let x = (sx + dx).clamp(0, self.doc.image.width as i32 - 1) as usize;
                let y = (sy + dy).clamp(0, self.doc.image.height as i32 - 1) as usize;
                within(source(x, y), shown, 2)
            })
        });
        if !undimmed {
            let original = source(
                sx.clamp(0, self.doc.image.width as i32 - 1) as usize,
                sy.clamp(0, self.doc.image.height as i32 - 1) as usize,
            );
            return Err(format!(
                "the pinned picture is drawn as {shown:?} at {ax},{ay}, expected the undimmed {original:?}"
            )
            .into());
        }
        self.renderer.record_composed(false);
        let width = self.doc.image.width;
        let height = self.doc.image.height;
        self.finish_pin_edit()?;
        Ok(serde_json::json!({
            "image": image_path.to_string_lossy(),
            "document": document_path.to_string_lossy(),
            "width": width,
            "height": height,
            "annotations": 1,
            "annotation": {"x": rect.x, "y": rect.y, "width": rect.width, "height": rect.height},
            "composed_pixel": painted,
        }))
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
                // A fixed picture has no capture region, so a press cannot resize
                // or move one and the select tool has nothing to draw: it only
                // remembers where the pointer is.
                if state.fixed_image() {
                    if state.tool == Tool::Select {
                        return LRESULT(0);
                    }
                } else {
                    let handle = state.doc.selection.and_then(|r| {
                        selection_handle(r, p, state.dpi, state.view.scale, MIN_VIEW_SCALE)
                            .map(|h| (r, h))
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
                    // A press inside the committed region moves it: the select tool
                    // has nothing to draw there, and re-selecting the same area from
                    // the inside is never what the gesture means. A press anywhere
                    // else still draws a new region.
                    if let Some(rect) = move_target(state.tool, state.doc.selection, p) {
                        park_toolbar(state);
                        state.moving = Some(rect);
                        state.start = Some(p);
                        state.interaction.dragging = true;
                        SetCapture(hwnd);
                        state.update_drag(p);
                        state.invalidate();
                        return LRESULT(0);
                    }
                }
                if state.tool == Tool::Text {
                    if !state.text.is_empty() {
                        let style = state.style();
                        state.history.execute(
                            &mut state.doc,
                            Edit::Add(Annotation {
                                shape: Shape::Text {
                                    at: p,
                                    text: state.text.clone(),
                                    size: style.font_size,
                                },
                                style,
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
                            state.doc.selection.and_then(|r| {
                                selection_handle(r, p, state.dpi, state.view.scale, MIN_VIEW_SCALE)
                            })
                        });
                        let cursor = if state.fixed_image() {
                            // There is no region to resize or move: the pointer
                            // draws, and the select tool has nothing to do.
                            if state.tool == Tool::Select {
                                IDC_ARROW
                            } else {
                                IDC_CROSS
                            }
                        } else {
                            match handle {
                                Some(h) if h.column() == 1 => IDC_SIZENS,
                                Some(h) if h.row() == 1 => IDC_SIZEWE,
                                Some(h) if h.column() == h.row() => IDC_SIZENWSE,
                                Some(_) => IDC_SIZENESW,
                                // Inside the committed region the select tool moves
                                // it, so the cursor offers that instead of a cross.
                                None if move_target(state.tool, state.doc.selection, p)
                                    .is_some() =>
                                {
                                    IDC_SIZEALL
                                }
                                None => IDC_CROSS,
                            }
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
                // A fixed picture is never detected: the region under the pointer
                // belongs to the pin, not to a capture about to be taken.
                if !state.fixed_image() {
                    state.request_hover(p);
                }
                // Holding 1, 2 or 3 while the pointer moves adjusts one drawing
                // value, so the gesture is driven from the movement itself.
                state.scrub_move(state.screen.left + (l.0 as u16 as i16 as i32));
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
            // Activation follows a click anywhere on the capture, including the
            // parts the floating windows are not covering.
            WM_ACTIVATE if w.0 as u32 != 0 => {
                raise_floating_windows();
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
                state.moving = None;
                state.interaction.dragging = false;
                // The preview belongs to the pointer, not to a finished gesture.
                state.hover_probe.pending = None;
                state.interaction.hover = None;
                // Drawing on the capture activates it, which puts the overlay in
                // front of the floating windows; they belong on top again.
                raise_floating_windows();
                let _ = ReleaseCapture();
                publish_history(state);
                state.invalidate();
                return LRESULT(0);
            }
            WM_CAPTURECHANGED => {
                let interrupted = state.start.is_some();
                state.start = None;
                state.resize = None;
                state.moving = None;
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
                // Space is the shortcut back to painting: it brings the paint bar
                // and the last drawing tool back, whatever happened before.
                if w.0 as u16 == 0x20 {
                    state.show_paint_bar();
                    return LRESULT(0);
                }
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
            // The readout of a scrub goes away on its own once the pointer stops
            // moving, because a released key produces no message here.
            WM_TIMER if w.0 == SCRUB_TIMER => {
                state.scrub = None;
                state.clear_hud();
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
/// What this editor process was asked to do: the verification switches, the long
/// capture demo region, and the pinned image session if there is one.
pub(super) struct EditorSession {
    /// `--smoke`: scripted checks against a synthetic frame instead of the user.
    pub(super) smoke: bool,
    /// `--stress`: the 4K frame loop, which implies the smoke frame.
    pub(super) stress: bool,
    /// `--long-capture-demo=x,y,w,h`: one long capture against a fixed region.
    pub(super) long_capture: Option<RECT>,
    /// `--edit-pin=<image>`: annotate a pinned image in place.
    pub(super) pin_edit: Option<PinEdit>,
}
/// Forwards `--long-capture-demo=x,y,w,h` parsing to the long-capture module,
/// which owns the argument and its fixed region.
pub(super) fn parse_long_capture_demo(arg: &str) -> Option<RECT> {
    scroll_capture::parse_long_capture_demo(arg)
}
pub(super) fn canvas_thread(
    config: Config,
    queue: Arc<Mutex<Vec<UiCommand>>>,
    ready: std::sync::mpsc::Sender<std::result::Result<CaptureReady, String>>,
    ui_slots: Arc<UiSlots>,
    session: EditorSession,
) -> std::result::Result<(), String> {
    let EditorSession {
        smoke,
        stress,
        long_capture: demo,
        mut pin_edit,
    } = session;
    let result = (|| -> std::result::Result<(), Box<dyn std::error::Error>> {
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
            // A pinned image session edits the picture the pin shows instead of
            // capturing the desktop: the pin is on the desktop, so a fresh
            // capture would photograph the pin into its own base image. The
            // document comes along, so the annotations it carries come back.
            let mut pin_document = None;
            let mut frames = Vec::new();
            let capture_us;
            if let Some(edit) = &pin_edit {
                let (image, base, document) = edit.load()?;
                capture_us = 0;
                frames.push(Gpu::new(None)?.frame_from_bgra(image, &base, edit.screen_rect())?);
                pin_document = Some((base, document));
            } else {
                let capture_started = std::time::Instant::now();
                frames = capture_with_retry()?;
                capture_us = capture_started.elapsed().as_micros() as u64;
            }
            // The scripted pin session drives itself and needs no message loop.
            let pin_demo = pin_edit.as_ref().is_some_and(|edit| edit.is_demo());
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
                    left: acc.left.min(frame.screen().left),
                    top: acc.top.min(frame.screen().top),
                    right: acc.right.max(frame.screen().right),
                    bottom: acc.bottom.max(frame.screen().bottom),
                },
            );
            let mut dpi = 96u32;
            for frame in frames {
                let s = frame.screen();
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
                // The swap chain is the window, not the image: a pinned image
                // session shows the picture at the pin's zoom inside a window of
                // the pin's size.
                let view = (
                    (s.right - s.left).max(1) as u32,
                    (s.bottom - s.top).max(1) as u32,
                );
                let renderer = match OverlayRenderer::new(hwnd, &frame, view) {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = DestroyWindow(hwnd);
                        return Err(e.into());
                    }
                };
                let pin_session = pin_edit.is_some();
                let (doc, base) = match pin_document.take() {
                    Some((base, doc)) => (doc, base),
                    None => {
                        let doc = Document {
                            version: 1,
                            image: frame.descriptor().clone(),
                            selection: None,
                            annotations: vec![],
                        };
                        // Retain the immutable base image (BGRA8, `doc.image.stride`) so a
                        // lost device can be rebuilt without re-capturing or losing the
                        // document.
                        let base = renderer.original(&doc)?;
                        (doc, base)
                    }
                };
                let window_dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd);
                if window_dpi > 0 {
                    dpi = window_dpi;
                }
                // A pinned image session shows the picture at the pin's zoom
                // inside the pin's window; a capture session is 1:1.
                let view_transform = if pin_session {
                    ViewTransform::fit(&doc.image, view)
                } else {
                    ViewTransform::plain()
                };
                let mut canvas = Box::new(Canvas {
                    hwnd,
                    renderer,
                    doc,
                    history: History::default(),
                    interaction: Interaction::default(),
                    tool: Tool::Select,
                    last_draw_tool: Tool::Rectangle,
                    styles: configured_styles(&config),
                    scrub: None,
                    block: config.mosaic_block,
                    text: String::new(),
                    start: None,
                    resize: None,
                    moving: None,
                    active: active.clone(),
                    error: None,
                    base,
                    screen: s,
                    view: view_transform,
                    device_rebuilds: 0,
                    capture_dir: capture_dir(&config),
                    ui_slots: ui_slots.clone(),
                    dpi: window_dpi.max(96),
                    detect: if pin_session {
                        WindowDetect::Off
                    } else {
                        config.window_detect
                    },
                    pin_edit: pin_edit.take(),
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
                    if !stress && frame.descriptor().id != ImageId(999) {
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
                        width: frame.descriptor().width.min(640).saturating_sub(20) as f32,
                        height: frame.descriptor().height.min(400).saturating_sub(20) as f32,
                    });
                    canvas.doc.annotations = if stress {
                        stress_annotations(frame.descriptor().width, frame.descriptor().height)
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
                            // Paint checks: one filled shape, one outlined shape
                            // over the same pixels, and text that is filled and
                            // outlined from the same glyph outlines.
                            Annotation {
                                shape: Shape::Rectangle(ImageRect {
                                    x: FILLED.x,
                                    y: FILLED.y,
                                    width: FILLED.width,
                                    height: FILLED.height,
                                }),
                                style: Style {
                                    fill: FILL,
                                    fill_on: true,
                                    stroke_on: false,
                                    ..Style::default()
                                },
                            },
                            Annotation {
                                shape: Shape::Rectangle(ImageRect {
                                    x: OUTLINED.x,
                                    y: OUTLINED.y,
                                    width: OUTLINED.width,
                                    height: OUTLINED.height,
                                }),
                                style: Style {
                                    fill: FILL,
                                    fill_on: false,
                                    stroke: FILL,
                                    stroke_on: true,
                                    width: 6.0,
                                    ..Style::default()
                                },
                            },
                            Annotation {
                                shape: Shape::Text {
                                    at: SMOKE_TEXT,
                                    text: "Snaplight".into(),
                                    size: 34.0,
                                },
                                style: Style {
                                    fill: FILL,
                                    fill_on: true,
                                    stroke: [1.0, 1.0, 1.0, 1.0],
                                    stroke_on: true,
                                    width: 3.0,
                                    ..Style::default()
                                },
                            },
                        ]
                    };
                    canvas.renderer.record_composed(!stress);
                    canvas.renderer.render(
                        &canvas.doc,
                        Viewport::plain(
                            frame.descriptor().width,
                            frame.descriptor().height,
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
                        // Paint: a filled shape shows its fill, the same colour
                        // as an outline leaves the shape's interior as captured,
                        // and filled text with an outline produces ink from its
                        // glyph geometry.
                        let fill = [
                            (FILL[2] * 255.0).round() as u8,
                            (FILL[1] * 255.0).round() as u8,
                            (FILL[0] * 255.0).round() as u8,
                            255,
                        ];
                        let filled = pixel(
                            (FILLED.x + FILLED.width / 2.0) as usize,
                            (FILLED.y + FILLED.height / 2.0) as usize,
                        );
                        if !within(filled, fill, 6) {
                            return Err(format!(
                                "filled shape is {filled:?}, expected the fill {fill:?}"
                            )
                            .into());
                        }
                        let (ox, oy) = (
                            (OUTLINED.x + OUTLINED.width / 2.0) as usize,
                            (OUTLINED.y + OUTLINED.height / 2.0) as usize,
                        );
                        let outlined = pixel(ox, oy);
                        let wanted = captured(ox, oy);
                        if !within(outlined, wanted, 6) {
                            return Err(format!(
                                "outlined shape is {outlined:?}, expected the unfilled capture {wanted:?}"
                            )
                            .into());
                        }
                        let mut ink = 0;
                        for x in SMOKE_TEXT.x as usize..(SMOKE_TEXT.x as usize + 180) {
                            for y in SMOKE_TEXT.y as usize..(SMOKE_TEXT.y as usize + 50) {
                                if !within(pixel(x, y), captured(x, y), 8) {
                                    ink += 1;
                                }
                            }
                        }
                        if ink < 40 {
                            return Err(format!(
                                "filled and outlined text painted only {ink} pixels"
                            )
                            .into());
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
                                frame.descriptor().width,
                                frame.descriptor().height,
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
                                    frame.descriptor().width,
                                    frame.descriptor().height,
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
                    let telemetry = serde_json::json!({"width":frame.descriptor().width,"height":frame.descriptor().height,"capture_fallback":frame.is_fallback(),"capture_agreement":agreement,"software_gpu":frame.is_software(),"capture_us":capture_us,"frames":canvas.renderer.frame_count(),"device_rebuilds":canvas.device_rebuilds,"cpu_us":canvas.renderer.cpu_samples_us(),"present_wait_us":canvas.renderer.present_samples_us(),"gpu_us":canvas.renderer.gpu_samples_us(),"window_dpi":windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd),"system_dpi":windows::Win32::UI::HiDpi::GetDpiForSystem()});
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
            // `--edit-pin-demo` drives the session itself: one scripted
            // annotation, the composed-frame check that a pinned picture is drawn
            // without a capture region's mask and chrome, then the same write-back
            // 「钉住」 performs. It reports JSON and needs no message loop.
            if pin_demo {
                let canvas = canvases
                    .first_mut()
                    .ok_or("no canvas for the pin edit session")?;
                let report = canvas.pin_edit_demo()?;
                println!("{}", serde_json::to_string(&report)?);
                std::io::stdout().flush()?;
            }
            // The demo needs the message loop too: the session advances on a timer
            // and finishes when the driver sends Enter.
            if (!smoke && !pin_demo) || demo.is_some() {
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
                                UiCommand::Paint(paint) => {
                                    for c in &mut canvases {
                                        c.apply_paint(&paint);
                                    }
                                    // The floating windows show the values the
                                    // canvas will actually draw with.
                                    if let Some(c) = canvases
                                        .iter()
                                        .find(|c| c.hwnd.0 as usize == *active.lock())
                                    {
                                        publish_paint(c);
                                    }
                                }
                                UiCommand::ResumePinEditing => {
                                    // Only a fixed pinned image is brought back;
                                    // a capture session waits for a selection to
                                    // place its toolbar.
                                    if let Some(c) = canvases.iter_mut().find(|c| {
                                        resume_applies(c.fixed_image(), c.long_capture.is_some())
                                    }) {
                                        c.resume_pin_editing();
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
/// Captured desktop bounds, monitor DPI, and the canvas thread id.
#[derive(Clone, Copy)]
pub(super) struct CaptureReady {
    pub(super) thread_id: u32,
    pub(super) bounds: RECT,
    pub(super) dpi: u32,
}
/// Reflects the active document's history in the toolbar, so undo and redo only
/// look available when there is something to reverse.
fn publish_history(canvas: &Canvas) {
    let Some(weak) = canvas.ui_slots.toolbar() else {
        return;
    };
    snaplight_ui::set_toolbar_history(weak, canvas.history.can_undo(), canvas.history.can_redo());
    // The toolbar acts on the selection, so it only exists while there is one:
    // undoing the selection brings the capture back to the empty state.
    if let Some(selection) = canvas.doc.selection {
        publish_selection(canvas, selection);
    } else if canvas.fixed_image() {
        // A pinned image has no selection to follow, but its session still needs
        // the toolbar — it is what offers the tools and the colours.
        place_toolbar_for_pin(canvas);
    } else {
        park_toolbar(canvas);
    }
    // A finished long capture returns to the editor with the same tool, so the
    // floating paint windows have to come back with it.
    publish_paint(canvas);
}
/// Reflects the active tool's paint in the floating windows, so the options
/// panel and the bottom bar show the values the canvas will draw with.
fn publish_paint(canvas: &Canvas) {
    let paint = canvas.paint();
    let mut stroke_tools = Vec::new();
    let mut fill_tools = Vec::new();
    for tool in Tool::ALL {
        let style = canvas.style_of(tool);
        stroke_tools.push(style.stroke_on);
        fill_tools.push(style.fill_on);
    }
    if let Some(weak) = canvas.ui_slots.toolbar() {
        snaplight_ui::set_toolbar_paint(weak, &paint, stroke_tools, fill_tools);
    }
    if let Some(panel) = canvas.ui_slots.panel() {
        snaplight_ui::set_panel_paint(panel, &paint);
    }
    publish_paint_bar(canvas, &paint);
}
/// Shows the bottom paint bar for every tool that paints and takes it away for
/// the select tool, which has nothing to paint. The bar is only as wide as the
/// active tool's row and sits at the bottom centre of the capture, except in a
/// pinned session where it forms a pair with the toolbar.
fn publish_paint_bar(canvas: &Canvas, paint: &snaplight_ui::ToolPaint) {
    let Some(weak) = canvas.ui_slots.paint_bar() else {
        return;
    };
    if canvas.tool == Tool::Select || canvas.long_capture.is_some() {
        snaplight_ui::hide_paint_bar(weak);
        return;
    }
    snaplight_ui::set_paint_bar(weak, paint);
    let size = snaplight_ui::paint_bar_logical_size(paint.tool);
    let physical = snaplight_ui::scale_to_physical(size, canvas.dpi);
    // A pinned image places the bar as part of the floating pair, so it never
    // overlaps the toolbar; a capture keeps it at the bottom centre.
    let (x, y) = if canvas.fixed_image() {
        pin_pair_positions(canvas, physical).1
    } else {
        paint_bar_position(canvas.screen, physical)
    };
    // The row changes width with the tool, so the window is reshaped when the
    // tool changes rather than on every update.
    let reshaped = canvas.ui_slots.paint_tool_changed(paint.tool);
    let weak = weak.clone();
    let dpi = canvas.dpi;
    snaplight_ui::invoke(move || {
        if let Some(bar) = weak.upgrade() {
            if reshaped {
                bar.set_row_width(size.0 as f32);
            }
            if reshaped || !bar.window().is_visible() {
                bar.window().set_position(PhysicalPosition::new(x, y));
                let _ = bar.show();
            }
        }
        if reshaped {
            reshape_when_sized(snaplight_ui::PAINT_BAR_TITLE, size, dpi);
        }
    });
}
/// Hides the windows that hang off the toolbar. Quick settings and the picker
/// never outlive the selection they were opened against.
fn hide_overlays(canvas: &Canvas) {
    if let Some(panel) = canvas.ui_slots.panel() {
        snaplight_ui::hide_panel(panel);
    }
    if let Some(picker) = canvas.ui_slots.picker() {
        snaplight_ui::hide_picker(picker);
    }
}
fn park_toolbar(canvas: &Canvas) {
    canvas.ui_slots.clear_selection_cache();
    // Parked windows sit outside the virtual screen, so they cannot sit over a
    // region either.
    canvas.ui_slots.set_toolbar_rect(None);
    if let Some(weak) = canvas.ui_slots.toolbar() {
        let size =
            snaplight_ui::scale_to_physical(canvas.ui_slots.toolbar_size().unwrap(), canvas.dpi);
        let (x, y) = parked_position(size);
        snaplight_ui::move_toolbar(weak, PhysicalPosition::new(x, y));
    }
    hide_overlays(canvas);
}
/// Moves the toolbar under the completed selection and reports its pixel size.
fn publish_selection(canvas: &Canvas, selection: ImageRect) {
    if !canvas.ui_slots.selection_changed(selection) {
        return;
    }
    // Quick settings belong to the previous toolbar position.
    hide_overlays(canvas);
    let Some(weak) = canvas.ui_slots.toolbar() else {
        return;
    };
    let anchor = RECT {
        left: canvas.screen.left + selection.x as i32,
        top: canvas.screen.top + selection.y as i32,
        right: canvas.screen.left + (selection.x + selection.width) as i32,
        bottom: canvas.screen.top + (selection.y + selection.height) as i32,
    };
    let size = snaplight_ui::scale_to_physical(canvas.ui_slots.toolbar_size().unwrap(), canvas.dpi);
    let (x, y) = follow_position(canvas.screen, anchor, size);
    canvas.ui_slots.set_toolbar_rect(Some(RECT {
        left: x,
        top: y,
        right: x + size.0,
        bottom: y + size.1,
    }));
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
    let logical_size = canvas.ui_slots.toolbar_size().unwrap();
    snaplight_ui::invoke(move || reshape_and_raise(snaplight_ui::TOOLBAR_TITLE, logical_size));
}
/// Whether the resume command applies to a canvas: only a fixed pinned image has
/// no selection that would place its toolbar. A capture session stays parked
/// until one places it, and a long capture owns its own chrome.
fn resume_applies(fixed_image: bool, long_capture: bool) -> bool {
    fixed_image && !long_capture
}
/// The tool whose paint bar is on screen: the select tool has nothing to paint,
/// so its bar belongs to the last drawing tool.
fn pin_paint_tool(canvas: &Canvas) -> Tool {
    if canvas.tool == Tool::Select {
        canvas.last_draw_tool
    } else {
        canvas.tool
    }
}
/// Physical size of the paint bar the pinned session's active tool needs.
fn pin_paint_size(canvas: &Canvas) -> (i32, i32) {
    snaplight_ui::scale_to_physical(
        snaplight_ui::paint_bar_logical_size(pin_paint_tool(canvas).id()),
        canvas.dpi,
    )
}
/// Toolbar and paint bar positions around a pinned image, computed as one pair
/// so the two floating windows never overlap. `paint` is the paint bar's
/// physical size; the toolbar keeps its own size even when wider than the pin.
fn pin_pair_positions(canvas: &Canvas, paint: (i32, i32)) -> ((i32, i32), (i32, i32)) {
    let toolbar = snaplight_ui::scale_to_physical(
        canvas.ui_slots.toolbar_size().unwrap_or_default(),
        canvas.dpi,
    );
    pin_floating_positions(
        canvas.screen,
        toolbar,
        paint,
        monitor_work_area(canvas.screen),
    )
}
/// Places the toolbar as part of the floating pair around a pinned image.
///
/// A capture session follows its selection, so the toolbar is parked until there
/// is one. A pinned image is the whole document and has no selection, so without
/// this the toolbar would stay parked off-screen and the session would offer no
/// tools at all. The toolbar is placed as part of the floating pair, so it never
/// lands on the paint bar.
fn place_toolbar_for_pin(canvas: &Canvas) {
    let whole = ImageRect {
        x: 0.0,
        y: 0.0,
        width: canvas.doc.image.width as f32,
        height: canvas.doc.image.height as f32,
    };
    if !canvas.ui_slots.selection_changed(whole) {
        return;
    }
    let Some(weak) = canvas.ui_slots.toolbar() else {
        return;
    };
    // The pair is laid out against the pin: the toolbar may reach beyond a small
    // pin, but it starts where the picture does and stays clear of the bar.
    let (x, y) = pin_pair_positions(canvas, pin_paint_size(canvas)).0;
    snaplight_ui::move_toolbar(weak, PhysicalPosition::new(x, y));
    let logical_size = canvas.ui_slots.toolbar_size().unwrap();
    snaplight_ui::invoke(move || reshape_and_raise(snaplight_ui::TOOLBAR_TITLE, logical_size));
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
    fn scrubbing_moves_a_value_by_pointer_distance() {
        // Two pixels of movement are one pixel of outline width, and the value
        // never leaves its range.
        assert_eq!(scrub_value(ScrubSlot::StrokeWidth, 3.0, 0.0), 3.0);
        assert_eq!(scrub_value(ScrubSlot::StrokeWidth, 3.0, 8.0), 7.0);
        assert_eq!(scrub_value(ScrubSlot::StrokeWidth, 3.0, -8.0), 1.0);
        assert_eq!(scrub_value(ScrubSlot::StrokeWidth, 60.0, 400.0), 64.0);
        assert_eq!(scrub_value(ScrubSlot::StrokeWidth, 3.0, -400.0), 1.0);
        // The fill opacity covers 0..=100 and the text size 12..=200.
        assert_eq!(scrub_value(ScrubSlot::FillOpacity, 50.0, 25.0), 60.0);
        assert_eq!(scrub_value(ScrubSlot::FillOpacity, 95.0, 400.0), 100.0);
        assert_eq!(scrub_value(ScrubSlot::FillOpacity, 5.0, -400.0), 0.0);
        assert_eq!(scrub_value(ScrubSlot::FontSize, 32.0, 15.0), 42.0);
        assert_eq!(scrub_value(ScrubSlot::FontSize, 32.0, -400.0), 12.0);
        assert_eq!(scrub_value(ScrubSlot::FontSize, 180.0, 400.0), 200.0);
        // Every slot reads and writes its own field, and none of them is an
        // alias: adjusting the fill keeps the outline width.
        let mut style = Style::default();
        for (slot, value) in [
            (ScrubSlot::StrokeWidth, 9.0),
            (ScrubSlot::FillOpacity, 40.0),
            (ScrubSlot::FontSize, 72.0),
        ] {
            slot.set(&mut style, value);
            assert_eq!(slot.value(style), value);
        }
        assert_eq!(style.width, 9.0);
        assert_eq!(style.fill[3], 0.4);
        assert_eq!(style.font_size, 72.0);
        assert_eq!(scrub_value(ScrubSlot::StrokeWidth, style.width, 0.0), 9.0);
        // The readout names the value it is changing and its unit.
        assert_eq!(ScrubSlot::StrokeWidth.hud(6.4), "描边宽度 6 px");
        assert_eq!(ScrubSlot::FillOpacity.hud(64.5), "填充不透明度 65%");
        assert_eq!(ScrubSlot::FontSize.hud(32.0), "字号 32 px");
    }
    #[test]
    fn the_resume_command_scopes_to_a_fixed_pin_session() {
        // The pinned image comes back; a capture session waits for a selection
        // that places its toolbar, and a long capture owns its own chrome.
        assert!(resume_applies(true, false));
        assert!(!resume_applies(false, false));
        assert!(!resume_applies(true, true));
    }
    #[test]
    fn configured_styles_follow_the_tool_lists() {
        let config = Config::default();
        let styles = configured_styles(&config);
        let rectangle = styles[Tool::Rectangle.id() as usize];
        assert!(rectangle.stroke_on && !rectangle.fill_on);
        assert_eq!(rectangle.width, config.stroke_width);
        let text = styles[Tool::Text.id() as usize];
        assert!(!text.stroke_on && text.fill_on);
        assert_eq!(text.font_size, config.font_size as f32);
        // The fill carries the configured opacity, and a configuration that
        // switched a tool off keeps it off.
        let mut config = config;
        config.fill_opacity = 40;
        config.stroke_tools.retain(|tool| *tool != Tool::Rectangle);
        let styles = configured_styles(&config);
        let rectangle = styles[Tool::Rectangle.id() as usize];
        assert!(!rectangle.stroke_on);
        assert_eq!(rectangle.fill[3], 0.4);
        assert!(styles[Tool::Ellipse.id() as usize].stroke_on);
        // An unparsable colour falls back to the default instead of painting
        // with a zeroed brush.
        let styles = configured_styles(&Config {
            stroke_color: "#GGGGGG".into(),
            ..Config::default()
        });
        assert_eq!(
            styles[Tool::Rectangle.id() as usize].stroke,
            Style::default().stroke
        );
    }
}

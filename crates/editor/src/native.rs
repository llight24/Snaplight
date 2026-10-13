use parking_lot::Mutex;
use snaplight_domain::*;
use snaplight_platform::{
    atomic_write, capture_dir, data_dir, load_config, load_window_state, native::PointHole,
    read_pixels, save_config, save_window_state, write_pixels, CaptureService,
};
use snaplight_render_win::*;
use snaplight_ui::{
    ColorPicker, ComponentHandle, ModelRc, OptionsPanel, PaintBar, PhysicalPosition, Toolbar,
    VecModel,
};
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
/// Paint checks of the smoke frame: a filled shape at `FILLED`, the same colour
/// as an outline at `OUTLINED`, so the composed pixels prove that each paint is
/// applied where it should be and only there.
const FILL: [f32; 4] = [0.2, 0.85, 0.35, 1.0];
const FILLED: ImageRect = ImageRect {
    x: 300.0,
    y: 200.0,
    width: 120.0,
    height: 60.0,
};
const OUTLINED: ImageRect = ImageRect {
    x: 460.0,
    y: 200.0,
    width: 120.0,
    height: 60.0,
};
/// Text annotation of the smoke frame: filled and outlined, so its glyph
/// outlines have to produce ink in the box the check scans.
const SMOKE_TEXT: ImagePoint = ImagePoint { x: 320.0, y: 120.0 };
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
#[derive(Clone)]
enum UiCommand {
    Action(i32),
    /// One paint snapshot, from the tool options panel or the bottom paint bar.
    /// Both windows edit the same style, so they send the same message.
    Paint(snaplight_ui::ToolPaint),
}
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
/// Translates a selection by a pointer delta, keeping its size: the rectangle
/// stops at the image edges instead of being clipped, so a drag inside the
/// selection moves it rather than drawing a new one.
fn move_selection(rect: ImageRect, dx: f32, dy: f32, width: u32, height: u32) -> ImageRect {
    ImageRect {
        x: (rect.x + dx).clamp(0.0, (width as f32 - rect.width).max(0.0)),
        y: (rect.y + dy).clamp(0.0, (height as f32 - rect.height).max(0.0)),
        ..rect
    }
}
/// The region a press drags: inside the committed selection of the select tool
/// the move wins over drawing, so the capture range can be shifted instead of
/// being drawn again from scratch. Anywhere else the press starts a new region.
fn move_target(tool: Tool, selection: Option<ImageRect>, point: ImagePoint) -> Option<ImageRect> {
    if tool != Tool::Select {
        return None;
    }
    selection.filter(|rect| rect.contains(point))
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
            if let Some(weak) = self.ui_slots.toolbar.get() {
                snaplight_ui::set_toolbar_active(weak, self.tool.id());
            }
        }
        publish_paint(self);
        raise_floating_windows();
        unsafe { self.invalidate() };
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
            edit.write_back(&image, &bytes, &self.doc)?;
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
                            state
                                .doc
                                .selection
                                .and_then(|r| selection_handle(r, p, state.dpi, state.view.scale))
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
                                Some(SelectionHandle { col: 1, .. }) => IDC_SIZENS,
                                Some(SelectionHandle { row: 1, .. }) => IDC_SIZEWE,
                                Some(SelectionHandle { col, row }) if col == row => IDC_SIZENWSE,
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
/// Parses `x,y,w,h`, in physical screen pixels.
fn parse_rect(value: &str) -> Option<RECT> {
    let numbers: Vec<i32> = value
        .split(',')
        .map(|part| part.trim().parse().ok())
        .collect::<Option<_>>()?;
    match numbers[..] {
        [x, y, w, h] if w > 0 && h > 0 => Some(RECT {
            left: x,
            top: y,
            right: x + w,
            bottom: y + h,
        }),
        _ => None,
    }
}
/// A pinned image re-opened for annotation, from `--edit-pin=<image path>`.
///
/// The image belongs to the file group the editor writes when a capture is
/// pinned: `<stem>.bgra` is the flattened picture the pin shows,
/// `<stem>.document.json` keeps the annotations and `<stem>.source.bgra` the
/// pixels the capture started from. Editing the document instead of the
/// flattened picture is what makes a second annotation pass non-destructive;
/// the picture is the fallback for a pin that was saved without them.
///
/// `--pin-rect=x,y,w,h` is the pin's window on screen, so the session opens over
/// the pin instead of covering the desktop, and `--edit-pin-demo=<image path>`
/// runs the same session without a user: it annotates one scripted rectangle and
/// writes the result back, so the round trip can be verified.
struct PinEdit {
    /// Window rectangle of the pin, in screen pixels.
    rect: RECT,
    /// The pin's files without their extensions.
    stem: PathBuf,
    /// `--edit-pin-demo`: no UI, scripted annotation and write-back.
    demo: bool,
}
impl PinEdit {
    fn from_args() -> std::result::Result<Option<Self>, Box<dyn std::error::Error>> {
        let mut image = None;
        let mut demo = false;
        let mut rect = None;
        for arg in std::env::args() {
            if let Some(value) = arg.strip_prefix("--edit-pin=") {
                image = Some(value.to_owned());
                demo = false;
            } else if let Some(value) = arg.strip_prefix("--edit-pin-demo=") {
                image = Some(value.to_owned());
                demo = true;
            } else if let Some(value) = arg.strip_prefix("--pin-rect=") {
                rect = parse_rect(value);
            }
        }
        let Some(image) = image else {
            return Ok(None);
        };
        let rect = rect.ok_or("--edit-pin needs --pin-rect=x,y,w,h to know where the pin is")?;
        Ok(Some(Self {
            rect,
            stem: PathBuf::from(image).with_extension(""),
            demo,
        }))
    }
    /// The picture the pin shows: what the session writes back.
    fn image_path(&self) -> PathBuf {
        self.stem.with_extension("bgra")
    }
    /// The document with the annotations, so a later edit still has them.
    fn document_path(&self) -> PathBuf {
        self.stem.with_extension("document.json")
    }
    /// The flattened document the pin process reads when it opens a pin.
    fn pin_document_path(&self) -> PathBuf {
        self.stem.with_extension("pin.json")
    }
    /// The pixels the capture started from, kept so annotations stay editable.
    fn source_path(&self) -> PathBuf {
        self.stem.with_extension("source.bgra")
    }
    /// Loads the pixels and the document to annotate.
    fn load(
        &self,
    ) -> std::result::Result<(ImageDescriptor, Vec<u8>, Document), Box<dyn std::error::Error>> {
        let (image, base) =
            read_pixels(&self.source_path()).or_else(|_| read_pixels(&self.image_path()))?;
        let document = match std::fs::read(self.document_path()) {
            Ok(bytes) => serde_json::from_slice::<Document>(&bytes)?,
            // A pin saved without a document still opens: it is a picture, with
            // no annotations to restore.
            Err(_) => Document {
                version: 1,
                image: image.clone(),
                selection: None,
                annotations: vec![],
            },
        };
        if document.version != 1 {
            return Err("unsupported pin document version".into());
        }
        if document.image.width != image.width || document.image.height != image.height {
            return Err(format!(
                "pin document {} describes a {}x{} image, the pixels are {}x{}",
                self.document_path().display(),
                document.image.width,
                document.image.height,
                image.width,
                image.height
            )
            .into());
        }
        Ok((image, base, document))
    }
    /// Writes the annotated picture back into the pin's file group. The pin
    /// window keeps its place and its zoom; these files are the handoff, and the
    /// pin process reloads the picture when the session ends.
    fn write_back(
        &self,
        image: &ImageDescriptor,
        bytes: &[u8],
        document: &Document,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        write_pixels(&self.image_path(), image, bytes)?;
        let flattened = Document {
            version: 1,
            image: image.clone(),
            selection: None,
            annotations: vec![],
        };
        atomic_write(&self.pin_document_path(), &serde_json::to_vec(&flattened)?)?;
        atomic_write(&self.document_path(), &serde_json::to_vec_pretty(document)?)?;
        // A pin that had no source keeps the picture it started from, so the
        // next session is editable even though this one was not handed one.
        if !self.source_path().exists() {
            write_pixels(&self.source_path(), image, bytes)?;
        }
        Ok(())
    }
}
/// What this editor process was asked to do: the verification switches, the long
/// capture demo region, and the pinned image session if there is one.
struct EditorSession {
    /// `--smoke`: scripted checks against a synthetic frame instead of the user.
    smoke: bool,
    /// `--stress`: the 4K frame loop, which implies the smoke frame.
    stress: bool,
    /// `--long-capture-demo=x,y,w,h`: one long capture against a fixed region.
    long_capture: Option<RECT>,
    /// `--edit-pin=<image>`: annotate a pinned image in place.
    pin_edit: Option<PinEdit>,
}
fn canvas_thread(
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
                frames.push(Gpu::new(None)?.frame_from_bgra(image, &base, edit.rect)?);
                pin_document = Some((base, document));
            } else {
                let capture_started = std::time::Instant::now();
                frames = capture_with_retry()?;
                capture_us = capture_started.elapsed().as_micros() as u64;
            }
            // The scripted pin session drives itself and needs no message loop.
            let pin_demo = pin_edit.as_ref().is_some_and(|edit| edit.demo);
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
                            image: frame.descriptor.clone(),
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
    /// Bottom paint bar and colour picker; both outlive the toolbar position.
    paint_bar: OnceLock<snaplight_ui::PaintBarWeak>,
    picker: OnceLock<snaplight_ui::PickerWeak>,
    toolbar_size: OnceLock<(u32, u32)>,
    toolbar_selection: Mutex<Option<ImageRect>>,
    /// Physical screen rectangle of the placed toolbar, so a long capture can
    /// keep its own pill out of the region it stitches.
    toolbar_rect: Mutex<Option<RECT>>,
    /// Tool the paint bar is currently sized for, so its width changes only when
    /// the tool does.
    bar_tool: Mutex<Option<i32>>,
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
fn reshape_when_sized(title: &'static str, logical_size: (u32, u32), dpi: u32) {
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
fn reshape_and_raise(title: &'static str, logical_size: (u32, u32)) {
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
    if let Some(weak) = canvas.ui_slots.toolbar.get() {
        snaplight_ui::set_toolbar_paint(weak, &paint, stroke_tools, fill_tools);
    }
    if let Some(panel) = canvas.ui_slots.panel.get() {
        snaplight_ui::set_panel_paint(panel, &paint);
    }
    publish_paint_bar(canvas, &paint);
}
/// Shows the bottom paint bar for every tool that paints and takes it away for
/// the select tool, which has nothing to paint. The bar is only as wide as the
/// active tool's row and sits at the bottom centre of the capture.
fn publish_paint_bar(canvas: &Canvas, paint: &snaplight_ui::ToolPaint) {
    let Some(weak) = canvas.ui_slots.paint_bar.get() else {
        return;
    };
    if canvas.tool == Tool::Select || canvas.long_capture.is_some() {
        snaplight_ui::hide_paint_bar(weak);
        return;
    }
    snaplight_ui::set_paint_bar(weak, paint);
    let size = snaplight_ui::paint_bar_logical_size(paint.tool);
    let (x, y) = paint_bar_position(
        canvas.screen,
        snaplight_ui::scale_to_physical(size, canvas.dpi),
    );
    // The row changes width with the tool, so the window is reshaped when the
    // tool changes rather than on every update.
    let reshaped = {
        let mut last = canvas.ui_slots.bar_tool.lock();
        let changed = *last != Some(paint.tool);
        *last = Some(paint.tool);
        changed
    };
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
/// Bottom centre of the captured desktop: under the drawing hand but clear of
/// the toolbar, which parks in the bottom left corner.
fn paint_bar_position(screen: RECT, size: (i32, i32)) -> (i32, i32) {
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
/// Hides the windows that hang off the toolbar. Quick settings and the picker
/// never outlive the selection they were opened against.
fn hide_overlays(canvas: &Canvas) {
    if let Some(panel) = canvas.ui_slots.panel.get() {
        snaplight_ui::hide_panel(panel);
    }
    if let Some(picker) = canvas.ui_slots.picker.get() {
        snaplight_ui::hide_picker(picker);
    }
}
/// Puts the floating windows back in front of the capture overlay. Clicking the
/// capture activates it, and an activated topmost window takes the top of the
/// topmost band, which would leave the toolbar and the paint bar behind the
/// full-screen overlay: visible to Slint, unreachable for the pointer.
fn raise_floating_windows() {
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
    hide_overlays(canvas);
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
    hide_overlays(canvas);
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
/// Places the toolbar inside a pinned image's window.
///
/// A capture session follows its selection, so the toolbar is parked until there
/// is one. A pinned image is the whole document and has no selection, so without
/// this the toolbar would stay parked off-screen and the session would offer no
/// tools at all.
fn place_toolbar_for_pin(canvas: &Canvas) {
    let whole = ImageRect {
        x: 0.0,
        y: 0.0,
        width: canvas.doc.image.width as f32,
        height: canvas.doc.image.height as f32,
    };
    {
        let mut displayed = canvas.ui_slots.toolbar_selection.lock();
        if *displayed == Some(whole) {
            return;
        }
        *displayed = Some(whole);
    }
    let Some(weak) = canvas.ui_slots.toolbar.get() else {
        return;
    };
    let size =
        snaplight_ui::scale_to_physical(*canvas.ui_slots.toolbar_size.get().unwrap(), canvas.dpi);
    // Bottom-left of the pin: the toolbar is a window of its own and may reach
    // beyond a small pin, but it starts where the picture does.
    let (x, y) = initial_toolbar_position(canvas.screen, size);
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
/// Position of a top-level window in the desktop z-order, counting from the
/// front. Used by the smoke check that the floating windows stay in front of the
/// capture overlay.
fn z_order(hwnd: HWND) -> Option<usize> {
    unsafe {
        let mut current = GetTopWindow(None).ok()?;
        for index in 0..2000 {
            if current == hwnd {
                return Some(index);
            }
            current = GetWindow(current, GW_HWNDNEXT).ok()?;
        }
    }
    None
}
/// Window procedure of the smoke stand-in for the capture overlay. It only has
/// to exist: the check compares z-order, not behaviour.
unsafe extern "system" fn overlay_stand_in_proc(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}
/// Extent of the paint row in a window snapshot, in the snapshot's own pixels.
/// The window is rendered wider than the row needs, so the row sits in the middle
/// with the panel's close button at the right edge; `right_margin` keeps that
/// button out of the measurement. Anything that is not white counts as painted,
/// which covers the light borders of spin boxes and text fields.
fn row_extent(bytes: &[u8], width: u32, height: u32, right_margin: u32) -> Option<(u32, u32)> {
    // The band is a strip through the middle of the row: the rounded window
    // border and the transparent corners are outside it, so counting them as
    // painted would stretch the measurement to the whole window.
    let band = (height / 2 - 6)..(height / 2 + 6);
    let limit = width.saturating_sub(right_margin);
    let painted = |x: u32| {
        band.clone().any(|y| {
            let n = (y * width + x) as usize * 4;
            bytes[n] < 250 || bytes[n + 1] < 250 || bytes[n + 2] < 250
        })
    };
    // Four columns are skipped at the edges for the same reason.
    let left = (4..limit).find(|x| painted(*x))?;
    let right = (4..limit).rev().find(|x| painted(*x))?;
    (right >= left).then_some((left, right))
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
/// Clamps one shadow metric from the settings page into the range the pin
/// process sizes its window by, so a value the page cannot produce still cannot
/// leave the configuration in a form the loader rejects.
fn clamp_shadow_metric(value: i32, max: u32) -> u32 {
    value.clamp(0, max as i32) as u32
}

/// Tells the pin process that the configuration changed.
///
/// Pins live in their own process and only read the configuration file, so
/// nothing would reach a pin that is already on screen without a word from
/// here. A settings window is launched by the core exactly like a capture
/// editor is, which makes its standard output the channel the core forwards
/// `PinCommand`s over.
fn tell_pins_to_reload() {
    let line = match serde_json::to_string(&Envelope::new(PinCommand::Reload)) {
        Ok(line) => line,
        Err(error) => {
            eprintln!("could not encode the pin reload: {error}");
            return;
        }
    };
    let printed = writeln!(std::io::stdout(), "{line}").and_then(|()| std::io::stdout().flush());
    if let Err(error) = printed {
        // The settings are saved either way: pins that are already open keep
        // the shadow they have, and the next pin gets the new one.
        eprintln!("could not tell the pin process about the new settings: {error}");
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
    // The per-tool paint switches are edited one cell at a time; the model is
    // rebuilt from the resulting configuration so the row shows its new state.
    let rows_source = config.clone();
    let weak = ui.as_weak();
    ui.on_paint_toggled(move |tool, slot, on| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let Some(tool) = Tool::from_id(tool) else {
            return;
        };
        let (mut stroke, mut fill) = snaplight_ui::paint_tools(&ui.get_paint_tools());
        let list = if slot == 1 { &mut fill } else { &mut stroke };
        list.retain(|listed| *listed != tool);
        if on {
            list.push(tool);
        }
        let mut edited = rows_source.clone();
        edited.stroke_tools = stroke;
        edited.fill_tools = fill;
        ui.set_paint_tools(ModelRc::new(VecModel::from(snaplight_ui::tool_paint_rows(
            &edited,
        ))));
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
        // The pin prompt follows this switch, and it also turns the switch off
        // when the user ticks "don't ask again" there.
        c.pin_close_confirm = ui.get_pin_close_confirm();
        c.pin_shadow = ui.get_pin_shadow();
        c.pin_shadow_blur = clamp_shadow_metric(ui.get_pin_shadow_blur(), MAX_PIN_SHADOW_BLUR);
        c.pin_shadow_offset =
            clamp_shadow_metric(ui.get_pin_shadow_offset(), MAX_PIN_SHADOW_OFFSET);
        c.pin_shadow_opacity = clamp_shadow_metric(ui.get_pin_shadow_opacity(), 100) as u8;
        c.fill_opacity = ui.get_fill_opacity().clamp(0, 100) as u8;
        c.font_size = ui
            .get_font_size()
            .clamp(MIN_FONT_SIZE as i32, MAX_FONT_SIZE as i32) as u16;
        // Colour text is normalized before it is written, so a hand-typed value
        // never leaves the configuration in a form the loader rejects.
        for (text, target) in [
            (ui.get_stroke_color_value().to_string(), &mut c.stroke_color),
            (ui.get_fill_color_value().to_string(), &mut c.fill_color),
        ] {
            let Some(rgba) = parse_hex_color(&text) else {
                ui.set_status("颜色需要写成 #RRGGBB，未保存任何更改".into());
                return;
            };
            *target = format_hex_color(rgba);
        }
        let (stroke_tools, fill_tools) = snaplight_ui::paint_tools(&ui.get_paint_tools());
        c.stroke_tools = stroke_tools;
        c.fill_tools = fill_tools;
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
                // Pins run in their own process, which reads the configuration
                // file. The shadow is presentation, so a change there has to
                // reach pins that are already on screen instead of waiting for
                // the next one; this channel is how core forwards commands to
                // that process.
                tell_pins_to_reload();
                ui.set_status("已保存".into());
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
    // `--edit-pin=<image>` re-opens a pinned image for annotation inside a window
    // over the pin itself; `--edit-pin-demo` does the same without a user.
    let pin_edit = PinEdit::from_args()?;
    // The demos run without the Slint toolbar: they drive the canvas directly and
    // exit when they are done.
    let interactive = !smoke && demo.is_none() && !pin_edit.as_ref().is_some_and(|edit| edit.demo);
    // A pinned image session keeps its own toolbar placement, inside the pin, and
    // must not hand that position on to the next capture session.
    let pin_session = pin_edit.is_some();
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
            EditorSession {
                smoke,
                stress,
                long_capture: demo,
                pin_edit,
            },
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
        // A pinned image session has no selection to follow, so its toolbar
        // starts inside the pin; a capture session waits off-screen until the
        // selection places it.
        let start = if pin_session {
            initial_toolbar_position(ready.bounds, size)
        } else {
            parked
        };
        ui.window()
            .set_position(PhysicalPosition::new(start.0, start.1));
        ui.show()?;
        // The window has its final size once it is shown, so the pill shape and
        // the tool window style are applied now and never again.
        reshape_and_raise(
            snaplight_ui::TOOLBAR_TITLE,
            snaplight_ui::toolbar_logical_size(&config.tools),
        );
        let panel: OptionsPanel = snaplight_ui::options_panel(&config)?;
        let _ = ui_slots.panel.set(panel.as_weak());
        // Before a tool is known, the row is the width of the shape tools; it is
        // resized when the panel or the bar opens for a tool.
        let default_size = snaplight_ui::paint_logical_size(Tool::Rectangle.id());
        reshape_and_raise(snaplight_ui::OPTIONS_TITLE, default_size);
        let bar: PaintBar = snaplight_ui::paint_bar(&config)?;
        let _ = ui_slots.paint_bar.set(bar.as_weak());
        reshape_and_raise(snaplight_ui::PAINT_BAR_TITLE, default_size);
        let picker: ColorPicker = snaplight_ui::color_picker()?;
        let _ = ui_slots.picker.set(picker.as_weak());
        reshape_and_raise(
            snaplight_ui::PICKER_TITLE,
            snaplight_ui::PICKER_LOGICAL_SIZE,
        );
        // The paint bar and the picker start hidden; the canvas shows them when a
        // drawing tool is selected and a colour is picked.
        snaplight_ui::hide_paint_bar(&bar.as_weak());
        snaplight_ui::hide_picker(&picker.as_weak());
        // Both paint surfaces edit the same style, so they send the same
        // message and the canvas answers with the values it will draw with.
        let push_paint = {
            let q = queue.clone();
            move |paint: snaplight_ui::ToolPaint| {
                q.lock().push(UiCommand::Paint(paint));
                unsafe {
                    let _ = PostThreadMessageW(id, COMMAND, WPARAM(0), LPARAM(0));
                }
            }
        };
        panel.on_properties_changed(push_paint.clone());
        bar.on_properties_changed(push_paint.clone());
        // Quick settings and the bottom bar open the colour picker for one of the
        // two colours; every pick is applied as it happens, so there is no
        // separate apply step to remember.
        let picker_target = Arc::new(Mutex::new(0i32));
        let open_picker = {
            let picker = picker.as_weak();
            let panel = panel.as_weak();
            let target = picker_target.clone();
            let dpi = ready.dpi;
            let bounds = ready.bounds;
            move |which: i32, anchor: PhysicalPosition, anchor_height: i32| {
                let (Some(picker), Some(panel)) = (picker.upgrade(), panel.upgrade()) else {
                    return;
                };
                *target.lock() = which;
                // The picker opens on the colour that window is already showing.
                let color = if which == 1 {
                    panel.get_fill_color()
                } else {
                    panel.get_stroke_color()
                };
                snaplight_ui::picker_set_color(&picker, color);
                picker.set_target_label(if which == 1 { "填充色" } else { "描边色" }.into());
                let size = snaplight_ui::scale_to_physical(snaplight_ui::PICKER_LOGICAL_SIZE, dpi);
                let (x, y) = options_position(bounds, anchor.x, anchor.y, anchor_height, size);
                picker.window().set_position(PhysicalPosition::new(x, y));
                let _ = picker.show();
                reshape_and_raise(
                    snaplight_ui::PICKER_TITLE,
                    snaplight_ui::PICKER_LOGICAL_SIZE,
                );
            }
        };
        let apply_picked = {
            let panel = panel.as_weak();
            let bar = bar.as_weak();
            let target = picker_target.clone();
            let push = push_paint.clone();
            move |color: snaplight_ui::Color| {
                let (Some(panel), Some(bar)) = (panel.upgrade(), bar.upgrade()) else {
                    return;
                };
                // The other values come from the panel, which the canvas keeps
                // current, so a pick cannot undo a change made meanwhile.
                let mut paint = panel.get_paint();
                if *target.lock() == 1 {
                    paint.fill_color = color;
                    panel.set_fill_color(color);
                    bar.set_fill_color(color);
                } else {
                    paint.stroke_color = color;
                    panel.set_stroke_color(color);
                    bar.set_stroke_color(color);
                }
                push(paint);
            }
        };
        let panel_pick = open_picker.clone();
        let anchor = panel.as_weak();
        panel.on_pick_color(move |which| {
            if let Some(panel) = anchor.upgrade() {
                let position = panel.window().position();
                panel_pick(which, position, panel.window().size().height as i32);
            }
        });
        let bar_pick = open_picker.clone();
        let anchor = bar.as_weak();
        bar.on_pick_color(move |which| {
            if let Some(bar) = anchor.upgrade() {
                let position = bar.window().position();
                bar_pick(which, position, bar.window().size().height as i32);
            }
        });
        let picker_weak = picker.as_weak();
        let apply = apply_picked.clone();
        picker.on_hue_picked(move |hue| {
            if let Some(picker) = picker_weak.upgrade() {
                let color = snaplight_ui::picker_set_hue(&picker, hue);
                apply(color);
            }
        });
        let picker_weak = picker.as_weak();
        let apply = apply_picked.clone();
        picker.on_sv_picked(move |saturation, brightness| {
            if let Some(picker) = picker_weak.upgrade() {
                let color = snaplight_ui::picker_set_sv(&picker, saturation, brightness);
                apply(color);
            }
        });
        let picker_weak = picker.as_weak();
        let apply = apply_picked.clone();
        picker.on_hex_edited(move |text| {
            if let Some(picker) = picker_weak.upgrade() {
                // Unparsable text is left in place so it can be corrected; the
                // last usable colour stays applied.
                if let Some(color) = snaplight_ui::picker_set_hex(&picker, text.as_str()) {
                    apply(color);
                }
            }
        });
        let picker_weak = picker.as_weak();
        let apply = apply_picked.clone();
        picker.on_preset_picked(move |preset| {
            if let Some(picker) = picker_weak.upgrade() {
                let color = snaplight_ui::picker_set_color(&picker, preset);
                apply(color);
            }
        });
        let picker_handles = picker.as_weak();
        let dismissed = panel.as_weak();
        panel.on_dismissed(move || {
            if let Some(panel) = dismissed.upgrade() {
                let _ = panel.hide();
            }
            if let Some(picker) = picker_handles.upgrade() {
                let _ = picker.hide();
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
            // The panel opens with the values the canvas is drawing with; the
            // toolbar mirrors them because it is the only window the canvas can
            // reach from the render thread without blocking.
            snaplight_ui::set_panel_paint(
                &panel.as_weak(),
                &snaplight_ui::toolbar_paint(&toolbar, tool),
            );
            let logical = snaplight_ui::paint_logical_size(tool);
            let panel_size = snaplight_ui::scale_to_physical(logical, panel_dpi);
            let bar_size = snaplight_ui::scale_to_physical(
                snaplight_ui::toolbar_logical_size(&config.tools),
                panel_dpi,
            );
            let position = toolbar.window().position();
            let (x, y) = options_position(bounds, position.x, position.y, bar_size.1, panel_size);
            panel.set_row_width(logical.0 as f32);
            panel.window().set_position(PhysicalPosition::new(x, y));
            let _ = panel.show();
            reshape_when_sized(snaplight_ui::OPTIONS_TITLE, logical, panel_dpi);
        });
        let q = queue.clone();
        ui.on_command(move |action| {
            q.lock().push(UiCommand::Action(action));
            unsafe {
                let _ = PostThreadMessageW(id, COMMAND, WPARAM(0), LPARAM(0));
            }
        });
        // The panel is created hidden and only shown when a tool asks for it.
        snaplight_ui::hide_panel(&panel.as_weak());
        let result = snaplight_ui::run_event_loop();
        // A parked toolbar was never placed by a selection, so its position is not
        // worth remembering — and neither is a pinned image session's, which sits
        // inside the pin rather than where a capture toolbar belongs.
        let position = ui.window().position();
        if !pin_session && (position.x, position.y) != parked {
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
        // The per-tool paint table has to survive the round trip through the page,
        // or saving the settings would rewrite the configured tool lists.
        ui.set_paint_tools(ModelRc::new(VecModel::from(snaplight_ui::tool_paint_rows(
            &config,
        ))));
        let (stroke_tools, fill_tools) = snaplight_ui::paint_tools(&ui.get_paint_tools());
        if stroke_tools != config.stroke_tools || fill_tools != config.fill_tools {
            return Err(format!(
                "the paint table round-tripped to {stroke_tools:?} and {fill_tools:?}, expected {:?} and {:?}",
                config.stroke_tools, config.fill_tools
            )
            .into());
        }
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
        // variant: rectangle (fill and outline), pen (outline width), mosaic
        // (block size) and text (font size plus the text field). Each variant is
        // first rendered wider than it needs, where the row lays out at its own
        // preferred size, to prove the configured width covers the row.
        const GENEROUS: u32 = 1400;
        let panel = snaplight_ui::options_panel(&config)?;
        for (tool, slug) in [(1, "rect"), (4, "pen"), (5, "mosaic"), (6, "text")] {
            let size = snaplight_ui::paint_logical_size(tool);
            panel.set_row_width(GENEROUS as f32);
            panel.set_tool(tool);
            panel.set_fill_on(tool != 4 && tool != 5);
            panel.show()?;
            // A width change reaches the native window on the next event-loop
            // turn, so the wide render has to settle before it is measured.
            snaplight_ui::run_event_loop_for(std::time::Duration::from_millis(80));
            let wide = panel.window().take_snapshot()?;
            // The snapshot and the painted extent are physical pixels while every
            // size below is logical, so the extent has to be measured at the
            // window's scale.
            let scale = panel.window().scale_factor().max(1.0);
            let (left, right) = row_extent(wide.as_bytes(), wide.width(), wide.height(), 52)
                .ok_or("the options panel painted nothing")?;
            let row = ((right - left + 1) as f32 / scale).ceil() as u32;
            // The row follows 14px of padding and is followed by the layout gap,
            // the close button and 8px of padding.
            let needed = 14 + row + 10 + 40 + 8;
            eprintln!(
                "options panel {slug}: row needs {needed} px, window {} px",
                size.0
            );
            if size.0 < needed {
                return Err(format!(
                    "the {slug} options panel is {} px wide but its row needs {needed} px",
                    size.0
                )
                .into());
            }
            // Slint applies the new width to the native window from the event
            // loop, so the review snapshot is taken after it has settled.
            panel.set_row_width(size.0 as f32);
            snaplight_ui::run_event_loop_for(std::time::Duration::from_millis(80));
            let snapshot = panel.window().take_snapshot()?;
            let bytes = snapshot.as_bytes();
            let physical = snaplight_ui::scale_to_physical(size, (scale * 96.0).round() as u32).0;
            if snapshot.width() != physical as u32 {
                return Err(format!(
                    "the {slug} options panel is {} physical pixels wide, expected {physical}",
                    snapshot.width(),
                )
                .into());
            }
            write_rgba_png(
                &data_dir().join(format!("options-smoke-{slug}.png")),
                snapshot.width(),
                snapshot.height(),
                bytes,
            )?;
            if tool == 1 {
                write_rgba_png(
                    &data_dir().join("options-smoke.png"),
                    snapshot.width(),
                    snapshot.height(),
                    bytes,
                )?;
            }
        }
        panel.hide()?;
        // The bottom paint bar stays visible while a drawing tool is selected, so
        // every row it can show has to render without clipping as well. The bar
        // carries the row alone, with no close button.
        let bar = snaplight_ui::paint_bar(&config)?;
        for (tool, slug) in [(1, "rect"), (5, "mosaic"), (6, "text")] {
            let size = snaplight_ui::paint_bar_logical_size(tool);
            bar.set_row_width(GENEROUS as f32);
            bar.set_tool(tool);
            bar.set_fill_on(tool != 5);
            bar.show()?;
            snaplight_ui::run_event_loop_for(std::time::Duration::from_millis(80));
            let wide = bar.window().take_snapshot()?;
            let scale = bar.window().scale_factor().max(1.0);
            let (left, right) = row_extent(wide.as_bytes(), wide.width(), wide.height(), 4)
                .ok_or("the paint bar painted nothing")?;
            let row = ((right - left + 1) as f32 / scale).ceil() as u32;
            // The bar has no close button: the row sits between two 14px paddings.
            let needed = 14 + row + 14;
            eprintln!(
                "paint bar {slug}: row needs {needed} px, window {} px",
                size.0
            );
            if size.0 < needed {
                return Err(format!(
                    "the {slug} paint bar is {} px wide but its row needs {needed} px",
                    size.0
                )
                .into());
            }
            bar.set_row_width(size.0 as f32);
            snaplight_ui::run_event_loop_for(std::time::Duration::from_millis(80));
            let snapshot = bar.window().take_snapshot()?;
            let bytes = snapshot.as_bytes();
            let physical = snaplight_ui::scale_to_physical(size, (scale * 96.0).round() as u32).0;
            if snapshot.width() != physical as u32 {
                return Err(format!(
                    "the {slug} paint bar is {} physical pixels wide, expected {physical}",
                    snapshot.width(),
                )
                .into());
            }
            write_rgba_png(
                &data_dir().join(format!("paint-bar-smoke-{slug}.png")),
                snapshot.width(),
                snapshot.height(),
                bytes,
            )?;
            if tool == 6 {
                write_rgba_png(
                    &data_dir().join("paint-bar-smoke.png"),
                    snapshot.width(),
                    snapshot.height(),
                    bytes,
                )?;
            }
        }
        // Slint creates its native windows from the event loop, and the place a
        // window ends up in the z-order is native state, so run the loop briefly
        // before the check below looks for the bar's window.
        snaplight_ui::run_event_loop_for(std::time::Duration::from_millis(200));
        // The capture overlay is topmost, and being activated puts it in front of
        // the other topmost windows. Raising the floating windows again has to
        // put the paint bar back in front of it, or its controls stop responding
        // after the first shape is drawn. A topmost stand-in for the overlay
        // reproduces that state without a second capture.
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = w!("Snaplight.ZOrderCheck");
            let description = WNDCLASSW {
                hInstance: instance.into(),
                lpszClassName: class,
                lpfnWndProc: Some(overlay_stand_in_proc),
                ..Default::default()
            };
            let _ = RegisterClassW(&description);
            let overlay = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                class,
                w!("capture"),
                WS_POPUP,
                0,
                0,
                200,
                100,
                None,
                None,
                instance,
                None,
            )?;
            let bar_hwnd = find_process_window(snaplight_ui::PAINT_BAR_TITLE)
                .ok_or("the paint bar window is missing")?;
            // The stand-in takes the front of the topmost band, as the activated
            // overlay does, and must be in front of the bar to be a fair test.
            SetWindowPos(
                overlay,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )?;
            let (bar_z, overlay_z) = (
                z_order(bar_hwnd).unwrap_or(usize::MAX),
                z_order(overlay).unwrap_or(usize::MAX),
            );
            if overlay_z > bar_z {
                return Err(
                    "the z-order check needs the capture stand-in in front of the paint bar".into(),
                );
            }
            raise_floating_windows();
            let (bar_z, overlay_z) = (
                z_order(bar_hwnd).unwrap_or(usize::MAX),
                z_order(overlay).unwrap_or(usize::MAX),
            );
            if bar_z > overlay_z {
                return Err(format!(
                    "the paint bar is at z-order {bar_z}, still behind the capture at {overlay_z}"
                )
                .into());
            }
            let _ = DestroyWindow(overlay);
        }
        bar.hide()?;
        // The colour picker is the only window with hand-drawn gradients, so its
        // appearance is recorded for the interface review.
        let picker = snaplight_ui::color_picker()?;
        picker.set_target_label("填充色".into());
        picker.show()?;
        let snapshot = picker.window().take_snapshot()?;
        write_rgba_png(
            &data_dir().join("picker-smoke.png"),
            snapshot.width(),
            snapshot.height(),
            snapshot.as_bytes(),
        )?;
        // A picked colour has to survive the round trip through the square, the
        // hex field and the slider, or the picker would show one colour and
        // apply another.
        let picked = snaplight_ui::picker_set_color(&picker, snaplight_ui::slint_color(FILL));
        if picker.get_color_value() != picked {
            return Err("the picker did not keep the colour it was given".into());
        }
        if picker.get_hex_text().as_str() != format_hex_color(FILL) {
            return Err(format!(
                "the picker shows {:?} instead of {}",
                picker.get_hex_text().as_str(),
                format_hex_color(FILL)
            )
            .into());
        }
        let green = snaplight_ui::slint_color([0.0, 1.0, 0.0, 1.0]);
        if snaplight_ui::picker_set_hex(&picker, "#00FF00").is_none()
            || picker.get_color_value() != green
        {
            return Err("the picker did not accept a hex colour".into());
        }
        if snaplight_ui::picker_set_hex(&picker, "not a colour").is_some()
            || picker.get_color_value() != green
        {
            return Err("invalid hex text changed the picked colour".into());
        }
        // The hue slider keeps saturation and brightness: half of each at a
        // quarter turn is the half-green tone below.
        snaplight_ui::picker_set_sv(&picker, 0.5, 0.5);
        let hue = snaplight_ui::rgba_of(snaplight_ui::picker_set_hue(&picker, 0.25));
        if (hue[0] - 0.375).abs() > 0.01
            || (hue[1] - 0.5).abs() > 0.01
            || (hue[2] - 0.25).abs() > 0.01
        {
            return Err(format!(
                "a quarter turn at half saturation and brightness is {hue:?}, expected [0.375, 0.5, 0.25]"
            )
            .into());
        }
        picker.hide()?;
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
    fn pin_rect_needs_four_positive_numbers() {
        let area = parse_rect("10, 20, 300, 400").expect("a valid rectangle");
        assert_eq!(
            (area.left, area.top, area.right, area.bottom),
            (10, 20, 310, 420)
        );
        // A window without a size cannot be placed, and trailing junk is a typo
        // rather than a rectangle.
        assert!(parse_rect("10,20,0,400").is_none());
        assert!(parse_rect("10,20,-3,400").is_none());
        assert!(parse_rect("10,20,300").is_none());
        assert!(parse_rect("10,20,300,400,5").is_none());
        assert!(parse_rect("10,20,300,abc").is_none());
    }
    #[test]
    fn pin_session_files_share_the_image_stem() {
        // These four files are the handoff with the pin process: the picture the
        // pin reloads when the session ends, the document with the annotations so
        // the next session still has them, and the pixels they were drawn over.
        let edit = PinEdit {
            rect: rect(0, 0, 10, 10),
            stem: PathBuf::from("C:/captures/1680000000000000000"),
            demo: true,
        };
        assert_eq!(
            edit.image_path(),
            PathBuf::from("C:/captures/1680000000000000000.bgra")
        );
        assert_eq!(
            edit.document_path(),
            PathBuf::from("C:/captures/1680000000000000000.document.json")
        );
        assert_eq!(
            edit.pin_document_path(),
            PathBuf::from("C:/captures/1680000000000000000.pin.json")
        );
        assert_eq!(
            edit.source_path(),
            PathBuf::from("C:/captures/1680000000000000000.source.bgra")
        );
    }
    #[test]
    fn pin_session_loads_the_document_and_falls_back_to_the_picture() {
        let dir = std::env::temp_dir().join("snaplight-pin-edit-load");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp folder");
        let descriptor = |id: ImageId| ImageDescriptor {
            id,
            width: 8,
            height: 4,
            stride: 32,
            format: PixelFormat::Bgra8,
            alpha: AlphaMode::Opaque,
            color_space: ColorSpace::Srgb,
        };
        let source = descriptor(ImageId(1));
        let picture = descriptor(ImageId(2));
        let edit = |dir: std::path::PathBuf| PinEdit {
            rect: rect(0, 0, 8, 4),
            stem: dir.join("123"),
            demo: false,
        };
        let edit = edit(dir.clone());
        write_pixels(&edit.source_path(), &source, &[7u8; 128]).expect("source pixels");
        // The picture the pin shows differs from the source, so the loader cannot
        // pass one test by returning the other.
        write_pixels(&edit.image_path(), &picture, &[9u8; 128]).expect("picture pixels");
        let annotation = Annotation {
            shape: Shape::Rectangle(ImageRect {
                x: 1.0,
                y: 1.0,
                width: 2.0,
                height: 2.0,
            }),
            style: Style::default(),
        };
        let document = Document {
            version: 1,
            image: source.clone(),
            selection: None,
            annotations: vec![annotation.clone()],
        };
        std::fs::write(
            edit.document_path(),
            serde_json::to_vec(&document).expect("document json"),
        )
        .expect("document");

        // The document and the pixels it was drawn over come back, so a second
        // pass over a pinned image is still editable.
        let (image, pixels, loaded) = edit.load().expect("the session loads");
        assert_eq!(image.id, ImageId(1));
        assert_eq!(pixels, vec![7u8; 128]);
        assert_eq!(loaded.annotations.len(), 1);
        assert_eq!(loaded.version, 1);

        // A pin saved without a document still opens on the pixels the capture
        // started from, with no annotations to restore.
        std::fs::remove_file(edit.document_path()).expect("remove document");
        let (image, pixels, loaded) = edit.load().expect("the session loads");
        assert_eq!(image.id, ImageId(1));
        assert_eq!(pixels, vec![7u8; 128]);
        assert!(loaded.annotations.is_empty());

        // Without a source either, the picture the pin is showing becomes the
        // base: the pass is still possible, only the earlier annotations are gone.
        std::fs::remove_file(edit.source_path()).expect("remove source");
        let (image, pixels, loaded) = edit.load().expect("the session loads");
        assert_eq!(image.id, ImageId(2));
        assert_eq!(pixels, vec![9u8; 128]);
        assert!(loaded.annotations.is_empty());
        assert_eq!(loaded.image.id, ImageId(2));

        // A document describing a different image would annotate the wrong
        // pixels, so the mismatch is refused instead of being drawn anyway.
        let wrong = Document {
            image: ImageDescriptor {
                width: 16,
                ..descriptor(ImageId(3))
            },
            annotations: vec![annotation],
            ..loaded
        };
        std::fs::write(
            edit.document_path(),
            serde_json::to_vec(&wrong).expect("document json"),
        )
        .expect("document");
        assert!(edit.load().is_err());
        let _ = std::fs::remove_dir_all(&dir);
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
    fn a_press_inside_the_committed_region_moves_it() {
        let r = ImageRect {
            x: 100.0,
            y: 100.0,
            width: 200.0,
            height: 100.0,
        };
        let selection = Some(r);
        // Inside the region - edges included - the select tool drags the region.
        for point in [
            ImagePoint { x: 150.0, y: 150.0 },
            ImagePoint { x: 100.0, y: 100.0 },
            ImagePoint { x: 300.0, y: 200.0 },
        ] {
            assert_eq!(move_target(Tool::Select, selection, point), selection);
        }
        // Outside it the press starts a new region.
        for point in [
            ImagePoint { x: 99.0, y: 150.0 },
            ImagePoint { x: 301.0, y: 150.0 },
            ImagePoint { x: 150.0, y: 99.0 },
            ImagePoint { x: 150.0, y: 201.0 },
        ] {
            assert_eq!(move_target(Tool::Select, selection, point), None);
        }
        // Without a committed region there is nothing to move, and a drawing
        // tool uses the press to draw.
        assert_eq!(
            move_target(Tool::Select, None, ImagePoint { x: 150.0, y: 150.0 }),
            None
        );
        assert_eq!(
            move_target(
                Tool::Rectangle,
                selection,
                ImagePoint { x: 150.0, y: 150.0 }
            ),
            None
        );
    }
    #[test]
    fn moving_translates_the_selection_and_stops_at_the_image_edges() {
        let r = ImageRect {
            x: 100.0,
            y: 100.0,
            width: 200.0,
            height: 100.0,
        };
        // A plain drag moves the region without changing its size.
        assert_eq!(
            move_selection(r, 60.0, 40.0, 500, 400),
            ImageRect {
                x: 160.0,
                y: 140.0,
                width: 200.0,
                height: 100.0
            }
        );
        // At every edge the rectangle stops instead of being clipped: the far
        // edge can always reach the image border and nothing sticks out.
        assert_eq!(
            move_selection(r, -500.0, -500.0, 500, 400),
            ImageRect {
                x: 0.0,
                y: 0.0,
                width: 200.0,
                height: 100.0
            }
        );
        assert_eq!(
            move_selection(r, 500.0, 500.0, 500, 400),
            ImageRect {
                x: 300.0,
                y: 300.0,
                width: 200.0,
                height: 100.0
            }
        );
        assert_eq!(
            move_selection(r, 0.0, -500.0, 500, 400),
            ImageRect {
                x: 100.0,
                y: 0.0,
                width: 200.0,
                height: 100.0
            }
        );
        // A selection as large as the image cannot move at all.
        let full = ImageRect {
            x: 0.0,
            y: 0.0,
            width: 500.0,
            height: 400.0,
        };
        assert_eq!(move_selection(full, 60.0, 40.0, 500, 400), full);
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

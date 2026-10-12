//! Long capture (plan B09): the user scrolls the selected region by hand while
//! the editor stitches every new row into one tall image, then trims the last
//! screen and turns the result into a document of its own.
//!
//! The session is driven by a timer on the overlay window instead of a blocking
//! loop: the overlay is hidden, so the wheel keeps reaching the content
//! underneath, and Esc and Enter are polled from the keyboard state because no
//! Snaplight window holds the focus while the user scrolls. The region is framed
//! by a window whose middle is a hole, so the frame can be dragged without
//! stealing clicks or the wheel from the content inside it.
//!
//! The bird's eye panel is a preview and nothing else: the session is ended with
//! the toolbar's own save, copy, pin and close buttons, which act on the stitch
//! exactly as they act on a normal capture, so the two paths share one set of
//! controls (see `Canvas::action`).

use super::*;
use snaplight_domain::long_capture::{Direction, Frame, LongCapture, Step};

/// How often the region is sampled, in milliseconds.
const TICK_MS: u32 = 120;
/// Identical samples in a row that count as "the content came to rest".
const STILLS: u32 = 2;
/// Thickness of the frame drawn around the region, in physical pixels. Wide
/// enough to grab, narrow enough not to hide what sits next to the region.
const BAND: i32 = 22;
/// Bird's eye panel: width, gap to the region and the title strip.
const PANEL_WIDTH: i32 = 320;
const PANEL_GAP: i32 = 12;
const PANEL_TITLE: i32 = 40;
const PANEL_MIN_HEIGHT: i32 = 220;
/// Window classes of the two helper windows.
const RING_CLASS: PCWSTR = w!("Snaplight.LongCaptureRing");
const PANEL_CLASS: PCWSTR = w!("Snaplight.LongCapturePanel");
/// Palette, in `COLORREF` order (`0x00BBGGRR`).
const ACCENT: COLORREF = COLORREF(0x00ff8833);
const BAND_BACK: COLORREF = COLORREF(0x00241c18);
const PANEL_BACK: COLORREF = COLORREF(0x00241c18);
const PANEL_TEXT: COLORREF = COLORREF(0x00f2f2f2);
const PANEL_HINT: COLORREF = COLORREF(0x00a8a8a8);
const PANEL_WARN: COLORREF = COLORREF(0x0070b0ff);
const BUTTON_TEXT: COLORREF = COLORREF(0x00ffffff);

/// One running long capture.
pub(super) struct LongCaptureSession {
    /// The region the user selected, in screen pixels: what is stitched.
    region: RECT,
    /// The part of `region` that is read. It excludes the Snaplight windows that
    /// sit over the region (the bird's eye panel and the toolbar), because a
    /// Snaplight window must never end up inside the stitched image.
    source: RECT,
    /// `None` until the first frame has been read.
    engine: Option<LongCapture>,
    /// Direction of the last scroll; the matching edge trims the last screen.
    direction: Option<Direction>,
    /// Screens stitched so far, the first one included.
    screens: u32,
    /// Previous sample, so two identical reads mean the content came to rest.
    last: Option<Vec<u8>>,
    stills: u32,
    /// True while the content is at rest.
    settled: bool,
    /// Rows cut from the top and the bottom of the stitch.
    trim_start: u32,
    trim_end: u32,
    /// Edge being dragged (`0` top, `1` bottom) and the screen row it started at.
    drag: Option<(u8, i32)>,
    /// Key states, so a held key fires once.
    escape: bool,
    enter: bool,
    /// Why the stitch stopped growing on its own.
    note: Option<String>,
    /// Whether the overlay was on screen when the session started, so it is only
    /// restored when it was.
    overlay_was_visible: bool,
    ring: HWND,
    panel_window: HWND,
}

/// Outcome of one long capture, phrased for the notice and the demo.
struct Report {
    region: RECT,
    screens: u32,
    width: u32,
    height: u32,
    /// Saved PNG; `None` when nothing was written.
    image: Option<PathBuf>,
    /// Why the stitch stopped early, or why it could not be saved.
    note: Option<String>,
    cancelled: bool,
}

impl Report {
    fn message(&self) -> String {
        let mut text = if self.cancelled {
            format!("长截图已取消：已拼 {} 屏，未保存。", self.screens)
        } else {
            format!(
                "长截图完成：{} 屏 → {} × {} 像素",
                self.screens, self.width, self.height
            )
        };
        if let Some(path) = &self.image {
            text.push_str(&format!("，已保存到 {}", path.display()));
        }
        if let Some(note) = &self.note {
            text.push_str(&format!("（{note}）"));
        }
        text
    }
}

/// What a closed session leaves behind: the trimmed stitch and the key its
/// capture files are named after.
struct Stitch {
    image: ImageDescriptor,
    bytes: Vec<u8>,
    key: u128,
}

impl Canvas {
    /// Starts a long capture on the current selection: the overlay is hidden, the
    /// region gets a draggable frame and a bird's eye panel, and a timer samples
    /// the region while the user scrolls the content underneath it.
    ///
    /// The toolbar stays on screen and carries the buttons that end the session,
    /// so the session is driven by the same controls as a normal capture.
    pub(super) fn start_long_capture(
        &mut self,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        if self.long_capture.is_some() {
            return Ok(());
        }
        let Some(selection) = self.doc.selection else {
            return Err("长截图需要先框选区域：滚动与拼接都以该选区为准。".into());
        };
        let region = region_of(self.screen, selection);
        if region.right - region.left < 8 || region.bottom - region.top < 8 {
            return Err("长截图区域太小：请至少框选 8 × 8 像素。".into());
        }
        // The toolbar has to stay reachable for the duration of the session, so
        // it is put back beside the selection (it may have been dragged) and both
        // it and the panel are kept out of the region that gets sampled.
        *self.ui_slots.toolbar_selection.lock() = None;
        publish_selection(self, selection);
        let panel = panel_rect(region, self.screen);
        let mut occluders = vec![panel];
        if let Some(toolbar) = *self.ui_slots.toolbar_rect.lock() {
            occluders.push(toolbar);
        }
        let source = source_rect(region, &occluders);
        if source.right - source.left < 8 || source.bottom - source.top < 8 {
            return Err("长截图区域太小：请把选区放大一些，给鸟瞰图和工具条留出位置。".into());
        }
        // The helper windows only draw once a session exists, so they are created
        // before anything that would need undoing when they fail.
        let canvas = self as *mut Canvas;
        let ring = unsafe { create_helper(RING_CLASS, Some(ring_proc), region, BAND, canvas) }?;
        let panel_window =
            match unsafe { create_helper(PANEL_CLASS, Some(panel_proc), panel, 0, canvas) } {
                Ok(hwnd) => hwnd,
                Err(error) => {
                    unsafe {
                        let _ = DestroyWindow(ring);
                    }
                    return Err(error);
                }
            };
        // The overlay covers the region: it would be stitched into the image, and
        // it would take the wheel away from the content being scrolled.
        let overlay_was_visible = unsafe { IsWindowVisible(self.hwnd).as_bool() };
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        if let Some(weak) = self.ui_slots.toolbar.get() {
            snaplight_ui::set_toolbar_long_capture(weak, true);
        }
        self.long_capture = Some(LongCaptureSession {
            region,
            source,
            engine: None,
            direction: None,
            screens: 0,
            last: None,
            stills: 0,
            settled: false,
            trim_start: 0,
            trim_end: 0,
            drag: None,
            escape: false,
            enter: false,
            note: None,
            overlay_was_visible,
            ring,
            panel_window,
        });
        unsafe {
            SetTimer(self.hwnd, LONG_CAPTURE_TIMER, TICK_MS, None);
            let _ = InvalidateRect(panel_window, None, true);
        }
        Ok(())
    }

    /// One timer tick: read the region, stitch it when it moved, and poll the two
    /// keys that finish or cancel the session.
    pub(super) fn long_capture_tick(&mut self) {
        let Some(session) = self.long_capture.as_mut() else {
            return;
        };
        // No Snaplight window holds the focus while the content is scrolled, so
        // the two keys are polled from the keyboard state, on a rising edge.
        let escape = snaplight_platform::native::key_down(VK_ESCAPE);
        let enter = snaplight_platform::native::key_down(VK_RETURN);
        let cancel = escape && !session.escape;
        let finish = enter && !session.enter;
        session.escape = escape;
        session.enter = enter;
        if cancel {
            self.cancel_long_capture();
            return;
        }
        if finish {
            self.finish_long_capture();
            return;
        }
        let Some(source) = self.long_capture.as_ref().map(|s| s.source) else {
            return;
        };
        let (width, height, bytes) = match screen_pixels(source) {
            Ok(sample) => sample,
            Err(error) => {
                if let Some(session) = self.long_capture.as_mut() {
                    session.note = Some(error.to_string());
                }
                self.cancel_long_capture();
                return;
            }
        };
        let (moved, settled_changed) = {
            let Some(session) = self.long_capture.as_mut() else {
                return;
            };
            let moved = session.last.as_deref() != Some(bytes.as_slice());
            session.last = Some(bytes.clone());
            let was_settled = session.settled;
            if moved {
                session.stills = 0;
                session.settled = false;
            } else {
                session.stills += 1;
                if session.stills >= STILLS {
                    session.settled = true;
                }
            }
            (moved, was_settled != session.settled)
        };
        if moved {
            if settled_changed {
                self.repaint_long_capture();
            }
            return;
        }
        if !settled_changed {
            return;
        }
        // The content came to rest, so this sample is a whole scroll position and
        // only now is it worth stitching: a scrolling window paints in place, and
        // a frame caught mid-repaint still holds rows from the older position, so
        // stitching it would splice content that runs backwards.
        let demo = self.demo;
        if let Some(session) = self.long_capture.as_mut() {
            let step = stitch_sample(session, width, height, &bytes);
            if demo {
                eprintln!(
                    "long capture: step {step:?} screens {} settled {}",
                    session.screens, session.settled
                );
            }
        }
        self.repaint_long_capture();
    }

    /// Ends the session: the trimmed stitch is saved into the capture folder and
    /// opened as this canvas's document, so it can be annotated like any capture.
    /// This is the toolbar's save button and Enter.
    pub(super) fn finish_long_capture(&mut self) {
        let (stitch, mut report) = self.settle_long_capture(true);
        if let Some(stitch) = stitch {
            match self.save_long_capture(&stitch.image, &stitch.bytes, stitch.key) {
                Ok(path) => report.image = Some(path),
                Err(error) => report.note = Some(format!("保存失败：{error}")),
            }
            if let Err(error) = self.open_document(stitch.image, stitch.bytes) {
                self.error = Some(error.to_string());
            }
            // The toolbar goes back to its normal duties and is placed beside the
            // stitch that has just become the document.
            self.restore_after_long_capture();
        }
        self.report_long_capture(report);
    }

    /// Pins the stitch and quits, the same way pinning a normal capture does:
    /// the session is closed, the result is written into the capture folder as a
    /// bundle the pins process understands, and that process takes over.
    /// This is the toolbar's pin button.
    pub(super) fn pin_long_capture(
        &mut self,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        match self.settle_long_capture(false) {
            (Some(stitch), report) => {
                let document = Document {
                    version: 1,
                    image: stitch.image.clone(),
                    selection: None,
                    annotations: vec![],
                };
                // The pin lands where the user framed the capture, which is what
                // pinning a normal capture does with its region.
                let region = report.region;
                let screen = ScreenPoint {
                    x: region.left,
                    y: region.top,
                };
                write_pin_bundle(
                    &self.capture_dir,
                    &document,
                    &stitch.bytes,
                    stitch.image,
                    &stitch.bytes,
                    screen,
                )
            }
            (None, report) => {
                // Nothing was readable: the session is over, so the editor comes
                // back instead of quitting with nothing to show.
                self.restore_after_long_capture();
                self.report_long_capture(report);
                Ok(())
            }
        }
    }

    /// Copies the stitch, with the current trim applied, to the clipboard. The
    /// session keeps running, exactly like copying a normal capture does.
    /// This is the toolbar's copy button.
    pub(super) fn copy_long_capture(
        &mut self,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let Some(session) = self.long_capture.as_ref() else {
            return Ok(());
        };
        let Some(engine) = &session.engine else {
            return Err("还没有拼出任何画面：先滚动内容，停下后再复制。".into());
        };
        let (image, bytes) = engine.cropped(session.trim_start, session.trim_end, ImageId(0));
        if image.width == 0 || image.height == 0 {
            return Err("长图为空，没有可复制的内容。".into());
        }
        clipboard(self.hwnd, &image, &bytes)?;
        Ok(())
    }

    /// Closes the session and returns the trimmed stitch. `restore` brings the
    /// overlay and the toolbar back into their normal roles; the pin path skips
    /// that because that command quits the editor as soon as the bundle is
    /// written. Nothing is reported or opened here: the caller decides what
    /// becomes of the stitch.
    fn settle_long_capture(&mut self, restore: bool) -> (Option<Stitch>, Report) {
        let Some(mut session) = self.long_capture.take() else {
            return (
                None,
                Report {
                    region: RECT::default(),
                    screens: 0,
                    width: 0,
                    height: 0,
                    image: None,
                    note: Some("没有正在进行的长截图".to_string()),
                    cancelled: true,
                },
            );
        };
        session.teardown(self);
        // Finishing before the last screen came to rest must not drop that
        // scroll, so the newest sample is stitched here. The helper windows are
        // already gone, and `source` never covered them anyway.
        if !session.settled {
            if let Some(bytes) = session.last.clone() {
                let width = (session.source.right - session.source.left).max(1) as u32;
                let height = (session.source.bottom - session.source.top).max(1) as u32;
                let step = stitch_sample(&mut session, width, height, &bytes);
                if self.demo {
                    eprintln!(
                        "long capture: finish step {step:?} screens {}",
                        session.screens
                    );
                }
            }
        }
        let Some(engine) = &session.engine else {
            session.restore_overlay(self);
            return (
                None,
                Report {
                    region: session.region,
                    screens: 0,
                    width: 0,
                    height: 0,
                    image: None,
                    note: Some("没有读到任何画面".to_string()),
                    cancelled: true,
                },
            );
        };
        let key = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let (image, bytes) =
            engine.cropped(session.trim_start, session.trim_end, ImageId(key as u64));
        let report = Report {
            region: session.region,
            screens: session.screens,
            width: image.width,
            height: image.height,
            image: None,
            note: session.note.clone(),
            cancelled: false,
        };
        if restore {
            session.restore_overlay(self);
        }
        (Some(Stitch { image, bytes, key }), report)
    }

    /// Drops the session without saving anything. This is the toolbar's close
    /// button and Esc.
    pub(super) fn cancel_long_capture(&mut self) {
        let Some(session) = self.long_capture.take() else {
            return;
        };
        session.teardown(self);
        session.restore_overlay(self);
        self.restore_after_long_capture();
        self.report_long_capture(Report {
            region: session.region,
            screens: session.screens,
            width: 0,
            height: 0,
            image: None,
            note: session.note.clone(),
            cancelled: true,
        });
    }

    /// Applies an edge drag: dragging an edge inwards cuts rows off that end. The
    /// source is a vertical crop of the region, so one screen row is one stitched
    /// row. Returns false when the pointer is not part of a drag.
    pub(super) fn drag_long_capture(&mut self, screen_y: i32) -> bool {
        let Some(session) = self.long_capture.as_mut() else {
            return false;
        };
        let Some((edge, start)) = session.drag else {
            return false;
        };
        let height = session
            .engine
            .as_ref()
            .map(LongCapture::height)
            .unwrap_or(0);
        if height == 0 {
            return true;
        }
        // The source is a vertical crop of the region, so one screen row is one
        // stitched row and one screen's worth of new rows is the source height.
        let screen_height = (session.source.bottom - session.source.top).max(1) as u32;
        let (trim_start, trim_end) = trim_after_drag(
            edge,
            start,
            screen_y,
            height,
            screen_height,
            session.trim_start,
            session.trim_end,
        );
        session.trim_start = trim_start;
        session.trim_end = trim_end;
        self.repaint_long_capture();
        true
    }

    /// Repaints the frame and the panel, which show the progress, the trim and the
    /// direction the content was scrolled in.
    fn repaint_long_capture(&self) {
        let Some(session) = self.long_capture.as_ref() else {
            return;
        };
        unsafe {
            let _ = InvalidateRect(session.ring, None, false);
            let _ = InvalidateRect(session.panel_window, None, false);
        }
    }

    /// Brings the canvas and the toolbar back to their normal state after a
    /// session: the toolbar leaves long-capture mode and is placed beside
    /// whatever document is current.
    fn restore_after_long_capture(&mut self) {
        if let Some(weak) = self.ui_slots.toolbar.get() {
            snaplight_ui::set_toolbar_long_capture(weak, false);
        }
        publish_history(self);
        unsafe {
            self.invalidate();
        }
    }

    /// Reports the outcome: the demo prints JSON and quits, the interactive editor
    /// shows a notice.
    fn report_long_capture(&mut self, report: Report) {
        if self.demo {
            print_demo(&report);
            unsafe {
                PostQuitMessage(0);
            }
        } else {
            snaplight_platform::native::show_notice(&report.message());
        }
    }

    /// Writes the stitched image into the capture folder the way a normal capture
    /// is stored, so it can be reopened from the settings page.
    fn save_long_capture(
        &self,
        image: &ImageDescriptor,
        bytes: &[u8],
        key: u128,
    ) -> std::result::Result<PathBuf, Box<dyn std::error::Error>> {
        let folder = self.capture_dir.clone();
        std::fs::create_dir_all(&folder)?;
        let stem = folder.join(format!("{key}"));
        write_pixels(&stem.with_extension("source.bgra"), image, bytes)?;
        atomic_write(
            &stem.with_extension("document.json"),
            &serde_json::to_vec_pretty(&Document {
                version: 1,
                image: image.clone(),
                selection: None,
                annotations: vec![],
            })?,
        )?;
        let png = stem.with_extension("png");
        write_png(&png, image, bytes)?;
        Ok(png)
    }

    /// Replaces this canvas's document with the stitched image, shrunk to fit so
    /// the whole result is visible at once.
    fn open_document(
        &mut self,
        image: ImageDescriptor,
        bytes: Vec<u8>,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let view = self.view_size();
        self.renderer.rebuild(&bytes, &image, self.screen, view)?;
        let selection = ImageRect {
            x: 0.0,
            y: 0.0,
            width: image.width as f32,
            height: image.height as f32,
        };
        self.doc = Document {
            version: 1,
            image,
            selection: Some(selection),
            annotations: vec![],
        };
        self.base = bytes;
        self.history = History::default();
        self.interaction = Interaction::default();
        self.start = None;
        self.resize = None;
        self.tool = Tool::Select;
        self.view = ViewTransform::fit(&self.doc.image, view);
        self.hover_probe.pending = None;
        self.hover_probe.last = None;
        self.set_hover(None);
        unsafe {
            self.invalidate();
        }
        Ok(())
    }
}

impl LongCaptureSession {
    /// Removes the helper windows and the sampling timer.
    fn teardown(&self, canvas: &Canvas) {
        unsafe {
            let _ = KillTimer(canvas.hwnd, LONG_CAPTURE_TIMER);
            let _ = DestroyWindow(self.ring);
            let _ = DestroyWindow(self.panel_window);
        }
    }

    /// Puts the overlay back on screen when the session took it off.
    fn restore_overlay(&self, canvas: &Canvas) {
        if self.overlay_was_visible {
            unsafe {
                let _ = ShowWindow(canvas.hwnd, SW_SHOW);
            }
        }
    }
}

/// Adds one sample to the stitch, starting it when this is the first frame.
/// Returns the step the engine reported and whether the stitch was just started.
fn advance_stitch(
    engine: &mut Option<LongCapture>,
    direction: Option<Direction>,
    width: u32,
    height: u32,
    bytes: &[u8],
) -> (Step, bool) {
    let frame = Frame {
        width,
        height,
        stride: width as usize * 4,
        bytes,
    };
    match engine.as_mut() {
        Some(stitch) => (stitch.advance(&frame, direction), false),
        None => match LongCapture::new(frame, snaplight_domain::long_capture::MAX_BYTES) {
            Ok(stitch) => {
                *engine = Some(stitch);
                (Step::Unchanged, true)
            }
            Err(why) => (Step::Budget(why), false),
        },
    }
}

/// Adds one settled sample to the stitch and folds the reported step into the
/// session counters. A jump or a full budget is recorded but does not end the
/// session: the stitch so far stays usable, so the user can still trim it and
/// finish.
fn stitch_sample(session: &mut LongCaptureSession, width: u32, height: u32, bytes: &[u8]) -> Step {
    let (step, started) =
        advance_stitch(&mut session.engine, session.direction, width, height, bytes);
    if started {
        session.screens = 1;
    }
    match &step {
        Step::Scrolled { .. } => {
            session.screens += 1;
            session.direction = Some(Direction::Down);
        }
        Step::Prepended { .. } => {
            session.screens += 1;
            session.direction = Some(Direction::Up);
        }
        Step::Unchanged => {}
        Step::Jumped => {
            session.note =
                Some("滚动跳变：新一屏与上一屏没有足够重叠，为避免漏行已停止".to_string())
        }
        Step::Budget(why) => session.note = Some(why.to_string()),
    }
    step
}

/// Applies an edge drag: dragging an edge inwards cuts rows off that end. `edge`
/// is `0` for the top edge and `1` for the bottom one, `start` is the screen row
/// the drag began at, and `screen_height` is how many rows one screen
/// contributes, so at most one screen can be trimmed.
fn trim_after_drag(
    edge: u8,
    start: i32,
    screen_y: i32,
    height: u32,
    screen_height: u32,
    trim_start: u32,
    trim_end: u32,
) -> (u32, u32) {
    // At most the last screen is trimmed, so content the user asked for is never
    // cut away by accident, and the two ends can never overlap.
    let most = screen_height.min(height.saturating_sub(1));
    let delta = if edge == 0 {
        screen_y - start
    } else {
        start - screen_y
    };
    let rows = (delta.max(0) as u32).min(most);
    let room = height.saturating_sub(1);
    if edge == 0 {
        (rows.min(room.saturating_sub(trim_end)), trim_end)
    } else {
        (trim_start, rows.min(room.saturating_sub(trim_start)))
    }
}

/// The region the session stitches, in screen pixels: the selection mapped into
/// screen space and clamped to the captured screen.
fn region_of(screen: RECT, selection: ImageRect) -> RECT {
    RECT {
        left: (screen.left + selection.x as i32).clamp(screen.left, screen.right),
        top: (screen.top + selection.y as i32).clamp(screen.top, screen.bottom),
        right: (screen.left + (selection.x + selection.width) as i32)
            .clamp(screen.left, screen.right),
        bottom: (screen.top + (selection.y + selection.height) as i32)
            .clamp(screen.top, screen.bottom),
    }
}

/// Screen rectangle of the bird's eye panel: beside the region when there is
/// room, on top of its right edge when there is not.
fn panel_rect(region: RECT, screen: RECT) -> RECT {
    let screen_height = (screen.bottom - screen.top).max(PANEL_MIN_HEIGHT);
    let height = (region.bottom - region.top).clamp(PANEL_MIN_HEIGHT, screen_height);
    let right_of = region.right + PANEL_GAP;
    let left_of = region.left - PANEL_GAP - PANEL_WIDTH;
    let left = if right_of + PANEL_WIDTH <= screen.right {
        right_of
    } else if left_of >= screen.left {
        left_of
    } else {
        // No room on either side: the panel covers the region's right edge, and
        // the columns underneath it are left out of the stitch.
        (region.right - PANEL_GAP - PANEL_WIDTH).max(screen.left)
    };
    let top = region
        .top
        .clamp(screen.top, (screen.bottom - height).max(screen.top));
    RECT {
        left,
        top,
        right: left + PANEL_WIDTH,
        bottom: top + height,
    }
}

/// The part of the region that is read: the region minus every Snaplight window
/// that sits on top of it. A window that covers an edge of the region cuts rows
/// off, a strip beside it cuts columns, and the side that keeps the most of the
/// region wins.
fn source_rect(region: RECT, occluders: &[RECT]) -> RECT {
    occluders
        .iter()
        .fold(region, |source, occluder| exclude(source, *occluder))
}

/// Removes one occluder from `source` by cutting the side that keeps the most
/// area. The result never overlaps `occluder`, and shrinking an already clear
/// rectangle keeps it clear of the occluders handled before.
fn exclude(source: RECT, occluder: RECT) -> RECT {
    let clear = source.right <= occluder.left
        || source.left >= occluder.right
        || source.bottom <= occluder.top
        || source.top >= occluder.bottom;
    if clear {
        return source;
    }
    let area = |rect: &RECT| {
        (rect.right - rect.left).max(0) as i64 * (rect.bottom - rect.top).max(0) as i64
    };
    let candidates = [
        RECT {
            top: occluder.bottom + 1,
            ..source
        },
        RECT {
            bottom: occluder.top - 1,
            ..source
        },
        RECT {
            left: occluder.right + 1,
            ..source
        },
        RECT {
            right: occluder.left - 1,
            ..source
        },
    ];
    let mut best = candidates[0];
    for candidate in &candidates[1..] {
        if area(candidate) > area(&best) {
            best = *candidate;
        }
    }
    best
}

/// Which edge a press at `screen_y` grabbed, if any: the frame's hit region only
/// covers the band outside the region.
fn edge_at(region: RECT, screen_y: i32) -> Option<u8> {
    if screen_y < region.top {
        Some(0)
    } else if screen_y > region.bottom {
        Some(1)
    } else {
        None
    }
}

/// Creates one helper window: a borderless, topmost popup that never takes the
/// focus, sized `rect` grown by `band` on every side. With a band, the middle is
/// cut out of the window region so the content inside stays reachable.
unsafe fn create_helper(
    class: PCWSTR,
    procedure: WNDPROC,
    rect: RECT,
    band: i32,
    canvas: *mut Canvas,
) -> std::result::Result<HWND, Box<dyn std::error::Error>> {
    let instance = GetModuleHandleW(None)?;
    let window_class = WNDCLASSW {
        hInstance: instance.into(),
        lpszClassName: class,
        lpfnWndProc: procedure,
        ..Default::default()
    };
    // Registered once per process: a second canvas reuses the same class.
    RegisterClassW(&window_class);
    let width = (rect.right - rect.left) + band * 2;
    let height = (rect.bottom - rect.top) + band * 2;
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        class,
        w!("Snaplight Long Capture"),
        WS_POPUP,
        rect.left - band,
        rect.top - band,
        width,
        height,
        None,
        None,
        instance,
        None,
    )?;
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, canvas as isize);
    if band > 0 {
        let outer = CreateRectRgn(0, 0, width, height);
        let inner = CreateRectRgn(
            band,
            band,
            band + (rect.right - rect.left),
            band + (rect.bottom - rect.top),
        );
        CombineRgn(outer, outer, inner, RGN_DIFF);
        // The window region owns `inner`'s replacement: the system frees `outer`.
        let _ = DeleteObject(inner);
        SetWindowRgn(hwnd, outer, true);
    }
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    let _ = UpdateWindow(hwnd);
    Ok(hwnd)
}

/// Window procedure of the frame around the region: the top and bottom bands cut
/// rows off the stitched image.
unsafe extern "system" fn ring_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Canvas;
    if !ptr.is_null() {
        let canvas = &mut *ptr;
        match msg {
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut paint);
                paint_ring(hdc, canvas, hwnd);
                let _ = EndPaint(hwnd, &paint);
                return LRESULT(0);
            }
            WM_ERASEBKGND => return LRESULT(1),
            WM_LBUTTONDOWN => {
                let mut point = POINT::default();
                if GetCursorPos(&mut point).is_ok() {
                    let edge = canvas
                        .long_capture
                        .as_ref()
                        .and_then(|session| edge_at(session.region, point.y));
                    if let Some(edge) = edge {
                        if let Some(session) = canvas.long_capture.as_mut() {
                            session.drag = Some((edge, point.y));
                        }
                        SetCapture(hwnd);
                    }
                }
                return LRESULT(0);
            }
            WM_MOUSEMOVE => {
                let mut point = POINT::default();
                if GetCursorPos(&mut point).is_ok() && canvas.drag_long_capture(point.y) {
                    return LRESULT(0);
                }
            }
            WM_LBUTTONUP => {
                if let Some(session) = canvas.long_capture.as_mut() {
                    session.drag = None;
                }
                let _ = ReleaseCapture();
                return LRESULT(0);
            }
            WM_SETCURSOR => {
                if (l.0 as u32 & 0xffff) == HTCLIENT {
                    if let Ok(cursor) = LoadCursorW(None, IDC_SIZENS) {
                        SetCursor(cursor);
                    }
                    return LRESULT(1);
                }
            }
            WM_RBUTTONUP => {
                canvas.cancel_long_capture();
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, w, l)
}

/// Window procedure of the bird's eye panel. It is a preview only: the session
/// is driven by the toolbar buttons, so the panel takes no input at all.
unsafe extern "system" fn panel_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Canvas;
    if !ptr.is_null() {
        let canvas = &mut *ptr;
        match msg {
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut paint);
                paint_panel(hdc, canvas, hwnd);
                let _ = EndPaint(hwnd, &paint);
                return LRESULT(0);
            }
            WM_ERASEBKGND => return LRESULT(1),
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, w, l)
}

/// Paints the frame: the band, the edge that trims the last screen, and what that
/// edge does.
unsafe fn paint_ring(hdc: HDC, canvas: &Canvas, hwnd: HWND) {
    let Some(session) = canvas.long_capture.as_ref() else {
        return;
    };
    let mut client = RECT::default();
    if GetClientRect(hwnd, &mut client).is_err() {
        return;
    }
    let back = CreateSolidBrush(BAND_BACK);
    FillRect(hdc, &client, back);
    let _ = DeleteObject(back);
    SetBkMode(hdc, TRANSPARENT);
    let old = SelectObject(hdc, GetStockObject(DEFAULT_GUI_FONT));
    // The last screen is at the bottom when the content scrolled down and at the
    // top when it scrolled up, so that is the edge that trims it.
    let (top_edge, hint) = match session.direction {
        Some(Direction::Up) => (true, "向上滚动 · 拖动此边裁掉最后一段"),
        _ => (false, "向下滚动 · 拖动此边裁掉最后一段"),
    };
    let band = if top_edge {
        RECT {
            left: 0,
            top: 0,
            right: client.right,
            bottom: BAND,
        }
    } else {
        RECT {
            left: 0,
            top: client.bottom - BAND,
            right: client.right,
            bottom: client.bottom,
        }
    };
    let brush = CreateSolidBrush(ACCENT);
    FillRect(hdc, &band, brush);
    let _ = DeleteObject(brush);
    SetTextColor(hdc, BUTTON_TEXT);
    let mut text: Vec<u16> = hint.encode_utf16().collect();
    let mut area = band;
    DrawTextW(
        hdc,
        &mut text,
        &mut area,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
    SelectObject(hdc, old);
}

/// Paints the bird's eye: how much has been stitched, the stitch itself with the
/// trim applied, and what to do next. Everything the user can press lives on the
/// toolbar, so the panel carries no buttons.
unsafe fn paint_panel(hdc: HDC, canvas: &Canvas, hwnd: HWND) {
    let Some(session) = canvas.long_capture.as_ref() else {
        return;
    };
    let mut client = RECT::default();
    if GetClientRect(hwnd, &mut client).is_err() {
        return;
    }
    let back = CreateSolidBrush(PANEL_BACK);
    FillRect(hdc, &client, back);
    let _ = DeleteObject(back);
    SetBkMode(hdc, TRANSPARENT);
    let old = SelectObject(hdc, GetStockObject(DEFAULT_GUI_FONT));
    let title = match &session.engine {
        None => "长截图 · 正在读取选区…".to_string(),
        Some(engine) => format!(
            "长截图 · 已拼 {} 屏 · {} × {}",
            session.screens,
            engine.width(),
            engine
                .height()
                .saturating_sub(session.trim_start + session.trim_end)
        ),
    };
    // A note from the engine (a jump, a full budget) outranks the usual hint: it
    // explains why the stitch stopped growing.
    let (hint, hint_colour) = match (&session.note, session.settled) {
        (Some(note), _) => (note.as_str(), PANEL_WARN),
        (None, true) => ("画面已停下：拖边线微调，用工具条保存或贴图", PANEL_HINT),
        (None, false) => ("把鼠标放进选区滚动，画面停下后自动拼接", PANEL_HINT),
    };
    let mut title_area = RECT {
        left: 8,
        top: 2,
        right: client.right - 8,
        bottom: 20,
    };
    SetTextColor(hdc, PANEL_TEXT);
    let mut title_text: Vec<u16> = title.encode_utf16().collect();
    DrawTextW(
        hdc,
        &mut title_text,
        &mut title_area,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
    );
    let mut hint_area = RECT {
        left: 8,
        top: 20,
        right: client.right - 8,
        bottom: PANEL_TITLE - 2,
    };
    SetTextColor(hdc, hint_colour);
    let mut hint_text: Vec<u16> = hint.encode_utf16().collect();
    DrawTextW(
        hdc,
        &mut hint_text,
        &mut hint_area,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
    );
    let area = RECT {
        left: 0,
        top: PANEL_TITLE,
        right: client.right,
        bottom: client.bottom,
    };
    if let Some(engine) = &session.engine {
        let (image, bytes) = engine.cropped(session.trim_start, session.trim_end, ImageId(0));
        if image.width > 0 && image.height > 0 && !bytes.is_empty() {
            let area_width = (area.right - area.left).max(1) as f32;
            let area_height = (area.bottom - area.top).max(1) as f32;
            let scale = (area_width / image.width as f32).min(area_height / image.height as f32);
            let width = ((image.width as f32 * scale) as i32).clamp(1, area.right - area.left);
            let height = ((image.height as f32 * scale) as i32).clamp(1, area.bottom - area.top);
            let x = area.left + (area.right - area.left - width) / 2;
            let y = area.top + (area.bottom - area.top - height) / 2;
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: image.width as i32,
                    // A negative height keeps the rows top-down.
                    biHeight: -(image.height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            StretchDIBits(
                hdc,
                x,
                y,
                width,
                height,
                0,
                0,
                image.width as i32,
                image.height as i32,
                Some(bytes.as_ptr() as *const core::ffi::c_void),
                &info,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }
    }
    SelectObject(hdc, old);
}

/// Prints the demo outcome as JSON, so an end-to-end run can be checked from a
/// script.
fn print_demo(report: &Report) {
    let region = report.region;
    println!(
        "{}",
        serde_json::json!({
            "long_capture": {
                "region": [
                    region.left,
                    region.top,
                    region.right - region.left,
                    region.bottom - region.top
                ],
                "screens": report.screens,
                "width": report.width,
                "height": report.height,
                "image": report.image.as_ref().map(|p| p.display().to_string()),
                "note": report.note,
                "cancelled": report.cancelled,
                "message": report.message(),
            }
        })
    );
    let _ = std::io::stdout().flush();
}

/// Parses `--long-capture-demo=x,y,w,h`, in physical screen pixels.
pub(super) fn parse_long_capture_demo(arg: &str) -> Option<RECT> {
    let value = arg.strip_prefix("--long-capture-demo=")?;
    let mut parts = value.split(',').map(|part| part.trim().parse::<i32>());
    let x = parts.next()?.ok()?;
    let y = parts.next()?.ok()?;
    let width = parts.next()?.ok()?;
    let height = parts.next()?.ok()?;
    if parts.next().is_some() || width <= 0 || height <= 0 {
        return None;
    }
    Some(RECT {
        left: x,
        top: y,
        right: x + width,
        bottom: y + height,
    })
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
    fn the_panel_sits_beside_the_region_when_there_is_room() {
        let screen = rect(0, 0, 3840, 2160);
        let region = rect(1000, 400, 1800, 1000);
        let panel = panel_rect(region, screen);
        assert_eq!(panel.left, region.right + PANEL_GAP);
        assert_eq!(panel.right - panel.left, PANEL_WIDTH);
        assert_eq!(panel.top, region.top);
        // Nothing overlaps, so the whole region is read.
        assert_eq!(source_rect(region, &[panel]), region);
    }

    #[test]
    fn the_panel_moves_left_when_the_region_reaches_the_edge() {
        let screen = rect(0, 0, 1920, 1080);
        let region = rect(1500, 100, 1900, 700);
        let panel = panel_rect(region, screen);
        assert!(panel.right <= region.left);
        assert_eq!(source_rect(region, &[panel]), region);
    }

    #[test]
    fn a_panel_with_nowhere_else_to_go_leaves_the_region_out_of_the_stitch() {
        let screen = rect(0, 0, 1920, 1080);
        // Wide enough that neither side has room for the panel.
        let region = rect(100, 100, 1900, 700);
        let panel = panel_rect(region, screen);
        assert!(panel.left > region.left && panel.left < region.right);
        let source = source_rect(region, &[panel]);
        // The panel covers the right edge, so the sampled columns stop before it.
        assert!(source.right <= panel.left);
        assert_eq!(source.left, region.left);
        assert_eq!(source.top, region.top);
        assert_eq!(source.bottom, region.bottom);
    }

    #[test]
    fn a_toolbar_over_the_region_cuts_rows_but_stays_clear_below_it() {
        let region = rect(200, 40, 1200, 1060);
        // The usual placement: below the region, so nothing is given up.
        let below = rect(200, 1076, 1337, 1154);
        assert_eq!(source_rect(region, &[below]), region);
        // A region that reaches the top of the screen pushes the toolbar onto its
        // top rows. The band covers the whole width, so rows go and columns stay.
        let above = rect(200, 16, 1337, 94);
        let source = source_rect(region, &[above]);
        assert_eq!(source.left, region.left);
        assert_eq!(source.right, region.right);
        assert!(source.top > above.bottom);
        assert_eq!(source.bottom, region.bottom);
        // A strip beside the region still cuts columns instead.
        let beside = rect(900, 100, 1300, 400);
        let source = source_rect(region, &[beside]);
        assert!(source.right <= beside.left);
        assert_eq!(source.top, region.top);
        assert_eq!(source.bottom, region.bottom);
    }

    #[test]
    fn every_occluder_is_kept_out_of_the_source() {
        // The panel on the right edge and the toolbar over the top at once: the
        // two cuts are applied one after the other and neither is undone.
        let screen = rect(0, 0, 1920, 1080);
        let region = rect(100, 20, 1900, 1040);
        let panel = panel_rect(region, screen);
        let toolbar = rect(100, 900, 1237, 978);
        let source = source_rect(region, &[panel, toolbar]);
        let clear_of = |source: RECT, occluder: RECT| {
            source.right <= occluder.left
                || source.left >= occluder.right
                || source.bottom <= occluder.top
                || source.top >= occluder.bottom
        };
        assert!(clear_of(source, panel), "{source:?} still covers the panel");
        assert!(
            clear_of(source, toolbar),
            "{source:?} still covers the toolbar"
        );
        assert!(source.right - source.left >= 8 && source.bottom - source.top >= 8);
    }

    #[test]
    fn a_region_below_the_screen_keeps_the_panel_on_screen() {
        let screen = rect(0, 0, 1920, 1080);
        let region = rect(400, 900, 1200, 1080);
        let panel = panel_rect(region, screen);
        assert!(panel.bottom <= screen.bottom);
        assert!(panel.top >= screen.top);
    }

    #[test]
    fn only_the_band_outside_the_region_trims() {
        let region = rect(100, 200, 500, 600);
        assert_eq!(edge_at(region, 150), Some(0));
        assert_eq!(edge_at(region, 650), Some(1));
        assert_eq!(edge_at(region, 400), None);
        assert_eq!(edge_at(region, 200), None);
        assert_eq!(edge_at(region, 600), None);
    }

    #[test]
    fn the_demo_rectangle_is_parsed_in_screen_pixels() {
        assert_eq!(
            parse_long_capture_demo("--long-capture-demo=10,20,300,400"),
            Some(rect(10, 20, 310, 420))
        );
        assert_eq!(
            parse_long_capture_demo("--long-capture-demo=10,20,0,400"),
            None
        );
        assert_eq!(parse_long_capture_demo("--long-capture-demo=1,2,3"), None);
        assert_eq!(parse_long_capture_demo("--other=1,2,3,4"), None);
    }

    /// A synthetic page: a sticky header stays on top while the body moves up by
    /// `SCROLL` rows per sample, and every content row has its own colour, so the
    /// overlap can only be found at the one correct offset.
    struct FakePage;
    impl FakePage {
        const WIDTH: u32 = 200;
        const HEIGHT: u32 = 300;
        /// Rows of the header that never scroll away.
        const HEADER: u32 = 30;
        /// Rows one scroll step moves the body.
        const SCROLL: u32 = 90;
        /// Samples with new content.
        const FRAMES: u32 = 6;
        /// Colour of one header row.
        fn header_colour(row: u32) -> [u8; 4] {
            [(row * 3) as u8, (row * 5) as u8, (row * 7) as u8, 255]
        }
        /// Colour of one content row, unlike its neighbours in every channel.
        fn content_colour(row: u32) -> [u8; 4] {
            [
                (row % 251) as u8,
                (row * 7 % 241) as u8,
                (row * 13 % 239) as u8,
                255,
            ]
        }
        /// One screen at the given scroll offset, in BGRA rows.
        fn view(scroll: u32) -> Vec<u8> {
            let mut bytes = vec![0u8; (Self::WIDTH * Self::HEIGHT * 4) as usize];
            for y in 0..Self::HEIGHT {
                let colour = if y < Self::HEADER {
                    Self::header_colour(y)
                } else {
                    Self::content_colour(scroll + y - Self::HEADER)
                };
                for x in 0..Self::WIDTH {
                    let at = ((y * Self::WIDTH + x) * 4) as usize;
                    bytes[at..at + 4].copy_from_slice(&colour);
                }
            }
            bytes
        }
    }

    #[test]
    fn the_stitch_keeps_every_row_of_a_scrolling_page() {
        let mut engine = None;
        let mut direction = None;
        let mut screens = 0;
        for index in 0..FakePage::FRAMES {
            let bytes = FakePage::view(index * FakePage::SCROLL);
            let (step, started) = advance_stitch(
                &mut engine,
                direction,
                FakePage::WIDTH,
                FakePage::HEIGHT,
                &bytes,
            );
            if started {
                screens = 1;
            }
            match step {
                Step::Scrolled { .. } => {
                    screens += 1;
                    direction = Some(Direction::Down);
                }
                Step::Prepended { .. } => {
                    screens += 1;
                    direction = Some(Direction::Up);
                }
                Step::Unchanged => {}
                _ => panic!("the synthetic page only scrolls"),
            }
        }
        // The last screen is read twice, which is how the session sees the end of
        // the content.
        let bytes = FakePage::view((FakePage::FRAMES - 1) * FakePage::SCROLL);
        let (step, _) = advance_stitch(
            &mut engine,
            direction,
            FakePage::WIDTH,
            FakePage::HEIGHT,
            &bytes,
        );
        assert!(matches!(step, Step::Unchanged));
        assert_eq!(screens, FakePage::FRAMES);
        let engine = engine.expect("the first frame starts a stitch");
        let expected = FakePage::HEIGHT + (FakePage::FRAMES - 1) * FakePage::SCROLL;
        assert_eq!(engine.height(), expected);
        assert_eq!(engine.width(), FakePage::WIDTH);
        // Every row has to land in its own place: a missing or duplicated row would
        // shift every row below it.
        let stride = engine.stride();
        let bytes = engine.bytes();
        let colour_at = |y: u32| {
            let at = y as usize * stride + 100 * 4;
            &bytes[at..at + 4]
        };
        for y in 0..FakePage::HEADER {
            assert_eq!(colour_at(y), FakePage::header_colour(y), "header row {y}");
        }
        for y in FakePage::HEADER..expected {
            assert_eq!(
                colour_at(y),
                FakePage::content_colour(y - FakePage::HEADER),
                "content row {y}"
            );
        }
    }

    #[test]
    fn dragging_an_edge_cuts_rows_off_that_end_only() {
        // A 900-row stitch read from a 300-row region: one drag cuts at most one
        // screen, and only off the end it was dragged from.
        assert_eq!(trim_after_drag(0, 500, 560, 900, 300, 0, 0), (60, 0));
        assert_eq!(trim_after_drag(1, 500, 440, 900, 300, 0, 0), (0, 60));
        // Dragging outwards does not grow the stitch back.
        assert_eq!(trim_after_drag(1, 500, 900, 900, 300, 0, 0), (0, 0));
        // Dragging further than one screen stops at the screen.
        assert_eq!(trim_after_drag(1, 500, 0, 900, 300, 0, 0), (0, 300));
        // The two ends can never overlap, so the top stops where the bottom is.
        assert_eq!(trim_after_drag(0, 0, 400, 900, 300, 0, 750), (149, 750));
    }
}

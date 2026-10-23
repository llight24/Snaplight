//! Long capture (plan B09): the user scrolls the selected region by hand while
//! a worker thread stitches every new row into one tall image, then trims the
//! last screen and turns the result into a document of its own.
//!
//! The pixels are handled on a worker thread: a `RegionGrabber` blocks on
//! Desktop Duplication when the desktop is idle, and the UI thread only polls
//! key state, copies the cached thumbnail into the bird's-eye panel and sends
//! commands. The overlay is hidden, so the wheel keeps reaching the content
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
use snaplight_domain::long_capture::{
    Bands, Direction, Frame, LongCapture, Step, MAX_BYTES, MAX_HEIGHT, MIN_OVERLAP,
};
use snaplight_render_win::{GrabBackend, RegionGrabber};
use std::sync::atomic::{AtomicU8, Ordering};

/// How often the compatible path samples the region, in milliseconds. The
/// duplication path uses it as the wait timeout instead: a desktop update wakes
/// the worker immediately.
const TICK_MS: u32 = 120;
/// Identical samples in a row that count as "the content came to rest".
const STILLS: u32 = 2;
/// Shortest fixed band worth reporting. Fewer rows are more likely to be a
/// coincidence than a sticky bar.
const MIN_BAND: u32 = 4;
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
/// Commands from the UI thread to the worker.
const RUN: u8 = 0;
const FINISH: u8 = 1;
const CANCEL: u8 = 2;
/// Palette, in `COLORREF` order (`0x00BBGGRR`).
const ACCENT: COLORREF = COLORREF(0x00ff8833);
const BAND_BACK: COLORREF = COLORREF(0x00241c18);
const PANEL_BACK: COLORREF = COLORREF(0x00241c18);
const PANEL_TEXT: COLORREF = COLORREF(0x00f2f2f2);
const PANEL_HINT: COLORREF = COLORREF(0x00a8a8a8);
const PANEL_WARN: COLORREF = COLORREF(0x0070b0ff);
const BUTTON_TEXT: COLORREF = COLORREF(0x00ffffff);

/// State shared between the worker and the UI thread. The engine belongs to the
/// worker; the UI only reads it for the thumbnail, copying and finishing, and
/// never touches pixels on a paint path.
#[derive(Default)]
struct Shared {
    engine: Option<LongCapture>,
    screens: u32,
    direction: Option<Direction>,
    settled: bool,
    note: Option<String>,
    backend: Option<GrabBackend>,
    /// Bumped on every visible change, so the UI only repaints when needed.
    generation: u64,
}

/// One running long capture.
pub(super) struct LongCaptureSession {
    /// The region the user selected, in screen pixels: what is stitched.
    region: RECT,
    /// The part of `region` that is read. It excludes the Snaplight windows that
    /// sit over the region (the bird's eye panel and the toolbar), because a
    /// Snaplight window must never end up inside the stitched image.
    source: RECT,
    shared: Arc<Mutex<Shared>>,
    command: Arc<AtomicU8>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// Bird's-eye preview at thumbnail scale, rebuilt only when the stitch or the
    /// trim changed. Holding it here keeps paint independent of the full image.
    preview: Option<(u32, u32, Vec<u8>)>,
    /// Last generation copied into `preview`.
    generation: u64,
    /// Rows cut from the top and the bottom of the body.
    trim_start: u32,
    trim_end: u32,
    /// Edge being dragged (`0` top, `1` bottom) and the screen row it started at.
    drag: Option<(u8, i32)>,
    /// Key states, so a held key fires once.
    escape: bool,
    enter: bool,
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
    /// region gets a draggable frame and a bird's eye panel, and a worker thread
    /// samples the region while the user scrolls the content underneath it.
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
        // A selection that reaches past the captured screen would be silently
        // narrowed by a clamp, and the result would be missing a strip the user
        // asked for. Long capture is single-screen until mixed-DPI stitching is
        // handled separately, so refuse instead of lying.
        let region = selection_region(self.screen, selection);
        if !screen_contains(self.screen, region) {
            return Err("选区跨越了显示器边界：长截图目前只能在单块屏幕内进行。".into());
        }
        if region.right - region.left < 8 || region.bottom - region.top < 8 {
            return Err("长截图区域太小：请至少框选 8 × 8 像素。".into());
        }
        // The toolbar has to stay reachable for the duration of the session, so
        // it is put back beside the selection (it may have been dragged) and both
        // it and the panel are kept out of the region that gets sampled.
        self.ui_slots.clear_selection_cache();
        publish_selection(self, selection);
        let panel = panel_rect(region, self.screen);
        let mut occluders = vec![panel];
        if let Some(toolbar) = self.ui_slots.toolbar_rect() {
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
        let shared = Arc::new(Mutex::new(Shared::default()));
        let command = Arc::new(AtomicU8::new(RUN));
        let worker_shared = Arc::clone(&shared);
        let worker_command = Arc::clone(&command);
        let worker = match std::thread::Builder::new()
            .name("snaplight-long-capture".to_string())
            .spawn(move || run(worker_shared, worker_command, source))
        {
            Ok(worker) => worker,
            Err(error) => {
                unsafe {
                    let _ = DestroyWindow(ring);
                    let _ = DestroyWindow(panel_window);
                }
                if overlay_was_visible {
                    unsafe {
                        let _ = ShowWindow(self.hwnd, SW_SHOW);
                    }
                }
                return Err(format!("无法启动长截图线程：{error}").into());
            }
        };
        if let Some(weak) = self.ui_slots.toolbar() {
            snaplight_ui::set_toolbar_long_capture(weak, true);
        }
        self.long_capture = Some(LongCaptureSession {
            region,
            source,
            shared,
            command,
            worker: Some(worker),
            preview: None,
            generation: u64::MAX,
            trim_start: 0,
            trim_end: 0,
            drag: None,
            escape: false,
            enter: false,
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

    /// One timer tick: poll the two keys that finish or cancel the session, and
    /// pick up the worker's latest progress. No pixels are read or copied here.
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
        let generation = session.shared.lock().generation;
        if generation == session.generation {
            return;
        }
        session.refresh_preview();
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
        let shared = session.shared.lock();
        let Some(engine) = &shared.engine else {
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
        // The helper windows must go before the worker's final read: duplication
        // captures them, unlike the GDI source rectangle that excluded them.
        session.teardown(self);
        session.stop_worker(FINISH);
        let (stitch, screens, note) = {
            let shared = session.shared.lock();
            let stitch = shared.engine.as_ref().map(|engine| {
                let key = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos())
                    .unwrap_or(0);
                let (image, bytes) =
                    engine.cropped(session.trim_start, session.trim_end, ImageId(key as u64));
                Stitch { image, bytes, key }
            });
            (stitch, shared.screens, shared.note.clone())
        };
        if restore {
            session.restore_overlay(self);
        }
        match stitch {
            Some(stitch) => {
                let report = Report {
                    region: session.region,
                    screens,
                    width: stitch.image.width,
                    height: stitch.image.height,
                    image: None,
                    note,
                    cancelled: false,
                };
                (Some(stitch), report)
            }
            None => (
                None,
                Report {
                    region: session.region,
                    screens: 0,
                    width: 0,
                    height: 0,
                    image: None,
                    note: Some(note.unwrap_or_else(|| "没有读到任何画面".to_string())),
                    cancelled: true,
                },
            ),
        }
    }

    /// Drops the session without saving anything. This is the toolbar's close
    /// button and Esc.
    pub(super) fn cancel_long_capture(&mut self) {
        let Some(mut session) = self.long_capture.take() else {
            return;
        };
        session.teardown(self);
        session.stop_worker(CANCEL);
        let (screens, note) = {
            let shared = session.shared.lock();
            (shared.screens, shared.note.clone())
        };
        session.restore_overlay(self);
        self.restore_after_long_capture();
        self.report_long_capture(Report {
            region: session.region,
            screens,
            width: 0,
            height: 0,
            image: None,
            note,
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
        let (height, bands) = {
            let shared = session.shared.lock();
            shared
                .engine
                .as_ref()
                .map(|engine| (engine.height(), engine.bands()))
                .unwrap_or((0, Bands::NONE))
        };
        if height == 0 {
            return true;
        }
        // The source is a vertical crop of the region, so one screen row is one
        // stitched row and one screen's worth of new rows is the source height.
        let screen_height = body_screen_height(session.source, bands);
        let body_height = height;
        let (trim_start, trim_end) = trim_after_drag(
            edge,
            start,
            screen_y,
            body_height,
            screen_height,
            session.trim_start,
            session.trim_end,
        );
        session.trim_start = trim_start;
        session.trim_end = trim_end;
        session.refresh_preview();
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
        if let Some(weak) = self.ui_slots.toolbar() {
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
    /// Copies the worker's latest thumbnail into the session, so paint only ever
    /// reads a small cached buffer.
    fn refresh_preview(&mut self) {
        let (generation, preview) = {
            let shared = self.shared.lock();
            let preview = shared
                .engine
                .as_ref()
                .map(|engine| engine.preview(self.trim_start, self.trim_end));
            (shared.generation, preview)
        };
        self.generation = generation;
        self.preview = preview;
    }
    /// Removes the helper windows and the sampling timer.
    fn teardown(&self, canvas: &Canvas) {
        unsafe {
            let _ = KillTimer(canvas.hwnd, LONG_CAPTURE_TIMER);
            let _ = DestroyWindow(self.ring);
            let _ = DestroyWindow(self.panel_window);
        }
    }
    /// Asks the worker to stop and waits for it. The longest wait is one sampling
    /// interval, because both backends use it as their blocking timeout.
    fn stop_worker(&mut self, command: u8) {
        self.command.store(command, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
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

/// Worker entry point: open the best grabber, announce the backend and run the
/// loop that belongs to it.
fn run(shared: Arc<Mutex<Shared>>, command: Arc<AtomicU8>, source: RECT) {
    let grabber = match RegionGrabber::open(source) {
        Ok(grabber) => grabber,
        Err(error) => {
            let mut shared = shared.lock();
            shared.note = Some(format!("无法读取选区：{error}"));
            shared.generation += 1;
            return;
        }
    };
    let backend = grabber.backend();
    let width = grabber.width();
    let height = grabber.height();
    {
        let mut shared = shared.lock();
        shared.backend = Some(backend);
        shared.generation += 1;
    }
    let interval = Duration::from_millis(u64::from(TICK_MS));
    match backend {
        GrabBackend::Direct => {
            run_direct(grabber, shared, command, interval, source, width, height)
        }
        GrabBackend::Compatible => {
            run_compatible(grabber, shared, command, interval, width, height)
        }
    }
}

/// Duplication loop: every desktop update is a consistent frame, so it is
/// stitched immediately instead of waiting for the content to stop. That is what
/// keeps an animated page (a spinner, a blinking caret, a video) from starving
/// the stitch.
fn run_direct(
    mut grabber: RegionGrabber,
    shared: Arc<Mutex<Shared>>,
    command: Arc<AtomicU8>,
    interval: Duration,
    source: RECT,
    width: u32,
    height: u32,
) {
    let mut current = Vec::new();
    // An idle desktop produces no duplication frame at all, so the first screen
    // is taken once through GDI. It becomes the first anchor as soon as a real
    // duplication frame arrives, which is also when the fixed bands can be
    // detected; without any update it is still the whole capture.
    let mut previous: Option<Vec<u8>> = None;
    let mut seed = Vec::new();
    if screen_pixels_into(source, &mut seed).is_ok() {
        previous = Some(seed);
    }
    let mut previous_hash: Option<u64> = None;
    loop {
        let requested = command.load(Ordering::Relaxed);
        if requested == CANCEL {
            break;
        }
        let finishing = requested == FINISH;
        let read = grabber.read(
            &mut current,
            if finishing { Duration::ZERO } else { interval },
        );
        let got = match read {
            Ok(got) => got,
            Err(error) => {
                let mut shared = shared.lock();
                shared.note = Some(format!("抓取中断：{error}；已拼部分仍可用"));
                shared.generation += 1;
                break;
            }
        };
        if got {
            let hash = sampled_hash(&current, width, height);
            let changed = previous_hash != Some(hash);
            if changed {
                previous_hash = Some(hash);
                let mut state = shared.lock();
                let frame = frame_of(&current, width, height);
                if state.engine.is_none() {
                    let Some(seed) = previous.as_deref() else {
                        // The GDI seed failed too: start from this frame alone.
                        let stop = !start_engine(&mut state, &current, Bands::NONE, width, height);
                        state.settled = false;
                        state.generation += 1;
                        drop(state);
                        swap_previous(&mut previous, &mut current);
                        if stop || finishing {
                            break;
                        }
                        continue;
                    };
                    let bands = detect_bands(seed, &current, width, height);
                    match LongCapture::new_banded(frame_of(seed, width, height), bands, MAX_BYTES) {
                        Ok(mut engine) => {
                            let step = engine.advance(&frame, None);
                            // A frame that matches the seed is a real scroll (or
                            // a still page with known bands). A stale window or
                            // a jump is dropped and the seed stays the anchor, so
                            // the next frame is compared against the initial
                            // screen again instead of a wrong anchor.
                            let acceptable =
                                matches!(step, Step::Scrolled { .. } | Step::Prepended { .. })
                                    || (matches!(step, Step::Unchanged) && !bands.is_empty());
                            if !acceptable {
                                // Keep the rejection visible if the user ends the
                                // session here; a later valid frame clears it.
                                if !matches!(step, Step::Unchanged) {
                                    state.note = Some(
                                        "滚动跳变：新一屏与起始画面没有足够重叠，已跳过这一屏"
                                            .to_string(),
                                    );
                                    state.generation += 1;
                                }
                                drop(state);
                                continue;
                            }
                            state.engine = Some(engine);
                            state.screens = 1;
                            let stop = apply_step(&mut state, step);
                            state.settled = matches!(step, Step::Unchanged);
                            state.generation += 1;
                            drop(state);
                            swap_previous(&mut previous, &mut current);
                            if stop || finishing {
                                break;
                            }
                            continue;
                        }
                        Err(why) => {
                            state.note = Some(why.to_string());
                            state.generation += 1;
                            drop(state);
                            break;
                        }
                    }
                }
                let direction = state.direction;
                let step = state
                    .engine
                    .as_mut()
                    .expect("engine checked above")
                    .advance(&frame, direction);
                let stop = apply_step(&mut state, step);
                let was_settled = state.settled;
                state.settled = matches!(step, Step::Unchanged);
                if state.settled != was_settled {
                    state.generation += 1;
                }
                drop(state);
                swap_previous(&mut previous, &mut current);
                if stop {
                    break;
                }
            }
        } else {
            let mut state = shared.lock();
            if state.engine.is_some() && !state.settled {
                state.settled = true;
                state.generation += 1;
            }
        }
        if finishing {
            finish_direct(&shared, previous.as_deref(), width, height);
            break;
        }
    }
}

/// Starts from the initial GDI seed when the session ends before duplication
/// delivered any frame.
fn finish_direct(shared: &Arc<Mutex<Shared>>, latest: Option<&[u8]>, width: u32, height: u32) {
    let Some(latest) = latest else {
        return;
    };
    let mut state = shared.lock();
    if state.engine.is_none() {
        start_engine(&mut state, latest, Bands::NONE, width, height);
        state.settled = true;
        state.generation += 1;
    }
}

/// Compatible loop: GDI can hand back a frame in the middle of a repaint, so a
/// frame is stitched only after the scrolling body has been identical for
/// [`STILLS`] samples. The comparison is limited to the body between the fixed
/// bands, so a blinking caret in a sticky bar no longer prevents settling.
fn run_compatible(
    mut grabber: RegionGrabber,
    shared: Arc<Mutex<Shared>>,
    command: Arc<AtomicU8>,
    interval: Duration,
    width: u32,
    height: u32,
) {
    let mut current = Vec::new();
    let mut previous: Option<Vec<u8>> = None;
    let mut previous_hash: Option<u64> = None;
    // The first frame is the screen the user started from. It is kept until the
    // stitch starts so the result never begins in the middle of a scroll.
    let mut seed: Option<Vec<u8>> = None;
    let mut stills = 0u32;
    let mut settled = false;
    let mut bands = Bands::NONE;
    loop {
        let requested = command.load(Ordering::Relaxed);
        if requested == CANCEL {
            break;
        }
        let finishing = requested == FINISH;
        let got = match grabber.read(
            &mut current,
            if finishing { Duration::ZERO } else { interval },
        ) {
            Ok(got) => got,
            Err(error) => {
                let mut shared = shared.lock();
                shared.note = Some(format!("抓取中断：{error}；已拼部分仍可用"));
                shared.generation += 1;
                break;
            }
        };
        if got {
            let hash = sampled_hash(&current, width, height);
            let changed = previous_hash != Some(hash);
            if changed {
                previous_hash = Some(hash);
                let Some(older) = previous.as_deref() else {
                    previous = Some(std::mem::take(&mut current));
                    seed = previous.clone();
                    stills = 1;
                    settled = false;
                    continue;
                };
                let body_same = {
                    let state = shared.lock();
                    match &state.engine {
                        Some(engine) => rows_equal(older, &current, width, height, engine.bands()),
                        None => rows_equal(older, &current, width, height, bands),
                    }
                };
                if body_same {
                    stills += 1;
                } else {
                    stills = 1;
                    settled = false;
                    if shared.lock().engine.is_none() && bands.is_empty() {
                        bands = detect_bands(older, &current, width, height);
                    }
                }
                swap_previous(&mut previous, &mut current);
            } else {
                stills = stills.saturating_add(1);
            }
            let mut state = shared.lock();
            if stills >= STILLS && !settled {
                let frame = previous.as_deref().unwrap_or(&current);
                if state.engine.is_none() {
                    let Some(first) = seed.as_deref() else {
                        drop(state);
                        continue;
                    };
                    match LongCapture::new_banded(frame_of(first, width, height), bands, MAX_BYTES)
                    {
                        Ok(mut engine) => {
                            let step = engine.advance(&frame_of(frame, width, height), None);
                            let acceptable =
                                matches!(step, Step::Scrolled { .. } | Step::Prepended { .. })
                                    || (matches!(step, Step::Unchanged) && !bands.is_empty());
                            if acceptable {
                                state.engine = Some(engine);
                                state.screens = 1;
                                let stop = apply_step(&mut state, step);
                                state.settled = true;
                                state.generation += 1;
                                settled = true;
                                if stop {
                                    drop(state);
                                    break;
                                }
                            } else {
                                // Nothing to stitch yet: the start screen only
                                // repeats itself, or the frame is not a valid
                                // scroll. Wait for the next motion instead of
                                // rebuilding a candidate every tick.
                                if !matches!(step, Step::Unchanged) {
                                    state.note = Some(
                                        "滚动跳变：新一屏与起始画面没有足够重叠，已跳过这一屏"
                                            .to_string(),
                                    );
                                }
                                state.generation += 1;
                                state.settled = true;
                                settled = true;
                            }
                        }
                        Err(why) => {
                            state.note = Some(why.to_string());
                            state.generation += 1;
                            drop(state);
                            break;
                        }
                    }
                } else {
                    let direction = state.direction;
                    let step = state
                        .engine
                        .as_mut()
                        .expect("engine checked above")
                        .advance(&frame_of(frame, width, height), direction);
                    let stop = apply_step(&mut state, step);
                    if !state.settled {
                        state.generation += 1;
                    }
                    state.settled = true;
                    settled = true;
                    if stop {
                        drop(state);
                        break;
                    }
                }
            }
            drop(state);
        }
        if finishing {
            finish_compatible(
                &shared,
                seed.as_deref(),
                previous.as_deref(),
                width,
                height,
                bands,
            );
            break;
        }
    }
}

/// Stitches the newest sample one last time when the user ends the session
/// before the content settled, so the final scroll is not dropped.
fn finish_compatible(
    shared: &Arc<Mutex<Shared>>,
    seed: Option<&[u8]>,
    latest: Option<&[u8]>,
    width: u32,
    height: u32,
    bands: Bands,
) {
    let Some(latest) = latest else {
        return;
    };
    let mut state = shared.lock();
    if state.engine.is_none() {
        start_engine(&mut state, seed.unwrap_or(latest), bands, width, height);
        return;
    }
    let direction = state.direction;
    let step = state
        .engine
        .as_mut()
        .expect("engine checked above")
        .advance(&frame_of(latest, width, height), direction);
    apply_step(&mut state, step);
    state.settled = true;
    state.generation += 1;
}

/// Starts the stitch from `bytes` with the detected bands.
fn start_engine(shared: &mut Shared, bytes: &[u8], bands: Bands, width: u32, height: u32) -> bool {
    match LongCapture::new_banded(frame_of(bytes, width, height), bands, MAX_BYTES) {
        Ok(engine) => {
            shared.engine = Some(engine);
            shared.screens = 1;
            shared.generation += 1;
            true
        }
        Err(why) => {
            shared.note = Some(why.to_string());
            shared.generation += 1;
            false
        }
    }
}

/// Folds one engine step into the session counters. Returns true when the worker
/// should stop growing the stitch.
fn apply_step(shared: &mut Shared, step: Step) -> bool {
    let mut changed = false;
    let stop = match step {
        Step::Scrolled { .. } => {
            shared.screens += 1;
            shared.direction = Some(Direction::Down);
            shared.note = None;
            changed = true;
            false
        }
        Step::Prepended { .. } => {
            shared.screens += 1;
            shared.direction = Some(Direction::Up);
            shared.note = None;
            changed = true;
            false
        }
        Step::Unchanged => {
            if shared.note.take().is_some() {
                changed = true;
            }
            false
        }
        Step::Jumped => {
            let note = "滚动跳变：新一屏与最近几屏没有足够重叠，已跳过这一屏";
            if shared.note.as_deref() != Some(note) {
                shared.note = Some(note.to_string());
                changed = true;
            }
            false
        }
        Step::LowConfidence { best } => {
            let note = format!(
                "匹配置信度不足（最佳重合 {:.0}%），已跳过这一屏；继续滚动或停下结束",
                best * 100.0
            );
            if shared.note.as_deref() != Some(note.as_str()) {
                shared.note = Some(note);
                changed = true;
            }
            false
        }
        Step::Budget(why) => {
            let note = why.to_string();
            if shared.note.as_deref() != Some(note.as_str()) {
                shared.note = Some(note);
                changed = true;
            }
            true
        }
    };
    if changed {
        shared.generation += 1;
    }
    stop
}

fn frame_of(bytes: &[u8], width: u32, height: u32) -> Frame<'_> {
    Frame {
        width,
        height,
        stride: width as usize * 4,
        bytes,
    }
}

/// Puts `current` into `previous` and keeps the old buffer for the next read.
fn swap_previous(previous: &mut Option<Vec<u8>>, current: &mut Vec<u8>) {
    match previous {
        Some(previous) => std::mem::swap(previous, current),
        None => *previous = Some(std::mem::take(current)),
    }
}

/// A cheap content signature over a fixed grid. Equal hashes skip the frame; a
/// collision is harmless because the next stage compares the actual body rows.
fn sampled_hash(bytes: &[u8], width: u32, height: u32) -> u64 {
    const COLUMNS: u32 = 64;
    const ROWS: u32 = 32;
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    if width == 0 || height == 0 {
        return hash;
    }
    for row in 0..ROWS {
        let y = (u64::from(row) * u64::from(height - 1) / u64::from(ROWS - 1)) as u32;
        for column in 0..COLUMNS {
            let x = (u64::from(column) * u64::from(width - 1) / u64::from(COLUMNS - 1)) as u32;
            let at = (y as usize * width as usize + x as usize) * 4;
            for channel in 0..4 {
                hash ^= u64::from(bytes[at + channel]);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    hash
}

/// Whether the scrolling body is identical in both frames. Rows under the fixed
/// bands are compared too, so the caller can settle while they animate.
fn rows_equal(a: &[u8], b: &[u8], width: u32, height: u32, bands: Bands) -> bool {
    let stride = width as usize * 4;
    let start = bands.top as usize * stride;
    let end = (height.saturating_sub(bands.bottom)) as usize * stride;
    if end <= start || a.len() < end || b.len() < end {
        return a == b;
    }
    a[start..end] == b[start..end]
}

/// Whether two rows of the same frame row are effectively the same. Sampled
/// columns and a small tolerance keep a blinking caret or a clock from breaking
/// an otherwise fixed bar, while a row that scrolled away differs in almost
/// every sample.
fn row_similar(a: &[u8], b: &[u8], width: u32) -> bool {
    const COLUMNS: u32 = 48;
    const TOLERANCE: u8 = 12;
    const RATIO: u32 = 9; // nine tenths
    if width == 0 {
        return true;
    }
    let mut same = 0u32;
    for column in 0..COLUMNS {
        let x = (u64::from(column) * u64::from(width - 1) / u64::from(COLUMNS - 1)) as usize;
        let at = x * 4;
        if (0..3).all(|channel| a[at + channel].abs_diff(b[at + channel]) <= TOLERANCE) {
            same += 1;
        }
    }
    same * 10 >= COLUMNS * RATIO
}

/// Fixed rows at the top and bottom: the longest identical prefix and suffix of
/// two frames at different scroll positions. Rows a sticky bar keeps on screen
/// are the only ones that do not move with the page.
fn detect_bands(a: &[u8], b: &[u8], width: u32, height: u32) -> Bands {
    let stride = width as usize * 4;
    if width == 0 || height == 0 || a.len() < height as usize * stride || b.len() < a.len() {
        return Bands::NONE;
    }
    let equal = |row: u32| {
        let at = row as usize * stride;
        row_similar(&a[at..at + stride], &b[at..at + stride], width)
    };
    let mut top = 0u32;
    while top < height && equal(top) {
        top += 1;
    }
    let mut bottom = 0u32;
    while bottom < height - top && equal(height - 1 - bottom) {
        bottom += 1;
    }
    if top < MIN_BAND {
        top = 0;
    }
    if bottom < MIN_BAND {
        bottom = 0;
    }
    // A pair that shares nearly every row is not a scroll at all, and a band that
    // leaves less than the minimum overlap is not something the engine can
    // stitch. Both are better reported as "no fixed rows" than fabricated.
    if top + bottom > height.saturating_sub(MIN_OVERLAP) {
        return Bands::NONE;
    }
    Bands { top, bottom }
}

/// Body rows one screen contributes: the source height minus the fixed bands.
fn body_screen_height(source: RECT, bands: Bands) -> u32 {
    let height = (source.bottom - source.top).max(1) as u32;
    height.saturating_sub(bands.top + bands.bottom).max(1)
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
/// screen space, without clamping. Callers must reject a region that leaves the
/// screen instead of silently narrowing it.
fn selection_region(screen: RECT, selection: ImageRect) -> RECT {
    RECT {
        left: screen.left + selection.x as i32,
        top: screen.top + selection.y as i32,
        right: screen.left + (selection.x + selection.width) as i32,
        bottom: screen.top + (selection.y + selection.height) as i32,
    }
}

/// Whether `rect` lies entirely inside `screen`.
fn screen_contains(screen: RECT, rect: RECT) -> bool {
    rect.left >= screen.left
        && rect.top >= screen.top
        && rect.right <= screen.right
        && rect.bottom <= screen.bottom
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
    let direction = session.shared.lock().direction;
    let back = CreateSolidBrush(BAND_BACK);
    FillRect(hdc, &client, back);
    let _ = DeleteObject(back);
    SetBkMode(hdc, TRANSPARENT);
    let old = SelectObject(hdc, GetStockObject(DEFAULT_GUI_FONT));
    // The last screen is at the bottom when the content scrolled down and at the
    // top when it scrolled up, so that is the edge that trims it.
    let (top_edge, hint) = match direction {
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

/// Paints the bird's eye: how much has been stitched, the cached thumbnail with
/// the trim applied, and what to do next. Everything the user can press lives on
/// the toolbar, so the panel carries no buttons.
unsafe fn paint_panel(hdc: HDC, canvas: &Canvas, hwnd: HWND) {
    let Some(session) = canvas.long_capture.as_ref() else {
        return;
    };
    let mut client = RECT::default();
    if GetClientRect(hwnd, &mut client).is_err() {
        return;
    }
    let (screens, width, total_height, note, settled, backend) = {
        let shared = session.shared.lock();
        let (width, total_height) = shared
            .engine
            .as_ref()
            .map(|engine| (engine.width(), engine.total_height()))
            .unwrap_or((0, 0));
        (
            shared.screens,
            width,
            total_height,
            shared.note.clone(),
            shared.settled,
            shared.backend,
        )
    };
    let back = CreateSolidBrush(PANEL_BACK);
    FillRect(hdc, &client, back);
    let _ = DeleteObject(back);
    SetBkMode(hdc, TRANSPARENT);
    let old = SelectObject(hdc, GetStockObject(DEFAULT_GUI_FONT));
    let shown_height = total_height.saturating_sub(session.trim_start + session.trim_end);
    let title = if total_height == 0 {
        "正在读取选区…".to_string()
    } else {
        format!("已拼 {screens} 屏 · {width} × {shown_height}")
    };
    // A note from the engine (a jump, a low-confidence frame, a full budget)
    // outranks the usual hint: it explains why the stitch stopped growing.
    let (hint, hint_colour): (String, COLORREF) = match &note {
        Some(note) => (note.clone(), PANEL_WARN),
        None if settled => (format!("已停下 · 上限 {MAX_HEIGHT} 行"), PANEL_HINT),
        None if backend == Some(GrabBackend::Compatible) => {
            ("兼容采集 · 画面停下后拼接".to_string(), PANEL_HINT)
        }
        None => ("把鼠标放进选区滚动 · 停下后拼接".to_string(), PANEL_HINT),
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
    if let Some((image_width, image_height, bytes)) = &session.preview {
        if *image_width > 0 && *image_height > 0 && !bytes.is_empty() {
            let area_width = (area.right - area.left).max(1) as f32;
            let area_height = (area.bottom - area.top).max(1) as f32;
            let scale = (area_width / *image_width as f32).min(area_height / *image_height as f32);
            let width = ((*image_width as f32 * scale) as i32).clamp(1, area.right - area.left);
            let height = ((*image_height as f32 * scale) as i32).clamp(1, area.bottom - area.top);
            let x = area.left + (area.right - area.left - width) / 2;
            let y = area.top + (area.bottom - area.top - height) / 2;
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: *image_width as i32,
                    // A negative height keeps the rows top-down.
                    biHeight: -(*image_height as i32),
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
                *image_width as i32,
                *image_height as i32,
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
    fn a_selection_outside_the_screen_is_rejected_instead_of_clamped() {
        let screen = rect(0, 0, 1920, 1080);
        let selection = ImageRect {
            x: 1900.0,
            y: 100.0,
            width: 100.0,
            height: 100.0,
        };
        let region = selection_region(screen, selection);
        assert!(!screen_contains(screen, region));
        // The old path clamped the right edge and silently lost 80 columns.
        assert_eq!(region.right, 2000);
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
    fn the_sticky_header_is_detected_between_two_scroll_positions() {
        let bands = detect_bands(
            &FakePage::view(0),
            &FakePage::view(FakePage::SCROLL),
            FakePage::WIDTH,
            FakePage::HEIGHT,
        );
        assert_eq!(
            bands,
            Bands {
                top: FakePage::HEADER,
                bottom: 0
            }
        );
        // A pair with no identical rows at all has no fixed rows either.
        let mut other = FakePage::view(0);
        for row in 0..FakePage::HEIGHT {
            let at = row as usize * FakePage::WIDTH as usize * 4;
            other[at..at + 4].copy_from_slice(&[9, 8, 7, 255]);
        }
        assert_eq!(
            detect_bands(
                &FakePage::view(0),
                &other,
                FakePage::WIDTH,
                FakePage::HEIGHT
            ),
            Bands::NONE
        );
    }

    #[test]
    fn the_stitch_keeps_every_row_of_a_scrolling_page() {
        let bands = Bands {
            top: FakePage::HEADER,
            bottom: 0,
        };
        let mut engine = LongCapture::new_banded(
            frame_of(&FakePage::view(0), FakePage::WIDTH, FakePage::HEIGHT),
            bands,
            MAX_BYTES,
        )
        .unwrap();
        let mut screens = 1;
        let mut direction = None;
        for index in 1..FakePage::FRAMES {
            let bytes = FakePage::view(index * FakePage::SCROLL);
            let step = engine.advance(
                &frame_of(&bytes, FakePage::WIDTH, FakePage::HEIGHT),
                direction,
            );
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
                other => panic!("the synthetic page only scrolls: {other:?}"),
            }
        }
        // The last screen is read twice, which is how the session sees the end of
        // the content.
        let bytes = FakePage::view((FakePage::FRAMES - 1) * FakePage::SCROLL);
        assert!(matches!(
            engine.advance(
                &frame_of(&bytes, FakePage::WIDTH, FakePage::HEIGHT),
                direction
            ),
            Step::Unchanged
        ));
        assert_eq!(screens, FakePage::FRAMES);
        let expected_body =
            FakePage::HEIGHT - FakePage::HEADER + (FakePage::FRAMES - 1) * FakePage::SCROLL;
        assert_eq!(engine.height(), expected_body);
        assert_eq!(engine.width(), FakePage::WIDTH);
        // Every row has to land in its own place: a missing or duplicated row would
        // shift every row below it.
        let (image, bytes) = engine.cropped(0, 0, ImageId(1));
        assert_eq!(image.height, FakePage::HEADER + expected_body);
        let stride = engine.stride();
        let colour_at = |y: u32| {
            let at = y as usize * stride + 100 * 4;
            &bytes[at..at + 4]
        };
        for y in 0..FakePage::HEADER {
            assert_eq!(colour_at(y), FakePage::header_colour(y), "header row {y}");
        }
        for y in 0..expected_body {
            assert_eq!(
                colour_at(FakePage::HEADER + y),
                FakePage::content_colour(y),
                "content row {y}"
            );
        }
    }

    #[test]
    fn the_preview_follows_the_stitch_and_the_trim() {
        let bands = Bands {
            top: FakePage::HEADER,
            bottom: 0,
        };
        let mut engine = LongCapture::new_banded(
            frame_of(&FakePage::view(0), FakePage::WIDTH, FakePage::HEIGHT),
            bands,
            MAX_BYTES,
        )
        .unwrap();
        for index in 1..FakePage::FRAMES {
            engine.push(&frame_of(
                &FakePage::view(index * FakePage::SCROLL),
                FakePage::WIDTH,
                FakePage::HEIGHT,
            ));
        }
        let (width, height, bytes) = engine.preview(0, 0);
        assert_eq!(width, engine.thumbnail().0);
        assert!(width <= FakePage::WIDTH);
        assert!(height > 0);
        assert_eq!(bytes.len(), width as usize * height as usize * 4);
        let (trimmed_width, trimmed_height, _) = engine.preview(100, 0);
        assert_eq!(trimmed_width, width);
        assert!(trimmed_height <= height);
    }

    #[test]
    fn sampling_sees_a_body_change_but_not_the_cursor() {
        let a = FakePage::view(0);
        let mut b = a.clone();
        assert_eq!(
            sampled_hash(&a, FakePage::WIDTH, FakePage::HEIGHT),
            sampled_hash(&b, FakePage::WIDTH, FakePage::HEIGHT)
        );
        // A change inside the body is visible to the sampled hash. Use a pixel
        // the 64×32 grid actually samples.
        let y = (4 * (FakePage::HEIGHT - 1) / 31) as usize;
        let x = ((FakePage::WIDTH - 1) / 63) as usize;
        let at = (y * FakePage::WIDTH as usize + x) * 4;
        b[at] ^= 0xff;
        assert_ne!(
            sampled_hash(&a, FakePage::WIDTH, FakePage::HEIGHT),
            sampled_hash(&b, FakePage::WIDTH, FakePage::HEIGHT)
        );
        // The body comparison ignores the sticky header, which is where a caret
        // would blink.
        let bands = Bands {
            top: FakePage::HEADER,
            bottom: 0,
        };
        let mut header_b = a.clone();
        header_b[4] ^= 0xff;
        assert!(rows_equal(
            &a,
            &header_b,
            FakePage::WIDTH,
            FakePage::HEIGHT,
            bands
        ));
    }

    #[test]
    fn a_banded_screen_trims_only_its_body() {
        let source = rect(200, 200, 700, 800);
        assert_eq!(
            body_screen_height(
                source,
                Bands {
                    top: 40,
                    bottom: 30
                }
            ),
            530
        );
        assert_eq!(body_screen_height(source, Bands::NONE), 600);
        // A pathological band pair still leaves one row to trim from.
        assert_eq!(
            body_screen_height(
                source,
                Bands {
                    top: 590,
                    bottom: 30
                }
            ),
            1
        );
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

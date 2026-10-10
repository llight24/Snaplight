use snaplight_domain::*;
use snaplight_platform::{
    atomic_write, capture_dir, data_dir, load_config, load_window_state, save_config,
    save_window_state, write_pixels, CaptureService,
};
use snaplight_render_win::*;
use snaplight_ui::{ComponentHandle, ModelRc, PhysicalPosition, Toolbar, VecModel};
use std::{
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
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

const COMMAND: u32 = WM_APP + 42;
/// Frames rendered by the `--stress` 4K annotation loop.
const STRESS_FRAMES: usize = 240;
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
    active: Arc<Mutex<usize>>,
    error: Option<String>,
    // Immutable base image pixels (BGRA8, `doc.image.stride`) so GPU resources can
    // be rebuilt after device loss without changing the captured content.
    base: Vec<u8>,
    screen: RECT,
    device_rebuilds: u32,
    capture_dir: PathBuf,
    toolbar: Arc<ToolbarSlot>,
    dpi: u32,
}
impl Canvas {
    fn draw(&mut self) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let viewport = Viewport {
            width: self.doc.image.width,
            height: self.doc.image.height,
            dpi: 96.0,
        };
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
    fn rebuild_device(&mut self) -> std::result::Result<(), Box<dyn std::error::Error>> {
        self.renderer
            .rebuild(&self.base, &self.doc.image, self.screen)?;
        self.device_rebuilds += 1;
        Ok(())
    }
}
impl Canvas {
    unsafe fn invalidate(&self) {
        let _ = InvalidateRect(self.hwnd, None, false);
    }
    fn position(&self, l: LPARAM) -> ImagePoint {
        ImagePoint {
            x: (l.0 as u16 as i16 as f32).clamp(0.0, self.doc.image.width as f32),
            y: ((l.0 >> 16) as u16 as i16 as f32).clamp(0.0, self.doc.image.height as f32),
        }
    }
    fn update_drag(&mut self, p: ImagePoint) {
        let Some(start) = self.start else {
            return;
        };
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
            }
            10 => {
                self.history.undo(&mut self.doc);
                self.renderer.invalidate_annotations();
            }
            11 => {
                self.history.redo(&mut self.doc);
                self.renderer.invalidate_annotations();
            }
            12..=14 => {
                let (image, bytes) = self.renderer.export(&self.doc)?;
                if id == 12 {
                    clipboard(self.hwnd, &image, &bytes)?;
                } else {
                    let folder = self.capture_dir.clone();
                    std::fs::create_dir_all(&folder)?;
                    let key = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
                    let stem = folder.join(format!("{key}"));
                    let original = self.renderer.original(&self.doc)?;
                    write_pixels(
                        &stem.with_extension("source.bgra"),
                        &self.doc.image,
                        &original,
                    )?;
                    drop(original);
                    atomic_write(
                        &stem.with_extension("document.json"),
                        &serde_json::to_vec_pretty(&self.doc)?,
                    )?;
                    if id == 13 {
                        write_png(&stem.with_extension("png"), &image, &bytes)?;
                    } else {
                        let path = stem.with_extension("bgra");
                        write_pixels(&path, &image, &bytes)?;
                        let flattened = Document {
                            version: 1,
                            image,
                            selection: None,
                            annotations: vec![],
                        };
                        let doc_path = stem.with_extension("pin.json");
                        atomic_write(&doc_path, &serde_json::to_vec(&flattened)?)?;
                        let origin = self.renderer.export_origin(&self.doc);
                        let screen = ScreenPoint {
                            x: self.screen.left + origin.x as i32,
                            y: self.screen.top + origin.y as i32,
                        };
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
                    }
                }
            }
            15 => unsafe {
                PostQuitMessage(0);
            },
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
                *state.active.lock().unwrap() = hwnd.0 as usize;
                let p = state.position(l);
                state.interaction.cursor = Some(p);
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
                    SetCapture(hwnd);
                    state.start = Some(p);
                    state.interaction.dragging = true;
                    state.update_drag(p);
                    state.invalidate();
                }
                return LRESULT(0);
            }
            WM_MOUSEMOVE => {
                let p = state.position(l);
                state.interaction.cursor = Some(p);
                state.update_drag(p);
                state.invalidate();
                return LRESULT(0);
            }
            WM_LBUTTONUP => {
                if state.start.is_some() {
                    state.update_drag(state.position(l));
                    if let Some(r) = state.interaction.selection_draft.take() {
                        if r.width >= 1.0 && r.height >= 1.0 {
                            state
                                .history
                                .execute(&mut state.doc, Edit::Selection(Some(r)));
                            publish_selection(state, r);
                        }
                    } else if let Some(a) = state.interaction.draft.take() {
                        state.history.execute(&mut state.doc, Edit::Add(a));
                        state.renderer.invalidate_annotations();
                    }
                }
                state.start = None;
                state.interaction.dragging = false;
                let _ = ReleaseCapture();
                state.invalidate();
                return LRESULT(0);
            }
            WM_CAPTURECHANGED => {
                state.start = None;
                state.interaction.draft = None;
                state.interaction.selection_draft = None;
                state.interaction.dragging = false;
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
    toolbar: Arc<ToolbarSlot>,
    smoke: bool,
    stress: bool,
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
            let wc = WNDCLASSW {
                hInstance: instance.into(),
                lpszClassName: class,
                lpfnWndProc: Some(wndproc),
                hCursor: LoadCursorW(None, IDC_CROSS)?,
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
                    active: active.clone(),
                    error: None,
                    base,
                    screen: s,
                    device_rebuilds: 0,
                    capture_dir: capture_dir(&config),
                    toolbar: toolbar.clone(),
                    dpi: window_dpi.max(96),
                });
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&mut *canvas as *mut Canvas) as isize);
                if *active.lock().unwrap() == 0 {
                    *active.lock().unwrap() = hwnd.0 as usize;
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
                    canvas.renderer.render(
                        &canvas.doc,
                        Viewport {
                            width: frame.descriptor.width,
                            height: frame.descriptor.height,
                            dpi: 96.0,
                        },
                        &Interaction::default(),
                    )?;
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
                                Viewport {
                                    width: frame.descriptor.width,
                                    height: frame.descriptor.height,
                                    dpi: 96.0,
                                },
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
                }
                canvases.push(canvas);
            }
            let thread_id = GetCurrentThreadId();
            let _ = ready.send(Ok(CaptureReady {
                thread_id,
                bounds,
                dpi: dpi.max(96),
            }));
            if !smoke {
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
                        let commands = std::mem::take(&mut *queue.lock().unwrap());
                        for command in commands {
                            match command {
                                UiCommand::Action(id) => {
                                    if id <= 6 {
                                        for c in &mut canvases {
                                            let _ = c.action(id);
                                        }
                                    } else if let Some(c) = canvases
                                        .iter_mut()
                                        .find(|c| c.hwnd.0 as usize == *active.lock().unwrap())
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
/// Toolbar handle shared with the canvas thread so it can follow the selection
/// without touching Slint from the wrong thread.
type ToolbarSlot = OnceLock<snaplight_ui::ToolbarWeak>;
/// Captured desktop bounds, monitor DPI, and the canvas thread id.
#[derive(Clone, Copy)]
struct CaptureReady {
    thread_id: u32,
    bounds: RECT,
    dpi: u32,
}
/// Places the toolbar inside the captured region when the user has never moved
/// it: bottom-left, so it stays visible on a full-screen capture.
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
/// Moves the toolbar under the completed selection and reports its pixel size.
fn publish_selection(canvas: &Canvas, selection: ImageRect) {
    let Some(weak) = canvas.toolbar.get() else {
        return;
    };
    let anchor = RECT {
        left: canvas.screen.left + selection.x as i32,
        top: canvas.screen.top + selection.y as i32,
        right: canvas.screen.left + (selection.x + selection.width) as i32,
        bottom: canvas.screen.top + (selection.y + selection.height) as i32,
    };
    let size = snaplight_ui::scale_to_physical(snaplight_ui::TOOLBAR_LOGICAL_SIZE, canvas.dpi);
    let (x, y) = follow_position(canvas.screen, anchor, size);
    snaplight_ui::move_toolbar(weak, PhysicalPosition::new(x, y));
    snaplight_ui::set_toolbar_status(
        weak,
        format!(
            "{} × {}",
            selection.width.round() as u32,
            selection.height.round() as u32
        ),
    );
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
    ui.set_captures(ModelRc::new(VecModel::from(capture_items(
        &capture_dir_from_field(&ui.get_capture_dir_value()),
    ))));
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
            ui.set_status(format!("Press Confirm to move {id} to the recycle bin").into());
            return;
        }
        ui.set_pending_delete(Default::default());
        let folder = capture_dir_from_field(&ui.get_capture_dir_value());
        let files = snaplight_platform::capture_files(&folder, &id);
        let removed = files.len();
        match snaplight_platform::native::delete_to_recycle_bin(&files) {
            Ok(()) => {
                ui.set_captures(ModelRc::new(VecModel::from(capture_items(&folder))));
                ui.set_status(format!("Moved {removed} files to the recycle bin").into());
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
        // Normalize and validate the shortcuts before anything is written.
        let hotkeys = (|| -> std::result::Result<(), &'static str> {
            let capture = parse_hotkey(ui.get_capture_hotkey_value().trim())
                .map_err(|_| "invalid capture hotkey")?
                .canonical();
            let restore = parse_hotkey(ui.get_restore_hotkey_value().trim())
                .map_err(|_| "invalid restore hotkey")?
                .canonical();
            ui.set_capture_hotkey_value(capture.clone().into());
            ui.set_restore_hotkey_value(restore.clone().into());
            c.capture_hotkey = capture;
            c.restore_hotkey = restore;
            Ok(())
        })();
        if let Err(message) = hotkeys {
            ui.set_status(format!("{message}; nothing saved").into());
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
                ui.set_status("Saved".into())
            }
            Err(e) => ui.set_status(e.to_string().into()),
        }
    });
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
    let queue = Arc::new(Mutex::new(vec![]));
    let thread_queue = queue.clone();
    let toolbar_state = Arc::new(OnceLock::<snaplight_ui::ToolbarWeak>::new());
    let thread_toolbar = toolbar_state.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let result = canvas_thread(
            config,
            thread_queue,
            tx.clone(),
            thread_toolbar,
            smoke,
            stress,
        );
        if let Err(e) = &result {
            let _ = tx.send(Err(e.clone()));
        }
        if !smoke {
            snaplight_ui::invoke(snaplight_ui::quit);
        }
        result
    });
    let ready = rx.recv()??;
    let id = ready.thread_id;
    if !smoke {
        let config = load_config()?;
        let ui: Toolbar = snaplight_ui::toolbar(&config)?;
        let _ = toolbar_state.set(ui.as_weak());
        let size = snaplight_ui::scale_to_physical(snaplight_ui::TOOLBAR_LOGICAL_SIZE, ready.dpi);
        let remembered = load_window_state().toolbar;
        if let Some(p) = remembered {
            ui.window().set_position(PhysicalPosition::new(p.x, p.y));
        } else {
            let (x, y) = initial_toolbar_position(ready.bounds, size);
            ui.window().set_position(PhysicalPosition::new(x, y));
        }
        let q = queue.clone();
        ui.on_command(move |action| {
            q.lock().unwrap().push(UiCommand::Action(action));
            unsafe {
                let _ = PostThreadMessageW(id, COMMAND, WPARAM(0), LPARAM(0));
            }
        });
        let q = queue.clone();
        ui.on_properties_changed(move |width, block, text, swatch| {
            q.lock().unwrap().push(UiCommand::Properties {
                width,
                block,
                text: text.into(),
                swatch,
            });
            unsafe {
                let _ = PostThreadMessageW(id, COMMAND, WPARAM(0), LPARAM(0));
            }
        });
        let result = ui.run();
        remember_position(false, ui.window());
        queue.lock().unwrap().push(UiCommand::Action(15));
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
        let expected =
            snaplight_ui::scale_to_physical(snaplight_ui::TOOLBAR_LOGICAL_SIZE, ready.dpi);
        if physical != expected {
            return Err(format!(
                "toolbar physical size {physical:?} does not match dpi-scaled {expected:?} at {} dpi",
                ready.dpi
            )
            .into());
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
        let file = std::fs::File::create(data_dir().join("toolbar-smoke.png"))?;
        let mut encoder = png::Encoder::new(file, snapshot.width(), snapshot.height());
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()?
            .write_image_data(snapshot.as_bytes())?;
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
        // The settings page is part of the UI surface check as well.
        let ui = snaplight_ui::settings(&config)?;
        ui.set_capture_dir_value(capture_dir(&config).to_string_lossy().into_owned().into());
        ui.set_captures(ModelRc::new(VecModel::from(capture_items(
            &capture_dir_from_field(&ui.get_capture_dir_value()),
        ))));
        ui.show()?;
        let snapshot = ui.window().take_snapshot()?;
        let file = std::fs::File::create(data_dir().join("settings-smoke.png"))?;
        let mut encoder = png::Encoder::new(file, snapshot.width(), snapshot.height());
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()?
            .write_image_data(snapshot.as_bytes())?;
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

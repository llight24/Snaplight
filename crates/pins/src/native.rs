use snaplight_domain::*;
use snaplight_platform::{read_pixels, Capabilities, WindowService};
use snaplight_render_win::{Gpu, StaticSurface};
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{Com::*, LibraryLoader::*},
        UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
};

const INCOMING: u32 = WM_APP + 51;
const CLOSED: u32 = WM_APP + 52;
struct Pin {
    hwnd: HWND,
    control: HWND,
    gpu: Option<Gpu>,
    surface: Option<StaticSurface>,
    path: PathBuf,
    image: ImageDescriptor,
    closed: bool,
    through: bool,
    scale: f32,
    opacity: f32,
}
impl Pin {
    unsafe fn restore(&mut self) -> std::result::Result<(), Box<dyn std::error::Error>> {
        if self.through || self.gpu.is_none() {
            return self.update_layered();
        }
        if let Some(surface) = &self.surface {
            surface.scale(self.scale)?;
            surface.opacity(self.opacity)?;
            return Ok(());
        }
        let (image, bytes) = read_pixels(&self.path)?;
        let surface = StaticSurface::new(self.gpu.as_ref().unwrap(), self.hwnd, &image, &bytes)?;
        surface.scale(self.scale)?;
        surface.opacity(self.opacity)?;
        self.surface = Some(surface);
        Ok(())
    }
    unsafe fn update_layered(&mut self) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let (image, mut bytes) = read_pixels(&self.path)?;
        let width = (image.width as f32 * self.scale).max(1.0) as u32;
        let height = (image.height as f32 * self.scale).max(1.0) as u32;
        if width != image.width || height != image.height {
            let size = (width as usize)
                .checked_mul(height as usize)
                .and_then(|n| n.checked_mul(4))
                .ok_or("scaled image overflow")?;
            let mut scaled = vec![0; size];
            for y in 0..height {
                for x in 0..width {
                    let sx = (x as u64 * image.width as u64 / width as u64) as usize;
                    let sy = (y as u64 * image.height as u64 / height as u64) as usize;
                    let src = sy * image.stride as usize + sx * 4;
                    let dest = (y as usize * width as usize + x as usize) * 4;
                    scaled[dest..dest + 4].copy_from_slice(&bytes[src..src + 4]);
                }
            }
            bytes = scaled;
        }
        let desktop = GetDC(None);
        let memory = CreateCompatibleDC(desktop);
        let mut bits = std::ptr::null_mut();
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: 40,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let bitmap = match CreateDIBSection(desktop, &info, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(v) => v,
            Err(e) => {
                let _ = DeleteDC(memory);
                ReleaseDC(None, desktop);
                return Err(e.into());
            }
        };
        let old = SelectObject(memory, bitmap);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), bits.cast(), bytes.len());
        let mut screen = RECT::default();
        GetWindowRect(self.hwnd, &mut screen)?;
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: (self.opacity * 255.0) as u8,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let result = UpdateLayeredWindow(
            self.hwnd,
            desktop,
            Some(&POINT {
                x: screen.left,
                y: screen.top,
            }),
            Some(&SIZE {
                cx: width as i32,
                cy: height as i32,
            }),
            memory,
            Some(&POINT::default()),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
        SelectObject(memory, old);
        let _ = DeleteObject(bitmap);
        let _ = DeleteDC(memory);
        ReleaseDC(None, desktop);
        result?;
        Ok(())
    }
    unsafe fn toggle(&mut self) -> std::result::Result<(), Box<dyn std::error::Error>> {
        self.surface = None;
        self.through = !self.through;
        let style = WS_EX_TOPMOST
            | WS_EX_TOOLWINDOW
            | WS_EX_NOACTIVATE
            | if self.through {
                WS_EX_LAYERED | WS_EX_TRANSPARENT
            } else if self.gpu.is_some() {
                WS_EX_NOREDIRECTIONBITMAP
            } else {
                WS_EX_LAYERED
            };
        SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, style.0 as isize);
        if self.through && self.gpu.is_some() {
            self.scale = 1.0;
        }
        SetWindowPos(
            self.hwnd,
            HWND_TOPMOST,
            0,
            0,
            (self.image.width as f32 * self.scale) as i32,
            (self.image.height as f32 * self.scale) as i32,
            SWP_NOMOVE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )?;
        self.restore()
    }
    unsafe fn zoom(&mut self, value: f32) -> std::result::Result<(), Box<dyn std::error::Error>> {
        if self.through {
            return Ok(());
        }
        self.scale = value.clamp(0.1, 4.0);
        if let Some(surface) = &self.surface {
            surface.scale(self.scale)?;
        } else {
            self.update_layered()?;
        }
        SetWindowPos(
            self.hwnd,
            HWND_TOPMOST,
            0,
            0,
            (self.image.width as f32 * self.scale).max(1.0) as i32,
            (self.image.height as f32 * self.scale).max(1.0) as i32,
            SWP_NOMOVE | SWP_NOACTIVATE,
        )?;
        Ok(())
    }
}
struct NativeWindows;
impl WindowService for NativeWindows {
    type Window = HWND;
    type Error = Error;
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            capture: true,
            global_hotkeys: true,
            positioning: true,
            topmost: true,
            click_through: true,
        }
    }
    fn set_click_through(&self, window: &HWND, enabled: bool) -> Result<()> {
        unsafe {
            let ptr = GetWindowLongPtrW(*window, GWLP_USERDATA) as *mut Pin;
            if ptr.is_null() {
                return Err(Error::new(E_INVALIDARG, "unknown pin window"));
            }
            let pin = &mut *ptr;
            if pin.through != enabled {
                pin.toggle()
                    .map_err(|e| Error::new(E_FAIL, e.to_string()))?;
            }
            Ok(())
        }
    }
}
unsafe extern "system" fn pin_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Pin;
    if !ptr.is_null() {
        let pin = &mut *ptr;
        let action = (|| -> std::result::Result<Option<LRESULT>, Box<dyn std::error::Error>> {
            match msg {
                WM_PAINT => {
                    let _ = ValidateRect(hwnd, None);
                    return Ok(Some(LRESULT(0)));
                }
                WM_ERASEBKGND => return Ok(Some(LRESULT(1))),
                WM_MOUSEACTIVATE => return Ok(Some(LRESULT(MA_NOACTIVATE as isize))),
                WM_NCHITTEST => return Ok(Some(LRESULT(HTCAPTION as isize))),
                WM_NCLBUTTONDBLCLK => {
                    pin.zoom(1.0)?;
                    return Ok(Some(LRESULT(0)));
                }
                WM_MOUSEWHEEL => {
                    let delta = ((w.0 >> 16) as u16 as i16) as f32 / 120.0;
                    if GetKeyState(VK_CONTROL.0 as i32) < 0 {
                        pin.opacity = (pin.opacity + delta * 0.05).clamp(0.1, 1.0);
                        if let Some(surface) = &pin.surface {
                            surface.opacity(pin.opacity)?;
                        } else {
                            pin.update_layered()?;
                        }
                    } else {
                        pin.zoom(pin.scale * 1.1f32.powf(delta))?;
                    }
                    return Ok(Some(LRESULT(0)));
                }
                WM_CONTEXTMENU => {
                    let menu = CreatePopupMenu()?;
                    AppendMenuW(menu, MF_STRING, 1, w!("鼠标穿透（Ctrl+Shift+P 恢复）"))?;
                    AppendMenuW(menu, MF_STRING, 2, w!("原始大小"))?;
                    AppendMenuW(menu, MF_STRING, 3, w!("缩小一半"))?;
                    AppendMenuW(menu, MF_STRING, 4, w!("关闭"))?;
                    let mut p = POINT::default();
                    GetCursorPos(&mut p)?;
                    let choice = TrackPopupMenu(
                        menu,
                        TPM_RETURNCMD | TPM_RIGHTBUTTON,
                        p.x,
                        p.y,
                        0,
                        hwnd,
                        None,
                    )
                    .0;
                    DestroyMenu(menu)?;
                    match choice {
                        1 => pin.toggle()?,
                        2 => pin.zoom(1.0)?,
                        3 => pin.zoom(0.5)?,
                        4 => {
                            PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0))?;
                        }
                        _ => {}
                    }
                    return Ok(Some(LRESULT(0)));
                }
                WM_SHOWWINDOW => {
                    if w.0 == 0 {
                        pin.surface = None;
                    } else if pin.surface.is_none() {
                        pin.restore()?;
                    }
                }
                WM_CLOSE => {
                    pin.surface = None;
                    pin.closed = true;
                    let control = pin.control;
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    DestroyWindow(hwnd)?;
                    PostMessageW(control, CLOSED, WPARAM(0), LPARAM(0))?;
                    return Ok(Some(LRESULT(0)));
                }
                WM_DESTROY => {
                    pin.surface = None;
                    pin.closed = true;
                    PostMessageW(pin.control, CLOSED, WPARAM(0), LPARAM(0))?;
                    return Ok(Some(LRESULT(0)));
                }
                _ => {}
            }
            Ok(None)
        })();
        match action {
            Ok(Some(result)) => return result,
            Err(e) => {
                snaplight_platform::native::show_error(&e.to_string());
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, w, l)
}
struct Host {
    gpu: Option<Gpu>,
    // HWND user data stores these addresses; vector growth must not move a Pin.
    #[allow(clippy::vec_box)]
    pins: Vec<Box<Pin>>,
    queue: Arc<Mutex<Vec<PinCommand>>>,
}
impl Host {
    unsafe fn open(
        &mut self,
        control: HWND,
        path: PathBuf,
        document: PathBuf,
        screen: ScreenPoint,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let doc: Document = serde_json::from_slice(&std::fs::read(document)?)?;
        if doc.version != 1 {
            return Err("unsupported pin document version".into());
        }
        let (image, bytes) = read_pixels(&path)?;
        let instance = GetModuleHandleW(None)?;
        // The pin opens where the region was captured, so pinning does not move
        // the image. The only adjustment is the width clamp for regions wider
        // than the primary display, which cannot be shown unscaled.
        let width = image.width.min(GetSystemMetrics(SM_CXSCREEN) as u32);
        let scale = width as f32 / image.width as f32;
        let window_backend = if self.gpu.is_some() {
            WS_EX_NOREDIRECTIONBITMAP
        } else {
            WS_EX_LAYERED
        };
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | window_backend,
            w!("Snaplight.Pin"),
            w!("Snaplight Pin"),
            WS_POPUP,
            screen.x,
            screen.y,
            width as i32,
            (image.height as f32 * scale) as i32,
            None,
            None,
            instance,
            None,
        )?;
        let surface = if let Some(gpu) = &self.gpu {
            match StaticSurface::new(gpu, hwnd, &image, &bytes) {
                Ok(v) => {
                    v.scale(scale)?;
                    Some(v)
                }
                Err(e) => {
                    let _ = DestroyWindow(hwnd);
                    return Err(e.into());
                }
            }
        } else {
            None
        };
        drop(bytes);
        let mut pin = Box::new(Pin {
            hwnd,
            control,
            gpu: self.gpu.clone(),
            surface,
            path,
            image,
            closed: false,
            through: false,
            scale,
            opacity: 1.0,
        });
        if pin.gpu.is_none() {
            pin.update_layered()?;
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&mut *pin as *mut Pin) as isize);
        self.pins.push(pin);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        Ok(())
    }
    unsafe fn close_all(&mut self) {
        for pin in &mut self.pins {
            if !pin.closed {
                pin.surface = None;
                pin.closed = true;
                SetWindowLongPtrW(pin.hwnd, GWLP_USERDATA, 0);
                let _ = DestroyWindow(pin.hwnd);
            }
        }
    }
}
unsafe extern "system" fn control_proc(hwnd: HWND, msg: u32, _w: WPARAM, l: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Host;
    if !ptr.is_null() {
        let host = &mut *ptr;
        match msg {
            INCOMING => {
                let commands = std::mem::take(&mut *host.queue.lock().unwrap());
                for command in commands {
                    match command {
                        PinCommand::Open {
                            image,
                            document,
                            screen,
                        } => {
                            if let Err(e) = host.open(hwnd, image, document, screen) {
                                snaplight_platform::native::show_error(&e.to_string());
                            }
                        }
                        PinCommand::CloseAll | PinCommand::Shutdown => {
                            host.close_all();
                            PostQuitMessage(0);
                        }
                    }
                }
                return LRESULT(0);
            }
            CLOSED => {
                host.pins.retain(|p| !p.closed);
                if host.pins.is_empty() {
                    PostQuitMessage(0);
                }
                return LRESULT(0);
            }
            WM_HOTKEY => {
                for pin in &mut host.pins {
                    if pin.through {
                        let _ = NativeWindows.set_click_through(&pin.hwnd, false);
                    }
                }
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, _w, l)
}
/// Registers the configured pin-restore shortcut, falling back to the default
/// when another application already owns the configured combination.
unsafe fn register_restore_hotkey(
    hwnd: HWND,
    config: &Config,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let hotkey = parse_hotkey(&config.restore_hotkey)?;
    let (modifiers, key) = snaplight_platform::native::hotkey_registration(&hotkey);
    if let Err(error) = RegisterHotKey(hwnd, 2, modifiers, key) {
        let fallback = parse_hotkey(DEFAULT_RESTORE_HOTKEY).expect("default restore hotkey");
        let (modifiers, key) = snaplight_platform::native::hotkey_registration(&fallback);
        eprintln!(
            "restore hotkey {} unavailable ({error}); using {DEFAULT_RESTORE_HOTKEY}",
            config.restore_hotkey
        );
        RegisterHotKey(hwnd, 2, modifiers, key)?;
    }
    Ok(())
}
pub fn run() -> std::result::Result<(), Box<dyn std::error::Error>> {
    snaplight_platform::native::dpi_aware();
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let use_gpu = snaplight_platform::load_config()?.gpu_pins
            || std::env::args().any(|a| a == "--composition" || a == "--smoke");
        let gpu = if use_gpu { Some(Gpu::new(None)?) } else { None };
        let instance = GetModuleHandleW(None)?;
        // Pin windows are frameless, but the class icon still shows up in
        // Alt-Tab previews and any dialog the process opens. `WNDCLASSW` has no
        // separate small icon; the system derives it from this one.
        let class_icon = snaplight_platform::native::app_icon(false).unwrap_or_default();
        for (name, proc) in [
            (
                w!("Snaplight.Pin"),
                pin_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
            ),
            (w!("Snaplight.PinHost"), control_proc),
        ] {
            let wc = WNDCLASSW {
                style: CS_DBLCLKS,
                hInstance: instance.into(),
                lpszClassName: name,
                lpfnWndProc: Some(proc),
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                hIcon: class_icon,
                ..Default::default()
            };
            if RegisterClassW(&wc) == 0 {
                return Err(Error::from_win32().into());
            }
        }
        let control = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("Snaplight.PinHost"),
            w!("Snaplight Pin Host"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            instance,
            None,
        )?;
        let queue = Arc::new(Mutex::new(vec![]));
        let mut host = Box::new(Host {
            gpu,
            pins: vec![],
            queue: queue.clone(),
        });
        SetWindowLongPtrW(control, GWLP_USERDATA, (&mut *host as *mut Host) as isize);
        register_restore_hotkey(control, &snaplight_platform::load_config()?)?;
        let address = control.0 as usize;
        if std::env::args().any(|a| a == "--smoke" || a == "--smoke-low-memory") {
            let dir = snaplight_platform::data_dir();
            std::fs::create_dir_all(&dir)?;
            let image = ImageDescriptor {
                id: ImageId(99),
                width: 320,
                height: 200,
                stride: 1280,
                format: PixelFormat::Bgra8,
                alpha: AlphaMode::Opaque,
                color_space: ColorSpace::Srgb,
            };
            let mut bytes = vec![0; 320 * 200 * 4];
            for (index, p) in bytes.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                p.copy_from_slice(&[(index / 320) as u8, (index % 320) as u8, 180, 255]);
            }
            let path = dir.join("pin-smoke.bgra");
            snaplight_platform::write_pixels(&path, &image, &bytes)?;
            let doc = dir.join("pin-smoke.json");
            snaplight_platform::atomic_write(
                &doc,
                &serde_json::to_vec(&Document {
                    version: 1,
                    image,
                    selection: None,
                    annotations: vec![],
                })?,
            )?;
            // The pin must open exactly where the captured region was, not at the
            // pointer, so verify the window rect against the requested position.
            let requested = ScreenPoint { x: 60, y: 90 };
            host.open(control, path, doc, requested)?;
            let mut placed = RECT::default();
            GetWindowRect(host.pins[0].hwnd, &mut placed)?;
            if (placed.left, placed.top) != (requested.x, requested.y) {
                return Err(format!(
                    "pin opened at ({}, {}) instead of ({}, {})",
                    placed.left, placed.top, requested.x, requested.y
                )
                .into());
            }
            host.pins[0].zoom(0.5)?;
            let mut zoomed = RECT::default();
            GetWindowRect(host.pins[0].hwnd, &mut zoomed)?;
            if (zoomed.left, zoomed.top) != (placed.left, placed.top) {
                return Err("scaling moved the pin window".into());
            }
            host.pins[0].toggle()?;
            host.pins[0].toggle()?;
            host.pins[0].opacity = 0.8;
            host.pins[0].restore()?;
            host.close_all();
            snaplight_platform::atomic_write(
                &dir.join(if use_gpu {
                    "pin-smoke-result.json"
                } else {
                    "pin-smoke-low-memory-result.json"
                }),
                &serde_json::to_vec(
                    &serde_json::json!({"composition":use_gpu,"layered_transition":true,"scale":true,"opacity":true,"position":[placed.left,placed.top],"size":[placed.right-placed.left,placed.bottom-placed.top]}),
                )?,
            )?;
        } else {
            std::thread::spawn(move || {
                for line in BufReader::new(std::io::stdin())
                    .lines()
                    .map_while(std::result::Result::ok)
                {
                    if let Ok(e) = serde_json::from_str::<Envelope<PinCommand>>(&line) {
                        if e.version == PROTOCOL_VERSION {
                            queue.lock().unwrap().push(e.payload);
                            let _ =
                                PostMessageW(HWND(address as _), INCOMING, WPARAM(0), LPARAM(0));
                        }
                    }
                }
                queue.lock().unwrap().push(PinCommand::Shutdown);
                let _ = PostMessageW(HWND(address as _), INCOMING, WPARAM(0), LPARAM(0));
            });
            snaplight_platform::native::message_loop()?;
        }
        host.close_all();
        SetWindowLongPtrW(control, GWLP_USERDATA, 0);
        let _ = UnregisterHotKey(control, 2);
        DestroyWindow(control)?;
        drop(host);
        CoUninitialize();
    }
    Ok(())
}

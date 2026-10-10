use snaplight_domain::*;
use snaplight_platform::CaptureService;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use windows::{
    core::*,
    Foundation::Numerics::Matrix3x2,
    Win32::{
        Foundation::*,
        Graphics::{
            Direct2D::{Common::*, *},
            Direct3D::*,
            Direct3D11::*,
            DirectComposition::*,
            DirectWrite::*,
            Dxgi::{Common::*, *},
            Gdi::*,
        },
    },
};

/// Selection chrome, in logical pixels at 96 DPI; the overlay draws in image
/// pixels (the capture's physical pixels), so every size is scaled by the
/// monitor DPI.
const CHROME_BORDER: f32 = 2.2;
const CHROME_HANDLE_OUTER: f32 = 6.5;
const CHROME_HANDLE_INNER: f32 = 4.6;
const CHROME_CHIP_HEIGHT: f32 = 28.0;
const CHROME_CHIP_PADDING: f32 = 13.0;
const CHROME_CHIP_GAP: f32 = 7.0;
const CHROME_CHIP_FONT: f32 = 17.0;

fn identity() -> Matrix3x2 {
    Matrix3x2 {
        M11: 1.0,
        M12: 0.0,
        M21: 0.0,
        M22: 1.0,
        M31: 0.0,
        M32: 0.0,
    }
}
fn rect(r: ImageRect) -> D2D_RECT_F {
    D2D_RECT_F {
        left: r.x,
        top: r.y,
        right: r.x + r.width,
        bottom: r.y + r.height,
    }
}
fn bounds(w: u32, h: u32) -> D2D_RECT_F {
    rect(ImageRect {
        x: 0.0,
        y: 0.0,
        width: w as f32,
        height: h as f32,
    })
}
fn point(p: ImagePoint) -> D2D_POINT_2F {
    D2D_POINT_2F { x: p.x, y: p.y }
}
fn properties(options: D2D1_BITMAP_OPTIONS, alpha: D2D1_ALPHA_MODE) -> D2D1_BITMAP_PROPERTIES1 {
    D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: alpha,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: options,
        ..Default::default()
    }
}
unsafe fn hdr_to_sdr(
    dc: &ID2D1DeviceContext,
    bitmap: &ID2D1Bitmap1,
    color: DXGI_COLOR_SPACE_TYPE,
    max_luminance: f32,
) -> Result<ID2D1Image> {
    let dc5: ID2D1DeviceContext5 = dc.cast()?;
    let input = dc5
        .CreateColorContextFromDxgiColorSpace(color)
        .map_err(|e| Error::new(e.code(), format!("HDR input context: {e}")))?;
    let linear = dc
        .CreateColorContext(D2D1_COLOR_SPACE_SCRGB, None)
        .map_err(|e| Error::new(e.code(), format!("HDR linear context: {e}")))?;
    let srgb = dc.CreateColorContext(D2D1_COLOR_SPACE_SRGB, None)?;
    unsafe fn context(effect: &ID2D1Effect, index: u32, value: &impl Interface) -> Result<()> {
        effect
            .SetValue(
                index,
                D2D1_PROPERTY_TYPE_COLOR_CONTEXT,
                &(value.as_raw() as usize).to_ne_bytes(),
            )
            .map_err(|e| Error::new(e.code(), format!("HDR color-context property {index}: {e}")))
    }
    unsafe fn convert(
        dc: &ID2D1DeviceContext,
        source: &impl Interface,
        destination: &impl Interface,
    ) -> Result<ID2D1Effect> {
        let effect = dc.CreateEffect(&CLSID_D2D1ColorManagement)?;
        context(
            &effect,
            D2D1_COLORMANAGEMENT_PROP_SOURCE_COLOR_CONTEXT.0 as u32,
            source,
        )?;
        context(
            &effect,
            D2D1_COLORMANAGEMENT_PROP_DESTINATION_COLOR_CONTEXT.0 as u32,
            destination,
        )
        .map_err(|e| Error::new(e.code(), format!("HDR color quality: {e}")))?;
        effect
            .SetValue(
                D2D1_COLORMANAGEMENT_PROP_QUALITY.0 as u32,
                D2D1_PROPERTY_TYPE_ENUM,
                &D2D1_COLORMANAGEMENT_QUALITY_BEST.0.to_ne_bytes(),
            )
            .map_err(|e| Error::new(e.code(), format!("HDR color precision: {e}")))?;
        effect.SetValue(
            D2D1_PROPERTY_PRECISION.0 as u32,
            D2D1_PROPERTY_TYPE_ENUM,
            &D2D1_BUFFER_PRECISION_16BPC_FLOAT.0.to_ne_bytes(),
        )?;
        Ok(effect)
    }
    let to_linear = convert(dc, &input, &linear)?;
    to_linear.SetInput(0, bitmap, true);
    let tone = dc.CreateEffect(&CLSID_D2D1HdrToneMap)?;
    tone.SetInput(0, &to_linear.GetOutput()?, true);
    // Desktop capture has no content MaxCLL metadata; use the display luminance estimate.
    tone.SetValue(
        D2D1_HDRTONEMAP_PROP_INPUT_MAX_LUMINANCE.0 as u32,
        D2D1_PROPERTY_TYPE_FLOAT,
        &max_luminance.to_ne_bytes(),
    )?;
    tone.SetValue(
        D2D1_HDRTONEMAP_PROP_OUTPUT_MAX_LUMINANCE.0 as u32,
        D2D1_PROPERTY_TYPE_FLOAT,
        &80.0f32.to_ne_bytes(),
    )?;
    tone.SetValue(
        D2D1_HDRTONEMAP_PROP_DISPLAY_MODE.0 as u32,
        D2D1_PROPERTY_TYPE_ENUM,
        &D2D1_HDRTONEMAP_DISPLAY_MODE_SDR.0.to_ne_bytes(),
    )?;
    tone.SetValue(
        D2D1_PROPERTY_PRECISION.0 as u32,
        D2D1_PROPERTY_TYPE_ENUM,
        &D2D1_BUFFER_PRECISION_16BPC_FLOAT.0.to_ne_bytes(),
    )?;
    let to_srgb = convert(dc, &linear, &srgb)?;
    to_srgb.SetInput(0, &tone.GetOutput()?, true);
    to_srgb.GetOutput()
}

#[derive(Clone)]
pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub d2d: ID2D1Device,
    pub dxgi: IDXGIDevice,
    pub factory: IDXGIFactory2,
    pub write: IDWriteFactory,
    pub software: bool,
}
impl Gpu {
    pub fn frame_from_bgra(
        &self,
        image: ImageDescriptor,
        pixels: &[u8],
        screen: RECT,
    ) -> Result<FrozenFrame> {
        unsafe {
            if image.byte_len().map_err(|e| Error::new(E_INVALIDARG, e))? != pixels.len() {
                return Err(Error::new(E_INVALIDARG, "pixel length mismatch"));
            }
            let desc = D3D11_TEXTURE2D_DESC {
                Width: image.width,
                Height: image.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET).0 as u32,
                ..Default::default()
            };
            let data = D3D11_SUBRESOURCE_DATA {
                pSysMem: pixels.as_ptr().cast(),
                SysMemPitch: image.stride,
                ..Default::default()
            };
            let mut texture = None;
            self.device
                .CreateTexture2D(&desc, Some(&data), Some(&mut texture))?;
            Ok(FrozenFrame {
                gpu: self.clone(),
                texture: texture.unwrap(),
                screen,
                rotation: DXGI_MODE_ROTATION_IDENTITY,
                descriptor: image,
                fallback: false,
                hdr: None,
            })
        }
    }
    pub fn new(adapter: Option<&IDXGIAdapter1>) -> Result<Self> {
        unsafe {
            let mut device = None;
            let mut context = None;
            let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
            let base_adapter: Option<IDXGIAdapter> = adapter.map(|a| a.cast()).transpose()?;
            let attempt = D3D11CreateDevice(
                base_adapter.as_ref(),
                D3D_DRIVER_TYPE(if adapter.is_some() {
                    D3D_DRIVER_TYPE_UNKNOWN.0
                } else {
                    D3D_DRIVER_TYPE_HARDWARE.0
                }),
                HMODULE::default(),
                flags,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            );
            let software = attempt.is_err();
            if software {
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_WARP,
                    HMODULE::default(),
                    flags,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )?;
            }
            let device = device.unwrap();
            let context = context.unwrap();
            let dxgi: IDXGIDevice = device.cast()?;
            let d2d_factory: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let d2d = d2d_factory.CreateDevice(&dxgi)?;
            let adapter = dxgi.GetAdapter()?;
            let factory: IDXGIFactory2 = adapter.GetParent()?;
            let write: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            Ok(Self {
                device,
                context,
                d2d,
                dxgi,
                factory,
                write,
                software,
            })
        }
    }
    pub fn dc(&self) -> Result<ID2D1DeviceContext> {
        unsafe {
            self.d2d
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)
        }
    }
}
pub struct FrozenFrame {
    pub gpu: Gpu,
    pub texture: ID3D11Texture2D,
    pub screen: RECT,
    pub rotation: DXGI_MODE_ROTATION,
    pub descriptor: ImageDescriptor,
    pub fallback: bool,
    pub hdr: Option<(DXGI_COLOR_SPACE_TYPE, f32)>,
}
pub struct DesktopCapture;
impl CaptureService for DesktopCapture {
    type Frame = FrozenFrame;
    type Error = Error;
    fn capture(&mut self) -> Result<Vec<FrozenFrame>> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let mut frames = vec![];
            let mut ai = 0;
            while let Ok(adapter) = factory.EnumAdapters1(ai) {
                ai += 1;
                let mut oi = 0;
                let mut gpu = None;
                while let Ok(output) = adapter.EnumOutputs(oi) {
                    oi += 1;
                    let desc = output.GetDesc()?;
                    if !desc.AttachedToDesktop.as_bool() {
                        continue;
                    }
                    let display_color = output
                        .cast::<IDXGIOutput6>()
                        .ok()
                        .and_then(|o| o.GetDesc1().ok());
                    let hdr_display = display_color.as_ref().is_some_and(|d| {
                        d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
                    });
                    if gpu.is_none() {
                        gpu = Some(Gpu::new(Some(&adapter))?);
                    }
                    let gpu = gpu.as_ref().unwrap().clone();
                    let output1: IDXGIOutput1 = output.cast()?;
                    let capture = (|| -> Result<(ID3D11Texture2D, DXGI_MODE_ROTATION)> {
                        let duplication = if hdr_display {
                            let output5: IDXGIOutput5 = output.cast()?;
                            output5.DuplicateOutput1(
                                &gpu.device,
                                0,
                                &[
                                    DXGI_FORMAT_R16G16B16A16_FLOAT,
                                    DXGI_FORMAT_R10G10B10A2_UNORM,
                                    DXGI_FORMAT_B8G8R8A8_UNORM,
                                ],
                            )?
                        } else {
                            output1.DuplicateOutput(&gpu.device)?
                        };
                        let texture = acquire_desktop_frame(&gpu, &duplication)?;
                        Ok((texture, duplication.GetDesc().Rotation))
                    })();
                    let fallback = capture.is_err();
                    let (texture, rotation) = match capture {
                        Ok(v) => v,
                        Err(e) if hdr_display => return Err(e),
                        Err(_) => (
                            capture_gdi(&gpu, desc.DesktopCoordinates)?,
                            DXGI_MODE_ROTATION_IDENTITY,
                        ),
                    };
                    let hdr = if hdr_display {
                        let mut td = D3D11_TEXTURE2D_DESC::default();
                        texture.GetDesc(&mut td);
                        let color=match td.Format {DXGI_FORMAT_R16G16B16A16_FLOAT=>DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709,DXGI_FORMAT_R10G10B10A2_UNORM=>DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,_=>return Err(Error::new(E_NOTIMPL,"HDR capture returned an 8-bit surface; full-range capture is unavailable on this display"))};
                        let max = display_color
                            .as_ref()
                            .map(|d| d.MaxLuminance)
                            .filter(|v| v.is_finite() && *v > 80.0)
                            .unwrap_or(1000.0);
                        Some((color, max))
                    } else {
                        None
                    };
                    let w = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left) as u32;
                    let h = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top) as u32;
                    frames.push(FrozenFrame {
                        gpu,
                        texture,
                        screen: desc.DesktopCoordinates,
                        rotation,
                        descriptor: ImageDescriptor {
                            id: ImageId(frames.len() as u64 + 1),
                            width: w,
                            height: h,
                            stride: w * 4,
                            format: PixelFormat::Bgra8,
                            alpha: AlphaMode::Opaque,
                            color_space: if hdr.is_some() {
                                ColorSpace::HdrConvertedToSdr
                            } else {
                                ColorSpace::Srgb
                            },
                        },
                        fallback,
                        hdr,
                    });
                }
            }
            if frames.is_empty() {
                return Err(Error::new(E_FAIL, "no active desktop displays"));
            }
            Ok(frames)
        }
    }
}
/// Acquires a frame that carries a desktop image.
///
/// The first frame returned after `DuplicateOutput` can report no desktop update
/// (its `LastPresentTime` is zero) and hand back an uninitialized, black
/// surface; copying it silently produces a black capture. Frames without a
/// desktop update are therefore released and skipped until one arrives or the
/// deadline passes, in which case the caller can use the GDI fallback.
unsafe fn acquire_desktop_frame(
    gpu: &Gpu,
    duplication: &IDXGIOutputDuplication,
) -> Result<ID3D11Texture2D> {
    const DEADLINE: Duration = Duration::from_millis(1500);
    const WAIT_MS: u32 = 500;
    let started = Instant::now();
    loop {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        match duplication.AcquireNextFrame(WAIT_MS, &mut info, &mut resource) {
            Ok(()) => {}
            // A timeout means the desktop has not changed yet, which is normal on
            // an idle screen; keep waiting instead of taking a slower path.
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT && started.elapsed() < DEADLINE => {
                continue;
            }
            Err(e) => return Err(e),
        }
        let copied = if info.LastPresentTime == 0 {
            Err(Error::new(
                E_FAIL,
                "desktop duplication frame carries no image update",
            ))
        } else {
            (|| -> Result<ID3D11Texture2D> {
                let source: ID3D11Texture2D = resource.unwrap().cast()?;
                let mut td = D3D11_TEXTURE2D_DESC::default();
                source.GetDesc(&mut td);
                td.Usage = D3D11_USAGE_DEFAULT;
                td.BindFlags =
                    D3D11_BIND_SHADER_RESOURCE.0 as u32 | D3D11_BIND_RENDER_TARGET.0 as u32;
                td.CPUAccessFlags = 0;
                td.MiscFlags = 0;
                let mut texture = None;
                gpu.device.CreateTexture2D(&td, None, Some(&mut texture))?;
                let texture = texture.unwrap();
                gpu.context.CopyResource(&texture, &source);
                Ok(texture)
            })()
        };
        let released = duplication.ReleaseFrame();
        match copied {
            Ok(texture) => {
                released?;
                return Ok(texture);
            }
            Err(e) => {
                released?;
                if started.elapsed() >= DEADLINE {
                    return Err(e);
                }
            }
        }
    }
}
/// Copies the desktop into a top-down BGRA8 buffer through GDI.
unsafe fn gdi_bits(screen: RECT) -> Result<(u32, u32, Vec<u8>)> {
    let w = screen.right - screen.left;
    let h = screen.bottom - screen.top;
    let desktop = GetDC(None);
    let memory = CreateCompatibleDC(desktop);
    let mut bits = std::ptr::null_mut();
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: 40,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let bitmap_result = CreateDIBSection(desktop, &info, DIB_RGB_COLORS, &mut bits, None, 0);
    let bitmap = match bitmap_result {
        Ok(v) => v,
        Err(e) => {
            let _ = DeleteDC(memory);
            ReleaseDC(None, desktop);
            return Err(e);
        }
    };
    let old = SelectObject(memory, bitmap);
    let copied = BitBlt(
        memory,
        0,
        0,
        w,
        h,
        desktop,
        screen.left,
        screen.top,
        SRCCOPY | CAPTUREBLT,
    );
    let result = (|| {
        copied?;
        let len = w as usize * h as usize * 4;
        let mut bytes = vec![0u8; len];
        std::ptr::copy_nonoverlapping(bits.cast::<u8>(), bytes.as_mut_ptr(), len);
        Ok((w as u32, h as u32, bytes))
    })();
    SelectObject(memory, old);
    let _ = DeleteObject(bitmap);
    let _ = DeleteDC(memory);
    ReleaseDC(None, desktop);
    result
}
unsafe fn capture_gdi(gpu: &Gpu, screen: RECT) -> Result<ID3D11Texture2D> {
    let (width, height, bytes) = gdi_bits(screen)?;
    let td = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    let data = D3D11_SUBRESOURCE_DATA {
        pSysMem: bytes.as_ptr().cast(),
        SysMemPitch: width * 4,
        ..Default::default()
    };
    let mut texture = None;
    gpu.device
        .CreateTexture2D(&td, Some(&data), Some(&mut texture))?;
    Ok(texture.unwrap())
}
/// Cross-checks a captured frame against an independent GDI capture of the same
/// screen and returns the fraction of sparse samples that agree.
///
/// `acquire_desktop_frame` skips duplication frames that carry no desktop image,
/// but a driver can still hand back an empty surface, and an all-black capture is
/// invisible to pixel checks that only cover a synthetic frame. Comparing against
/// the GDI path catches that class of failure. Returns `None` for frames that
/// cannot be compared this way: HDR output and rotated displays go through a
/// different conversion than the GDI path.
pub fn capture_agreement(frame: &FrozenFrame, bytes: &[u8]) -> Result<Option<f32>> {
    const COLUMNS: u32 = 32;
    const ROWS: u32 = 18;
    const TOLERANCE: u8 = 32;
    if frame.hdr.is_some() || frame.rotation != DXGI_MODE_ROTATION_IDENTITY {
        return Ok(None);
    }
    let width = frame.descriptor.width;
    let height = frame.descriptor.height;
    let stride = frame.descriptor.stride;
    unsafe {
        let (w, h, reference) = gdi_bits(frame.screen)?;
        if (w, h) != (width, height) {
            return Err(Error::new(
                E_FAIL,
                format!("gdi capture is {w}x{h} but the frame is {width}x{height}"),
            ));
        }
        let mut agreeing = 0u32;
        let mut total = 0u32;
        // GDI rows are `w * 4` bytes wide, like the frame's own stride.
        let reference_stride = w * 4;
        for row in 0..ROWS {
            for column in 0..COLUMNS {
                let x = column * width / COLUMNS + width / (COLUMNS * 2);
                let y = row * height / ROWS + height / (ROWS * 2);
                let a = &bytes[(y * stride + x * 4) as usize..][..3];
                let b = &reference[(y * reference_stride + x * 4) as usize..][..3];
                total += 1;
                if a.iter().zip(b).all(|(p, q)| p.abs_diff(*q) <= TOLERANCE) {
                    agreeing += 1;
                }
            }
        }
        Ok(Some(agreeing as f32 / total as f32))
    }
}

struct Composition {
    device: IDCompositionDevice,
    _target: IDCompositionTarget,
    visual: IDCompositionVisual,
    opacity: IDCompositionEffectGroup,
}
impl Composition {
    unsafe fn new(gpu: &Gpu, hwnd: HWND) -> Result<Self> {
        let device: IDCompositionDevice = DCompositionCreateDevice(&gpu.dxgi)?;
        let target = device.CreateTargetForHwnd(hwnd, true)?;
        let visual = device.CreateVisual()?;
        target.SetRoot(&visual)?;
        let opacity = device.CreateEffectGroup()?;
        visual.SetEffect(&opacity)?;
        Ok(Self {
            device,
            _target: target,
            visual,
            opacity,
        })
    }
}
pub struct OverlayRenderer {
    hwnd: HWND,
    gpu: Gpu,
    dc: ID2D1DeviceContext,
    source: ID2D1Bitmap1,
    target: ID2D1Bitmap1,
    swap: IDXGISwapChain1,
    // Held as an option so a rebuild can release the composition target before
    // creating a new one for the same HWND.
    composition: Option<Composition>,
    annotations: ID2D1Bitmap1,
    cache_dirty: bool,
    mosaics: HashMap<u32, ID2D1Bitmap1>,
    width: u32,
    height: u32,
    pub frames: u64,
    pub cpu_us: Vec<u64>,
    pub present_us: Vec<u64>,
    pub gpu_us: Vec<f64>,
    profiling: bool,
    queries: std::collections::VecDeque<TimestampQueries>,
    chip_format: Option<IDWriteTextFormat>,
    chip_font: f32,
    record_composed: bool,
    composed: Option<ComposedFrame>,
}
/// Composed overlay frame (base image, dim mask, annotations and selection
/// chrome) read back from the render target. Automation uses it to check the
/// chrome pixels without depending on what is visible on the desktop.
pub struct ComposedFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}
struct TimestampQueries {
    disjoint: ID3D11Query,
    start: ID3D11Query,
    end: ID3D11Query,
}
impl TimestampQueries {
    unsafe fn new(gpu: &Gpu) -> Result<Self> {
        let make = |kind| -> Result<ID3D11Query> {
            let mut query = None;
            gpu.device.CreateQuery(
                &D3D11_QUERY_DESC {
                    Query: kind,
                    MiscFlags: 0,
                },
                Some(&mut query),
            )?;
            Ok(query.unwrap())
        };
        Ok(Self {
            disjoint: make(D3D11_QUERY_TIMESTAMP_DISJOINT)?,
            start: make(D3D11_QUERY_TIMESTAMP)?,
            end: make(D3D11_QUERY_TIMESTAMP)?,
        })
    }
    unsafe fn sample(&self, context: &ID3D11DeviceContext) -> Option<f64> {
        unsafe fn read<T: Default>(
            context: &ID3D11DeviceContext,
            query: &ID3D11Query,
        ) -> Option<T> {
            let mut value = T::default();
            let status = (Interface::vtable(context).GetData)(
                context.as_raw(),
                query.as_raw(),
                (&mut value as *mut T).cast(),
                std::mem::size_of::<T>() as u32,
                D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
            );
            if status == S_OK {
                Some(value)
            } else {
                None
            }
        }
        let d: D3D11_QUERY_DATA_TIMESTAMP_DISJOINT = read(context, &self.disjoint)?;
        if d.Disjoint.as_bool() || d.Frequency == 0 {
            return Some(f64::NAN);
        }
        let start: u64 = read(context, &self.start)?;
        let end: u64 = read(context, &self.end)?;
        Some(end.saturating_sub(start) as f64 * 1_000_000.0 / d.Frequency as f64)
    }
}
/// Device-dependent resources owned by an `OverlayRenderer`.
struct Targets {
    gpu: Gpu,
    dc: ID2D1DeviceContext,
    source: ID2D1Bitmap1,
    target: ID2D1Bitmap1,
    swap: IDXGISwapChain1,
    composition: Composition,
}
/// Recreates every device-dependent resource for `hwnd` from `frame`.
fn create_targets(hwnd: HWND, frame: &FrozenFrame) -> Result<Targets> {
    unsafe {
        let gpu = frame.gpu.clone();
        let dc = gpu.dc()?;
        let width = frame.descriptor.width;
        let height = frame.descriptor.height;
        let surface: IDXGISurface = frame.texture.cast()?;
        let mut texture_desc = D3D11_TEXTURE2D_DESC::default();
        frame.texture.GetDesc(&mut texture_desc);
        let mut raw_props = properties(D2D1_BITMAP_OPTIONS_NONE, D2D1_ALPHA_MODE_IGNORE);
        raw_props.pixelFormat.format = texture_desc.Format;
        let raw = dc.CreateBitmapFromDxgiSurface(&surface, Some(&raw_props))?;
        let normalized = if let Some((color, max_luminance)) = frame.hdr {
            Some(hdr_to_sdr(&dc, &raw, color, max_luminance)?)
        } else {
            None
        };
        let source = dc.CreateBitmap(
            D2D_SIZE_U { width, height },
            None,
            0,
            &properties(D2D1_BITMAP_OPTIONS_TARGET, D2D1_ALPHA_MODE_PREMULTIPLIED),
        )?;
        dc.SetTarget(&source);
        dc.BeginDraw();
        dc.Clear(Some(&D2D1_COLOR_F {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        }));
        let t = match frame.rotation {
            DXGI_MODE_ROTATION_ROTATE90 => Matrix3x2 {
                M11: 0.0,
                M12: 1.0,
                M21: -1.0,
                M22: 0.0,
                M31: width as f32,
                M32: 0.0,
            },
            DXGI_MODE_ROTATION_ROTATE180 => Matrix3x2 {
                M11: -1.0,
                M12: 0.0,
                M21: 0.0,
                M22: -1.0,
                M31: width as f32,
                M32: height as f32,
            },
            DXGI_MODE_ROTATION_ROTATE270 => Matrix3x2 {
                M11: 0.0,
                M12: -1.0,
                M21: 1.0,
                M22: 0.0,
                M31: 0.0,
                M32: height as f32,
            },
            _ => identity(),
        };
        dc.SetTransform(&t);
        if let Some(effect) = &normalized {
            dc.DrawImage(
                effect,
                None,
                None,
                D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                D2D1_COMPOSITE_MODE_SOURCE_OVER,
            );
        } else {
            dc.DrawBitmap(
                &raw,
                None,
                1.0,
                D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                None,
                None,
            );
        }
        dc.EndDraw(None, None)?;
        dc.SetTransform(&identity());
        dc.SetTarget(None);
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
            ..Default::default()
        };
        let swap = gpu
            .factory
            .CreateSwapChainForComposition(&gpu.device, &desc, None)?;
        if let Ok(device) = gpu.dxgi.cast::<IDXGIDevice1>() {
            device.SetMaximumFrameLatency(1)?;
        }
        let back: IDXGISurface = swap.GetBuffer(0)?;
        let target = dc.CreateBitmapFromDxgiSurface(
            &back,
            Some(&properties(
                D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                D2D1_ALPHA_MODE_PREMULTIPLIED,
            )),
        )?;
        let composition = Composition::new(&gpu, hwnd)?;
        composition.visual.SetContent(&swap)?;
        composition.device.Commit()?;
        Ok(Targets {
            gpu,
            dc,
            source,
            target,
            swap,
            composition,
        })
    }
}
/// True when the error means device-dependent resources must be rebuilt.
pub fn is_recoverable_device_error(error: &Error) -> bool {
    let hr = error.code();
    hr == DXGI_ERROR_DEVICE_REMOVED || hr == DXGI_ERROR_DEVICE_RESET || hr == D2DERR_RECREATE_TARGET
}
impl OverlayRenderer {
    pub fn new(hwnd: HWND, frame: &FrozenFrame) -> Result<Self> {
        unsafe {
            let targets = create_targets(hwnd, frame)?;
            let width = frame.descriptor.width;
            let height = frame.descriptor.height;
            let annotations = targets.dc.CreateBitmap(
                D2D_SIZE_U { width, height },
                None,
                0,
                &properties(D2D1_BITMAP_OPTIONS_TARGET, D2D1_ALPHA_MODE_PREMULTIPLIED),
            )?;
            Ok(Self {
                hwnd,
                gpu: targets.gpu,
                dc: targets.dc,
                source: targets.source,
                target: targets.target,
                swap: targets.swap,
                composition: Some(targets.composition),
                annotations,
                cache_dirty: true,
                mosaics: HashMap::new(),
                width,
                height,
                frames: 0,
                cpu_us: vec![],
                present_us: vec![],
                gpu_us: vec![],
                profiling: false,
                queries: std::collections::VecDeque::new(),
                chip_format: None,
                chip_font: 0.0,
                record_composed: false,
                composed: None,
            })
        }
    }
    /// Rebuilds every device-dependent resource on a fresh device, keeping the window.
    ///
    /// `base` must be the immutable base image pixels for `descriptor` (BGRA8, `.stride`).
    /// The caller must re-render the document afterwards; annotations are invalidated here.
    pub fn rebuild(
        &mut self,
        base: &[u8],
        descriptor: &ImageDescriptor,
        screen: RECT,
    ) -> Result<()> {
        unsafe {
            // Release the composition target before creating one for the same HWND.
            self.composition = None;
            let gpu = Gpu::new(None)?;
            let frame = gpu.frame_from_bgra(descriptor.clone(), base, screen)?;
            let targets = create_targets(self.hwnd, &frame)?;
            let annotations = targets.dc.CreateBitmap(
                D2D_SIZE_U {
                    width: descriptor.width,
                    height: descriptor.height,
                },
                None,
                0,
                &properties(D2D1_BITMAP_OPTIONS_TARGET, D2D1_ALPHA_MODE_PREMULTIPLIED),
            )?;
            self.gpu = targets.gpu;
            self.dc = targets.dc;
            self.source = targets.source;
            self.target = targets.target;
            self.swap = targets.swap;
            self.composition = Some(targets.composition);
            self.annotations = annotations;
            self.width = descriptor.width;
            self.height = descriptor.height;
            self.mosaics.clear();
            self.cache_dirty = true;
            self.queries.clear();
            // The text format belongs to the old DirectWrite factory.
            self.chip_format = None;
            Ok(())
        }
    }
    /// True when the underlying D3D11 device has been removed or reset.
    pub fn device_lost(&self) -> bool {
        unsafe { self.gpu.device.GetDeviceRemovedReason().is_err() }
    }
    pub fn invalidate_annotations(&mut self) {
        self.cache_dirty = true;
    }
    pub fn enable_profiling(&mut self) {
        self.profiling = true;
    }
    /// Selection chrome: a blue outline, eight round handles, and a chip that
    /// reports the origin and size in screen pixels.
    unsafe fn draw_selection_chrome(
        &mut self,
        r: ImageRect,
        dpi: f32,
        origin: ScreenPoint,
    ) -> Result<()> {
        let scale = (dpi / 96.0).clamp(1.0, 4.0);
        let center = |p: D2D_POINT_2F, radius: f32| D2D1_ELLIPSE {
            point: p,
            radiusX: radius,
            radiusY: radius,
        };
        let blue = self.dc.CreateSolidColorBrush(
            &D2D1_COLOR_F {
                r: 0.2,
                g: 0.533,
                b: 1.0,
                a: 1.0,
            },
            None,
        )?;
        let white = self.dc.CreateSolidColorBrush(
            &D2D1_COLOR_F {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            },
            None,
        )?;
        self.dc
            .DrawRectangle(&rect(r), &blue, CHROME_BORDER * scale, None);
        let xs = [r.x, r.x + r.width / 2.0, r.x + r.width];
        let ys = [r.y, r.y + r.height / 2.0, r.y + r.height];
        for (row, y) in ys.iter().enumerate() {
            for (col, x) in xs.iter().enumerate() {
                if row == 1 && col == 1 {
                    continue;
                }
                let p = D2D_POINT_2F { x: *x, y: *y };
                self.dc
                    .FillEllipse(&center(p, CHROME_HANDLE_OUTER * scale), &white);
                self.dc
                    .FillEllipse(&center(p, CHROME_HANDLE_INNER * scale), &blue);
            }
        }
        let text = format!(
            "{},{}  {} × {} px",
            origin.x + r.x.round() as i32,
            origin.y + r.y.round() as i32,
            r.width.round().max(0.0) as u32,
            r.height.round().max(0.0) as u32
        );
        let format = self.chip_text_format(CHROME_CHIP_FONT * scale)?;
        let utf16: Vec<u16> = text.encode_utf16().collect();
        let layout = self.gpu.write.CreateTextLayout(
            &utf16,
            &format,
            self.width as f32,
            CHROME_CHIP_HEIGHT * scale,
        )?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        layout.GetMetrics(&mut metrics)?;
        let width = metrics.widthIncludingTrailingWhitespace + 2.0 * CHROME_CHIP_PADDING * scale;
        let height = CHROME_CHIP_HEIGHT * scale;
        let chip = ImageRect {
            x: r.x.clamp(
                2.0 * scale,
                (self.width as f32 - width - 2.0 * scale).max(2.0 * scale),
            ),
            y: (r.y - CHROME_CHIP_GAP * scale - height).max(2.0 * scale),
            width,
            height,
        };
        layout.SetMaxWidth(width)?;
        layout.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
        layout.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
        let background = self.dc.CreateSolidColorBrush(
            &D2D1_COLOR_F {
                r: 0.157,
                g: 0.192,
                b: 0.22,
                a: 1.0,
            },
            None,
        )?;
        self.dc.FillRoundedRectangle(
            &D2D1_ROUNDED_RECT {
                rect: rect(chip),
                radiusX: height / 2.0,
                radiusY: height / 2.0,
            },
            &background,
        );
        self.dc.DrawTextLayout(
            D2D_POINT_2F {
                x: chip.x,
                y: chip.y,
            },
            &layout,
            &white,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
        );
        Ok(())
    }
    /// Text format for the size chip, recreated when the DPI-scaled size changes.
    unsafe fn chip_text_format(&mut self, size: f32) -> Result<IDWriteTextFormat> {
        if self.chip_format.is_none() || (self.chip_font - size).abs() > 0.01 {
            self.chip_format = Some(self.gpu.write.CreateTextFormat(
                w!("Segoe UI"),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                w!(""),
            )?);
            self.chip_font = size;
        }
        self.chip_format
            .clone()
            .ok_or_else(|| Error::new(E_UNEXPECTED, "chip text format"))
    }
    /// Reads the composed frame back after every `render`. Automation uses this
    /// to check the chrome pixels; it costs one copy per recorded frame.
    pub fn record_composed(&mut self, on: bool) {
        self.record_composed = on;
        self.composed = None;
    }
    /// Frame read back by the last `render` call with recording enabled.
    pub fn composed(&self) -> Option<&ComposedFrame> {
        self.composed.as_ref()
    }
    /// Copies the render target into a CPU-readable buffer. Must run between
    /// `EndDraw` and `Present`, where the target holds the composed frame.
    unsafe fn copy_composed(&self) -> Result<ComposedFrame> {
        let staging = self.dc.CreateBitmap(
            D2D_SIZE_U {
                width: self.width,
                height: self.height,
            },
            None,
            0,
            &properties(
                D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                D2D1_ALPHA_MODE_PREMULTIPLIED,
            ),
        )?;
        staging.CopyFromBitmap(None, &self.target, None)?;
        let mapped = staging.Map(D2D1_MAP_OPTIONS_READ)?;
        let stride = self.width as usize * 4;
        let mut pixels = vec![0u8; stride * self.height as usize];
        for y in 0..self.height as usize {
            let row = std::slice::from_raw_parts(
                (mapped.bits as *const u8).add(y * mapped.pitch as usize),
                stride,
            );
            pixels[y * stride..(y + 1) * stride].copy_from_slice(row);
        }
        let _ = staging.Unmap();
        Ok(ComposedFrame {
            width: self.width,
            height: self.height,
            pixels,
        })
    }
    pub fn collect_gpu_samples(&mut self) {
        unsafe {
            while let Some(query) = self.queries.front() {
                let Some(sample) = query.sample(&self.gpu.context) else {
                    break;
                };
                if sample.is_finite() {
                    self.gpu_us.push(sample);
                }
                self.queries.pop_front();
            }
        }
    }
    pub fn original(&self, doc: &Document) -> Result<Vec<u8>> {
        unsafe {
            let readable = self.dc.CreateBitmap(
                D2D_SIZE_U {
                    width: self.width,
                    height: self.height,
                },
                None,
                0,
                &properties(
                    D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                    D2D1_ALPHA_MODE_PREMULTIPLIED,
                ),
            )?;
            readable.CopyFromBitmap(None, &self.source, None)?;
            let mapped = readable.Map(D2D1_MAP_OPTIONS_READ)?;
            let mut bytes = vec![0; doc.image.stride as usize * self.height as usize];
            for row in 0..self.height as usize {
                std::ptr::copy_nonoverlapping(
                    mapped.bits.add(row * mapped.pitch as usize),
                    bytes.as_mut_ptr().add(row * doc.image.stride as usize),
                    self.width as usize * 4,
                );
            }
            readable.Unmap()?;
            Ok(bytes)
        }
    }
    unsafe fn prepare_mosaic(&mut self, block: u32) -> Result<()> {
        let block = block.clamp(2, 128);
        if self.mosaics.contains_key(&block) {
            return Ok(());
        }
        // Block sizes are bounded to 2..=128; keep all sizes referenced by undoable annotations.
        let bitmap = self.dc.CreateBitmap(
            D2D_SIZE_U {
                width: self.width.div_ceil(block),
                height: self.height.div_ceil(block),
            },
            None,
            0,
            &properties(D2D1_BITMAP_OPTIONS_TARGET, D2D1_ALPHA_MODE_PREMULTIPLIED),
        )?;
        self.dc.SetTarget(&bitmap);
        self.dc.BeginDraw();
        self.dc.Clear(None);
        self.dc.SetTransform(&identity());
        self.dc.DrawBitmap(
            &self.source,
            Some(&D2D_RECT_F {
                left: 0.0,
                top: 0.0,
                right: self.width as f32 / block as f32,
                bottom: self.height as f32 / block as f32,
            }),
            1.0,
            D2D1_INTERPOLATION_MODE_LINEAR,
            None,
            None,
        );
        self.dc.EndDraw(None, None)?;
        self.dc.SetTarget(None);
        self.mosaics.insert(block, bitmap);
        Ok(())
    }
    unsafe fn draw_annotation(&self, a: &Annotation) -> Result<()> {
        let c = a.style.rgba;
        let brush = self.dc.CreateSolidColorBrush(
            &D2D1_COLOR_F {
                r: c[0],
                g: c[1],
                b: c[2],
                a: c[3],
            },
            None,
        )?;
        match &a.shape {
            Shape::Rectangle(r) => self
                .dc
                .DrawRectangle(&rect(*r), &brush, a.style.width, None),
            Shape::Ellipse(r) => self.dc.DrawEllipse(
                &D2D1_ELLIPSE {
                    point: D2D_POINT_2F {
                        x: r.x + r.width / 2.0,
                        y: r.y + r.height / 2.0,
                    },
                    radiusX: r.width / 2.0,
                    radiusY: r.height / 2.0,
                },
                &brush,
                a.style.width,
                None,
            ),
            Shape::Arrow { from, to } => {
                self.dc
                    .DrawLine(point(*from), point(*to), &brush, a.style.width, None);
                let angle = (to.y - from.y).atan2(to.x - from.x);
                let length = 12.0 + a.style.width * 2.0;
                for delta in [-0.45f32, 0.45] {
                    let p = ImagePoint {
                        x: to.x - length * (angle + delta).cos(),
                        y: to.y - length * (angle + delta).sin(),
                    };
                    self.dc
                        .DrawLine(point(p), point(*to), &brush, a.style.width, None);
                }
            }
            Shape::Pen(points) => {
                for pair in points.windows(2) {
                    self.dc
                        .DrawLine(point(pair[0]), point(pair[1]), &brush, a.style.width, None);
                }
                if points.len() == 1 {
                    self.dc.FillEllipse(
                        &D2D1_ELLIPSE {
                            point: point(points[0]),
                            radiusX: a.style.width / 2.0,
                            radiusY: a.style.width / 2.0,
                        },
                        &brush,
                    );
                }
            }
            Shape::Mosaic { rect: r, block } => {
                if let Some(bitmap) = self.mosaics.get(&(*block).clamp(2, 128)) {
                    let b = (*block).clamp(2, 128) as f32;
                    let src = D2D_RECT_F {
                        left: r.x / b,
                        top: r.y / b,
                        right: (r.x + r.width) / b,
                        bottom: (r.y + r.height) / b,
                    };
                    self.dc.DrawBitmap(
                        bitmap,
                        Some(&rect(*r)),
                        1.0,
                        D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                        Some(&src),
                        None,
                    );
                }
            }
            Shape::Text { at, text, size } => {
                let format = self.gpu.write.CreateTextFormat(
                    w!("Segoe UI"),
                    None,
                    DWRITE_FONT_WEIGHT_NORMAL,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    *size,
                    w!(""),
                )?;
                let text: Vec<u16> = text.encode_utf16().collect();
                let layout = D2D_RECT_F {
                    left: at.x,
                    top: at.y,
                    right: self.width as f32,
                    bottom: self.height as f32,
                };
                self.dc.DrawText(
                    &text,
                    &format,
                    &layout,
                    &brush,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
        }
        Ok(())
    }
    unsafe fn update_cache(&mut self, doc: &Document, interaction: &Interaction) -> Result<()> {
        for a in doc.annotations.iter().chain(interaction.draft.iter()) {
            if let Shape::Mosaic { block, .. } = a.shape {
                self.prepare_mosaic(block)?;
            }
        }
        if self.cache_dirty {
            self.dc.SetTarget(&self.annotations);
            self.dc.SetTransform(&identity());
            self.dc.BeginDraw();
            self.dc.Clear(None);
            for a in &doc.annotations {
                self.draw_annotation(a)?;
            }
            self.dc.EndDraw(None, None)?;
            self.dc.SetTarget(None);
            self.cache_dirty = false;
        }
        Ok(())
    }
    /// Image area `export` will produce: the selection when one exists, clamped
    /// to the frame. Kept separate so callers can map the result back to screen
    /// coordinates with the same rounding.
    fn export_area(&self, doc: &Document) -> ImageRect {
        doc.selection
            .unwrap_or(ImageRect {
                x: 0.0,
                y: 0.0,
                width: self.width as f32,
                height: self.height as f32,
            })
            .clamp(self.width, self.height)
    }
    /// Image pixel the next `export` crops from, as integers.
    pub fn export_origin(&self, doc: &Document) -> ImagePoint {
        let area = self.export_area(doc);
        ImagePoint {
            x: area.x.floor(),
            y: area.y.floor(),
        }
    }
    pub fn export(&mut self, doc: &Document) -> Result<(ImageDescriptor, Vec<u8>)> {
        unsafe {
            self.update_cache(doc, &Interaction::default())?;
            let area = self.export_area(doc);
            let x = area.x.floor();
            let y = area.y.floor();
            let w = (area.x + area.width).ceil() as u32 - x as u32;
            let h = (area.y + area.height).ceil() as u32 - y as u32;
            if w == 0 || h == 0 {
                return Err(Error::new(E_INVALIDARG, "empty selection"));
            }
            let output = self.dc.CreateBitmap(
                D2D_SIZE_U {
                    width: w,
                    height: h,
                },
                None,
                0,
                &properties(D2D1_BITMAP_OPTIONS_TARGET, D2D1_ALPHA_MODE_PREMULTIPLIED),
            )?;
            self.dc.SetTarget(&output);
            self.dc.SetTransform(&Matrix3x2 {
                M31: -x,
                M32: -y,
                ..identity()
            });
            self.dc.BeginDraw();
            self.dc.Clear(None);
            self.dc.DrawBitmap(
                &self.source,
                None,
                1.0,
                D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                None,
                None,
            );
            self.dc.DrawBitmap(
                &self.annotations,
                None,
                1.0,
                D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                None,
                None,
            );
            self.dc.EndDraw(None, None)?;
            self.dc.SetTransform(&identity());
            self.dc.SetTarget(None);
            let readable = self.dc.CreateBitmap(
                D2D_SIZE_U {
                    width: w,
                    height: h,
                },
                None,
                0,
                &properties(
                    D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                    D2D1_ALPHA_MODE_PREMULTIPLIED,
                ),
            )?;
            readable.CopyFromBitmap(None, &output, None)?;
            let mapped = readable.Map(D2D1_MAP_OPTIONS_READ)?;
            let mut bytes = vec![0; w as usize * h as usize * 4];
            for row in 0..h as usize {
                std::ptr::copy_nonoverlapping(
                    mapped.bits.add(row * mapped.pitch as usize),
                    bytes.as_mut_ptr().add(row * w as usize * 4),
                    w as usize * 4,
                );
            }
            readable.Unmap()?;
            let image = ImageDescriptor {
                id: doc.image.id,
                width: w,
                height: h,
                stride: w * 4,
                format: PixelFormat::Bgra8,
                alpha: AlphaMode::Opaque,
                color_space: doc.image.color_space,
            };
            Ok((image, bytes))
        }
    }
}
impl CanvasRenderer for OverlayRenderer {
    type Error = Error;
    fn render(
        &mut self,
        doc: &Document,
        viewport: Viewport,
        interaction: &Interaction,
    ) -> Result<()> {
        unsafe {
            let started = std::time::Instant::now();
            self.collect_gpu_samples();
            let timing = if self.profiling && self.queries.len() < 8 {
                let q = TimestampQueries::new(&self.gpu)?;
                self.gpu.context.Begin(&q.disjoint);
                self.gpu.context.End(&q.start);
                Some(q)
            } else {
                None
            };
            self.update_cache(doc, interaction)?;
            self.dc.SetTarget(&self.target);
            self.dc.SetTransform(&identity());
            self.dc.BeginDraw();
            self.dc.Clear(None);
            self.dc.DrawBitmap(
                &self.source,
                None,
                1.0,
                D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                None,
                None,
            );
            let dim = self.dc.CreateSolidColorBrush(
                &D2D1_COLOR_F {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 0.45,
                },
                None,
            )?;
            let selection = interaction.selection_draft.or(doc.selection);
            if let Some(r) = selection {
                for zone in [
                    D2D_RECT_F {
                        left: 0.0,
                        top: 0.0,
                        right: self.width as f32,
                        bottom: r.y,
                    },
                    D2D_RECT_F {
                        left: 0.0,
                        top: r.y + r.height,
                        right: self.width as f32,
                        bottom: self.height as f32,
                    },
                    D2D_RECT_F {
                        left: 0.0,
                        top: r.y,
                        right: r.x,
                        bottom: r.y + r.height,
                    },
                    D2D_RECT_F {
                        left: r.x + r.width,
                        top: r.y,
                        right: self.width as f32,
                        bottom: r.y + r.height,
                    },
                ] {
                    self.dc.FillRectangle(&zone, &dim);
                }
            } else {
                self.dc
                    .FillRectangle(&bounds(self.width, self.height), &dim);
            }
            self.dc.DrawBitmap(
                &self.annotations,
                None,
                1.0,
                D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                None,
                None,
            );
            if let Some(a) = &interaction.draft {
                self.draw_annotation(a)?;
            }
            if let Some(r) = selection {
                self.draw_selection_chrome(r, viewport.dpi, viewport.origin)?;
            }
            if let Some(p) = interaction.cursor {
                let sample = ImageRect {
                    x: (p.x - 10.0).clamp(0.0, (self.width as f32 - 20.0).max(0.0)),
                    y: (p.y - 10.0).clamp(0.0, (self.height as f32 - 20.0).max(0.0)),
                    width: 20.0,
                    height: 20.0,
                };
                let zoom = ImageRect {
                    x: (p.x + 24.0).min((self.width as f32 - 124.0).max(0.0)),
                    y: (p.y + 24.0).min((self.height as f32 - 124.0).max(0.0)),
                    width: 120.0,
                    height: 120.0,
                };
                self.dc.DrawBitmap(
                    &self.source,
                    Some(&rect(zoom)),
                    1.0,
                    D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                    Some(&rect(sample)),
                    None,
                );
                let brush = self.dc.CreateSolidColorBrush(
                    &D2D1_COLOR_F {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 1.0,
                    },
                    None,
                )?;
                self.dc.DrawRectangle(&rect(zoom), &brush, 1.0, None);
                let cx = zoom.x + (p.x - sample.x) * 6.0;
                let cy = zoom.y + (p.y - sample.y) * 6.0;
                self.dc.DrawLine(
                    D2D_POINT_2F { x: cx - 5.0, y: cy },
                    D2D_POINT_2F { x: cx + 5.0, y: cy },
                    &brush,
                    1.0,
                    None,
                );
                self.dc.DrawLine(
                    D2D_POINT_2F { x: cx, y: cy - 5.0 },
                    D2D_POINT_2F { x: cx, y: cy + 5.0 },
                    &brush,
                    1.0,
                    None,
                );
            }
            self.dc.EndDraw(None, None)?;
            if self.record_composed {
                self.composed = Some(self.copy_composed()?);
            }
            if let Some(q) = timing {
                self.gpu.context.End(&q.end);
                self.gpu.context.End(&q.disjoint);
                self.queries.push_back(q);
            }
            let cpu = started.elapsed().as_micros() as u64;
            let present = std::time::Instant::now();
            self.swap.Present(1, DXGI_PRESENT(0)).ok()?;
            self.composition
                .as_ref()
                .ok_or_else(|| Error::new(E_FAIL, "renderer has no composition target"))?
                .device
                .Commit()?;
            self.frames += 1;
            if self.cpu_us.len() < 10000 {
                self.cpu_us.push(cpu);
                self.present_us.push(present.elapsed().as_micros() as u64);
            }
            // Device loss can also surface as a silently ignored Present result; report it
            // explicitly so the caller rebuilds instead of displaying stale frames.
            if self.device_lost() {
                return Err(Error::new(DXGI_ERROR_DEVICE_REMOVED, "GPU device removed"));
            }
            Ok(())
        }
    }
}

pub struct StaticSurface {
    _gpu: Gpu,
    composition: Composition,
    _surface: IDCompositionSurface,
    pub width: u32,
    pub height: u32,
}

impl StaticSurface {
    pub fn new(gpu: &Gpu, hwnd: HWND, image: &ImageDescriptor, bytes: &[u8]) -> Result<Self> {
        unsafe {
            if image.byte_len().map_err(|e| Error::new(E_INVALIDARG, e))? != bytes.len() {
                return Err(Error::new(E_INVALIDARG, "pixel length mismatch"));
            }
            let composition = Composition::new(gpu, hwnd)?;
            let surface = composition.device.CreateSurface(
                image.width,
                image.height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_ALPHA_MODE_PREMULTIPLIED,
            )?;
            let mut offset = POINT::default();
            let dxgi: IDXGISurface = surface.BeginDraw(None, &mut offset)?;
            let result = (|| -> Result<()> {
                let dc = gpu.dc()?;
                let target = dc.CreateBitmapFromDxgiSurface(
                    &dxgi,
                    Some(&properties(
                        D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                        D2D1_ALPHA_MODE_PREMULTIPLIED,
                    )),
                )?;
                let source = dc.CreateBitmap(
                    D2D_SIZE_U {
                        width: image.width,
                        height: image.height,
                    },
                    Some(bytes.as_ptr().cast()),
                    image.stride,
                    &properties(D2D1_BITMAP_OPTIONS_NONE, D2D1_ALPHA_MODE_PREMULTIPLIED),
                )?;
                dc.SetTarget(&target);
                dc.SetTransform(&Matrix3x2 {
                    M31: offset.x as f32,
                    M32: offset.y as f32,
                    ..identity()
                });
                dc.BeginDraw();
                dc.PushAxisAlignedClip(
                    &bounds(image.width, image.height),
                    D2D1_ANTIALIAS_MODE_ALIASED,
                );
                dc.Clear(None);
                dc.DrawBitmap(
                    &source,
                    None,
                    1.0,
                    D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                    None,
                    None,
                );
                dc.PopAxisAlignedClip();
                dc.EndDraw(None, None)?;
                dc.SetTarget(None);
                Ok(())
            })();
            let ended = surface.EndDraw();
            result?;
            ended?;
            composition.visual.SetContent(&surface)?;
            composition.device.Commit()?;
            Ok(Self {
                _gpu: gpu.clone(),
                composition,
                _surface: surface,
                width: image.width,
                height: image.height,
            })
        }
    }
    pub fn opacity(&self, value: f32) -> Result<()> {
        unsafe {
            self.composition
                .opacity
                .SetOpacity2(value.clamp(0.1, 1.0))?;
            self.composition.device.Commit()
        }
    }
    pub fn scale(&self, value: f32) -> Result<()> {
        unsafe {
            let transform = self.composition.device.CreateScaleTransform()?;
            transform.SetScaleX2(value)?;
            transform.SetScaleY2(value)?;
            self.composition.visual.SetTransform(&transform)?;
            self.composition.device.Commit()
        }
    }
    pub fn release_content(&self) -> Result<()> {
        unsafe {
            self.composition.visual.SetContent(None)?;
            self.composition.device.Commit()
        }
    }
}

#[cfg(test)]
#[path = "windows_tests.rs"]
mod tests;

use crate::gpu::Gpu;
use snaplight_domain::*;
use snaplight_platform::CaptureService;
use std::time::{Duration, Instant};
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Graphics::{
            Direct3D11::*,
            Dxgi::{Common::*, *},
            Gdi::*,
        },
    },
};

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
            self.device()
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
}

pub struct FrozenFrame {
    gpu: Gpu,
    texture: ID3D11Texture2D,
    screen: RECT,
    rotation: DXGI_MODE_ROTATION,
    descriptor: ImageDescriptor,
    fallback: bool,
    hdr: Option<(DXGI_COLOR_SPACE_TYPE, f32)>,
}
impl FrozenFrame {
    /// Descriptor of the captured image.
    pub fn descriptor(&self) -> &ImageDescriptor {
        &self.descriptor
    }
    /// Screen rectangle the frame was captured from.
    pub fn screen(&self) -> RECT {
        self.screen
    }
    /// True when the frame came from the GDI fallback path.
    pub fn is_fallback(&self) -> bool {
        self.fallback
    }
    /// True when the frame was produced on the software (WARP) device.
    pub fn is_software(&self) -> bool {
        self.gpu.is_software()
    }
    /// Device that owns the frame's texture, borrowed by the renderer.
    pub(crate) fn gpu(&self) -> &Gpu {
        &self.gpu
    }
    /// Captured texture, borrowed by the renderer.
    pub(crate) fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }
    /// Display rotation the frame was captured under.
    pub(crate) fn rotation(&self) -> DXGI_MODE_ROTATION {
        self.rotation
    }
    /// HDR colour space and peak luminance, when the frame is HDR content.
    pub(crate) fn hdr(&self) -> Option<(DXGI_COLOR_SPACE_TYPE, f32)> {
        self.hdr
    }
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
                                gpu.device(),
                                0,
                                &[
                                    DXGI_FORMAT_R16G16B16A16_FLOAT,
                                    DXGI_FORMAT_R10G10B10A2_UNORM,
                                    DXGI_FORMAT_B8G8R8A8_UNORM,
                                ],
                            )?
                        } else {
                            output1.DuplicateOutput(gpu.device())?
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
                gpu.device()
                    .CreateTexture2D(&td, None, Some(&mut texture))?;
                let texture = texture.unwrap();
                gpu.context().CopyResource(&texture, &source);
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
/// Copies the desktop into a caller-supplied top-down BGRA8 buffer through GDI.
unsafe fn gdi_bits_into(screen: RECT, bytes: &mut Vec<u8>) -> Result<(u32, u32)> {
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
        bytes.resize(len, 0);
        std::ptr::copy_nonoverlapping(bits.cast::<u8>(), bytes.as_mut_ptr(), len);
        Ok((w as u32, h as u32))
    })();
    SelectObject(memory, old);
    let _ = DeleteObject(bitmap);
    let _ = DeleteDC(memory);
    ReleaseDC(None, desktop);
    result
}
/// Copies a desktop rectangle into a top-down BGRA8 buffer through GDI.
unsafe fn gdi_bits(screen: RECT) -> Result<(u32, u32, Vec<u8>)> {
    let mut bytes = Vec::new();
    let (width, height) = gdi_bits_into(screen, &mut bytes)?;
    Ok((width, height, bytes))
}
/// Copies a desktop rectangle into an existing buffer through GDI.
pub fn screen_pixels_into(screen: RECT, bytes: &mut Vec<u8>) -> Result<(u32, u32)> {
    unsafe { gdi_bits_into(screen, bytes) }
}
/// Which path a [`RegionGrabber`] ended up on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrabBackend {
    /// Desktop Duplication: a consistent DWM frame, no cursor, event driven.
    Direct,
    /// GDI `BitBlt` fallback, used when duplication is unavailable.
    Compatible,
}
/// A long-capture frame source for one screen rectangle.
///
/// The direct path keeps one `IDXGIOutputDuplication` for the whole session:
/// `AcquireNextFrame` blocks until the desktop changes, so an idle screen costs
/// nothing, and only the selected rectangle is copied out of the frame. The
/// compatible path keeps one memory DC and DIB section, so a read does not
/// allocate a new bitmap every tick.
pub struct RegionGrabber {
    backend: Backend,
    width: u32,
    height: u32,
}
enum Backend {
    Direct(DirectGrabber),
    Compatible(GdiGrabber),
}
impl RegionGrabber {
    /// Opens a grabber for `region`. Duplication is preferred; when the output
    /// cannot be duplicated (remote desktop, another capture app, a rotated
    /// display) the GDI path takes over.
    pub fn open(region: RECT) -> Result<Self> {
        if region.right <= region.left || region.bottom <= region.top {
            return Err(Error::new(E_INVALIDARG, "capture region is empty"));
        }
        let width = (region.right - region.left) as u32;
        let height = (region.bottom - region.top) as u32;
        // A diagnostic switch for exercising the compatible path on a machine
        // where duplication works, so both paths can be verified end to end.
        let force_compatible = std::env::var_os("SNAPLIGHT_LONG_CAPTURE_GDI").is_some();
        let backend = if force_compatible {
            Backend::Compatible(GdiGrabber::open(region)?)
        } else {
            match DirectGrabber::open(region) {
                Ok(direct) => Backend::Direct(direct),
                Err(_) => Backend::Compatible(GdiGrabber::open(region)?),
            }
        };
        Ok(Self {
            backend,
            width,
            height,
        })
    }
    pub fn backend(&self) -> GrabBackend {
        match self.backend {
            Backend::Direct(_) => GrabBackend::Direct,
            Backend::Compatible(_) => GrabBackend::Compatible,
        }
    }
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    /// Waits up to `timeout` for the next frame and writes it into `dst`.
    /// Returns false when nothing changed. On the compatible path this always
    /// sleeps for the timeout before returning true, which keeps the session at
    /// a fixed sampling rate.
    pub fn read(&mut self, dst: &mut Vec<u8>, timeout: Duration) -> Result<bool> {
        match &mut self.backend {
            Backend::Direct(direct) => direct.read(dst, timeout),
            Backend::Compatible(gdi) => gdi.read(dst, timeout),
        }
    }
}
/// Duplication-backed grabber. The frame resource is only held between
/// `AcquireNextFrame` and `ReleaseFrame`, and every path releases it, because a
/// leaked frame makes the next acquire fail.
struct DirectGrabber {
    gpu: Gpu,
    duplication: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    offset: (i32, i32),
    width: u32,
    height: u32,
}
impl DirectGrabber {
    fn open(region: RECT) -> Result<Self> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let mut adapter_index = 0;
            while let Ok(adapter) = factory.EnumAdapters1(adapter_index) {
                adapter_index += 1;
                let mut output_index = 0;
                while let Ok(output) = adapter.EnumOutputs(output_index) {
                    output_index += 1;
                    let desc = output.GetDesc()?;
                    if !desc.AttachedToDesktop.as_bool() {
                        continue;
                    }
                    let desktop = desc.DesktopCoordinates;
                    let inside = region.left >= desktop.left
                        && region.top >= desktop.top
                        && region.right <= desktop.right
                        && region.bottom <= desktop.bottom;
                    if !inside {
                        continue;
                    }
                    let output1: IDXGIOutput1 = match output.cast() {
                        Ok(output) => output,
                        Err(_) => continue,
                    };
                    let gpu = match Gpu::new(Some(&adapter)) {
                        Ok(gpu) => gpu,
                        Err(_) => continue,
                    };
                    let duplication = match output1.DuplicateOutput(gpu.device()) {
                        Ok(duplication) => duplication,
                        Err(_) => continue,
                    };
                    // A rotated output hands back a differently oriented frame;
                    // the compatible path handles those.
                    if duplication.GetDesc().Rotation != DXGI_MODE_ROTATION_IDENTITY {
                        continue;
                    }
                    let width = (region.right - region.left) as u32;
                    let height = (region.bottom - region.top) as u32;
                    let staging = match create_staging(gpu.device(), width, height) {
                        Ok(staging) => staging,
                        Err(_) => continue,
                    };
                    return Ok(Self {
                        gpu,
                        duplication,
                        staging,
                        offset: (region.left - desktop.left, region.top - desktop.top),
                        width,
                        height,
                    });
                }
            }
            Err(Error::new(E_FAIL, "no output can be duplicated"))
        }
    }
    fn read(&mut self, dst: &mut Vec<u8>, timeout: Duration) -> Result<bool> {
        unsafe {
            let millis = timeout.as_millis().min(u128::from(u32::MAX)) as u32;
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            match self
                .duplication
                .AcquireNextFrame(millis, &mut info, &mut resource)
            {
                Ok(()) => {}
                Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
                Err(error) => return Err(error),
            }
            let result = (|| -> Result<bool> {
                // The first frame after the duplication starts can report no
                // desktop update and hold an uninitialized surface.
                if info.LastPresentTime == 0 {
                    return Ok(false);
                }
                let resource = resource
                    .ok_or_else(|| Error::new(E_FAIL, "duplication frame has no resource"))?;
                let source: ID3D11Texture2D = resource.cast()?;
                let mut source_desc = D3D11_TEXTURE2D_DESC::default();
                source.GetDesc(&mut source_desc);
                if source_desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                    return Err(Error::new(
                        E_INVALIDARG,
                        format!(
                            "duplication returned {:?}; the compatible path handles it",
                            source_desc.Format
                        ),
                    ));
                }
                let (left, top) = self.offset;
                let region = D3D11_BOX {
                    left: left as u32,
                    top: top as u32,
                    front: 0,
                    right: left as u32 + self.width,
                    bottom: top as u32 + self.height,
                    back: 1,
                };
                self.gpu.context().CopySubresourceRegion(
                    &self.staging,
                    0,
                    0,
                    0,
                    0,
                    &source,
                    0,
                    Some(&region),
                );
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                self.gpu.context().Map(
                    &self.staging,
                    0,
                    D3D11_MAP_READ,
                    0,
                    Some(&mut mapped as *mut _),
                )?;
                let len = self.width as usize * self.height as usize * 4;
                dst.resize(len, 0);
                for row in 0..self.height as usize {
                    let source = (mapped.pData as *const u8).add(row * mapped.RowPitch as usize);
                    let target = dst.as_mut_ptr().add(row * self.width as usize * 4);
                    std::ptr::copy_nonoverlapping(source, target, self.width as usize * 4);
                }
                self.gpu.context().Unmap(&self.staging, 0);
                Ok(true)
            })();
            let released = self.duplication.ReleaseFrame();
            result.and_then(|frame| {
                released?;
                Ok(frame)
            })
        }
    }
}
unsafe fn create_staging(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut texture = None;
    device.CreateTexture2D(&desc, None, Some(&mut texture))?;
    texture.ok_or_else(|| Error::new(E_FAIL, "no staging texture"))
}
/// GDI-backed grabber with a persistent memory DC and DIB section.
struct GdiGrabber {
    desktop: HDC,
    memory: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut core::ffi::c_void,
    left: i32,
    top: i32,
    width: u32,
    height: u32,
}
// The DC, bitmap and pointer are owned by this value and only touched from the
// thread that owns it; nothing here is shared.
unsafe impl Send for GdiGrabber {}
impl Drop for GdiGrabber {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.memory, self.previous);
            let _ = DeleteObject(self.bitmap);
            let _ = DeleteDC(self.memory);
            ReleaseDC(None, self.desktop);
        }
    }
}
impl GdiGrabber {
    fn open(screen: RECT) -> Result<Self> {
        unsafe {
            let width = (screen.right - screen.left).max(1) as u32;
            let height = (screen.bottom - screen.top).max(1) as u32;
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
            let bitmap = match CreateDIBSection(desktop, &info, DIB_RGB_COLORS, &mut bits, None, 0)
            {
                Ok(bitmap) => bitmap,
                Err(error) => {
                    let _ = DeleteDC(memory);
                    ReleaseDC(None, desktop);
                    return Err(error);
                }
            };
            let previous = SelectObject(memory, bitmap);
            Ok(Self {
                desktop,
                memory,
                bitmap,
                previous,
                bits,
                left: screen.left,
                top: screen.top,
                width,
                height,
            })
        }
    }
    fn read(&mut self, dst: &mut Vec<u8>, timeout: Duration) -> Result<bool> {
        unsafe {
            BitBlt(
                self.memory,
                0,
                0,
                self.width as i32,
                self.height as i32,
                self.desktop,
                self.left,
                self.top,
                SRCCOPY | CAPTUREBLT,
            )?;
            let len = self.width as usize * self.height as usize * 4;
            dst.resize(len, 0);
            std::ptr::copy_nonoverlapping(self.bits.cast::<u8>(), dst.as_mut_ptr(), len);
        }
        std::thread::sleep(timeout);
        Ok(true)
    }
}

/// Copies a desktop rectangle into a top-down BGRA8 buffer through GDI.
///
/// Long capture re-reads the scrolled region many times per session; GDI is the
/// cheapest path that returns pixels for an arbitrary screen rectangle without a
/// device round-trip. HDR displays go through the duplication path instead, so
/// their content is tone mapped differently here.
pub fn screen_pixels(screen: RECT) -> Result<(u32, u32, Vec<u8>)> {
    unsafe { gdi_bits(screen) }
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
    gpu.device()
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

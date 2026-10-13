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

//! DirectComposition target and surface used to present pinned images.
//!
//! `Composition` owns the DComp device, target and root visual for one HWND.
//! `StaticSurface` uploads the composed pin bitmap into a `IDCompositionSurface`
//! and presents it through that composition target. The fields stay private:
//! callers create a target with `new`, attach content with `set_content` and
//! flush changes with `commit`; `StaticSurface` reaches the fields directly
//! because it lives in this module.

use crate::d2d::{bounds, identity, properties};
use crate::gpu::Gpu;
use snaplight_domain::ImageDescriptor;
use windows::{
    core::*,
    Foundation::Numerics::Matrix3x2,
    Win32::{
        Foundation::*,
        Graphics::{
            Direct2D::{Common::*, *},
            DirectComposition::*,
            Dxgi::{Common::*, *},
        },
    },
};

pub(crate) struct Composition {
    device: IDCompositionDevice,
    _target: IDCompositionTarget,
    visual: IDCompositionVisual,
    opacity: IDCompositionEffectGroup,
}

impl Composition {
    pub(crate) unsafe fn new(gpu: &Gpu, hwnd: HWND) -> Result<Self> {
        let device: IDCompositionDevice = DCompositionCreateDevice(gpu.dxgi())?;
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

    /// Attaches `content` (a surface or swap chain) to the root visual.
    pub(crate) unsafe fn set_content<P: Param<IUnknown>>(&self, content: P) -> Result<()> {
        self.visual.SetContent(content)
    }

    /// Flushes pending composition changes to the compositor.
    pub(crate) fn commit(&self) -> Result<()> {
        unsafe { self.device.Commit() }
    }
}

pub struct StaticSurface {
    _gpu: Gpu,
    composition: Composition,
    _surface: IDCompositionSurface,
    width: u32,
    height: u32,
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
            composition.set_content(&surface)?;
            composition.commit()?;
            Ok(Self {
                _gpu: gpu.clone(),
                composition,
                _surface: surface,
                width: image.width,
                height: image.height,
            })
        }
    }

    /// Pixel size of the uploaded bitmap.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn opacity(&self, value: f32) -> Result<()> {
        unsafe {
            self.composition
                .opacity
                .SetOpacity2(value.clamp(0.1, 1.0))?;
            self.composition.commit()
        }
    }

    pub fn scale(&self, value: f32) -> Result<()> {
        unsafe {
            let transform = self.composition.device.CreateScaleTransform()?;
            transform.SetScaleX2(value)?;
            transform.SetScaleY2(value)?;
            self.composition.visual.SetTransform(&transform)?;
            self.composition.commit()
        }
    }

    pub fn release_content(&self) -> Result<()> {
        unsafe {
            self.composition.visual.SetContent(None)?;
            self.composition.commit()
        }
    }
}

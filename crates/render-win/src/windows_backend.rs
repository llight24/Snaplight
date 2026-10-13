use crate::capture::FrozenFrame;
use crate::composition::Composition;
use crate::d2d::{bounds, identity, properties};
use crate::gpu::Gpu;
use snaplight_domain::*;
use std::collections::HashMap;
use windows::{
    core::*,
    Foundation::Numerics::Matrix3x2,
    Win32::{
        Foundation::*,
        Graphics::{
            Direct2D::{Common::*, *},
            Direct3D11::*,
            DirectWrite::*,
            Dxgi::{Common::*, *},
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

fn rect(r: ImageRect) -> D2D_RECT_F {
    D2D_RECT_F {
        left: r.x,
        top: r.y,
        right: r.x + r.width,
        bottom: r.y + r.height,
    }
}
/// Maps a rectangle from image pixels to view pixels through `viewport`.
fn to_view(r: ImageRect, viewport: &Viewport) -> ImageRect {
    let (x, y) = viewport.to_view(r.x, r.y);
    ImageRect {
        x,
        y,
        width: r.width * viewport.scale,
        height: r.height * viewport.scale,
    }
}
fn point(p: ImagePoint) -> D2D_POINT_2F {
    D2D_POINT_2F { x: p.x, y: p.y }
}
/// Horizontal reference of a chip: `Left` from its left edge, `Centre` from its
/// middle, so a chip can sit next to a selection or over the pointer.
enum ChipAnchor {
    Left(f32),
    Centre(f32),
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
    // Size of the swap chain and of the window's client area, in view pixels.
    view_width: u32,
    view_height: u32,
    frames: u64,
    cpu_us: Vec<u64>,
    present_us: Vec<u64>,
    gpu_us: Vec<f64>,
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
            gpu.device().CreateQuery(
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
///
/// The source bitmap holds the whole document, while the swap chain is sized to
/// `view`: a zoomed-out document (a long capture) is drawn into a smaller or
/// differently shaped window than the image itself.
fn create_targets(hwnd: HWND, frame: &FrozenFrame, view: (u32, u32)) -> Result<Targets> {
    unsafe {
        let gpu = frame.gpu().clone();
        let dc = gpu.dc()?;
        let width = frame.descriptor().width;
        let height = frame.descriptor().height;
        let surface: IDXGISurface = frame.texture().cast()?;
        let mut texture_desc = D3D11_TEXTURE2D_DESC::default();
        frame.texture().GetDesc(&mut texture_desc);
        let mut raw_props = properties(D2D1_BITMAP_OPTIONS_NONE, D2D1_ALPHA_MODE_IGNORE);
        raw_props.pixelFormat.format = texture_desc.Format;
        let raw = dc.CreateBitmapFromDxgiSurface(&surface, Some(&raw_props))?;
        let normalized = if let Some((color, max_luminance)) = frame.hdr() {
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
        let t = match frame.rotation() {
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
            Width: view.0.max(1),
            Height: view.1.max(1),
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
            .factory()
            .CreateSwapChainForComposition(gpu.device(), &desc, None)?;
        if let Ok(device) = gpu.dxgi().cast::<IDXGIDevice1>() {
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
        composition.set_content(&swap)?;
        composition.commit()?;
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
    /// Builds the renderer for `hwnd`. `view` is the swap chain size in view
    /// pixels, normally the client area of the window: a session that shows a
    /// pinned image at the pin's zoom draws the document through the viewport,
    /// so the view size is the window and does not have to match the image.
    pub fn new(hwnd: HWND, frame: &FrozenFrame, view: (u32, u32)) -> Result<Self> {
        unsafe {
            let width = frame.descriptor().width;
            let height = frame.descriptor().height;
            let targets = create_targets(hwnd, frame, view)?;
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
                view_width: view.0.max(1),
                view_height: view.1.max(1),
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
    /// `view` is the swap chain size in view pixels (normally the window's client area).
    /// The caller must re-render the document afterwards; annotations are invalidated here.
    pub fn rebuild(
        &mut self,
        base: &[u8],
        descriptor: &ImageDescriptor,
        screen: RECT,
        view: (u32, u32),
    ) -> Result<()> {
        unsafe {
            // Release the composition target before creating one for the same HWND.
            self.composition = None;
            let gpu = Gpu::new(None)?;
            let frame = gpu.frame_from_bgra(descriptor.clone(), base, screen)?;
            let targets = create_targets(self.hwnd, &frame, view)?;
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
            self.view_width = view.0.max(1);
            self.view_height = view.1.max(1);
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
        unsafe { self.gpu.device().GetDeviceRemovedReason().is_err() }
    }
    pub fn invalidate_annotations(&mut self) {
        self.cache_dirty = true;
    }
    pub fn enable_profiling(&mut self) {
        self.profiling = true;
    }
    /// Number of frames presented so far.
    pub fn frame_count(&self) -> u64 {
        self.frames
    }
    /// Per-frame CPU submission durations in microseconds.
    pub fn cpu_samples_us(&self) -> &[u64] {
        &self.cpu_us
    }
    /// Per-frame blocking Present durations in microseconds.
    pub fn present_samples_us(&self) -> &[u64] {
        &self.present_us
    }
    /// Per-frame GPU timestamp durations in microseconds.
    pub fn gpu_samples_us(&self) -> &[f64] {
        &self.gpu_us
    }
    /// Selection chrome: a blue outline, eight round handles, and a chip that
    /// reports the origin and size in screen pixels.
    unsafe fn draw_selection_chrome(
        &mut self,
        r: ImageRect,
        image: ImageRect,
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
            image.width.round().max(0.0) as u32,
            image.height.round().max(0.0) as u32
        );
        self.draw_chip(
            &text,
            ChipAnchor::Left(r.x),
            r.y - CHROME_CHIP_GAP * scale - CHROME_CHIP_HEIGHT * scale,
            scale,
        )?;
        Ok(())
    }
    /// Dark rounded chip with centred white text, drawn in view pixels. The
    /// anchor is clamped to the view, so a chip never leaves the window.
    unsafe fn draw_chip(
        &mut self,
        text: &str,
        anchor: ChipAnchor,
        top: f32,
        scale: f32,
    ) -> Result<ImageRect> {
        let format = self.chip_text_format(CHROME_CHIP_FONT * scale)?;
        let utf16: Vec<u16> = text.encode_utf16().collect();
        let layout = self.gpu.write().CreateTextLayout(
            &utf16,
            &format,
            self.view_width as f32,
            CHROME_CHIP_HEIGHT * scale,
        )?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        layout.GetMetrics(&mut metrics)?;
        let width = metrics.widthIncludingTrailingWhitespace + 2.0 * CHROME_CHIP_PADDING * scale;
        let height = CHROME_CHIP_HEIGHT * scale;
        let left = match anchor {
            ChipAnchor::Left(x) => x,
            ChipAnchor::Centre(x) => x - width / 2.0,
        };
        let chip = ImageRect {
            x: left.clamp(
                2.0 * scale,
                (self.view_width as f32 - width - 2.0 * scale).max(2.0 * scale),
            ),
            y: top.max(2.0 * scale),
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
        let white = self.dc.CreateSolidColorBrush(
            &D2D1_COLOR_F {
                r: 1.0,
                g: 1.0,
                b: 1.0,
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
        Ok(chip)
    }
    /// Preview chrome for the region detected under the cursor: the committed
    /// selection's outline without handles or the size chip, so a preview is
    /// distinguishable from a selection that a click will keep.
    unsafe fn draw_hover_chrome(&mut self, r: ImageRect, dpi: f32) -> Result<()> {
        let scale = (dpi / 96.0).clamp(1.0, 4.0);
        let blue = self.dc.CreateSolidColorBrush(
            &D2D1_COLOR_F {
                r: 0.2,
                g: 0.533,
                b: 1.0,
                a: 1.0,
            },
            None,
        )?;
        self.dc
            .DrawRectangle(&rect(r), &blue, CHROME_BORDER * scale, None);
        Ok(())
    }
    /// Text format for the size chip, recreated when the DPI-scaled size changes.
    unsafe fn chip_text_format(&mut self, size: f32) -> Result<IDWriteTextFormat> {
        if self.chip_format.is_none() || (self.chip_font - size).abs() > 0.01 {
            self.chip_format = Some(self.gpu.write().CreateTextFormat(
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
        // The target is the swap chain back buffer, so it is view-sized: a
        // zoomed-out document is read back at the size it was drawn.
        let staging = self.dc.CreateBitmap(
            D2D_SIZE_U {
                width: self.view_width,
                height: self.view_height,
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
        let stride = self.view_width as usize * 4;
        let mut pixels = vec![0u8; stride * self.view_height as usize];
        for y in 0..self.view_height as usize {
            let row = std::slice::from_raw_parts(
                (mapped.bits as *const u8).add(y * mapped.pitch as usize),
                stride,
            );
            pixels[y * stride..(y + 1) * stride].copy_from_slice(row);
        }
        let _ = staging.Unmap();
        Ok(ComposedFrame {
            width: self.view_width,
            height: self.view_height,
            pixels,
        })
    }
    pub fn collect_gpu_samples(&mut self) {
        unsafe {
            while let Some(query) = self.queries.front() {
                let Some(sample) = query.sample(self.gpu.context()) else {
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
    /// Solid brush for one colour. An annotation needs at most one brush per
    /// paint, which is cheap next to the draw call that uses it.
    unsafe fn brush(&self, rgba: [f32; 4]) -> Result<ID2D1SolidColorBrush> {
        self.dc.CreateSolidColorBrush(
            &D2D1_COLOR_F {
                r: rgba[0],
                g: rgba[1],
                b: rgba[2],
                a: rgba[3],
            },
            None,
        )
    }
    unsafe fn draw_annotation(&self, a: &Annotation) -> Result<()> {
        let style = a.style;
        match &a.shape {
            // Shapes paint a fill under their outline, so a translucent fill keeps
            // the outline readable.
            Shape::Rectangle(r) => {
                if style.fill_on {
                    let fill = self.brush(style.fill)?;
                    self.dc.FillRectangle(&rect(*r), &fill);
                }
                if style.stroke_on {
                    let stroke = self.brush(style.stroke)?;
                    self.dc.DrawRectangle(&rect(*r), &stroke, style.width, None);
                }
            }
            Shape::Ellipse(r) => {
                let ellipse = D2D1_ELLIPSE {
                    point: D2D_POINT_2F {
                        x: r.x + r.width / 2.0,
                        y: r.y + r.height / 2.0,
                    },
                    radiusX: r.width / 2.0,
                    radiusY: r.height / 2.0,
                };
                if style.fill_on {
                    let fill = self.brush(style.fill)?;
                    self.dc.FillEllipse(&ellipse, &fill);
                }
                if style.stroke_on {
                    let stroke = self.brush(style.stroke)?;
                    self.dc.DrawEllipse(&ellipse, &stroke, style.width, None);
                }
            }
            // The arrow head and the pen are line art: they have no interior to
            // fill, so they always follow the outline colour.
            Shape::Arrow { from, to } => {
                let brush = self.brush(style.stroke)?;
                self.dc
                    .DrawLine(point(*from), point(*to), &brush, style.width, None);
                let angle = (to.y - from.y).atan2(to.x - from.x);
                let length = 12.0 + style.width * 2.0;
                for delta in [-0.45f32, 0.45] {
                    let p = ImagePoint {
                        x: to.x - length * (angle + delta).cos(),
                        y: to.y - length * (angle + delta).sin(),
                    };
                    self.dc
                        .DrawLine(point(p), point(*to), &brush, style.width, None);
                }
            }
            Shape::Pen(points) => {
                let brush = self.brush(style.stroke)?;
                for pair in points.windows(2) {
                    self.dc
                        .DrawLine(point(pair[0]), point(pair[1]), &brush, style.width, None);
                }
                if points.len() == 1 {
                    self.dc.FillEllipse(
                        &D2D1_ELLIPSE {
                            point: point(points[0]),
                            radiusX: style.width / 2.0,
                            radiusY: style.width / 2.0,
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
            Shape::Text { at, text, size } => self.draw_text(a, *at, text, *size)?,
        }
        Ok(())
    }
    /// Text: fill the glyphs, then draw their outline over it. Both come from one
    /// set of glyph outlines, so the outline hugs the letters exactly and a
    /// half-transparent fill does not leak past it.
    unsafe fn draw_text(
        &self,
        a: &Annotation,
        at: ImagePoint,
        text: &str,
        size: f32,
    ) -> Result<()> {
        let format = self.gpu.write().CreateTextFormat(
            w!("Segoe UI"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            w!(""),
        )?;
        let utf16: Vec<u16> = text.encode_utf16().collect();
        // The layout box keeps the wrapping DrawText used to apply: an annotation
        // may not run past the captured image.
        let layout = self.gpu.write().CreateTextLayout(
            &utf16,
            &format,
            (self.width as f32 - at.x).max(1.0),
            (self.height as f32 - at.y).max(1.0),
        )?;
        let runs = super::text_outline::collect(&self.dc, &layout, at.x, at.y)?;
        if a.style.fill_on {
            let fill = self.brush(a.style.fill)?;
            for run in runs.iter() {
                self.dc.FillGeometry(run, &fill, None);
            }
        }
        if a.style.stroke_on {
            let stroke = self.brush(a.style.stroke)?;
            for run in runs.iter() {
                self.dc.DrawGeometry(run, &stroke, a.style.width, None);
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
                self.gpu.context().Begin(&q.disjoint);
                self.gpu.context().End(&q.start);
                Some(q)
            } else {
                None
            };
            self.update_cache(doc, interaction)?;
            let scale = if viewport.scale > 0.0 {
                viewport.scale
            } else {
                1.0
            };
            // Document layers are drawn in image pixels through this transform.
            // Chrome and the magnifier are drawn afterwards in view pixels so
            // their line widths and sizes stay constant at any zoom.
            let document = Matrix3x2 {
                M11: scale,
                M12: 0.0,
                M21: 0.0,
                M22: scale,
                M31: viewport.offset_x,
                M32: viewport.offset_y,
            };
            self.dc.SetTarget(&self.target);
            self.dc.SetTransform(&document);
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
            let selection = if interaction.fixed_image {
                None
            } else {
                interaction.selection_draft.or(doc.selection)
            };
            // The detected region is only a preview: it is shown while nothing is
            // committed and the user is not drawing, and it never reaches the
            // document until a click adopts it. A fixed image never detects.
            let hover = if selection.is_none() && !interaction.dragging && !interaction.fixed_image
            {
                interaction.hover
            } else {
                None
            };
            let region = selection.or(hover);
            if let Some(r) = region {
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
            } else if !interaction.fixed_image {
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
            self.dc.SetTransform(&identity());
            if let Some(r) = selection {
                self.draw_selection_chrome(
                    to_view(r, &viewport),
                    r,
                    viewport.dpi,
                    viewport.origin,
                )?;
            } else if let Some(r) = hover {
                self.draw_hover_chrome(to_view(r, &viewport), viewport.dpi)?;
            }
            if let Some(p) = interaction.cursor {
                let sample = ImageRect {
                    x: (p.x - 10.0).clamp(0.0, (self.width as f32 - 20.0).max(0.0)),
                    y: (p.y - 10.0).clamp(0.0, (self.height as f32 - 20.0).max(0.0)),
                    width: 20.0,
                    height: 20.0,
                };
                let (cx, cy) = viewport.to_view(p.x, p.y);
                let zoom = ImageRect {
                    x: (cx + 24.0).min((self.view_width as f32 - 124.0).max(0.0)),
                    y: (cy + 24.0).min((self.view_height as f32 - 124.0).max(0.0)),
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
            // The scrub readout belongs to the pointer that is adjusting it, so
            // it is drawn last and follows the cursor.
            if let Some(hud) = &interaction.hud {
                let scale = (viewport.dpi / 96.0).clamp(1.0, 4.0);
                let (anchor, pointer_y) = match interaction.cursor {
                    Some(p) => {
                        let (x, y) = viewport.to_view(p.x, p.y);
                        (ChipAnchor::Centre(x), y)
                    }
                    None => (ChipAnchor::Centre(self.view_width as f32 / 2.0), 0.0),
                };
                let top = pointer_y - (CHROME_CHIP_HEIGHT + 2.0 * CHROME_CHIP_GAP) * scale;
                self.draw_chip(hud, anchor, top, scale)?;
            }
            self.dc.EndDraw(None, None)?;
            if self.record_composed {
                self.composed = Some(self.copy_composed()?);
            }
            if let Some(q) = timing {
                self.gpu.context().End(&q.end);
                self.gpu.context().End(&q.disjoint);
                self.queries.push_back(q);
            }
            let cpu = started.elapsed().as_micros() as u64;
            let present = std::time::Instant::now();
            self.swap.Present(1, DXGI_PRESENT(0)).ok()?;
            self.composition
                .as_ref()
                .ok_or_else(|| Error::new(E_FAIL, "renderer has no composition target"))?
                .commit()?;
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

#[cfg(test)]
#[path = "windows_tests.rs"]
mod tests;

//! Glyph outline collection for one text layout.
//!
//! DirectWrite draws a layout through an `IDWriteTextRenderer`; this module
//! captures the resolved glyph runs as D2D path geometries instead of painting
//! the glyphs directly.

use core::ffi::c_void;
use std::cell::RefCell;
use std::rc::Rc;
use windows::{
    core::*,
    Foundation::Numerics::Matrix3x2,
    Win32::{
        Foundation::*,
        Graphics::{
            Direct2D::{Common::ID2D1SimplifiedGeometrySink, *},
            DirectWrite::*,
        },
    },
};

/// Glyph outlines of one text layout, in image pixels and ready to draw.
type Outlines = Rc<RefCell<Vec<ID2D1Geometry>>>;

/// Collects the glyph outlines of a text layout instead of drawing the glyphs.
///
/// Text is rendered from its outlines so that one glyph shape serves both the
/// fill and the outline: a stroke drawn from a separately shaped glyph run would
/// drift away from the fill. The renderer is handed the runs DirectWrite already
/// resolved, so text in fonts the requested family does not cover (CJK, emoji)
/// keeps its fallback font instead of turning into boxes.
#[windows::core::implement(IDWriteTextRenderer)]
struct GlyphOutline {
    factory: ID2D1Factory,
    /// Shared with the caller: the object is moved into the interface that
    /// `IDWriteTextLayout::Draw` consumes, so the outlines are read through
    /// this handle afterwards.
    runs: Rc<RefCell<Vec<ID2D1Geometry>>>,
}
impl GlyphOutline {
    unsafe fn new(context: &ID2D1DeviceContext) -> Result<(Self, Outlines)> {
        let runs: Outlines = Rc::new(RefCell::new(Vec::new()));
        Ok((
            Self {
                factory: context.GetFactory()?,
                runs: runs.clone(),
            },
            runs,
        ))
    }
}
// The rendered traits are implemented for the companion type the attribute
// generates; it dereferences to `GlyphOutline`, so the fields stay readable.
impl IDWritePixelSnapping_Impl for GlyphOutline_Impl {
    fn IsPixelSnappingDisabled(&self, _client: *const c_void) -> Result<BOOL> {
        // Glyph geometry goes through the canvas transform, which does its own
        // snapping; snapping here would only shift the outline off the fill.
        Ok(TRUE)
    }
    fn GetCurrentTransform(
        &self,
        _client: *const c_void,
        transform: *mut DWRITE_MATRIX,
    ) -> Result<()> {
        unsafe {
            *transform = DWRITE_MATRIX {
                m11: 1.0,
                m12: 0.0,
                m21: 0.0,
                m22: 1.0,
                dx: 0.0,
                dy: 0.0,
            }
        };
        Ok(())
    }
    fn GetPixelsPerDip(&self, _client: *const c_void) -> Result<f32> {
        Ok(1.0)
    }
}
impl IDWriteTextRenderer_Impl for GlyphOutline_Impl {
    fn DrawGlyphRun(
        &self,
        _client: *const c_void,
        baseline_x: f32,
        baseline_y: f32,
        _measuring: DWRITE_MEASURING_MODE,
        run: *const DWRITE_GLYPH_RUN,
        _description: *const DWRITE_GLYPH_RUN_DESCRIPTION,
        _effect: Option<&IUnknown>,
    ) -> Result<()> {
        unsafe {
            let run = &*run;
            let face = run
                .fontFace
                .as_ref()
                .ok_or_else(|| Error::new(E_UNEXPECTED, "glyph run without a font face"))?;
            let geometry = self.factory.CreatePathGeometry()?;
            let sink: ID2D1SimplifiedGeometrySink = geometry.Open()?.cast()?;
            face.GetGlyphRunOutline(
                run.fontEmSize,
                run.glyphIndices,
                Some(run.glyphAdvances),
                (!run.glyphOffsets.is_null()).then_some(run.glyphOffsets),
                run.glyphCount,
                run.isSideways.as_bool(),
                run.bidiLevel % 2 == 1,
                &sink,
            )?;
            sink.Close()?;
            // Moving the geometry keeps the canvas transform (zoom and pan) in
            // force, which an absolute transform on the context would replace.
            let moved = self.factory.CreateTransformedGeometry(
                &geometry,
                &Matrix3x2::translation(baseline_x, baseline_y),
            )?;
            self.runs.borrow_mut().push(moved.cast()?);
        }
        Ok(())
    }
    fn DrawUnderline(
        &self,
        _client: *const c_void,
        _origin_x: f32,
        _origin_y: f32,
        _underline: *const DWRITE_UNDERLINE,
        _effect: Option<&IUnknown>,
    ) -> Result<()> {
        Ok(())
    }
    fn DrawStrikethrough(
        &self,
        _client: *const c_void,
        _origin_x: f32,
        _origin_y: f32,
        _strikethrough: *const DWRITE_STRIKETHROUGH,
        _effect: Option<&IUnknown>,
    ) -> Result<()> {
        Ok(())
    }
    fn DrawInlineObject(
        &self,
        _client: *const c_void,
        _origin_x: f32,
        _origin_y: f32,
        _object: Option<&IDWriteInlineObject>,
        _sideways: BOOL,
        _right_to_left: BOOL,
        _effect: Option<&IUnknown>,
    ) -> Result<()> {
        Ok(())
    }
}

/// Collects the glyph outlines `layout` resolves at `(x, y)`, in image pixels.
///
/// The renderer object and its shared buffer are hidden here: the returned
/// `Vec` owns the geometry interfaces, so each geometry stays alive while the
/// caller draws its fill and stroke.
pub(crate) unsafe fn collect(
    context: &ID2D1DeviceContext,
    layout: &IDWriteTextLayout,
    x: f32,
    y: f32,
) -> windows::core::Result<Vec<ID2D1Geometry>> {
    let (outline, runs) = GlyphOutline::new(context)?;
    let renderer: IDWriteTextRenderer = outline.into();
    layout.Draw(None, &renderer, x, y)?;
    // Hand the buffer's ownership to the caller instead of cloning the
    // interfaces: `take` leaves an empty Vec behind while moving the existing
    // one out, so no geometry is ref-counted again.
    let geometries = std::mem::take(&mut *runs.borrow_mut());
    Ok(geometries)
}

//! Direct2D conversions shared by the overlay renderer and the pinned static
//! surface.
//!
//! The surface and the swap chain both hold BGRA bitmaps, so the same
//! transform, rectangle and bitmap-property shapes apply to both; the alpha
//! mode is passed in by the caller. Keeping them here names that shared
//! contract once instead of duplicating it.

use windows::{
    Foundation::Numerics::Matrix3x2,
    Win32::Graphics::{
        Direct2D::{Common::*, *},
        Dxgi::Common::*,
    },
};

/// Identity transform, the starting point for the per-draw pixel offsets.
pub(crate) fn identity() -> Matrix3x2 {
    Matrix3x2 {
        M11: 1.0,
        M12: 0.0,
        M21: 0.0,
        M22: 1.0,
        M31: 0.0,
        M32: 0.0,
    }
}

/// Whole-surface rectangle for a bitmap of `w`×`h` pixels.
pub(crate) fn bounds(w: u32, h: u32) -> D2D_RECT_F {
    D2D_RECT_F {
        left: 0.0,
        top: 0.0,
        right: w as f32,
        bottom: h as f32,
    }
}

/// Bitmap properties for a BGRA bitmap at 96 DPI; the alpha mode comes from
/// the caller, so this does not assume premultiplied alpha.
pub(crate) fn properties(
    options: D2D1_BITMAP_OPTIONS,
    alpha: D2D1_ALPHA_MODE,
) -> D2D1_BITMAP_PROPERTIES1 {
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

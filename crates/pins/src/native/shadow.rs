//! Drop shadow behind a pin.
//!
//! A pin presents one bitmap in one layered window, so its shadow cannot be a
//! window of its own: it is baked into the bitmap that is presented. That makes
//! the window (and the bitmap) larger than the capture by a margin on every
//! side, which is why the pin anchors the image while it resizes: the capture
//! must stay on the screen pixels it was taken from.
//!
//! The shadow is the blurred rectangle of the image, pushed down by a small
//! offset and drawn in black behind it, with the same smoothstep falloff the
//! menu card uses (`snaplight_platform::popup_menu`). Pins are screen captures,
//! so the rectangle is the silhouette — but the falloff is computed per pixel
//! rather than by convolving the image, which keeps a 4K pin's shadow as cheap
//! as a small one's, and keeps the margin reserved for it exact.
//!
//! The composed bitmap is premultiplied BGRA, which is what both presentation
//! paths consume: `StaticSurface` uploads it as
//! `DXGI_ALPHA_MODE_PREMULTIPLIED` and `UpdateLayeredWindow` takes it with
//! `AC_SRC_ALPHA`.

use snaplight_domain::{Config, ImageDescriptor};

/// Shadow metrics as configured, in logical pixels at 96 DPI.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub blur: u32,
    pub offset_y: u32,
    /// Opacity at the image edge, in percent.
    pub opacity: u8,
}

impl Settings {
    pub fn from_config(config: &Config) -> Self {
        Self {
            blur: config.pin_shadow_blur,
            offset_y: config.pin_shadow_offset,
            opacity: config.pin_shadow_opacity,
        }
    }
    /// Metrics in physical pixels for a monitor whose scale is `dpi`. The pin's
    /// content is physical pixels, so the shadow follows the display scale the
    /// same way the rest of the interface does.
    pub fn at(&self, dpi: f32) -> Params {
        let dpi = if dpi.is_finite() {
            dpi.clamp(1.0, 4.0)
        } else {
            1.0
        };
        Params {
            blur: self.blur as f32 * dpi,
            offset_y: self.offset_y as f32 * dpi,
            opacity: f32::from(self.opacity.min(100)) / 100.0,
        }
    }
}

/// Shadow metrics in physical pixels at 100% zoom: the space the pin composes
/// its bitmap in before zooming.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    pub blur: f32,
    pub offset_y: f32,
    /// Opacity at the image edge, 0..1.
    pub opacity: f32,
}

impl Params {
    /// Margin the window reserves on every side. The blur fades out exactly
    /// `blur` pixels past the shadow rectangle, and the offset pushes that
    /// rectangle down, so this much room is what makes the shadow reach zero
    /// before the edge of the bitmap instead of being cut off there.
    pub fn margin(&self) -> i32 {
        let margin = (self.blur + self.offset_y).ceil();
        if margin.is_finite() && margin > 0.0 {
            margin as i32
        } else {
            0
        }
    }
}

/// Physical margin for a zoom, so the window and the bitmap it presents always
/// agree on where the image starts.
pub fn scaled_margin(margin: i32, scale: f32) -> u32 {
    let scaled = margin.max(0) as f32 * scale;
    if scaled.is_finite() && scaled > 0.0 {
        scaled.round() as u32
    } else {
        0
    }
}

/// Size in physical pixels of the bitmap that holds `image` plus `margin` on
/// every side, zoomed by `scale`.
pub fn composed_size(image_width: u32, image_height: u32, margin: i32, scale: f32) -> (u32, u32) {
    let margin = scaled_margin(margin, scale);
    (
        scaled(image_width, scale) + 2 * margin,
        scaled(image_height, scale) + 2 * margin,
    )
}

/// Zooms one dimension. The visible zoom comes from the window size, so the
/// bitmap keeps at least one pixel instead of collapsing to nothing.
fn scaled(value: u32, scale: f32) -> u32 {
    let scaled = value as f32 * scale;
    if scaled.is_finite() {
        scaled.max(1.0) as u32
    } else {
        value
    }
}

/// Renders `image` with its shadow: the image `scale` times its captured size,
/// centred in a bitmap that reserves `params.margin()` around it.
pub fn compose(
    image: &ImageDescriptor,
    bytes: &[u8],
    params: &Params,
    scale: f32,
) -> std::result::Result<(ImageDescriptor, Vec<u8>), Box<dyn std::error::Error>> {
    let length = image.byte_len().map_err(|e| e.to_string())?;
    if bytes.len() < length {
        return Err("pixel buffer is shorter than the image".into());
    }
    let scale = if scale.is_finite() {
        scale.clamp(0.1, 4.0)
    } else {
        1.0
    };
    let margin = scaled_margin(params.margin(), scale);
    let (width, height) = composed_size(image.width, image.height, params.margin(), scale);
    let image_width = scaled(image.width, scale);
    let image_height = scaled(image.height, scale);
    let stride = width
        .checked_mul(4)
        .ok_or("shadow bitmap stride overflow")?;
    let size = (stride as usize)
        .checked_mul(height as usize)
        .ok_or("shadow bitmap overflow")?;
    let mut out = vec![0u8; size];

    let blur = (params.blur * scale).max(0.0);
    let opacity = params.opacity.clamp(0.0, 1.0);
    // The shadow rectangle in bitmap space: the image pushed down by the
    // offset. Cells inside it are at full opacity; the falloff only happens
    // outside, which is what makes the top edge stay clean.
    let left = margin as f32;
    let top = margin as f32 + params.offset_y * scale;
    let right = left + image_width as f32;
    let bottom = top + image_height as f32;
    let margin = margin as i32;
    let image_width = image_width as i32;
    let image_height = image_height as i32;

    for y in 0..height as i32 {
        for x in 0..width as i32 {
            let dest = (y as usize * width as usize + x as usize) * 4;
            let (ix, iy) = (x - margin, y - margin);
            let inside = (0..image_width).contains(&ix) && (0..image_height).contains(&iy);
            if inside {
                let sx = (ix as u64 * u64::from(image.width) / image_width.max(1) as u64) as usize;
                let sy =
                    (iy as u64 * u64::from(image.height) / image_height.max(1) as u64) as usize;
                let source = sy * image.stride as usize + sx * 4;
                out[dest..dest + 4].copy_from_slice(&bytes[source..source + 4]);
                // An opaque capture hides the shadow behind it, which is every
                // pin in practice: only the margin costs anything.
                if out[dest + 3] == 255 {
                    continue;
                }
            }
            let coverage = coverage(
                x as f32 + 0.5,
                y as f32 + 0.5,
                left,
                top,
                right,
                bottom,
                blur,
                opacity,
            );
            if coverage <= 0.0 {
                continue;
            }
            // The shadow is black, so its premultiplied colour is zero and only
            // alpha changes: the image's own premultiplied colour is already
            // what shows through it.
            let under = if inside {
                f32::from(out[dest + 3]) / 255.0
            } else {
                0.0
            };
            let alpha = under + coverage * (1.0 - under);
            out[dest + 3] = (alpha * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }

    let composed = ImageDescriptor {
        id: image.id,
        width,
        height,
        stride,
        format: image.format,
        alpha: image.alpha,
        color_space: image.color_space,
    };
    Ok((composed, out))
}

/// Opacity of the shadow at one bitmap cell, 0..1. Inside the rectangle it is
/// the configured opacity; outside it falls off with the same smoothstep the
/// menu card shadow uses, reaching zero `blur` pixels past the edge.
#[allow(clippy::too_many_arguments)]
fn coverage(
    x: f32,
    y: f32,
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
    blur: f32,
    opacity: f32,
) -> f32 {
    let dx = (left - x).max(x - right).max(0.0);
    let dy = (top - y).max(y - bottom).max(0.0);
    let distance = (dx * dx + dy * dy).sqrt();
    if blur <= 0.0 {
        return if distance > 0.0 { 0.0 } else { opacity };
    }
    let t = (distance / blur).clamp(0.0, 1.0);
    opacity * (1.0 - t * t * (3.0 - 2.0 * t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use snaplight_domain::{AlphaMode, ColorSpace, ImageId, PixelFormat};

    /// An opaque image, one distinct colour per pixel so a copy can be told
    /// apart from a rendered shadow.
    fn image(width: u32, height: u32) -> (ImageDescriptor, Vec<u8>) {
        let descriptor = ImageDescriptor {
            id: ImageId(1),
            width,
            height,
            stride: width * 4,
            format: PixelFormat::Bgra8,
            alpha: AlphaMode::Opaque,
            color_space: ColorSpace::Srgb,
        };
        let mut bytes = Vec::with_capacity((width * height * 4) as usize);
        for index in 0..(width * height) {
            bytes.extend_from_slice(&[(index % 251) as u8, (index / 7 % 251) as u8, 200, 255]);
        }
        (descriptor, bytes)
    }

    fn params() -> Params {
        Settings {
            blur: 12,
            offset_y: 4,
            opacity: 40,
        }
        .at(1.0)
    }

    fn pixel(bytes: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let start = ((y * width + x) * 4) as usize;
        bytes[start..start + 4].try_into().unwrap()
    }

    #[test]
    fn settings_scale_with_the_display_and_keep_the_configured_shape() {
        let settings = Settings {
            blur: 12,
            offset_y: 4,
            opacity: 40,
        };
        assert_eq!(
            settings.at(1.0),
            Params {
                blur: 12.0,
                offset_y: 4.0,
                opacity: 0.4
            }
        );
        // A 150% display gets half again as many physical pixels, so the shadow
        // reads the same thickness there.
        assert_eq!(settings.at(1.5).blur, 18.0);
        assert_eq!(settings.at(1.5).margin(), 24);
        // A nonsensical scale falls back to 100% instead of collapsing.
        assert_eq!(settings.at(f32::NAN).blur, 12.0);
        assert_eq!(settings.at(40.0).blur, 48.0);
    }

    #[test]
    fn composed_size_reserves_the_margin_at_every_zoom() {
        assert_eq!(composed_size(100, 50, 16, 1.0), (132, 82));
        assert_eq!(composed_size(100, 50, 16, 2.0), (264, 164));
        assert_eq!(composed_size(100, 50, 16, 0.5), (66, 41));
        // Without a shadow the bitmap is the image, whatever the zoom.
        assert_eq!(composed_size(100, 50, 0, 1.0), (100, 50));
        assert_eq!(composed_size(100, 50, 0, 0.5), (50, 25));
    }

    #[test]
    fn an_opaque_image_keeps_its_pixels_and_gains_a_shadow_ring() {
        let (image, bytes) = image(8, 8);
        let params = params();
        let (composed, pixels) = compose(&image, &bytes, &params, 1.0).unwrap();
        assert_eq!((composed.width, composed.height), (8 + 32, 8 + 32));
        assert_eq!(composed.stride, composed.width * 4);
        // Every pixel of the capture is copied through untouched.
        for y in 0..image.height {
            for x in 0..image.width {
                assert_eq!(
                    pixel(&pixels, composed.width, x + 16, y + 16),
                    pixel(&bytes, image.width, x, y),
                    "capture pixel ({x}, {y}) changed"
                );
            }
        }
        // Just below the image the shadow is nearly at full opacity; it has
        // faded out completely by the outer corner of the bitmap.
        let edge = pixel(&pixels, composed.width, 16 + 4, 16 + image.height + 1);
        assert_eq!(edge[..3], [0, 0, 0], "the shadow must be black");
        assert!(
            (90..=110).contains(&edge[3]),
            "shadow at the bottom edge: {edge:?}"
        );
        let corner = pixel(&pixels, composed.width, 0, 0);
        assert_eq!(corner, [0, 0, 0, 0], "the shadow must not be cut off");
        // Far enough to the side the falloff is gone as well.
        let side = pixel(
            &pixels,
            composed.width,
            composed.width - 1,
            composed.height / 2,
        );
        assert_eq!(side, [0, 0, 0, 0]);
    }

    #[test]
    fn the_shadow_falls_below_the_image_not_above_it() {
        let (image, bytes) = image(8, 8);
        let params = Params {
            blur: 12.0,
            offset_y: 4.0,
            opacity: 0.4,
        };
        let (composed, pixels) = compose(&image, &bytes, &params, 1.0).unwrap();
        let above = pixel(&pixels, composed.width, 16 + 4, 15)[3];
        let below = pixel(&pixels, composed.width, 16 + 4, 16 + 8 + 1)[3];
        assert!(
            below > above,
            "the offset has to push the shadow down (above {above}, below {below})"
        );
        assert!(above > 0, "the blurred top edge still darkens a little");
    }

    #[test]
    fn a_shadow_with_no_room_is_the_image_itself() {
        let (image, bytes) = image(4, 4);
        // A zero blur and offset ask for a shadow that is entirely covered by
        // the image, so nothing is drawn and nothing is reserved.
        let params = Params {
            blur: 0.0,
            offset_y: 0.0,
            opacity: 0.4,
        };
        assert_eq!(params.margin(), 0);
        let (composed, pixels) = compose(&image, &bytes, &params, 1.0).unwrap();
        assert_eq!((composed.width, composed.height), (4, 4));
        assert_eq!(pixels, bytes);
    }

    #[test]
    fn a_translucent_image_keeps_its_colour_and_picks_up_the_shadow() {
        let (image, mut bytes) = image(4, 4);
        // Half-transparent pixel, premultiplied, in the middle of the capture.
        let middle = ((2 * image.width + 2) * 4) as usize;
        bytes[middle..middle + 4].copy_from_slice(&[0, 0, 128, 128]);
        let (composed, pixels) = compose(&image, &bytes, &params(), 1.0).unwrap();
        let out = pixel(&pixels, composed.width, 16 + 2, 16 + 2);
        assert_eq!(
            out[..3],
            [0, 0, 128],
            "the shadow is black, so the image keeps its premultiplied colour"
        );
        assert!(
            out[3] > 128 && out[3] < 255,
            "the shadow shows through the translucent capture: {out:?}"
        );
    }

    #[test]
    fn the_shadow_zooms_with_the_pin() {
        let (image, bytes) = image(8, 8);
        let (composed, pixels) = compose(&image, &bytes, &params(), 2.0).unwrap();
        assert_eq!((composed.width, composed.height), (80, 80));
        // The image sits in the middle of the reserved margin, doubled.
        assert_eq!(
            pixel(&pixels, composed.width, 32, 32),
            pixel(&bytes, 8, 0, 0)
        );
        // Sampling stays in range at both ends of the capture.
        assert_eq!(
            pixel(&pixels, composed.width, 47, 47),
            pixel(&bytes, 8, 7, 7)
        );
    }
}

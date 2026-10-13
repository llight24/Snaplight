//! Colour picker window. Keeps its HSV state, the hue square's base colour,
//! the swatch and the hex text in agreement; the RGB/HSV conversions live here
//! because the picker is their only caller.

use crate::{config_color, rgba_of, slint_color, ColorPicker};
use snaplight_domain::{format_hex_color, parse_hex_color, DEFAULT_STROKE_COLOR};

/// HSV (each 0..1) to opaque sRGB, using the standard six-sector formula.
fn hsv_to_rgb(hue: f32, saturation: f32, value: f32) -> [f32; 4] {
    let hue = hue.rem_euclid(1.0);
    let saturation = saturation.clamp(0.0, 1.0);
    let value = value.clamp(0.0, 1.0);
    let sector = hue * 6.0;
    let fraction = sector - sector.floor();
    let p = value * (1.0 - saturation);
    let q = value * (1.0 - fraction * saturation);
    let t = value * (1.0 - (1.0 - fraction) * saturation);
    let (red, green, blue) = match sector.floor() as i32 % 6 {
        0 => (value, t, p),
        1 => (q, value, p),
        2 => (p, value, t),
        3 => (p, q, value),
        4 => (t, p, value),
        _ => (value, p, q),
    };
    [red, green, blue, 1.0]
}
/// sRGB to HSV. A grey or black input has no hue, so it reports zero hue and
/// saturation; `picker_set_color` keeps the previous hue for black.
fn rgb_to_hsv(red: f32, green: f32, blue: f32) -> (f32, f32, f32) {
    let max = red.max(green).max(blue);
    let min = red.min(green).min(blue);
    let delta = max - min;
    if delta <= f32::EPSILON {
        return (0.0, 0.0, max);
    }
    let hue = if max == red {
        (green - blue) / delta
    } else if max == green {
        (blue - red) / delta + 2.0
    } else {
        (red - green) / delta + 4.0
    };
    ((hue / 6.0).rem_euclid(1.0), delta / max, max)
}
/// Parses `#RRGGBB`(/`AA`) into a Slint colour; the validation path behind
/// `picker_set_hex`.
fn picker_hex_color(text: &str) -> Option<slint::Color> {
    parse_hex_color(text).map(slint_color)
}
/// Creates the colour picker and seeds it with the default stroke colour, so
/// its swatch, hex text and hue square agree from the first frame.
pub fn color_picker() -> Result<ColorPicker, slint::PlatformError> {
    let ui = ColorPicker::new()?;
    picker_set_color(&ui, config_color(DEFAULT_STROKE_COLOR));
    Ok(ui)
}
/// Moves the hue slider, keeping the current saturation and brightness. The
/// square's base colour is the fully saturated shade of the new hue.
pub fn picker_set_hue(picker: &ColorPicker, hue: f32) -> slint::Color {
    let hue = hue.clamp(0.0, 1.0);
    let saturation = picker.get_saturation().clamp(0.0, 1.0);
    let brightness = picker.get_brightness().clamp(0.0, 1.0);
    picker.set_hue(hue);
    picker.set_sv_base(slint_color(hsv_to_rgb(hue, 1.0, 1.0)));
    let color = slint_color(hsv_to_rgb(hue, saturation, brightness));
    picker.set_color_value(color);
    picker.set_hex_text(format_hex_color(rgba_of(color)).into());
    color
}
/// Moves the position inside the saturation/brightness square, keeping the hue.
pub fn picker_set_sv(picker: &ColorPicker, saturation: f32, brightness: f32) -> slint::Color {
    let saturation = saturation.clamp(0.0, 1.0);
    let brightness = brightness.clamp(0.0, 1.0);
    let hue = picker.get_hue().clamp(0.0, 1.0);
    picker.set_saturation(saturation);
    picker.set_brightness(brightness);
    let color = slint_color(hsv_to_rgb(hue, saturation, brightness));
    picker.set_color_value(color);
    picker.set_hex_text(format_hex_color(rgba_of(color)).into());
    color
}
/// Parses a hex string typed into the picker. Returns `None` and leaves the
/// window untouched when the text is not a colour.
pub fn picker_set_hex(picker: &ColorPicker, text: &str) -> Option<slint::Color> {
    let color = picker_hex_color(text)?;
    Some(picker_set_color(picker, color))
}
/// Applies a colour wholesale, deriving hue, saturation and the square base
/// from its RGB. Black keeps the previous hue and saturation because it has
/// none of its own.
pub fn picker_set_color(picker: &ColorPicker, color: slint::Color) -> slint::Color {
    let rgba = rgba_of(color);
    let (mut hue, mut saturation, brightness) = rgb_to_hsv(rgba[0], rgba[1], rgba[2]);
    if brightness <= f32::EPSILON {
        hue = picker.get_hue();
        saturation = picker.get_saturation();
    }
    picker.set_hue(hue);
    picker.set_saturation(saturation);
    picker.set_brightness(brightness);
    picker.set_sv_base(slint_color(hsv_to_rgb(hue, 1.0, 1.0)));
    picker.set_color_value(color);
    picker.set_hex_text(format_hex_color(rgba).into());
    color
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_hex_parsing_accepts_only_colours() {
        let red = picker_hex_color("#F23340").expect("valid hex");
        assert_eq!(
            red,
            slint_color([242.0 / 255.0, 51.0 / 255.0, 64.0 / 255.0, 1.0])
        );
        // Surrounding whitespace and alpha are accepted, as in the domain.
        assert!(picker_hex_color(" #F23340 ").is_some());
        assert!(picker_hex_color("#F2334000").is_some());
        for text in ["F23340", "#GGGGGG", "#F2334", "", "#F23340Z"] {
            assert!(
                picker_hex_color(text).is_none(),
                "{text} should be rejected"
            );
        }
    }
    #[test]
    fn hsv_round_trips_through_rgb() {
        for (hue, saturation, brightness) in [
            (0.0, 1.0, 0.95),
            (0.5, 0.8, 0.6),
            (0.98, 0.5, 1.0),
            (0.33, 1.0, 1.0),
        ] {
            let rgb = hsv_to_rgb(hue, saturation, brightness);
            let (h, s, v) = rgb_to_hsv(rgb[0], rgb[1], rgb[2]);
            assert!((h - hue).abs() < 1e-3, "hue {hue} -> {h}");
            assert!(
                (s - saturation).abs() < 1e-3,
                "saturation {saturation} -> {s}"
            );
            assert!(
                (v - brightness).abs() < 1e-3,
                "brightness {brightness} -> {v}"
            );
        }
        // Pure red resolves to hue zero with full saturation.
        assert_eq!(rgb_to_hsv(1.0, 0.0, 0.0), (0.0, 1.0, 1.0));
    }
}

//! Smoke and stress support shared by the editor's self-checks: the fixed paint
//! geometry the smoke frame and its pixel checks agree on, a large deterministic
//! annotation set for the `--stress` frame loop, and the small window helpers the
//! smoke checks run against real windows. The smoke and stress sessions
//! themselves stay in the parent module; only these pieces are shared.

use snaplight_domain::{Annotation, ImagePoint, ImageRect, Shape, Style};
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    UI::WindowsAndMessaging::{DefWindowProcW, GetTopWindow, GetWindow, GW_HWNDNEXT},
};

/// Frames rendered by the `--stress` 4K annotation loop.
pub(super) const STRESS_FRAMES: usize = 240;
/// Paint checks of the smoke frame: a filled shape at `FILLED`, the same colour
/// as an outline at `OUTLINED`, so the composed pixels prove that each paint is
/// applied where it should be and only there.
pub(super) const FILL: [f32; 4] = [0.2, 0.85, 0.35, 1.0];
pub(super) const FILLED: ImageRect = ImageRect {
    x: 300.0,
    y: 200.0,
    width: 120.0,
    height: 60.0,
};
pub(super) const OUTLINED: ImageRect = ImageRect {
    x: 460.0,
    y: 200.0,
    width: 120.0,
    height: 60.0,
};
/// Text annotation of the smoke frame: filled and outlined, so its glyph
/// outlines have to produce ink in the box the check scans.
pub(super) const SMOKE_TEXT: ImagePoint = ImagePoint { x: 320.0, y: 120.0 };
/// Builds a large, deterministic annotation set for the `--stress` frame loop.
pub(super) fn stress_annotations(width: u32, height: u32) -> Vec<Annotation> {
    const COUNT: usize = 1000;
    let max_x = (width as f32 - 200.0).max(1.0);
    let max_y = (height as f32 - 160.0).max(1.0);
    (0..COUNT)
        .map(|i| {
            let x = (i as f32 * 37.0) % max_x;
            let y = (i as f32 * 91.0) % max_y;
            let shape = match i % 5 {
                0 => Shape::Rectangle(ImageRect {
                    x,
                    y,
                    width: 120.0,
                    height: 60.0,
                }),
                1 => Shape::Ellipse(ImageRect {
                    x,
                    y,
                    width: 90.0,
                    height: 70.0,
                }),
                2 => Shape::Arrow {
                    from: ImagePoint { x, y },
                    to: ImagePoint {
                        x: x + 140.0,
                        y: y + 90.0,
                    },
                },
                3 => Shape::Pen(vec![
                    ImagePoint { x, y },
                    ImagePoint {
                        x: x + 30.0,
                        y: y + 20.0,
                    },
                    ImagePoint {
                        x: x + 60.0,
                        y: y + 5.0,
                    },
                ]),
                _ => Shape::Mosaic {
                    rect: ImageRect {
                        x,
                        y,
                        width: 128.0,
                        height: 96.0,
                    },
                    block: 12,
                },
            };
            Annotation {
                shape,
                style: Style::default(),
            }
        })
        .collect()
}
/// Position of a top-level window in the desktop z-order, counting from the
/// front. Used by the smoke check that the floating windows stay in front of the
/// capture overlay.
pub(super) fn z_order(hwnd: HWND) -> Option<usize> {
    unsafe {
        let mut current = GetTopWindow(None).ok()?;
        for index in 0..2000 {
            if current == hwnd {
                return Some(index);
            }
            current = GetWindow(current, GW_HWNDNEXT).ok()?;
        }
    }
    None
}
/// Window procedure of the smoke stand-in for the capture overlay. It only has
/// to exist: the check compares z-order, not behaviour.
pub(super) unsafe extern "system" fn overlay_stand_in_proc(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}
/// Extent of the paint row in a window snapshot, in the snapshot's own pixels.
/// The window is rendered wider than the row needs, so the row sits in the middle
/// with the panel's close button at the right edge; `right_margin` keeps that
/// button out of the measurement. Anything that is not white counts as painted,
/// which covers the light borders of spin boxes and text fields.
pub(super) fn row_extent(
    bytes: &[u8],
    width: u32,
    height: u32,
    right_margin: u32,
) -> Option<(u32, u32)> {
    // The band is a strip through the middle of the row: the rounded window
    // border and the transparent corners are outside it, so counting them as
    // painted would stretch the measurement to the whole window.
    let band = (height / 2 - 6)..(height / 2 + 6);
    let limit = width.saturating_sub(right_margin);
    let painted = |x: u32| {
        band.clone().any(|y| {
            let n = (y * width + x) as usize * 4;
            bytes[n] < 250 || bytes[n + 1] < 250 || bytes[n + 2] < 250
        })
    };
    // Four columns are skipped at the edges for the same reason.
    let left = (4..limit).find(|x| painted(*x))?;
    let right = (4..limit).rev().find(|x| painted(*x))?;
    (right >= left).then_some((left, right))
}

//! Long-capture stitching: merges successive scroll frames of one screen region
//! into a single tall image.
//!
//! The engine is pure: it works on BGRA8 buffers and never touches the platform,
//! so the overlap search, the budget rules, and the stop conditions are unit
//! tested without a screen.

use crate::{AlphaMode, ColorSpace, ImageDescriptor, ImageId, PixelFormat};

/// Hard cap on the stitched height in physical pixels. Every image path in the
/// app (PNG, DIB, D2D bitmap) addresses this comfortably.
pub const MAX_HEIGHT: u32 = 32_768;
/// Hard cap on the stitched buffer, in bytes.
pub const MAX_BYTES: usize = 512 * 1024 * 1024;
/// Largest per-channel difference that still counts as the same pixel. Save and
/// scroll animations leave a little transitory noise behind.
const PIXEL_TOLERANCE: u8 = 12;
/// Rows and columns sampled inside a candidate overlap.
const SAMPLE_ROWS: u32 = 40;
const SAMPLE_COLUMNS: u32 = 48;
/// Share of a row's sampled pixels that must agree for the row to count as matched.
const ROW_MATCH_RATIO: f32 = 0.75;
/// Share of sampled rows that must match for a candidate overlap to be considered.
const CANDIDATE_ROW_RATIO: f32 = 0.6;
/// Smallest accepted overlap. Anything below this is a jump, not a scroll: a
/// frame that shares fewer rows than this cannot be stitched without a visible
/// gap, so the caller stops instead of inventing content.
const MIN_OVERLAP: u32 = 8;
/// Weight of the "close to the previous scroll delta" bonus. A constant wheel
/// delta scrolls by a nearly constant number of rows, which disambiguates
/// repeating content (stripes, tables, code) that matches at several offsets.
const DELTA_WEIGHT: f32 = 0.5;

/// One captured screen region: BGRA8, top-down, `stride` bytes per row.
#[derive(Clone, Copy)]
pub struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub bytes: &'a [u8],
}
/// What one [`LongCapture::push`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// `rows` new rows were appended after `overlap` rows of the previous image.
    Scrolled { overlap: u32, rows: u32 },
    /// The frame repeats the previous image: the content did not move.
    Unchanged,
    /// The frame shares no usable overlap with the previous image.
    Jumped,
    /// A cap stopped the capture; the stitched image so far stays usable.
    Budget(&'static str),
}

/// Sample `count` evenly spread indices below `end`, always including 0.
fn sample_indices(end: u32, count: u32) -> Vec<u32> {
    let count = count.min(end.max(1));
    (0..count).map(|i| i * end / count).collect()
}

/// Largest overlap `k` such that the bottom `k` rows of the previous image match
/// the top `k` rows of `next`.
///
/// `expected_rows` is the scroll delta of the previous step, when there was one;
/// candidates that scroll by a similar amount are preferred, which keeps
/// repeating content from anchoring the search on the wrong offset.
#[allow(clippy::too_many_arguments)]
pub fn matching_overlap(
    width: u32,
    previous: &[u8],
    previous_stride: usize,
    previous_height: u32,
    next: &[u8],
    next_stride: usize,
    next_height: u32,
    expected_rows: Option<u32>,
) -> Option<u32> {
    let max = previous_height.min(next_height);
    if width == 0 || max < MIN_OVERLAP {
        return None;
    }
    let columns = sample_indices(width, SAMPLE_COLUMNS);
    let mut best: Option<(f32, u32)> = None;
    for overlap in (MIN_OVERLAP..=max).rev() {
        let rows = sample_indices(overlap, SAMPLE_ROWS);
        let mut matched = 0u32;
        let mut failed = 0u32;
        for i in &rows {
            let i = *i;
            let a = &previous[(previous_height - overlap + i) as usize * previous_stride..];
            let b = &next[i as usize * next_stride..];
            let mut same = 0u32;
            for x in &columns {
                let x = *x as usize * 4;
                if (0..3).all(|c| a[x + c].abs_diff(b[x + c]) <= PIXEL_TOLERANCE) {
                    same += 1;
                }
            }
            if same as f32 / columns.len() as f32 >= ROW_MATCH_RATIO {
                matched += 1;
            } else {
                failed += 1;
                // Bail out as soon as this offset cannot reach the ratio anymore.
                if failed as f32 > rows.len() as f32 * (1.0 - CANDIDATE_ROW_RATIO) {
                    break;
                }
            }
        }
        let ratio = matched as f32 / rows.len().max(1) as f32;
        if ratio < CANDIDATE_ROW_RATIO {
            continue;
        }
        let appended = next_height - overlap;
        let score = match expected_rows {
            Some(expected) if expected > 0 => {
                ratio - DELTA_WEIGHT * (appended as f32 - expected as f32).abs() / expected as f32
            }
            _ => ratio,
        };
        if best.is_none_or(|(best_score, _)| score > best_score) {
            best = Some((score, overlap));
        }
    }
    best.map(|(_, overlap)| overlap)
}

/// Accumulates scroll frames into one tall image.
pub struct LongCapture {
    width: u32,
    height: u32,
    stride: usize,
    bytes: Vec<u8>,
    budget: usize,
    last_rows: Option<u32>,
}
impl LongCapture {
    /// Starts a capture from the first frame. Fails when that frame alone does
    /// not fit the memory budget.
    pub fn new(frame: Frame<'_>, budget: usize) -> Result<Self, &'static str> {
        if frame.width == 0 || frame.height == 0 {
            return Err("抓取区域为空");
        }
        if frame.stride != frame.width as usize * 4
            || frame.bytes.len() < frame.stride * frame.height as usize
        {
            return Err("抓取数据与区域尺寸不符");
        }
        let budget = budget.min(MAX_BYTES);
        let stride = frame.width as usize * 4;
        let len = stride * frame.height as usize;
        if len > budget {
            return Err("拼接内存预算不足");
        }
        let mut bytes = Vec::with_capacity(len);
        for row in 0..frame.height as usize {
            bytes.extend_from_slice(&frame.bytes[row * frame.stride..][..stride]);
        }
        opaque(&mut bytes);
        Ok(Self {
            width: frame.width,
            height: frame.height,
            stride,
            bytes,
            budget,
            last_rows: None,
        })
    }
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn stride(&self) -> usize {
        self.stride
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// Descriptor of the stitched image, ready for `write_png` or a document.
    pub fn descriptor(&self, id: ImageId) -> ImageDescriptor {
        ImageDescriptor {
            id,
            width: self.width,
            height: self.height,
            stride: self.stride as u32,
            format: PixelFormat::Bgra8,
            alpha: AlphaMode::Opaque,
            color_space: ColorSpace::Srgb,
        }
    }
    /// Appends one scroll frame, or reports why it cannot be appended.
    pub fn push(&mut self, frame: &Frame<'_>) -> Step {
        if frame.width != self.width
            || frame.height == 0
            || frame.stride != frame.width as usize * 4
            || frame.bytes.len() < frame.stride * frame.height as usize
        {
            return Step::Jumped;
        }
        let Some(overlap) = matching_overlap(
            self.width,
            &self.bytes,
            self.stride,
            self.height,
            frame.bytes,
            frame.stride,
            frame.height,
            self.last_rows,
        ) else {
            return Step::Jumped;
        };
        if overlap >= frame.height {
            return Step::Unchanged;
        }
        let rows = frame.height - overlap;
        if self.height + rows > MAX_HEIGHT {
            return Step::Budget("已达最大高度");
        }
        if (self.height + rows) as usize * self.stride > self.budget {
            return Step::Budget("已达内存预算");
        }
        let start = self.height as usize * self.stride;
        let mut appended = Vec::with_capacity(rows as usize * self.stride);
        for row in 0..rows as usize {
            appended.extend_from_slice(
                &frame.bytes[(overlap as usize + row) * frame.stride..][..self.stride],
            );
        }
        opaque(&mut appended);
        self.bytes.extend_from_slice(&appended);
        debug_assert_eq!(self.bytes.len(), start + appended.len());
        self.height += rows;
        self.last_rows = Some(rows);
        Step::Scrolled { overlap, rows }
    }
}

/// Forces the alpha channel opaque: GDI screen captures leave it zeroed, and the
/// stitched image is always opaque.
fn opaque(bytes: &mut [u8]) {
    for pixel in bytes.as_chunks_mut::<4>().0 {
        pixel[3] = 255;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDTH: u32 = 64;
    const FRAME_HEIGHT: u32 = 300;

    fn stride() -> usize {
        WIDTH as usize * 4
    }
    /// Deterministic "page": every row looks different, so an overlap can only
    /// match at the right offset.
    fn page(height: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; stride() * height as usize];
        for y in 0..height {
            for x in 0..WIDTH {
                let i = y as usize * stride() + x as usize * 4;
                bytes[i] = (x.wrapping_mul(7).wrapping_add(y.wrapping_mul(13))) as u8;
                bytes[i + 1] = (y.wrapping_mul(31) ^ x) as u8;
                bytes[i + 2] = (x.wrapping_mul(3).wrapping_add(y.wrapping_mul(97))) as u8;
                bytes[i + 3] = 255;
            }
        }
        bytes
    }
    fn crop(source: &[u8], offset: u32, height: u32) -> Vec<u8> {
        let start = offset as usize * stride();
        source[start..start + height as usize * stride()].to_vec()
    }
    fn frame(bytes: &[u8]) -> Frame<'_> {
        Frame {
            width: WIDTH,
            height: bytes.len() as u32 / stride() as u32,
            stride: stride(),
            bytes,
        }
    }
    /// Adds a per-frame noise floor, as a live screen has.
    fn noisy(bytes: &[u8], seed: u8) -> Vec<u8> {
        let mut out = bytes.to_vec();
        for (i, value) in out.iter_mut().enumerate() {
            if i % 4 != 3 {
                *value = value.wrapping_add((i as u8).wrapping_mul(seed).wrapping_add(seed) % 5);
            }
        }
        out
    }

    #[test]
    fn scrolling_frames_stitch_into_one_image() {
        let source = page(1000);
        let mut capture =
            LongCapture::new(frame(&crop(&source, 0, FRAME_HEIGHT)), MAX_BYTES).unwrap();
        for offset in [120u32, 240, 360, 480] {
            let next = crop(&source, offset, FRAME_HEIGHT);
            assert_eq!(
                capture.push(&frame(&next)),
                Step::Scrolled {
                    overlap: FRAME_HEIGHT - 120,
                    rows: 120
                }
            );
        }
        assert_eq!(capture.height(), 780);
        assert_eq!(capture.bytes(), &source[..780 * stride()]);
    }

    #[test]
    fn repeated_frame_is_unchanged() {
        let source = page(400);
        let first = crop(&source, 0, FRAME_HEIGHT);
        let mut capture = LongCapture::new(frame(&first), MAX_BYTES).unwrap();
        assert_eq!(capture.push(&frame(&first)), Step::Unchanged);
        assert_eq!(capture.height(), FRAME_HEIGHT);
    }

    #[test]
    fn noise_does_not_break_the_overlap_search() {
        let source = page(1000);
        let first = noisy(&crop(&source, 0, FRAME_HEIGHT), 7);
        let mut capture = LongCapture::new(frame(&first), MAX_BYTES).unwrap();
        for offset in [120u32, 240, 360] {
            let next = noisy(&crop(&source, offset, FRAME_HEIGHT), 7 + offset as u8);
            assert_eq!(
                capture.push(&frame(&next)),
                Step::Scrolled {
                    overlap: FRAME_HEIGHT - 120,
                    rows: 120
                }
            );
        }
        assert_eq!(capture.height(), 660);
    }

    #[test]
    fn a_fixed_band_at_the_top_is_tolerated() {
        // Sticky headers and browser chrome do not scroll with the page: their
        // rows never match, so the search has to work with the rest of the frame.
        let source = page(1000);
        let banded = |offset: u32| {
            let mut bytes = crop(&source, offset, FRAME_HEIGHT);
            for row in 0..40usize {
                for x in 0..WIDTH as usize {
                    let i = row * stride() + x * 4;
                    bytes[i] = 0x20;
                    bytes[i + 1] = 0x60;
                    bytes[i + 2] = 0xa0;
                }
            }
            bytes
        };
        let first = banded(0);
        let mut capture = LongCapture::new(frame(&first), MAX_BYTES).unwrap();
        for offset in [120u32, 240] {
            assert_eq!(
                capture.push(&frame(&banded(offset))),
                Step::Scrolled {
                    overlap: FRAME_HEIGHT - 120,
                    rows: 120
                }
            );
        }
        assert_eq!(capture.height(), 540);
    }

    #[test]
    fn unrelated_frame_is_a_jump() {
        let source = page(1000);
        let other = page(1000)
            .iter()
            .map(|v| v.wrapping_mul(3))
            .collect::<Vec<u8>>();
        let mut capture =
            LongCapture::new(frame(&crop(&source, 0, FRAME_HEIGHT)), MAX_BYTES).unwrap();
        assert_eq!(
            capture.push(&frame(&crop(&other, 500, FRAME_HEIGHT))),
            Step::Jumped
        );
        assert_eq!(capture.height(), FRAME_HEIGHT);
    }

    #[test]
    fn budget_stops_appending() {
        let source = page(1000);
        let budget = stride() * (FRAME_HEIGHT as usize + 200);
        let mut capture = LongCapture::new(frame(&crop(&source, 0, FRAME_HEIGHT)), budget).unwrap();
        assert_eq!(
            capture.push(&frame(&crop(&source, 120, FRAME_HEIGHT))),
            Step::Scrolled {
                overlap: 180,
                rows: 120
            }
        );
        // 300 + 120 rows fit; the next append would need 540 rows.
        assert_eq!(
            capture.push(&frame(&crop(&source, 240, FRAME_HEIGHT))),
            Step::Budget("已达内存预算")
        );
        assert_eq!(capture.height(), 420);
    }

    #[test]
    fn first_frame_larger_than_the_budget_is_refused() {
        let source = page(400);
        assert!(LongCapture::new(frame(&crop(&source, 0, FRAME_HEIGHT)), stride()).is_err());
    }
}

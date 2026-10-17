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
/// Bonus for the direction the content already moved in, so a close call keeps
/// scrolling the same way instead of flipping the stitched image around.
const KEEP_DIRECTION_BONUS: f32 = 0.1;

/// One captured screen region: BGRA8, top-down, `stride` bytes per row.
#[derive(Clone, Copy)]
pub struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub bytes: &'a [u8],
}
/// Which way the content moved between two frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Scrolling down: new rows appear at the bottom of the frame.
    Down,
    /// Scrolling up: new rows appear at the top of the frame.
    Up,
}
/// What one [`LongCapture::advance`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// `rows` new rows were appended after `overlap` rows of the previous image.
    Scrolled { overlap: u32, rows: u32 },
    /// `rows` new rows were inserted before `overlap` rows of the previous image,
    /// because the content scrolled up.
    Prepended { overlap: u32, rows: u32 },
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
pub fn matching_overlap(
    previous: Frame<'_>,
    next: Frame<'_>,
    expected_rows: Option<u32>,
) -> Option<u32> {
    search_overlap(previous, next, expected_rows, |overlap| {
        next.height - overlap
    })
    .map(|(_, overlap)| overlap)
}

/// Shared search behind [`matching_overlap`], returning the best overlap together
/// with its score so a caller can compare candidates from both directions of
/// travel. `delta` turns a candidate overlap into the number of new rows it would
/// add, which is what the previous step's delta is compared against; the two
/// directions count it differently.
fn search_overlap(
    previous: Frame<'_>,
    next: Frame<'_>,
    expected_rows: Option<u32>,
    delta: impl Fn(u32) -> u32,
) -> Option<(f32, u32)> {
    let width = previous.width;
    let previous_height = previous.height;
    let previous_stride = previous.stride;
    let previous = previous.bytes;
    let next_height = next.height;
    let next_stride = next.stride;
    let next = next.bytes;
    let max = previous_height.min(next_height);
    if width == 0 || max < MIN_OVERLAP {
        return None;
    }
    let columns = sample_indices(width, SAMPLE_COLUMNS);
    let mut best: Option<(f32, u32)> = None;
    let mut any_detail = false;
    for overlap in (MIN_OVERLAP..=max).rev() {
        let rows = sample_indices(overlap, SAMPLE_ROWS);
        let mut matched = 0u32;
        let mut voting = 0u32;
        let mut failed = 0u32;
        for i in &rows {
            let i = *i;
            let a = &previous[(previous_height - overlap + i) as usize * previous_stride..];
            let b = &next[i as usize * next_stride..];
            // A row of one flat colour says nothing about the offset: it matches
            // at every candidate, so it must not vote. Skipping it is what keeps
            // a page of mostly empty background from anchoring on a wrong offset.
            if !carries_detail(a, &columns) && !carries_detail(b, &columns) {
                continue;
            }
            voting += 1;
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
        if voting == 0 {
            continue;
        }
        any_detail = true;
        let ratio = matched as f32 / voting as f32;
        if ratio < CANDIDATE_ROW_RATIO {
            continue;
        }
        let added = delta(overlap);
        let score = match expected_rows {
            Some(expected) if expected > 0 => {
                ratio - DELTA_WEIGHT * (added as f32 - expected as f32).abs() / expected as f32
            }
            _ => ratio,
        };
        if best.is_none_or(|(best_score, _)| score > best_score) {
            best = Some((score, overlap));
        }
    }
    // A frame of one flat colour carries no offset at all. Reporting the largest
    // overlap leaves the caller's image untouched instead of guessing at rows.
    if !any_detail {
        return Some((0.0, max));
    }
    best
}

/// Whether a row carries any detail across the sampled columns. Flat rows match
/// at every offset, so they cannot say where an overlap is.
fn carries_detail(row: &[u8], columns: &[u32]) -> bool {
    let mut low = 255u8;
    let mut high = 0u8;
    for x in columns {
        let x = *x as usize * 4;
        for channel in 0..3 {
            let value = row[x + channel];
            low = low.min(value);
            high = high.max(value);
        }
    }
    high - low > PIXEL_TOLERANCE
}

/// Accumulates scroll frames into one tall image.
///
/// The engine works in *page* coordinates: row 0 of the stitched image is page
/// position `origin`, which turns negative once the content scrolls up past the
/// first captured row, and the covered range is the image's own height. The most
/// recently accepted frame is remembered by its page position and height instead
/// of a second pixel buffer: its pixels are the image rows that window covers,
/// which is possible because that window is always inside the covered range.
///
/// Every new frame is matched against that window, so a scroll that comes back
/// into rows the stitch already holds is located exactly instead of being
/// compared against the two ends of an ever-growing image. Only rows outside the
/// covered range extend the image; the rest is dropped. A frame inside the range
/// therefore returns [`Step::Unchanged`] while still becoming the anchor for the
/// next scroll, and crossing an end again adds exactly the new rows.
pub struct LongCapture {
    width: u32,
    height: u32,
    stride: usize,
    bytes: Vec<u8>,
    budget: usize,
    /// Page position of the stitched image's first row.
    origin: i64,
    /// Page position of the most recently accepted frame's first row.
    last_top: i64,
    /// Height of the most recently accepted frame.
    last_height: u32,
    /// Rows the content moved between the last two accepted frames, used as the
    /// expected scroll for the next one.
    last_rows: Option<u32>,
}
impl LongCapture {
    /// Starts a capture from the first frame. Fails when that frame alone does
    /// not fit the memory budget or already exceeds the height cap.
    pub fn new(frame: Frame<'_>, budget: usize) -> Result<Self, &'static str> {
        if frame.width == 0 || frame.height == 0 {
            return Err("抓取区域为空");
        }
        if frame.height > MAX_HEIGHT {
            return Err("已达最大高度");
        }
        // Dense BGRA8 rows only: a padded stride or a truncated buffer cannot be
        // stitched, and a size that does not fit `usize` cannot be addressed.
        let Some(stride) = row_stride(frame.width) else {
            return Err("抓取数据与区域尺寸不符");
        };
        let Some(source_len) = frame.stride.checked_mul(frame.height as usize) else {
            return Err("抓取数据与区域尺寸不符");
        };
        if frame.stride != stride || frame.bytes.len() < source_len {
            return Err("抓取数据与区域尺寸不符");
        }
        let Some(len) = stride.checked_mul(frame.height as usize) else {
            return Err("拼接内存预算不足");
        };
        let budget = budget.min(MAX_BYTES);
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
            origin: 0,
            last_top: 0,
            last_height: frame.height,
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
    /// Appends one frame captured while the content scrolled down.
    pub fn push(&mut self, frame: &Frame<'_>) -> Step {
        self.advance(frame, Some(Direction::Down))
    }
    /// Stitches one frame in whichever direction the content moved. Both
    /// directions are searched and the better candidate wins, so a scroll that
    /// reverses direction keeps working; `previous` breaks a close call in favour
    /// of the direction the user was already scrolling.
    pub fn advance(&mut self, frame: &Frame<'_>, previous: Option<Direction>) -> Step {
        if !self.accepts(frame) {
            return Step::Jumped;
        }
        let mut best: Option<(f32, Direction, u32)> = None;
        for direction in [Direction::Down, Direction::Up] {
            let Some((score, overlap)) = self.candidate(frame, direction) else {
                continue;
            };
            let score = score
                + if Some(direction) == previous {
                    KEEP_DIRECTION_BONUS
                } else {
                    0.0
                };
            if best.is_none_or(|(best_score, _, _)| score > best_score) {
                best = Some((score, direction, overlap));
            }
        }
        let Some((_, direction, overlap)) = best else {
            return Step::Jumped;
        };
        self.apply(frame, direction, overlap)
    }
    /// Whether `frame` can be stitched onto this capture: same width, non-empty,
    /// dense BGRA8 rows of the declared height, and no size overflow.
    fn accepts(&self, frame: &Frame<'_>) -> bool {
        if frame.width != self.width || frame.height == 0 {
            return false;
        }
        let Some(stride) = row_stride(frame.width) else {
            return false;
        };
        frame.stride == stride
            && stride
                .checked_mul(frame.height as usize)
                .is_some_and(|len| frame.bytes.len() >= len)
    }
    /// The most recently accepted frame, read back out of the stitched image. Its
    /// page window always lies inside the covered range, so the rows are there.
    fn last_frame(&self) -> Frame<'_> {
        let start = (self.last_top - self.origin) as usize * self.stride;
        let end = start + self.last_height as usize * self.stride;
        Frame {
            width: self.width,
            height: self.last_height,
            stride: self.stride,
            bytes: &self.bytes[start..end],
        }
    }
    /// Best overlap for `frame` assuming the content moved `direction`, with its
    /// score. Only the last accepted frame is searched, which is what makes
    /// adjacent motion reliable: a constant wheel delta also scrolls by a nearly
    /// constant number of rows.
    fn candidate(&self, frame: &Frame<'_>, direction: Direction) -> Option<(f32, u32)> {
        let last = self.last_frame();
        match direction {
            Direction::Down => search_overlap(last, *frame, self.last_rows, |overlap| {
                self.last_height - overlap
            }),
            Direction::Up => search_overlap(*frame, last, self.last_rows, |overlap| {
                frame.height - overlap
            }),
        }
    }
    /// Moves an overlap found by [`Self::candidate`] onto the page and extends the
    /// image only where it does not cover rows yet. A frame that fits inside the
    /// covered range leaves the pixels alone but still becomes the last accepted
    /// frame, so the next scroll is located from where the user really is.
    fn apply(&mut self, frame: &Frame<'_>, direction: Direction, overlap: u32) -> Step {
        // Rows the content moved, which is also the offset from the last accepted
        // frame. `overlap` never exceeds either height, so this cannot wrap.
        let moved = match direction {
            Direction::Down => self.last_height - overlap,
            Direction::Up => frame.height - overlap,
        };
        let top = match direction {
            Direction::Down => self.last_top + i64::from(moved),
            Direction::Up => self.last_top - i64::from(moved),
        };
        let end = self.origin + i64::from(self.height);
        // Down starts the frame at or below the last frame's top, up ends it at or
        // above the last frame's end, so at most one side has uncovered rows.
        let prepend = (self.origin - top).max(0) as u32;
        let append = (top + i64::from(frame.height) - end).max(0) as u32;
        debug_assert!(direction == Direction::Up || prepend == 0);
        debug_assert!(direction == Direction::Down || append == 0);
        let rows = self.height as usize + prepend as usize + append as usize;
        if rows > MAX_HEIGHT as usize {
            return Step::Budget("已达最大高度");
        }
        if rows
            .checked_mul(self.stride)
            .is_none_or(|bytes| bytes > self.budget)
        {
            return Step::Budget("已达内存预算");
        }
        if prepend > 0 {
            let mut head = Vec::with_capacity(prepend as usize * self.stride);
            for row in 0..prepend as usize {
                head.extend_from_slice(&frame.bytes[row * frame.stride..][..self.stride]);
            }
            opaque(&mut head);
            self.bytes.splice(0..0, head);
            self.origin -= i64::from(prepend);
            self.height += prepend;
        }
        if append > 0 {
            let mut tail = Vec::with_capacity(append as usize * self.stride);
            for row in (frame.height - append) as usize..frame.height as usize {
                tail.extend_from_slice(&frame.bytes[row * frame.stride..][..self.stride]);
            }
            opaque(&mut tail);
            self.bytes.extend_from_slice(&tail);
            self.height += append;
        }
        self.last_top = top;
        self.last_height = frame.height;
        self.last_rows = Some(moved);
        debug_assert_eq!(self.bytes.len(), self.height as usize * self.stride);
        match direction {
            Direction::Down => {
                if append == 0 {
                    Step::Unchanged
                } else {
                    Step::Scrolled {
                        overlap: frame.height - append,
                        rows: append,
                    }
                }
            }
            Direction::Up => {
                if prepend == 0 {
                    Step::Unchanged
                } else {
                    Step::Prepended {
                        overlap: frame.height - prepend,
                        rows: prepend,
                    }
                }
            }
        }
    }
    /// The stitched image with `start_rows` dropped from the top and `end_rows`
    /// from the bottom, as the caller trims the last screen. This is a view over
    /// the capture, not a change to it, so trimming never disturbs the overlap
    /// search and a later scroll still stitches onto the same image.
    pub fn cropped(
        &self,
        start_rows: u32,
        end_rows: u32,
        id: ImageId,
    ) -> (ImageDescriptor, Vec<u8>) {
        let start = start_rows.min(self.height.saturating_sub(1));
        let end = end_rows.min(self.height.saturating_sub(1).saturating_sub(start));
        let rows = self.height - start - end;
        let mut bytes = Vec::with_capacity(rows as usize * self.stride);
        for row in 0..rows as usize {
            let at = (start as usize + row) * self.stride;
            bytes.extend_from_slice(&self.bytes[at..at + self.stride]);
        }
        (
            ImageDescriptor {
                id,
                width: self.width,
                height: rows,
                stride: self.stride as u32,
                format: PixelFormat::Bgra8,
                alpha: AlphaMode::Opaque,
                color_space: ColorSpace::Srgb,
            },
            bytes,
        )
    }
}

/// Dense BGRA8 stride of `width` pixels, or `None` when it does not fit `usize`.
fn row_stride(width: u32) -> Option<usize> {
    (width as usize).checked_mul(4)
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
    /// match at the right offset. The hash matters: a plain `y * k + x` pattern
    /// repeats every 256 rows, which lets a wrong overlap match perfectly.
    fn page(height: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; stride() * height as usize];
        for y in 0..height {
            for x in 0..WIDTH {
                let i = y as usize * stride() + x as usize * 4;
                let h = y.wrapping_mul(2_654_435_761) ^ x.wrapping_mul(40_503);
                bytes[i] = (h >> 24) as u8;
                bytes[i + 1] = (h >> 16) as u8;
                bytes[i + 2] = (h >> 8) as u8;
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

    #[test]
    fn scrolling_up_prepends_rows() {
        let source = page(1000);
        // Start in the middle of the page, then scroll back up: the new rows
        // belong *before* the stitched image.
        let mut capture =
            LongCapture::new(frame(&crop(&source, 300, FRAME_HEIGHT)), MAX_BYTES).unwrap();
        let next = crop(&source, 180, FRAME_HEIGHT);
        assert_eq!(
            capture.advance(&frame(&next), Some(Direction::Up)),
            Step::Prepended {
                overlap: 180,
                rows: 120
            }
        );
        assert_eq!(capture.height(), 420);
        assert_eq!(capture.bytes(), &source[180 * stride()..600 * stride()]);
    }

    #[test]
    fn advance_picks_the_direction_the_content_moved() {
        let source = page(1000);
        let mut capture =
            LongCapture::new(frame(&crop(&source, 300, FRAME_HEIGHT)), MAX_BYTES).unwrap();
        // No previous direction: a down-scroll frame is recognised as down.
        assert_eq!(
            capture.advance(&frame(&crop(&source, 420, FRAME_HEIGHT)), None),
            Step::Scrolled {
                overlap: 180,
                rows: 120
            }
        );
        // Even though the last move was down, an up-scroll frame still stitches,
        // because the preferred direction is only a preference.
        assert_eq!(
            capture.advance(
                &frame(&crop(&source, 240, FRAME_HEIGHT)),
                Some(Direction::Down)
            ),
            Step::Prepended {
                overlap: 240,
                rows: 60
            }
        );
        assert_eq!(capture.height(), 480);
        assert_eq!(capture.bytes(), &source[240 * stride()..720 * stride()]);
    }

    #[test]
    fn cropping_is_a_view_over_the_stitched_image() {
        let source = page(1000);
        let mut capture =
            LongCapture::new(frame(&crop(&source, 0, FRAME_HEIGHT)), MAX_BYTES).unwrap();
        for offset in [120u32, 240, 360, 480] {
            capture.push(&frame(&crop(&source, offset, FRAME_HEIGHT)));
        }
        let (image, bytes) = capture.cropped(30, 50, ImageId(7));
        assert_eq!(image.width, WIDTH);
        assert_eq!(image.height, 700);
        assert_eq!(image.stride, stride() as u32);
        assert_eq!(bytes, source[30 * stride()..730 * stride()]);
        // The capture itself is untouched, so the next scroll still stitches.
        assert_eq!(capture.height(), 780);
        assert_eq!(
            capture.push(&frame(&crop(&source, 600, FRAME_HEIGHT))),
            Step::Scrolled {
                overlap: 180,
                rows: 120
            }
        );
        // Over-trimming keeps one row rather than producing an empty image.
        let (image, bytes) = capture.cropped(10_000, 10_000, ImageId(8));
        assert_eq!(image.height, 1);
        assert_eq!(bytes.len(), stride());
    }

    #[test]
    fn scrolling_back_into_the_stitch_adds_no_rows() {
        let source = page(1000);
        let mut capture =
            LongCapture::new(frame(&crop(&source, 0, FRAME_HEIGHT)), MAX_BYTES).unwrap();
        for offset in [120u32, 240] {
            assert_eq!(
                capture.push(&frame(&crop(&source, offset, FRAME_HEIGHT))),
                Step::Scrolled {
                    overlap: 180,
                    rows: 120
                }
            );
        }
        let covered = capture.bytes().to_vec();
        // Scroll back up into rows the stitch already holds. The frame is located
        // from the last accepted one, so nothing is appended.
        assert_eq!(
            capture.advance(
                &frame(&crop(&source, 120, FRAME_HEIGHT)),
                Some(Direction::Up)
            ),
            Step::Unchanged
        );
        assert_eq!(capture.height(), 540);
        assert_eq!(capture.bytes(), &covered[..]);
        // Down again over the same rows: still no duplicate content.
        assert_eq!(
            capture.push(&frame(&crop(&source, 240, FRAME_HEIGHT))),
            Step::Unchanged
        );
        assert_eq!(capture.height(), 540);
        assert_eq!(capture.bytes(), &covered[..]);
        // Crossing the bottom end again adds exactly the rows it did not have.
        assert_eq!(
            capture.push(&frame(&crop(&source, 360, FRAME_HEIGHT))),
            Step::Scrolled {
                overlap: 180,
                rows: 120
            }
        );
        assert_eq!(capture.height(), 660);
        assert_eq!(capture.bytes(), &source[..660 * stride()]);
    }

    #[test]
    fn scrolling_up_back_through_the_start_adds_only_new_rows() {
        let source = page(1000);
        let mut capture =
            LongCapture::new(frame(&crop(&source, 300, FRAME_HEIGHT)), MAX_BYTES).unwrap();
        // Up past the first captured row: the new rows go before the image.
        assert_eq!(
            capture.advance(
                &frame(&crop(&source, 180, FRAME_HEIGHT)),
                Some(Direction::Up)
            ),
            Step::Prepended {
                overlap: 180,
                rows: 120
            }
        );
        // Back down into the covered rows: no duplicate content.
        assert_eq!(
            capture.advance(
                &frame(&crop(&source, 240, FRAME_HEIGHT)),
                Some(Direction::Down)
            ),
            Step::Unchanged
        );
        assert_eq!(capture.height(), 420);
        assert_eq!(capture.bytes(), &source[180 * stride()..600 * stride()]);
        // Up again: only the 60 rows above the covered top are new.
        assert_eq!(
            capture.advance(
                &frame(&crop(&source, 120, FRAME_HEIGHT)),
                Some(Direction::Up)
            ),
            Step::Prepended {
                overlap: 240,
                rows: 60
            }
        );
        assert_eq!(capture.height(), 480);
        assert_eq!(capture.bytes(), &source[120 * stride()..600 * stride()]);
    }

    #[test]
    fn the_stitch_grows_past_both_ends() {
        let source = page(1000);
        let mut capture =
            LongCapture::new(frame(&crop(&source, 300, FRAME_HEIGHT)), MAX_BYTES).unwrap();
        assert_eq!(
            capture.advance(
                &frame(&crop(&source, 180, FRAME_HEIGHT)),
                Some(Direction::Up)
            ),
            Step::Prepended {
                overlap: 180,
                rows: 120
            }
        );
        // Down through rows the stitch already holds...
        assert_eq!(
            capture.push(&frame(&crop(&source, 300, FRAME_HEIGHT))),
            Step::Unchanged
        );
        // ...and out the bottom, where each further screen adds its own rows.
        for offset in [420u32, 540] {
            assert_eq!(
                capture.push(&frame(&crop(&source, offset, FRAME_HEIGHT))),
                Step::Scrolled {
                    overlap: 180,
                    rows: 120
                }
            );
        }
        assert_eq!(capture.height(), 660);
        assert_eq!(capture.bytes(), &source[180 * stride()..840 * stride()]);
    }

    #[test]
    fn a_jump_leaves_the_last_frame_in_place() {
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
        assert_eq!(capture.bytes(), &source[..FRAME_HEIGHT as usize * stride()]);
        // The rejected frame did not move the anchor: the next screen of the same
        // page still stitches from the first one.
        assert_eq!(
            capture.push(&frame(&crop(&source, 120, FRAME_HEIGHT))),
            Step::Scrolled {
                overlap: 180,
                rows: 120
            }
        );
        assert_eq!(capture.height(), 420);
        assert_eq!(capture.bytes(), &source[..420 * stride()]);
    }

    #[test]
    fn a_budget_stop_keeps_the_stitch_usable() {
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
        assert_eq!(
            capture.push(&frame(&crop(&source, 240, FRAME_HEIGHT))),
            Step::Budget("已达内存预算")
        );
        // The refused screen was not partly appended.
        assert_eq!(capture.height(), 420);
        assert_eq!(capture.bytes(), &source[..420 * stride()]);
        // A rollback into covered rows still works and costs nothing.
        assert_eq!(
            capture.advance(&frame(&crop(&source, 0, FRAME_HEIGHT)), Some(Direction::Up)),
            Step::Unchanged
        );
        assert_eq!(capture.height(), 420);
        // Crossing the end again reports the same budget and changes nothing.
        assert_eq!(
            capture.push(&frame(&crop(&source, 240, FRAME_HEIGHT))),
            Step::Budget("已达内存预算")
        );
        assert_eq!(capture.height(), 420);
        assert_eq!(capture.bytes(), &source[..420 * stride()]);
    }

    #[test]
    fn malformed_frames_are_jumps_and_keep_the_stitch() {
        let source = page(1000);
        let mut capture =
            LongCapture::new(frame(&crop(&source, 0, FRAME_HEIGHT)), MAX_BYTES).unwrap();
        let next = crop(&source, 120, FRAME_HEIGHT);
        let short = &next[..next.len() - stride()];
        let cases = [
            Frame {
                width: 0,
                ..frame(&next)
            },
            Frame {
                width: WIDTH - 1,
                ..frame(&next)
            },
            Frame {
                height: 0,
                ..frame(&next)
            },
            // A row of padding would shift every row after the first.
            Frame {
                stride: stride() + 4,
                ..frame(&next)
            },
            // One row short of the declared height.
            Frame {
                bytes: short,
                ..frame(&next)
            },
        ];
        for case in cases {
            assert_eq!(capture.push(&case), Step::Jumped);
        }
        assert_eq!(capture.height(), FRAME_HEIGHT);
        assert_eq!(capture.bytes(), &source[..FRAME_HEIGHT as usize * stride()]);
        // The anchor survived every rejected frame.
        assert_eq!(
            capture.push(&frame(&next)),
            Step::Scrolled {
                overlap: 180,
                rows: 120
            }
        );
    }

    #[test]
    fn the_first_frame_is_capped_at_the_maximum_height() {
        let source = page(MAX_HEIGHT + 1);
        assert!(matches!(
            LongCapture::new(frame(&crop(&source, 0, MAX_HEIGHT + 1)), MAX_BYTES),
            Err("已达最大高度")
        ));
        let full = LongCapture::new(frame(&crop(&source, 0, MAX_HEIGHT)), MAX_BYTES).unwrap();
        assert_eq!(full.height(), MAX_HEIGHT);
    }
}

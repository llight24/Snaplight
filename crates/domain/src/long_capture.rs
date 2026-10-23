//! Long-capture stitching: merges successive scroll frames of one screen region
//! into a single tall image.
//!
//! The engine is pure: it works on BGRA8 buffers and never touches the platform,
//! so the overlap search, the budget rules, and the stop conditions are unit
//! tested without a screen.
//!
//! Frames are split into a fixed top band, a scrolling body and a fixed bottom
//! band ([`Bands`]). Sticky headers and footers never enter the overlap search,
//! and each is drawn exactly once: the top band at the start of the stitch and
//! the bottom band at its end. Without bands the whole frame is body, which is
//! what a page without fixed chrome looks like.
//!
//! The body is stored as one segment per accepted step instead of a single
//! growing buffer: appending a screen is one push, prepending one push-front,
//! and no step pays for a full-image reallocation or memmove. Only the public
//! byte view and the export assemble the segments.

use crate::{AlphaMode, ColorSpace, ImageDescriptor, ImageId, PixelFormat};
use std::collections::VecDeque;

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
/// Share of sampled rows that must match before a candidate is accepted at all.
/// Candidates between [`CANDIDATE_ROW_RATIO`] and this are ambiguous enough to
/// report back, but not to stitch: a wrong offset silently corrupts every row
/// below it, while a rejected frame costs one more scroll.
const MIN_ACCEPT_RATIO: f32 = 0.85;
/// Two candidates this close are indistinguishable. Without a previous scroll
/// delta to prefer one of them the frame is reported as a jump instead of being
/// stitched onto a guess.
const AMBIGUITY_GAP: f32 = 0.05;
/// Smallest accepted overlap. Anything below this is a jump, not a scroll: a
/// frame that shares fewer rows than this cannot be stitched without a visible
/// gap, so the caller stops instead of inventing content.
pub const MIN_OVERLAP: u32 = 8;
/// Weight of the "close to the previous scroll delta" bonus. A constant wheel
/// delta scrolls by a nearly constant number of rows, which disambiguates
/// repeating content (stripes, tables, code) that matches at several offsets.
const DELTA_WEIGHT: f32 = 0.5;
/// Bonus for the direction the content already moved in, so a close call keeps
/// scrolling the same way instead of flipping the stitched image around.
const KEEP_DIRECTION_BONUS: f32 = 0.1;
/// How many recent accepted frames are searched for an overlap. A PageDown or a
/// fast flick can move past the last frame while still sharing rows with one
/// taken a step earlier.
const ANCHORS: usize = 3;
/// Score penalty per anchor of age, so the newest frame wins an otherwise equal
/// candidate.
const ANCHOR_AGE_PENALTY: f32 = 0.02;
/// Width of the incremental bird's-eye thumbnail, in pixels. Its height follows
/// the stitch, so the panel never scales the full image.
const THUMB_WIDTH: u32 = 128;

/// Fixed rows at the top and the bottom of every frame that do not scroll with
/// the page: sticky headers, a fixed footer bar, browser chrome inside the
/// selection. They are matched and stitched around, never through.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bands {
    pub top: u32,
    pub bottom: u32,
}
impl Bands {
    pub const NONE: Self = Self { top: 0, bottom: 0 };
    /// Height of the scrolling body, or `None` when the bands leave no body or
    /// the frame is shorter than the bands.
    pub fn body_height(&self, frame_height: u32) -> Option<u32> {
        self.top
            .checked_add(self.bottom)
            .and_then(|fixed| frame_height.checked_sub(fixed))
            .filter(|body| *body > 0)
    }
    pub fn is_empty(&self) -> bool {
        self.top == 0 && self.bottom == 0
    }
}

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
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    /// `rows` new rows were appended after `overlap` rows of the previous image.
    Scrolled { overlap: u32, rows: u32 },
    /// `rows` new rows were inserted before `overlap` rows of the previous image,
    /// because the content scrolled up.
    Prepended { overlap: u32, rows: u32 },
    /// The frame repeats the previous image: the content did not move.
    Unchanged,
    /// The frame shares no usable overlap with the previous image, or the best
    /// candidate is not unique enough to stitch without risking a shifted image.
    Jumped,
    /// The best candidate is not strong enough to accept. The frame was not
    /// stitched, and the capture keeps waiting for a clearer one.
    LowConfidence { best: f32 },
    /// A cap stopped the capture; the stitched image so far stays usable.
    Budget(&'static str),
}

/// One anchor frame: its page position and body height. The pixels are read back
/// out of the stitch when an older anchor is searched.
#[derive(Clone, Copy)]
struct Anchor {
    top: i64,
    height: u32,
}
/// A candidate overlap found by [`search_overlap`].
#[derive(Clone, Copy)]
struct Match {
    /// Ratio of matching sampled rows, before any prior is applied.
    ratio: f32,
    /// `ratio` plus the previous-scroll bonus, used to pick a candidate.
    score: f32,
    /// Score of the next-best distinct overlap, when there is one.
    second: Option<f32>,
    overlap: u32,
    /// False when every sampled row was flat: the frame carries no offset at all.
    detailed: bool,
}
/// The winning candidate, with everything needed to place it on the page.
#[derive(Clone, Copy)]
struct Chosen {
    found: Match,
    /// `found.score` before the direction and recency preferences, for comparing
    /// against the runner-up.
    raw_score: f32,
    direction: Direction,
    anchor_top: i64,
    anchor_height: u32,
    moved: u32,
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
    .map(|found| found.overlap)
}

/// Shared search behind [`matching_overlap`], returning the best overlap together
/// with its score and the runner-up so a caller can reject an ambiguous match.
/// `delta` turns a candidate overlap into the number of new rows it would add,
/// which is what the previous step's delta is compared against; the two
/// directions count it differently.
fn search_overlap(
    previous: Frame<'_>,
    next: Frame<'_>,
    expected_rows: Option<u32>,
    delta: impl Fn(u32) -> u32,
) -> Option<Match> {
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
    let mut best: Option<(f32, f32, u32)> = None;
    let mut second: Option<f32> = None;
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
        match best {
            Some((best_score, _, _)) if score <= best_score => {
                if second.is_none_or(|runner_up| score > runner_up) {
                    second = Some(score);
                }
            }
            _ => {
                if let Some((best_score, _, _)) = best {
                    if second.is_none_or(|runner_up| best_score > runner_up) {
                        second = Some(best_score);
                    }
                }
                best = Some((score, ratio, overlap));
            }
        }
    }
    // A frame of one flat colour carries no offset at all. Reporting the largest
    // overlap leaves the caller's image untouched instead of guessing at rows.
    if !any_detail {
        return Some(Match {
            ratio: 0.0,
            score: 0.0,
            second: None,
            overlap: max,
            detailed: false,
        });
    }
    best.map(|(score, ratio, overlap)| Match {
        ratio,
        score,
        second,
        overlap,
        detailed: true,
    })
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
/// The engine works in *page* coordinates: row 0 of the body is page position
/// `origin`, which turns negative once the content scrolls up past the first
/// captured row, and the covered range is the body's own height. The most recent
/// accepted frames are remembered by page position, not by keeping every frame:
/// their pixels are the stitch rows that window covers, which is possible
/// because a frame window is always inside the covered range. The newest one is
/// cached instead, because it is searched on every step.
///
/// Every new frame is matched against those anchors, so a scroll that comes back
/// into rows the stitch already holds is located exactly instead of being
/// compared against the two ends of an ever-growing image. Only rows outside the
/// covered range extend the image; the rest is dropped. A frame inside the range
/// therefore returns [`Step::Unchanged`] while still becoming the anchor for the
/// next scroll, and crossing an end again adds exactly the new rows.
pub struct LongCapture {
    width: u32,
    height: u32,
    stride: usize,
    /// Body rows, one segment per accepted step.
    segments: VecDeque<Vec<u8>>,
    budget: usize,
    /// Page position of the body's first row.
    origin: i64,
    /// Recently accepted frames, oldest first. The last one's pixels are cached.
    anchors: VecDeque<Anchor>,
    last_pixels: Vec<u8>,
    /// Rows the content moved between the last two accepted frames, used as the
    /// expected scroll for the next one.
    last_rows: Option<u32>,
    bands: Bands,
    /// Fixed rows, captured once from the first frame / the latest accepted one.
    header: Vec<u8>,
    footer: Vec<u8>,
    /// Bird's-eye thumbnail of the body, scaled to [`THUMB_WIDTH`].
    thumb_width: u32,
    thumb: Vec<u8>,
    thumb_body_height: u32,
}
impl LongCapture {
    /// Starts a capture from the first frame. Fails when that frame alone does
    /// not fit the memory budget or already exceeds the height cap.
    pub fn new(frame: Frame<'_>, budget: usize) -> Result<Self, &'static str> {
        Self::new_banded(frame, Bands::NONE, budget)
    }
    /// Starts a capture with fixed rows at the top and the bottom of every
    /// frame. The bands are taken from the first frame and never change: a page
    /// that moves its chrome mid-capture is better served by a new session.
    pub fn new_banded(frame: Frame<'_>, bands: Bands, budget: usize) -> Result<Self, &'static str> {
        if frame.width == 0 || frame.height == 0 {
            return Err("抓取区域为空");
        }
        let Some(body_height) = bands.body_height(frame.height) else {
            return Err("吸顶/吸底区域占满了抓取区域");
        };
        if body_height < MIN_OVERLAP {
            return Err("抓取区域太小");
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
        let total_rows = bands
            .top
            .checked_add(body_height)
            .and_then(|rows| rows.checked_add(bands.bottom))
            .ok_or("抓取数据与区域尺寸不符")?;
        if total_rows > MAX_HEIGHT {
            return Err("已达最大高度");
        }
        let budget = budget.min(MAX_BYTES);
        let Some(len) = stride.checked_mul(total_rows as usize) else {
            return Err("拼接内存预算不足");
        };
        if len > budget {
            return Err("拼接内存预算不足");
        }
        let body = body_bytes(frame, bands, stride, body_height);
        let mut segment = Vec::with_capacity(body.len());
        segment.extend_from_slice(body);
        opaque(&mut segment);
        let header = band_bytes(&frame, 0, bands.top);
        let footer = band_bytes(&frame, frame.height - bands.bottom, bands.bottom);
        let mut capture = Self {
            width: frame.width,
            height: body_height,
            stride,
            segments: VecDeque::from([segment]),
            budget,
            origin: 0,
            anchors: VecDeque::from([Anchor {
                top: 0,
                height: body_height,
            }]),
            last_pixels: body.to_vec(),
            last_rows: None,
            bands,
            header,
            footer,
            thumb_width: THUMB_WIDTH.min(frame.width),
            thumb: Vec::new(),
            thumb_body_height: 0,
        };
        capture.rebuild_thumbnail();
        Ok(capture)
    }
    pub fn width(&self) -> u32 {
        self.width
    }
    /// Height of the scrolling body, without the fixed bands.
    pub fn height(&self) -> u32 {
        self.height
    }
    /// Height of the exported image: top band, body and bottom band.
    pub fn total_height(&self) -> u32 {
        self.bands.top + self.height + self.bands.bottom
    }
    pub fn stride(&self) -> usize {
        self.stride
    }
    pub fn bands(&self) -> Bands {
        self.bands
    }
    /// The stitched body, assembled from its segments. This copies the whole
    /// body; export and preview use [`Self::cropped`] and [`Self::preview`]
    /// instead so neither pays for a full second copy.
    #[cfg(test)]
    pub fn bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.height as usize * self.stride);
        for segment in &self.segments {
            bytes.extend_from_slice(segment);
        }
        bytes
    }
    /// Descriptor of the exported image, ready for `write_png` or a document.
    pub fn descriptor(&self, id: ImageId) -> ImageDescriptor {
        ImageDescriptor {
            id,
            width: self.width,
            height: self.total_height(),
            stride: self.stride as u32,
            format: PixelFormat::Bgra8,
            alpha: AlphaMode::Opaque,
            color_space: ColorSpace::Srgb,
        }
    }
    /// Width and height of the body thumbnail, with its rows top-down.
    pub fn thumbnail(&self) -> (u32, u32, &[u8]) {
        (
            self.thumb_width,
            self.thumb_body_height,
            &self.thumb[..self.thumb_body_height as usize * self.thumb_width as usize * 4],
        )
    }
    /// The bird's-eye preview: top band, trimmed body and bottom band, scaled to
    /// thumbnail width. Copies only the thumbnail, never the full image.
    pub fn preview(&self, trim_start: u32, trim_end: u32) -> (u32, u32, Vec<u8>) {
        let header_rows = self.scaled_rows(self.bands.top);
        let body_rows = self.thumb_body_height;
        let footer_rows = self.scaled_rows(self.bands.bottom);
        let (start, end) = self.thumbnail_trim(trim_start, trim_end);
        let rows = body_rows - start - end;
        let total = header_rows + rows + footer_rows;
        let row_bytes = self.thumb_width as usize * 4;
        let mut bytes = Vec::with_capacity(total as usize * row_bytes);
        let at = |row: u32| row as usize * row_bytes;
        bytes.extend_from_slice(&self.thumb[at(0)..at(header_rows)]);
        bytes.extend_from_slice(
            &self.thumb[at(header_rows + start)..at(header_rows + body_rows - end)],
        );
        bytes.extend_from_slice(
            &self.thumb[at(header_rows + body_rows)..at(header_rows + body_rows + footer_rows)],
        );
        (self.thumb_width, total, bytes)
    }
    /// Appends one frame captured while the content scrolled down.
    pub fn push(&mut self, frame: &Frame<'_>) -> Step {
        self.advance(frame, Some(Direction::Down))
    }
    /// Stitches one frame in whichever direction the content moved. Every recent
    /// anchor is searched in both directions and the better candidate wins, so a
    /// scroll that reverses direction or skips past the newest frame keeps
    /// working; `previous` breaks a close call in favour of the direction the
    /// user was already scrolling.
    pub fn advance(&mut self, frame: &Frame<'_>, previous: Option<Direction>) -> Step {
        let Some(body) = self.body_frame(frame) else {
            return Step::Jumped;
        };
        let anchors: Vec<Anchor> = self.anchors.iter().copied().collect();
        let mut best: Option<Chosen> = None;
        let mut flat = false;
        for (age, anchor) in anchors.iter().rev().enumerate() {
            let owned;
            let window = if age == 0 {
                Frame {
                    width: self.width,
                    height: anchor.height,
                    stride: self.stride,
                    bytes: &self.last_pixels,
                }
            } else {
                owned = self.window(anchor.top, anchor.height);
                Frame {
                    width: self.width,
                    height: anchor.height,
                    stride: self.stride,
                    bytes: &owned,
                }
            };
            for direction in [Direction::Down, Direction::Up] {
                let found = match direction {
                    Direction::Down => search_overlap(window, body, self.last_rows, |overlap| {
                        anchor.height - overlap
                    }),
                    Direction::Up => search_overlap(body, window, self.last_rows, |overlap| {
                        body.height - overlap
                    }),
                };
                let Some(found) = found else {
                    continue;
                };
                if !found.detailed {
                    flat = true;
                    continue;
                }
                let moved = match direction {
                    Direction::Down => anchor.height - found.overlap,
                    Direction::Up => body.height - found.overlap,
                };
                let raw_score = found.score;
                let score = raw_score
                    + if Some(direction) == previous {
                        KEEP_DIRECTION_BONUS
                    } else {
                        0.0
                    }
                    - age as f32 * ANCHOR_AGE_PENALTY;
                if best.is_none_or(|current| score > current.found.score + 1e-6) {
                    best = Some(Chosen {
                        found: Match { score, ..found },
                        raw_score,
                        direction,
                        anchor_top: anchor.top,
                        anchor_height: anchor.height,
                        moved,
                    });
                }
            }
        }
        let Some(chosen) = best else {
            if !flat {
                return Step::Jumped;
            }
            // Every sampled row was flat: there is no offset, so the frame is
            // treated as the same position instead of guessing at rows.
            let anchor = *self.anchors.back().expect("a capture always has an anchor");
            return self.apply(
                frame,
                body,
                Direction::Down,
                anchor.height,
                anchor.top,
                anchor.height,
            );
        };
        if chosen.found.ratio < MIN_ACCEPT_RATIO {
            return Step::LowConfidence {
                best: chosen.found.ratio,
            };
        }
        if chosen.found.second.is_some_and(|second| {
            chosen.raw_score - second < AMBIGUITY_GAP && self.moved_is_unexpected(chosen.moved)
        }) {
            return Step::Jumped;
        }
        self.apply(
            frame,
            body,
            chosen.direction,
            chosen.found.overlap,
            chosen.anchor_top,
            chosen.anchor_height,
        )
    }
    /// The body of `frame`, or `None` when it cannot be stitched: same width,
    /// non-empty, dense BGRA8 rows of the declared height, and no size overflow.
    fn body_frame<'a>(&self, frame: &'a Frame<'_>) -> Option<Frame<'a>> {
        if frame.width != self.width {
            return None;
        }
        let stride = row_stride(frame.width)?;
        if frame.stride != stride {
            return None;
        }
        let height = self.bands.body_height(frame.height)?;
        if height < MIN_OVERLAP {
            return None;
        }
        let len = stride.checked_mul(frame.height as usize)?;
        if frame.bytes.len() < len {
            return None;
        }
        let start = self.bands.top as usize * stride;
        let end = start + height as usize * stride;
        Some(Frame {
            width: frame.width,
            height,
            stride,
            bytes: &frame.bytes[start..end],
        })
    }
    /// Whether the chosen movement is far from the previous scroll delta. With
    /// no previous step nothing can be preferred, so a near-tie is a jump.
    fn moved_is_unexpected(&self, moved: u32) -> bool {
        match self.last_rows {
            Some(expected) if expected > 0 => moved.abs_diff(expected) > expected / 2,
            _ => true,
        }
    }
    /// Copies a window of body rows out of the stitch, for older anchors.
    fn window(&self, top: i64, height: u32) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(height as usize * self.stride);
        self.copy_rows(&mut bytes, (top - self.origin) as u32, height);
        bytes
    }
    /// Appends `rows` body rows starting at `start` to `out`.
    fn copy_rows(&self, out: &mut Vec<u8>, start: u32, rows: u32) {
        let mut skip = start as usize;
        let mut left = rows as usize;
        for segment in &self.segments {
            let segment_rows = segment.len() / self.stride;
            if skip >= segment_rows {
                skip -= segment_rows;
                continue;
            }
            let take = left.min(segment_rows - skip);
            out.extend_from_slice(&segment[skip * self.stride..][..take * self.stride]);
            left -= take;
            skip = 0;
            if left == 0 {
                break;
            }
        }
        debug_assert_eq!(left, 0, "window outside the stitched body");
    }
    /// Moves an overlap found by [`Self::advance`] onto the page and extends the
    /// image only where it does not cover rows yet. A frame that fits inside the
    /// covered range leaves the pixels alone but still becomes the newest
    /// anchor, so the next scroll is located from where the user really is.
    fn apply(
        &mut self,
        frame: &Frame<'_>,
        body: Frame<'_>,
        direction: Direction,
        overlap: u32,
        anchor_top: i64,
        anchor_height: u32,
    ) -> Step {
        // Rows the content moved, which is also the offset from the anchor.
        let moved = match direction {
            Direction::Down => anchor_height - overlap,
            Direction::Up => body.height - overlap,
        };
        let top = match direction {
            Direction::Down => anchor_top + i64::from(moved),
            Direction::Up => anchor_top - i64::from(moved),
        };
        let end = self.origin + i64::from(self.height);
        // Down starts the frame at or below the anchor's top, up ends it at or
        // above the anchor's end, so at most one side has uncovered rows.
        let prepend = (self.origin - top).max(0) as u32;
        let append = (top + i64::from(body.height) - end).max(0) as u32;
        debug_assert!(direction == Direction::Up || prepend == 0);
        debug_assert!(direction == Direction::Down || append == 0);
        let rows = self.height as usize + prepend as usize + append as usize;
        let total_rows = self.bands.top as usize + rows + self.bands.bottom as usize;
        if total_rows > MAX_HEIGHT as usize {
            return Step::Budget("已达最大高度");
        }
        if total_rows
            .checked_mul(self.stride)
            .is_none_or(|bytes| bytes > self.budget)
        {
            return Step::Budget("已达内存预算");
        }
        let old_height = self.height;
        if prepend > 0 {
            let mut head = Vec::with_capacity(prepend as usize * self.stride);
            head.extend_from_slice(&body.bytes[..prepend as usize * self.stride]);
            opaque(&mut head);
            self.segments.push_front(head);
            self.origin -= i64::from(prepend);
            self.height += prepend;
            self.rebuild_thumbnail();
        }
        if append > 0 {
            let mut tail = Vec::with_capacity(append as usize * self.stride);
            let start = (body.height - append) as usize * self.stride;
            tail.extend_from_slice(&body.bytes[start..]);
            opaque(&mut tail);
            self.segments.push_back(tail);
            self.height += append;
            self.append_thumbnail(old_height, append);
        }
        // The newest frame is the anchor for the next step, including one that
        // landed fully inside the covered range.
        let mut last = Vec::with_capacity(body.bytes.len());
        last.extend_from_slice(body.bytes);
        self.last_pixels = last;
        self.footer = band_bytes(frame, frame.height - self.bands.bottom, self.bands.bottom);
        self.anchors.push_back(Anchor {
            top,
            height: body.height,
        });
        while self.anchors.len() > ANCHORS {
            self.anchors.pop_front();
        }
        // A still frame must not erase the last real scroll delta: the next
        // wheel notch is still expected to move by that much.
        if moved > 0 {
            self.last_rows = Some(moved);
        }
        debug_assert_eq!(self.body_len(), self.height as usize * self.stride);
        match direction {
            Direction::Down => {
                if append == 0 {
                    Step::Unchanged
                } else {
                    Step::Scrolled {
                        overlap: body.height - append,
                        rows: append,
                    }
                }
            }
            Direction::Up => {
                if prepend == 0 {
                    Step::Unchanged
                } else {
                    Step::Prepended {
                        overlap: body.height - prepend,
                        rows: prepend,
                    }
                }
            }
        }
    }
    fn body_len(&self) -> usize {
        self.segments.iter().map(Vec::len).sum()
    }
    /// The stitched image with `start_rows` dropped from the top and `end_rows`
    /// from the bottom of the *body*, with each fixed band drawn once. This is a
    /// view over the capture, not a change to it, so trimming never disturbs the
    /// overlap search and a later scroll still stitches onto the same image.
    pub fn cropped(
        &self,
        start_rows: u32,
        end_rows: u32,
        id: ImageId,
    ) -> (ImageDescriptor, Vec<u8>) {
        let (start, end) = self.body_trim(start_rows, end_rows);
        let rows = self.height - start - end;
        let total = self.bands.top + rows + self.bands.bottom;
        let mut bytes = Vec::with_capacity(total as usize * self.stride);
        bytes.extend_from_slice(&self.header);
        self.copy_rows(&mut bytes, start, rows);
        bytes.extend_from_slice(&self.footer);
        (
            ImageDescriptor {
                id,
                width: self.width,
                height: total,
                stride: self.stride as u32,
                format: PixelFormat::Bgra8,
                alpha: AlphaMode::Opaque,
                color_space: ColorSpace::Srgb,
            },
            bytes,
        )
    }
    /// Body rows removed at each end, never trimming away the whole body.
    fn body_trim(&self, start_rows: u32, end_rows: u32) -> (u32, u32) {
        let start = start_rows.min(self.height.saturating_sub(1));
        let end = end_rows.min(self.height.saturating_sub(1).saturating_sub(start));
        (start, end)
    }
    /// Body rows removed at each end, expressed in thumbnail rows.
    fn thumbnail_trim(&self, start_rows: u32, end_rows: u32) -> (u32, u32) {
        let (start, end) = self.body_trim(start_rows, end_rows);
        let rows = self.thumb_body_height;
        let scale = |value: u32| {
            if self.height == 0 {
                0
            } else {
                (u64::from(value) * u64::from(rows) / u64::from(self.height)) as u32
            }
        };
        let start = scale(start);
        let end = scale(end).min(rows.saturating_sub(1).saturating_sub(start));
        (start, end)
    }
    /// Thumbnail rows for a fixed band: same horizontal scale as the body.
    fn scaled_rows(&self, rows: u32) -> u32 {
        if rows == 0 || self.width == 0 {
            0
        } else {
            ((u64::from(rows) * u64::from(self.thumb_width) + u64::from(self.width) - 1)
                / u64::from(self.width))
            .max(1) as u32
        }
    }
    /// Thumbnail rows for a body height.
    fn scaled_body_rows(&self, rows: u32) -> u32 {
        self.scaled_rows(rows)
    }
    /// Rebuilds the whole thumbnail. Used when rows are prepended, where every
    /// destination row shifts, and when the capture starts; appends are
    /// incremental.
    fn rebuild_thumbnail(&mut self) {
        let header_rows = self.scaled_rows(self.bands.top);
        let body_rows = self.scaled_body_rows(self.height);
        let footer_rows = self.scaled_rows(self.bands.bottom);
        let row_bytes = self.thumb_width as usize * 4;
        let mut thumb =
            Vec::with_capacity((header_rows + body_rows + footer_rows) as usize * row_bytes);
        scale_block(
            &mut thumb,
            &self.header,
            self.bands.top,
            header_rows,
            self.width,
            self.thumb_width,
            self.stride,
        );
        let mut line = vec![0u8; row_bytes];
        for row in 0..body_rows {
            let source = if body_rows == 0 {
                0
            } else {
                (u64::from(row) * u64::from(self.height) / u64::from(body_rows)) as u32
            };
            self.scale_line(source as usize, row_bytes, &mut line);
            thumb.extend_from_slice(&line);
        }
        scale_block(
            &mut thumb,
            &self.footer,
            self.bands.bottom,
            footer_rows,
            self.width,
            self.thumb_width,
            self.stride,
        );
        self.thumb = thumb;
        self.thumb_body_height = body_rows;
    }
    /// Appends the rows of `segment` that became visible at `old_height`.
    fn append_thumbnail(&mut self, old_height: u32, added: u32) {
        let header_rows = self.scaled_rows(self.bands.top);
        let old_body_rows = self.scaled_body_rows(old_height);
        let new_body_rows = self.scaled_body_rows(old_height + added);
        if new_body_rows <= old_body_rows {
            self.thumb_body_height = new_body_rows;
            return;
        }
        let row_bytes = self.thumb_width as usize * 4;
        let mut rows = Vec::with_capacity((new_body_rows - old_body_rows) as usize * row_bytes);
        for row in old_body_rows..new_body_rows {
            let source =
                (u64::from(row) * u64::from(old_height + added) / u64::from(new_body_rows)) as u32;
            if source < old_height {
                let known = (u64::from(source) * u64::from(old_body_rows)
                    / u64::from(old_height.max(1))) as u32;
                let at =
                    (header_rows + known.min(old_body_rows.saturating_sub(1))) as usize * row_bytes;
                rows.extend_from_slice(&self.thumb[at..at + row_bytes]);
            } else {
                let mut line = vec![0u8; row_bytes];
                self.scale_line(source as usize, row_bytes, &mut line);
                rows.extend_from_slice(&line);
            }
        }
        let at = (header_rows + old_body_rows) as usize * row_bytes;
        self.thumb.splice(at..at, rows);
        self.thumb_body_height = new_body_rows;
    }
    /// Scales one body row into `out`.
    fn scale_line(&self, source: usize, row_bytes: usize, out: &mut [u8]) {
        let mut segment = None;
        let mut row = source;
        for candidate in &self.segments {
            let rows = candidate.len() / self.stride;
            if row < rows {
                segment = Some((candidate, row));
                break;
            }
            row -= rows;
        }
        let Some((segment, row)) = segment else {
            out.fill(0);
            return;
        };
        let at = row * self.stride;
        let line = &segment[at..at + self.stride];
        for x in 0..self.thumb_width as usize {
            let source_x = (x * self.width as usize / self.thumb_width as usize)
                .min(self.width.saturating_sub(1) as usize);
            let to = x * 4;
            out[to..to + 4].copy_from_slice(&line[source_x * 4..source_x * 4 + 4]);
            out[to + 3] = 255;
        }
        debug_assert_eq!(out.len(), row_bytes);
    }
}

/// Scales `dst_rows` thumbnail rows out of a contiguous band.
fn scale_block(
    out: &mut Vec<u8>,
    band: &[u8],
    source_rows: u32,
    dst_rows: u32,
    width: u32,
    thumb_width: u32,
    stride: usize,
) {
    if source_rows == 0 || dst_rows == 0 {
        return;
    }
    let mut line = vec![0u8; thumb_width as usize * 4];
    for row in 0..dst_rows {
        let source = (u64::from(row) * u64::from(source_rows) / u64::from(dst_rows))
            .min(u64::from(source_rows - 1)) as usize;
        let at = source * stride;
        let source_line = &band[at..at + stride];
        for x in 0..thumb_width as usize {
            let source_x =
                (x * width as usize / thumb_width as usize).min(width.saturating_sub(1) as usize);
            line[x * 4..x * 4 + 4].copy_from_slice(&source_line[source_x * 4..source_x * 4 + 4]);
            line[x * 4 + 3] = 255;
        }
        out.extend_from_slice(&line);
    }
}

/// The body rows of `frame`, as one contiguous slice.
fn body_bytes<'a>(frame: Frame<'a>, bands: Bands, stride: usize, height: u32) -> &'a [u8] {
    let start = bands.top as usize * stride;
    &frame.bytes[start..start + height as usize * stride]
}

/// `rows` rows of a frame starting at `row`, with alpha forced opaque.
fn band_bytes(frame: &Frame<'_>, row: u32, rows: u32) -> Vec<u8> {
    if rows == 0 {
        return Vec::new();
    }
    let stride = frame.stride;
    let mut bytes = frame.bytes[row as usize * stride..][..rows as usize * stride].to_vec();
    opaque(&mut bytes);
    bytes
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
        page_with(height, 0)
    }
    /// A page whose rows repeat every `period` rows, like stripes or a table.
    fn page_with(height: u32, period: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; stride() * height as usize];
        for y in 0..height {
            let row = if period == 0 { y } else { y % period };
            for x in 0..WIDTH {
                let i = y as usize * stride() + x as usize * 4;
                let h = row.wrapping_mul(2_654_435_761) ^ x.wrapping_mul(40_503);
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
        // Sticky headers and browser chrome do not scroll with the page. With
        // the band declared, the body matches perfectly and nothing is repeated.
        let source = page(1000);
        let bands = Bands { top: 40, bottom: 0 };
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
        let mut capture = LongCapture::new_banded(frame(&first), bands, MAX_BYTES).unwrap();
        for offset in [120u32, 240] {
            assert_eq!(
                capture.push(&frame(&banded(offset))),
                Step::Scrolled {
                    overlap: 260 - 120,
                    rows: 120
                }
            );
        }
        assert_eq!(capture.height(), 260 + 240);
        assert_eq!(capture.total_height(), 40 + 260 + 240);
    }

    #[test]
    fn a_sticky_header_and_footer_are_drawn_once() {
        // The body scrolls by 90 rows per screen; the 30-row header and 20-row
        // footer stay put. Every body row must appear exactly once, and the fixed
        // rows exactly once at the two ends.
        const TOP: u32 = 30;
        const BOTTOM: u32 = 20;
        let view = |scroll: u32| {
            let mut bytes = vec![0u8; FRAME_HEIGHT as usize * stride()];
            for y in 0..FRAME_HEIGHT {
                let (page_row, kind) = if y < TOP {
                    (y, 0)
                } else if y >= FRAME_HEIGHT - BOTTOM {
                    (y - (FRAME_HEIGHT - BOTTOM), 1)
                } else {
                    (scroll + y - TOP, 2)
                };
                let colour = match kind {
                    0 => [0x20u8, (page_row * 3) as u8, 0x60, 255],
                    1 => [0x40u8, 0x70, (page_row * 5) as u8, 255],
                    _ => [
                        (page_row % 251) as u8,
                        (page_row * 7 % 241) as u8,
                        (page_row * 13 % 239) as u8,
                        255,
                    ],
                };
                for x in 0..WIDTH as usize {
                    let at = y as usize * stride() + x * 4;
                    bytes[at..at + 4].copy_from_slice(&colour);
                }
            }
            bytes
        };
        let body = FRAME_HEIGHT - TOP - BOTTOM;
        let mut capture = LongCapture::new_banded(
            frame(&view(0)),
            Bands {
                top: TOP,
                bottom: BOTTOM,
            },
            MAX_BYTES,
        )
        .unwrap();
        for scroll in [90u32, 180] {
            assert_eq!(
                capture.push(&frame(&view(scroll))),
                Step::Scrolled {
                    overlap: body - 90,
                    rows: 90
                }
            );
        }
        let (image, bytes) = capture.cropped(0, 0, ImageId(1));
        assert_eq!(image.height, TOP + body + 180 + BOTTOM);
        let colour_at = |row: u32| &bytes[row as usize * stride() + 32 * 4..][..4];
        for row in 0..TOP {
            assert_eq!(
                colour_at(row),
                &[0x20, (row * 3) as u8, 0x60, 255],
                "header row {row}"
            );
        }
        for row in 0..body + 180 {
            assert_eq!(
                colour_at(TOP + row),
                &[
                    (row % 251) as u8,
                    (row * 7 % 241) as u8,
                    (row * 13 % 239) as u8,
                    255
                ],
                "body row {row}"
            );
        }
        for row in 0..BOTTOM {
            assert_eq!(
                colour_at(TOP + body + 180 + row),
                &[0x40, 0x70, (row * 5) as u8, 255],
                "footer row {row}"
            );
        }
    }

    #[test]
    fn an_ambiguous_repeat_is_a_jump() {
        // Every 100 rows look the same, so the first scroll has several perfect
        // candidates and no previous delta to prefer one. Stitching either would
        // be a guess, so the frame is refused.
        let source = page_with(1000, 100);
        let first = crop(&source, 0, FRAME_HEIGHT);
        let next = crop(&source, 100, FRAME_HEIGHT);
        let mut capture = LongCapture::new(frame(&first), MAX_BYTES).unwrap();
        assert_eq!(capture.push(&frame(&next)), Step::Jumped);
        assert_eq!(capture.height(), FRAME_HEIGHT);
    }

    #[test]
    fn a_low_confidence_match_is_not_stitched() {
        // A frame that only partially matches: above the candidate floor but
        // below the acceptance line. The capture keeps its old anchor.
        let source = page(1000);
        let first = crop(&source, 0, FRAME_HEIGHT);
        let mut capture = LongCapture::new(frame(&first), MAX_BYTES).unwrap();
        let mut next = crop(&source, 120, FRAME_HEIGHT);
        // Corrupt 45 rows of the overlapping region: 75% of the overlap matches.
        for row in 0..45usize {
            for x in 0..WIDTH as usize {
                let at = row * stride() + x * 4;
                next[at] = next[at].wrapping_add(0x80);
                next[at + 1] = next[at + 1].wrapping_add(0x80);
                next[at + 2] = next[at + 2].wrapping_add(0x80);
            }
        }
        match capture.push(&frame(&next)) {
            Step::LowConfidence { best } => assert!(best < MIN_ACCEPT_RATIO, "{best}"),
            other => panic!("expected LowConfidence, got {other:?}"),
        }
        assert_eq!(capture.height(), FRAME_HEIGHT);
    }

    #[test]
    fn an_older_anchor_stitches_after_a_large_scroll_back() {
        // Start in the middle, scroll down three steps, then jump up far enough
        // that the newest anchor shares no rows. The frame lands before the
        // covered top, so the rows above it are new and only an older anchor can
        // place it.
        let source = page(2000);
        let mut capture =
            LongCapture::new(frame(&crop(&source, 300, FRAME_HEIGHT)), MAX_BYTES).unwrap();
        for offset in [420u32, 540, 660] {
            assert!(matches!(
                capture.push(&frame(&crop(&source, offset, FRAME_HEIGHT))),
                Step::Scrolled { rows: 120, .. }
            ));
        }
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
        assert_eq!(capture.height(), FRAME_HEIGHT + 360 + 60);
        assert_eq!(
            capture.bytes(),
            &source[240 * stride()..(240 + FRAME_HEIGHT + 420) as usize * stride()]
        );
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
        let covered = capture.bytes();
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

    #[test]
    fn the_thumbnail_grows_with_the_stitch_without_repeating_rows() {
        let source = page(1000);
        let mut capture =
            LongCapture::new(frame(&crop(&source, 0, FRAME_HEIGHT)), MAX_BYTES).unwrap();
        let (_, first_rows, _) = capture.thumbnail();
        assert!(first_rows > 0);
        for offset in [120u32, 240, 360] {
            capture.push(&frame(&crop(&source, offset, FRAME_HEIGHT)));
        }
        let (width, rows, bytes) = capture.thumbnail();
        assert_eq!(width, WIDTH.min(THUMB_WIDTH));
        assert_eq!(rows, capture.scaled_body_rows(capture.height()));
        assert_eq!(bytes.len(), rows as usize * width as usize * 4);
        // The preview is the same thumbnail when nothing is trimmed.
        let (preview_width, preview_rows, preview) = capture.preview(0, 0);
        assert_eq!((preview_width, preview_rows), (width, rows));
        assert_eq!(preview, bytes);
        // A trim shortens the preview but never drops the fixed bands.
        let bands = Bands {
            top: 30,
            bottom: 20,
        };
        let banded_view = |scroll: u32| {
            let mut bytes = crop(&source, scroll, FRAME_HEIGHT);
            bytes[..30 * stride()].fill(0x30);
            bytes[(FRAME_HEIGHT - 20) as usize * stride()..].fill(0x70);
            bytes
        };
        let mut banded = LongCapture::new_banded(frame(&banded_view(0)), bands, MAX_BYTES).unwrap();
        banded.push(&frame(&banded_view(120)));
        let (_, full_rows, _) = banded.preview(0, 0);
        let (_, trimmed_rows, _) = banded.preview(100, 0);
        assert!(trimmed_rows < full_rows);
    }
}

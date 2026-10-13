//! Pinned image editing: re-opening the file group the pin process reads, so a
//! second annotation pass stays non-destructive and its result is handed back.

use snaplight_domain::{
    Annotation, Document, ImageDescriptor, ImagePoint, ImageRect, PixelFormat, Shape,
};
use snaplight_platform::{atomic_write, read_pixels, write_pixels};
use std::path::PathBuf;
use windows::Win32::Foundation::RECT;

/// Parses `x,y,w,h`, in physical screen pixels.
fn parse_rect(value: &str) -> Option<RECT> {
    let numbers: Vec<i32> = value
        .split(',')
        .map(|part| part.trim().parse().ok())
        .collect::<Option<_>>()?;
    match numbers[..] {
        [x, y, w, h] if w > 0 && h > 0 => Some(RECT {
            left: x,
            top: y,
            right: x + w,
            bottom: y + h,
        }),
        _ => None,
    }
}
/// Integer image pixels a selection is cropped to, rounded the way the
/// renderer's export rounds: floor the origin, ceil the far edge. `None` when
/// the rectangle is not finite or does not cover at least one pixel.
fn crop_bounds(selection: ImageRect, width: u32, height: u32) -> Option<(u32, u32, u32, u32)> {
    // The width and height are checked before rounding: a fractional origin must
    // not turn a zero or negative selection into one pixel of crop.
    if ![selection.x, selection.y, selection.width, selection.height]
        .iter()
        .all(|value| value.is_finite())
        || selection.width <= 0.0
        || selection.height <= 0.0
    {
        return None;
    }
    // `clamp` can still collapse the area to nothing once it is intersected with
    // the image, so an empty result is rejected before the integer conversion.
    let area = selection.clamp(width, height);
    if area.width <= 0.0 || area.height <= 0.0 {
        return None;
    }
    // Floor the origin, ceil the far edge, matching the renderer's export.
    let (x, y) = (area.x.floor(), area.y.floor());
    let right = (area.x + area.width).ceil();
    let bottom = (area.y + area.height).ceil();
    if x < 0.0 || y < 0.0 {
        return None;
    }
    // The clamped edges are within the image, but `f32` cannot represent every
    // `u32`, so the far edges are pinned to the real pixel size instead of
    // trusting `as u32` on a rounded float.
    let x = x as u32;
    let y = y as u32;
    let right = (right as u32).min(width);
    let bottom = (bottom as u32).min(height);
    let w = right.checked_sub(x)?;
    let h = bottom.checked_sub(y)?;
    if w == 0
        || h == 0
        || x.checked_add(w).is_none_or(|edge| edge > width)
        || y.checked_add(h).is_none_or(|edge| edge > height)
    {
        return None;
    }
    Some((x, y, w, h))
}
/// Copies a rectangle out of a BGRA8 image into a compact buffer with no row
/// padding. The source stride may carry padding, so every row is copied on its
/// own instead of the whole block moving at once.
fn crop_image(
    image: &ImageDescriptor,
    bytes: &[u8],
    area: (u32, u32, u32, u32),
) -> std::result::Result<(ImageDescriptor, Vec<u8>), Box<dyn std::error::Error>> {
    if !matches!(image.format, PixelFormat::Bgra8) {
        return Err("a pinned source must be BGRA8".into());
    }
    let len = image.byte_len().map_err(|e| format!("pinned source {e}"))?;
    if bytes.len() != len {
        return Err("pinned source pixel buffer length mismatch".into());
    }
    let (x, y, w, h) = area;
    if w == 0 || h == 0 {
        return Err("pinned crop area is empty".into());
    }
    // The area is validated in integer pixels before any slice arithmetic: an
    // overflow or an area past the edge is refused, never read out of range.
    if x.checked_add(w).is_none_or(|edge| edge > image.width)
        || y.checked_add(h).is_none_or(|edge| edge > image.height)
    {
        return Err("pinned crop area is outside the image".into());
    }
    let row = (w as usize)
        .checked_mul(4)
        .ok_or("pinned crop row overflow")?;
    let row_u32 = u32::try_from(row).map_err(|_| "pinned crop row overflow")?;
    let out_len = row
        .checked_mul(h as usize)
        .ok_or("pinned crop size overflow")?;
    let stride = image.stride as usize;
    let mut cropped = vec![0u8; out_len];
    for line in 0..h as usize {
        let start = (y as usize + line)
            .checked_mul(stride)
            .and_then(|n| n.checked_add((x as usize).checked_mul(4)?))
            .ok_or("pinned crop offset overflow")?;
        let end = start
            .checked_add(row)
            .ok_or("pinned crop offset overflow")?;
        let source = bytes
            .get(start..end)
            .ok_or("pinned crop reads outside the source")?;
        cropped[line * row..(line + 1) * row].copy_from_slice(source);
    }
    Ok((
        ImageDescriptor {
            id: image.id,
            width: w,
            height: h,
            stride: row_u32,
            format: image.format,
            alpha: image.alpha,
            color_space: image.color_space,
        },
        cropped,
    ))
}
/// Moves one shape by the crop origin, so an annotation stays on the pixels it
/// was drawn at after the image is cropped to a selection.
fn shift_shape(shape: &Shape, dx: f32, dy: f32) -> Shape {
    let point = |p: ImagePoint| ImagePoint {
        x: p.x - dx,
        y: p.y - dy,
    };
    let rect = |r: ImageRect| ImageRect {
        x: r.x - dx,
        y: r.y - dy,
        ..r
    };
    match shape {
        Shape::Rectangle(r) => Shape::Rectangle(rect(*r)),
        Shape::Ellipse(r) => Shape::Ellipse(rect(*r)),
        Shape::Arrow { from, to } => Shape::Arrow {
            from: point(*from),
            to: point(*to),
        },
        Shape::Pen(points) => Shape::Pen(points.iter().map(|p| point(*p)).collect()),
        Shape::Mosaic { rect: r, block } => Shape::Mosaic {
            rect: rect(*r),
            block: *block,
        },
        Shape::Text { at, text, size } => Shape::Text {
            at: point(*at),
            text: text.clone(),
            size: *size,
        },
    }
}
/// A pinned image re-opened for annotation, from `--edit-pin=<image path>`.
///
/// The image belongs to the file group the editor writes when a capture is
/// pinned: `<stem>.bgra` is the flattened picture the pin shows,
/// `<stem>.document.json` keeps the annotations and `<stem>.source.bgra` the
/// pixels the capture started from. Editing the document instead of the
/// flattened picture is what makes a second annotation pass non-destructive;
/// the picture is the fallback for a pin that was saved without them.
///
/// `--pin-rect=x,y,w,h` is the pin's window on screen, so the session opens over
/// the pin instead of covering the desktop, and `--edit-pin-demo=<image path>`
/// runs the same session without a user: it annotates one scripted rectangle and
/// writes the result back, so the round trip can be verified.
pub(super) struct PinEdit {
    /// Window rectangle of the pin, in screen pixels.
    rect: RECT,
    /// The pin's files without their extensions.
    stem: PathBuf,
    /// `--edit-pin-demo`: no UI, scripted annotation and write-back.
    demo: bool,
}
impl PinEdit {
    pub(super) fn from_args() -> std::result::Result<Option<Self>, Box<dyn std::error::Error>> {
        let mut image = None;
        let mut demo = false;
        let mut rect = None;
        for arg in std::env::args() {
            if let Some(value) = arg.strip_prefix("--edit-pin=") {
                image = Some(value.to_owned());
                demo = false;
            } else if let Some(value) = arg.strip_prefix("--edit-pin-demo=") {
                image = Some(value.to_owned());
                demo = true;
            } else if let Some(value) = arg.strip_prefix("--pin-rect=") {
                rect = parse_rect(value);
            }
        }
        let Some(image) = image else {
            return Ok(None);
        };
        let rect = rect.ok_or("--edit-pin needs --pin-rect=x,y,w,h to know where the pin is")?;
        Ok(Some(Self {
            rect,
            stem: PathBuf::from(image).with_extension(""),
            demo,
        }))
    }
    /// Window rectangle of the pin, in screen pixels.
    pub(super) fn screen_rect(&self) -> RECT {
        self.rect
    }
    /// `--edit-pin-demo`: no UI, scripted annotation and write-back.
    pub(super) fn is_demo(&self) -> bool {
        self.demo
    }
    /// The picture the pin shows: what the session writes back.
    pub(super) fn image_path(&self) -> PathBuf {
        self.stem.with_extension("bgra")
    }
    /// The document with the annotations, so a later edit still has them.
    pub(super) fn document_path(&self) -> PathBuf {
        self.stem.with_extension("document.json")
    }
    /// The flattened document the pin process reads when it opens a pin.
    fn pin_document_path(&self) -> PathBuf {
        self.stem.with_extension("pin.json")
    }
    /// The pixels the capture started from, kept so annotations stay editable.
    fn source_path(&self) -> PathBuf {
        self.stem.with_extension("source.bgra")
    }
    /// Loads the pixels and the document to annotate.
    pub(super) fn load(
        &self,
    ) -> std::result::Result<(ImageDescriptor, Vec<u8>, Document), Box<dyn std::error::Error>> {
        let (image, base) =
            read_pixels(&self.source_path()).or_else(|_| read_pixels(&self.image_path()))?;
        let document = match std::fs::read(self.document_path()) {
            Ok(bytes) => serde_json::from_slice::<Document>(&bytes)?,
            // A pin saved without a document still opens: it is a picture, with
            // no annotations to restore.
            Err(_) => Document {
                version: 1,
                image: image.clone(),
                selection: None,
                annotations: vec![],
            },
        };
        if document.version != 1 {
            return Err("unsupported pin document version".into());
        }
        if document.image.width != image.width || document.image.height != image.height {
            return Err(format!(
                "pin document {} describes a {}x{} image, the pixels are {}x{}",
                self.document_path().display(),
                document.image.width,
                document.image.height,
                image.width,
                image.height
            )
            .into());
        }
        // A pinned capture can carry the region it was placed on screen as a
        // selection, while its annotations are in whole-image coordinates. The
        // session always works on one picture, so the crop is applied here once:
        // the image becomes the selection, the annotations move with it, and the
        // selection is cleared, so the next export cannot crop a crop.
        let Some(selection) = document.selection else {
            return Ok((image, base, document));
        };
        let area = crop_bounds(selection, image.width, image.height)
            .ok_or("pin document selection is empty or not finite")?;
        let (cropped, cropped_base) = crop_image(&image, &base, area)?;
        let (dx, dy) = (area.0 as f32, area.1 as f32);
        let document = Document {
            version: 1,
            image: cropped.clone(),
            selection: None,
            annotations: document
                .annotations
                .iter()
                .map(|annotation| Annotation {
                    shape: shift_shape(&annotation.shape, dx, dy),
                    style: annotation.style,
                })
                .collect(),
        };
        Ok((cropped, cropped_base, document))
    }
    /// Writes the annotated picture back into the pin's file group. The pin
    /// window keeps its place and its zoom; these files are the handoff, and the
    /// pin process reloads the picture when the session ends. `original` is the
    /// immutable base the session annotated: the source file always keeps it, so
    /// a later session still has annotations to edit rather than a flat export.
    pub(super) fn write_back(
        &self,
        image: &ImageDescriptor,
        bytes: &[u8],
        document: &Document,
        original: &[u8],
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        // Validate every payload and serialize the documents before the first
        // file is touched, so a bad payload cannot leave the group half written.
        if document.version != 1 {
            return Err("unsupported pin document version".into());
        }
        if document.selection.is_some() {
            return Err("pin document still carries a selection".into());
        }
        // The picture and the document image are the same pixels: a different
        // size would store a document that no longer matches its base.
        if document.image.width != image.width || document.image.height != image.height {
            return Err("pin document image does not match the picture".into());
        }
        if !matches!(image.format, PixelFormat::Bgra8)
            || !matches!(document.image.format, PixelFormat::Bgra8)
        {
            return Err("a pinned document must be BGRA8".into());
        }
        if image.byte_len().map_err(|e| format!("pin picture {e}"))? != bytes.len() {
            return Err("pin picture length mismatch".into());
        }
        if document
            .image
            .byte_len()
            .map_err(|e| format!("pin source {e}"))?
            != original.len()
        {
            return Err("pin source length mismatch".into());
        }
        let document_json = serde_json::to_vec_pretty(document)?;
        let flattened_json = serde_json::to_vec(&Document {
            version: 1,
            image: image.clone(),
            selection: None,
            annotations: vec![],
        })?;
        write_pixels(&self.source_path(), &document.image, original)?;
        write_pixels(&self.image_path(), image, bytes)?;
        atomic_write(&self.pin_document_path(), &flattened_json)?;
        atomic_write(&self.document_path(), &document_json)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snaplight_domain::{
        AlphaMode, Annotation, ColorSpace, ImageId, ImagePoint, ImageRect, PixelFormat, Shape,
        Style,
    };

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }
    /// A fresh temp folder, so a test never reads another run's files.
    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp folder");
        dir
    }
    fn source_image(id: u64, width: u32, height: u32, stride: u32) -> ImageDescriptor {
        ImageDescriptor {
            id: ImageId(id),
            width,
            height,
            stride,
            format: PixelFormat::Bgra8,
            alpha: AlphaMode::Opaque,
            color_space: ColorSpace::Srgb,
        }
    }
    fn tiny_rect(x: f32, y: f32, width: f32, height: f32) -> ImageRect {
        ImageRect {
            x,
            y,
            width,
            height,
        }
    }
    #[test]
    fn pin_rect_needs_four_positive_numbers() {
        let area = parse_rect("10, 20, 300, 400").expect("a valid rectangle");
        assert_eq!(
            (area.left, area.top, area.right, area.bottom),
            (10, 20, 310, 420)
        );
        // A window without a size cannot be placed, and trailing junk is a typo
        // rather than a rectangle.
        assert!(parse_rect("10,20,0,400").is_none());
        assert!(parse_rect("10,20,-3,400").is_none());
        assert!(parse_rect("10,20,300").is_none());
        assert!(parse_rect("10,20,300,400,5").is_none());
        assert!(parse_rect("10,20,300,abc").is_none());
    }
    #[test]
    fn pin_session_files_share_the_image_stem() {
        // These four files are the handoff with the pin process: the picture the
        // pin reloads when the session ends, the document with the annotations so
        // the next session still has them, and the pixels they were drawn over.
        let edit = PinEdit {
            rect: rect(0, 0, 10, 10),
            stem: PathBuf::from("C:/captures/1680000000000000000"),
            demo: true,
        };
        assert_eq!(
            edit.image_path(),
            PathBuf::from("C:/captures/1680000000000000000.bgra")
        );
        assert_eq!(
            edit.document_path(),
            PathBuf::from("C:/captures/1680000000000000000.document.json")
        );
        assert_eq!(
            edit.pin_document_path(),
            PathBuf::from("C:/captures/1680000000000000000.pin.json")
        );
        assert_eq!(
            edit.source_path(),
            PathBuf::from("C:/captures/1680000000000000000.source.bgra")
        );
    }
    #[test]
    fn pin_session_loads_the_document_and_falls_back_to_the_picture() {
        let dir = std::env::temp_dir().join("snaplight-pin-edit-load");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp folder");
        let descriptor = |id: ImageId| ImageDescriptor {
            id,
            width: 8,
            height: 4,
            stride: 32,
            format: PixelFormat::Bgra8,
            alpha: AlphaMode::Opaque,
            color_space: ColorSpace::Srgb,
        };
        let source = descriptor(ImageId(1));
        let picture = descriptor(ImageId(2));
        let edit = |dir: std::path::PathBuf| PinEdit {
            rect: rect(0, 0, 8, 4),
            stem: dir.join("123"),
            demo: false,
        };
        let edit = edit(dir.clone());
        write_pixels(&edit.source_path(), &source, &[7u8; 128]).expect("source pixels");
        // The picture the pin shows differs from the source, so the loader cannot
        // pass one test by returning the other.
        write_pixels(&edit.image_path(), &picture, &[9u8; 128]).expect("picture pixels");
        let annotation = Annotation {
            shape: Shape::Rectangle(ImageRect {
                x: 1.0,
                y: 1.0,
                width: 2.0,
                height: 2.0,
            }),
            style: Style::default(),
        };
        let document = Document {
            version: 1,
            image: source.clone(),
            selection: None,
            annotations: vec![annotation.clone()],
        };
        std::fs::write(
            edit.document_path(),
            serde_json::to_vec(&document).expect("document json"),
        )
        .expect("document");

        // The document and the pixels it was drawn over come back, so a second
        // pass over a pinned image is still editable.
        let (image, pixels, loaded) = edit.load().expect("the session loads");
        assert_eq!(image.id, ImageId(1));
        assert_eq!(pixels, vec![7u8; 128]);
        assert_eq!(loaded.annotations.len(), 1);
        assert_eq!(loaded.version, 1);

        // A pin saved without a document still opens on the pixels the capture
        // started from, with no annotations to restore.
        std::fs::remove_file(edit.document_path()).expect("remove document");
        let (image, pixels, loaded) = edit.load().expect("the session loads");
        assert_eq!(image.id, ImageId(1));
        assert_eq!(pixels, vec![7u8; 128]);
        assert!(loaded.annotations.is_empty());

        // Without a source either, the picture the pin is showing becomes the
        // base: the pass is still possible, only the earlier annotations are gone.
        std::fs::remove_file(edit.source_path()).expect("remove source");
        let (image, pixels, loaded) = edit.load().expect("the session loads");
        assert_eq!(image.id, ImageId(2));
        assert_eq!(pixels, vec![9u8; 128]);
        assert!(loaded.annotations.is_empty());
        assert_eq!(loaded.image.id, ImageId(2));

        // A document describing a different image would annotate the wrong
        // pixels, so the mismatch is refused instead of being drawn anyway.
        let wrong = Document {
            image: ImageDescriptor {
                width: 16,
                ..descriptor(ImageId(3))
            },
            annotations: vec![annotation],
            ..loaded
        };
        std::fs::write(
            edit.document_path(),
            serde_json::to_vec(&wrong).expect("document json"),
        )
        .expect("document");
        assert!(edit.load().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn pin_session_crops_the_selection_with_the_renderer_rounding() {
        let dir = temp("snaplight-pin-edit-crop");
        let edit = PinEdit {
            rect: rect(0, 0, 6, 5),
            stem: dir.join("42"),
            demo: false,
        };
        // 6x5 with eight bytes of row padding: a crop has to honour the stride
        // and not copy padding or read the wrong rows.
        let source = source_image(7, 6, 5, 32);
        assert_eq!(source.byte_len(), Ok(5 * 32));
        let mut pixels = vec![0u8; 5 * 32];
        for y in 0..5u32 {
            for x in 0..6u32 {
                let n = (y * 32 + x * 4) as usize;
                // BGRA: blue and green carry the coordinates, red a mix, so a
                // shifted row or a leaked padding byte reads back wrong.
                pixels[n..n + 4].copy_from_slice(&[x as u8, y as u8, (x * 8 + y) as u8, 255]);
            }
        }
        write_pixels(&edit.source_path(), &source, &pixels).expect("source pixels");
        // floor(2.4)=2, floor(1.6)=1, ceil(4.6)=5, ceil(3.9)=4: a 3x3 crop at (2,1).
        let selection = tiny_rect(2.4, 1.6, 2.2, 2.3);
        let annotations = vec![
            Annotation {
                shape: Shape::Rectangle(tiny_rect(3.0, 2.0, 2.0, 1.0)),
                style: Style {
                    width: 7.0,
                    ..Style::default()
                },
            },
            Annotation {
                shape: Shape::Ellipse(tiny_rect(1.0, 1.0, 1.0, 1.0)),
                style: Style::default(),
            },
            Annotation {
                shape: Shape::Arrow {
                    from: ImagePoint { x: 0.0, y: 0.0 },
                    to: ImagePoint { x: 5.0, y: 4.0 },
                },
                style: Style::default(),
            },
            Annotation {
                shape: Shape::Pen(vec![
                    ImagePoint { x: 2.0, y: 2.0 },
                    ImagePoint { x: 4.0, y: 3.0 },
                ]),
                style: Style::default(),
            },
            Annotation {
                shape: Shape::Mosaic {
                    rect: tiny_rect(3.0, 3.0, 2.0, 2.0),
                    block: 6,
                },
                style: Style::default(),
            },
            Annotation {
                shape: Shape::Text {
                    at: ImagePoint { x: 1.0, y: 1.0 },
                    text: "hi".into(),
                    size: 12.0,
                },
                style: Style::default(),
            },
        ];
        let document = Document {
            version: 1,
            image: source,
            selection: Some(selection),
            annotations,
        };
        std::fs::write(
            edit.document_path(),
            serde_json::to_vec(&document).expect("document json"),
        )
        .expect("document");

        let (image, base, loaded) = edit.load().expect("the session loads");
        assert_eq!((image.width, image.height, image.stride), (3, 3, 12));
        assert_eq!((loaded.image.width, loaded.image.height), (3, 3));
        assert!(loaded.selection.is_none());
        // Every pixel is the source pixel at (2+cx, 1+cy), copied row by row.
        assert_eq!(base.len(), 3 * 3 * 4);
        for cy in 0..3u32 {
            for cx in 0..3u32 {
                let n = (cy * 12 + cx * 4) as usize;
                assert_eq!(base[n], (2 + cx) as u8, "blue at {cx},{cy}");
                assert_eq!(base[n + 1], (1 + cy) as u8, "green at {cx},{cy}");
                assert_eq!(
                    base[n + 2],
                    ((2 + cx) * 8 + (1 + cy)) as u8,
                    "red at {cx},{cy}"
                );
                assert_eq!(base[n + 3], 255);
            }
        }
        // All six shapes moved by the crop origin (-2,-1); none was dropped and
        // style, order, text and size survive.
        assert_eq!(loaded.annotations.len(), 6);
        // Style, order, text and size all survive the crop.
        assert_eq!(loaded.annotations[0].style.width, 7.0);
        match &loaded.annotations[0].shape {
            Shape::Rectangle(r) => {
                assert_eq!((r.x, r.y, r.width, r.height), (1.0, 1.0, 2.0, 1.0))
            }
            other => panic!("expected a rectangle, got {other:?}"),
        }
        match &loaded.annotations[1].shape {
            Shape::Ellipse(r) => {
                assert_eq!((r.x, r.y, r.width, r.height), (-1.0, 0.0, 1.0, 1.0))
            }
            other => panic!("expected an ellipse, got {other:?}"),
        }
        match &loaded.annotations[2].shape {
            Shape::Arrow { from, to } => {
                assert_eq!((from.x, from.y), (-2.0, -1.0));
                assert_eq!((to.x, to.y), (3.0, 3.0));
            }
            other => panic!("expected an arrow, got {other:?}"),
        }
        match &loaded.annotations[3].shape {
            Shape::Pen(points) => {
                assert_eq!((points[0].x, points[0].y), (0.0, 1.0));
                assert_eq!((points[1].x, points[1].y), (2.0, 2.0));
            }
            other => panic!("expected a pen, got {other:?}"),
        }
        match &loaded.annotations[4].shape {
            Shape::Mosaic { rect: r, block } => {
                assert_eq!((r.x, r.y, r.width, r.height), (1.0, 2.0, 2.0, 2.0));
                assert_eq!(*block, 6);
            }
            other => panic!("expected a mosaic, got {other:?}"),
        }
        match &loaded.annotations[5].shape {
            Shape::Text { at, text, size } => {
                assert_eq!((at.x, at.y), (-1.0, 0.0));
                assert_eq!(text, "hi");
                assert_eq!(*size, 12.0);
            }
            other => panic!("expected text, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn pin_write_back_keeps_the_pristine_source_and_never_shifts_twice() {
        let dir = temp("snaplight-pin-edit-write-back");
        let edit = PinEdit {
            rect: rect(0, 0, 3, 2),
            stem: dir.join("9"),
            demo: false,
        };
        let source = source_image(3, 6, 4, 24);
        let mut pixels = vec![0u8; 4 * 24];
        for y in 0..4u32 {
            for x in 0..6u32 {
                let n = (y * 24 + x * 4) as usize;
                pixels[n..n + 4].copy_from_slice(&[x as u8, y as u8, 7, 255]);
            }
        }
        write_pixels(&edit.source_path(), &source, &pixels).expect("source pixels");
        let document = Document {
            version: 1,
            image: source,
            selection: Some(tiny_rect(1.0, 1.0, 3.0, 2.0)),
            annotations: vec![Annotation {
                shape: Shape::Rectangle(tiny_rect(2.0, 1.0, 1.0, 1.0)),
                style: Style::default(),
            }],
        };
        std::fs::write(
            edit.document_path(),
            serde_json::to_vec(&document).expect("document json"),
        )
        .expect("document");

        let (image, base, loaded) = edit.load().expect("first load");
        assert_eq!((image.width, image.height), (3, 2));
        // The session annotates the crop; the flattened picture and the pristine
        // base therefore differ, so the test can tell which one was persisted.
        let flattened = source_image(3, 3, 2, 12);
        let flattened_bytes = vec![200u8; 3 * 2 * 4];
        edit.write_back(&flattened, &flattened_bytes, &loaded, &base)
            .expect("write back");

        let (picture, picture_bytes) = read_pixels(&edit.image_path()).expect("picture");
        assert_eq!((picture.width, picture.height), (3, 2));
        assert_eq!(picture_bytes, flattened_bytes);
        // The source keeps the immutable crop, not the annotated picture, so the
        // next session can still edit the annotations.
        let (stored, stored_bytes) = read_pixels(&edit.source_path()).expect("source");
        assert_eq!((stored.width, stored.height, stored.stride), (3, 2, 12));
        assert_eq!(stored_bytes, base);
        assert_ne!(stored_bytes, flattened_bytes);

        // Reloading finds the same crop and the annotation moved by the original
        // crop once; a second translation would push it out of the picture.
        let (image, reloaded_base, reloaded) = edit.load().expect("second load");
        assert_eq!((image.width, image.height), (3, 2));
        assert!(reloaded.selection.is_none());
        assert_eq!(reloaded_base, base);
        match &reloaded.annotations[0].shape {
            Shape::Rectangle(r) => {
                assert_eq!((r.x, r.y, r.width, r.height), (1.0, 0.0, 1.0, 1.0))
            }
            other => panic!("expected a rectangle, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn pin_session_refuses_an_empty_or_non_finite_selection() {
        let dir = temp("snaplight-pin-edit-bad-selection");
        let edit = PinEdit {
            rect: rect(0, 0, 4, 4),
            stem: dir.join("5"),
            demo: false,
        };
        let source = source_image(1, 4, 4, 16);
        let zeroes = vec![0u8; 16 * 4];
        write_pixels(&edit.source_path(), &source, &zeroes).expect("source pixels");
        let with_selection = |selection: ImageRect| {
            let document = Document {
                version: 1,
                image: source.clone(),
                selection: Some(selection),
                annotations: vec![],
            };
            std::fs::write(
                edit.document_path(),
                serde_json::to_vec(&document).expect("document json"),
            )
            .expect("document");
        };
        // A zero-size region, one entirely outside, and non-finite bounds are
        // not a crop: they are refused rather than opening a broken session.
        with_selection(tiny_rect(1.0, 1.0, 0.0, 2.0));
        assert!(edit.load().is_err());
        with_selection(tiny_rect(9.0, 9.0, 2.0, 2.0));
        assert!(edit.load().is_err());
        with_selection(tiny_rect(f32::NAN, 0.0, 2.0, 2.0));
        assert!(edit.load().is_err());
        with_selection(tiny_rect(0.0, 0.0, f32::INFINITY, 2.0));
        assert!(edit.load().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn pin_crop_bounds_rejects_empty_and_fractional_widths() {
        // A zero or negative width is refused before rounding, so a fractional
        // origin cannot inflate it to a pixel.
        assert_eq!(crop_bounds(tiny_rect(1.5, 1.5, 0.0, 2.0), 8, 8), None);
        assert_eq!(crop_bounds(tiny_rect(1.5, 1.5, 2.0, 0.0), 8, 8), None);
        assert_eq!(crop_bounds(tiny_rect(1.5, 1.5, -2.0, 2.0), 8, 8), None);
        // A selection clamped entirely outside the image has no pixels left.
        assert_eq!(crop_bounds(tiny_rect(20.0, 20.0, 4.0, 4.0), 8, 8), None);
        assert_eq!(crop_bounds(tiny_rect(-4.0, 0.0, 4.0, 4.0), 8, 8), None);
        // A sub-pixel selection that still covers a pixel rounds up to one.
        assert_eq!(
            crop_bounds(tiny_rect(1.5, 1.5, 0.25, 0.25), 8, 8),
            Some((1, 1, 1, 1))
        );
    }
    #[test]
    fn pin_crop_bounds_pins_large_edges_to_the_image_pixels() {
        // 16_777_219 is not exactly a `f32`: it rounds up, so the clamped far
        // edge can exceed the real width unless the integer result is pinned to
        // the image size. The crop must stay within the image pixels.
        let width = 16_777_219u32;
        let area = crop_bounds(tiny_rect(16_777_218.0, 0.0, 2.0, 1.0), width, 1)
            .expect("a crop inside the image");
        assert_eq!(area, (16_777_218, 0, 1, 1));
        assert!(area.0.checked_add(area.2).expect("no overflow") <= width);
    }
    #[test]
    fn pin_crop_image_refuses_an_area_outside_the_image() {
        let image = source_image(1, 4, 4, 16);
        let pixels = vec![0u8; 16 * 4];
        // Empty, past the right or bottom edge, and wrap-around areas are all
        // refused instead of reading a wrong slice.
        assert!(crop_image(&image, &pixels, (0, 0, 0, 1)).is_err());
        assert!(crop_image(&image, &pixels, (0, 0, 1, 0)).is_err());
        assert!(crop_image(&image, &pixels, (3, 0, 2, 1)).is_err());
        assert!(crop_image(&image, &pixels, (0, 3, 1, 2)).is_err());
        assert!(crop_image(&image, &pixels, (u32::MAX, 0, 2, 1)).is_err());
        assert!(crop_image(&image, &pixels, (0, 0, u32::MAX, 1)).is_err());
        // A valid area still crops, so the guard does not reject the real case.
        let (cropped, _) = crop_image(&image, &pixels, (1, 1, 2, 2)).expect("a valid crop");
        assert_eq!((cropped.width, cropped.height, cropped.stride), (2, 2, 8));
    }
    #[test]
    fn pin_write_back_rejects_a_bad_payload_before_touching_files() {
        let dir = temp("snaplight-pin-edit-bad-write-back");
        let edit = PinEdit {
            rect: rect(0, 0, 4, 4),
            stem: dir.join("11"),
            demo: false,
        };
        let image = source_image(1, 2, 2, 8);
        let bytes = vec![1u8; 2 * 2 * 4];
        // Pre-existing files, so a refused payload is observable as "unchanged".
        write_pixels(&edit.image_path(), &image, &bytes).expect("picture");
        write_pixels(&edit.source_path(), &image, &bytes).expect("source");
        let before_picture = std::fs::read(edit.image_path()).expect("read picture");
        let before_source = std::fs::read(edit.source_path()).expect("read source");
        let document = Document {
            version: 1,
            image: source_image(1, 2, 2, 8),
            selection: None,
            annotations: vec![],
        };
        // Wrong picture length.
        assert!(edit
            .write_back(&image, &[1u8; 2], &document, &bytes)
            .is_err());
        // Wrong document version.
        let mut bad = document.clone();
        bad.version = 2;
        assert!(edit.write_back(&image, &bytes, &bad, &bytes).is_err());
        // A selection that should have been cleared before writing back.
        let mut bad = document.clone();
        bad.selection = Some(tiny_rect(0.0, 0.0, 1.0, 1.0));
        assert!(edit.write_back(&image, &bytes, &bad, &bytes).is_err());
        // A document image of a different size than the picture.
        let mut bad = document.clone();
        bad.image = source_image(1, 3, 2, 12);
        assert!(edit.write_back(&image, &bytes, &bad, &bytes).is_err());
        // A document image in the wrong pixel format.
        let mut bad = document.clone();
        bad.image.format = PixelFormat::Rgba8;
        assert!(edit.write_back(&image, &bytes, &bad, &bytes).is_err());
        // Wrong source length.
        assert!(edit
            .write_back(&image, &bytes, &document, &[1u8; 2])
            .is_err());
        // Every refusal happened before the first write.
        assert_eq!(
            std::fs::read(edit.image_path()).expect("picture"),
            before_picture
        );
        assert_eq!(
            std::fs::read(edit.source_path()).expect("source"),
            before_source
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

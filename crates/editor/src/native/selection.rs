//! Selection geometry: hit-testing the eight resize handles, translating the
//! committed capture region, resizing it from a handle, and deciding when a
//! press moves the region instead of drawing a new one.

use snaplight_domain::{ImagePoint, ImageRect, Tool};

/// Grid indices match the eight handles drawn by the renderer (no centre).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct SelectionHandle {
    col: u8,
    row: u8,
}

impl SelectionHandle {
    pub(super) fn new(col: u8, row: u8) -> Self {
        Self { col, row }
    }

    /// Horizontal grid index: 0 left, 1 centre, 2 right.
    pub(super) fn column(self) -> u8 {
        self.col
    }

    /// Vertical grid index: 0 top, 1 centre, 2 bottom.
    pub(super) fn row(self) -> u8 {
        self.row
    }
}

/// Hit-tests the eight selection handles. `view_scale` is the zoom the document
/// is drawn at, so the grab radius stays a constant number of screen pixels.
/// `min_view_scale` is the caller's zoom floor: the grab radius never shrinks
/// below what that zoom produces on screen.
pub(super) fn selection_handle(
    rect: ImageRect,
    point: ImagePoint,
    dpi: u32,
    view_scale: f32,
    min_view_scale: f32,
) -> Option<SelectionHandle> {
    let radius = 8.0 * (dpi as f32 / 96.0).clamp(1.0, 4.0) / view_scale.max(min_view_scale);
    // Corners take priority when a tiny selection has overlapping hit regions.
    let handles = [
        (0, 0),
        (2, 0),
        (0, 2),
        (2, 2),
        (1, 0),
        (0, 1),
        (2, 1),
        (1, 2),
    ];
    handles.into_iter().find_map(|(col, row)| {
        let x = rect.x + rect.width * col as f32 / 2.0;
        let y = rect.y + rect.height * row as f32 / 2.0;
        ((point.x - x).powi(2) + (point.y - y).powi(2) <= radius * radius)
            .then_some(SelectionHandle::new(col, row))
    })
}

pub(super) fn resize_selection(
    rect: ImageRect,
    handle: SelectionHandle,
    point: ImagePoint,
    width: u32,
    height: u32,
) -> ImageRect {
    let (mut left, mut top, mut right, mut bottom) =
        (rect.x, rect.y, rect.x + rect.width, rect.y + rect.height);
    match handle.column() {
        0 => left = point.x.clamp(0.0, right - 1.0),
        2 => right = point.x.clamp(left + 1.0, width as f32),
        _ => {}
    }
    match handle.row() {
        0 => top = point.y.clamp(0.0, bottom - 1.0),
        2 => bottom = point.y.clamp(top + 1.0, height as f32),
        _ => {}
    }
    ImageRect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }
}

/// Translates a selection by a pointer delta, keeping its size: the rectangle
/// stops at the image edges instead of being clipped, so a drag inside the
/// selection moves it rather than drawing a new one.
pub(super) fn move_selection(
    rect: ImageRect,
    dx: f32,
    dy: f32,
    width: u32,
    height: u32,
) -> ImageRect {
    ImageRect {
        x: (rect.x + dx).clamp(0.0, (width as f32 - rect.width).max(0.0)),
        y: (rect.y + dy).clamp(0.0, (height as f32 - rect.height).max(0.0)),
        ..rect
    }
}

/// The region a press drags: inside the committed selection of the select tool
/// the move wins over drawing, so the capture range can be shifted instead of
/// being drawn again from scratch. Anywhere else the press starts a new region.
pub(super) fn move_target(
    tool: Tool,
    selection: Option<ImageRect>,
    point: ImagePoint,
) -> Option<ImageRect> {
    if tool != Tool::Select {
        return None;
    }
    selection.filter(|rect| rect.contains(point))
}

#[cfg(test)]
mod tests {
    use super::*;
    use snaplight_domain::{
        AlphaMode, ColorSpace, Document, Edit, History, ImageDescriptor, ImageId, PixelFormat,
    };

    #[test]
    fn selection_handles_hit_all_eight_at_scaled_dpi() {
        let r = ImageRect {
            x: 100.0,
            y: 100.0,
            width: 200.0,
            height: 100.0,
        };
        for row in 0..3 {
            for col in 0..3 {
                let p = ImagePoint {
                    x: 100.0 + 100.0 * col as f32,
                    y: 100.0 + 50.0 * row as f32,
                };
                let expected = (col != 1 || row != 1).then_some(SelectionHandle::new(col, row));
                assert_eq!(selection_handle(r, p, 144, 1.0, 0.02), expected);
            }
        }
        assert_eq!(
            selection_handle(r, ImagePoint { x: 89.0, y: 100.0 }, 144, 1.0, 0.02),
            Some(SelectionHandle::new(0, 0))
        );
        assert_eq!(
            selection_handle(r, ImagePoint { x: 89.0, y: 100.0 }, 96, 1.0, 0.02),
            None
        );
        // A zoomed-out view widens the grab radius in image pixels so the handles
        // stay reachable on screen: at 0.2 the radius is 12 / 0.2 = 60 image
        // pixels, so a point 50 away still grabs where it would not at 1.0.
        assert_eq!(
            selection_handle(r, ImagePoint { x: 50.0, y: 100.0 }, 144, 0.2, 0.02),
            Some(SelectionHandle::new(0, 0))
        );
        assert_eq!(
            selection_handle(r, ImagePoint { x: 50.0, y: 100.0 }, 144, 1.0, 0.02),
            None
        );
    }

    #[test]
    fn a_press_inside_the_committed_region_moves_it() {
        let r = ImageRect {
            x: 100.0,
            y: 100.0,
            width: 200.0,
            height: 100.0,
        };
        let selection = Some(r);
        // Inside the region - edges included - the select tool drags the region.
        for point in [
            ImagePoint { x: 150.0, y: 150.0 },
            ImagePoint { x: 100.0, y: 100.0 },
            ImagePoint { x: 300.0, y: 200.0 },
        ] {
            assert_eq!(move_target(Tool::Select, selection, point), selection);
        }
        // Outside it the press starts a new region.
        for point in [
            ImagePoint { x: 99.0, y: 150.0 },
            ImagePoint { x: 301.0, y: 150.0 },
            ImagePoint { x: 150.0, y: 99.0 },
            ImagePoint { x: 150.0, y: 201.0 },
        ] {
            assert_eq!(move_target(Tool::Select, selection, point), None);
        }
        // Without a committed region there is nothing to move, and a drawing
        // tool uses the press to draw.
        assert_eq!(
            move_target(Tool::Select, None, ImagePoint { x: 150.0, y: 150.0 }),
            None
        );
        assert_eq!(
            move_target(
                Tool::Rectangle,
                selection,
                ImagePoint { x: 150.0, y: 150.0 }
            ),
            None
        );
    }

    #[test]
    fn moving_translates_the_selection_and_stops_at_the_image_edges() {
        let r = ImageRect {
            x: 100.0,
            y: 100.0,
            width: 200.0,
            height: 100.0,
        };
        // A plain drag moves the region without changing its size.
        assert_eq!(
            move_selection(r, 60.0, 40.0, 500, 400),
            ImageRect {
                x: 160.0,
                y: 140.0,
                width: 200.0,
                height: 100.0
            }
        );
        // At every edge the rectangle stops instead of being clipped: the far
        // edge can always reach the image border and nothing sticks out.
        assert_eq!(
            move_selection(r, -500.0, -500.0, 500, 400),
            ImageRect {
                x: 0.0,
                y: 0.0,
                width: 200.0,
                height: 100.0
            }
        );
        assert_eq!(
            move_selection(r, 500.0, 500.0, 500, 400),
            ImageRect {
                x: 300.0,
                y: 300.0,
                width: 200.0,
                height: 100.0
            }
        );
        assert_eq!(
            move_selection(r, 0.0, -500.0, 500, 400),
            ImageRect {
                x: 100.0,
                y: 0.0,
                width: 200.0,
                height: 100.0
            }
        );
        // A selection as large as the image cannot move at all.
        let full = ImageRect {
            x: 0.0,
            y: 0.0,
            width: 500.0,
            height: 400.0,
        };
        assert_eq!(move_selection(full, 60.0, 40.0, 500, 400), full);
    }

    #[test]
    fn resizing_preserves_opposite_edges_and_clamps_to_image() {
        let r = ImageRect {
            x: 100.0,
            y: 100.0,
            width: 200.0,
            height: 100.0,
        };
        for row in 0..3 {
            for col in 0..3 {
                if col == 1 && row == 1 {
                    continue;
                }
                let resized = resize_selection(
                    r,
                    SelectionHandle::new(col, row),
                    ImagePoint { x: 150.0, y: 150.0 },
                    500,
                    400,
                );
                assert_eq!(resized.x, if col == 0 { 150.0 } else { 100.0 });
                assert_eq!(resized.y, if row == 0 { 150.0 } else { 100.0 });
                assert_eq!(
                    resized.x + resized.width,
                    if col == 2 { 150.0 } else { 300.0 }
                );
                assert_eq!(
                    resized.y + resized.height,
                    if row == 2 { 150.0 } else { 200.0 }
                );
            }
        }
        let h = SelectionHandle::new(0, 0);
        assert_eq!(
            resize_selection(r, h, ImagePoint { x: -50.0, y: -50.0 }, 500, 400),
            ImageRect {
                x: 0.0,
                y: 0.0,
                width: 300.0,
                height: 200.0
            }
        );
        assert_eq!(
            resize_selection(r, h, ImagePoint { x: 450.0, y: 350.0 }, 500, 400),
            ImageRect {
                x: 299.0,
                y: 199.0,
                width: 1.0,
                height: 1.0
            }
        );
        let enlarged = resize_selection(
            r,
            SelectionHandle::new(2, 2),
            ImagePoint { x: 900.0, y: 900.0 },
            500,
            400,
        );
        assert_eq!(
            enlarged,
            ImageRect {
                x: 100.0,
                y: 100.0,
                width: 400.0,
                height: 300.0
            }
        );
        let mut doc = Document {
            version: 1,
            image: ImageDescriptor {
                id: ImageId(1),
                width: 500,
                height: 400,
                stride: 2000,
                format: PixelFormat::Bgra8,
                alpha: AlphaMode::Opaque,
                color_space: ColorSpace::Srgb,
            },
            selection: Some(r),
            annotations: vec![],
        };
        let mut history = History::default();
        history.execute(&mut doc, Edit::Selection(Some(enlarged)));
        history.undo(&mut doc);
        assert_eq!(doc.selection, Some(r));
        history.redo(&mut doc);
        assert_eq!(doc.selection, Some(enlarged));
    }
}

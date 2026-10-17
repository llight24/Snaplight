//! 编辑器画布线程与 Slint 浮窗之间的桥接状态。
//!
//! 工具栏、选项面板、取色器和底部画笔条的句柄都集中在这里，字段全部私有。
//! Slint UI 线程创建这些浮窗后通过 `bind_*` 绑定弱引用，画布线程再通过查询
//! 方法读取句柄、更新少量位置/尺寸缓存，双方都不直接接触 `OnceLock` 或
//! `Mutex`。这里只保存句柄和少量缓存，不依赖画布、会话或绘制逻辑。

use parking_lot::Mutex;
use snaplight_domain::ImageRect;
use snaplight_ui::{
    ComponentHandle, PaintBarWeak, PanelWeak, PickerWeak, ShapeMenuWeak, ToolPaint, ToolbarWeak,
};
use std::sync::OnceLock;
use windows::Win32::Foundation::RECT;

use super::tool_windows::{self, ToolbarColumn};

/// 通过窗口消息投递给画布线程的 UI 命令。
#[derive(Clone)]
pub(super) enum UiCommand {
    Action(i32),
    /// One paint snapshot, from the tool options panel or the bottom paint bar.
    /// Both windows edit the same style, so they send the same message.
    Paint(ToolPaint),
    /// Restores a pinned image session once every Slint window exists: the pin
    /// has no selection for the toolbar to follow and the session starts on the
    /// select tool, so the canvas thread is told to bring the drawing surface
    /// back. It is an explicit command rather than an action id so it cannot
    /// collide with a toolbar button.
    ResumePinEditing,
}

/// Toolbar and tool options handles shared with the canvas thread, which moves
/// the toolbar after a selection and closes options that no longer belong to it.
#[derive(Default)]
pub(super) struct UiSlots {
    toolbar: OnceLock<ToolbarWeak>,
    panel: OnceLock<PanelWeak>,
    /// Bottom paint bar and colour picker; both outlive the toolbar position.
    paint_bar: OnceLock<PaintBarWeak>,
    picker: OnceLock<PickerWeak>,
    /// Transient shape switch menu; it outlives neither the toolbar, a long
    /// capture nor the session.
    shape_menu: OnceLock<ShapeMenuWeak>,
    toolbar_size: OnceLock<(u32, u32)>,
    toolbar_selection: Mutex<Option<ImageRect>>,
    /// Physical screen rectangle of the placed toolbar, so a long capture can
    /// keep its own pill out of the region it stitches.
    toolbar_rect: Mutex<Option<RECT>>,
    /// Physical sizes of the style windows under the toolbar while each is on
    /// screen: the paint bar (set by the canvas thread) and the options panel
    /// (set by the editor UI thread). Both hang off the toolbar, so both are
    /// part of its column.
    bar_size: Mutex<Option<(i32, i32)>>,
    panel_size: Mutex<Option<(i32, i32)>>,
    /// Tool the paint bar is currently sized for, so its width changes only when
    /// the tool does.
    bar_tool: Mutex<Option<i32>>,
}

impl UiSlots {
    pub(super) fn toolbar(&self) -> Option<&ToolbarWeak> {
        self.toolbar.get()
    }
    pub(super) fn panel(&self) -> Option<&PanelWeak> {
        self.panel.get()
    }
    pub(super) fn picker(&self) -> Option<&PickerWeak> {
        self.picker.get()
    }
    pub(super) fn paint_bar(&self) -> Option<&PaintBarWeak> {
        self.paint_bar.get()
    }
    pub(super) fn shape_menu(&self) -> Option<&ShapeMenuWeak> {
        self.shape_menu.get()
    }

    pub(super) fn bind_toolbar(&self, weak: ToolbarWeak) -> Result<(), ToolbarWeak> {
        self.toolbar.set(weak)
    }
    pub(super) fn bind_panel(&self, weak: PanelWeak) -> Result<(), PanelWeak> {
        self.panel.set(weak)
    }
    pub(super) fn bind_picker(&self, weak: PickerWeak) -> Result<(), PickerWeak> {
        self.picker.set(weak)
    }
    pub(super) fn bind_paint_bar(&self, weak: PaintBarWeak) -> Result<(), PaintBarWeak> {
        self.paint_bar.set(weak)
    }
    pub(super) fn bind_shape_menu(&self, weak: ShapeMenuWeak) -> Result<(), ShapeMenuWeak> {
        self.shape_menu.set(weak)
    }
    /// Records whether the paint bar is on screen and how big it is.
    pub(super) fn set_bar_size(&self, size: Option<(i32, i32)>) {
        *self.bar_size.lock() = size;
    }
    /// Records whether the options panel is on screen and how big it is.
    pub(super) fn set_panel_size(&self, size: Option<(i32, i32)>) {
        *self.panel_size.lock() = size;
    }
    /// The style windows under the toolbar, in the order the column places
    /// them: the paint bar first, then the options panel.
    pub(super) fn style_sizes(&self) -> Vec<(i32, i32)> {
        let mut sizes = Vec::new();
        if let Some(size) = *self.bar_size.lock() {
            sizes.push(size);
        }
        if let Some(size) = *self.panel_size.lock() {
            sizes.push(size);
        }
        sizes
    }

    pub(super) fn bind_toolbar_size(&self, size: (u32, u32)) -> Result<(), (u32, u32)> {
        self.toolbar_size.set(size)
    }
    pub(super) fn toolbar_size(&self) -> Option<(u32, u32)> {
        self.toolbar_size.get().copied()
    }

    /// Forgets the selection the toolbar was last placed against, so the next
    /// selection always repositions it.
    pub(super) fn clear_selection_cache(&self) {
        *self.toolbar_selection.lock() = None;
    }

    /// Records the selection the toolbar is placed against and reports whether it
    /// differs from the one already displayed. Comparing and updating inside one
    /// lock keeps a repeated selection from repositioning the toolbar.
    pub(super) fn selection_changed(&self, selection: ImageRect) -> bool {
        let mut displayed = self.toolbar_selection.lock();
        if *displayed == Some(selection) {
            return false;
        }
        *displayed = Some(selection);
        true
    }

    pub(super) fn toolbar_rect(&self) -> Option<RECT> {
        *self.toolbar_rect.lock()
    }
    pub(super) fn set_toolbar_rect(&self, rect: Option<RECT>) {
        *self.toolbar_rect.lock() = rect;
    }

    /// Records the tool the paint bar is sized for and reports whether it differs
    /// from the previous one, so the bar is reshaped only when the tool changes.
    pub(super) fn paint_tool_changed(&self, tool: i32) -> bool {
        let mut last = self.bar_tool.lock();
        let changed = *last != Some(tool);
        *last = Some(tool);
        changed
    }
}

/// The toolbar and the style windows under it, ready to be applied on the Slint
/// window thread. Positions are physical screen coordinates.
#[derive(Clone, Copy)]
pub(super) struct StyleColumn {
    pub(super) toolbar: (i32, i32),
    pub(super) toolbar_size: (i32, i32),
    pub(super) bar: Option<(i32, i32)>,
    pub(super) panel: Option<(i32, i32)>,
}

impl StyleColumn {
    fn from_column(column: ToolbarColumn, size: (i32, i32), bar: bool, panel: bool) -> Self {
        let mut style = column.style.into_iter();
        let bar = bar.then(|| style.next().unwrap_or(column.toolbar));
        let panel = panel.then(|| style.next().unwrap_or(column.toolbar));
        Self {
            toolbar: column.toolbar,
            toolbar_size: size,
            bar,
            panel,
        }
    }
}

/// Places the toolbar and the style windows hanging under it inside the monitor
/// work area, from the toolbar rectangle the session last used. `None` while the
/// toolbar has not been placed: a capture session before its selection has no
/// anchor for the column.
pub(super) fn style_column(slots: &UiSlots, dpi: u32, work_area: RECT) -> Option<StyleColumn> {
    let toolbar = slots.toolbar_rect()?;
    let size = snaplight_ui::scale_to_physical(slots.toolbar_size()?, dpi);
    let bar = slots.bar_size.lock().is_some();
    let panel = slots.panel_size.lock().is_some();
    let toolbar = RECT {
        left: toolbar.left,
        top: toolbar.top,
        right: toolbar.left + size.0,
        bottom: toolbar.top + size.1,
    };
    Some(StyleColumn::from_column(
        tool_windows::toolbar_column(toolbar, &slots.style_sizes(), work_area),
        size,
        bar,
        panel,
    ))
}

/// Applies a style column on the Slint window thread: the toolbar moves when the
/// column had to shift, and each style window takes its place under it. A window
/// is only moved when its position really changes, so a repeated layout costs
/// nothing and never repaints the toolbar.
pub(super) fn apply_column(slots: &UiSlots, column: &StyleColumn) {
    let (x, y) = column.toolbar;
    if let Some(weak) = slots.toolbar() {
        // Only move a toolbar that is on screen: `move_toolbar` shows the window,
        // so a late style pass after the toolbar was hidden must never bring it
        // back. The first display stays with the explicit canvas move.
        if let Some(ui) = weak.upgrade() {
            if ui.window().is_visible() {
                let position = ui.window().position();
                if (position.x, position.y) != (x, y) {
                    snaplight_ui::move_toolbar(weak, snaplight_ui::PhysicalPosition::new(x, y));
                }
            }
        }
    }
    slots.set_toolbar_rect(Some(RECT {
        left: x,
        top: y,
        right: x + column.toolbar_size.0,
        bottom: y + column.toolbar_size.1,
    }));
    if let Some(weak) = slots.paint_bar() {
        if let (Some(position), Some(ui)) = (column.bar, weak.upgrade()) {
            place_window(ui.window(), position);
        }
    }
    if let Some(weak) = slots.panel() {
        if let (Some(position), Some(ui)) = (column.panel, weak.upgrade()) {
            place_window(ui.window(), position);
        }
    }
}

/// Moves only the style windows to an explicit column, in the same order as
/// `style_sizes` (the paint bar first, then the options panel). The shape menu
/// uses it when it opens under the toolbar and has to push them down, so they
/// stay clear of it.
pub(super) fn apply_style_positions(slots: &UiSlots, positions: &[(i32, i32)]) {
    let mut positions = positions.iter();
    if slots.bar_size.lock().is_some() {
        if let (Some(position), Some(weak)) = (positions.next(), slots.paint_bar()) {
            if let Some(ui) = weak.upgrade() {
                place_window(ui.window(), *position);
            }
        }
    }
    if slots.panel_size.lock().is_some() {
        if let (Some(position), Some(weak)) = (positions.next(), slots.panel()) {
            if let Some(ui) = weak.upgrade() {
                place_window(ui.window(), *position);
            }
        }
    }
}

/// Moves a Slint window on the window thread, skipping a no-op move.
fn place_window(window: &snaplight_ui::Window, (x, y): (i32, i32)) {
    let position = window.position();
    if (position.x, position.y) != (x, y) {
        window.set_position(snaplight_ui::PhysicalPosition::new(x, y));
    }
}

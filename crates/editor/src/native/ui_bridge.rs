//! 编辑器画布线程与 Slint 浮窗之间的桥接状态。
//!
//! 工具栏、选项面板、取色器和底部画笔条的句柄都集中在这里，字段全部私有。
//! Slint UI 线程创建这些浮窗后通过 `bind_*` 绑定弱引用，画布线程再通过查询
//! 方法读取句柄、更新少量位置/尺寸缓存，双方都不直接接触 `OnceLock` 或
//! `Mutex`。这里只保存句柄和少量缓存，不依赖画布、会话或绘制逻辑。

use parking_lot::Mutex;
use snaplight_domain::ImageRect;
use snaplight_ui::{PaintBarWeak, PanelWeak, PickerWeak, ToolPaint, ToolbarWeak};
use std::sync::OnceLock;
use windows::Win32::Foundation::RECT;

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
    toolbar_size: OnceLock<(u32, u32)>,
    toolbar_selection: Mutex<Option<ImageRect>>,
    /// Physical screen rectangle of the placed toolbar, so a long capture can
    /// keep its own pill out of the region it stitches.
    toolbar_rect: Mutex<Option<RECT>>,
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

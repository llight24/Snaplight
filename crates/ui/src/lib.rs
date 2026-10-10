slint::include_modules!();
use slint::Model;
pub use slint::{ComponentHandle, ModelRc, PhysicalPosition, SharedString, VecModel, Window};
use snaplight_domain::{Config, Tool, WindowDetect};
/// Stable command id for each tool. The toolbar sends these ids, so the visual
/// order can change without changing the editor's tool mapping.
fn tool_id(tool: Tool) -> i32 {
    match tool {
        Tool::Select => 0,
        Tool::Rectangle => 1,
        Tool::Ellipse => 2,
        Tool::Arrow => 3,
        Tool::Pen => 4,
        Tool::Mosaic => 5,
        Tool::Text => 6,
    }
}
fn tool_label(tool: Tool) -> &'static str {
    match tool {
        Tool::Select => "Select",
        Tool::Rectangle => "Rect",
        Tool::Ellipse => "Ellipse",
        Tool::Arrow => "Arrow",
        Tool::Pen => "Pen",
        Tool::Mosaic => "Mosaic",
        Tool::Text => "Text",
    }
}
fn tool_items(tools: &[Tool]) -> Vec<ToolItem> {
    tools
        .iter()
        .map(|tool| ToolItem {
            id: tool_id(*tool),
            label: tool_label(*tool).into(),
        })
        .collect()
}
/// Tool ids in the order the toolbar displays them, as sent back to the editor.
pub fn tool_order(model: &ModelRc<ToolItem>) -> Vec<i32> {
    (0..model.row_count())
        .filter_map(|index| model.row_data(index).map(|item| item.id))
        .collect()
}
pub fn tool_from_id(id: i32) -> Option<Tool> {
    [
        Tool::Select,
        Tool::Rectangle,
        Tool::Ellipse,
        Tool::Arrow,
        Tool::Pen,
        Tool::Mosaic,
        Tool::Text,
    ]
    .get(usize::try_from(id).ok()?)
    .copied()
}
pub fn toolbar(config: &Config) -> Result<Toolbar, slint::PlatformError> {
    let ui = Toolbar::new()?;
    ui.set_actions(ModelRc::new(VecModel::from(toolbar_actions())));
    ui.set_bar_width(toolbar_logical_size(&config.tools).0 as f32);
    ui.set_tools(ModelRc::new(VecModel::from(tool_items(&config.tools))));
    ui.set_stroke(config.stroke_width as i32);
    ui.set_block(config.mosaic_block as i32);
    Ok(ui)
}
/// Quick-settings window shown when a tool's drop-down arrow is clicked.
pub fn options_panel(config: &Config) -> Result<OptionsPanel, slint::PlatformError> {
    let ui = OptionsPanel::new()?;
    ui.set_stroke(config.stroke_width as i32);
    ui.set_block(config.mosaic_block as i32);
    Ok(ui)
}
pub fn settings(config: &Config) -> Result<Settings, slint::PlatformError> {
    let ui = Settings::new()?;
    ui.set_tools(ModelRc::new(VecModel::from(tool_items(&config.tools))));
    ui.set_stroke(config.stroke_width as i32);
    ui.set_block(config.mosaic_block as i32);
    ui.set_gpu_pins(config.gpu_pins);
    ui.set_background_value(config.theme.background.clone().into());
    ui.set_foreground_value(config.theme.foreground.clone().into());
    ui.set_accent_value(config.theme.accent.clone().into());
    ui.set_capture_hotkey_value(config.capture_hotkey.clone().into());
    ui.set_restore_hotkey_value(config.restore_hotkey.clone().into());
    ui.set_detect_mode(window_detect_index(config.window_detect));
    Ok(ui)
}
/// Detection modes in combo-box order; keep in sync with the model in
/// `toolbar.slint`. A pixel-based mode would be appended here.
const WINDOW_DETECT_MODES: [WindowDetect; 2] = [WindowDetect::Off, WindowDetect::Ui];
/// Combo-box index of a detection mode.
pub fn window_detect_index(mode: WindowDetect) -> i32 {
    WINDOW_DETECT_MODES
        .iter()
        .position(|candidate| *candidate == mode)
        .unwrap_or(WINDOW_DETECT_MODES.len() - 1) as i32
}
/// Detection mode for a combo-box index. An unknown index falls back to the
/// default instead of silently disabling detection.
pub fn window_detect_from_index(index: i32) -> WindowDetect {
    usize::try_from(index)
        .ok()
        .and_then(|index| WINDOW_DETECT_MODES.get(index).copied())
        .unwrap_or_default()
}
/// Moves one entry of the tool order, clamped to the ends.
pub fn move_tool(items: &ModelRc<ToolItem>, index: i32, delta: i32) -> Option<Vec<ToolItem>> {
    let index = usize::try_from(index).ok()?;
    let mut tools: Vec<ToolItem> = (0..items.row_count())
        .filter_map(|i| items.row_data(i))
        .collect();
    let target = index.checked_add_signed(delta as isize)?;
    if index >= tools.len() || target >= tools.len() {
        return None;
    }
    tools.swap(index, target);
    Some(tools)
}
/// Copies the edited order back into the configuration. An empty or unusable
/// model leaves the configured order untouched, so a broken UI cannot wipe it.
pub fn apply_tool_order(config: &mut Config, items: &ModelRc<ToolItem>) {
    let tools: Vec<Tool> = tool_order(items)
        .into_iter()
        .filter_map(tool_from_id)
        .collect();
    if !tools.is_empty() {
        config.tools = tools;
    }
}
/// Logical size derived from configured tools and the action list.
// Display order is independent of stable editor command ids.
const TOOLBAR_ACTIONS: &[(i32, i32, &str)] = &[
    (16, 2, "Tool options"),
    (14, 5, "Pin"),
    (13, 4, "Save"),
    (15, 6, "Close"),
    (12, 3, "Copy"),
];
fn toolbar_actions() -> Vec<CommandItem> {
    TOOLBAR_ACTIONS
        .iter()
        .map(|&(id, glyph, label)| CommandItem {
            id,
            glyph,
            label: label.into(),
        })
        .collect()
}
pub fn toolbar_logical_size(tools: &[Tool]) -> (u32, u32) {
    // Padding + grip + two dividers + seven commands + inter-item gaps.
    let tools_width: u32 = tools
        .iter()
        .map(|tool| if *tool == Tool::Select { 40 } else { 50 })
        .sum();
    (
        144 + 40 * TOOLBAR_ACTIONS.len() as u32
            + tools_width
            + 2 * (tools.len() as u32 + TOOLBAR_ACTIONS.len() as u32 + 4),
        52,
    )
}
/// Logical size of the tool options window; keep in sync with `toolbar.slint`.
pub const OPTIONS_LOGICAL_SIZE: (u32, u32) = (340, 52);
/// Window titles, used to find the Slint-created windows again for the native
/// rounding and taskbar styling; keep in sync with `toolbar.slint`.
pub const TOOLBAR_TITLE: &str = "Snaplight Tools";
pub const OPTIONS_TITLE: &str = "Snaplight Tool Options";
/// Converts a logical size to physical pixels for a monitor DPI. The toolbar is
/// placed in physical coordinates before its window exists, so the size must be
/// derived from the monitor DPI rather than read back from Slint.
pub fn scale_to_physical(logical: (u32, u32), dpi: u32) -> (i32, i32) {
    let scale = |value: u32| ((value as u64 * dpi as u64 + 48) / 96) as i32;
    (scale(logical.0), scale(logical.1))
}
/// Handle to the toolbar window that other threads may use.
pub type ToolbarWeak = slint::Weak<Toolbar>;
/// Handle to the tool options window that other threads may use.
pub type PanelWeak = slint::Weak<OptionsPanel>;
/// Hides the tool options from another thread, so quick settings never outlive
/// the position of the toolbar they belong to.
pub fn hide_panel(weak: &PanelWeak) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = ui.hide();
        }
    });
}
/// Shows and places the toolbar after a completed selection.
pub fn move_toolbar(weak: &ToolbarWeak, position: PhysicalPosition) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            if !ui.window().is_visible() {
                let _ = ui.show();
            }
            ui.window().set_position(position);
            // Software renderer caches dirty regions. Moving the native window
            // invalidates its backing pixels, so refresh the entire opaque surface.
            ui.set_repaint(!ui.get_repaint());
            ui.window().request_redraw();
        }
    });
}
/// Updates the toolbar status line from another thread.
pub fn set_toolbar_status(weak: &ToolbarWeak, status: String) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_status(status.into());
        }
    });
}
/// Updates the history availability of the toolbar from another thread, so
/// undo and redo only look enabled when there is something to undo or redo.
pub fn set_toolbar_history(weak: &ToolbarWeak, can_undo: bool, can_redo: bool) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_can_undo(can_undo);
            ui.set_can_redo(can_redo);
        }
    });
}
/// Runs the event loop until `quit`. The toolbar runs this instead of
/// `ComponentHandle::run`, which would show the window before the first
/// selection exists; the window is shown on demand instead.
pub fn run_event_loop() -> std::result::Result<(), slint::PlatformError> {
    slint::run_event_loop_until_quit()
}
/// Runs the event loop for `duration`, then stops it. Smoke checks use this to
/// observe the real window geometry without user interaction.
pub fn run_event_loop_for(duration: std::time::Duration) {
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::SingleShot, duration, || {
        let _ = slint::quit_event_loop();
    });
    let _ = slint::run_event_loop();
}
pub fn quit() {
    let _ = slint::quit_event_loop();
}
pub fn invoke(f: impl FnOnce() + Send + 'static) {
    let _ = slint::invoke_from_event_loop(f);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn model(tools: &[Tool]) -> ModelRc<ToolItem> {
        ModelRc::new(VecModel::from(tool_items(tools)))
    }
    #[test]
    fn tool_order_moves_and_clamps_at_the_ends() {
        let items = model(&Config::default().tools);
        assert_eq!(tool_order(&items), (0..7).collect::<Vec<_>>());
        let moved = move_tool(&items, 0, 1).expect("move down");
        assert_eq!(
            moved.iter().map(|item| item.id).collect::<Vec<_>>(),
            vec![1, 0, 2, 3, 4, 5, 6]
        );
        let moved = move_tool(&items, 6, -1).expect("move up");
        assert_eq!(
            moved.iter().map(|item| item.id).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4, 6, 5]
        );
        assert!(move_tool(&items, 0, -1).is_none());
        assert!(move_tool(&items, 6, 1).is_none());
        assert!(move_tool(&items, 99, 1).is_none());
    }
    #[test]
    fn logical_sizes_scale_to_monitor_dpi() {
        assert_eq!(
            scale_to_physical(toolbar_logical_size(&Config::default().tools), 96),
            (716, 52)
        );
        assert_eq!(
            scale_to_physical(toolbar_logical_size(&Config::default().tools), 144),
            (1074, 78)
        );
        assert_eq!(scale_to_physical(OPTIONS_LOGICAL_SIZE, 144), (510, 78));
        assert_eq!(
            scale_to_physical(toolbar_logical_size(&Config::default().tools), 192),
            (1432, 104)
        );
        // Rounded to the nearest physical pixel.
        assert_eq!(scale_to_physical((1, 1), 144), (2, 2));
    }
    #[test]
    fn detection_mode_index_round_trips() {
        for mode in [WindowDetect::Off, WindowDetect::Ui] {
            assert_eq!(window_detect_from_index(window_detect_index(mode)), mode);
        }
        assert_eq!(window_detect_index(WindowDetect::Off), 0);
        // An out-of-range index keeps detection on rather than turning it off.
        assert_eq!(window_detect_from_index(-1), WindowDetect::Ui);
        assert_eq!(window_detect_from_index(7), WindowDetect::Ui);
    }
    #[test]
    fn applying_the_order_keeps_a_usable_tool_list() {
        let mut config = Config::default();
        let moved = move_tool(&model(&config.tools), 0, 1).unwrap();
        apply_tool_order(&mut config, &ModelRc::new(VecModel::from(moved)));
        assert_eq!(config.tools[0], Tool::Rectangle);
        assert_eq!(config.tools[1], Tool::Select);
        assert_eq!(config.tools.len(), 7);
        // An empty model must not erase the configured order.
        apply_tool_order(
            &mut config,
            &ModelRc::new(VecModel::from(Vec::<ToolItem>::new())),
        );
        assert_eq!(config.tools.len(), 7);
        assert!(config.validate().is_ok());
    }
}

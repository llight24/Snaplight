slint::include_modules!();
mod color_picker;
pub use color_picker::{
    color_picker, picker_set_color, picker_set_hex, picker_set_hue, picker_set_sv,
};
pub use slint::{
    Color, ComponentHandle, Model, ModelRc, PhysicalPosition, SharedString, VecModel, Window,
};
use snaplight_domain::{
    parse_hex_color, Config, Tool, WindowDetect, DEFAULT_STROKE_COLOR, MAX_PIN_SHADOW_BLUR,
    MAX_PIN_SHADOW_OFFSET,
};
/// Display name of each tool. Settings and the accessibility tree are the only
/// places the toolbar shows text, and both are Chinese first.
fn tool_label(tool: Tool) -> &'static str {
    match tool {
        Tool::Select => "选择",
        Tool::Rectangle => "矩形",
        Tool::Ellipse => "椭圆",
        Tool::Arrow => "箭头",
        Tool::Pen => "画笔",
        Tool::Mosaic => "马赛克",
        Tool::Text => "文字",
    }
}
fn tool_items(tools: &[Tool]) -> Vec<ToolItem> {
    tools
        .iter()
        .map(|tool| ToolItem {
            id: tool.id(),
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
/// Ids and tools are defined once, in the domain, so the toolbar and the editor
/// cannot disagree about what a message means.
pub fn tool_from_id(id: i32) -> Option<Tool> {
    Tool::from_id(id)
}
/// Parses a configured `#RRGGBB` colour, falling back to the default stroke red
/// so a hand-edited configuration cannot leave the UI without a colour.
fn config_color(text: &str) -> slint::Color {
    let rgba = parse_hex_color(text)
        .or_else(|| parse_hex_color(DEFAULT_STROKE_COLOR))
        .unwrap_or([0.95, 0.2, 0.25, 1.0]);
    slint_color(rgba)
}
/// One settings row per stable tool id, so every tool gets a paint choice even
/// when the configured lists omit it. Tools that cannot paint the kind at all
/// always report `false`, matching the options panel's conditional rows.
pub fn tool_paint_rows(config: &Config) -> Vec<ToolPaintRow> {
    Tool::ALL
        .iter()
        .map(|&tool| ToolPaintRow {
            tool: tool.id(),
            label: tool_label(tool).into(),
            stroke: tool.can_stroke() && config.stroke_tools.contains(&tool),
            fill: tool.can_fill() && config.fill_tools.contains(&tool),
        })
        .collect()
}
/// Reads the two configured tool lists back out of the settings model. Unknown
/// tool ids are ignored and unchecked rows are left out.
pub fn paint_tools(rows: &ModelRc<ToolPaintRow>) -> (Vec<Tool>, Vec<Tool>) {
    let mut stroke = Vec::new();
    let mut fill = Vec::new();
    for index in 0..rows.row_count() {
        let Some(row) = rows.row_data(index) else {
            continue;
        };
        let Some(tool) = Tool::from_id(row.tool) else {
            continue;
        };
        if row.stroke && tool.can_stroke() {
            stroke.push(tool);
        }
        if row.fill && tool.can_fill() {
            fill.push(tool);
        }
    }
    (stroke, fill)
}
pub fn toolbar(config: &Config) -> Result<Toolbar, slint::PlatformError> {
    let ui = Toolbar::new()?;
    ui.set_actions(ModelRc::new(VecModel::from(toolbar_actions())));
    ui.set_bar_width(toolbar_logical_size(&config.tools).0 as f32);
    ui.set_tools(ModelRc::new(VecModel::from(tool_items(&config.tools))));
    ui.set_stroke_width(config.stroke_width as i32);
    ui.set_block(config.mosaic_block as i32);
    ui.set_fill_opacity(config.fill_opacity as i32);
    ui.set_font_size(config.font_size as i32);
    ui.set_stroke_color(config_color(&config.stroke_color));
    ui.set_fill_color(config_color(&config.fill_color));
    Ok(ui)
}
/// Quick-settings window shown when a tool's drop-down arrow is clicked.
pub fn options_panel(config: &Config) -> Result<OptionsPanel, slint::PlatformError> {
    let ui = OptionsPanel::new()?;
    ui.set_stroke_width(config.stroke_width as i32);
    ui.set_block(config.mosaic_block as i32);
    ui.set_fill_opacity(config.fill_opacity as i32);
    ui.set_font_size(config.font_size as i32);
    ui.set_stroke_color(config_color(&config.stroke_color));
    ui.set_fill_color(config_color(&config.fill_color));
    Ok(ui)
}
/// Persistent paint bar under the selection. It starts from the same values as
/// the options panel; the editor sets the active tool when it shows it.
pub fn paint_bar(config: &Config) -> Result<PaintBar, slint::PlatformError> {
    let ui = PaintBar::new()?;
    ui.set_stroke_width(config.stroke_width as i32);
    ui.set_block(config.mosaic_block as i32);
    ui.set_fill_opacity(config.fill_opacity as i32);
    ui.set_font_size(config.font_size as i32);
    ui.set_stroke_color(config_color(&config.stroke_color));
    ui.set_fill_color(config_color(&config.fill_color));
    Ok(ui)
}
/// File-name slugs of the settings navigation entries, in the order the window
/// shows them. Keep in sync with the page conditions in `toolbar.slint`.
pub const SETTINGS_PAGES: [&str; 7] = [
    "general",
    "appearance",
    "hotkeys",
    "capture",
    "translation",
    "recording",
    "settings",
];
/// Page shown when the settings window opens: appearance, pins and toolbar.
pub const SETTINGS_DEFAULT_PAGE: i32 = 0;
/// Result of classifying one captured key event.
enum CapturedKey {
    /// A key the configuration can store (`A`, `7`, `F12`).
    Key(String),
    /// One of Slint's private key codes for a modifier key.
    Modifier,
    /// Anything the configuration cannot store; the recorder waits for another
    /// key instead of surfacing a value that saving would reject.
    Unsupported,
}
/// Classifies the `KeyEvent.text` of a settings recording.
fn captured_key(text: &str) -> CapturedKey {
    let Some(key) = text.chars().next() else {
        return CapturedKey::Unsupported;
    };
    if text.chars().nth(1).is_some() {
        return CapturedKey::Unsupported;
    }
    // The private characters Slint uses for modifier keys (i-slint-common's key
    // map): Shift, Control, Alt, AltGr, ShiftR, ControlR, Meta, MetaR.
    if matches!(
        key,
        '\u{0010}'
            | '\u{0011}'
            | '\u{0012}'
            | '\u{0013}'
            | '\u{0015}'
            | '\u{0016}'
            | '\u{0017}'
            | '\u{0018}'
    ) {
        return CapturedKey::Modifier;
    }
    if key.is_ascii_alphabetic() {
        return CapturedKey::Key(key.to_ascii_uppercase().to_string());
    }
    if key.is_ascii_digit() {
        return CapturedKey::Key(key.to_string());
    }
    // F1-F24 share the private range starting at U+F704.
    let code = key as u32;
    if (0xF704..=0xF71B).contains(&code) {
        return CapturedKey::Key(format!("F{}", code - 0xF704 + 1));
    }
    CapturedKey::Unsupported
}
/// Formats a key captured in the settings recorder as a stored shortcut, the
/// same canonical spelling `parse_hotkey` and `parse_held_shortcut` accept.
///
/// An empty result means the recorder must keep waiting: either the key is not
/// supported, or a registered hotkey has no key/modifier yet. The held
/// interaction key is the one shortcut that may be a bare modifier or a bare
/// key, so `allow_modifier_only` enables those shapes there only.
fn captured_hotkey(
    key: &str,
    ctrl: bool,
    shift: bool,
    alt: bool,
    win: bool,
    allow_modifier_only: bool,
) -> String {
    let mut parts = Vec::new();
    if ctrl {
        parts.push("Ctrl".to_string());
    }
    if shift {
        parts.push("Shift".to_string());
    }
    if alt {
        parts.push("Alt".to_string());
    }
    if win {
        parts.push("Win".to_string());
    }
    match captured_key(key) {
        CapturedKey::Key(key) => parts.push(key),
        CapturedKey::Modifier if allow_modifier_only => {}
        CapturedKey::Modifier | CapturedKey::Unsupported => return String::new(),
    }
    if parts.is_empty() || (!allow_modifier_only && parts.len() < 2) {
        return String::new();
    }
    parts.join("+")
}
pub fn settings(config: &Config) -> Result<Settings, slint::PlatformError> {
    let ui = Settings::new()?;
    ui.set_page(SETTINGS_DEFAULT_PAGE);
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
    ui.set_tools(ModelRc::new(VecModel::from(tool_items(&config.tools))));
    ui.set_stroke(config.stroke_width as i32);
    ui.set_block(config.mosaic_block as i32);
    ui.set_fill_opacity(config.fill_opacity as i32);
    ui.set_font_size(config.font_size as i32);
    ui.set_stroke_color_value(config.stroke_color.clone().into());
    ui.set_fill_color_value(config.fill_color.clone().into());
    ui.set_paint_tools(ModelRc::new(VecModel::from(tool_paint_rows(config))));
    ui.set_gpu_pins(config.gpu_pins);
    ui.set_pin_close_confirm(config.pin_close_confirm);
    ui.set_pin_shadow(config.pin_shadow);
    ui.set_pin_shadow_blur(config.pin_shadow_blur.min(MAX_PIN_SHADOW_BLUR) as i32);
    ui.set_pin_shadow_offset(config.pin_shadow_offset.min(MAX_PIN_SHADOW_OFFSET) as i32);
    ui.set_pin_shadow_opacity(config.pin_shadow_opacity.min(100) as i32);
    ui.set_background_value(config.theme.background.clone().into());
    ui.set_foreground_value(config.theme.foreground.clone().into());
    ui.set_accent_value(config.theme.accent.clone().into());
    ui.set_capture_hotkey_value(config.capture_hotkey.clone().into());
    ui.set_restore_hotkey_value(config.restore_hotkey.clone().into());
    ui.set_through_hotkey_value(config.through_hotkey.clone().into());
    ui.on_captured_hotkey(|key, ctrl, shift, alt, win, allow_modifier_only| {
        captured_hotkey(key.as_str(), ctrl, shift, alt, win, allow_modifier_only).into()
    });
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
// Display order is independent of stable editor command ids. The last field marks
// the buttons that stay live while a long capture runs: saving, copying, pinning
// and closing the stitch act exactly like the same buttons on a normal capture.
const TOOLBAR_ACTIONS: &[(i32, i32, &str, bool)] = &[
    (16, 2, "工具选项", false),
    (17, 7, "长截图", false),
    (14, 5, "贴图", true),
    (13, 4, "保存", true),
    (15, 6, "关闭", true),
    (12, 3, "复制", true),
];
fn toolbar_actions() -> Vec<CommandItem> {
    TOOLBAR_ACTIONS
        .iter()
        .map(|&(id, glyph, label, capture)| CommandItem {
            id,
            glyph,
            label: label.into(),
            capture,
        })
        .collect()
}
pub fn toolbar_logical_size(tools: &[Tool]) -> (u32, u32) {
    // Padding + grip + two dividers + eight commands + inter-item gaps.
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
/// Logical size of the options panel for one tool: the paint row plus the close
/// button, which only the panel has.
pub fn paint_logical_size(tool: i32) -> (u32, u32) {
    const CLOSE_BUTTON: u32 = 40;
    const GAP: u32 = 10;
    let (width, height) = paint_row_size(tool);
    (width + CLOSE_BUTTON + GAP, height)
}
/// Logical size of the bottom paint bar for one tool; it carries the row alone.
pub fn paint_bar_logical_size(tool: i32) -> (u32, u32) {
    paint_row_size(tool)
}
/// Width and height of the row of paint controls for one tool. The options panel
/// and the bottom paint bar show the same row, so both size themselves to it and
/// a tool with fewer controls gets a shorter window instead of a wide one with
/// empty space. The widths come from what the smoke measures for the row at its
/// preferred size, plus that row's own padding and a little slack: the two shape
/// groups need 423, text adds the size and text fields (711), a single outline
/// group 219 and the mosaic row 186.
fn paint_row_size(tool: i32) -> (u32, u32) {
    const ROW_HEIGHT: u32 = 52;
    let width = match tool {
        1 | 2 => 430,
        6 => 730,
        // Mosaic shows one label and one spin box, the pen and arrow a single
        // outline group.
        5 => 205,
        _ => 245,
    };
    (width, ROW_HEIGHT)
}
/// Logical size of the colour picker; keep in sync with `toolbar.slint`.
pub const PICKER_LOGICAL_SIZE: (u32, u32) = (280, 256);
/// Window titles, used to find the Slint-created windows again for the native
/// rounding, taskbar styling and app icon; keep in sync with `toolbar.slint`.
pub const TOOLBAR_TITLE: &str = "Snaplight Tools";
pub const OPTIONS_TITLE: &str = "Snaplight Tool Options";
pub const PICKER_TITLE: &str = "Snaplight Color";
pub const PAINT_BAR_TITLE: &str = "Snaplight Paint";
/// The settings window keeps its system frame, so it is the one Slint window
/// that also needs the application icon applied to it.
pub const SETTINGS_TITLE: &str = "Snaplight 设置";
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
/// Handle to the colour picker window that other threads may use.
pub type PickerWeak = slint::Weak<ColorPicker>;
/// Handle to the persistent paint bar that other threads may use.
pub type PaintBarWeak = slint::Weak<PaintBar>;
/// Paint values of the toolbar for one tool id, so the editor can seed the
/// options panel and the paint bar before showing them. `stroke-tools` and
/// `fill-tools` are indexed by stable tool id; an out-of-range id reads false.
pub fn toolbar_paint(toolbar: &Toolbar, tool: i32) -> ToolPaint {
    let flag = |tools: ModelRc<bool>, id: i32| {
        usize::try_from(id)
            .ok()
            .and_then(|id| tools.row_data(id))
            .unwrap_or(false)
    };
    let stroke_on = flag(toolbar.get_stroke_tools(), tool);
    let fill_on = flag(toolbar.get_fill_tools(), tool);
    ToolPaint {
        tool,
        stroke_width: toolbar.get_stroke_width(),
        fill_opacity: toolbar.get_fill_opacity(),
        font_size: toolbar.get_font_size(),
        block: toolbar.get_block(),
        stroke_color: toolbar.get_stroke_color(),
        fill_color: toolbar.get_fill_color(),
        stroke_on,
        fill_on,
        text: toolbar.get_text_value(),
    }
}
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
/// Hides the paint bar from another thread.
pub fn hide_paint_bar(weak: &PaintBarWeak) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = ui.hide();
        }
    });
}
/// Hides the colour picker from another thread, e.g. after the editor consumes
/// a picked colour.
pub fn hide_picker(weak: &PickerWeak) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = ui.hide();
        }
    });
}
/// Writes a paint snapshot and the two tool lists into the toolbar from another
/// thread, so a change made in the settings or the options panel is reflected on
/// the floating bar.
pub fn set_toolbar_paint(
    weak: &ToolbarWeak,
    paint: &ToolPaint,
    stroke_tools: Vec<bool>,
    fill_tools: Vec<bool>,
) {
    let weak = weak.clone();
    let paint = paint.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_stroke_width(paint.stroke_width);
            ui.set_fill_opacity(paint.fill_opacity);
            ui.set_font_size(paint.font_size);
            ui.set_block(paint.block);
            ui.set_text_value(paint.text.clone());
            ui.set_stroke_color(paint.stroke_color);
            ui.set_fill_color(paint.fill_color);
            ui.set_stroke_tools(ModelRc::new(VecModel::from(stroke_tools)));
            ui.set_fill_tools(ModelRc::new(VecModel::from(fill_tools)));
        }
    });
}
/// Writes a paint snapshot into the options panel from another thread, so the
/// panel opens with the values of the active tool.
pub fn set_panel_paint(weak: &PanelWeak, paint: &ToolPaint) {
    let weak = weak.clone();
    let paint = paint.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_tool(paint.tool);
            ui.set_stroke_width(paint.stroke_width);
            ui.set_fill_opacity(paint.fill_opacity);
            ui.set_font_size(paint.font_size);
            ui.set_block(paint.block);
            ui.set_text_value(paint.text.clone());
            ui.set_stroke_color(paint.stroke_color);
            ui.set_fill_color(paint.fill_color);
            ui.set_stroke_on(paint.stroke_on);
            ui.set_fill_on(paint.fill_on);
        }
    });
}
/// Writes a paint snapshot into the paint bar from another thread. Same fields
/// as `set_panel_paint`, kept separate because the two windows are distinct
/// component types.
pub fn set_paint_bar(weak: &PaintBarWeak, paint: &ToolPaint) {
    let weak = weak.clone();
    let paint = paint.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_tool(paint.tool);
            ui.set_stroke_width(paint.stroke_width);
            ui.set_fill_opacity(paint.fill_opacity);
            ui.set_font_size(paint.font_size);
            ui.set_block(paint.block);
            ui.set_text_value(paint.text.clone());
            ui.set_stroke_color(paint.stroke_color);
            ui.set_fill_color(paint.fill_color);
            ui.set_stroke_on(paint.stroke_on);
            ui.set_fill_on(paint.fill_on);
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
/// Puts the toolbar into long-capture mode, or takes it out again: the drawing
/// tools, the history and the tool options step aside while the four action
/// buttons stay live, and the pill stops being draggable.
pub fn set_toolbar_long_capture(weak: &ToolbarWeak, active: bool) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_long_capture(active);
        }
    });
}
/// Selects a tool in the toolbar, so the highlight follows a tool the canvas
/// switched to on its own (the Space shortcut returning to drawing).
pub fn set_toolbar_active(weak: &ToolbarWeak, tool: i32) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_active(tool);
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

/// Converts sRGB components to a Slint colour. Alpha is kept so a fill colour
/// round-trips without losing its opacity.
pub fn slint_color(rgba: [f32; 4]) -> slint::Color {
    let byte = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    slint::Color::from_argb_u8(byte(rgba[3]), byte(rgba[0]), byte(rgba[1]), byte(rgba[2]))
}
/// Converts a Slint colour back to sRGB components.
pub fn rgba_of(color: slint::Color) -> [f32; 4] {
    [
        f32::from(color.red()) / 255.0,
        f32::from(color.green()) / 255.0,
        f32::from(color.blue()) / 255.0,
        f32::from(color.alpha()) / 255.0,
    ]
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
            (758, 52)
        );
        assert_eq!(
            scale_to_physical(toolbar_logical_size(&Config::default().tools), 144),
            (1137, 78)
        );
        // The paint rows are sized per tool: the text tool is the widest, the two
        // shape groups come next and a single group is the shortest.
        assert_eq!(scale_to_physical(paint_logical_size(6), 96), (780, 52));
        assert_eq!(scale_to_physical(paint_logical_size(6), 144), (1170, 78));
        assert_eq!(scale_to_physical(paint_logical_size(1), 96), (480, 52));
        assert_eq!(scale_to_physical(paint_logical_size(5), 96), (255, 52));
        assert_eq!(scale_to_physical(paint_bar_logical_size(6), 96), (730, 52));
        assert_eq!(scale_to_physical(paint_bar_logical_size(1), 96), (430, 52));
        // The panel is the row plus its close button, the bar is the row alone.
        assert_eq!(paint_logical_size(4).0 - paint_bar_logical_size(4).0, 50);
        assert_eq!(
            scale_to_physical(toolbar_logical_size(&Config::default().tools), 192),
            (1516, 104)
        );
        // Rounded to the nearest physical pixel.
        assert_eq!(scale_to_physical((1, 1), 144), (2, 2));
    }
    #[test]
    fn long_capture_sits_left_of_the_pin_command() {
        let actions = toolbar_actions();
        let pin = actions
            .iter()
            .position(|action| action.id == 14)
            .expect("pin command");
        let long_capture = &actions[pin - 1];
        assert_eq!(long_capture.id, 17);
        assert_eq!(long_capture.glyph, 7);
        assert_eq!(long_capture.label, "长截图");
        // Command ids stay unique, so no button shadows another command.
        let mut ids: Vec<i32> = actions.iter().map(|action| action.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count);
        // A long capture keeps exactly the four action buttons live: save, pin,
        // close and copy, in display order.
        let live: Vec<i32> = actions
            .iter()
            .filter(|action| action.capture)
            .map(|action| action.id)
            .collect();
        assert_eq!(live, vec![14, 13, 15, 12]);
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
    #[test]
    fn recorded_hotkeys_match_the_stored_spelling() {
        // Letters normalize regardless of the shifted text the platform reports.
        assert_eq!(
            captured_hotkey("a", true, true, false, false, false),
            "Ctrl+Shift+A"
        );
        assert_eq!(
            captured_hotkey("7", true, false, false, false, false),
            "Ctrl+7"
        );
        // F12 arrives as the private Slint key code for F12.
        assert_eq!(
            captured_hotkey("\u{F70F}", false, false, true, true, false),
            "Alt+Win+F12"
        );
        // A registered hotkey needs a modifier and a key.
        assert_eq!(captured_hotkey("a", false, false, false, false, false), "");
        assert_eq!(
            captured_hotkey("\u{0011}", true, false, false, false, false),
            ""
        );
        // The held interaction key also accepts a bare key or bare modifiers.
        assert_eq!(captured_hotkey("a", false, false, false, false, true), "A");
        assert_eq!(
            captured_hotkey("\u{0011}", true, false, false, false, true),
            "Ctrl"
        );
        assert_eq!(
            captured_hotkey("\u{0010}", true, true, false, false, true),
            "Ctrl+Shift"
        );
        // Keys the configuration cannot store never become a draft, not even
        // in the held-key row.
        assert_eq!(captured_hotkey(" ", true, false, false, false, true), "");
        assert_eq!(
            captured_hotkey("\u{001b}", false, false, false, false, true),
            ""
        );
    }
    #[test]
    fn slint_colour_round_trips_within_a_byte() {
        for rgba in [
            [0.0, 0.0, 0.0, 1.0],
            [1.0, 1.0, 1.0, 1.0],
            [0.95, 0.2, 0.25, 1.0],
            [0.0, 0.5, 1.0, 0.25],
        ] {
            let back = rgba_of(slint_color(rgba));
            for (expected, actual) in rgba.iter().zip(back.iter()) {
                assert!(
                    (expected - actual).abs() <= 1.0 / 255.0,
                    "{rgba:?} -> {back:?}"
                );
            }
        }
    }
    #[test]
    fn paint_rows_round_trip_through_the_settings_model() {
        let config = Config::default();
        let rows = ModelRc::new(VecModel::from(tool_paint_rows(&config)));
        let (stroke, fill) = paint_tools(&rows);
        assert_eq!(stroke, config.stroke_tools);
        assert_eq!(fill, config.fill_tools);
        let rows = tool_paint_rows(&config);
        // Mosaic cannot be outlined and the arrow cannot be filled, so their
        // rows stay false even if a configuration listed them.
        assert!(!rows.iter().find(|row| row.tool == 5).unwrap().stroke);
        assert!(!rows.iter().find(|row| row.tool == 3).unwrap().fill);
        assert!(rows.iter().find(|row| row.tool == 6).unwrap().fill);
    }
}

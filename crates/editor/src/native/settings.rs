//! 编辑器设置会话：把 `--settings` 启动的进程变成 Slint 设置页。
//!
//! 这里只负责设置窗口本身：目录选择、截图列表、快捷键与颜色的校验和写盘，
//! 保存后通知贴图进程重新读取配置。它不接触截图画布的高频绘制，也不拥有
//! `Canvas`、`UiSlots` 等父模块私有状态；与浮窗相关的原生行为只通过
//! `super::tool_windows` 的公开接口协作。

use snaplight_domain::{
    format_hex_color, parse_held_shortcut, parse_hex_color, parse_hotkey, Config, Theme, Tool,
    MAX_FONT_SIZE, MAX_PIN_SHADOW_BLUR, MAX_PIN_SHADOW_OFFSET, MIN_FONT_SIZE,
};
use snaplight_platform::{capture_dir, data_dir, load_window_state, save_config};
use snaplight_ui::{ComponentHandle, ModelRc, VecModel};
use std::path::PathBuf;
use windows::{
    core::{w, Error, HSTRING, PCWSTR},
    Win32::{
        Foundation::{CloseHandle, HWND},
        UI::WindowsAndMessaging::{FindWindowW, SW_SHOWNORMAL},
    },
};

use super::tool_windows::{apply_icon_when_ready, remember_position, restore_position};

/// Directory the settings page acts on: the typed folder, or the default one
/// when the field is empty.
pub(super) fn capture_dir_from_field(value: &str) -> PathBuf {
    let value = value.trim();
    if value.is_empty() {
        data_dir().join("captures")
    } else {
        PathBuf::from(value)
    }
}
/// Builds the capture list rows: local time, saved kind, and total size.
pub(super) fn capture_items(dir: &std::path::Path) -> Vec<snaplight_ui::CaptureItem> {
    snaplight_platform::list_captures(dir)
        .into_iter()
        .map(|entry| snaplight_ui::CaptureItem {
            label: format!(
                "{} · {} · {}",
                entry
                    .modified
                    .map(snaplight_platform::native::format_local_time)
                    .unwrap_or_else(|| entry.id.clone()),
                if entry.png { "PNG" } else { "BGRA" },
                human_bytes(entry.bytes)
            )
            .into(),
            id: entry.id.into(),
        })
        .collect()
}
fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if value >= KIB * KIB {
        format!("{:.1} MiB", value / (KIB * KIB))
    } else if value >= KIB {
        format!("{:.0} KiB", value / KIB)
    } else {
        format!("{bytes} B")
    }
}
/// Opens a file or folder with the shell's default handler.
fn shell_open(path: &std::path::Path) -> windows::core::Result<()> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    unsafe {
        let result = ShellExecuteW(
            None,
            w!("open"),
            &HSTRING::from(path.as_os_str()),
            None,
            None,
            SW_SHOWNORMAL,
        );
        // ShellExecute returns a value greater than 32 on success.
        if result.0 as usize <= 32 {
            return Err(Error::from_win32());
        }
    }
    Ok(())
}
/// Folder picker for the capture directory. Returns `None` when the user
/// cancels or the shell dialog is unavailable.
fn pick_folder() -> Option<PathBuf> {
    use windows::Win32::{
        System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER},
        UI::Shell::{
            FileOpenDialog, IFileOpenDialog, FILEOPENDIALOGOPTIONS, FOS_FORCEFILESYSTEM,
            FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
        },
    };
    unsafe {
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        let options = FILEOPENDIALOGOPTIONS(options.0 | FOS_PICKFOLDERS.0 | FOS_FORCEFILESYSTEM.0);
        dialog.SetOptions(options).ok()?;
        // Without an owner the dialog is not modal to the Slint window, but the
        // winit backend does not expose its HWND.
        dialog.Show(HWND::default()).ok()?;
        let item = dialog.GetResult().ok()?;
        let wide = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = wide.to_string().ok()?;
        CoTaskMemFree(Some(wide.0 as *const std::ffi::c_void));
        Some(PathBuf::from(path))
    }
}
/// Clamps one shadow metric from the settings page into the range the pin
/// process sizes its window by, so a value the page cannot produce still cannot
/// leave the configuration in a form the loader rejects.
fn clamp_shadow_metric(value: i32, max: u32) -> u32 {
    value.clamp(0, max as i32) as u32
}

/// Colour labels are bold when the text differs from the loaded value, even if
/// it is only a different spelling of the same colour.
fn same_color(a: &str, b: &str) -> bool {
    match (parse_hex_color(a), parse_hex_color(b)) {
        (Some(a), Some(b)) => a == b,
        _ => a.trim().eq_ignore_ascii_case(b.trim()),
    }
}

/// Shortcut labels use the stored spelling, so `ctrl+shift+a` and
/// `Ctrl+Shift+A` are the same value.
fn same_hotkey(a: &str, b: &str) -> bool {
    let canonical = |text: &str| parse_hotkey(text.trim()).map(|key| key.canonical()).ok();
    match (canonical(a), canonical(b)) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(&b),
        _ => a.trim().eq_ignore_ascii_case(b.trim()),
    }
}

fn same_held_shortcut(a: &str, b: &str) -> bool {
    let canonical = |text: &str| {
        parse_held_shortcut(text.trim())
            .ok()
            .flatten()
            .map(|key| key.canonical())
    };
    match (canonical(a), canonical(b)) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(&b),
        _ => a.trim().eq_ignore_ascii_case(b.trim()),
    }
}

/// Membership only: the paint toggles cannot reorder the lists.
fn tool_membership(tools: &[Tool]) -> Vec<i32> {
    let mut ids: Vec<i32> = tools.iter().map(|tool| tool.id()).collect();
    ids.sort_unstable();
    ids
}

/// Refreshes the bold markers. The baseline is the built-in default
/// configuration, so a customised value stays emphasised across sessions until
/// it is reset to its default; the markers are computed on open as well as
/// after every save.
fn refresh_changed_markers(ui: &snaplight_ui::Settings, defaults: &Config) {
    ui.set_changed_gpu_pins(ui.get_gpu_pins() != defaults.gpu_pins);
    ui.set_changed_pin_close_confirm(ui.get_pin_close_confirm() != defaults.pin_close_confirm);
    ui.set_changed_tool_order(
        snaplight_ui::tool_order(&ui.get_tools())
            != defaults
                .tools
                .iter()
                .map(|tool| tool.id())
                .collect::<Vec<_>>(),
    );
    ui.set_changed_background(same_color(
        &ui.get_background_value(),
        &defaults.theme.background,
    ));
    ui.set_changed_foreground(same_color(
        &ui.get_foreground_value(),
        &defaults.theme.foreground,
    ));
    ui.set_changed_accent(same_color(&ui.get_accent_value(), &defaults.theme.accent));
    ui.set_changed_pin_shadow(ui.get_pin_shadow() != defaults.pin_shadow);
    ui.set_changed_shadow_blur(
        ui.get_pin_shadow_blur() != defaults.pin_shadow_blur.min(MAX_PIN_SHADOW_BLUR) as i32,
    );
    ui.set_changed_shadow_offset(
        ui.get_pin_shadow_offset() != defaults.pin_shadow_offset.min(MAX_PIN_SHADOW_OFFSET) as i32,
    );
    ui.set_changed_shadow_opacity(
        ui.get_pin_shadow_opacity() != defaults.pin_shadow_opacity.min(100) as i32,
    );
    ui.set_changed_capture_hotkey(!same_hotkey(
        &ui.get_capture_hotkey_value(),
        &defaults.capture_hotkey,
    ));
    ui.set_changed_restore_hotkey(!same_hotkey(
        &ui.get_restore_hotkey_value(),
        &defaults.restore_hotkey,
    ));
    ui.set_changed_through_hotkey(!same_held_shortcut(
        &ui.get_through_hotkey_value(),
        &defaults.through_hotkey,
    ));
    ui.set_changed_stroke(ui.get_stroke() != defaults.stroke_width as i32);
    ui.set_changed_block(ui.get_block() != defaults.mosaic_block as i32);
    ui.set_changed_stroke_color(same_color(
        &ui.get_stroke_color_value(),
        &defaults.stroke_color,
    ));
    ui.set_changed_fill_color(same_color(&ui.get_fill_color_value(), &defaults.fill_color));
    ui.set_changed_fill_opacity(ui.get_fill_opacity() != defaults.fill_opacity as i32);
    ui.set_changed_font_size(ui.get_font_size() != defaults.font_size as i32);
    let (stroke, fill) = snaplight_ui::paint_tools(&ui.get_paint_tools());
    ui.set_changed_paint_tools(
        tool_membership(&stroke) != tool_membership(&defaults.stroke_tools)
            || tool_membership(&fill) != tool_membership(&defaults.fill_tools),
    );
    ui.set_changed_detect_mode(
        snaplight_ui::window_detect_from_index(ui.get_detect_mode()) != defaults.window_detect,
    );
    ui.set_changed_capture_dir(
        capture_dir_from_field(&ui.get_capture_dir_value()) != capture_dir(defaults),
    );
}

/// Points at the settings page that is already open: the page is brought to the
/// front and shaken for a moment. The open instance does not publish its window
/// handle, so the page is found by the title that `SETTINGS_TITLE` keeps in step
/// with the Slint layout. The short retry covers the moment before the first
/// window has been created.
fn alert_open_settings() {
    let title: Vec<u16> = snaplight_ui::SETTINGS_TITLE
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        for _ in 0..40 {
            let hwnd = match FindWindowW(None, PCWSTR(title.as_ptr())) {
                Ok(hwnd) if !hwnd.0.is_null() => hwnd,
                _ => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                    continue;
                }
            };
            snaplight_platform::native::alert_window(
                hwnd,
                snaplight_platform::native::ALERT_DURATION,
            );
            return;
        }
    }
}

pub(super) fn run(config: Config) -> std::result::Result<(), Box<dyn std::error::Error>> {
    // Only one settings page may exist. The core already refuses to spawn a
    // second one, but scripts and manual runs start the editor directly; those
    // bring the open page forward and shake it instead of stacking another copy
    // of the fixed layout.
    let Some(instance) = snaplight_platform::native::singleton("Local\\Snaplight.Settings.v1")?
    else {
        // Two overlapping shakes would restore different origins, so only one
        // alert may run at a time; the name is shared with the core's tray path.
        let Some(guard) =
            snaplight_platform::native::singleton("Local\\Snaplight.Settings.Alert.v1")?
        else {
            return Ok(());
        };
        alert_open_settings();
        let _ = unsafe { CloseHandle(guard) };
        return Ok(());
    };
    let ui = snaplight_ui::settings(&config)?;
    restore_position(ui.window(), load_window_state().settings);
    ui.set_capture_dir_value(capture_dir(&config).to_string_lossy().into_owned().into());
    ui.set_config_path(
        snaplight_platform::data_dir()
            .join("config.json")
            .to_string_lossy()
            .into_owned()
            .into(),
    );
    ui.set_data_dir(
        snaplight_platform::data_dir()
            .to_string_lossy()
            .into_owned()
            .into(),
    );
    ui.set_captures(ModelRc::new(VecModel::from(capture_items(
        &capture_dir_from_field(&ui.get_capture_dir_value()),
    ))));
    // Customised values are bold from the moment the page opens, not only
    // after the first edit in this window.
    refresh_changed_markers(&ui, &Config::default());
    let weak = ui.as_weak();
    ui.on_open_data_dir(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let folder = snaplight_platform::data_dir();
        let _ = std::fs::create_dir_all(&folder);
        if let Err(e) = shell_open(&folder) {
            ui.set_status(e.to_string().into());
        }
    });
    let weak = ui.as_weak();
    ui.on_open_capture_folder(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let folder = capture_dir_from_field(&ui.get_capture_dir_value());
        let _ = std::fs::create_dir_all(&folder);
        if let Err(e) = shell_open(&folder) {
            ui.set_status(e.to_string().into());
        }
    });
    let weak = ui.as_weak();
    ui.on_open_capture(move |id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let folder = capture_dir_from_field(&ui.get_capture_dir_value());
        let png = folder.join(format!("{id}.png"));
        let path = if png.exists() {
            png
        } else {
            folder.join(format!("{id}.bgra"))
        };
        if let Err(e) = shell_open(&path) {
            ui.set_status(e.to_string().into());
        }
    });
    let weak = ui.as_weak();
    ui.on_delete_capture(move |id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        // Deleting needs a second click, so a stray click cannot remove a capture.
        if ui.get_pending_delete().as_str() != id.as_str() {
            ui.set_pending_delete(id.clone());
            ui.set_status(format!("再次点击「确认删除」可把 {id} 移入回收站").into());
            return;
        }
        ui.set_pending_delete(Default::default());
        let folder = capture_dir_from_field(&ui.get_capture_dir_value());
        let files = snaplight_platform::capture_files(&folder, &id);
        let removed = files.len();
        match snaplight_platform::native::delete_to_recycle_bin(&files) {
            Ok(()) => {
                ui.set_captures(ModelRc::new(VecModel::from(capture_items(&folder))));
                ui.set_status(format!("已把 {removed} 个文件移入回收站").into());
            }
            Err(e) => ui.set_status(e.to_string().into()),
        }
    });
    let weak = ui.as_weak();
    ui.on_move_tool(move |index, delta| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if let Some(items) = snaplight_ui::move_tool(&ui.get_tools(), index, delta) {
            ui.set_tools(ModelRc::new(VecModel::from(items)));
        }
    });
    // The per-tool paint switches are edited one cell at a time; the model is
    // rebuilt from the resulting configuration so the row shows its new state.
    let rows_source = config.clone();
    let weak = ui.as_weak();
    ui.on_paint_toggled(move |tool, slot, on| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let Some(tool) = Tool::from_id(tool) else {
            return;
        };
        let (mut stroke, mut fill) = snaplight_ui::paint_tools(&ui.get_paint_tools());
        let list = if slot == 1 { &mut fill } else { &mut stroke };
        list.retain(|listed| *listed != tool);
        if on {
            list.push(tool);
        }
        let mut edited = rows_source.clone();
        edited.stroke_tools = stroke;
        edited.fill_tools = fill;
        ui.set_paint_tools(ModelRc::new(VecModel::from(snaplight_ui::tool_paint_rows(
            &edited,
        ))));
    });
    let weak = ui.as_weak();
    ui.on_browse_capture_dir(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if let Some(folder) = pick_folder() {
            ui.set_capture_dir_value(folder.to_string_lossy().into_owned().into());
        }
    });
    let weak = ui.as_weak();
    ui.on_save(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        // Bold marks values that differ from the built-in defaults, including
        // edits that fail validation below; the status line explains a failure.
        refresh_changed_markers(&ui, &Config::default());
        let mut c = config.clone();
        c.stroke_width = ui.get_stroke() as f32;
        c.mosaic_block = ui.get_block() as u32;
        c.gpu_pins = ui.get_gpu_pins();
        // The pin prompt follows this switch, and it also turns the switch off
        // when the user ticks "don't ask again" there.
        c.pin_close_confirm = ui.get_pin_close_confirm();
        c.pin_shadow = ui.get_pin_shadow();
        c.pin_shadow_blur = clamp_shadow_metric(ui.get_pin_shadow_blur(), MAX_PIN_SHADOW_BLUR);
        c.pin_shadow_offset =
            clamp_shadow_metric(ui.get_pin_shadow_offset(), MAX_PIN_SHADOW_OFFSET);
        c.pin_shadow_opacity = clamp_shadow_metric(ui.get_pin_shadow_opacity(), 100) as u8;
        c.fill_opacity = ui.get_fill_opacity().clamp(0, 100) as u8;
        c.font_size = ui
            .get_font_size()
            .clamp(MIN_FONT_SIZE as i32, MAX_FONT_SIZE as i32) as u16;
        // Colour text is normalized before it is written, so a hand-typed value
        // never leaves the configuration in a form the loader rejects.
        for (text, target) in [
            (ui.get_stroke_color_value().to_string(), &mut c.stroke_color),
            (ui.get_fill_color_value().to_string(), &mut c.fill_color),
        ] {
            let Some(rgba) = parse_hex_color(&text) else {
                ui.set_status("颜色需要写成 #RRGGBB，未保存任何更改".into());
                return;
            };
            *target = format_hex_color(rgba);
        }
        let (stroke_tools, fill_tools) = snaplight_ui::paint_tools(&ui.get_paint_tools());
        c.stroke_tools = stroke_tools;
        c.fill_tools = fill_tools;
        c.theme = Theme {
            background: ui.get_background_value().into(),
            foreground: ui.get_foreground_value().into(),
            accent: ui.get_accent_value().into(),
        };
        snaplight_ui::apply_tool_order(&mut c, &ui.get_tools());
        c.window_detect = snaplight_ui::window_detect_from_index(ui.get_detect_mode());
        // Normalize and validate the shortcuts before anything is written.
        let hotkeys = (|| -> std::result::Result<(), &'static str> {
            let capture = parse_hotkey(ui.get_capture_hotkey_value().trim())
                .map_err(|_| "截图快捷键格式无效")?
                .canonical();
            let restore = parse_hotkey(ui.get_restore_hotkey_value().trim())
                .map_err(|_| "恢复贴图快捷键格式无效")?
                .canonical();
            // The held shortcut is not registered with the shell, so it accepts
            // a lone modifier or a lone key as well.
            let through = parse_held_shortcut(ui.get_through_hotkey_value().trim())
                .map_err(|_| "穿透临时交互按键格式无效")?
                .ok_or("穿透临时交互按键不能为空")?
                .canonical();
            ui.set_capture_hotkey_value(capture.clone().into());
            ui.set_restore_hotkey_value(restore.clone().into());
            ui.set_through_hotkey_value(through.clone().into());
            c.capture_hotkey = capture;
            c.restore_hotkey = restore;
            c.through_hotkey = through;
            Ok(())
        })();
        if let Err(message) = hotkeys {
            ui.set_status(format!("{message}，未保存任何更改").into());
            return;
        }
        let folder = ui.get_capture_dir_value().trim().to_string();
        c.capture_dir = (!folder.is_empty()).then(|| folder.clone());
        let saved = (|| -> std::result::Result<(), Box<dyn std::error::Error>> {
            if !folder.is_empty() {
                // Fail loudly instead of saving a directory that cannot be created.
                std::fs::create_dir_all(&folder)?;
            }
            save_config(&c)?;
            Ok(())
        })();
        match saved {
            Ok(()) => {
                // The list follows the folder that was just saved.
                ui.set_captures(ModelRc::new(VecModel::from(capture_items(
                    &capture_dir_from_field(&ui.get_capture_dir_value()),
                ))));
                // The core re-registers the capture shortcut and forwards a
                // reload to the pin process, so neither process has to be
                // restarted for a shortcut or shadow change to apply.
                snaplight_platform::native::notify_config_reload();
                ui.set_status("已自动保存".into());
            }
            Err(e) => ui.set_status(e.to_string().into()),
        }
    });
    // The settings window keeps its system frame, so it needs the icon itself.
    apply_icon_when_ready(snaplight_ui::SETTINGS_TITLE);
    ui.run()?;
    remember_position(true, ui.window());
    // The name only has to live as long as the page does; releasing it here
    // makes the next launch open a window immediately.
    let _ = unsafe { CloseHandle(instance) };
    Ok(())
}

use parking_lot::Mutex;
use snaplight_domain::*;
use snaplight_platform::{atomic_write, capture_dir, data_dir, load_config, load_window_state};
use snaplight_ui::{
    ColorPicker, ComponentHandle, ModelRc, OptionsPanel, PaintBar, PhysicalPosition, Toolbar,
    VecModel,
};
use std::sync::Arc;
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        System::{Com::*, LibraryLoader::*},
        UI::WindowsAndMessaging::*,
    },
};

/// The editor session: the overlay canvas window, its interaction state and the
/// thread that owns them. Only the entry point, the session inputs and the ready
/// report leave the module; the canvas itself stays private.
mod canvas_session;
mod output;
/// Pinned image editing: re-opening the file group the pin process reads.
mod pin_edit;
mod selection;
/// The settings session: the `--settings` page, its callbacks and the save path.
mod settings;
mod smoke;
/// Native window helpers for the floating tool windows: placement, rounding,
/// the frameless subclass, and the app icon.
mod tool_windows;
mod ui_bridge;
use canvas_session::{canvas_thread, parse_long_capture_demo, EditorSession, COMMAND};
use output::write_rgba_png;
use pin_edit::PinEdit;
use settings::{capture_dir_from_field, capture_items};
use smoke::{overlay_stand_in_proc, row_extent, z_order, FILL};
use tool_windows::{
    find_process_window, initial_toolbar_position, monitor_work_area, options_position,
    parked_position, pin_floating_positions, raise_floating_windows, remember_position,
    reshape_and_raise, reshape_when_sized, tool_window_style,
};
use ui_bridge::{UiCommand, UiSlots};

pub fn run() -> std::result::Result<(), Box<dyn std::error::Error>> {
    unsafe {
        // The settings folder picker needs a COM apartment on this thread; Slint
        // and the shell tolerate an already-initialized apartment.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    snaplight_platform::native::dpi_aware();
    let config = load_config()?;
    if std::env::args().any(|a| a == "--settings") {
        return settings::run(config);
    }
    let stress = std::env::args().any(|a| a == "--stress");
    let smoke = std::env::args().any(|a| a == "--smoke") || stress;
    // `--long-capture-demo=x,y,w,h` runs one real long capture session against a
    // screen region and prints its result, so scrolling and stitching can be
    // verified end to end against a scrollable window without a human clicking.
    let demo = std::env::args().find_map(|a| parse_long_capture_demo(&a));
    // `--edit-pin=<image>` re-opens a pinned image for annotation inside a window
    // over the pin itself; `--edit-pin-demo` does the same without a user.
    let pin_edit = PinEdit::from_args()?;
    // The demos run without the Slint toolbar: they drive the canvas directly and
    // exit when they are done.
    let interactive =
        !smoke && demo.is_none() && !pin_edit.as_ref().is_some_and(|edit| edit.is_demo());
    // A pinned image session keeps its own toolbar placement, around the pin in
    // the monitor work area, and must not hand that position on to the next
    // capture session.
    let pin_session = pin_edit.is_some();
    let queue = Arc::new(Mutex::new(vec![]));
    let thread_queue = queue.clone();
    let ui_slots = Arc::new(UiSlots::default());
    let thread_slots = ui_slots.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let result = canvas_thread(
            config,
            thread_queue,
            tx.clone(),
            thread_slots,
            EditorSession {
                smoke,
                stress,
                long_capture: demo,
                pin_edit,
            },
        );
        if let Err(e) = &result {
            let _ = tx.send(Err(e.clone()));
        }
        if interactive {
            snaplight_ui::invoke(snaplight_ui::quit);
        }
        result
    });
    let ready = rx.recv()??;
    let id = ready.thread_id;
    if interactive {
        let config = load_config()?;
        let ui: Toolbar = snaplight_ui::toolbar(&config)?;
        let _ = ui_slots.bind_toolbar(ui.as_weak());
        let _ = ui_slots.bind_toolbar_size(snaplight_ui::toolbar_logical_size(&config.tools));
        let size = snaplight_ui::scale_to_physical(
            snaplight_ui::toolbar_logical_size(&config.tools),
            ready.dpi,
        );
        let parked = parked_position(size);
        // A pinned image session has no selection to follow, so its toolbar is
        // placed as part of the floating pair around the pin; a capture session
        // waits off-screen until the selection places it.
        let start = if pin_session {
            // Both floating windows are placed as a pair around the pin, so the
            // toolbar never lands on the paint bar. The default rectangle row is
            // the width the session starts its paint bar at.
            let paint = snaplight_ui::scale_to_physical(
                snaplight_ui::paint_bar_logical_size(Tool::Rectangle.id()),
                ready.dpi,
            );
            pin_floating_positions(ready.bounds, size, paint, monitor_work_area(ready.bounds)).0
        } else {
            parked
        };
        ui.window()
            .set_position(PhysicalPosition::new(start.0, start.1));
        ui.show()?;
        // The window has its final size once it is shown, so the pill shape and
        // the tool window style are applied now and never again.
        reshape_and_raise(
            snaplight_ui::TOOLBAR_TITLE,
            snaplight_ui::toolbar_logical_size(&config.tools),
        );
        let panel: OptionsPanel = snaplight_ui::options_panel(&config)?;
        let _ = ui_slots.bind_panel(panel.as_weak());
        // Before a tool is known, the row is the width of the shape tools; it is
        // resized when the panel or the bar opens for a tool.
        let default_size = snaplight_ui::paint_logical_size(Tool::Rectangle.id());
        reshape_and_raise(snaplight_ui::OPTIONS_TITLE, default_size);
        let bar: PaintBar = snaplight_ui::paint_bar(&config)?;
        let _ = ui_slots.bind_paint_bar(bar.as_weak());
        reshape_and_raise(snaplight_ui::PAINT_BAR_TITLE, default_size);
        let picker: ColorPicker = snaplight_ui::color_picker()?;
        let _ = ui_slots.bind_picker(picker.as_weak());
        reshape_and_raise(
            snaplight_ui::PICKER_TITLE,
            snaplight_ui::PICKER_LOGICAL_SIZE,
        );
        // The paint bar and the picker start hidden; the canvas shows them when a
        // drawing tool is selected and a colour is picked.
        snaplight_ui::hide_paint_bar(&bar.as_weak());
        snaplight_ui::hide_picker(&picker.as_weak());
        // Both paint surfaces edit the same style, so they send the same
        // message and the canvas answers with the values it will draw with.
        let push_paint = {
            let q = queue.clone();
            move |paint: snaplight_ui::ToolPaint| {
                q.lock().push(UiCommand::Paint(paint));
                unsafe {
                    let _ = PostThreadMessageW(id, COMMAND, WPARAM(0), LPARAM(0));
                }
            }
        };
        panel.on_properties_changed(push_paint.clone());
        bar.on_properties_changed(push_paint.clone());
        // Quick settings and the bottom bar open the colour picker for one of the
        // two colours; every pick is applied as it happens, so there is no
        // separate apply step to remember.
        let picker_target = Arc::new(Mutex::new(0i32));
        let open_picker = {
            let picker = picker.as_weak();
            let panel = panel.as_weak();
            let target = picker_target.clone();
            let dpi = ready.dpi;
            let bounds = ready.bounds;
            move |which: i32, anchor: PhysicalPosition, anchor_height: i32| {
                let (Some(picker), Some(panel)) = (picker.upgrade(), panel.upgrade()) else {
                    return;
                };
                *target.lock() = which;
                // The picker opens on the colour that window is already showing.
                let color = if which == 1 {
                    panel.get_fill_color()
                } else {
                    panel.get_stroke_color()
                };
                snaplight_ui::picker_set_color(&picker, color);
                picker.set_target_label(if which == 1 { "填充色" } else { "描边色" }.into());
                let size = snaplight_ui::scale_to_physical(snaplight_ui::PICKER_LOGICAL_SIZE, dpi);
                let (x, y) = options_position(bounds, anchor.x, anchor.y, anchor_height, size);
                picker.window().set_position(PhysicalPosition::new(x, y));
                let _ = picker.show();
                reshape_and_raise(
                    snaplight_ui::PICKER_TITLE,
                    snaplight_ui::PICKER_LOGICAL_SIZE,
                );
            }
        };
        let apply_picked = {
            let panel = panel.as_weak();
            let bar = bar.as_weak();
            let target = picker_target.clone();
            let push = push_paint.clone();
            move |color: snaplight_ui::Color| {
                let (Some(panel), Some(bar)) = (panel.upgrade(), bar.upgrade()) else {
                    return;
                };
                // The other values come from the panel, which the canvas keeps
                // current, so a pick cannot undo a change made meanwhile.
                let mut paint = panel.get_paint();
                if *target.lock() == 1 {
                    paint.fill_color = color;
                    panel.set_fill_color(color);
                    bar.set_fill_color(color);
                } else {
                    paint.stroke_color = color;
                    panel.set_stroke_color(color);
                    bar.set_stroke_color(color);
                }
                push(paint);
            }
        };
        let panel_pick = open_picker.clone();
        let anchor = panel.as_weak();
        panel.on_pick_color(move |which| {
            if let Some(panel) = anchor.upgrade() {
                let position = panel.window().position();
                panel_pick(which, position, panel.window().size().height as i32);
            }
        });
        let bar_pick = open_picker.clone();
        let anchor = bar.as_weak();
        bar.on_pick_color(move |which| {
            if let Some(bar) = anchor.upgrade() {
                let position = bar.window().position();
                bar_pick(which, position, bar.window().size().height as i32);
            }
        });
        let picker_weak = picker.as_weak();
        let apply = apply_picked.clone();
        picker.on_hue_picked(move |hue| {
            if let Some(picker) = picker_weak.upgrade() {
                let color = snaplight_ui::picker_set_hue(&picker, hue);
                apply(color);
            }
        });
        let picker_weak = picker.as_weak();
        let apply = apply_picked.clone();
        picker.on_sv_picked(move |saturation, brightness| {
            if let Some(picker) = picker_weak.upgrade() {
                let color = snaplight_ui::picker_set_sv(&picker, saturation, brightness);
                apply(color);
            }
        });
        let picker_weak = picker.as_weak();
        let apply = apply_picked.clone();
        picker.on_hex_edited(move |text| {
            if let Some(picker) = picker_weak.upgrade() {
                // Unparsable text is left in place so it can be corrected; the
                // last usable colour stays applied.
                if let Some(color) = snaplight_ui::picker_set_hex(&picker, text.as_str()) {
                    apply(color);
                }
            }
        });
        let picker_weak = picker.as_weak();
        let apply = apply_picked.clone();
        picker.on_preset_picked(move |preset| {
            if let Some(picker) = picker_weak.upgrade() {
                let color = snaplight_ui::picker_set_color(&picker, preset);
                apply(color);
            }
        });
        let picker_handles = picker.as_weak();
        let dismissed = panel.as_weak();
        panel.on_dismissed(move || {
            if let Some(panel) = dismissed.upgrade() {
                let _ = panel.hide();
            }
            if let Some(picker) = picker_handles.upgrade() {
                let _ = picker.hide();
            }
        });
        let toolbar_ui = ui.as_weak();
        let panel_ui = panel.as_weak();
        let panel_dpi = ready.dpi;
        let bounds = ready.bounds;
        ui.on_options_toggled(move |tool| {
            let (Some(toolbar), Some(panel)) = (toolbar_ui.upgrade(), panel_ui.upgrade()) else {
                return;
            };
            // The select tool has nothing to configure.
            if !(1..=6).contains(&tool) {
                return;
            }
            let open = panel.window().is_visible() && panel.get_tool() == tool;
            if open {
                let _ = panel.hide();
                return;
            }
            // The panel opens with the values the canvas is drawing with; the
            // toolbar mirrors them because it is the only window the canvas can
            // reach from the render thread without blocking.
            snaplight_ui::set_panel_paint(
                &panel.as_weak(),
                &snaplight_ui::toolbar_paint(&toolbar, tool),
            );
            let logical = snaplight_ui::paint_logical_size(tool);
            let panel_size = snaplight_ui::scale_to_physical(logical, panel_dpi);
            let bar_size = snaplight_ui::scale_to_physical(
                snaplight_ui::toolbar_logical_size(&config.tools),
                panel_dpi,
            );
            let position = toolbar.window().position();
            let (x, y) = options_position(bounds, position.x, position.y, bar_size.1, panel_size);
            panel.set_row_width(logical.0 as f32);
            panel.window().set_position(PhysicalPosition::new(x, y));
            let _ = panel.show();
            reshape_when_sized(snaplight_ui::OPTIONS_TITLE, logical, panel_dpi);
        });
        let q = queue.clone();
        ui.on_command(move |action| {
            q.lock().push(UiCommand::Action(action));
            unsafe {
                let _ = PostThreadMessageW(id, COMMAND, WPARAM(0), LPARAM(0));
            }
        });
        // The panel is created hidden and only shown when a tool asks for it.
        snaplight_ui::hide_panel(&panel.as_weak());
        // A pinned image session has no selection to place its toolbar and
        // starts on the select tool, so once every Slint window exists the
        // canvas thread is told to bring its drawing surface back. A capture
        // session waits, parked, for a selection instead.
        if pin_session {
            queue.lock().push(UiCommand::ResumePinEditing);
            unsafe {
                let _ = PostThreadMessageW(id, COMMAND, WPARAM(0), LPARAM(0));
            }
        }
        let result = snaplight_ui::run_event_loop();
        // A parked toolbar was never placed by a selection, so its position is not
        // worth remembering — and neither is a pinned image session's, which sits
        // around the pin rather than where a capture toolbar belongs.
        let position = ui.window().position();
        if !pin_session && (position.x, position.y) != parked {
            remember_position(false, ui.window());
        }
        queue.lock().push(UiCommand::Action(15));
        unsafe {
            let _ = PostThreadMessageW(id, COMMAND, WPARAM(0), LPARAM(0));
        }
        result?;
    }
    thread.join().map_err(|_| "canvas thread panicked")??;
    if smoke && !stress {
        let config = load_config()?;
        let ui = snaplight_ui::toolbar(&config)?;
        ui.show()?;
        reshape_and_raise(
            snaplight_ui::TOOLBAR_TITLE,
            snaplight_ui::toolbar_logical_size(&config.tools),
        );
        // Without a remembered position the toolbar must be placed inside the
        // captured desktop, so it stays reachable on a full-screen capture.
        let size = ui.window().size();
        // The selection readout is published through the same helper the canvas
        // thread uses; running the loop applies it and creates the window.
        snaplight_ui::set_toolbar_status(&ui.as_weak(), "1920 × 1080".into());
        snaplight_ui::run_event_loop_for(std::time::Duration::from_millis(120));
        let physical = ui.window().size();
        let physical = (physical.width as i32, physical.height as i32);
        // The placement math uses the toolbar's logical size scaled by the
        // monitor DPI; the real window must match it.
        let expected = snaplight_ui::scale_to_physical(
            snaplight_ui::toolbar_logical_size(&config.tools),
            ready.dpi,
        );
        if physical != expected {
            return Err(format!(
                "toolbar physical size {physical:?} does not match dpi-scaled {expected:?} at {} dpi",
                ready.dpi
            )
            .into());
        }
        // Permanent regression: attribute updates must never restore a system
        // frame, even after the toolbar has been shown and rendered.
        let hwnd = find_process_window(snaplight_ui::TOOLBAR_TITLE)
            .ok_or("toolbar native window missing")?;
        unsafe {
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            SetWindowLongPtrW(
                hwnd,
                GWL_STYLE,
                style | (WS_CAPTION | WS_THICKFRAME | WS_SYSMENU).0 as isize,
            );
            let applied = GetWindowLongPtrW(hwnd, GWL_STYLE);
            if applied != tool_window_style(applied) {
                return Err("toolbar allowed system window decorations to return".into());
            }
            let mut outer = RECT::default();
            let mut client = RECT::default();
            GetWindowRect(hwnd, &mut outer)?;
            GetClientRect(hwnd, &mut client)?;
            if outer.right - outer.left != client.right - client.left
                || outer.bottom - outer.top != client.bottom - client.top
            {
                return Err("toolbar has an unexpected nonclient frame".into());
            }
        }
        atomic_write(
            &data_dir().join("toolbar-window.json"),
            &serde_json::to_vec_pretty(&serde_json::json!({
                "size_logical": [size.width, size.height],
                "size_physical": [physical.0, physical.1],
                "dpi": ready.dpi,
                "scale": ui.window().scale_factor(),
                "position": [ui.window().position().x, ui.window().position().y],
            }))?,
        )?;
        let size = physical;
        let bounds = ready.bounds;
        let (x, y) = initial_toolbar_position(bounds, size);
        ui.window().set_position(PhysicalPosition::new(x, y));
        let placed = ui.window().position();
        let inside = placed.x >= bounds.left
            && placed.y >= bounds.top
            && placed.x + size.0 <= bounds.right
            && placed.y + size.1 <= bounds.bottom;
        if !inside {
            return Err(format!(
                "toolbar placed outside the capture: {:?} {size:?} bounds {:?}",
                placed, bounds
            )
            .into());
        }
        let snapshot = ui.window().take_snapshot()?;
        write_rgba_png(
            &data_dir().join("toolbar-smoke.png"),
            snapshot.width(),
            snapshot.height(),
            snapshot.as_bytes(),
        )?;
        ui.hide()?;
        // Window position memory must survive a real save/load round trip.
        remember_position(false, ui.window());
        let position = ui.window().position();
        if load_window_state().toolbar
            != Some(ScreenPoint {
                x: position.x,
                y: position.y,
            })
        {
            return Err("toolbar position did not round-trip through ui-state.json".into());
        }
        // Image management: the settings page must list saved captures, and
        // deletion must move a capture's files to the recycle bin.
        let captures_dir = capture_dir(&config);
        std::fs::create_dir_all(&captures_dir)?;
        for id in ["1700000000000000000", "1700000001000000000"] {
            atomic_write(&captures_dir.join(format!("{id}.png")), &[0u8; 16])?;
            atomic_write(&captures_dir.join(format!("{id}.document.json")), b"{}")?;
        }
        if snaplight_platform::list_captures(&captures_dir).len() < 2 {
            return Err("capture list did not include the smoke captures".into());
        }
        let doomed = captures_dir.join("1700000000000000000.png");
        let files = snaplight_platform::capture_files(&captures_dir, "1700000000000000000");
        if files.len() != 2 {
            return Err(format!("expected 2 files for the capture, found {}", files.len()).into());
        }
        snaplight_platform::native::delete_to_recycle_bin(&files)?;
        if doomed.exists() {
            return Err("deleting a capture did not remove its files".into());
        }
        // The settings page is part of the UI surface check as well: the window
        // is a navigation pane plus one options page, so every page has to
        // render without clipping. `settings-smoke.png` keeps showing the page
        // with the capture list.
        let ui = snaplight_ui::settings(&config)?;
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
        // The per-tool paint table has to survive the round trip through the page,
        // or saving the settings would rewrite the configured tool lists.
        ui.set_paint_tools(ModelRc::new(VecModel::from(snaplight_ui::tool_paint_rows(
            &config,
        ))));
        let (stroke_tools, fill_tools) = snaplight_ui::paint_tools(&ui.get_paint_tools());
        if stroke_tools != config.stroke_tools || fill_tools != config.fill_tools {
            return Err(format!(
                "the paint table round-tripped to {stroke_tools:?} and {fill_tools:?}, expected {:?} and {:?}",
                config.stroke_tools, config.fill_tools
            )
            .into());
        }
        ui.show()?;
        for (page, slug) in snaplight_ui::SETTINGS_PAGES.iter().enumerate() {
            ui.set_page(page as i32);
            let snapshot = ui.window().take_snapshot()?;
            let bytes = snapshot.as_bytes();
            write_rgba_png(
                &data_dir().join(format!("settings-smoke-{slug}.png")),
                snapshot.width(),
                snapshot.height(),
                bytes,
            )?;
            if *slug == "capture" {
                write_rgba_png(
                    &data_dir().join("settings-smoke.png"),
                    snapshot.width(),
                    snapshot.height(),
                    bytes,
                )?;
            }
        }
        ui.hide()?;
        remember_position(true, ui.window());
        let position = ui.window().position();
        if load_window_state().settings
            != Some(ScreenPoint {
                x: position.x,
                y: position.y,
            })
        {
            return Err("settings position did not round-trip through ui-state.json".into());
        }
        // The quick tool options share the settings wording, so snapshot each
        // variant: rectangle (fill and outline), pen (outline width), mosaic
        // (block size) and text (font size plus the text field). Each variant is
        // first rendered wider than it needs, where the row lays out at its own
        // preferred size, to prove the configured width covers the row.
        const GENEROUS: u32 = 1400;
        let panel = snaplight_ui::options_panel(&config)?;
        for (tool, slug) in [(1, "rect"), (4, "pen"), (5, "mosaic"), (6, "text")] {
            let size = snaplight_ui::paint_logical_size(tool);
            panel.set_row_width(GENEROUS as f32);
            panel.set_tool(tool);
            panel.set_fill_on(tool != 4 && tool != 5);
            panel.show()?;
            // A width change reaches the native window on the next event-loop
            // turn, so the wide render has to settle before it is measured.
            snaplight_ui::run_event_loop_for(std::time::Duration::from_millis(80));
            let wide = panel.window().take_snapshot()?;
            // The snapshot and the painted extent are physical pixels while every
            // size below is logical, so the extent has to be measured at the
            // window's scale.
            let scale = panel.window().scale_factor().max(1.0);
            let (left, right) = row_extent(wide.as_bytes(), wide.width(), wide.height(), 52)
                .ok_or("the options panel painted nothing")?;
            let row = ((right - left + 1) as f32 / scale).ceil() as u32;
            // The row follows 14px of padding and is followed by the layout gap,
            // the close button and 8px of padding.
            let needed = 14 + row + 10 + 40 + 8;
            eprintln!(
                "options panel {slug}: row needs {needed} px, window {} px",
                size.0
            );
            if size.0 < needed {
                return Err(format!(
                    "the {slug} options panel is {} px wide but its row needs {needed} px",
                    size.0
                )
                .into());
            }
            // Slint applies the new width to the native window from the event
            // loop, so the review snapshot is taken after it has settled.
            panel.set_row_width(size.0 as f32);
            snaplight_ui::run_event_loop_for(std::time::Duration::from_millis(80));
            let snapshot = panel.window().take_snapshot()?;
            let bytes = snapshot.as_bytes();
            let physical = snaplight_ui::scale_to_physical(size, (scale * 96.0).round() as u32).0;
            if snapshot.width() != physical as u32 {
                return Err(format!(
                    "the {slug} options panel is {} physical pixels wide, expected {physical}",
                    snapshot.width(),
                )
                .into());
            }
            write_rgba_png(
                &data_dir().join(format!("options-smoke-{slug}.png")),
                snapshot.width(),
                snapshot.height(),
                bytes,
            )?;
            if tool == 1 {
                write_rgba_png(
                    &data_dir().join("options-smoke.png"),
                    snapshot.width(),
                    snapshot.height(),
                    bytes,
                )?;
            }
        }
        panel.hide()?;
        // The bottom paint bar stays visible while a drawing tool is selected, so
        // every row it can show has to render without clipping as well. The bar
        // carries the row alone, with no close button.
        let bar = snaplight_ui::paint_bar(&config)?;
        for (tool, slug) in [(1, "rect"), (5, "mosaic"), (6, "text")] {
            let size = snaplight_ui::paint_bar_logical_size(tool);
            bar.set_row_width(GENEROUS as f32);
            bar.set_tool(tool);
            bar.set_fill_on(tool != 5);
            bar.show()?;
            snaplight_ui::run_event_loop_for(std::time::Duration::from_millis(80));
            let wide = bar.window().take_snapshot()?;
            let scale = bar.window().scale_factor().max(1.0);
            let (left, right) = row_extent(wide.as_bytes(), wide.width(), wide.height(), 4)
                .ok_or("the paint bar painted nothing")?;
            let row = ((right - left + 1) as f32 / scale).ceil() as u32;
            // The bar has no close button: the row sits between two 14px paddings.
            let needed = 14 + row + 14;
            eprintln!(
                "paint bar {slug}: row needs {needed} px, window {} px",
                size.0
            );
            if size.0 < needed {
                return Err(format!(
                    "the {slug} paint bar is {} px wide but its row needs {needed} px",
                    size.0
                )
                .into());
            }
            bar.set_row_width(size.0 as f32);
            snaplight_ui::run_event_loop_for(std::time::Duration::from_millis(80));
            let snapshot = bar.window().take_snapshot()?;
            let bytes = snapshot.as_bytes();
            let physical = snaplight_ui::scale_to_physical(size, (scale * 96.0).round() as u32).0;
            if snapshot.width() != physical as u32 {
                return Err(format!(
                    "the {slug} paint bar is {} physical pixels wide, expected {physical}",
                    snapshot.width(),
                )
                .into());
            }
            write_rgba_png(
                &data_dir().join(format!("paint-bar-smoke-{slug}.png")),
                snapshot.width(),
                snapshot.height(),
                bytes,
            )?;
            if tool == 6 {
                write_rgba_png(
                    &data_dir().join("paint-bar-smoke.png"),
                    snapshot.width(),
                    snapshot.height(),
                    bytes,
                )?;
            }
        }
        // Slint creates its native windows from the event loop, and the place a
        // window ends up in the z-order is native state, so run the loop briefly
        // before the check below looks for the bar's window.
        snaplight_ui::run_event_loop_for(std::time::Duration::from_millis(200));
        // The capture overlay is topmost, and being activated puts it in front of
        // the other topmost windows. Raising the floating windows again has to
        // put the paint bar back in front of it, or its controls stop responding
        // after the first shape is drawn. A topmost stand-in for the overlay
        // reproduces that state without a second capture.
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = w!("Snaplight.ZOrderCheck");
            let description = WNDCLASSW {
                hInstance: instance.into(),
                lpszClassName: class,
                lpfnWndProc: Some(overlay_stand_in_proc),
                ..Default::default()
            };
            let _ = RegisterClassW(&description);
            let overlay = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                class,
                w!("capture"),
                WS_POPUP,
                0,
                0,
                200,
                100,
                None,
                None,
                instance,
                None,
            )?;
            let bar_hwnd = find_process_window(snaplight_ui::PAINT_BAR_TITLE)
                .ok_or("the paint bar window is missing")?;
            // The stand-in takes the front of the topmost band, as the activated
            // overlay does, and must be in front of the bar to be a fair test.
            SetWindowPos(
                overlay,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )?;
            let (bar_z, overlay_z) = (
                z_order(bar_hwnd).unwrap_or(usize::MAX),
                z_order(overlay).unwrap_or(usize::MAX),
            );
            if overlay_z > bar_z {
                return Err(
                    "the z-order check needs the capture stand-in in front of the paint bar".into(),
                );
            }
            raise_floating_windows();
            let (bar_z, overlay_z) = (
                z_order(bar_hwnd).unwrap_or(usize::MAX),
                z_order(overlay).unwrap_or(usize::MAX),
            );
            if bar_z > overlay_z {
                return Err(format!(
                    "the paint bar is at z-order {bar_z}, still behind the capture at {overlay_z}"
                )
                .into());
            }
            let _ = DestroyWindow(overlay);
        }
        bar.hide()?;
        // The colour picker is the only window with hand-drawn gradients, so its
        // appearance is recorded for the interface review.
        let picker = snaplight_ui::color_picker()?;
        picker.set_target_label("填充色".into());
        picker.show()?;
        let snapshot = picker.window().take_snapshot()?;
        write_rgba_png(
            &data_dir().join("picker-smoke.png"),
            snapshot.width(),
            snapshot.height(),
            snapshot.as_bytes(),
        )?;
        // A picked colour has to survive the round trip through the square, the
        // hex field and the slider, or the picker would show one colour and
        // apply another.
        let picked = snaplight_ui::picker_set_color(&picker, snaplight_ui::slint_color(FILL));
        if picker.get_color_value() != picked {
            return Err("the picker did not keep the colour it was given".into());
        }
        if picker.get_hex_text().as_str() != format_hex_color(FILL) {
            return Err(format!(
                "the picker shows {:?} instead of {}",
                picker.get_hex_text().as_str(),
                format_hex_color(FILL)
            )
            .into());
        }
        let green = snaplight_ui::slint_color([0.0, 1.0, 0.0, 1.0]);
        if snaplight_ui::picker_set_hex(&picker, "#00FF00").is_none()
            || picker.get_color_value() != green
        {
            return Err("the picker did not accept a hex colour".into());
        }
        if snaplight_ui::picker_set_hex(&picker, "not a colour").is_some()
            || picker.get_color_value() != green
        {
            return Err("invalid hex text changed the picked colour".into());
        }
        // The hue slider keeps saturation and brightness: half of each at a
        // quarter turn is the half-green tone below.
        snaplight_ui::picker_set_sv(&picker, 0.5, 0.5);
        let hue = snaplight_ui::rgba_of(snaplight_ui::picker_set_hue(&picker, 0.25));
        if (hue[0] - 0.375).abs() > 0.01
            || (hue[1] - 0.5).abs() > 0.01
            || (hue[2] - 0.25).abs() > 0.01
        {
            return Err(format!(
                "a quarter turn at half saturation and brightness is {hue:?}, expected [0.375, 0.5, 0.25]"
            )
            .into());
        }
        picker.hide()?;
    }
    Ok(())
}

use parking_lot::Mutex;
use snaplight_domain::*;
use snaplight_platform::{atomic_write, capture_dir, data_dir, load_config, load_window_state};
use snaplight_ui::{
    ColorPicker, ComponentHandle, ModelRc, OptionsPanel, PaintBar, PhysicalPosition, ShapeMenu,
    Toolbar, VecModel,
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
    find_process_window, focus_window, initial_toolbar_position, monitor_work_area,
    options_position, parked_position, pin_floating_positions, raise_floating_windows,
    remember_position, reshape_and_raise, reshape_when_sized, set_menu_dismissed, set_menu_escape,
    set_toolbar_moved, shape_menu_column, tool_window_style, ANCHOR_GAP,
};
use ui_bridge::{apply_column, apply_style_positions, style_column, UiCommand, UiSlots};

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
        // The shape switch menu is a window of its own: the arrow in the merged
        // rectangle/ellipse entry opens it above the toolbar, where the embedded
        // popup could not leave the pill. It starts hidden and is placed, focused
        // and dismissed through the same helpers the smoke drives.
        let shape_menu: ShapeMenu = snaplight_ui::shape_menu(&config)?;
        let _ = ui_slots.bind_shape_menu(shape_menu.as_weak());
        let menu_size = snaplight_ui::shape_menu_size(&config);
        snaplight_ui::hide_shape_menu(&shape_menu.as_weak());
        reshape_and_raise(snaplight_ui::SHAPE_MENU_TITLE, menu_size);
        wire_shape_menu(
            &ui,
            &shape_menu,
            &ui_slots,
            snaplight_ui::shape_entry_x(&config.tools),
            menu_size,
            ready.dpi,
            monitor_work_area(ready.bounds),
        );
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
        let dismiss_slots = ui_slots.clone();
        let panel_dpi = ready.dpi;
        let bounds = ready.bounds;
        let work_area = monitor_work_area(bounds);
        panel.on_dismissed(move || {
            // Closing the panel takes it out of the toolbar column, so the paint
            // bar closes up under the pill again.
            dismiss_slots.set_panel_size(None);
            restyle_column(&dismiss_slots, panel_dpi, work_area);
            if let Some(panel) = dismissed.upgrade() {
                let _ = panel.hide();
            }
            if let Some(picker) = picker_handles.upgrade() {
                let _ = picker.hide();
            }
        });
        let toolbar_ui = ui.as_weak();
        let panel_ui = panel.as_weak();
        let toggle_slots = ui_slots.clone();
        ui.on_options_toggled(move |tool| {
            let (Some(toolbar), Some(panel)) = (toolbar_ui.upgrade(), panel_ui.upgrade()) else {
                return;
            };
            // The select tool has nothing to configure.
            if !(1..=6).contains(&tool) {
                return;
            }
            if panel.window().is_visible() && panel.get_tool() == tool {
                toggle_slots.set_panel_size(None);
                restyle_column(&toggle_slots, panel_dpi, work_area);
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
            // The panel opens directly under the toolbar, as the second window of
            // its column, instead of floating above it and covering the pill.
            let logical = snaplight_ui::paint_logical_size(tool);
            panel.set_row_width(logical.0 as f32);
            toggle_slots.set_panel_size(Some(snaplight_ui::scale_to_physical(logical, panel_dpi)));
            restyle_column(&toggle_slots, panel_dpi, work_area);
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
        // The session is over: the shape menu window and the native hooks that
        // reach for it must not outlive the toolbar they belong to.
        let _ = shape_menu.hide();
        set_menu_dismissed(Arc::new(|| {}));
        set_menu_escape(Arc::new(|| {}));
        set_toolbar_moved(Arc::new(|| {}));
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
        // Rectangles and ellipses share one toolbar slot; the arrow next to it
        // opens the shape switch menu, which is a window of its own rather than
        // the embedded popup that could not leave the 52px pill. The checks below
        // drive the real windows with pointer events and read back their native
        // rectangles, so they cover hit-testing and placement, not only the
        // generated callbacks.
        use snaplight_ui::Model;
        use std::cell::RefCell;
        use std::rc::Rc;
        let commands: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let sink = commands.clone();
            ui.on_command(move |id| sink.borrow_mut().push(id));
        }
        let ids = |model: ModelRc<snaplight_ui::ToolItem>| -> Vec<i32> {
            (0..model.row_count())
                .filter_map(|index| model.row_data(index).map(|item| item.id))
                .collect()
        };
        let entries = ids(ui.get_tools());
        if entries != vec![0, 1, 3, 4, 5, 6] {
            return Err(
                format!("the merged toolbar shows {entries:?} instead of one shape entry").into(),
            );
        }
        let shapes = ids(ui.get_shapes());
        if shapes != vec![1, 2] {
            return Err(format!("the shape menu offers {shapes:?}, expected [1, 2]").into());
        }
        // The menu rows follow the configured order, and the merged button starts
        // on the first of them.
        let (first_shape, second_shape) = (shapes[0], shapes[1]);
        if ui.get_last_shape() != first_shape {
            return Err(format!(
                "the merged button starts on shape {}, expected the first configured one ({first_shape})",
                ui.get_last_shape()
            )
            .into());
        }
        // The menu is wired exactly like the editor wires it: the same placement
        // helpers, the same focus hand-off and the same command path.
        let menu: ShapeMenu = snaplight_ui::shape_menu(&config)?;
        let menu_logical = snaplight_ui::shape_menu_size(&config);
        let slots = Arc::new(UiSlots::default());
        let _ = slots.bind_toolbar(ui.as_weak());
        let _ = slots.bind_shape_menu(menu.as_weak());
        let _ = slots.bind_toolbar_size(snaplight_ui::toolbar_logical_size(&config.tools));
        let work_area = monitor_work_area(bounds);
        let entry_x = snaplight_ui::shape_entry_x(&config.tools);
        publish_toolbar_rect(&slots, ui.window().position(), ready.dpi);
        wire_shape_menu(
            &ui,
            &menu,
            &slots,
            entry_x,
            menu_logical,
            ready.dpi,
            work_area,
        );
        let _ = menu.hide();
        let tap = |window: &snaplight_ui::Window, x: f32, y: f32| {
            let position = snaplight_ui::LogicalPosition::new(x, y);
            window.dispatch_event(snaplight_ui::WindowEvent::PointerMoved { position });
            for event in [
                snaplight_ui::WindowEvent::PointerPressed {
                    position,
                    button: snaplight_ui::PointerEventButton::Left,
                },
                snaplight_ui::WindowEvent::PointerReleased {
                    position,
                    button: snaplight_ui::PointerEventButton::Left,
                },
            ] {
                window.dispatch_event(event);
            }
        };
        // The arrow is the right 19px of the merged entry, in toolbar coordinates.
        let arrow = entry_x as f32 + 40.0;
        let settle = |millis: u64| {
            snaplight_ui::run_event_loop_for(std::time::Duration::from_millis(millis));
        };
        let window_rect = |title: &'static str| -> std::result::Result<RECT, String> {
            let hwnd =
                find_process_window(title).ok_or_else(|| format!("{title} window missing"))?;
            let mut rect = RECT::default();
            unsafe {
                GetWindowRect(hwnd, &mut rect).map_err(|error| error.to_string())?;
            }
            Ok(rect)
        };
        // Physical rectangles and the DPI are recorded for the interface review:
        // the drop-down above the pill, the style row below it, a finished drag
        // and the top-edge fallback, so the placement can be checked without
        // re-running the smoke.
        let mut geometry = serde_json::Map::new();
        let rect_json = |r: RECT| serde_json::json!([r.left, r.top, r.right, r.bottom]);
        geometry.insert("dpi".into(), serde_json::json!(ready.dpi));
        let post_message = |title: &'static str, message: u32| -> std::result::Result<(), String> {
            let hwnd =
                find_process_window(title).ok_or_else(|| format!("{title} window missing"))?;
            unsafe { PostMessageW(hwnd, message, WPARAM(0), LPARAM(0)) }
                .map_err(|error| error.to_string())
        };
        // The arrow opens the menu above the toolbar and picks nothing by itself.
        tap(ui.window(), arrow, 26.0);
        settle(200);
        if ui.get_active() != 0 || !commands.borrow().is_empty() {
            return Err(format!(
                "the shape arrow activated {} and sent {:?}",
                ui.get_active(),
                commands.borrow()
            )
            .into());
        }
        if !menu.window().is_visible() {
            return Err("the shape arrow did not open the menu window".into());
        }
        let toolbar_rect = window_rect(snaplight_ui::TOOLBAR_TITLE)?;
        let menu_rect = window_rect(snaplight_ui::SHAPE_MENU_TITLE)?;
        // The menu is its own top-level window: it sits 8px above the pill and is
        // anchored at the merged shape entry, not at the toolbar's left edge.
        if menu_rect.bottom != toolbar_rect.top - ANCHOR_GAP {
            return Err(format!(
                "the shape menu sits at {menu_rect:?}, not 8px above the toolbar {toolbar_rect:?}"
            )
            .into());
        }
        let scaled_entry = snaplight_ui::scale_to_physical((entry_x, 0), ready.dpi).0;
        if menu_rect.left != toolbar_rect.left + scaled_entry {
            return Err(format!(
                "the shape menu starts at {}, not at the shape entry {}",
                menu_rect.left,
                toolbar_rect.left + scaled_entry
            )
            .into());
        }
        let scaled_menu = snaplight_ui::scale_to_physical(menu_logical, ready.dpi);
        if (
            menu_rect.right - menu_rect.left,
            menu_rect.bottom - menu_rect.top,
        ) != scaled_menu
        {
            return Err(format!(
                "the shape menu window is {:?} physical, expected {scaled_menu:?}",
                (
                    menu_rect.right - menu_rect.left,
                    menu_rect.bottom - menu_rect.top
                )
            )
            .into());
        }
        // The menu is a window of its own, so the snapshot records its own rows.
        geometry.insert("toolbar_menu_above".into(), rect_json(toolbar_rect));
        geometry.insert("menu_above".into(), rect_json(menu_rect));
        let snapshot = menu.window().take_snapshot()?;
        write_rgba_png(
            &data_dir().join("shape-menu-smoke.png"),
            snapshot.width(),
            snapshot.height(),
            snapshot.as_bytes(),
        )?;
        // Picking the second row on the menu window closes it, selects that shape
        // and reports its command; the row centre is inside the 24px row.
        tap(menu.window(), 50.0, 38.0);
        settle(200);
        if ui.get_active() != second_shape
            || ui.get_last_shape() != second_shape
            || commands.borrow().clone() != vec![second_shape]
        {
            return Err(format!(
                "picking shape {second_shape} left active {} / remembered {} / commands {:?}",
                ui.get_active(),
                ui.get_last_shape(),
                commands.borrow()
            )
            .into());
        }
        if menu.window().is_visible() {
            return Err("the shape menu stayed open after a pick".into());
        }
        // The menu toggles: the arrow opens it again and a second click closes
        // it, without reporting a command.
        tap(ui.window(), arrow, 26.0);
        settle(200);
        if !menu.window().is_visible() {
            return Err("the arrow did not reopen the shape menu".into());
        }
        tap(ui.window(), arrow, 26.0);
        settle(200);
        if menu.window().is_visible() {
            return Err("a second click on the arrow did not close the shape menu".into());
        }
        if ui.get_active() != second_shape || commands.borrow().clone() != vec![second_shape] {
            return Err(format!(
                "toggling the menu changed the selection: active {} commands {:?}",
                ui.get_active(),
                commands.borrow()
            )
            .into());
        }
        // The paint bar hangs off the toolbar as the first window of the same
        // column: it opens directly under the pill, never at the bottom of the
        // screen or over the toolbar.
        let bar = snaplight_ui::paint_bar(&config)?;
        let _ = slots.bind_paint_bar(bar.as_weak());
        let bar_logical = snaplight_ui::paint_bar_logical_size(Tool::Rectangle.id());
        bar.set_tool(Tool::Rectangle.id());
        bar.set_row_width(bar_logical.0 as f32);
        slots.set_bar_size(Some(snaplight_ui::scale_to_physical(
            bar_logical,
            ready.dpi,
        )));
        let _ = bar.show();
        reshape_and_raise(snaplight_ui::PAINT_BAR_TITLE, bar_logical);
        restyle_column(&slots, ready.dpi, work_area);
        settle(200);
        let toolbar_rect = window_rect(snaplight_ui::TOOLBAR_TITLE)?;
        let bar_rect = window_rect(snaplight_ui::PAINT_BAR_TITLE)?;
        if !bar.window().is_visible() {
            return Err("the paint bar is not visible".into());
        }
        if bar_rect.top != toolbar_rect.bottom + ANCHOR_GAP {
            return Err(format!(
                "the paint bar sits at {bar_rect:?}, not 8px below the toolbar {toolbar_rect:?}"
            )
            .into());
        }
        geometry.insert("toolbar_style_below".into(), rect_json(toolbar_rect));
        geometry.insert("paint_bar_below".into(), rect_json(bar_rect));
        // A finished drag is reported by the toolbar's own native subclass; the
        // style bar must follow the pill to its new spot through that message
        // alone, with no extra push from the test.
        let dragged = PhysicalPosition::new(work_area.left + 120, work_area.top + 200);
        ui.window().set_position(dragged);
        settle(80);
        post_message(snaplight_ui::TOOLBAR_TITLE, WM_EXITSIZEMOVE)?;
        settle(200);
        let moved_toolbar = window_rect(snaplight_ui::TOOLBAR_TITLE)?;
        let moved_bar = window_rect(snaplight_ui::PAINT_BAR_TITLE)?;
        if (moved_toolbar.left, moved_toolbar.top) != (dragged.x, dragged.y) {
            return Err(format!(
                "the dragged toolbar sat at {moved_toolbar:?}, not at {dragged:?}"
            )
            .into());
        }
        if moved_bar.top != moved_toolbar.bottom + ANCHOR_GAP {
            return Err(format!(
                "the paint bar did not follow the dragged toolbar: bar {moved_bar:?} toolbar {moved_toolbar:?}"
            )
            .into());
        }
        geometry.insert("toolbar_after_drag".into(), rect_json(moved_toolbar));
        geometry.insert("paint_bar_after_drag".into(), rect_json(moved_bar));
        // A toolbar against the top of the work area has no room above it, so the
        // menu flips below the pill and the style bar moves under the menu; the
        // toolbar and the bar both stay visible and never overlap the menu.
        let top_edge = PhysicalPosition::new(work_area.left + 16, work_area.top);
        ui.window().set_position(top_edge);
        publish_toolbar_rect(&slots, top_edge, ready.dpi);
        settle(80);
        tap(ui.window(), arrow, 26.0);
        settle(200);
        let toolbar_rect = window_rect(snaplight_ui::TOOLBAR_TITLE)?;
        let menu_rect = window_rect(snaplight_ui::SHAPE_MENU_TITLE)?;
        let bar_rect = window_rect(snaplight_ui::PAINT_BAR_TITLE)?;
        if !menu.window().is_visible() || !bar.window().is_visible() {
            return Err("the edge case hid the menu or the paint bar".into());
        }
        if menu_rect.top != toolbar_rect.bottom + ANCHOR_GAP {
            return Err(format!(
                "the menu did not flip under the toolbar: menu {menu_rect:?} toolbar {toolbar_rect:?}"
            )
            .into());
        }
        if bar_rect.top != menu_rect.bottom + ANCHOR_GAP {
            return Err(format!(
                "the paint bar did not move below the menu: bar {bar_rect:?} menu {menu_rect:?}"
            )
            .into());
        }
        geometry.insert("toolbar_top_edge".into(), rect_json(toolbar_rect));
        geometry.insert("menu_below".into(), rect_json(menu_rect));
        geometry.insert("paint_bar_below_menu".into(), rect_json(bar_rect));
        // The menu is open from the edge case above; a long capture owns the pill
        // and closes that transient window by itself, through the toolbar
        // callback rather than a manual hide.
        if !menu.window().is_visible() {
            return Err("the edge case left the shape menu closed".into());
        }
        snaplight_ui::set_toolbar_long_capture(&ui.as_weak(), true);
        settle(200);
        if menu.window().is_visible() {
            return Err("the long capture did not close the shape menu".into());
        }
        // The disabled entry ignores the arrow while the long capture runs.
        tap(ui.window(), arrow, 26.0);
        settle(120);
        if menu.window().is_visible() {
            return Err("the shape arrow opened the menu during a long capture".into());
        }
        snaplight_ui::set_toolbar_long_capture(&ui.as_weak(), false);
        settle(120);
        tap(ui.window(), arrow, 26.0);
        settle(120);
        if !menu.window().is_visible() {
            return Err("the shape arrow did not work again after the long capture".into());
        }
        if ui.get_active() != second_shape || commands.borrow().clone() != vec![second_shape] {
            return Err(format!(
                "opening the menu changed the selection: active {} commands {:?}",
                ui.get_active(),
                commands.borrow()
            )
            .into());
        }
        // The reopened menu highlights the remembered shape, not the first one.
        if menu.get_shape_tool() != second_shape {
            return Err(format!(
                "the reopened menu highlighted shape {}, not the remembered {second_shape}",
                menu.get_shape_tool()
            )
            .into());
        }
        // Hiding the main toolbar closes the menu with it, through the same
        // toolbar callback, and the restyle that follows must not bring the
        // hidden toolbar back on screen.
        snaplight_ui::hide_toolbar(&ui.as_weak());
        settle(200);
        if menu.window().is_visible() {
            return Err("hiding the toolbar left the shape menu open".into());
        }
        if ui.window().is_visible() {
            return Err("a menu dismissal brought the hidden toolbar back".into());
        }
        // The menu is a separate window, so opening and closing it never resizes
        // the toolbar it belongs to.
        let toolbar_size = ui.window().size();
        if (toolbar_size.width, toolbar_size.height) != (expected.0 as u32, expected.1 as u32) {
            return Err(format!(
                "the shape menu resized the toolbar to {toolbar_size:?}, expected {expected:?}"
            )
            .into());
        }
        atomic_write(
            &data_dir().join("shape-menu-geometry.json"),
            &serde_json::to_vec_pretty(&serde_json::Value::Object(geometry))?,
        )?;
        // The smoke owns every window: take them down before the toolbar hides so
        // no stray top-level window outlives the check.
        bar.hide()?;
        let _ = menu.hide();
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
        // variant: rectangle and ellipse (fill and outline), pen (outline width),
        // mosaic (block size) and text (font size plus the text field). Each
        // variant is first rendered wider than it needs, where the row lays out at
        // its own preferred size, to prove the configured width covers the row.
        const GENEROUS: u32 = 1400;
        let panel = snaplight_ui::options_panel(&config)?;
        for (tool, slug) in [
            (1, "rect"),
            (2, "ellipse"),
            (4, "pen"),
            (5, "mosaic"),
            (6, "text"),
        ] {
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
        for (tool, slug) in [(1, "rect"), (2, "ellipse"), (5, "mosaic"), (6, "text")] {
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
        // The shape row is driven with real pointer events as well, so the check
        // covers the hit-testing of the swatches and the colour target switch, not
        // just the generated `changed`/`pick` callbacks. The bar is left on the
        // shape tool by the loop above; the points below mirror that row (14px
        // padding, two 48px toggles, two 92px spin boxes, two 1px rules, the 120px
        // target switch, eight 20px swatches 4px apart and the 26px picker
        // button, all with 10px gaps and a 30px row centred in the 52px bar).
        bar.set_tool(1);
        bar.set_fill_on(true);
        snaplight_ui::run_event_loop_for(std::time::Duration::from_millis(80));
        let last_paint: Rc<RefCell<Option<snaplight_ui::ToolPaint>>> = Rc::new(RefCell::new(None));
        {
            let sink = last_paint.clone();
            bar.on_properties_changed(move |paint| {
                *sink.borrow_mut() = Some(paint);
            });
        }
        let pick_index: Rc<RefCell<Option<i32>>> = Rc::new(RefCell::new(None));
        {
            let sink = pick_index.clone();
            bar.on_pick_color(move |which| {
                *sink.borrow_mut() = Some(which);
            });
        }
        let bar_click = |x: f32, y: f32| {
            let position = snaplight_ui::LogicalPosition::new(x, y);
            bar.window()
                .dispatch_event(snaplight_ui::WindowEvent::PointerMoved { position });
            for event in [
                snaplight_ui::WindowEvent::PointerPressed {
                    position,
                    button: snaplight_ui::PointerEventButton::Left,
                },
                snaplight_ui::WindowEvent::PointerReleased {
                    position,
                    button: snaplight_ui::PointerEventButton::Left,
                },
            ] {
                bar.window().dispatch_event(event);
            }
            snaplight_ui::run_event_loop_for(std::time::Duration::from_millis(80));
        };
        // Colours written as [r, g, b, a] so the assertions can compare them with
        // the tolerance the u8 colour round trip needs.
        let rgb = |hex: (f32, f32, f32)| [hex.0, hex.1, hex.2, 1.0];
        let red = rgb((242.0 / 255.0, 51.0 / 255.0, 64.0 / 255.0));
        let blue = rgb((47.0 / 255.0, 128.0 / 255.0, 237.0 / 255.0));
        let violet = rgb((155.0 / 255.0, 81.0 / 255.0, 224.0 / 255.0));
        let near = |color: snaplight_ui::Color, expected: [f32; 4]| {
            let got = snaplight_ui::rgba_of(color);
            (0..4).all(|index| (got[index] - expected[index]).abs() < 0.01)
        };
        // The row's vertical centre: the 30px row is centred in the 52px bar.
        let row_y = 26.0;
        // Swatch centres: the row starts at x 14, the swatches at +472, each 24px
        // apart (20px wide plus 4px spacing).
        let swatch = |index: f32| 14.0 + 472.0 + index * 24.0 + 10.0;
        // Blue is the seventh preset, after the first swatch of the row.
        let blue_x = swatch(6.0);
        let violet_x = swatch(7.0);
        // The 120px target switch sits after the two dividers; its right half is
        // the fill target.
        let fill_target_x = 14.0 + 342.0 + 60.0 + 30.0;
        // The picker button is the last 26px element of the row.
        let picker_x = 14.0 + 670.0 + 13.0;
        // The stroke target is selected first, so a preset changes only the
        // outline and leaves the fill (and every non-colour value) alone.
        *last_paint.borrow_mut() = None;
        bar_click(blue_x, row_y);
        let Some(paint) = last_paint.borrow().clone() else {
            return Err("the stroke swatch sent no paint".into());
        };
        if !near(paint.stroke_color, blue) {
            return Err(format!(
                "the stroke swatch set the outline to {:?} instead of blue",
                snaplight_ui::rgba_of(paint.stroke_color)
            )
            .into());
        }
        if !near(paint.fill_color, red)
            || paint.stroke_width != 3
            || paint.fill_opacity != 100
            || !paint.stroke_on
            || !paint.fill_on
        {
            return Err(format!(
                "the stroke swatch also changed {:?} / width {} / fill opacity {} / on {}/{}",
                snaplight_ui::rgba_of(paint.fill_color),
                paint.stroke_width,
                paint.fill_opacity,
                paint.stroke_on,
                paint.fill_on
            )
            .into());
        }
        // Switching the target to the fill must leave the outline as it was, so a
        // second preset then changes only the fill.
        bar_click(fill_target_x, row_y);
        *last_paint.borrow_mut() = None;
        bar_click(violet_x, row_y);
        let Some(paint) = last_paint.borrow().clone() else {
            return Err("the fill swatch sent no paint".into());
        };
        if !near(paint.fill_color, violet) {
            return Err(format!(
                "the fill swatch set the fill to {:?} instead of violet",
                snaplight_ui::rgba_of(paint.fill_color)
            )
            .into());
        }
        if !near(paint.stroke_color, blue) {
            return Err(format!(
                "the fill swatch also changed the outline to {:?}",
                snaplight_ui::rgba_of(paint.stroke_color)
            )
            .into());
        }
        // The custom button opens the picker for the target that is selected, so
        // it has to ask for the fill (1) now that the fill target owns the row.
        *pick_index.borrow_mut() = None;
        bar_click(picker_x, row_y);
        if *pick_index.borrow() != Some(1) {
            return Err(format!(
                "the custom colour button asked for target {:?} instead of the fill",
                *pick_index.borrow()
            )
            .into());
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

/// Places the toolbar and the style windows hanging under it on the window
/// thread. Used when the toolbar column changes without a canvas paint update:
/// opening or closing the options panel, a menu dismissal, a toolbar drag.
/// A hidden toolbar is left alone: a dismissal or a move event that arrives
/// after the pill is gone must not bring it back on screen.
fn restyle_column(slots: &Arc<UiSlots>, dpi: u32, work_area: RECT) {
    let visible = slots
        .toolbar()
        .and_then(|weak| weak.upgrade())
        .is_some_and(|ui| ui.window().is_visible());
    if !visible {
        return;
    }
    if let Some(column) = style_column(slots, dpi, work_area) {
        apply_column(slots, &column);
    }
}

/// Refreshes the toolbar rectangle the column is laid out from, using the
/// window's current position. A user drag moves the pill without telling the
/// canvas thread, so the column would otherwise keep stacking the style windows
/// under where the toolbar used to be.
fn publish_toolbar_rect(slots: &UiSlots, position: PhysicalPosition, dpi: u32) {
    let Some(logical) = slots.toolbar_size() else {
        return;
    };
    let size = snaplight_ui::scale_to_physical(logical, dpi);
    slots.set_toolbar_rect(Some(RECT {
        left: position.x,
        top: position.y,
        right: position.x + size.0,
        bottom: position.y + size.1,
    }));
}

/// Wires the shape switch menu to the toolbar.
///
/// The menu is a top-level window of its own, so the editor places it above the
/// toolbar - anchored at the merged rectangle/ellipse entry - and hands it the
/// activation, which is what lets Escape and focus loss reach the native
/// subclass. It closes on a pick, a second toggle, Escape, a focus loss, a long
/// capture, a toolbar drag and the end of the session. Only Escape returns the
/// focus to the toolbar: the generic dismissal - a click on another application,
/// a long capture, a hidden pill - closes the menu without stealing the focus
/// back. The menu refuses to open while the toolbar is hidden or stitching, and
/// re-syncs its highlight from the shape the pill remembers on every open. When
/// the work area has no room above the toolbar the menu opens below it and pushes
/// the style windows further down, so it never covers the pill or the style row.
/// Every handler holds weak component handles only: the editor never keeps a
/// window alive through a callback or a hook.
fn wire_shape_menu(
    toolbar: &Toolbar,
    menu: &ShapeMenu,
    slots: &Arc<UiSlots>,
    entry_x: u32,
    menu_size: (u32, u32),
    dpi: u32,
    work_area: RECT,
) {
    let toggle = {
        let menu = menu.as_weak();
        let toolbar = toolbar.as_weak();
        let slots = slots.clone();
        move || {
            let Some(menu) = menu.upgrade() else {
                return;
            };
            if menu.window().is_visible() {
                let _ = menu.hide();
                return;
            }
            // A hidden or long-capture toolbar owns no drop-down: the arrow is
            // only a request, and the menu must not open over a pill that is not
            // there or is busy stitching.
            let Some(toolbar) = toolbar.upgrade() else {
                return;
            };
            if !toolbar.window().is_visible() || toolbar.get_long_capture() {
                return;
            }
            // The highlight follows the shape the pill's main button remembers,
            // so reopening the menu never falls back to the first shape.
            menu.set_shape_tool(toolbar.get_last_shape());
            let (Some(rect), Some(logical)) = (slots.toolbar_rect(), slots.toolbar_size()) else {
                return;
            };
            let size = snaplight_ui::scale_to_physical(logical, dpi);
            let toolbar_rect = RECT {
                left: rect.left,
                top: rect.top,
                right: rect.left + size.0,
                bottom: rect.top + size.1,
            };
            // The menu is anchored at the shape entry, so it opens under the
            // button that asked for it instead of at the toolbar's left edge.
            let anchor = toolbar_rect.left + snaplight_ui::scale_to_physical((entry_x, 0), dpi).0;
            let placement = shape_menu_column(
                toolbar_rect,
                anchor,
                snaplight_ui::scale_to_physical(menu_size, dpi),
                &slots.style_sizes(),
                work_area,
            );
            menu.window()
                .set_position(PhysicalPosition::new(placement.menu.0, placement.menu.1));
            let _ = menu.show();
            reshape_and_raise(snaplight_ui::SHAPE_MENU_TITLE, menu_size);
            // A drop-down needs the foreground for Escape and focus loss; it is
            // handed back to the toolbar on every close.
            focus_window(snaplight_ui::SHAPE_MENU_TITLE);
            if let Some(positions) = placement.style {
                apply_style_positions(&slots, &positions);
            }
        }
    };
    toolbar.on_shape_menu_toggled(toggle);
    let pick = {
        let menu = menu.as_weak();
        let toolbar = toolbar.as_weak();
        let slots = slots.clone();
        move |shape: i32| {
            if let Some(menu) = menu.upgrade() {
                let _ = menu.hide();
            }
            // A menu that opened below the toolbar pushed the style windows
            // down; putting them back is the same column the toolbar would use.
            restyle_column(&slots, dpi, work_area);
            focus_window(snaplight_ui::TOOLBAR_TITLE);
            // The pick follows the same path as a click on the merged button:
            // the toolbar selects the shape and reports its command, so the
            // remembered shape and the canvas both stay in step.
            if let Some(toolbar) = toolbar.upgrade() {
                toolbar.set_active(shape);
                toolbar.invoke_command(shape);
            }
        }
    };
    menu.on_picked(pick);
    // Generic dismissal: hide the menu and put the style windows back, but never
    // move the focus. It runs when the user clicks another application, when the
    // toolbar goes away or when a long capture starts, and taking the focus back
    // would fight the very click that dismissed the menu.
    let dismissed = {
        let menu = menu.as_weak();
        let slots = slots.clone();
        Arc::new(move || {
            if let Some(menu) = menu.upgrade() {
                let _ = menu.hide();
            }
            // The restyle must not bring a hidden toolbar back, which the column
            // guard already enforces.
            restyle_column(&slots, dpi, work_area);
        })
    };
    let dismiss_cb = dismissed.clone();
    // The toolbar reports its own dismissal - a long capture started or the pill
    // is being hidden - so the menu never outlives the toolbar it belongs to.
    toolbar.on_shape_menu_dismissed(move || dismiss_cb());
    set_menu_dismissed(dismissed);
    // Escape is the one dismissal the user asked for with the keyboard, so it is
    // the one that hands the focus back to the toolbar it was taken from.
    let escaped = {
        let menu = menu.as_weak();
        let slots = slots.clone();
        Arc::new(move || {
            if let Some(menu) = menu.upgrade() {
                let _ = menu.hide();
            }
            restyle_column(&slots, dpi, work_area);
            focus_window(snaplight_ui::TOOLBAR_TITLE);
        })
    };
    set_menu_escape(escaped);
    let moved = {
        let menu = menu.as_weak();
        let toolbar = toolbar.as_weak();
        let slots = slots.clone();
        Arc::new(move || {
            // A drag moves the pill: the transient menu belongs to the old spot
            // and the style windows follow the toolbar.
            if let Some(menu) = menu.upgrade() {
                let _ = menu.hide();
            }
            if let Some(toolbar) = toolbar.upgrade() {
                publish_toolbar_rect(&slots, toolbar.window().position(), dpi);
            }
            restyle_column(&slots, dpi, work_area);
        })
    };
    set_toolbar_moved(moved);
}

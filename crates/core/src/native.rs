use snaplight_domain::{
    parse_hotkey, Config, Envelope, PinCommand, DEFAULT_CAPTURE_HOTKEY, PROTOCOL_VERSION,
};
use snaplight_platform::popup_menu::{Entry, Item};
use snaplight_platform::{data_dir, load_config, popup_menu, sibling};
use std::{
    io::{BufRead, BufReader, Write},
    os::windows::io::{AsHandle, AsRawHandle, OwnedHandle},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
};
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        System::{LibraryLoader::*, Threading::*},
        UI::{Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
    },
};

const TRAY: u32 = WM_APP + 1;
const PIN: u32 = WM_APP + 2;
/// Entry indices of the tray menu, in display order.
mod tray_item {
    pub const CAPTURE: usize = 0;
    pub const SETTINGS: usize = 1;
    pub const CLOSE_PINS: usize = 2;
    pub const QUIT: usize = 4;
}
/// Tray menu content. The layout follows the reference design: a primary action
/// with its shortcut on the right, secondary actions, a separator, then the
/// command that ends the session. Closing every pin destroys content, so it is
/// drawn in red.
fn tray_menu_entries(capture_hotkey: &str) -> Vec<Entry> {
    vec![
        Entry::Item(Item::new("截图").with_shortcut(capture_hotkey)),
        Entry::Item(Item::new("设置")),
        Entry::Item(Item::new("关闭所有贴图").danger()),
        Entry::Separator,
        Entry::Item(Item::new("退出")),
    ]
}
/// Which page the one allowed editor process is showing. The tray uses it to
/// tell a repeated settings request from a capture that must be left alone.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EditorKind {
    Capture,
    Settings,
}
struct State {
    pins: Option<Child>,
    queue: Arc<Mutex<Vec<PinCommand>>>,
    editor: Arc<Mutex<Option<EditorKind>>>,
    editor_handle: Arc<Mutex<Option<OwnedHandle>>>,
    capture_hotkey: String,
}
unsafe fn launch_editor(hwnd: HWND, state: &State, settings: bool) {
    let kind = if settings {
        EditorKind::Settings
    } else {
        EditorKind::Capture
    };
    let running = {
        let mut editor = state.editor.lock().unwrap();
        match *editor {
            Some(running) => Some(running),
            None => {
                *editor = Some(kind);
                None
            }
        }
    };
    if let Some(running) = running {
        // A capture session is left alone. If the user asks for the settings
        // page twice, only one page may exist, so the open one flashes as a
        // reminder instead of stacking a second window.
        if kind == EditorKind::Settings && running == EditorKind::Settings {
            if let Some(handle) = state.editor_handle.lock().unwrap().as_ref() {
                if let Some(hwnd) = editor_settings_window(handle) {
                    start_settings_alert(hwnd);
                }
            }
        }
        return;
    }
    let result = (|| -> std::result::Result<Child, Box<dyn std::error::Error>> {
        let mut command = Command::new(sibling("snaplight-editor")?);
        if settings {
            command.arg("--settings");
        }
        use std::os::windows::process::CommandExt;
        command
            .creation_flags(0x08000000)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(command.spawn()?)
    })();
    match result {
        Ok(mut child) => {
            let handle = match child.as_handle().try_clone_to_owned() {
                Ok(handle) => handle,
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    *state.editor.lock().unwrap() = None;
                    snaplight_platform::native::show_error(&e.to_string());
                    return;
                }
            };
            *state.editor_handle.lock().unwrap() = Some(handle);
            let queue = state.queue.clone();
            let editor = state.editor.clone();
            let owned_handle = state.editor_handle.clone();
            let address = hwnd.0 as usize;
            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take().unwrap();
            let errors = std::thread::spawn(move || {
                use std::io::Read;
                let mut message = String::new();
                let _ = BufReader::new(stderr).read_to_string(&mut message);
                message
            });
            std::thread::spawn(move || {
                for line in BufReader::new(stdout)
                    .lines()
                    .map_while(std::result::Result::ok)
                {
                    if let Ok(e) = serde_json::from_str::<Envelope<PinCommand>>(&line) {
                        if e.version == PROTOCOL_VERSION {
                            queue.lock().unwrap().push(e.payload);
                            let _ = PostMessageW(HWND(address as _), PIN, WPARAM(0), LPARAM(0));
                        }
                    }
                }
                let status = child.wait();
                let message = errors.join().unwrap_or_default();
                *owned_handle.lock().unwrap() = None;
                *editor.lock().unwrap() = None;
                if status.is_ok_and(|s| !s.success()) {
                    snaplight_platform::native::show_error(&format!("Editor failed: {message}"));
                }
            });
        }
        Err(e) => {
            *state.editor.lock().unwrap() = None;
            snaplight_platform::native::show_error(&e.to_string());
        }
    }
}
/// Finds the running editor's settings page. The window belongs to the child
/// process and the core only holds its process handle, so it is found by
/// process id. The editor also owns an untitled event-loop window, hence the
/// non-empty title requirement.
unsafe fn editor_settings_window(handle: &OwnedHandle) -> Option<HWND> {
    let pid = GetProcessId(HANDLE(handle.as_raw_handle()));
    if pid == 0 {
        return None;
    }
    struct Search {
        pid: u32,
        found: HWND,
    }
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` carries the `Search` owned by `flash_editor_window`,
        // which outlives the enumeration.
        let search = unsafe { &mut *(lparam.0 as *mut Search) };
        let mut window_pid = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut window_pid));
        }
        // The editor process also owns an untitled event-loop window, so only
        // the captioned page is the settings window worth flashing.
        let mut title = [0u16; 256];
        if window_pid == search.pid
            && unsafe { GetWindowTextW(hwnd, &mut title) } > 0
            && unsafe { IsWindowVisible(hwnd).as_bool() }
        {
            search.found = hwnd;
            return BOOL(0);
        }
        BOOL(1)
    }
    let mut search = Search {
        pid,
        found: HWND::default(),
    };
    let _ = EnumWindows(Some(visit), LPARAM((&mut search as *mut Search) as isize));
    if search.found.0.is_null() {
        None
    } else {
        Some(search.found)
    }
}
/// Brings the open settings page to the front and shakes it for a moment. The
/// animation runs on its own thread so the tray loop keeps processing messages;
/// the named mutex makes a second click during the animation do nothing (two
/// shakes would restore different origins and leave the page displaced).
fn start_settings_alert(hwnd: HWND) {
    let Ok(Some(guard)) =
        snaplight_platform::native::singleton("Local\\Snaplight.Settings.Alert.v1")
    else {
        return;
    };
    let guard = guard.0 as isize;
    let hwnd = hwnd.0 as isize;
    std::thread::spawn(move || {
        snaplight_platform::native::alert_window(
            HWND(hwnd as _),
            snaplight_platform::native::ALERT_DURATION,
        );
        let _ = unsafe { CloseHandle(HANDLE(guard as _)) };
    });
}
unsafe fn send_pin(
    state: &mut State,
    command: PinCommand,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let first = send_pin_once(state, command.clone());
    if first.as_ref().err().is_some_and(|e| {
        e.downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe)
    }) {
        if let Some(mut child) = state.pins.take() {
            let _ = child.wait();
        }
        return send_pin_once(state, command);
    }
    first
}
unsafe fn send_pin_once(
    state: &mut State,
    command: PinCommand,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let alive = state
        .pins
        .as_mut()
        .map(|c| c.try_wait())
        .transpose()?
        .flatten()
        .is_none()
        && state.pins.is_some();
    if !alive {
        if !matches!(command, PinCommand::Open { .. }) {
            return Ok(());
        }
        use std::os::windows::process::CommandExt;
        state.pins = Some(
            Command::new(sibling("snaplight-pins")?)
                .creation_flags(0x08000000)
                .stdin(Stdio::piped())
                .spawn()?,
        );
    }
    if let Some(input) = state.pins.as_mut().and_then(|c| c.stdin.as_mut()) {
        writeln!(input, "{}", serde_json::to_string(&Envelope::new(command))?)?;
        input.flush()?;
    }
    Ok(())
}
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let create = &*(l.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
    if !ptr.is_null() {
        let state = &mut *ptr;
        match msg {
            WM_HOTKEY => {
                launch_editor(hwnd, state, false);
                return LRESULT(0);
            }
            msg if msg == snaplight_platform::native::RELOAD_CONFIG => {
                reload_settings(hwnd, state);
                return LRESULT(0);
            }
            PIN => {
                let commands = std::mem::take(&mut *state.queue.lock().unwrap());
                for command in commands {
                    if let Err(e) = send_pin(state, command) {
                        snaplight_platform::native::show_error(&e.to_string());
                    }
                }
                return LRESULT(0);
            }
            TRAY => {
                match l.0 as u32 {
                    WM_LBUTTONUP => launch_editor(hwnd, state, false),
                    WM_RBUTTONUP => {
                        // The popup owns its drawing, so the tray menu no longer
                        // depends on the system theme. Claiming the foreground
                        // window first is what makes clicking elsewhere close it.
                        let _ = SetForegroundWindow(hwnd);
                        let entries = tray_menu_entries(&state.capture_hotkey);
                        if let Some(choice) = popup_menu::show(&entries, None) {
                            match choice {
                                tray_item::CAPTURE => launch_editor(hwnd, state, false),
                                tray_item::SETTINGS => launch_editor(hwnd, state, true),
                                tray_item::CLOSE_PINS => {
                                    let _ = send_pin(state, PinCommand::CloseAll);
                                }
                                tray_item::QUIT => {
                                    let _ = DestroyWindow(hwnd);
                                }
                                _ => {}
                            }
                        }
                        let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
                    }
                    _ => {}
                }
                return LRESULT(0);
            }
            WM_DESTROY => {
                let _ = UnregisterHotKey(hwnd, 1);
                let icon = NOTIFYICONDATAW {
                    cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                    hWnd: hwnd,
                    uID: 1,
                    ..Default::default()
                };
                let _ = Shell_NotifyIconW(NIM_DELETE, &icon);
                let _ = send_pin(state, PinCommand::Shutdown);
                if let Some(child) = state.pins.as_mut() {
                    let _ = child.wait();
                }
                if let Some(handle) = state.editor_handle.lock().unwrap().take() {
                    let _ = TerminateProcess(HANDLE(handle.as_raw_handle()), 0);
                }
                PostQuitMessage(0);
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, w, l)
}
pub fn run() -> std::result::Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|a| a == "--plugin-check") {
        return plugin_check();
    }
    let config = load_config()?;
    std::fs::create_dir_all(data_dir())?;
    snaplight_platform::native::dpi_aware();
    let capture_hotkey = parse_hotkey(&config.capture_hotkey)
        .map(|hotkey| hotkey.canonical())
        .unwrap_or_else(|_| DEFAULT_CAPTURE_HOTKEY.to_string());
    // Manual check for the tray menu: it shows the real popup, reports the choice
    // and never performs the action. `--menu-demo <x> <y>` pins the anchor so the
    // placement can be checked without racing the pointer.
    if let Some(index) = std::env::args().position(|a| a == "--menu-demo") {
        let mut rest = std::env::args().skip(index + 1);
        let anchor = match (rest.next(), rest.next()) {
            (Some(x), Some(y)) => x.parse::<i32>().ok().zip(y.parse::<i32>().ok()),
            _ => None,
        };
        let entries = tray_menu_entries(&capture_hotkey);
        match popup_menu::show(&entries, anchor) {
            Some(choice) => println!("menu choice: {choice}"),
            None => println!("menu dismissed"),
        }
        return Ok(());
    }
    let Some(mutex) = snaplight_platform::native::singleton("Local\\Snaplight.Core.v1")? else {
        return Ok(());
    };
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("Snaplight.Core");
        let wc = WNDCLASSW {
            hInstance: instance.into(),
            lpszClassName: class,
            lpfnWndProc: Some(wndproc),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(Error::from_win32().into());
        }
        let mut state = Box::new(State {
            pins: None,
            queue: Arc::new(Mutex::new(vec![])),
            editor: Arc::new(Mutex::new(None)),
            editor_handle: Arc::new(Mutex::new(None)),
            capture_hotkey,
        });
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            w!("Snaplight Core"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            instance,
            Some((&mut *state as *mut State).cast()),
        )?;
        register_capture_hotkey(hwnd, &config)?;
        // The tray wants a small icon; loading the 16px entry of the embedded
        // resource keeps it crisp instead of letting the shell shrink the 32px
        // default. The handle stays alive for the process lifetime.
        let tray_icon = snaplight_platform::native::app_icon(true)?;
        let mut icon = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: TRAY,
            hIcon: tray_icon,
            ..Default::default()
        };
        let tip: Vec<u16> = "Snaplight - Ctrl+Shift+A".encode_utf16().collect();
        icon.szTip[..tip.len()].copy_from_slice(&tip);
        if !Shell_NotifyIconW(NIM_ADD, &icon).as_bool() {
            let _ = DestroyWindow(hwnd);
            return Err(Error::from_win32().into());
        }
        if std::env::args().any(|a| a == "--smoke") {
            tray_menu_smoke()?;
            DestroyWindow(hwnd)?;
        } else {
            snaplight_platform::native::message_loop()?;
        }
        CloseHandle(mutex)?;
    }
    Ok(())
}
/// Re-reads the configuration after the settings window saved a change. The
/// capture shortcut is re-registered in place and the pin process is asked to
/// reload its own shortcuts, so neither process has to be restarted.
unsafe fn reload_settings(hwnd: HWND, state: &mut State) {
    let config = match load_config() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("settings reload failed ({error}); keeping the current shortcuts");
            return;
        }
    };
    let capture = parse_hotkey(&config.capture_hotkey)
        .map(|hotkey| hotkey.canonical())
        .unwrap_or_else(|_| DEFAULT_CAPTURE_HOTKEY.to_string());
    if capture != state.capture_hotkey {
        let previous = state.capture_hotkey.clone();
        // Registering the new combination needs the old id to be free. If the
        // new one is taken, put the previous one back rather than leaving the
        // application without a capture shortcut.
        let _ = UnregisterHotKey(hwnd, 1);
        match register_capture_hotkey(hwnd, &config) {
            Ok(()) => state.capture_hotkey = capture,
            Err(error) => {
                eprintln!(
                    "capture hotkey {} unavailable on reload ({error}); restoring {previous}",
                    config.capture_hotkey
                );
                if let Ok(hotkey) = parse_hotkey(&previous) {
                    let (modifiers, key) = snaplight_platform::native::hotkey_registration(&hotkey);
                    let _ = RegisterHotKey(hwnd, 1, modifiers, key);
                }
            }
        }
    }
    // Pins re-read the same file for their restore and held shortcuts plus the
    // shadow metrics. This also covers the case where the settings window was
    // started without a piped stdout.
    if let Err(error) = send_pin(state, PinCommand::Reload) {
        eprintln!("could not tell the pin process to reload: {error}");
    }
}
/// Registers the configured capture shortcut, falling back to the default when
/// another application already owns the configured combination.
unsafe fn register_capture_hotkey(
    hwnd: HWND,
    config: &Config,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let hotkey = parse_hotkey(&config.capture_hotkey)?;
    let (modifiers, key) = snaplight_platform::native::hotkey_registration(&hotkey);
    if let Err(error) = RegisterHotKey(hwnd, 1, modifiers, key) {
        let fallback = parse_hotkey(DEFAULT_CAPTURE_HOTKEY).expect("default capture hotkey");
        let (modifiers, key) = snaplight_platform::native::hotkey_registration(&fallback);
        eprintln!(
            "capture hotkey {} unavailable ({error}); using {DEFAULT_CAPTURE_HOTKEY}",
            config.capture_hotkey
        );
        RegisterHotKey(hwnd, 1, modifiers, key)?;
    }
    Ok(())
}
/// Renders the tray menu at the DPI scales this machine is likely to use and
/// checks the reference styling, so a regression is caught by `--smoke` instead
/// of by eye. The PNGs stay in the data directory as reviewable artifacts.
fn tray_menu_smoke() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // The close-all entry is hovered so the highlight rendering is covered too.
    let entries = tray_menu_entries(DEFAULT_CAPTURE_HOTKEY);
    for percent in [100u32, 125, 150, 200] {
        let scale = percent as f32 / 100.0;
        let menu = popup_menu::render(&entries, scale, Some(tray_item::CLOSE_PINS))?;
        write_menu_png(&data_dir().join(format!("menu-smoke-{percent}.png")), &menu)?;
        let layout = menu.layout.clone();
        let card = layout.card;
        let corner = menu
            .pixel(card.x, card.y)
            .ok_or("menu corner outside bitmap")?;
        if corner[3] >= 128 {
            return Err(format!("menu corner is opaque at {percent}%: {corner:?}").into());
        }
        let padding_y = ((card.bottom() - layout.rows[3].band.bottom()) / 2).max(1);
        let inside = menu
            .pixel(card.x + card.width / 2, card.bottom() - padding_y)
            .ok_or("menu padding outside bitmap")?;
        if inside != [255, 255, 255, 255] {
            return Err(format!("menu card is not white at {percent}%: {inside:?}").into());
        }
        let row_named = |index: usize| {
            layout
                .rows
                .iter()
                .find(|row| row.entry == index)
                .copied()
                .ok_or_else(|| format!("menu row {index} missing at {percent}%"))
        };
        let close = row_named(tray_item::CLOSE_PINS)?;
        if !row_has(&menu, &close, |pixel| {
            pixel[2] as i32 > 150 && pixel[2] as i32 > pixel[0] as i32 + 60
        }) {
            return Err(format!("close-all entry is not red at {percent}%").into());
        }
        if !row_has(&menu, &close, |pixel| pixel == [0xF2, 0xF2, 0xF2, 255]) {
            return Err(format!("hovered row is not highlighted at {percent}%").into());
        }
        let capture = row_named(tray_item::CAPTURE)?;
        if !row_has(&menu, &capture, |pixel| {
            pixel[3] == 255
                && (60..130).contains(&pixel[0])
                && pixel[0].abs_diff(pixel[1]) < 12
                && pixel[1].abs_diff(pixel[2]) < 12
        }) {
            return Err(format!("shortcut hint is missing at {percent}%").into());
        }
        if !row_has(&menu, &capture, |pixel| {
            pixel[3] == 255 && pixel[0] < 60 && pixel[1] < 60 && pixel[2] < 60
        }) {
            return Err(format!("menu label is missing at {percent}%").into());
        }
        let (_, line) = layout.separators[0];
        let separator = menu
            .pixel(line.x + line.width / 2, line.y)
            .ok_or("separator outside bitmap")?;
        if separator != [0xFD, 0xE3, 0xD3, 255] {
            return Err(format!("separator colour changed at {percent}%: {separator:?}").into());
        }
    }
    Ok(())
}
/// Whether any pixel of a menu row satisfies `predicate`.
fn row_has(
    menu: &popup_menu::MenuBitmap,
    row: &popup_menu::RowLayout,
    predicate: impl Fn([u8; 4]) -> bool,
) -> bool {
    (row.band.y..row.band.bottom())
        .any(|y| (row.text_left..row.text_right).any(|x| menu.pixel(x, y).is_some_and(&predicate)))
}
fn write_menu_png(
    path: &std::path::Path,
    menu: &popup_menu::MenuBitmap,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut encoder = png::Encoder::new(
        std::fs::File::create(path)?,
        menu.width as u32,
        menu.height as u32,
    );
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()?
        .write_image_data(&menu.straight_rgba())?;
    Ok(())
}
fn plugin_check() -> std::result::Result<(), Box<dyn std::error::Error>> {
    use snaplight_domain::*;
    use snaplight_plugin_api::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    let bytes = vec![42u8; 64 * 64 * 4];
    let name = format!(
        "Local\\Snaplight.Plugin.{}.{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let mapping = snaplight_platform::native::SharedPixels::create(name.clone(), &bytes)?;
    let library = std::env::current_exe()?
        .parent()
        .unwrap()
        .join("snaplight_example_plugin.dll");
    let request = RunRequest {
        library: library.to_string_lossy().into(),
        operation: "inspect".into(),
        image: ImageDescriptor {
            id: ImageId(1),
            width: 64,
            height: 64,
            stride: 256,
            format: PixelFormat::Bgra8,
            alpha: AlphaMode::Opaque,
            color_space: ColorSpace::Srgb,
        },
        mapping: name,
        bytes: bytes.len(),
        timeout_ms: 5000,
        gpu: None,
    };
    let mut child = Command::new(sibling("snaplight-plugin-host")?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    writeln!(
        child.stdin.take().unwrap(),
        "{}",
        serde_json::to_string(&Envelope::new(request))?
    )?;
    let output = child.wait_with_output()?;
    drop(mapping);
    if !output.status.success() {
        return Err("plugin host failed".into());
    }
    let response: Envelope<RunResponse> = serde_json::from_slice(&output.stdout)?;
    if response.version != PROTOCOL_VERSION || response.payload.status != STATUS_OK {
        return Err("plugin response failed".into());
    }
    let value: serde_json::Value = serde_json::from_str(&response.payload.result)?;
    if value["bytes"] != bytes.len() {
        return Err("plugin received incorrect shared image".into());
    }
    snaplight_platform::atomic_write(&data_dir().join("plugin-check.json"), &output.stdout)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tray_menu_follows_the_reference_layout() {
        let entries = tray_menu_entries(DEFAULT_CAPTURE_HOTKEY);
        assert_eq!(entries.len(), 5);
        let divider = entries
            .iter()
            .position(|entry| matches!(entry, Entry::Separator))
            .expect("the menu needs a divider");
        assert_eq!(divider, 3, "divider sits between the pin command and quit");
        for index in [
            tray_item::CAPTURE,
            tray_item::SETTINGS,
            tray_item::CLOSE_PINS,
            tray_item::QUIT,
        ] {
            assert_ne!(index, divider, "actions never point at the divider");
            assert!(matches!(entries[index], Entry::Item(_)));
        }
        let Entry::Item(capture) = &entries[tray_item::CAPTURE] else {
            panic!("capture entry must be an item");
        };
        assert_eq!(capture.label, "截图");
        assert_eq!(capture.shortcut.as_deref(), Some(DEFAULT_CAPTURE_HOTKEY));
        assert!(!capture.danger);
        let Entry::Item(close) = &entries[tray_item::CLOSE_PINS] else {
            panic!("close-all entry must be an item");
        };
        assert_eq!(close.label, "关闭所有贴图");
        assert!(
            close.danger,
            "close-all must be drawn as a destructive entry"
        );
        let Entry::Item(quit) = &entries[tray_item::QUIT] else {
            panic!("quit entry must be an item");
        };
        assert_eq!(quit.label, "退出");
    }
}

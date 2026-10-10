use snaplight_domain::{
    parse_hotkey, Config, Envelope, PinCommand, DEFAULT_CAPTURE_HOTKEY, PROTOCOL_VERSION,
};
use snaplight_platform::{data_dir, load_config, sibling};
use std::{
    io::{BufRead, BufReader, Write},
    os::windows::io::{AsHandle, AsRawHandle, OwnedHandle},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
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
struct State {
    pins: Option<Child>,
    queue: Arc<Mutex<Vec<PinCommand>>>,
    editing: Arc<AtomicBool>,
    editor_handle: Arc<Mutex<Option<OwnedHandle>>>,
}
unsafe fn launch_editor(hwnd: HWND, state: &State, settings: bool) {
    if state.editing.swap(true, Ordering::AcqRel) {
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
                    state.editing.store(false, Ordering::Release);
                    snaplight_platform::native::show_error(&e.to_string());
                    return;
                }
            };
            *state.editor_handle.lock().unwrap() = Some(handle);
            let queue = state.queue.clone();
            let editing = state.editing.clone();
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
                editing.store(false, Ordering::Release);
                if status.is_ok_and(|s| !s.success()) {
                    snaplight_platform::native::show_error(&format!("Editor failed: {message}"));
                }
            });
        }
        Err(e) => {
            state.editing.store(false, Ordering::Release);
            snaplight_platform::native::show_error(&e.to_string());
        }
    }
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
                        if let Ok(menu) = CreatePopupMenu() {
                            let _ = AppendMenuW(menu, MF_STRING, 1, w!("Capture"));
                            let _ = AppendMenuW(menu, MF_STRING, 2, w!("Settings"));
                            let _ = AppendMenuW(menu, MF_STRING, 3, w!("Close all pins"));
                            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
                            let _ = AppendMenuW(menu, MF_STRING, 4, w!("Exit"));
                            let mut p = POINT::default();
                            let _ = GetCursorPos(&mut p);
                            let _ = SetForegroundWindow(hwnd);
                            let choice = TrackPopupMenu(
                                menu,
                                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                                p.x,
                                p.y,
                                0,
                                hwnd,
                                None,
                            )
                            .0;
                            let _ = DestroyMenu(menu);
                            match choice {
                                1 => launch_editor(hwnd, state, false),
                                2 => launch_editor(hwnd, state, true),
                                3 => {
                                    let _ = send_pin(state, PinCommand::CloseAll);
                                }
                                4 => {
                                    let _ = DestroyWindow(hwnd);
                                }
                                _ => {}
                            }
                        }
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
            editing: Arc::new(AtomicBool::new(false)),
            editor_handle: Arc::new(Mutex::new(None)),
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
        let mut icon = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: TRAY,
            hIcon: LoadIconW(None, IDI_APPLICATION)?,
            ..Default::default()
        };
        let tip: Vec<u16> = "Snaplight - Ctrl+Shift+A".encode_utf16().collect();
        icon.szTip[..tip.len()].copy_from_slice(&tip);
        if !Shell_NotifyIconW(NIM_ADD, &icon).as_bool() {
            let _ = DestroyWindow(hwnd);
            return Err(Error::from_win32().into());
        }
        if std::env::args().any(|a| a == "--smoke") {
            DestroyWindow(hwnd)?;
        } else {
            snaplight_platform::native::message_loop()?;
        }
        CloseHandle(mutex)?;
    }
    Ok(())
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

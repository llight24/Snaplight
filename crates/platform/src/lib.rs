use snaplight_domain::{Config, ImageDescriptor, WindowState};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default)]
pub struct Capabilities {
    pub capture: bool,
    pub global_hotkeys: bool,
    pub positioning: bool,
    pub topmost: bool,
    pub click_through: bool,
}
pub trait CaptureService {
    type Frame;
    type Error;
    fn capture(&mut self) -> Result<Vec<Self::Frame>, Self::Error>;
}
pub trait WindowService {
    type Window;
    type Error;
    fn capabilities(&self) -> Capabilities;
    fn set_click_through(&self, window: &Self::Window, enabled: bool) -> Result<(), Self::Error>;
}
pub trait HotkeyService {
    type Error;
    fn register(&mut self, id: i32, modifiers: u32, key: u32) -> Result<(), Self::Error>;
}
pub trait ClipboardService {
    type Error;
    fn write_image(&self, image: &ImageDescriptor, bytes: &[u8]) -> Result<(), Self::Error>;
}

pub fn data_dir() -> PathBuf {
    std::env::var_os("SNAPLIGHT_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
                .join("Snaplight")
        })
}
pub fn load_config() -> Result<Config, Box<dyn std::error::Error>> {
    let p = data_dir().join("config.json");
    let config = if p.exists() {
        serde_json::from_slice(&fs::read(p)?)?
    } else {
        Config::default()
    };
    config.validate()?;
    Ok(config)
}
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    #[cfg(windows)]
    {
        use windows::{
            core::HSTRING,
            Win32::Storage::FileSystem::{
                MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
            },
        };
        unsafe {
            MoveFileExW(
                &HSTRING::from(tmp.as_os_str()),
                &HSTRING::from(path.as_os_str()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
            .map_err(std::io::Error::other)?;
        }
    }
    #[cfg(not(windows))]
    fs::rename(tmp, path)?;
    Ok(())
}
pub fn save_config(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    config.validate()?;
    atomic_write(
        &data_dir().join("config.json"),
        &serde_json::to_vec_pretty(config)?,
    )?;
    Ok(())
}
/// Directory that receives saved captures, honoring the configured override.
pub fn capture_dir(config: &Config) -> PathBuf {
    config
        .capture_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir().join("captures"))
}
/// Window positions are cosmetic state: a missing, malformed, or
/// future-versioned file must not stop the tool windows from opening, so this
/// falls back to unset positions instead of failing like `load_config`.
pub fn load_window_state() -> WindowState {
    let path = data_dir().join("ui-state.json");
    fs::read(path)
        .map(|bytes| parse_window_state(&bytes))
        .unwrap_or_default()
}
fn parse_window_state(bytes: &[u8]) -> WindowState {
    serde_json::from_slice(bytes)
        .ok()
        .filter(|state: &WindowState| state.validate().is_ok())
        .unwrap_or_default()
}
pub fn save_window_state(state: &WindowState) -> std::io::Result<()> {
    atomic_write(
        &data_dir().join("ui-state.json"),
        &serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?,
    )
}
pub fn write_pixels(path: &Path, image: &ImageDescriptor, bytes: &[u8]) -> std::io::Result<()> {
    let expected = image.byte_len().map_err(std::io::Error::other)?;
    if expected != bytes.len() {
        return Err(std::io::Error::other("pixel buffer length mismatch"));
    }
    let header = serde_json::to_vec(image)?;
    if let Some(p) = path.parent() {
        fs::create_dir_all(p)?;
    }
    let mut f = fs::File::create(path)?;
    f.write_all(b"SNAPBG01")?;
    f.write_all(&(header.len() as u32).to_le_bytes())?;
    f.write_all(&header)?;
    f.write_all(bytes)?;
    f.sync_all()
}
pub fn read_pixels(path: &Path) -> Result<(ImageDescriptor, Vec<u8>), Box<dyn std::error::Error>> {
    let mut f = fs::File::open(path)?;
    let mut magic = [0; 8];
    f.read_exact(&mut magic)?;
    if &magic != b"SNAPBG01" {
        return Err("invalid image file".into());
    }
    let mut n = [0; 4];
    f.read_exact(&mut n)?;
    let n = u32::from_le_bytes(n) as usize;
    if n > 4096 {
        return Err("oversized image header".into());
    }
    let mut h = vec![0; n];
    f.read_exact(&mut h)?;
    let image: ImageDescriptor = serde_json::from_slice(&h)?;
    let size = image.byte_len()?;
    if size > 1024 * 1024 * 1024 {
        return Err("image exceeds 1 GiB per decode safety limit".into());
    }
    let mut bytes = vec![0; size];
    f.read_exact(&mut bytes)?;
    Ok((image, bytes))
}
pub fn sibling(name: &str) -> std::io::Result<PathBuf> {
    Ok(std::env::current_exe()?
        .parent()
        .ok_or_else(|| std::io::Error::other("missing executable directory"))?
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
}
/// One saved capture: the timestamp-named group of files the editor writes
/// (`<nanos>.png` or `<nanos>.bgra`, `<nanos>.source.bgra`,
/// `<nanos>.document.json`, `<nanos>.pin.json`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureEntry {
    pub id: String,
    /// Whether a flattened PNG was saved for this capture.
    pub png: bool,
    /// Total size of the capture's files.
    pub bytes: u64,
    /// Newest modification time among the capture's files.
    pub modified: Option<std::time::SystemTime>,
}
fn capture_id(file_name: &str) -> Option<&str> {
    let id = file_name.split('.').next()?;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}
/// Lists saved captures in `dir`, newest first. Files that do not follow the
/// editor's numeric naming are ignored, so a shared folder stays usable.
pub fn list_captures(dir: &Path) -> Vec<CaptureEntry> {
    let mut entries: Vec<CaptureEntry> = Vec::new();
    let Ok(files) = fs::read_dir(dir) else {
        return entries;
    };
    for file in files.flatten() {
        let name = file.file_name().to_string_lossy().into_owned();
        let Some(id) = capture_id(&name) else {
            continue;
        };
        let Ok(metadata) = file.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let index = match entries.iter().position(|entry| entry.id == id) {
            Some(index) => index,
            None => {
                entries.push(CaptureEntry {
                    id: id.to_string(),
                    png: false,
                    bytes: 0,
                    modified: None,
                });
                entries.len() - 1
            }
        };
        let entry = &mut entries[index];
        entry.png |= name.ends_with(".png");
        entry.bytes += metadata.len();
        if let Ok(modified) = metadata.modified() {
            entry.modified = Some(entry.modified.map_or(modified, |known| known.max(modified)));
        }
    }
    // Ids are nanosecond timestamps; longer ids are newer, which keeps the order
    // correct even if the clock produced different digit counts.
    entries.sort_by(|a, b| b.id.len().cmp(&a.id.len()).then_with(|| b.id.cmp(&a.id)));
    entries
}
/// Every file that belongs to the capture `id`.
pub fn capture_files(dir: &Path, id: &str) -> Vec<PathBuf> {
    let Ok(files) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = files
        .flatten()
        .filter(|file| capture_id(&file.file_name().to_string_lossy()) == Some(id))
        .map(|file| file.path())
        .collect();
    paths.sort();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use snaplight_domain::*;
    #[test]
    fn pixels_round_trip_and_reject_truncation() {
        let path =
            std::env::temp_dir().join(format!("snaplight-pixels-test-{}.bgra", std::process::id()));
        let image = ImageDescriptor {
            id: ImageId(7),
            width: 2,
            height: 2,
            stride: 8,
            format: PixelFormat::Bgra8,
            alpha: AlphaMode::Opaque,
            color_space: ColorSpace::Srgb,
        };
        let bytes = vec![42; 16];
        write_pixels(&path, &image, &bytes).unwrap();
        let (decoded, data) = read_pixels(&path).unwrap();
        assert_eq!(decoded.id, image.id);
        assert_eq!(data, bytes);
        let mut raw = fs::read(&path).unwrap();
        raw.pop();
        fs::write(&path, raw).unwrap();
        assert!(read_pixels(&path).is_err());
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn captures_are_grouped_and_listed_newest_first() {
        let dir = std::env::temp_dir().join(format!("snaplight-captures-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("1000.png"), [0u8; 4]).unwrap();
        fs::write(dir.join("1000.document.json"), b"{}").unwrap();
        fs::write(dir.join("1000.source.bgra"), [0u8; 8]).unwrap();
        fs::write(dir.join("2000.bgra"), [0u8; 16]).unwrap();
        fs::write(dir.join("notes.txt"), b"ignore me").unwrap();
        fs::create_dir_all(dir.join("3000.png")).unwrap();
        let entries = list_captures(&dir);
        assert_eq!(
            entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["2000", "1000"]
        );
        assert!(!entries[0].png);
        assert!(entries[1].png);
        // The size covers every file of the capture, including its metadata.
        assert_eq!(entries[1].bytes, 4 + 2 + 8);
        assert!(entries.iter().all(|e| e.modified.is_some()));
        assert_eq!(capture_files(&dir, "1000").len(), 3);
        assert!(capture_files(&dir, "9999").is_empty());
        // A missing directory lists nothing instead of failing.
        assert!(list_captures(&dir.join("missing")).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn capture_dir_prefers_the_configured_override() {
        let mut config = Config::default();
        assert_eq!(capture_dir(&config), data_dir().join("captures"));
        config.capture_dir = Some("D:\\Snaplight\\shots".into());
        assert_eq!(capture_dir(&config), PathBuf::from("D:\\Snaplight\\shots"));
    }
    #[test]
    fn window_state_falls_back_to_defaults_when_unusable() {
        let state = WindowState {
            toolbar: Some(snaplight_domain::ScreenPoint { x: -1920, y: 40 }),
            ..WindowState::default()
        };
        let bytes = serde_json::to_vec(&state).unwrap();
        assert_eq!(parse_window_state(&bytes).toolbar, state.toolbar);
        assert_eq!(parse_window_state(b"{").toolbar, None);
        assert_eq!(
            parse_window_state(br#"{"version":7,"toolbar":{"x":1,"y":2}}"#).toolbar,
            None
        );
    }
    #[test]
    fn atomic_replacement_preserves_complete_file() {
        let path =
            std::env::temp_dir().join(format!("snaplight-atomic-test-{}.json", std::process::id()));
        atomic_write(&path, b"old").unwrap();
        atomic_write(&path, b"new-content").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new-content");
        fs::remove_file(path).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn hotkeys_map_to_win32_flags_and_virtual_keys() {
        use windows::Win32::UI::Input::KeyboardAndMouse::*;
        let hotkey = snaplight_domain::parse_hotkey(snaplight_domain::DEFAULT_CAPTURE_HOTKEY)
            .expect("default capture hotkey");
        let (modifiers, key) = native::hotkey_registration(&hotkey);
        assert_eq!(modifiers, MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT);
        assert_eq!(key, 'A' as u32);
        let hotkey = snaplight_domain::parse_hotkey("Alt+Win+F12").expect("function hotkey");
        let (modifiers, key) = native::hotkey_registration(&hotkey);
        assert_eq!(modifiers, MOD_ALT | MOD_WIN | MOD_NOREPEAT);
        assert_eq!(key, 0x7b);
        let hotkey = snaplight_domain::parse_hotkey("Ctrl+7").expect("digit hotkey");
        assert_eq!(native::hotkey_registration(&hotkey).1, '7' as u32);
    }
    #[cfg(windows)]
    #[test]
    fn shared_pixels_are_read_only_in_receiver_and_size_checked() {
        use native::SharedPixels;
        let name = format!("Local\\Snaplight.Test.{}", std::process::id());
        let writer = SharedPixels::create(name.clone(), &[1, 2, 3, 4]).unwrap();
        let reader = SharedPixels::open(name.clone(), 4).unwrap();
        assert_eq!(reader.bytes(), writer.bytes());
        assert!(SharedPixels::open(name, 0).is_err());
    }
}

#[cfg(windows)]
pub mod native {
    use snaplight_domain::{Hotkey, HotkeyKey};
    use std::path::PathBuf;
    use std::ptr::NonNull;
    use windows::{
        core::*,
        Win32::{
            Foundation::*,
            System::{Memory::*, Threading::*},
            UI::{HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
        },
    };
    /// Translates a parsed hotkey into Win32 registration flags (including
    /// `MOD_NOREPEAT`) and a virtual-key code.
    pub fn hotkey_registration(hotkey: &Hotkey) -> (HOT_KEY_MODIFIERS, u32) {
        let mut modifiers = MOD_NOREPEAT;
        if hotkey.ctrl {
            modifiers |= MOD_CONTROL;
        }
        if hotkey.shift {
            modifiers |= MOD_SHIFT;
        }
        if hotkey.alt {
            modifiers |= MOD_ALT;
        }
        if hotkey.win {
            modifiers |= MOD_WIN;
        }
        let key = match hotkey.key {
            HotkeyKey::Letter(c) | HotkeyKey::Digit(c) => c as u32,
            HotkeyKey::Function(n) => 0x70 + u32::from(n) - 1,
        };
        (modifiers, key)
    }
    /// Moves files to the recycle bin, so a mistaken deletion in the capture
    /// list can be undone. Returns an error instead of silently deleting when
    /// the shell refuses the operation.
    pub fn delete_to_recycle_bin(paths: &[PathBuf]) -> Result<()> {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::UI::Shell::*;
        if paths.is_empty() {
            return Ok(());
        }
        unsafe {
            let mut from: Vec<u16> = Vec::new();
            for path in paths {
                from.extend(path.as_os_str().encode_wide());
                from.push(0);
            }
            from.push(0);
            let mut operation = SHFILEOPSTRUCTW {
                wFunc: FO_DELETE,
                pFrom: PCWSTR(from.as_ptr()),
                fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI).0 as u16,
                ..Default::default()
            };
            let result = SHFileOperationW(&mut operation);
            if result != 0 {
                return Err(Error::from_hresult(HRESULT::from_win32(result as u32)));
            }
            if operation.fAnyOperationsAborted.as_bool() {
                return Err(Error::new(E_ABORT, "recycle bin operation aborted"));
            }
        }
        Ok(())
    }
    /// Formats a file timestamp as local `YYYY-MM-DD HH:MM` for the capture list.
    pub fn format_local_time(time: std::time::SystemTime) -> String {
        use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
        use windows::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
        use windows::Win32::System::Time::FileTimeToSystemTime;
        let since_epoch = time
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        // 100-nanosecond ticks since 1601-01-01 plus the Unix epoch offset.
        const EPOCH_OFFSET_TICKS: u64 = 116_444_736_000_000_000;
        let ticks = EPOCH_OFFSET_TICKS + since_epoch.as_nanos() as u64 / 100;
        let file_time = FILETIME {
            dwLowDateTime: ticks as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
        unsafe {
            let mut local = FILETIME::default();
            let mut system = SYSTEMTIME::default();
            if FileTimeToLocalFileTime(&file_time, &mut local).is_err()
                || FileTimeToSystemTime(&local, &mut system).is_err()
            {
                return "unknown time".into();
            }
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}",
                system.wYear, system.wMonth, system.wDay, system.wHour, system.wMinute
            )
        }
    }
    pub fn dpi_aware() {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
    }
    pub fn show_error(text: &str) {
        if std::env::var_os("SNAPLIGHT_NONINTERACTIVE").is_some() {
            eprintln!("{text}");
            return;
        }
        unsafe {
            MessageBoxW(
                None,
                &HSTRING::from(text),
                w!("Snaplight"),
                MB_OK | MB_ICONERROR,
            );
        }
    }
    pub fn message_loop() -> Result<()> {
        unsafe {
            let mut msg = MSG::default();
            loop {
                let r = GetMessageW(&mut msg, None, 0, 0).0;
                if r == -1 {
                    return Err(Error::from_win32());
                }
                if r == 0 {
                    return Ok(());
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
    pub fn singleton(name: &str) -> Result<Option<HANDLE>> {
        unsafe {
            let handle = CreateMutexW(None, false, &HSTRING::from(name))?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(handle)?;
                Ok(None)
            } else {
                Ok(Some(handle))
            }
        }
    }
    pub struct SharedPixels {
        handle: HANDLE,
        address: NonNull<u8>,
        pub len: usize,
        pub name: String,
    }
    impl SharedPixels {
        pub fn create(name: String, bytes: &[u8]) -> Result<Self> {
            unsafe {
                if bytes.is_empty() {
                    return Err(Error::new(E_INVALIDARG, "empty shared image"));
                }
                let n = bytes.len() as u64;
                let handle = CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    None,
                    PAGE_READWRITE,
                    (n >> 32) as u32,
                    n as u32,
                    &HSTRING::from(&name),
                )?;
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    let _ = CloseHandle(handle);
                    return Err(Error::new(E_FAIL, "mapping name collision"));
                }
                let view = MapViewOfFile(handle, FILE_MAP_WRITE, 0, 0, bytes.len());
                let Some(address) = NonNull::new(view.Value as *mut u8) else {
                    let _ = CloseHandle(handle);
                    return Err(Error::from_win32());
                };
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), address.as_ptr(), bytes.len());
                Ok(Self {
                    handle,
                    address,
                    len: bytes.len(),
                    name,
                })
            }
        }
        pub fn open(name: String, len: usize) -> Result<Self> {
            unsafe {
                if len == 0 || len > 1024 * 1024 * 1024 {
                    return Err(Error::new(E_INVALIDARG, "invalid mapping size"));
                }
                let handle = OpenFileMappingW(FILE_MAP_READ.0, false, &HSTRING::from(&name))?;
                let view = MapViewOfFile(handle, FILE_MAP_READ, 0, 0, len);
                let Some(address) = NonNull::new(view.Value as *mut u8) else {
                    let _ = CloseHandle(handle);
                    return Err(Error::from_win32());
                };
                Ok(Self {
                    handle,
                    address,
                    len,
                    name,
                })
            }
        }
        pub fn bytes(&self) -> &[u8] {
            unsafe { std::slice::from_raw_parts(self.address.as_ptr(), self.len) }
        }
    }
    impl Drop for SharedPixels {
        fn drop(&mut self) {
            unsafe {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.address.as_ptr() as _,
                });
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

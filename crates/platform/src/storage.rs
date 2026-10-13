//! File persistence: data directory resolution, config and window-state
//! files, pixel payloads, and saved-capture listing.

use snaplight_domain::{Config, ImageDescriptor, WindowState};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

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
}

use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    time::Instant,
};

pub const PROTOCOL_VERSION: u32 = 1;
/// Default global shortcut that opens the capture overlay.
pub const DEFAULT_CAPTURE_HOTKEY: &str = "Ctrl+Shift+A";
/// Default global shortcut that restores interaction for click-through pins.
pub const DEFAULT_RESTORE_HOTKEY: &str = "Ctrl+Shift+P";

/// Key part of a parsed hotkey. Letters and digits map to their ASCII virtual-key
/// code; functions map to `VK_F1`..`VK_F24`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyKey {
    Letter(char),
    Digit(char),
    Function(u8),
}
/// Parsed global shortcut. Modifiers are stored individually so the platform
/// layer can translate them to its own flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
    pub key: HotkeyKey,
}
impl Hotkey {
    /// Canonical spelling (`Ctrl+Shift+A`) used for storage and display.
    pub fn canonical(&self) -> String {
        let mut text = String::new();
        for (enabled, name) in [
            (self.ctrl, "Ctrl"),
            (self.shift, "Shift"),
            (self.alt, "Alt"),
            (self.win, "Win"),
        ] {
            if enabled {
                text.push_str(name);
                text.push('+');
            }
        }
        match self.key {
            HotkeyKey::Letter(c) | HotkeyKey::Digit(c) => text.push(c),
            HotkeyKey::Function(n) => text.push_str(&format!("F{n}")),
        }
        text
    }
}
/// Parses `Ctrl+Shift+A`-style text. Modifiers are case-insensitive, may appear
/// in any order, and at least one is required; supported keys are A-Z, 0-9, and
/// F1-F24.
pub fn parse_hotkey(text: &str) -> Result<Hotkey, &'static str> {
    let parts: Vec<&str> = text.split('+').map(str::trim).collect();
    let (key_text, modifiers) = parts.split_last().ok_or("empty hotkey")?;
    let mut hotkey = Hotkey {
        ctrl: false,
        shift: false,
        alt: false,
        win: false,
        key: HotkeyKey::Letter('A'),
    };
    for modifier in modifiers {
        match modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => hotkey.ctrl = true,
            "shift" => hotkey.shift = true,
            "alt" => hotkey.alt = true,
            "win" | "super" | "meta" => hotkey.win = true,
            "" => return Err("empty hotkey modifier"),
            _ => return Err("unknown hotkey modifier"),
        }
    }
    if !(hotkey.ctrl || hotkey.shift || hotkey.alt || hotkey.win) {
        return Err("hotkey needs at least one modifier");
    }
    hotkey.key = parse_hotkey_key(key_text)?;
    Ok(hotkey)
}
fn parse_hotkey_key(text: &str) -> Result<HotkeyKey, &'static str> {
    let upper = text.to_ascii_uppercase();
    let mut chars = upper.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphabetic() {
            return Ok(HotkeyKey::Letter(c));
        }
        if c.is_ascii_digit() {
            return Ok(HotkeyKey::Digit(c));
        }
    }
    if let Some(number) = upper.strip_prefix('F') {
        if let Ok(number) = number.parse::<u8>() {
            if (1..=24).contains(&number) {
                return Ok(HotkeyKey::Function(number));
            }
        }
    }
    Err("unsupported hotkey key")
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ImagePoint {
    pub x: f32,
    pub y: f32,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScreenPoint {
    pub x: i32,
    pub y: i32,
}
/// Rectangle in screen physical pixels, matching `ScreenPoint`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}
impl ScreenRect {
    pub fn right(&self) -> i32 {
        self.x + self.width
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.height
    }
    /// Overlap of two rectangles, or `None` when they do not overlap.
    pub fn intersect(self, other: Self) -> Option<Self> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        (right > x && bottom > y).then_some(Self {
            x,
            y,
            width: right - x,
            height: bottom - y,
        })
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LogicalPoint {
    pub x: f32,
    pub y: f32,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ImageRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}
impl ImageRect {
    pub fn between(a: ImagePoint, b: ImagePoint) -> Self {
        Self {
            x: a.x.min(b.x),
            y: a.y.min(b.y),
            width: (a.x - b.x).abs(),
            height: (a.y - b.y).abs(),
        }
    }
    pub fn contains(&self, p: ImagePoint) -> bool {
        p.x >= self.x && p.y >= self.y && p.x <= self.x + self.width && p.y <= self.y + self.height
    }
    pub fn clamp(self, width: u32, height: u32) -> Self {
        let x = self.x.clamp(0.0, width as f32);
        let y = self.y.clamp(0.0, height as f32);
        Self {
            x,
            y,
            width: (self.x + self.width).clamp(x, width as f32) - x,
            height: (self.y + self.height).clamp(y, height as f32) - y,
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct CoordinateMap {
    pub screen_origin: ScreenPoint,
    pub scale: f32,
}
impl CoordinateMap {
    pub fn screen_to_image(self, p: ScreenPoint) -> ImagePoint {
        ImagePoint {
            x: (p.x - self.screen_origin.x) as f32,
            y: (p.y - self.screen_origin.y) as f32,
        }
    }
    pub fn logical_to_image(self, p: LogicalPoint) -> ImagePoint {
        ImagePoint {
            x: p.x * self.scale,
            y: p.y * self.scale,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ImageId(pub u64);
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum PixelFormat {
    Bgra8,
    Rgba8,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum AlphaMode {
    Opaque,
    Premultiplied,
    Straight,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum ColorSpace {
    Srgb,
    HdrConvertedToSdr,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageDescriptor {
    pub id: ImageId,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
    pub alpha: AlphaMode,
    pub color_space: ColorSpace,
}
impl ImageDescriptor {
    pub fn byte_len(&self) -> Result<usize, &'static str> {
        if self.width == 0
            || self.height == 0
            || self.width.checked_mul(4).is_none_or(|n| n > self.stride)
        {
            return Err("invalid image dimensions/stride");
        }
        (self.stride as usize)
            .checked_mul(self.height as usize)
            .ok_or("image size overflow")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tool {
    Select,
    Rectangle,
    Ellipse,
    Arrow,
    Pen,
    Mosaic,
    Text,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Style {
    pub rgba: [f32; 4],
    pub width: f32,
}
impl Default for Style {
    fn default() -> Self {
        Self {
            rgba: [0.95, 0.2, 0.25, 1.0],
            width: 3.0,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Shape {
    Rectangle(ImageRect),
    Ellipse(ImageRect),
    Arrow {
        from: ImagePoint,
        to: ImagePoint,
    },
    Pen(Vec<ImagePoint>),
    Mosaic {
        rect: ImageRect,
        block: u32,
    },
    Text {
        at: ImagePoint,
        text: String,
        size: f32,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Annotation {
    pub shape: Shape,
    pub style: Style,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Document {
    pub version: u32,
    pub image: ImageDescriptor,
    pub selection: Option<ImageRect>,
    pub annotations: Vec<Annotation>,
}
#[derive(Clone, Debug)]
pub enum Edit {
    Add(Annotation),
    Selection(Option<ImageRect>),
}
#[derive(Clone, Debug)]
enum Reversal {
    Add(Annotation),
    Selection(Option<ImageRect>),
    RemoveLast,
}
#[derive(Default)]
pub struct History {
    undo: Vec<Reversal>,
    redo: Vec<Reversal>,
}
impl History {
    fn apply(doc: &mut Document, edit: Reversal) -> Reversal {
        match edit {
            Reversal::Add(a) => {
                doc.annotations.push(a);
                Reversal::RemoveLast
            }
            Reversal::RemoveLast => {
                Reversal::Add(doc.annotations.pop().expect("history invariant"))
            }
            Reversal::Selection(s) => Reversal::Selection(std::mem::replace(&mut doc.selection, s)),
        }
    }
    pub fn execute(&mut self, doc: &mut Document, edit: Edit) {
        let reversal = match edit {
            Edit::Add(a) => Reversal::Add(a),
            Edit::Selection(s) => Reversal::Selection(s),
        };
        self.undo.push(Self::apply(doc, reversal));
        self.redo.clear();
    }
    pub fn undo(&mut self, doc: &mut Document) -> bool {
        if let Some(e) = self.undo.pop() {
            self.redo.push(Self::apply(doc, e));
            true
        } else {
            false
        }
    }
    /// True when there is an edit to reverse.
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    /// True when there is a reversed edit to reapply.
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    pub fn redo(&mut self, doc: &mut Document) -> bool {
        if let Some(e) = self.redo.pop() {
            self.undo.push(Self::apply(doc, e));
            true
        } else {
            false
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct Interaction {
    pub draft: Option<Annotation>,
    pub selection_draft: Option<ImageRect>,
    /// Candidate region detected under the cursor before it is committed.
    /// Renderers show it as a preview; it never changes the document.
    pub hover: Option<ImageRect>,
    pub cursor: Option<ImagePoint>,
    pub dragging: bool,
}
#[derive(Clone, Copy, Debug)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
    pub dpi: f32,
    /// Screen position of the image origin, so chrome can report screen pixels.
    pub origin: ScreenPoint,
}
pub trait CanvasRenderer {
    type Error;
    fn render(
        &mut self,
        document: &Document,
        viewport: Viewport,
        interaction: &Interaction,
    ) -> Result<(), Self::Error>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Theme {
    pub background: String,
    pub foreground: String,
    pub accent: String,
}
/// How the editor picks a region automatically while the cursor moves.
///
/// Only UI Automation detection exists today; the enum keeps room for a
/// pixel-based mode without changing the stored configuration shape.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowDetect {
    /// Never detect; regions are always drawn by hand.
    Off,
    /// Highlight the UI Automation element under the cursor.
    #[default]
    Ui,
}
impl Default for Theme {
    fn default() -> Self {
        Self {
            background: "#202124".into(),
            foreground: "#f4f5f6".into(),
            accent: "#36c9a0".into(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub theme: Theme,
    pub tools: Vec<Tool>,
    pub stroke_width: f32,
    pub mosaic_block: u32,
    pub cpu_cache_bytes: usize,
    pub gpu_pins: bool,
    /// Directory for saved captures. `None` uses `data_dir()/captures`.
    pub capture_dir: Option<String>,
    /// Global shortcut that opens the capture overlay.
    pub capture_hotkey: String,
    /// Global shortcut that restores interaction for click-through pins.
    pub restore_hotkey: String,
    /// Automatic region detection under the cursor.
    pub window_detect: WindowDetect,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            theme: Theme::default(),
            tools: vec![
                Tool::Select,
                Tool::Rectangle,
                Tool::Ellipse,
                Tool::Arrow,
                Tool::Pen,
                Tool::Mosaic,
                Tool::Text,
            ],
            stroke_width: 3.0,
            mosaic_block: 12,
            cpu_cache_bytes: 8 * 1024 * 1024,
            gpu_pins: false,
            capture_dir: None,
            capture_hotkey: DEFAULT_CAPTURE_HOTKEY.into(),
            restore_hotkey: DEFAULT_RESTORE_HOTKEY.into(),
            window_detect: WindowDetect::default(),
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.version != 1 {
            return Err("unsupported config version");
        }
        if !self.stroke_width.is_finite()
            || !(1.0..=64.0).contains(&self.stroke_width)
            || !(2..=128).contains(&self.mosaic_block)
        {
            return Err("invalid drawing settings");
        }
        for c in [
            &self.theme.background,
            &self.theme.foreground,
            &self.theme.accent,
        ] {
            if c.len() != 7 || !c.starts_with('#') || !c[1..].bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err("theme colors must be #RRGGBB");
            }
        }
        if let Some(dir) = &self.capture_dir {
            if dir.trim().is_empty() || dir.contains('\0') {
                return Err("capture directory must not be empty");
            }
        }
        parse_hotkey(&self.capture_hotkey).map_err(|_| "invalid capture hotkey")?;
        parse_hotkey(&self.restore_hotkey).map_err(|_| "invalid restore hotkey")?;
        Ok(())
    }
}
/// Persisted position of the Slint tool windows. Positions are screen physical
/// coordinates, so a saved window can be restored on the same display layout.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowState {
    pub version: u32,
    pub toolbar: Option<ScreenPoint>,
    pub settings: Option<ScreenPoint>,
}
impl Default for WindowState {
    fn default() -> Self {
        Self {
            version: 1,
            toolbar: None,
            settings: None,
        }
    }
}
impl WindowState {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.version != 1 {
            return Err("unsupported window state version");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub version: u32,
    pub payload: T,
}
impl<T> Envelope<T> {
    pub fn new(payload: T) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            payload,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PinCommand {
    /// `screen` is the physical screen position of the flattened image's top-left
    /// pixel, so the pin opens exactly where the region was captured instead of
    /// jumping to the pointer.
    Open {
        image: PathBuf,
        document: PathBuf,
        screen: ScreenPoint,
    },
    CloseAll,
    Shutdown,
}

pub struct ByteCache {
    budget: usize,
    used: usize,
    order: VecDeque<ImageId>,
    items: HashMap<ImageId, Vec<u8>>,
}
impl ByteCache {
    pub fn new(budget: usize) -> Self {
        Self {
            budget,
            used: 0,
            order: VecDeque::new(),
            items: HashMap::new(),
        }
    }
    pub fn insert(&mut self, id: ImageId, data: Vec<u8>) {
        if let Some(old) = self.items.remove(&id) {
            self.used -= old.len();
            self.order.retain(|x| *x != id);
        }
        if data.len() > self.budget {
            return;
        }
        while self.used + data.len() > self.budget {
            if let Some(key) = self.order.pop_front() {
                self.used -= self.items.remove(&key).unwrap().len();
            }
        }
        self.used += data.len();
        self.items.insert(id, data);
        self.order.push_back(id);
    }
    pub fn get(&mut self, id: ImageId) -> Option<&[u8]> {
        if self.items.contains_key(&id) {
            self.order.retain(|x| *x != id);
            self.order.push_back(id);
        }
        self.items.get(&id).map(Vec::as_slice)
    }
    pub fn used(&self) -> usize {
        self.used
    }
}
#[derive(Default)]
pub struct FrameDemand {
    dirty: bool,
    pending: bool,
}
impl FrameDemand {
    pub fn invalidate(&mut self) -> bool {
        self.dirty = true;
        if self.pending {
            false
        } else {
            self.pending = true;
            true
        }
    }
    pub fn take(&mut self) -> bool {
        self.pending = false;
        std::mem::take(&mut self.dirty)
    }
}
pub struct FrameSample {
    pub started: Instant,
    pub cpu_us: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn doc() -> Document {
        Document {
            version: 1,
            image: ImageDescriptor {
                id: ImageId(1),
                width: 100,
                height: 100,
                stride: 400,
                format: PixelFormat::Bgra8,
                alpha: AlphaMode::Opaque,
                color_space: ColorSpace::Srgb,
            },
            selection: None,
            annotations: vec![],
        }
    }
    #[test]
    fn history_branches_and_selection() {
        let mut d = doc();
        let mut h = History::default();
        h.execute(&mut d, Edit::Selection(Some(ImageRect::default())));
        h.execute(
            &mut d,
            Edit::Add(Annotation {
                shape: Shape::Pen(vec![]),
                style: Style::default(),
            }),
        );
        assert!(h.undo(&mut d));
        assert!(d.annotations.is_empty());
        assert!(h.redo(&mut d));
        assert_eq!(d.annotations.len(), 1);
        h.undo(&mut d);
        h.execute(&mut d, Edit::Selection(None));
        assert!(!h.redo(&mut d));
        h.undo(&mut d);
        assert!(d.selection.is_some());
    }
    #[test]
    fn coordinates_preserve_negative_monitor_origin() {
        let c = CoordinateMap {
            screen_origin: ScreenPoint { x: -1920, y: -100 },
            scale: 1.5,
        };
        assert_eq!(
            c.screen_to_image(ScreenPoint { x: -1800, y: 0 }),
            ImagePoint { x: 120.0, y: 100.0 }
        );
        assert_eq!(
            c.logical_to_image(LogicalPoint { x: 10.0, y: 20.0 }),
            ImagePoint { x: 15.0, y: 30.0 }
        );
    }
    #[test]
    fn cache_evicts_lru_and_rejects_oversize() {
        let mut c = ByteCache::new(8);
        c.insert(ImageId(1), vec![0; 4]);
        c.insert(ImageId(2), vec![0; 4]);
        c.get(ImageId(1));
        c.insert(ImageId(3), vec![0; 4]);
        assert!(c.get(ImageId(2)).is_none());
        c.insert(ImageId(4), vec![0; 9]);
        assert_eq!(c.used(), 8);
    }
    #[test]
    fn demand_coalesces_without_losing_dirty() {
        let mut f = FrameDemand::default();
        assert!(f.invalidate());
        assert!(!f.invalidate());
        assert!(f.take());
        assert!(!f.take());
        assert!(f.invalidate());
    }
    #[test]
    fn invalid_stride_is_rejected() {
        let mut i = doc().image;
        i.stride = 1;
        assert!(i.byte_len().is_err());
    }
    #[test]
    fn hotkeys_parse_case_insensitively_and_round_trip() {
        let hotkey = parse_hotkey("ctrl + shift + a").unwrap();
        assert_eq!(
            hotkey,
            Hotkey {
                ctrl: true,
                shift: true,
                alt: false,
                win: false,
                key: HotkeyKey::Letter('A'),
            }
        );
        assert_eq!(hotkey.canonical(), DEFAULT_CAPTURE_HOTKEY);
        assert_eq!(
            parse_hotkey("Win+Alt+F12").unwrap().canonical(),
            "Alt+Win+F12"
        );
        assert_eq!(parse_hotkey("super+5").unwrap().key, HotkeyKey::Digit('5'));
    }
    #[test]
    fn invalid_hotkeys_are_rejected() {
        for text in [
            "",
            "A",
            "Ctrl+",
            "Ctrl+Shift",
            "Ctrl+Shift+F25",
            "Ctrl+Esc",
            "Foo+A",
        ] {
            assert!(parse_hotkey(text).is_err(), "{text} should be rejected");
        }
        // A configured hotkey that cannot be parsed fails config validation
        // instead of registering a different shortcut than the user asked for.
        let config = Config {
            capture_hotkey: "Ctrl+Shift".into(),
            ..Config::default()
        };
        assert_eq!(config.validate(), Err("invalid capture hotkey"));
        let config = Config {
            restore_hotkey: "not a hotkey".into(),
            ..Config::default()
        };
        assert_eq!(config.validate(), Err("invalid restore hotkey"));
        assert!(Config::default().validate().is_ok());
    }
    #[test]
    fn config_requires_a_usable_capture_directory() {
        let mut config = Config::default();
        assert!(config.validate().is_ok());
        assert_eq!(config.capture_dir, None);
        config.capture_dir = Some("   ".into());
        assert_eq!(
            config.validate(),
            Err("capture directory must not be empty")
        );
        config.capture_dir = Some("D:\\Snaplight\\shots".into());
        assert!(config.validate().is_ok());
    }
    #[test]
    fn window_detect_defaults_to_ui_and_stays_compatible() {
        assert_eq!(Config::default().window_detect, WindowDetect::Ui);
        // A configuration written before the option existed keeps the default.
        let config: Config = serde_json::from_str("{\"version\":1}").unwrap();
        assert_eq!(config.window_detect, WindowDetect::Ui);
        let config: Config = serde_json::from_str("{\"window_detect\":\"off\"}").unwrap();
        assert_eq!(config.window_detect, WindowDetect::Off);
        assert_eq!(serde_json::to_string(&WindowDetect::Ui).unwrap(), "\"ui\"");
        // An unknown mode is rejected instead of silently detecting anyway.
        assert!(serde_json::from_str::<Config>("{\"window_detect\":\"pixel\"}").is_err());
    }
    #[test]
    fn screen_rects_intersect_or_report_no_overlap() {
        let a = ScreenRect {
            x: -1920,
            y: -100,
            width: 1920,
            height: 1080,
        };
        assert_eq!(a.right(), 0);
        assert_eq!(a.bottom(), 980);
        assert_eq!(
            a.intersect(ScreenRect {
                x: -100,
                y: 0,
                width: 200,
                height: 200,
            }),
            Some(ScreenRect {
                x: -100,
                y: 0,
                width: 100,
                height: 200,
            })
        );
        assert_eq!(
            a.intersect(ScreenRect {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            }),
            None
        );
        // Touching edges are not an overlap.
        assert_eq!(
            a.intersect(ScreenRect {
                x: 0,
                y: -100,
                width: 10,
                height: 10,
            }),
            None
        );
    }
    #[test]
    fn window_state_defaults_to_unset_positions() {
        let state = WindowState::default();
        assert!(state.validate().is_ok());
        assert!(state.toolbar.is_none() && state.settings.is_none());
        let state = WindowState {
            version: 2,
            ..WindowState::default()
        };
        assert!(state.validate().is_err());
        let state = WindowState {
            toolbar: Some(ScreenPoint { x: -1920, y: 40 }),
            ..WindowState::default()
        };
        assert_eq!(state.toolbar, Some(ScreenPoint { x: -1920, y: 40 }));
    }
}

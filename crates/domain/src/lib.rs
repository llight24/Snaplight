use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    time::Instant,
};

pub mod long_capture;

pub const PROTOCOL_VERSION: u32 = 1;
/// Default global shortcut that opens the capture overlay.
pub const DEFAULT_CAPTURE_HOTKEY: &str = "Ctrl+Shift+A";
/// Default global shortcut that restores interaction for click-through pins.
pub const DEFAULT_RESTORE_HOTKEY: &str = "Ctrl+Shift+P";
/// Default shortcut held down to interact with click-through pins. Holding it
/// only releases the click-through state while the keys are down, so the pin
/// goes back to letting clicks through as soon as they are released.
pub const DEFAULT_THROUGH_HOTKEY: &str = "Ctrl";
/// Default outline and fill colour of new annotations, so a fresh configuration
/// matches the shape style the editor has always drawn.
pub const DEFAULT_STROKE_COLOR: &str = "#F23340";
/// Default fill opacity, in percent.
pub const DEFAULT_FILL_OPACITY: u8 = 100;
/// Range and default of the text size of annotations, in image pixels.
pub const MIN_FONT_SIZE: u16 = 12;
pub const MAX_FONT_SIZE: u16 = 200;
pub const DEFAULT_FONT_SIZE: u16 = 32;
/// Default drop shadow of a pin: a pin lifts off the desktop like the menu
/// cards do, and the shadow is what says so. Its metrics are logical pixels at
/// 96 DPI, so the pin scales them for the monitor it opens on.
pub const DEFAULT_PIN_SHADOW: bool = true;
pub const DEFAULT_PIN_SHADOW_BLUR: u32 = 8;
pub const DEFAULT_PIN_SHADOW_OFFSET: u32 = 2;
/// Default shadow opacity, in percent.
pub const DEFAULT_PIN_SHADOW_OPACITY: u8 = 20;
pub const MAX_PIN_SHADOW_BLUR: u32 = 48;
pub const MAX_PIN_SHADOW_OFFSET: u32 = 24;

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
        apply_modifier(&mut hotkey, modifier)?;
    }
    if !(hotkey.ctrl || hotkey.shift || hotkey.alt || hotkey.win) {
        return Err("hotkey needs at least one modifier");
    }
    hotkey.key = parse_hotkey_key(key_text)?;
    Ok(hotkey)
}
/// Shortcut that is *held* rather than pressed, used to interact with a
/// click-through pin for as long as it is down.
///
/// Unlike `Hotkey` this is not registered with the shell, so it may be a lone
/// modifier (`Ctrl`), a modifier combination (`Ctrl+Shift`), a lone key (`A`),
/// or a combination (`Ctrl+Shift+A`). `None` means the shortcut is disabled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeldShortcut {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
    pub key: Option<HotkeyKey>,
}
impl HeldShortcut {
    /// Canonical spelling (`Ctrl+Shift`), matching `Hotkey::canonical` so both
    /// the settings field and the menu hint show the same text.
    pub fn canonical(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for (enabled, name) in [
            (self.ctrl, "Ctrl"),
            (self.shift, "Shift"),
            (self.alt, "Alt"),
            (self.win, "Win"),
        ] {
            if enabled {
                parts.push(name.into());
            }
        }
        match self.key {
            None => {}
            Some(HotkeyKey::Letter(c) | HotkeyKey::Digit(c)) => parts.push(c.to_string()),
            Some(HotkeyKey::Function(n)) => parts.push(format!("F{n}")),
        }
        parts.join("+")
    }
}
/// Parses a held shortcut. `None` disables the shortcut; an empty string is
/// rejected so a blank settings field cannot silently disable it.
pub fn parse_held_shortcut(text: &str) -> Result<Option<HeldShortcut>, &'static str> {
    if text.trim().is_empty() {
        return Err("empty held shortcut");
    }
    let parts: Vec<&str> = text.split('+').map(str::trim).collect();
    let mut shortcut = HeldShortcut {
        ctrl: false,
        shift: false,
        alt: false,
        win: false,
        key: None,
    };
    let (key_text, modifiers) = parts.split_last().ok_or("empty held shortcut")?;
    for modifier in modifiers {
        apply_modifier(&mut shortcut, modifier)?;
    }
    // The last part is a modifier too when the whole shortcut is just
    // modifiers, which is what makes `Ctrl`, `Ctrl+Shift` and `Alt` valid.
    match shortcut_modifier(key_text) {
        Some(None) => return Err("empty held shortcut modifier"),
        Some(Some(modifier)) => apply_named_modifier(&mut shortcut, modifier),
        // A lone non-modifier key needs no modifier at all: it is only watched
        // while a pin is click-through, never registered with the shell.
        None => shortcut.key = Some(parse_hotkey_key(key_text)?),
    }
    if !(shortcut.ctrl || shortcut.shift || shortcut.alt || shortcut.win || shortcut.key.is_some())
    {
        return Err("held shortcut needs a key");
    }
    Ok(Some(shortcut))
}
/// Modifiers of a parsed shortcut, in canonical order.
fn apply_modifier<T>(shortcut: &mut T, modifier: &str) -> Result<(), &'static str>
where
    T: ModifierFlags,
{
    match shortcut_modifier(modifier) {
        Some(Some(modifier)) => {
            shortcut.set_modifier(modifier);
            Ok(())
        }
        Some(None) => Err("empty hotkey modifier"),
        None => Err("unknown hotkey modifier"),
    }
}
/// Resolves a text part that names a modifier. `Some(None)` is an empty part, a
/// shape the callers reject with their own message.
fn shortcut_modifier(text: &str) -> Option<Option<Modifier>> {
    if text.is_empty() {
        return Some(None);
    }
    match text.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some(Some(Modifier::Ctrl)),
        "shift" => Some(Some(Modifier::Shift)),
        "alt" => Some(Some(Modifier::Alt)),
        "win" | "super" | "meta" => Some(Some(Modifier::Win)),
        _ => None,
    }
}
fn apply_named_modifier<T>(shortcut: &mut T, modifier: Modifier)
where
    T: ModifierFlags,
{
    shortcut.set_modifier(modifier);
}
/// Modifier flag shared by `Hotkey` and `HeldShortcut`, so parsing them cannot
/// drift apart.
enum Modifier {
    Ctrl,
    Shift,
    Alt,
    Win,
}
trait ModifierFlags {
    fn set_modifier(&mut self, modifier: Modifier);
}
impl ModifierFlags for Hotkey {
    fn set_modifier(&mut self, modifier: Modifier) {
        match modifier {
            Modifier::Ctrl => self.ctrl = true,
            Modifier::Shift => self.shift = true,
            Modifier::Alt => self.alt = true,
            Modifier::Win => self.win = true,
        }
    }
}
impl ModifierFlags for HeldShortcut {
    fn set_modifier(&mut self, modifier: Modifier) {
        match modifier {
            Modifier::Ctrl => self.ctrl = true,
            Modifier::Shift => self.shift = true,
            Modifier::Alt => self.alt = true,
            Modifier::Win => self.win = true,
        }
    }
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
impl Tool {
    /// Every tool in configuration order. The index is the stable tool id the
    /// toolbar and the editor exchange, so the display order can change without
    /// changing what a message means.
    pub const ALL: [Tool; 7] = [
        Tool::Select,
        Tool::Rectangle,
        Tool::Ellipse,
        Tool::Arrow,
        Tool::Pen,
        Tool::Mosaic,
        Tool::Text,
    ];
    pub fn id(self) -> i32 {
        Tool::ALL
            .iter()
            .position(|tool| *tool == self)
            .expect("every tool is listed") as i32
    }
    pub fn from_id(id: i32) -> Option<Tool> {
        usize::try_from(id)
            .ok()
            .and_then(|id| Tool::ALL.get(id))
            .copied()
    }
    /// True when the tool draws lines: a shape outline, an arrow or a stroke.
    /// Mosaic has no paint of its own and the select tool draws nothing.
    pub fn can_stroke(self) -> bool {
        matches!(
            self,
            Tool::Rectangle | Tool::Ellipse | Tool::Arrow | Tool::Pen | Tool::Text
        )
    }
    /// True when the tool has an interior to fill: the two shapes and text, which
    /// fills its glyphs. An arrow or a pen stroke has none.
    pub fn can_fill(self) -> bool {
        matches!(self, Tool::Rectangle | Tool::Ellipse | Tool::Text)
    }
}
/// Paint of one annotation: an outline, a fill, or both. Colours are RGBA in
/// sRGB with premultiply-free alpha, so the fill carries its own opacity.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Style {
    pub stroke: [f32; 4],
    pub fill: [f32; 4],
    /// Stroke width in image pixels.
    pub width: f32,
    pub stroke_on: bool,
    pub fill_on: bool,
    /// Text size in image pixels; only `Shape::Text` reads it.
    pub font_size: f32,
}
impl Default for Style {
    fn default() -> Self {
        Self {
            stroke: [0.95, 0.2, 0.25, 1.0],
            fill: [0.95, 0.2, 0.25, 1.0],
            width: 3.0,
            stroke_on: true,
            fill_on: false,
            font_size: 32.0,
        }
    }
}
/// Parses `#RRGGBB` (or `#RRGGBBAA`) into sRGB components. Whitespace around the
/// text is ignored, so a value typed into a settings field is accepted as is.
pub fn parse_hex_color(text: &str) -> Option<[f32; 4]> {
    let text = text.trim();
    let digits = text.strip_prefix('#')?;
    if !matches!(digits.len(), 6 | 8) || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |at: usize| u8::from_str_radix(&digits[at..at + 2], 16).ok();
    let alpha = if digits.len() == 8 {
        byte(6)? as f32 / 255.0
    } else {
        1.0
    };
    Some([
        byte(0)? as f32 / 255.0,
        byte(2)? as f32 / 255.0,
        byte(4)? as f32 / 255.0,
        alpha,
    ])
}
/// Formats sRGB components as `#RRGGBB`; alpha is reported separately by the
/// callers that keep it, so it is not part of the text.
pub fn format_hex_color(rgba: [f32; 4]) -> String {
    let byte = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "#{:02X}{:02X}{:02X}",
        byte(rgba[0]),
        byte(rgba[1]),
        byte(rgba[2])
    )
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
    /// Transient readout drawn over the canvas while a value is being scrubbed
    /// with the number keys, so the change is visible where the user is looking.
    pub hud: Option<String>,
    /// True while the document is a fixed image re-opened for annotation, which
    /// is what a pinned image becomes. There is no capture region to choose, so
    /// renderers draw neither the dimming mask nor the selection chrome: the
    /// window is the image, and the canvas keeps region edits and detection off.
    pub fixed_image: bool,
}
#[derive(Clone, Copy, Debug)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
    pub dpi: f32,
    /// Screen position of the image origin, so chrome can report screen pixels.
    pub origin: ScreenPoint,
    /// Zoom applied to the document inside the viewport. `1.0` shows image pixels
    /// 1:1; smaller values shrink a document taller than the window to fit it.
    pub scale: f32,
    /// Where the document's top-left corner lands inside the viewport, in view
    /// pixels, after `scale`.
    pub offset_x: f32,
    pub offset_y: f32,
}
impl Viewport {
    /// A 1:1 view: one image pixel per view pixel, document origin at the corner.
    pub fn plain(width: u32, height: u32, dpi: f32, origin: ScreenPoint) -> Self {
        Self {
            width,
            height,
            dpi,
            origin,
            scale: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
        }
    }
    /// Maps a point in image pixels to view pixels.
    pub fn to_view(self, x: f32, y: f32) -> (f32, f32) {
        (
            x * self.scale + self.offset_x,
            y * self.scale + self.offset_y,
        )
    }
    /// Maps a point in view pixels back to image pixels.
    pub fn to_image(self, x: f32, y: f32) -> (f32, f32) {
        let scale = if self.scale > 0.0 { self.scale } else { 1.0 };
        ((x - self.offset_x) / scale, (y - self.offset_y) / scale)
    }
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
    /// Outline colour of new annotations, as `#RRGGBB`.
    pub stroke_color: String,
    /// Fill colour of new annotations, as `#RRGGBB`. Its opacity comes from
    /// `fill_opacity` instead of the text, so one slider covers every tool.
    pub fill_color: String,
    /// Fill opacity of new annotations, in percent.
    pub fill_opacity: u8,
    /// Text size of new text annotations, in image pixels.
    pub font_size: u16,
    /// Tools that paint an outline, and tools that paint a fill. Text paints a
    /// fill (its glyphs) instead of an outline, so the two lists overlap only
    /// for tools the user asked to paint both.
    pub stroke_tools: Vec<Tool>,
    pub fill_tools: Vec<Tool>,
    pub cpu_cache_bytes: usize,
    pub gpu_pins: bool,
    /// Directory for saved captures. `None` uses `data_dir()/captures`.
    pub capture_dir: Option<String>,
    /// Global shortcut that opens the capture overlay.
    pub capture_hotkey: String,
    /// Global shortcut that restores interaction for click-through pins.
    pub restore_hotkey: String,
    /// Shortcut held down to interact with click-through pins. Unlike the two
    /// shortcuts above this one is not registered with the shell, so a lone
    /// modifier such as `Ctrl` is allowed.
    pub through_hotkey: String,
    /// Whether closing a pin asks for confirmation first. The pin manager reads
    /// this at the moment of the double-click instead of at startup, so the
    /// "don't ask again" box in the dialog and the settings switch take effect
    /// without restarting the pin process.
    pub pin_close_confirm: bool,
    /// Whether a new pin draws a drop shadow. The pin context menu toggles the
    /// shadow of one pin without touching this default.
    pub pin_shadow: bool,
    /// Blur radius of the pin shadow, in logical pixels at 96 DPI: the distance
    /// over which it fades out past the image edge.
    pub pin_shadow_blur: u32,
    /// How far the pin shadow is pushed down, in logical pixels. Pins are lit
    /// from above, so it only ever moves down; the window reserves the margin
    /// on every side regardless.
    pub pin_shadow_offset: u32,
    /// Opacity of the pin shadow at the image edge, in percent.
    pub pin_shadow_opacity: u8,
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
            stroke_color: DEFAULT_STROKE_COLOR.into(),
            fill_color: DEFAULT_STROKE_COLOR.into(),
            fill_opacity: DEFAULT_FILL_OPACITY,
            font_size: DEFAULT_FONT_SIZE,
            stroke_tools: vec![Tool::Rectangle, Tool::Ellipse, Tool::Arrow, Tool::Pen],
            fill_tools: vec![Tool::Text],
            cpu_cache_bytes: 8 * 1024 * 1024,
            gpu_pins: false,
            capture_dir: None,
            capture_hotkey: DEFAULT_CAPTURE_HOTKEY.into(),
            restore_hotkey: DEFAULT_RESTORE_HOTKEY.into(),
            through_hotkey: DEFAULT_THROUGH_HOTKEY.into(),
            pin_close_confirm: true,
            pin_shadow: DEFAULT_PIN_SHADOW,
            pin_shadow_blur: DEFAULT_PIN_SHADOW_BLUR,
            pin_shadow_offset: DEFAULT_PIN_SHADOW_OFFSET,
            pin_shadow_opacity: DEFAULT_PIN_SHADOW_OPACITY,
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
        if self.fill_opacity > 100 || !(MIN_FONT_SIZE..=MAX_FONT_SIZE).contains(&self.font_size) {
            return Err("invalid fill opacity or font size");
        }
        // A shadow that is wider than its range would make a pin reserve an
        // unbounded margin, so the configured range is the contract the pin
        // process relies on when it sizes the window.
        if self.pin_shadow_blur > MAX_PIN_SHADOW_BLUR
            || self.pin_shadow_offset > MAX_PIN_SHADOW_OFFSET
            || self.pin_shadow_opacity > 100
        {
            return Err("invalid pin shadow settings");
        }
        for color in [&self.stroke_color, &self.fill_color] {
            parse_hex_color(color).ok_or("stroke and fill colours must be #RRGGBB")?;
        }
        // The lists say which tools paint what, so a tool that cannot paint a way
        // at all has no business being listed for it.
        if self.stroke_tools.iter().any(|tool| !tool.can_stroke())
            || self.fill_tools.iter().any(|tool| !tool.can_fill())
        {
            return Err("paint defaults list tools that cannot paint that way");
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
        parse_held_shortcut(&self.through_hotkey).map_err(|_| "invalid through hotkey")?;
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
    /// Re-reads the configuration and reapplies the settings that are read live:
    /// the restore and held shortcuts, and the pin shadow and its metrics. The
    /// core sends this after the settings window saves, so a change reaches
    /// pins that are already on screen without restarting the process.
    Reload,
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
    fn held_shortcuts_accept_modifiers_keys_and_combinations() {
        let held = |text: &str| parse_held_shortcut(text).unwrap().unwrap();
        // A lone modifier, which the registered shortcuts must reject.
        assert_eq!(held("Ctrl").canonical(), DEFAULT_THROUGH_HOTKEY);
        assert!(parse_hotkey("Ctrl").is_err());
        assert_eq!(held("ctrl+shift").canonical(), "Ctrl+Shift");
        assert_eq!(held("alt").canonical(), "Alt");
        assert_eq!(held("Win").canonical(), "Win");
        // A lone key needs no modifier: nothing is registered with the shell.
        let a = held("a");
        assert_eq!(a.key, Some(HotkeyKey::Letter('A')));
        assert_eq!(a.canonical(), "A");
        assert_eq!(held("F8").key, Some(HotkeyKey::Function(8)));
        let combo = held("Ctrl+Alt+F12");
        assert_eq!((combo.ctrl, combo.alt), (true, true));
        assert_eq!(combo.key, Some(HotkeyKey::Function(12)));
        assert_eq!(combo.canonical(), "Ctrl+Alt+F12");
        assert_eq!(held("  Ctrl + 5 ").key, Some(HotkeyKey::Digit('5')));
    }
    #[test]
    fn invalid_held_shortcuts_are_rejected() {
        for text in ["", "   ", "Ctrl+", "Ctrl+Shift+F25", "Foo", "Ctrl+Esc"] {
            assert!(
                parse_held_shortcut(text).is_err(),
                "{text} should be rejected"
            );
        }
        let config = Config {
            through_hotkey: "Ctrl+".into(),
            ..Config::default()
        };
        assert_eq!(config.validate(), Err("invalid through hotkey"));
        // A configuration written before the option existed keeps the default.
        let config: Config = serde_json::from_str("{\"version\":1}").unwrap();
        assert_eq!(config.through_hotkey, DEFAULT_THROUGH_HOTKEY);
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
    fn closing_a_pin_confirms_by_default_and_survives_an_old_config() {
        assert!(Config::default().pin_close_confirm);
        // Closing a pin throws away what the user pinned, so a configuration
        // written before the option existed has to keep asking.
        let config: Config = serde_json::from_str("{\"version\":1}").unwrap();
        assert!(config.pin_close_confirm);
        let config: Config = serde_json::from_str("{\"pin_close_confirm\":false}").unwrap();
        assert!(!config.pin_close_confirm);
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
    fn tool_ids_round_trip_and_stay_stable() {
        for (index, tool) in Tool::ALL.iter().enumerate() {
            assert_eq!(tool.id(), index as i32);
            assert_eq!(Tool::from_id(index as i32), Some(*tool));
        }
        assert_eq!(Tool::from_id(-1), None);
        assert_eq!(Tool::from_id(7), None);
        // The toolbar and the editor agree on the id of every tool, so a
        // reordered palette cannot make a message mean another tool.
        assert_eq!(Tool::Rectangle.id(), 1);
        assert_eq!(Tool::Text.id(), 6);
    }
    #[test]
    fn paint_defaults_separate_text_from_shapes() {
        // Defaults that documents written before fills existed deserialize to:
        // an outline of the default red, which is what those documents show.
        let old = Style::default();
        assert!(old.stroke_on && !old.fill_on);
        assert_eq!(old.width, 3.0);
        assert_eq!(old.font_size, DEFAULT_FONT_SIZE as f32);
        let stored: Style = serde_json::from_str("{\"rgba\":[1.0,0.0,0.0,1.0]}").unwrap();
        assert_eq!(stored, old);
        // New sessions start from the configured lists: shapes are outlined and
        // text is filled, which is what its glyphs need to be readable.
        let config = Config::default();
        assert!(config.stroke_tools.contains(&Tool::Rectangle));
        assert!(!config.fill_tools.contains(&Tool::Rectangle));
        assert_eq!(config.fill_tools, vec![Tool::Text]);
        assert!(!config.stroke_tools.contains(&Tool::Text));
    }
    #[test]
    fn hex_colours_parse_and_round_trip() {
        let red = parse_hex_color("#F23340").unwrap();
        assert!((red[0] - 0.95).abs() < 0.01);
        assert!((red[1] - 0.2).abs() < 0.01);
        assert!((red[2] - 0.25).abs() < 0.01);
        assert_eq!(red[3], 1.0);
        assert_eq!(format_hex_color(red), "#F23340");
        // Whitespace from a settings field is tolerated; alpha is parsed too.
        assert_eq!(parse_hex_color(" #F23340 ").unwrap(), red);
        assert_eq!(parse_hex_color("#F2334000").unwrap()[3], 0.0);
        for text in ["", "F23340", "#F2334", "#F2334G", "#", "0xFF0000"] {
            assert!(parse_hex_color(text).is_none(), "{text} should be rejected");
        }
        // Out-of-range components clamp instead of wrapping to another colour.
        assert_eq!(format_hex_color([2.0, -1.0, 0.5, 1.0]), "#FF0080");
    }
    #[test]
    fn config_rejects_unusable_paint_defaults() {
        let mut config = Config::default();
        assert!(config.validate().is_ok());
        config.fill_opacity = 101;
        assert_eq!(config.validate(), Err("invalid fill opacity or font size"));
        config.fill_opacity = DEFAULT_FILL_OPACITY;
        config.font_size = MIN_FONT_SIZE - 1;
        assert_eq!(config.validate(), Err("invalid fill opacity or font size"));
        config.font_size = DEFAULT_FONT_SIZE;
        config.fill_color = "red".into();
        assert_eq!(
            config.validate(),
            Err("stroke and fill colours must be #RRGGBB")
        );
        // A configuration written before paint defaults existed keeps the
        // defaults instead of failing to load.
        let config: Config = serde_json::from_str("{\"version\":1}").unwrap();
        assert_eq!(config.stroke_color, DEFAULT_STROKE_COLOR);
        assert_eq!(config.fill_tools, vec![Tool::Text]);
        assert_eq!(config.stroke_tools.len(), 4);
        // An arrow has no interior to fill and a mosaic has no paint at all, so
        // listing them for that paint is rejected instead of silently ignored.
        let arrow_fill = Config {
            fill_tools: vec![Tool::Arrow],
            ..Config::default()
        };
        assert_eq!(
            arrow_fill.validate(),
            Err("paint defaults list tools that cannot paint that way")
        );
        let mosaic_stroke = Config {
            stroke_tools: vec![Tool::Mosaic],
            ..Config::default()
        };
        assert_eq!(
            mosaic_stroke.validate(),
            Err("paint defaults list tools that cannot paint that way")
        );
    }
    #[test]
    fn pin_shadow_defaults_on_and_rejects_unusable_metrics() {
        // Pins lift off the desktop by default; a configuration written before
        // the shadow existed gets the same defaults.
        assert!(Config::default().pin_shadow);
        let config: Config = serde_json::from_str("{\"version\":1}").unwrap();
        assert!(config.pin_shadow);
        // The metrics are a product decision, so they are pinned by value here
        // rather than compared against the constants they come from.
        assert_eq!(config.pin_shadow_blur, 8);
        assert_eq!(config.pin_shadow_offset, 2);
        assert_eq!(config.pin_shadow_opacity, 20);
        assert_eq!(DEFAULT_PIN_SHADOW_BLUR, 8);
        assert_eq!(DEFAULT_PIN_SHADOW_OFFSET, 2);
        assert_eq!(DEFAULT_PIN_SHADOW_OPACITY, 20);
        assert!(config.validate().is_ok());
        // Every metric has a range the pin process sizes its window by, so a
        // hand-edited value outside it must not reach the pin.
        for broken in [
            Config {
                pin_shadow_blur: MAX_PIN_SHADOW_BLUR + 1,
                ..Config::default()
            },
            Config {
                pin_shadow_offset: MAX_PIN_SHADOW_OFFSET + 1,
                ..Config::default()
            },
            Config {
                pin_shadow_opacity: 101,
                ..Config::default()
            },
        ] {
            assert_eq!(broken.validate(), Err("invalid pin shadow settings"));
        }
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

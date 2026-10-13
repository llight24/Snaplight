//! Owner-drawn popup menu used by the tray context menu.
//!
//! `TrackPopupMenu` can only draw the current system theme, so the tray menu
//! cannot follow the reference design (white rounded card, soft shadow,
//! right-aligned shortcut hints, red destructive entry). This module renders the
//! card with GDI into a 32-bit top-down DIB, derives the per-pixel coverage of
//! the rounded rectangle and its shadow, and presents it through
//! `UpdateLayeredWindow`. While the menu is open the thread blocks in
//! `GetMessageW`, so the resident core still runs no polling loop, no GPU device
//! and no frame timer.
//!
//! Layout is computed in physical pixels from a logical (96 DPI) metric set that
//! matches the reference menu; [`layout`] is a pure function so the geometry can
//! be asserted without creating a window, and [`render`] exposes the pixels for
//! automated checks.
//!
//! The card geometry, palette and font helpers are `pub(crate)` because
//! [`crate::confirm`] draws the confirmation prompt with the same card, palette
//! and modal message pump; the two must not drift apart.

use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
            Input::KeyboardAndMouse::*,
            WindowsAndMessaging::*,
        },
    },
};

/// Window class of the popup; scripts use it to find the window.
pub const WINDOW_CLASS: &str = "Snaplight.PopupMenu";

/// The `windows` crate does not export `WM_MOUSELEAVE`.
const WM_MOUSELEAVE: u32 = 0x02A3;

// Logical (96 DPI) metrics, matched to the reference menu: white card with a
// 12px corner radius, 28px rows, 12px card padding, 20px text padding.
pub(crate) const CARD_RADIUS: f32 = 12.0;
const PAD_Y: f32 = 12.0;
const ROW_HEIGHT: f32 = 28.0;
const PAD_X: f32 = 20.0;
const COLUMN_GAP: f32 = 24.0;
const SEPARATOR_MARGIN: f32 = 8.0;
const SEPARATOR_THICKNESS: f32 = 1.0;
const MIN_WIDTH: f32 = 180.0;
const HOVER_INSET: f32 = 4.0;
pub(crate) const HOVER_RADIUS: f32 = 6.0;
/// Text size in logical pixels. The reference measures closer to 13px, but that
/// reads too large next to the card, so the shipped size is a step smaller.
pub(crate) const FONT_EM: f32 = 12.0;
pub(crate) const FONT_FACE: PCWSTR = w!("Segoe UI");
/// Room reserved around the card for the drop shadow.
pub(crate) const SHADOW_MARGIN: f32 = 14.0;
const SHADOW_OFFSET_Y: f32 = 2.0;
const SHADOW_BLUR: f32 = 10.0;
const SHADOW_ALPHA: f32 = 0.13;

// Colours sampled from the reference menu.
pub(crate) const CARD: (u8, u8, u8) = (0xFF, 0xFF, 0xFF);
pub(crate) const LABEL: (u8, u8, u8) = (0x1F, 0x1F, 0x1F);
/// Muted text: shortcut hints here, the secondary line of the confirmation card.
pub(crate) const MUTED: (u8, u8, u8) = (0x47, 0x47, 0x47);
const SEPARATOR: (u8, u8, u8) = (0xD3, 0xE3, 0xFD);
pub(crate) const HOVER: (u8, u8, u8) = (0xF2, 0xF2, 0xF2);
/// Destructive entries (closing every pin) are drawn red.
pub(crate) const DANGER: (u8, u8, u8) = (0xD1, 0x34, 0x38);

pub(crate) const fn rgb(color: (u8, u8, u8)) -> COLORREF {
    COLORREF(color.0 as u32 | (color.1 as u32) << 8 | (color.2 as u32) << 16)
}

/// One selectable menu row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub label: String,
    /// Shortcut hint, drawn right-aligned in the muted colour.
    pub shortcut: Option<String>,
    /// Draws the label red for commands that destroy something.
    pub danger: bool,
}
impl Item {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            shortcut: None,
            danger: false,
        }
    }
    pub fn with_shortcut(mut self, shortcut: impl Into<String>) -> Self {
        self.shortcut = Some(shortcut.into());
        self
    }
    /// Appends a parenthesised explanation to the label. A row only carries one
    /// right-aligned shortcut, so extra keys belong in the label text.
    pub fn with_hint(mut self, hint: impl AsRef<str>) -> Self {
        self.label = format!("{}（{}）", self.label, hint.as_ref());
        self
    }
    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }
}
/// Menu content in display order. Only [`Entry::Item`] can be chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Item(Item),
    Separator,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}
impl Rect {
    pub fn right(&self) -> i32 {
        self.x + self.width
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.height
    }
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}
/// Placement of one selectable row, in window coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowLayout {
    /// Index of this row in the entry list.
    pub entry: usize,
    /// Full-width band used for hit testing.
    pub band: Rect,
    /// Highlight rectangle, inset from the card edge.
    pub highlight: Rect,
    pub text_left: i32,
    pub text_right: i32,
}
/// Window and row geometry for one menu at one DPI scale.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub scale: f32,
    /// Size of the layered window bitmap, card plus shadow margin.
    pub width: i32,
    pub height: i32,
    /// The card inside the window bitmap.
    pub card: Rect,
    pub radius: i32,
    pub rows: Vec<RowLayout>,
    /// Entry index and line rectangle of every separator.
    pub separators: Vec<(usize, Rect)>,
}
/// Lays out `entries` for `scale` (1.0 = 96 DPI). `text` holds the measured
/// label and shortcut widths in physical pixels, index-aligned with `entries`.
pub fn layout(entries: &[Entry], scale: f32, text: &[(i32, i32)]) -> Layout {
    let scale = scale.clamp(0.5, 4.0);
    let px = |value: f32| (value * scale).round() as i32;
    let radius = px(CARD_RADIUS).max(2);
    let pad_x = px(PAD_X);
    let pad_y = px(PAD_Y);
    let row_height = px(ROW_HEIGHT);
    let gap = px(COLUMN_GAP);
    let separator_margin = px(SEPARATOR_MARGIN);
    let separator_thickness = px(SEPARATOR_THICKNESS).max(1);
    let inset = px(HOVER_INSET);
    let margin = px(SHADOW_MARGIN);

    let mut width = px(MIN_WIDTH);
    let mut content_height = 0;
    for (index, entry) in entries.iter().enumerate() {
        match entry {
            Entry::Item(_) => {
                let (label, shortcut) = text.get(index).copied().unwrap_or_default();
                let row = label + if shortcut > 0 { gap + shortcut } else { 0 };
                width = width.max(row + 2 * pad_x);
                content_height += row_height;
            }
            Entry::Separator => content_height += 2 * separator_margin + separator_thickness,
        }
    }
    let card = Rect {
        x: margin,
        y: margin,
        width,
        height: content_height + 2 * pad_y,
    };
    let mut rows = Vec::new();
    let mut separators = Vec::new();
    let mut y = card.y + pad_y;
    for (index, entry) in entries.iter().enumerate() {
        match entry {
            Entry::Item(_) => {
                rows.push(RowLayout {
                    entry: index,
                    band: Rect {
                        x: card.x,
                        y,
                        width: card.width,
                        height: row_height,
                    },
                    highlight: Rect {
                        x: card.x + inset,
                        y,
                        width: (card.width - 2 * inset).max(1),
                        height: row_height,
                    },
                    text_left: card.x + pad_x,
                    text_right: card.x + card.width - pad_x,
                });
                y += row_height;
            }
            Entry::Separator => {
                separators.push((
                    index,
                    Rect {
                        x: card.x,
                        y: y + separator_margin,
                        width: card.width,
                        height: separator_thickness,
                    },
                ));
                y += 2 * separator_margin + separator_thickness;
            }
        }
    }
    Layout {
        scale,
        width: card.width + 2 * margin,
        height: card.height + 2 * margin,
        card,
        radius,
        rows,
        separators,
    }
}

/// A rendered menu: premultiplied BGRA pixels, top-down, ready for
/// `UpdateLayeredWindow`.
#[derive(Clone, Debug)]
pub struct MenuBitmap {
    pub width: i32,
    pub height: i32,
    pub layout: Layout,
    pub premultiplied_bgra: Vec<u8>,
}
impl MenuBitmap {
    /// Premultiplied BGRA at `(x, y)`, or `None` outside the bitmap.
    pub fn pixel(&self, x: i32, y: i32) -> Option<[u8; 4]> {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return None;
        }
        let offset = ((y * self.width + x) * 4) as usize;
        let p = &self.premultiplied_bgra[offset..offset + 4];
        Some([p[0], p[1], p[2], p[3]])
    }
    /// Straight (non-premultiplied) RGBA, which is what PNG stores. The pixel
    /// buffer is BGRA, so the red and blue channels swap here; a PNG written
    /// without that swap shows red entries in blue.
    pub fn straight_rgba(&self) -> Vec<u8> {
        let mut rgba = self.premultiplied_bgra.clone();
        for pixel in rgba.as_chunks_mut::<4>().0 {
            let alpha = pixel[3] as u32;
            if alpha == 0 {
                pixel.copy_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            let straight =
                |channel: u8| ((channel as u32 * 255 + alpha / 2) / alpha).min(255) as u8;
            *pixel = [
                straight(pixel[2]),
                straight(pixel[1]),
                straight(pixel[0]),
                alpha as u8,
            ];
        }
        rgba
    }
}

/// Renders `entries` at `scale`, highlighting `hover` (an entry index).
pub fn render(entries: &[Entry], scale: f32, hover: Option<usize>) -> Result<MenuBitmap> {
    let scale = scale.clamp(0.5, 4.0);
    unsafe {
        let desktop = GetDC(None);
        if desktop.is_invalid() {
            return Err(Error::from_win32());
        }
        let memory = CreateCompatibleDC(desktop);
        if memory.is_invalid() {
            ReleaseDC(None, desktop);
            return Err(Error::from_win32());
        }
        let result = render_into(memory, entries, scale, hover);
        let _ = DeleteDC(memory);
        ReleaseDC(None, desktop);
        result
    }
}

/// Creates the card face at `scale`. `weight` is a `FW_*` value; the menu uses
/// `FW_NORMAL` and the confirmation card a semibold title from the same face.
pub(crate) unsafe fn create_font(scale: f32, em: f32, weight: i32) -> Result<HFONT> {
    let font = CreateFontW(
        -(em * scale).round() as i32,
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        OUT_TT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        CLEARTYPE_QUALITY.0 as u32,
        DEFAULT_PITCH.0 as u32 | FF_DONTCARE.0 as u32,
        FONT_FACE,
    );
    if font.is_invalid() {
        return Err(Error::from_win32());
    }
    Ok(font)
}

unsafe fn render_into(
    dc: HDC,
    entries: &[Entry],
    scale: f32,
    hover: Option<usize>,
) -> Result<MenuBitmap> {
    let font = create_font(scale, FONT_EM, FW_NORMAL.0 as i32)?;
    let old_font = SelectObject(dc, font);
    let result = (|| -> Result<MenuBitmap> {
        let measured: Vec<(i32, i32)> = entries
            .iter()
            .map(|entry| match entry {
                Entry::Item(item) => (
                    text_width(dc, &item.label),
                    item.shortcut
                        .as_deref()
                        .map(|shortcut| text_width(dc, shortcut))
                        .unwrap_or(0),
                ),
                Entry::Separator => (0, 0),
            })
            .collect();
        let layout = layout(entries, scale, &measured);

        let mut bits = std::ptr::null_mut();
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: layout.width,
                biHeight: -layout.height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
        if bits.is_null() {
            let _ = DeleteObject(bitmap);
            return Err(Error::from_win32());
        }
        let old_bitmap = SelectObject(dc, bitmap);

        draw(dc, entries, &layout, hover);

        // GDI has no per-pixel alpha, so the alpha channel is derived from the
        // card geometry: opaque white inside the rounded rectangle, black shadow
        // blended outside it. Colours stay correct because the card is drawn on a
        // white background and every opaque pixel keeps its drawn colour.
        let pixels = std::slice::from_raw_parts_mut(
            bits.cast::<u8>(),
            (layout.width * layout.height * 4) as usize,
        );
        apply_alpha(pixels, layout.width, layout.height, &layout);

        let bgra = pixels.to_vec();
        SelectObject(dc, old_bitmap);
        let _ = DeleteObject(bitmap);
        Ok(MenuBitmap {
            width: layout.width,
            height: layout.height,
            layout,
            premultiplied_bgra: bgra,
        })
    })();
    SelectObject(dc, old_font);
    let _ = DeleteObject(font);
    result
}

unsafe fn draw(dc: HDC, entries: &[Entry], layout: &Layout, hover: Option<usize>) {
    let width = layout.width;
    let height = layout.height;
    let background = CreateSolidBrush(rgb(CARD));
    let full = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };
    FillRect(dc, &full, background);
    let _ = DeleteObject(background);

    if let Some(row) = hover.and_then(|index| layout.rows.iter().find(|row| row.entry == index)) {
        let brush = CreateSolidBrush(rgb(HOVER));
        let old_brush = SelectObject(dc, brush);
        let old_pen = SelectObject(dc, GetStockObject(NULL_PEN));
        let corner = ((HOVER_RADIUS * layout.scale).round() as i32).max(2);
        let _ = RoundRect(
            dc,
            row.highlight.x,
            row.highlight.y,
            row.highlight.right(),
            row.highlight.bottom(),
            corner * 2,
            corner * 2,
        );
        SelectObject(dc, old_pen);
        SelectObject(dc, old_brush);
        let _ = DeleteObject(brush);
    }

    let separator = CreateSolidBrush(rgb(SEPARATOR));
    for (index, line) in &layout.separators {
        if matches!(entries.get(*index), Some(Entry::Separator)) {
            let rect = RECT {
                left: line.x,
                top: line.y,
                right: line.right(),
                bottom: line.bottom(),
            };
            FillRect(dc, &rect, separator);
        }
    }
    let _ = DeleteObject(separator);

    SetBkMode(dc, TRANSPARENT);
    for row in &layout.rows {
        let Some(Entry::Item(item)) = entries.get(row.entry) else {
            continue;
        };
        let mut rect = RECT {
            left: row.text_left,
            top: row.band.y,
            right: row.text_right,
            bottom: row.band.bottom(),
        };
        SetTextColor(dc, rgb(if item.danger { DANGER } else { LABEL }));
        let mut label: Vec<u16> = item.label.encode_utf16().collect();
        DrawTextW(
            dc,
            &mut label,
            &mut rect,
            DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX,
        );
        if let Some(shortcut) = &item.shortcut {
            SetTextColor(dc, rgb(MUTED));
            let mut text: Vec<u16> = shortcut.encode_utf16().collect();
            DrawTextW(
                dc,
                &mut text,
                &mut rect,
                DT_SINGLELINE | DT_VCENTER | DT_RIGHT | DT_NOPREFIX,
            );
        }
    }
}

/// Signed distance from `(x, y)` to a rounded rectangle centred on the origin;
/// positive outside, negative inside.
fn rounded_rect_distance(x: f32, y: f32, half_width: f32, half_height: f32, radius: f32) -> f32 {
    let qx = x.abs() - (half_width - radius);
    let qy = y.abs() - (half_height - radius);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - radius
}

fn apply_alpha(pixels: &mut [u8], width: i32, height: i32, layout: &Layout) {
    card_alpha(
        pixels,
        width,
        height,
        layout.card,
        layout.radius,
        layout.scale,
    );
}

/// Derives the alpha channel of a card bitmap from its rounded-rectangle
/// geometry: opaque inside `card`, the blurred drop shadow outside it.
///
/// GDI has no per-pixel alpha, so the coverage is computed here instead. Colours
/// stay correct because the card is drawn on a white background and every opaque
/// pixel keeps its drawn colour.
pub(crate) fn card_alpha(
    pixels: &mut [u8],
    width: i32,
    height: i32,
    card: Rect,
    radius: i32,
    scale: f32,
) {
    let half_width = card.width as f32 / 2.0;
    let half_height = card.height as f32 / 2.0;
    let center_x = card.x as f32 + half_width;
    let center_y = card.y as f32 + half_height;
    let radius = radius as f32;
    let offset = SHADOW_OFFSET_Y * scale;
    let blur = SHADOW_BLUR * scale;
    for y in 0..height {
        for x in 0..width {
            let px = x as f32 + 0.5 - center_x;
            let py = y as f32 + 0.5 - center_y;
            let distance = rounded_rect_distance(px, py, half_width, half_height, radius);
            let coverage = (0.5 - distance).clamp(0.0, 1.0);
            // The shadow is the same shape shifted down and blurred, so the top
            // edge stays almost clean while the bottom edge reads as elevation.
            let shifted = rounded_rect_distance(px, py - offset, half_width, half_height, radius);
            let t = (shifted / blur).clamp(0.0, 1.0);
            let shadow = SHADOW_ALPHA * (1.0 - t * t * (3.0 - 2.0 * t));
            let alpha = coverage + (1.0 - coverage) * shadow;
            let pixel = &mut pixels[((y * width + x) * 4) as usize..][..4];
            for channel in &mut pixel[..3] {
                *channel = (*channel as f32 * coverage).round() as u8;
            }
            pixel[3] = (alpha * 255.0).round() as u8;
        }
    }
}

pub(crate) unsafe fn text_width(dc: HDC, text: &str) -> i32 {
    if text.is_empty() {
        return 0;
    }
    let wide: Vec<u16> = text.encode_utf16().collect();
    let mut size = SIZE::default();
    if GetTextExtentPoint32W(dc, &wide, &mut size).as_bool() {
        size.cx
    } else {
        0
    }
}

struct MenuWindow {
    entries: Vec<Entry>,
    layout: Layout,
    scale: f32,
    hover: Option<usize>,
    pressed: Option<usize>,
    chosen: Option<usize>,
    closed: bool,
}
/// Presents premultiplied BGRA pixels as the whole client area of `hwnd`,
/// keeping the window's current screen position.
pub(crate) unsafe fn present_layered(
    hwnd: HWND,
    width: i32,
    height: i32,
    premultiplied_bgra: &[u8],
) -> Result<()> {
    let desktop = GetDC(None);
    if desktop.is_invalid() {
        return Err(Error::from_win32());
    }
    let memory = CreateCompatibleDC(desktop);
    if memory.is_invalid() {
        ReleaseDC(None, desktop);
        return Err(Error::from_win32());
    }
    let result = (|| -> Result<()> {
        let mut bits = std::ptr::null_mut();
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let target = CreateDIBSection(memory, &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
        if bits.is_null() {
            let _ = DeleteObject(target);
            return Err(Error::from_win32());
        }
        std::ptr::copy_nonoverlapping(
            premultiplied_bgra.as_ptr(),
            bits.cast::<u8>(),
            premultiplied_bgra.len(),
        );
        let old = SelectObject(memory, target);
        let mut window = RECT::default();
        GetWindowRect(hwnd, &mut window)?;
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let presented = UpdateLayeredWindow(
            hwnd,
            desktop,
            Some(&POINT {
                x: window.left,
                y: window.top,
            }),
            Some(&SIZE {
                cx: width,
                cy: height,
            }),
            memory,
            Some(&POINT::default()),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
        SelectObject(memory, old);
        let _ = DeleteObject(target);
        presented
    })();
    let _ = DeleteDC(memory);
    ReleaseDC(None, desktop);
    result
}

impl MenuWindow {
    unsafe fn update_layered(&self, hwnd: HWND) -> Result<()> {
        let bitmap = render(&self.entries, self.scale, self.hover)?;
        present_layered(
            hwnd,
            bitmap.width,
            bitmap.height,
            &bitmap.premultiplied_bgra,
        )
    }
    fn hit(&self, x: i32, y: i32) -> Option<usize> {
        self.layout
            .rows
            .iter()
            .find(|row| row.band.contains(x, y))
            .map(|row| row.entry)
    }
    unsafe fn set_hover(&mut self, hwnd: HWND, hover: Option<usize>) {
        if self.hover == hover {
            return;
        }
        self.hover = hover;
        if let Err(error) = self.update_layered(hwnd) {
            eprintln!("menu repaint failed: {error}");
        }
    }
    fn move_selection(&mut self, forward: bool) {
        let count = self.layout.rows.len();
        if count == 0 {
            return;
        }
        let next = match self
            .hover
            .and_then(|index| self.layout.rows.iter().position(|row| row.entry == index))
        {
            Some(position) if forward => (position + 1) % count,
            Some(position) => (position + count - 1) % count,
            None if forward => 0,
            None => count - 1,
        };
        self.hover = Some(self.layout.rows[next].entry);
    }
}

unsafe extern "system" fn menu_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let create = &*(l.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut MenuWindow;
    if !ptr.is_null() {
        let state = &mut *ptr;
        let point = || {
            let x = (l.0 & 0xFFFF) as u16 as i16 as i32;
            let y = ((l.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
            (x, y)
        };
        match msg {
            WM_PAINT => {
                let _ = ValidateRect(hwnd, None);
                return LRESULT(0);
            }
            WM_ERASEBKGND => return LRESULT(1),
            WM_MOUSEMOVE => {
                let (x, y) = point();
                let hover = state.hit(x, y);
                state.set_hover(hwnd, hover);
                let mut track = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut track);
                return LRESULT(0);
            }
            WM_MOUSELEAVE => {
                state.set_hover(hwnd, None);
                return LRESULT(0);
            }
            WM_LBUTTONDOWN => {
                let (x, y) = point();
                if state.layout.card.contains(x, y) {
                    state.pressed = state.hit(x, y);
                    state.set_hover(hwnd, state.pressed);
                } else {
                    state.closed = true;
                }
                return LRESULT(0);
            }
            WM_LBUTTONUP => {
                let (x, y) = point();
                let hover = state.hit(x, y);
                if state.pressed.is_some() && state.pressed == hover {
                    state.chosen = hover;
                    state.closed = true;
                }
                state.pressed = None;
                return LRESULT(0);
            }
            WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_MOUSEWHEEL => {
                state.closed = true;
                return LRESULT(0);
            }
            WM_ACTIVATE => {
                if (w.0 & 0xFFFF) as u32 == WA_INACTIVE {
                    state.closed = true;
                }
                return LRESULT(0);
            }
            WM_KILLFOCUS | WM_CLOSE | WM_CANCELMODE => {
                state.closed = true;
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, w, l)
}

/// Registers `name` with `proc` for this process. Registering a class twice is
/// not an error: both popups register their own class on first use.
pub(crate) unsafe fn register_class(
    name: PCWSTR,
    proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
) -> Result<()> {
    let instance = GetModuleHandleW(None)?;
    let class = WNDCLASSW {
        hInstance: instance.into(),
        lpszClassName: name,
        lpfnWndProc: Some(proc),
        ..Default::default()
    };
    if RegisterClassW(&class) == 0 {
        let error = Error::from_win32();
        if error.code() != HRESULT::from_win32(ERROR_CLASS_ALREADY_EXISTS.0) {
            return Err(error);
        }
    }
    Ok(())
}

/// DPI scale of the monitor that owns `(x, y)`; 1.0 on failure.
pub(crate) unsafe fn scale_at(x: i32, y: i32) -> f32 {
    let monitor = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
    let mut dpi_x = 96;
    let mut dpi_y = 96;
    if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_err() || dpi_x == 0 {
        return 1.0;
    }
    dpi_x as f32 / 96.0
}

/// Top-left corner of the card for a pointer at `anchor` (screen physical
/// pixels). The card opens at the pointer's upper right: its bottom-left corner
/// sits on the pointer and the menu grows up and to the right. It drops below
/// the pointer only when it cannot fit above, and is clamped into the work area
/// of the monitor it opens on.
pub fn place(width: i32, height: i32, anchor: (i32, i32), work: RECT) -> (i32, i32) {
    let (anchor_x, anchor_y) = anchor;
    let mut x = anchor_x;
    let mut y = anchor_y - height;
    if y < work.top {
        y = anchor_y;
    }
    if x + width > work.right {
        x = work.right - width;
    }
    if x < work.left {
        x = work.left;
    }
    if y + height > work.bottom {
        y = work.bottom - height;
    }
    if y < work.top {
        y = work.top;
    }
    (x, y)
}

/// Shows the menu at `anchor` (screen physical pixels, the cursor by default)
/// and blocks until it is dismissed. Returns the chosen entry index.
pub fn show(entries: &[Entry], anchor: Option<(i32, i32)>) -> Option<usize> {
    if entries.is_empty() {
        return None;
    }
    unsafe {
        let instance = GetModuleHandleW(None).ok()?;
        register_class(w!("Snaplight.PopupMenu"), menu_proc).ok()?;
        let (anchor_x, anchor_y) = anchor.unwrap_or_else(|| {
            let mut point = POINT::default();
            let _ = GetCursorPos(&mut point);
            (point.x, point.y)
        });
        let scale = scale_at(anchor_x, anchor_y);
        let first = render(entries, scale, None).ok()?;
        let margin = first.layout.card.x;
        let work = work_area(anchor_x, anchor_y);
        let (card_x, card_y) = place(
            first.layout.card.width,
            first.layout.card.height,
            (anchor_x, anchor_y),
            work,
        );

        let mut state = Box::new(MenuWindow {
            entries: entries.to_vec(),
            layout: first.layout.clone(),
            scale,
            hover: None,
            pressed: None,
            chosen: None,
            closed: false,
        });
        let state_ptr: *mut MenuWindow = std::ptr::addr_of_mut!(*state);
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            w!("Snaplight.PopupMenu"),
            w!(""),
            WS_POPUP,
            card_x - margin,
            card_y - margin,
            first.width,
            first.height,
            None,
            None,
            instance,
            Some(state_ptr.cast()),
        )
        .ok()?;
        if let Err(error) = state.update_layered(hwnd) {
            eprintln!("tray menu render failed: {error}");
            let _ = DestroyWindow(hwnd);
            return None;
        }
        let previous = GetForegroundWindow();
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(hwnd);
        pump_until_closed(
            hwnd,
            state_ptr,
            |state: *const MenuWindow| (*state).closed,
            |state: *mut MenuWindow, hwnd: HWND, key: u32| match key {
                key if key == VK_ESCAPE.0 as u32 => {
                    (*state).closed = true;
                    true
                }
                key if key == VK_UP.0 as u32 => {
                    (*state).move_selection(false);
                    if let Err(error) = (*state).update_layered(hwnd) {
                        eprintln!("menu repaint failed: {error}");
                    }
                    true
                }
                key if key == VK_DOWN.0 as u32 => {
                    (*state).move_selection(true);
                    if let Err(error) = (*state).update_layered(hwnd) {
                        eprintln!("menu repaint failed: {error}");
                    }
                    true
                }
                key if key == VK_RETURN.0 as u32 => {
                    (*state).chosen = (*state).hover;
                    (*state).closed = true;
                    true
                }
                _ => false,
            },
        );
        if IsWindow(hwnd).as_bool() {
            let _ = DestroyWindow(hwnd);
        }
        if !previous.is_invalid() && previous != hwnd {
            let _ = SetForegroundWindow(previous);
        }
        (*state_ptr).chosen
    }
}

/// Pumps this thread's messages until `is_closed` reports true or the queue ends.
///
/// Both popups run their own modal loop while they are open, so the resident
/// process still waits in `GetMessageW` instead of polling. Keyboard messages
/// reach `on_key` first; returning `false` lets the default dispatch handle them.
/// Messages queued while the window was being created (activation in particular)
/// are dropped first, so they cannot be mistaken for the user clicking away.
///
/// The state is reached through a raw pointer on purpose: the window procedure
/// the loop dispatches to owns the same object (handed over through
/// `GWLP_USERDATA`) and writes the answer into it. A `&mut` here would let the
/// optimiser assume the loop's exit condition cannot change and hoist it out of
/// the loop — which is exactly what happened in release builds: the prompt came
/// up, the answer arrived, and the loop never noticed it.
pub(crate) unsafe fn pump_until_closed<T>(
    hwnd: HWND,
    state: *mut T,
    is_closed: fn(*const T) -> bool,
    on_key: fn(*mut T, HWND, u32) -> bool,
) {
    let mut queued = MSG::default();
    while PeekMessageW(&mut queued, hwnd, WM_ACTIVATE, WM_KILLFOCUS, PM_REMOVE).as_bool() {}

    let mut message = MSG::default();
    while !is_closed(state) {
        let result = GetMessageW(&mut message, None, 0, 0);
        if result.0 <= 0 {
            break;
        }
        if (message.message == WM_KEYDOWN || message.message == WM_SYSKEYDOWN)
            && on_key(state, hwnd, message.wParam.0 as u32)
        {
            continue;
        }
        let _ = TranslateMessage(&message);
        DispatchMessageW(&message);
    }
}

pub(crate) unsafe fn work_area(x: i32, y: i32) -> RECT {
    let monitor = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(monitor, &mut info).as_bool() {
        info.rcWork
    } else {
        RECT {
            left: 0,
            top: 0,
            right: GetSystemMetrics(SM_CXSCREEN),
            bottom: GetSystemMetrics(SM_CYSCREEN),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<Entry> {
        vec![
            Entry::Item(Item::new("截图").with_shortcut("Ctrl+Shift+A")),
            Entry::Item(Item::new("设置")),
            Entry::Item(Item::new("关闭所有贴图").danger()),
            Entry::Separator,
            Entry::Item(Item::new("退出")),
        ]
    }

    fn widths() -> Vec<(i32, i32)> {
        vec![(26, 84), (26, 0), (78, 0), (0, 0), (26, 0)]
    }

    fn work_area() -> RECT {
        RECT {
            left: 0,
            top: 0,
            right: 2560,
            bottom: 1440,
        }
    }

    #[test]
    fn menu_opens_at_the_upper_right_of_the_pointer() {
        let work = work_area();
        // The card's bottom-left corner sits on the pointer, so the menu grows
        // up and to the right of it.
        assert_eq!(place(180, 153, (700, 500), work), (700, 347));
        // A pointer on the bottom-right corner (the notification area) keeps the
        // bottom-left corner and slides left to stay inside the work area.
        assert_eq!(place(180, 153, (2540, 1430), work), (2380, 1277));
        // No room above: it drops below the pointer instead of leaving the screen.
        assert_eq!(place(180, 153, (700, 40), work), (700, 40));
        // A work area that is shorter than the card pins the top edge.
        let short = RECT {
            left: 0,
            top: 0,
            right: 800,
            bottom: 100,
        };
        assert_eq!(place(180, 153, (400, 60), short), (400, 0));
        // And it never leaves the work area on the left either.
        let narrow = RECT {
            left: 100,
            top: 100,
            right: 200,
            bottom: 400,
        };
        assert_eq!(place(180, 153, (120, 350), narrow), (100, 197));
    }

    #[test]
    fn layout_stacks_rows_and_separator_with_reference_metrics() {
        let layout = layout(&entries(), 1.0, &widths());
        // The shortcut row is the widest, but the card keeps its minimum width.
        assert_eq!(layout.card.width, 180);
        assert_eq!(layout.card.x, 14);
        assert_eq!(layout.card.y, 14);
        assert_eq!(layout.radius, 12);
        // 12 padding, four 28px rows and one 17px separator.
        assert_eq!(layout.card.height, 12 + 28 * 4 + 17 + 12);
        assert_eq!(layout.width, layout.card.width + 28);
        assert_eq!(layout.height, layout.card.height + 28);
        let bands: Vec<Rect> = layout.rows.iter().map(|row| row.band).collect();
        assert_eq!((bands[0].y, bands[0].height), (26, 28));
        assert_eq!(bands[1].y, 54);
        assert_eq!(bands[2].y, 82);
        // The separator sits between the third and fourth row.
        let (index, line) = layout.separators[0];
        assert_eq!(index, 3);
        assert_eq!(line.y, 118);
        assert_eq!(line.width, layout.card.width);
        assert_eq!(bands[3].y, 127);
        assert_eq!(layout.rows[0].text_left, 34);
        assert_eq!(layout.rows[0].text_right, 174);
        assert_eq!(layout.rows[0].highlight.x, 18);
        assert!(layout.rows[0].band.contains(layout.card.x, 30));
    }

    #[test]
    fn layout_scales_metrics_with_dpi() {
        let base = layout(&entries(), 1.0, &widths());
        let scaled = layout(
            &entries(),
            1.5,
            &widths()
                .iter()
                .map(|&(label, shortcut)| {
                    (
                        (label as f32 * 1.5).round() as i32,
                        (shortcut as f32 * 1.5).round() as i32,
                    )
                })
                .collect::<Vec<_>>(),
        );
        assert_eq!(scaled.radius, 18);
        assert_eq!(scaled.rows[0].band.height, 42);
        assert_eq!(scaled.rows[0].text_left, 51);
        assert_eq!(
            (scaled.card.width, scaled.card.height),
            (
                (base.card.width as f32 * 1.5).round() as i32,
                (base.card.height as f32 * 1.5).round() as i32
            )
        );
        assert_eq!(scaled.separators[0].1.height, 2);
    }

    #[test]
    fn layout_keeps_a_minimum_card_width() {
        let narrow = vec![Entry::Item(Item::new("退出"))];
        let layout = layout(&narrow, 1.0, &[(26, 0)]);
        assert_eq!(layout.card.width, 180);
        assert_eq!(layout.rows[0].text_right, 14 + 180 - 20);
    }

    #[test]
    fn render_draws_the_reference_card() {
        let entries = entries();
        let menu = render(&entries, 1.0, None).expect("render");
        let layout = menu.layout.clone();
        // A corner outside the rounded rectangle stays transparent.
        let corner = menu.pixel(layout.card.x, layout.card.y).expect("corner");
        assert!(corner[3] < 128, "corner should be transparent: {corner:?}");
        // Padding inside the card is opaque white.
        let inside = menu
            .pixel(layout.card.right() - 6, layout.card.bottom() - 6)
            .expect("card interior");
        assert_eq!(inside, [255, 255, 255, 255]);
        // The shadow below the card darkens the transparent margin.
        let shadow = menu
            .pixel(
                layout.card.x + layout.card.width / 2,
                layout.card.bottom() + 2,
            )
            .expect("shadow");
        assert!(shadow[3] > 10 && shadow[3] < 100, "shadow: {shadow:?}");
        assert!(shadow[0] < 40, "shadow must stay black: {shadow:?}");
        // The separator keeps its own colour across the card.
        let (_, line) = layout.separators[0];
        let separator = menu
            .pixel(line.x + line.width / 2, line.y)
            .expect("separator");
        assert_eq!(separator, [0xFD, 0xE3, 0xD3, 255]);
        // Labels are dark, the destructive row is red and hints are grey.
        let row_pixels = |row: &RowLayout| {
            let mut collected = Vec::new();
            for y in row.band.y..row.band.bottom() {
                for x in row.text_left..row.text_right {
                    if let Some(pixel) = menu.pixel(x, y) {
                        if pixel[3] == 255 {
                            collected.push(pixel);
                        }
                    }
                }
            }
            collected
        };
        let first = row_pixels(&layout.rows[0]);
        assert!(first.iter().any(|p| p[0] < 60 && p[1] < 60 && p[2] < 60));
        let danger = row_pixels(&layout.rows[2]);
        assert!(
            danger
                .iter()
                .any(|p| p[2] as i32 > 150 && p[2] as i32 > p[0] as i32 + 60),
            "destructive row must be red"
        );
        // The shortcut hint sits right of the label, in the shortcut colour.
        assert!(first.iter().any(|p| (60..120).contains(&p[0])
            && p[0].abs_diff(p[1]) < 12
            && p[0].abs_diff(p[2]) < 12));
    }

    #[test]
    fn render_marks_the_hovered_row() {
        let entries = entries();
        let plain = render(&entries, 1.0, None).expect("plain");
        let hovered = render(&entries, 1.0, Some(4)).expect("hovered");
        let row = hovered.layout.rows[3];
        let sample = (
            row.highlight.x + 2,
            row.highlight.y + row.highlight.height / 2,
        );
        assert_eq!(
            plain.pixel(sample.0, sample.1).unwrap(),
            [255, 255, 255, 255]
        );
        assert_eq!(
            hovered.pixel(sample.0, sample.1).unwrap(),
            [0xF2, 0xF2, 0xF2, 255]
        );
        // Hovering the separator entry must not paint anything.
        let none = render(&entries, 1.0, Some(99)).expect("no hover");
        assert_eq!(
            none.pixel(sample.0, sample.1).unwrap(),
            [255, 255, 255, 255]
        );
    }

    #[test]
    fn straight_rgba_uses_the_png_channel_order() {
        let entries = entries();
        let menu = render(&entries, 1.0, None).expect("render");
        let rgba = menu.straight_rgba();
        let offset = |x: i32, y: i32| ((y * menu.width + x) * 4) as usize;
        // The separator must reach the PNG as #D3E3FD, not as its mirror.
        let (_, line) = menu.layout.separators[0];
        let separator = &rgba[offset(line.x + line.width / 2, line.y)..][..4];
        assert_eq!(separator, [0xD3, 0xE3, 0xFD, 255]);
        // The destructive label stays red in RGB order.
        let row = menu.layout.rows[2];
        assert!(
            (row.band.y..row.band.bottom()).any(|y| (row.text_left..row.text_right).any(|x| {
                let p = &rgba[offset(x, y)..][..4];
                p[3] == 255 && p[0] as i32 > 150 && p[0] as i32 > p[2] as i32 + 60
            }))
        );
        // A transparent pixel stays transparent instead of dividing by zero.
        let corner = &rgba[offset(menu.layout.card.x, menu.layout.card.y)..][..4];
        assert!(corner[3] < 128);
    }
}

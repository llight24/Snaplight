//! Owner-drawn confirmation card with an "don't ask again" checkbox.
//!
//! `MessageBoxW` has no checkbox and `TaskDialogIndirect` — the system dialog
//! that has one — needs the Common Controls v6 activation context these
//! executables do not carry, so the prompt is drawn as the same GDI card the
//! popup menu uses: white rounded rectangle, soft shadow, presented through
//! `UpdateLayeredWindow`. [`popup_menu`] owns the card geometry, palette, font
//! and message-pump helpers so the two popups cannot drift apart.
//!
//! [`layout`] and [`render`] are separated from the window so the geometry, the
//! drawing and the answer state machine can be asserted without a user; [`ask`]
//! only adds the modal window around them.

use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
};

use super::popup_menu::{
    self, card_alpha, create_font, present_layered, rgb, scale_at, text_width, work_area, Rect,
    CARD, CARD_RADIUS, DANGER, FONT_EM, HOVER, HOVER_RADIUS, LABEL, MUTED, SHADOW_MARGIN,
};

/// Window class of the prompt; the smoke finds the window by it.
pub const WINDOW_CLASS: &str = "Snaplight.Confirm";

/// The `windows` crate does not export `WM_MOUSELEAVE`.
const WM_MOUSELEAVE: u32 = 0x02A3;

// Logical (96 DPI) metrics. The card follows the popup menu's; the prompt adds
// a title line, one remembered-answer row and a button pair.
const PAD: f32 = 20.0;
const TITLE_EM: f32 = FONT_EM + 1.0;
/// Text band height as a multiple of the em size.
const LINE_HEIGHT: f32 = 1.4;
const ROW_GAP: f32 = 6.0;
const SECTION_GAP: f32 = 16.0;
const MIN_WIDTH: f32 = 240.0;
const BUTTON_GAP: f32 = 8.0;
const BUTTON_PAD_X: f32 = 16.0;
const BUTTON_HEIGHT: f32 = 30.0;
const BOX: f32 = 16.0;
const BOX_RADIUS: f32 = 4.0;
const BOX_GAP: f32 = 8.0;
/// Hit band of the remembered-answer row.
const ROW_HEIGHT: f32 = 20.0;
/// Inset of that row's hover highlight from the card edge, as in the menu.
const ROW_INSET: f32 = 4.0;

/// Accent of a ticked box: the same blue Windows uses for its own checkbox.
const CHECK: (u8, u8, u8) = (0x00, 0x67, 0xC0);
/// Border of an empty box, and the check mark drawn inside a ticked one.
const BOX_BORDER: (u8, u8, u8) = (0x8A, 0x8A, 0x8A);
const CONFIRM_HOVER: (u8, u8, u8) = (0xDC, 0x4B, 0x4F);
const CONFIRM_PRESSED: (u8, u8, u8) = (0xB6, 0x28, 0x2C);
const CANCEL_HOVER: (u8, u8, u8) = (0xE6, 0xE6, 0xE6);
const CANCEL_PRESSED: (u8, u8, u8) = (0xDA, 0xDA, 0xDA);

/// What the user clicked on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Checkbox,
    Confirm,
    Cancel,
}

/// Text of one prompt. Everything the card shows comes from here, so the same
/// prompt can be asserted from a test without a window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub title: String,
    /// Secondary line under the title, drawn in the muted colour.
    pub message: String,
    /// Label of the remembered-answer row. `None` leaves the row out.
    pub checkbox: Option<String>,
    /// Label of the destructive button, drawn red.
    pub confirm: String,
    /// Label of the button that keeps the current state.
    pub cancel: String,
}

/// Text widths in physical pixels, one per field of [`Options`] and measured
/// with the font that field is drawn in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Measured {
    pub title: i32,
    pub message: i32,
    pub checkbox: i32,
    pub confirm: i32,
    pub cancel: i32,
}

/// The remembered-answer row: hit band, box and label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckRow {
    pub row: Rect,
    /// The square itself.
    pub tick: Rect,
    pub label: Rect,
}

/// Window and card geometry of one prompt at one DPI scale.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub scale: f32,
    /// Size of the layered window bitmap: card plus shadow margin.
    pub width: i32,
    pub height: i32,
    /// The card inside the window bitmap.
    pub card: Rect,
    pub radius: i32,
    /// Text bands, ready to be passed to `DrawTextW`.
    pub title: Rect,
    pub message: Rect,
    pub checkbox: Option<CheckRow>,
    pub confirm: Rect,
    pub cancel: Rect,
}
impl Layout {
    /// Control under `(x, y)`, in window coordinates. The card body itself is
    /// not a control: a click there is a click on the prompt, not on a button.
    pub fn hit(&self, x: i32, y: i32) -> Option<Target> {
        if let Some(row) = &self.checkbox {
            if row.row.contains(x, y) {
                return Some(Target::Checkbox);
            }
        }
        if self.confirm.contains(x, y) {
            return Some(Target::Confirm);
        }
        if self.cancel.contains(x, y) {
            return Some(Target::Cancel);
        }
        None
    }
}

/// Lays the prompt out for `scale` (1.0 = 96 DPI) from the measured `text`.
/// The card is as wide as its widest line, so no metrics can clip a label.
pub fn layout(options: &Options, scale: f32, text: &Measured) -> Layout {
    let scale = scale.clamp(0.5, 4.0);
    let px = |value: f32| (value * scale).round() as i32;
    let pad = px(PAD);
    let radius = px(CARD_RADIUS).max(2);
    let title_height = px(TITLE_EM * LINE_HEIGHT);
    let body_height = px(FONT_EM * LINE_HEIGHT);
    let row_height = px(ROW_HEIGHT).max(px(BOX));
    let tick_size = px(BOX);
    let label_gap = px(BOX_GAP);
    let button_height = px(BUTTON_HEIGHT);
    let button_pad = px(BUTTON_PAD_X);
    let button_gap = px(BUTTON_GAP);

    let row_width = options
        .checkbox
        .as_ref()
        .map(|_| tick_size + label_gap + text.checkbox)
        .unwrap_or(0);
    let buttons_width = text.confirm + 2 * button_pad + button_gap + text.cancel + 2 * button_pad;
    let content_width = text
        .title
        .max(text.message)
        .max(row_width)
        .max(buttons_width);
    let card_width = content_width.max(px(MIN_WIDTH)) + 2 * pad;

    let mut content_height = title_height + px(ROW_GAP) + body_height;
    if options.checkbox.is_some() {
        content_height += px(SECTION_GAP) + row_height;
    }
    content_height += px(SECTION_GAP) + button_height;

    let margin = px(SHADOW_MARGIN);
    let card = Rect {
        x: margin,
        y: margin,
        width: card_width,
        height: content_height + 2 * pad,
    };
    let left = card.x + pad;
    let content = card.width - 2 * pad;

    let mut y = card.y + pad;
    let title = Rect {
        x: left,
        y,
        width: content,
        height: title_height,
    };
    y += title_height + px(ROW_GAP);
    let message = Rect {
        x: left,
        y,
        width: content,
        height: body_height,
    };
    y += body_height;
    let checkbox = options.checkbox.as_ref().map(|_| {
        y += px(SECTION_GAP);
        let row = Rect {
            x: left,
            y,
            width: content,
            height: row_height,
        };
        let tick = Rect {
            x: left,
            y: y + (row_height - tick_size) / 2,
            width: tick_size,
            height: tick_size,
        };
        let label = Rect {
            x: left + tick_size + label_gap,
            y,
            width: content - tick_size - label_gap,
            height: row_height,
        };
        y += row_height;
        CheckRow { row, tick, label }
    });
    y += px(SECTION_GAP);

    // The button pair sits at the trailing edge, cancel last, as in the menu.
    let cancel_width = text.cancel + 2 * button_pad;
    let confirm_width = text.confirm + 2 * button_pad;
    let cancel = Rect {
        x: card.x + card.width - pad - cancel_width,
        y,
        width: cancel_width,
        height: button_height,
    };
    let confirm = Rect {
        x: cancel.x - button_gap - confirm_width,
        y,
        width: confirm_width,
        height: button_height,
    };

    Layout {
        scale,
        width: card.width + 2 * margin,
        height: card.height + 2 * margin,
        card,
        radius,
        title,
        message,
        checkbox,
        confirm,
        cancel,
    }
}

/// Top-left corner of a `width`×`height` window centred on `anchor` (screen
/// physical pixels), clamped into the work area it opens on.
pub fn center(width: i32, height: i32, anchor: (i32, i32), work: RECT) -> (i32, i32) {
    let mut x = anchor.0 - width / 2;
    let mut y = anchor.1 - height / 2;
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

/// What the user answered. `remember` is the state of the checkbox and is only
/// meaningful together with `accepted`: cancelling claims nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Answer {
    pub accepted: bool,
    pub remember: bool,
}

/// Interaction state of one prompt: which control is hovered, armed or ticked,
/// and what the answer is once it is chosen.
#[derive(Clone, Debug)]
pub struct State {
    layout: Layout,
    checked: bool,
    hover: Option<Target>,
    pressed: Option<Target>,
    closed: bool,
    answer: Answer,
}
impl State {
    /// Measures `options` at `scale` and starts with nothing hovered, nothing
    /// ticked and no answer.
    pub fn new(options: &Options, scale: f32) -> Result<Self> {
        Ok(Self {
            layout: probe(options, scale)?,
            checked: false,
            hover: None,
            pressed: None,
            closed: false,
            answer: Answer::default(),
        })
    }
    pub fn layout(&self) -> &Layout {
        &self.layout
    }
    pub fn answer(&self) -> Answer {
        self.answer
    }
    fn is_closed(&self) -> bool {
        self.closed
    }
    /// Pointer moved to `(x, y)`; returns whether the appearance changed.
    pub fn hover_at(&mut self, x: i32, y: i32) -> bool {
        let hover = self.layout.hit(x, y);
        let changed = self.hover != hover;
        self.hover = hover;
        changed
    }
    /// Presses the pointer: a press on a control arms it, a press outside the
    /// card dismisses the prompt the way Esc does.
    pub fn press_at(&mut self, x: i32, y: i32) -> bool {
        if !self.layout.card.contains(x, y) {
            self.cancel();
            return true;
        }
        let target = self.layout.hit(x, y);
        let changed = self.pressed != target || self.hover != target;
        self.pressed = target;
        self.hover = target;
        changed
    }
    /// Releases the pointer. Only the control the press armed is chosen, and the
    /// remembered-answer row toggles instead of answering.
    pub fn release_at(&mut self, x: i32, y: i32) -> bool {
        let target = self.layout.hit(x, y);
        let armed = self.pressed;
        self.pressed = None;
        let mut changed = self.hover != target;
        self.hover = target;
        if armed.is_none() || armed != target {
            return changed;
        }
        match target {
            Some(Target::Checkbox) => {
                self.checked = !self.checked;
                changed = true;
            }
            Some(Target::Confirm) => {
                self.accept();
                changed = true;
            }
            Some(Target::Cancel) => {
                self.cancel();
                changed = true;
            }
            None => {}
        }
        changed
    }
    /// Handles a key press: Enter accepts, Esc cancels and Space toggles the
    /// remembered-answer row, which only exists when the caller offered one.
    pub fn key(&mut self, virtual_key: u32) -> bool {
        match virtual_key {
            key if key == VK_RETURN.0 as u32 => {
                self.accept();
                true
            }
            key if key == VK_ESCAPE.0 as u32 => {
                self.cancel();
                true
            }
            key if key == VK_SPACE.0 as u32 && self.layout.checkbox.is_some() => {
                self.checked = !self.checked;
                true
            }
            _ => false,
        }
    }
    /// Accepts with the current checkbox state.
    fn accept(&mut self) {
        self.answer = Answer {
            accepted: true,
            remember: self.checked,
        };
        self.closed = true;
    }
    /// Dismisses the prompt without a choice. Also what losing focus, clicking
    /// outside the card and the owner window disappearing mean, so a prompt can
    /// never outlive the moment it was asked for.
    pub fn cancel(&mut self) {
        self.answer = Answer::default();
        self.closed = true;
    }
}

/// A rendered prompt: premultiplied BGRA pixels, top-down, ready for
/// `UpdateLayeredWindow`.
#[derive(Clone, Debug)]
pub struct Bitmap {
    pub width: i32,
    pub height: i32,
    pub premultiplied_bgra: Vec<u8>,
}
impl Bitmap {
    /// Premultiplied BGRA at `(x, y)`, or `None` outside the bitmap.
    pub fn pixel(&self, x: i32, y: i32) -> Option<[u8; 4]> {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return None;
        }
        let offset = ((y * self.width + x) * 4) as usize;
        let p = &self.premultiplied_bgra[offset..offset + 4];
        Some([p[0], p[1], p[2], p[3]])
    }
}

/// Measures `options` and returns the prompt's geometry. Separate from
/// [`render`] so the window can be created at its final size.
pub fn probe(options: &Options, scale: f32) -> Result<Layout> {
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
        let result = (|| -> Result<Layout> {
            let title_font = create_font(scale, TITLE_EM, FW_SEMIBOLD.0 as i32)?;
            let body_font = create_font(scale, FONT_EM, FW_NORMAL.0 as i32)?;
            let measured = {
                let old = SelectObject(memory, title_font);
                let title = text_width(memory, &options.title);
                SelectObject(memory, body_font);
                let message = text_width(memory, &options.message);
                let checkbox = options
                    .checkbox
                    .as_deref()
                    .map(|label| text_width(memory, label))
                    .unwrap_or(0);
                let confirm = text_width(memory, &options.confirm);
                let cancel = text_width(memory, &options.cancel);
                SelectObject(memory, old);
                Measured {
                    title,
                    message,
                    checkbox,
                    confirm,
                    cancel,
                }
            };
            let _ = DeleteObject(title_font);
            let _ = DeleteObject(body_font);
            Ok(layout(options, scale, &measured))
        })();
        let _ = DeleteDC(memory);
        ReleaseDC(None, desktop);
        result
    }
}

/// Renders the prompt in the state it is currently in.
pub fn render(options: &Options, state: &State) -> Result<Bitmap> {
    let layout = state.layout();
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
        let result = render_into(
            memory,
            options,
            layout,
            state.checked,
            state.hover,
            state.pressed,
        );
        let _ = DeleteDC(memory);
        ReleaseDC(None, desktop);
        result
    }
}

unsafe fn render_into(
    dc: HDC,
    options: &Options,
    layout: &Layout,
    checked: bool,
    hover: Option<Target>,
    pressed: Option<Target>,
) -> Result<Bitmap> {
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
    let result = (|| -> Result<Bitmap> {
        draw(dc, options, layout, checked, hover, pressed)?;
        // GDI has no per-pixel alpha, so the alpha channel is derived from the
        // card geometry: opaque inside the rounded rectangle, shadow outside.
        let pixels = std::slice::from_raw_parts_mut(
            bits.cast::<u8>(),
            (layout.width * layout.height * 4) as usize,
        );
        card_alpha(
            pixels,
            layout.width,
            layout.height,
            layout.card,
            layout.radius,
            layout.scale,
        );
        Ok(Bitmap {
            width: layout.width,
            height: layout.height,
            premultiplied_bgra: pixels.to_vec(),
        })
    })();
    SelectObject(dc, old_bitmap);
    let _ = DeleteObject(bitmap);
    result
}

unsafe fn draw(
    dc: HDC,
    options: &Options,
    layout: &Layout,
    checked: bool,
    hover: Option<Target>,
    pressed: Option<Target>,
) -> Result<()> {
    let title_font = create_font(layout.scale, TITLE_EM, FW_SEMIBOLD.0 as i32)?;
    let body_font = create_font(layout.scale, FONT_EM, FW_NORMAL.0 as i32)?;
    let background = CreateSolidBrush(rgb(CARD));
    let full = RECT {
        left: 0,
        top: 0,
        right: layout.width,
        bottom: layout.height,
    };
    FillRect(dc, &full, background);
    let _ = DeleteObject(background);

    let inset = (ROW_INSET * layout.scale).round() as i32;
    if let Some(row) = &layout.checkbox {
        if hover == Some(Target::Checkbox) {
            let highlight = Rect {
                x: row.row.x + inset,
                y: row.row.y,
                width: row.row.width - 2 * inset,
                height: row.row.height,
            };
            fill_round(
                dc,
                highlight,
                (HOVER_RADIUS * layout.scale).round() as i32,
                HOVER,
            );
        }
        let corner = (BOX_RADIUS * layout.scale).round() as i32;
        if checked {
            fill_round(dc, row.tick, corner, CHECK);
            let pen = CreatePen(PS_SOLID, box_pen(layout.scale), rgb(CARD));
            let old_pen = SelectObject(dc, pen);
            let points = [
                POINT {
                    x: row.tick.x + (row.tick.width as f32 * 0.26) as i32,
                    y: row.tick.y + (row.tick.height as f32 * 0.52) as i32,
                },
                POINT {
                    x: row.tick.x + (row.tick.width as f32 * 0.43) as i32,
                    y: row.tick.y + (row.tick.height as f32 * 0.69) as i32,
                },
                POINT {
                    x: row.tick.x + (row.tick.width as f32 * 0.74) as i32,
                    y: row.tick.y + (row.tick.height as f32 * 0.33) as i32,
                },
            ];
            let _ = Polyline(dc, &points);
            SelectObject(dc, old_pen);
            let _ = DeleteObject(pen);
        } else {
            outline_round(dc, row.tick, corner, box_pen(layout.scale), BOX_BORDER);
        }
    }

    let corner = (HOVER_RADIUS * layout.scale).round() as i32;
    let buttons = [
        (
            layout.confirm,
            Target::Confirm,
            DANGER,
            CONFIRM_HOVER,
            CONFIRM_PRESSED,
        ),
        (
            layout.cancel,
            Target::Cancel,
            HOVER,
            CANCEL_HOVER,
            CANCEL_PRESSED,
        ),
    ];
    for (rect, target, base, hover_color, pressed_color) in buttons {
        let fill = if pressed == Some(target) {
            pressed_color
        } else if hover == Some(target) {
            hover_color
        } else {
            base
        };
        fill_round(dc, rect, corner, fill);
    }

    SetBkMode(dc, TRANSPARENT);
    let centered = DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX;
    let leading = DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX;
    let old_font = SelectObject(dc, title_font);
    draw_text(dc, layout.title, &options.title, LABEL, leading);
    SelectObject(dc, body_font);
    draw_text(dc, layout.message, &options.message, MUTED, leading);
    if let (Some(row), Some(label)) = (&layout.checkbox, options.checkbox.as_deref()) {
        draw_text(dc, row.label, label, LABEL, leading);
    }
    draw_text(dc, layout.confirm, &options.confirm, CARD, centered);
    draw_text(dc, layout.cancel, &options.cancel, LABEL, centered);
    SelectObject(dc, old_font);
    let _ = DeleteObject(title_font);
    let _ = DeleteObject(body_font);
    Ok(())
}

/// Pen width of the box outline and check mark: one logical pixel, thin enough
/// not to fill a 16px box.
fn box_pen(scale: f32) -> i32 {
    ((1.5 * scale).round() as i32).max(1)
}

unsafe fn fill_round(dc: HDC, rect: Rect, corner: i32, color: (u8, u8, u8)) {
    let brush = CreateSolidBrush(rgb(color));
    let old_brush = SelectObject(dc, brush);
    let old_pen = SelectObject(dc, GetStockObject(NULL_PEN));
    let _ = RoundRect(
        dc,
        rect.x,
        rect.y,
        rect.right(),
        rect.bottom(),
        corner * 2,
        corner * 2,
    );
    SelectObject(dc, old_pen);
    SelectObject(dc, old_brush);
    let _ = DeleteObject(brush);
}

unsafe fn outline_round(dc: HDC, rect: Rect, corner: i32, width: i32, color: (u8, u8, u8)) {
    let pen = CreatePen(PS_SOLID, width, rgb(color));
    let old_pen = SelectObject(dc, pen);
    let old_brush = SelectObject(dc, GetStockObject(NULL_BRUSH));
    let _ = RoundRect(
        dc,
        rect.x,
        rect.y,
        rect.right(),
        rect.bottom(),
        corner * 2,
        corner * 2,
    );
    SelectObject(dc, old_brush);
    SelectObject(dc, old_pen);
    let _ = DeleteObject(pen);
}

unsafe fn draw_text(
    dc: HDC,
    rect: Rect,
    text: &str,
    color: (u8, u8, u8),
    format: DRAW_TEXT_FORMAT,
) {
    if text.is_empty() {
        return;
    }
    SetTextColor(dc, rgb(color));
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    let mut bounds = RECT {
        left: rect.x,
        top: rect.y,
        right: rect.right(),
        bottom: rect.bottom(),
    };
    DrawTextW(dc, &mut wide, &mut bounds, format);
}

/// One open prompt: its text and its interaction state. The window procedure
/// reaches both through the window's user data, so the box must not move.
struct Dialog {
    options: Options,
    state: State,
}

unsafe extern "system" fn confirm_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let create = &*(l.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Dialog;
    if !ptr.is_null() {
        let dialog = &mut *ptr;
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
            // Keys are handled here instead of by the message pump: a message
            // sent to the window (as the smoke does) reaches this procedure
            // directly and never passes the pump's keyboard branch.
            WM_KEYDOWN => {
                if dialog.state.key(w.0 as u32) && !dialog.state.is_closed() {
                    repaint(hwnd, dialog);
                }
                return LRESULT(0);
            }
            WM_MOUSEMOVE => {
                let (x, y) = point();
                if dialog.state.hover_at(x, y) {
                    repaint(hwnd, dialog);
                }
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
                // Off the card: no control is hovered any more.
                if dialog.state.hover_at(-1, -1) {
                    repaint(hwnd, dialog);
                }
                return LRESULT(0);
            }
            WM_LBUTTONDOWN => {
                // A press outside the card dismisses the prompt, and the loop is
                // about to end, so there is nothing left to repaint.
                if dialog.state.press_at(point().0, point().1) && !dialog.state.is_closed() {
                    repaint(hwnd, dialog);
                }
                return LRESULT(0);
            }
            WM_LBUTTONUP => {
                if dialog.state.release_at(point().0, point().1) && !dialog.state.is_closed() {
                    repaint(hwnd, dialog);
                }
                return LRESULT(0);
            }
            WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_MOUSEWHEEL => {
                dialog.state.cancel();
                return LRESULT(0);
            }
            WM_ACTIVATE => {
                // Losing focus is treated as "not now": the answer stays a
                // cancel, so the caller never closes anything by accident.
                if (w.0 & 0xFFFF) as u32 == WA_INACTIVE {
                    dialog.state.cancel();
                }
                return LRESULT(0);
            }
            WM_KILLFOCUS | WM_CLOSE | WM_CANCELMODE => {
                dialog.state.cancel();
                return LRESULT(0);
            }
            WM_DESTROY => {
                // The prompt can also die with its owner; the loop has to end
                // even then, so the thread cannot wait for a window that is gone.
                dialog.state.cancel();
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, w, l)
}

unsafe fn repaint(hwnd: HWND, dialog: &Dialog) {
    match render(&dialog.options, &dialog.state) {
        Ok(bitmap) => {
            if let Err(error) = present_layered(
                hwnd,
                bitmap.width,
                bitmap.height,
                &bitmap.premultiplied_bgra,
            ) {
                eprintln!("confirm repaint failed: {error}");
            }
        }
        Err(error) => eprintln!("confirm repaint failed: {error}"),
    }
}

/// Shows the prompt centred on `anchor` (screen physical pixels) and owned by
/// `owner`, and blocks until the user answers or the prompt is dismissed.
///
/// The owner is what ties the prompt's life to the thing being closed: going
/// away destroys the prompt, and its cancellation leaves the caller's state
/// alone. Failure to show the prompt is reported to the caller instead of being
/// treated as an answer — nothing is closed without the user saying so.
///
/// # Safety
///
/// `owner` must be a window handle that stays valid while the prompt is open (or
/// is destroyed together with it, which is what the ownership gives), and the
/// caller must be on a thread with a message queue: the prompt runs its own
/// modal loop and needs the queue to receive input, timers and the owner's
/// destruction. Intended to be called from a window procedure, as the pin
/// manager does.
pub unsafe fn ask(owner: HWND, anchor: (i32, i32), options: &Options) -> Result<Answer> {
    let scale = scale_at(anchor.0, anchor.1);
    let mut dialog = Box::new(Dialog {
        options: options.clone(),
        state: State::new(options, scale)?,
    });
    popup_menu::register_class(w!("Snaplight.Confirm"), confirm_proc)?;
    let instance = GetModuleHandleW(None)?;
    let (width, height) = (dialog.state.layout.width, dialog.state.layout.height);
    let work = work_area(anchor.0, anchor.1);
    let (x, y) = center(width, height, anchor, work);
    let ptr: *mut Dialog = std::ptr::addr_of_mut!(*dialog);
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
        w!("Snaplight.Confirm"),
        w!("Snaplight Confirm"),
        WS_POPUP,
        x,
        y,
        width,
        height,
        owner,
        None,
        instance,
        Some(ptr.cast()),
    )?;
    repaint(hwnd, &dialog);
    let previous = GetForegroundWindow();
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    let _ = SetForegroundWindow(hwnd);
    let _ = SetFocus(hwnd);
    popup_menu::pump_until_closed(
        hwnd,
        ptr,
        |dialog: *const Dialog| unsafe { (*dialog).state.is_closed() },
        // Every key is handled by the window procedure, so the pump has nothing
        // to intercept here.
        |_: *mut Dialog, _: HWND, _: u32| false,
    );
    // Read before destroying: destroying the window cancels the state, and that
    // must not turn an accepted answer into a rejection. The window procedure
    // owns the same dialog, so the answer is read through the same pointer.
    let result = (*ptr).state.answer();
    if IsWindow(hwnd).as_bool() {
        let _ = DestroyWindow(hwnd);
    }
    if !previous.is_invalid() && previous != hwnd {
        let _ = SetForegroundWindow(previous);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Options {
        Options {
            title: "关闭贴图".into(),
            message: "关闭后贴图会从桌面移除，截图文件仍保留。".into(),
            checkbox: Some("下次不再提醒".into()),
            confirm: "关闭贴图".into(),
            cancel: "取消".into(),
        }
    }

    fn measured() -> Measured {
        Measured {
            title: 52,
            message: 240,
            checkbox: 72,
            confirm: 52,
            cancel: 26,
        }
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
    fn layout_stacks_the_lines_and_keeps_the_buttons_at_the_trailing_edge() {
        let layout = layout(&options(), 1.0, &measured());
        // Card inset by the shadow margin; the widest line is the message, and
        // it is narrower than the minimum width, so the card keeps its floor.
        assert_eq!(layout.card.x, 14);
        assert_eq!(layout.card.width, 240 + 40);
        assert_eq!(layout.radius, 12);
        // 20 padding, a 18px title, a 17px message, a 20px row, a 30px button
        // row and three gaps.
        assert_eq!(
            layout.card.height,
            20 + 18 + 6 + 17 + 16 + 20 + 16 + 30 + 20
        );
        assert_eq!(layout.width, layout.card.width + 28);
        assert_eq!(layout.height, layout.card.height + 28);

        assert_eq!((layout.title.x, layout.title.y), (34, 34));
        assert_eq!(layout.title.height, 18);
        assert_eq!(layout.message.y, 34 + 18 + 6);
        // The box is centred in the row and the label follows it.
        let row = layout.checkbox.expect("checkbox row");
        assert_eq!(row.row.y, layout.message.y + 17 + 16);
        assert_eq!(row.tick.x, 34);
        assert_eq!(row.tick.width, 16);
        assert_eq!(row.tick.y, row.row.y + 2);
        assert_eq!(row.label.x, 34 + 16 + 8);

        // Buttons share the baseline, cancel last: the widest button is as wide
        // as its label plus the padding, and nothing leaves the card.
        assert_eq!(layout.cancel.height, 30);
        assert_eq!(layout.cancel.right(), layout.card.right() - 20);
        assert_eq!(layout.cancel.width, 26 + 32);
        assert_eq!(layout.confirm.width, 52 + 32);
        assert_eq!(layout.confirm.right() + 8, layout.cancel.x);
        assert_eq!(layout.confirm.bottom(), layout.card.bottom() - 20);
    }

    #[test]
    fn layout_widens_for_the_widest_line_and_scales_with_dpi() {
        let wide = Measured {
            message: 400,
            ..measured()
        };
        let widened = layout(&options(), 1.0, &wide);
        assert_eq!(widened.card.width, 400 + 40);
        assert_eq!(widened.cancel.right(), widened.card.right() - 20);

        let base = layout(&options(), 1.0, &measured());
        let doubled = layout(
            &options(),
            2.0,
            &Measured {
                title: 104,
                message: 480,
                checkbox: 144,
                confirm: 104,
                cancel: 52,
            },
        );
        assert_eq!(doubled.card.width, base.card.width * 2);
        assert_eq!(doubled.card.height, base.card.height * 2);
        assert_eq!(doubled.radius, base.radius * 2);
        assert_eq!(doubled.confirm.height, base.confirm.height * 2);
    }

    #[test]
    fn hit_testing_finds_the_controls_and_ignores_the_card_body() {
        let layout = layout(&options(), 1.0, &measured());
        let row = layout.checkbox.expect("checkbox row");
        assert_eq!(
            layout.hit(row.tick.x + 2, row.tick.y + 2),
            Some(Target::Checkbox)
        );
        assert_eq!(
            layout.hit(row.label.x + 4, row.label.y + row.label.height / 2),
            Some(Target::Checkbox),
            "the label is part of the row"
        );
        assert_eq!(
            layout.hit(
                layout.confirm.x + layout.confirm.width / 2,
                layout.confirm.y + layout.confirm.height / 2
            ),
            Some(Target::Confirm)
        );
        assert_eq!(
            layout.hit(
                layout.cancel.x + layout.cancel.width / 2,
                layout.cancel.y + layout.cancel.height / 2
            ),
            Some(Target::Cancel)
        );
        // The title, the message and the gap between the buttons are the prompt
        // itself, not a control.
        assert_eq!(layout.hit(layout.title.x + 4, layout.title.y + 4), None);
        assert_eq!(
            layout.hit(
                layout.confirm.right() + 2,
                layout.confirm.y + layout.confirm.height / 2
            ),
            None
        );
    }

    #[test]
    fn the_prompt_opens_centred_and_stays_inside_the_work_area() {
        let work = work_area();
        assert_eq!(center(300, 200, (1000, 700), work), (850, 600));
        // Near the right and bottom edges it is pushed back inside instead of
        // hanging off the screen.
        assert_eq!(center(300, 200, (2550, 1430), work), (2260, 1240));
        assert_eq!(center(300, 200, (10, 10), work), (0, 0));
    }

    #[test]
    fn render_draws_the_card_the_box_and_both_buttons() {
        let options = options();
        let mut state = State::new(&options, 1.0).expect("state");
        let plain = render(&options, &state).expect("render");
        let layout = state.layout.clone();

        // A corner outside the rounded card stays transparent, the interior is
        // opaque white.
        let corner = plain.pixel(layout.card.x, layout.card.y).expect("corner");
        assert!(corner[3] < 128, "corner should be transparent: {corner:?}");
        assert_eq!(
            plain
                .pixel(layout.card.right() - 6, layout.card.bottom() - 6)
                .unwrap(),
            [255, 255, 255, 255]
        );
        // The destructive button is filled red, the other one is the menu's
        // hover grey, both with their labels on top.
        let inside = |rect: Rect| (rect.x + 3, rect.y + rect.height / 2);
        let (x, y) = inside(layout.confirm);
        let confirm = plain.pixel(x, y).unwrap();
        assert_eq!(confirm, [0x38, 0x34, 0xD1, 255], "confirm is red");
        let (x, y) = inside(layout.cancel);
        assert_eq!(plain.pixel(x, y).unwrap(), [0xF2, 0xF2, 0xF2, 255]);

        // An empty box shows its border; a ticked one is filled with the accent
        // and carries a light check mark.
        let row = layout.checkbox.expect("checkbox row");
        let middle = row.tick.y + row.tick.height / 2;
        let border = (row.tick.x - 1..=row.tick.x + 2)
            .filter_map(|x| plain.pixel(x, middle))
            .any(|p| p[3] == 255 && p[0] < 220);
        assert!(border, "an empty box needs a visible border");
        assert_eq!(
            plain
                .pixel(row.tick.x + row.tick.width / 2, middle)
                .unwrap(),
            [255, 255, 255, 255],
            "an empty box stays white inside"
        );
        state.press_at(row.label.x + 2, row.label.y + 2);
        state.release_at(row.label.x + 2, row.label.y + 2);
        let ticked = render(&options, &state).expect("ticked");
        // Away from the check mark, a ticked box is filled with the accent blue
        // (premultiplied BGRA, so blue is the first channel).
        for (x, y) in [
            (row.tick.x + 3, row.tick.y + 3),
            (row.tick.x + 3, row.tick.y + row.tick.height - 4),
        ] {
            let filled = ticked.pixel(x, y).unwrap();
            assert!(
                filled[3] == 255 && filled[0] > 120 && filled[0] > filled[2] + 60,
                "a ticked box is filled with the accent blue: {filled:?}"
            );
        }
        // Hovering a control has to be visible; the button under the pointer
        // changes shade and the remembered-answer row gets a highlight.
        state.hover_at(layout.cancel.x + 3, layout.cancel.y + 3);
        let hovered = render(&options, &state).expect("hovered");
        let (x, y) = inside(layout.cancel);
        assert_ne!(hovered.pixel(x, y).unwrap(), [0xF2, 0xF2, 0xF2, 255]);

        let row_centre = (
            row.row.x + row.row.width / 2,
            row.row.y + row.row.height / 2,
        );
        assert_eq!(
            plain.pixel(row_centre.0, row_centre.1).unwrap(),
            [255, 255, 255, 255],
            "the row is part of the card while nothing is hovered"
        );
        state.hover_at(row_centre.0, row_centre.1);
        let over_row = render(&options, &state).expect("row hover");
        assert_ne!(
            over_row.pixel(row_centre.0, row_centre.1).unwrap(),
            [255, 255, 255, 255]
        );
    }

    #[test]
    fn the_answer_comes_from_the_armed_control_and_the_checkbox() {
        let options = options();
        let mut state = State::new(&options, 1.0).expect("state");
        let layout = state.layout.clone();
        assert_eq!(state.answer(), Answer::default());

        // Releasing away from the armed control answers nothing at all.
        let confirm = (
            layout.confirm.x + layout.confirm.width / 2,
            layout.confirm.y + layout.confirm.height / 2,
        );
        state.press_at(confirm.0, confirm.1);
        state.release_at(confirm.0, layout.title.y + 2);
        assert!(!state.is_closed());
        assert_eq!(state.answer(), Answer::default());

        // A press on the card body is not a press on anything either.
        state.press_at(layout.message.x + 2, layout.message.y + 2);
        state.release_at(layout.message.x + 2, layout.message.y + 2);
        assert!(!state.is_closed());

        // Ticking the box and accepting reports both facts.
        let row = layout.checkbox.expect("checkbox row");
        state.press_at(row.label.x + 2, row.label.y + 2);
        state.release_at(row.label.x + 2, row.label.y + 2);
        assert!(state.checked);
        assert!(!state.is_closed(), "ticking does not answer");
        state.press_at(confirm.0, confirm.1);
        state.release_at(confirm.0, confirm.1);
        assert!(state.is_closed());
        assert_eq!(
            state.answer(),
            Answer {
                accepted: true,
                remember: true
            }
        );
    }

    #[test]
    fn cancelling_and_the_keyboard_leave_the_caller_alone() {
        let options = options();
        let mut state = State::new(&options, 1.0).expect("state");
        let layout = state.layout.clone();

        // Cancel button.
        let cancel = (
            layout.cancel.x + layout.cancel.width / 2,
            layout.cancel.y + layout.cancel.height / 2,
        );
        state.press_at(cancel.0, cancel.1);
        state.release_at(cancel.0, cancel.1);
        assert!(state.is_closed());
        assert_eq!(state.answer(), Answer::default());

        // Esc cancels; Space ticks the box; Enter accepts with what it shows.
        let mut state = State::new(&options, 1.0).expect("state");
        assert!(state.key(VK_SPACE.0 as u32));
        assert!(state.checked);
        assert!(state.key(VK_RETURN.0 as u32));
        assert_eq!(
            state.answer(),
            Answer {
                accepted: true,
                remember: true
            }
        );

        let mut state = State::new(&options, 1.0).expect("state");
        assert!(!state.key(VK_LEFT.0 as u32), "unrelated keys pass through");
        assert!(state.key(VK_ESCAPE.0 as u32));
        assert_eq!(state.answer(), Answer::default());

        // A press outside the card dismisses the prompt, as a menu does.
        let mut state = State::new(&options, 1.0).expect("state");
        state.press_at(2, 2);
        assert!(state.is_closed());
        assert_eq!(state.answer(), Answer::default());
    }

    #[test]
    fn a_prompt_without_the_row_toggles_nothing() {
        let options = Options {
            checkbox: None,
            ..options()
        };
        let mut state = State::new(&options, 1.0).expect("state");
        assert!(state.layout.checkbox.is_none());
        assert!(!state.key(VK_SPACE.0 as u32), "there is nothing to tick");
        assert!(state.key(VK_RETURN.0 as u32));
        assert_eq!(
            state.answer(),
            Answer {
                accepted: true,
                remember: false
            }
        );
    }
}

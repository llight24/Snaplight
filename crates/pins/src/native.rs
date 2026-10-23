use snaplight_domain::*;
use snaplight_platform::popup_menu::{Entry, Item};
use snaplight_platform::{confirm, popup_menu, read_pixels, Capabilities, WindowService};
use snaplight_render_win::{Gpu, StaticSurface};
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command},
    sync::{Arc, Mutex},
};
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{Com::*, LibraryLoader::*},
        UI::{
            HiDpi::GetDpiForWindow,
            Input::{
                GetRawInputData, KeyboardAndMouse::*, RegisterRawInputDevices, HRAWINPUT, RAWINPUT,
                RAWINPUTDEVICE, RAWINPUTHEADER, RAWINPUT_0, RAWKEYBOARD, RAWMOUSE, RAWMOUSE_0,
                RAWMOUSE_0_0, RIDEV_INPUTSINK, RIDEV_REMOVE, RID_INPUT, RIM_TYPEKEYBOARD,
                RIM_TYPEMOUSE,
            },
            WindowsAndMessaging::*,
        },
    },
};

/// Drop shadow of a pin: metrics, geometry and the composed bitmap.
mod shadow;
const INCOMING: u32 = WM_APP + 51;
const CLOSED: u32 = WM_APP + 52;
/// Posted by a pin window after a command that can change whether any pin is
/// click-through (the context menu entry). The control window re-evaluates the
/// held-shortcut timer on it: a pin only becomes click-through from its own
/// window procedure, so without this signal the poll would never start and
/// holding the key would do nothing.
const HELD_SYNC: u32 = WM_APP + 53;
/// Timer that watches the held shortcut. It only runs while at least one pin is
/// click-through, so an idle pin process never polls.
const HELD_TIMER: usize = 1;
/// Short enough that holding the key feels immediate, long enough that the
/// polling stays negligible next to the compositor's own work.
const HELD_POLL_MS: u32 = 16;
/// Posted by the thread that waits for an annotation session's editor process,
/// so the pin can pick up the picture the session wrote back.
const EDIT_DONE: u32 = WM_APP + 54;
/// Posted by a pin the pointer clicked, so the host can make it the only selected
/// pin. The address of that pin arrives in the wparam: clearing every other pin
/// needs all of them, so the host owns the selection, and posting is what keeps
/// the host from being re-entered while its own frame is on the stack. The
/// lparam carries the click's `GetMessageTime`, so a signal that arrives after a
/// newer selection cannot move the selection back.
const SELECTED: u32 = WM_APP + 55;
/// Thickness of the frame that marks the selected pin, in physical pixels on
/// screen.
const OUTLINE_PX: f32 = 2.0;
/// Colour of that frame: a fixed blue, deliberately not the configured theme's
/// accent, so the selected pin reads the same whatever theme is in use.
const SELECTION_COLOR: [f32; 4] = [0x33 as f32 / 255.0, 0x88 as f32 / 255.0, 1.0, 1.0];
/// Entry indices of the pin context menu, in display order.
mod pin_item {
    pub const EDIT: usize = 0;
    pub const THROUGH: usize = 1;
    pub const RESET: usize = 2;
    pub const SHADOW: usize = 3;
    pub const DESTROY: usize = 5;
}
/// Pin context menu content, drawn by the same owner-drawn card as the tray
/// menu. `restore_hotkey` is the shortcut that brings interaction back after the
/// pin goes click-through. It is shown on the right of that entry, and the entry
/// itself stays a plain label so the card never has to explain the held key.
/// Destroying the pin discards the image the user pinned, so it is drawn in red.
///
/// Restoring the original size is one of the things a middle click on the pin
/// does too, so it is not only a menu entry. It deliberately does not live on a
/// double-click: that now closes the pin (the "close" gesture the user asked
/// for), so the entry and the middle click both say what the reset does.
///
/// The shadow entry names what it does instead of naming a state: the card has
/// no checkmark to show one, and the shadow being on or off is visible on the
/// desktop anyway.
fn menu_entries(restore_hotkey: &str, shadow_on: bool) -> Vec<Entry> {
    let shadow = if shadow_on {
        "关闭阴影"
    } else {
        "开启阴影"
    };
    vec![
        // Annotating a pin again is the first thing the menu can do to it, and
        // the shortcut on its right teaches the key that does the same from the
        // pointer.
        Entry::Item(Item::new("编辑").with_shortcut("空格")),
        Entry::Item(Item::new("鼠标穿透").with_shortcut(restore_hotkey)),
        Entry::Item(Item::new("恢复原始大小")),
        Entry::Item(Item::new(shadow)),
        Entry::Separator,
        Entry::Item(Item::new("销毁").danger()),
    ]
}
/// Shadow state of one pin: what the configuration supplies plus the choice
/// made in this pin's context menu.
#[derive(Clone, Copy, Debug)]
struct Shadow {
    /// Metrics as configured, in logical pixels at 96 DPI.
    settings: shadow::Settings,
    /// Scale of the monitor the pin opened on, so a shadow keeps the same
    /// visual thickness on every display.
    dpi: f32,
    /// What the configuration says a new pin gets; re-read on reload.
    default_on: bool,
    /// The per-pin choice from the context menu; `None` follows the default.
    /// It deliberately survives a reload: changing the metrics in the settings
    /// must not switch a shadow the user turned off back on.
    choice: Option<bool>,
}
impl Shadow {
    fn enabled(&self) -> bool {
        self.choice.unwrap_or(self.default_on)
    }
    /// Metrics in physical pixels at 100% zoom, or `None` without a shadow.
    fn params(&self) -> Option<shadow::Params> {
        self.enabled().then(|| self.settings.at(self.dpi))
    }
    /// Room the shadow needs around the image at 100% zoom.
    fn margin(&self) -> i32 {
        self.params().map(|params| params.margin()).unwrap_or(0)
    }
}
struct Pin {
    hwnd: HWND,
    control: HWND,
    gpu: Option<Gpu>,
    surface: Option<StaticSurface>,
    path: PathBuf,
    image: ImageDescriptor,
    closed: bool,
    through: bool,
    /// True while the held shortcut is temporarily restoring interaction, so the
    /// release restores click-through without waiting for the next poll.
    temporary: bool,
    /// True while this pin is the selected one: its frame is drawn, and pressing
    /// the annotation shortcut opens its session. The selection is exclusive and
    /// deliberately outlives the pointer leaving the pin — the user picks a pin
    /// with a click, not by keeping the pointer on it — and the host clears it
    /// when another pin is clicked, when a press lands elsewhere, on Escape, and
    /// as soon as this pin stops being a valid target (click-through, closed, or
    /// editing).
    selected: bool,
    /// True while an annotation session is running for this pin, so the shortcut
    /// and the context menu cannot start a second one.
    editing: bool,
    scale: f32,
    opacity: f32,
    /// Drop shadow drawn behind the capture. The window reserves its margin, so
    /// the pin is larger than the image whenever it is on.
    shadow: Shadow,
    /// Shortcut shown next to the context menu entry, resolved at startup.
    restore_hotkey: String,
}
/// Nearest-neighbour zoom of a capture that has no shadow to compose. The
/// descriptor describes the scaled bitmap, not the file, so callers can size the
/// window and the DIB from it without a second formula.
fn scaled_image(
    image: &ImageDescriptor,
    bytes: &[u8],
    scale: f32,
) -> std::result::Result<(ImageDescriptor, Vec<u8>), Box<dyn std::error::Error>> {
    if bytes.len() < image.byte_len().map_err(|e| e.to_string())? {
        return Err("pixel buffer is shorter than the image".into());
    }
    let (width, height) = shadow::composed_size(image.width, image.height, 0, scale);
    let stride = width.checked_mul(4).ok_or("scaled image stride overflow")?;
    let size = (stride as usize)
        .checked_mul(height as usize)
        .ok_or("scaled image overflow")?;
    let mut scaled = vec![0u8; size];
    for y in 0..height {
        for x in 0..width {
            let sx = (x as u64 * image.width as u64 / u64::from(width.max(1))) as usize;
            let sy = (y as u64 * image.height as u64 / u64::from(height.max(1))) as usize;
            let src = sy * image.stride as usize + sx * 4;
            let dest = (y as usize * width as usize + x as usize) * 4;
            scaled[dest..dest + 4].copy_from_slice(&bytes[src..src + 4]);
        }
    }
    Ok((
        ImageDescriptor {
            id: image.id,
            width,
            height,
            stride,
            format: image.format,
            alpha: image.alpha,
            color_space: image.color_space,
        },
        scaled,
    ))
}
impl Pin {
    /// The bitmap this pin presents: the capture, plus its shadow when it has
    /// one, both at `scale` (the surface path zooms with a transform, so it
    /// composes at 100% instead).
    fn composed_at(
        &self,
        scale: f32,
    ) -> std::result::Result<(ImageDescriptor, Vec<u8>), Box<dyn std::error::Error>> {
        let (image, bytes) = read_pixels(&self.path)?;
        if let Some(params) = self.shadow.params() {
            return shadow::compose(&image, &bytes, &params, scale);
        }
        if shadow::composed_size(image.width, image.height, 0, scale) == (image.width, image.height)
        {
            return Ok((image, bytes));
        }
        scaled_image(&image, &bytes, scale)
    }
    /// The bitmap this pin presents, framed while it is selected so the target of
    /// the annotation shortcut is visible. `zoom` is the scale the bitmap itself
    /// is drawn at: the layered path composes at the pin's zoom, the surface path
    /// composes at 100% and zooms with a transform, so the frame is `OUTLINE_PX`
    /// physical pixels in both.
    fn bitmap(
        &self,
        zoom: f32,
    ) -> std::result::Result<(ImageDescriptor, Vec<u8>), Box<dyn std::error::Error>> {
        let (image, mut bytes) = self.composed_at(zoom)?;
        if self.selected {
            let margin = shadow::scaled_margin(self.shadow.margin(), zoom) as i32;
            let thickness = (OUTLINE_PX / zoom).round().max(1.0) as i32;
            draw_outline(
                &mut bytes,
                &image,
                (
                    margin,
                    margin,
                    image.width as i32 - margin,
                    image.height as i32 - margin,
                ),
                thickness,
                SELECTION_COLOR,
            );
        }
        Ok((image, bytes))
    }
    /// Margin the window reserves for the shadow at the current zoom.
    fn margin(&self) -> i32 {
        shadow::scaled_margin(self.shadow.margin(), self.scale) as i32
    }
    /// Size of the window: the zoomed image plus the shadow margin on every
    /// side, from the same formula the composed bitmap is sized with.
    fn window_size(&self) -> (i32, i32) {
        let (width, height) = shadow::composed_size(
            self.image.width,
            self.image.height,
            self.shadow.margin(),
            self.scale,
        );
        (width as i32, height as i32)
    }
    /// Resizes and moves the window, keeping the image on the screen pixels it
    /// occupies: the capture sits `margin` inside the window, so a margin that
    /// changed by `previous - margin` moves the window's origin the other way.
    /// Without this, toggling the shadow would shift the pin by the margin.
    unsafe fn place(
        &mut self,
        previous_margin: i32,
        flags: SET_WINDOW_POS_FLAGS,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut rect = RECT::default();
        GetWindowRect(self.hwnd, &mut rect)?;
        let (width, height) = self.window_size();
        let margin = self.margin();
        SetWindowPos(
            self.hwnd,
            HWND_TOPMOST,
            rect.left + previous_margin - margin,
            rect.top + previous_margin - margin,
            width,
            height,
            flags,
        )?;
        Ok(())
    }
    unsafe fn restore(&mut self) -> std::result::Result<(), Box<dyn std::error::Error>> {
        if self.is_click_through() || self.gpu.is_none() {
            return self.update_layered();
        }
        if let Some(surface) = &self.surface {
            surface.scale(self.scale)?;
            surface.opacity(self.opacity)?;
            return Ok(());
        }
        // The surface holds the bitmap at 100% and reaches the pin's zoom with
        // its own scale transform, so the shadow is composed unscaled here.
        let (image, bytes) = self.bitmap(1.0)?;
        // Coming back from click-through means the composition target of this
        // window has to be created again, and DirectComposition answers
        // `DCOMPOSITION_ERROR_WINDOW_ALREADY_COMPOSED` while it is still winding
        // down the previous target, which made the toggle fail. Retrying keeps
        // the pin on the composition path.
        let mut attempt = 0;
        let surface = loop {
            match StaticSurface::new(self.gpu.as_ref().unwrap(), self.hwnd, &image, &bytes) {
                Ok(surface) => break surface,
                Err(error) => {
                    if attempt >= 20 {
                        return Err(error.into());
                    }
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        };
        surface.scale(self.scale)?;
        surface.opacity(self.opacity)?;
        self.surface = Some(surface);
        Ok(())
    }
    unsafe fn update_layered(&mut self) -> std::result::Result<(), Box<dyn std::error::Error>> {
        // The composer already renders at this pin's zoom, so there is no
        // second scaling step: this is the bitmap that goes on screen, shadow
        // margin included.
        let (image, bytes) = self.bitmap(self.scale)?;
        let width = image.width;
        let height = image.height;
        let desktop = GetDC(None);
        let memory = CreateCompatibleDC(desktop);
        let mut bits = std::ptr::null_mut();
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: 40,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let bitmap = match CreateDIBSection(desktop, &info, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(v) => v,
            Err(e) => {
                let _ = DeleteDC(memory);
                ReleaseDC(None, desktop);
                return Err(e.into());
            }
        };
        let old = SelectObject(memory, bitmap);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), bits.cast(), bytes.len());
        let mut screen = RECT::default();
        GetWindowRect(self.hwnd, &mut screen)?;
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: (self.opacity * 255.0) as u8,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let result = UpdateLayeredWindow(
            self.hwnd,
            desktop,
            Some(&POINT {
                x: screen.left,
                y: screen.top,
            }),
            Some(&SIZE {
                cx: width as i32,
                cy: height as i32,
            }),
            memory,
            Some(&POINT::default()),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
        SelectObject(memory, old);
        let _ = DeleteObject(bitmap);
        let _ = DeleteDC(memory);
        ReleaseDC(None, desktop);
        result?;
        Ok(())
    }
    /// True when the pin is passing the mouse through at this instant: the user
    /// asked for click-through and the held shortcut is not lifting that.
    ///
    /// The intent (`through`) and the lift (`temporary`) are deliberately two
    /// fields. Lifting has to leave the intent alone: the poll asks which pins
    /// want click-through, and the first version cleared that flag while lifting
    /// — so the very next tick saw "nothing wants click-through", put the pin
    /// back, and the two states traded places every 16 ms. The user saw the pin
    /// repaint over and over instead of being operable while the key was down.
    fn is_click_through(&self) -> bool {
        self.through && !self.temporary
    }
    /// Flips the pin's click-through intent, as the context menu entry does.
    unsafe fn toggle(&mut self) -> std::result::Result<(), Box<dyn std::error::Error>> {
        self.set_through(!self.through, false)
    }
    /// Sets the click-through intent and the held-shortcut lift together, and
    /// touches the window only when the resulting state really changed: applying
    /// the style throws the composition target away and rebuilds it, so a call
    /// that changes nothing must not repaint the pin.
    unsafe fn set_through(
        &mut self,
        intent: bool,
        temporary: bool,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let was = self.is_click_through();
        self.through = intent;
        self.temporary = temporary;
        if self.is_click_through() == was {
            return Ok(());
        }
        // A click-through pin is no longer the pointer's target, so it cannot be
        // waiting for the annotation shortcut either.
        if self.is_click_through() {
            self.selected = false;
        }
        self.apply_through()
    }
    /// Applies the extended style for `is_click_through()` and redraws the pin.
    ///
    /// Click-through is the layered hit-testing path: asking only for
    /// `WS_EX_TRANSPARENT` leaves the window in the hit test, and the pin kept
    /// swallowing the clicks. A composition pin therefore hands its window over
    /// to the layered rendering while it passes the mouse through — its
    /// no-redirection-bitmap style cannot be cleared afterwards, so both styles
    /// coexist and `restore` draws the image as a layered bitmap meanwhile.
    unsafe fn apply_through(&mut self) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let previous = self.margin();
        self.surface = None;
        let through = self.is_click_through();
        let mut style = WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        if through {
            style |= WS_EX_LAYERED | WS_EX_TRANSPARENT;
        }
        if self.gpu.is_some() {
            style |= WS_EX_NOREDIRECTIONBITMAP;
        } else if !through {
            style |= WS_EX_LAYERED;
        }
        SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, style.0 as isize);
        if through && self.gpu.is_some() {
            self.scale = 1.0;
        }
        self.place(previous, SWP_NOACTIVATE | SWP_FRAMECHANGED)?;
        self.restore()
    }
    unsafe fn zoom(&mut self, value: f32) -> std::result::Result<(), Box<dyn std::error::Error>> {
        if self.is_click_through() {
            return Ok(());
        }
        let previous = self.margin();
        self.scale = value.clamp(0.1, 4.0);
        if let Some(surface) = &self.surface {
            surface.scale(self.scale)?;
        } else {
            self.update_layered()?;
        }
        // Zoom grows the window, and with a shadow also grows the margin: the
        // window's origin moves so the capture keeps its place on screen.
        self.place(previous, SWP_NOACTIVATE)
    }
    /// Records the choice made in this pin's context menu and redraws it.
    unsafe fn set_shadow(
        &mut self,
        enabled: bool,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        if self.shadow.enabled() == enabled {
            return Ok(());
        }
        // The margin the window still has, needed to keep the capture in place.
        let previous = self.margin();
        self.shadow.choice = Some(enabled);
        self.refresh_shadow(previous)
    }
    /// Rebuilds the presented bitmap after the shadow changed, either from the
    /// context menu or from the settings. The window is resized and moved first
    /// so the capture keeps its screen position while the margin changes, then
    /// the new bitmap is presented.
    unsafe fn refresh_shadow(
        &mut self,
        previous_margin: i32,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        self.surface = None;
        self.place(previous_margin, SWP_NOACTIVATE)?;
        self.restore()
    }
    /// Marks this pin as selected — or takes that back — and repaints it with the
    /// frame that says so. The frame lives in the presented bitmap, so the pin is
    /// re-presented at once. Exclusivity is not this method's job: it only ever
    /// changes this pin, and the host clears the others.
    unsafe fn set_selected(
        &mut self,
        selected: bool,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        if self.selected == selected {
            return Ok(());
        }
        self.selected = selected;
        self.refresh()
    }
    /// Presents the pin again: reloads the picture (an annotation session
    /// rewrites the file while the pin is on screen), resizes the window when the
    /// picture changed size, and uploads the bitmap. The surface path keeps the
    /// bitmap it was built from, so it is rebuilt rather than rescaled.
    unsafe fn refresh(&mut self) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let (image, _) = read_pixels(&self.path)?;
        if image.width != self.image.width || image.height != self.image.height {
            self.image = image;
            let margin = self.margin();
            self.surface = None;
            self.place(margin, SWP_NOACTIVATE)?;
            return self.restore();
        }
        if self.gpu.is_none() || self.is_click_through() {
            return self.update_layered();
        }
        self.surface = None;
        self.restore()
    }
    /// Screen rectangle the picture occupies: the window without the shadow
    /// margin, which is what an annotation session has to cover.
    unsafe fn picture_rect(&self) -> RECT {
        let mut rect = RECT::default();
        if GetWindowRect(self.hwnd, &mut rect).is_err() {
            return rect;
        }
        let margin = self.margin();
        let width = (self.image.width as f32 * self.scale).round() as i32;
        let height = (self.image.height as f32 * self.scale).round() as i32;
        RECT {
            left: rect.left + margin,
            top: rect.top + margin,
            right: rect.left + margin + width,
            bottom: rect.top + margin + height,
        }
    }
}
/// Draws a frame of one opaque colour along the inside of `area`, where the
/// picture sits inside a composed bitmap: the mark of the pin the pointer last
/// clicked, so the target of the annotation shortcut is visible before the
/// session opens.
fn draw_outline(
    bytes: &mut [u8],
    image: &ImageDescriptor,
    area: (i32, i32, i32, i32),
    thickness: i32,
    color: [f32; 4],
) {
    let (left, top, right, bottom) = area;
    if thickness < 1 || right - left <= 2 * thickness || bottom - top <= 2 * thickness {
        return;
    }
    let byte = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    // BGRA8 and opaque: the frame has to mark the pin over any picture.
    let pixel = [byte(color[2]), byte(color[1]), byte(color[0]), 255];
    let (width, height) = (image.width as i32, image.height as i32);
    let mut put = |x: i32, y: i32| {
        if x < 0 || y < 0 || x >= width || y >= height {
            return;
        }
        let n = (y as usize * width as usize + x as usize) * 4;
        if n + 4 <= bytes.len() {
            bytes[n..n + 4].copy_from_slice(&pixel);
        }
    };
    for band in 0..thickness {
        for x in left + band..right - band {
            put(x, top + band);
            put(x, bottom - 1 - band);
        }
        for y in top + band..bottom - band {
            put(left + band, y);
            put(right - 1 - band, y);
        }
    }
}
/// Starts the editor on this pin's files, so the pinned picture can be annotated
/// again. `demo` runs the editor's scripted session instead of showing a window,
/// which is how the handoff is checked without a user.
fn spawn_pin_editor(pin: &Pin, demo: bool) -> std::io::Result<Child> {
    let rect = unsafe { pin.picture_rect() };
    let flag = if demo {
        "--edit-pin-demo"
    } else {
        "--edit-pin"
    };
    Command::new(snaplight_platform::sibling("snaplight-editor")?)
        .arg(format!("{flag}={}", pin.path.display()))
        .arg(format!(
            "--pin-rect={},{},{},{}",
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top
        ))
        .spawn()
}
/// Opens an annotation session on a pin: the editor loads the document the
/// capture was pinned with and writes the annotated picture back into the same
/// files, which the pin reloads when that process exits.
unsafe fn open_annotation(
    pin: &mut Pin,
    demo: bool,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    if pin.editing || pin.closed {
        return Ok(());
    }
    let mut child = spawn_pin_editor(pin, demo)?;
    pin.editing = true;
    // The session's own window is the feedback from here on, and a pin whose
    // session is running is not a target the shortcut may pick again.
    pin.set_selected(false)?;
    // The window handle is carried as an integer: window handles are not `Send`,
    // and the waiter only has to post one message to it.
    let control = pin.control.0 as isize;
    let address = (pin as *const Pin) as usize;
    std::thread::spawn(move || {
        let _ = child.wait();
        unsafe {
            let _ = PostMessageW(
                HWND(control as *mut core::ffi::c_void),
                EDIT_DONE,
                WPARAM(address),
                LPARAM(0),
            );
        }
    });
    Ok(())
}
/// Identity of a pin in the messages its window and the host exchange: the
/// address of the boxed `Pin`. Pins are never moved or dropped while their window
/// lives — closed pins stay allocated on purpose — so this stays a valid identity
/// for the pin's whole lifetime.
fn pin_address(pin: &Pin) -> usize {
    pin as *const Pin as usize
}
/// Asks the host to make `pin` the only selected pin, for the click whose
/// message time is `time`.
///
/// Posted, not sent: clearing every other pin needs the whole host, and posting
/// keeps the host from being re-entered while one of its own frames is on the
/// stack — the same reason the close and click-through signals are posted. The
/// pin's address is carried as an integer because that is what identifies it to
/// the host, and pins are boxed so the address is stable for their lifetime.
/// The click's message time travels in the lparam so the host can tell a late
/// signal from a click that is older than the selection already in force.
unsafe fn post_selected(pin: &Pin, time: u32) -> Result<()> {
    PostMessageW(
        pin.control,
        SELECTED,
        WPARAM(pin_address(pin)),
        LPARAM(time as isize),
    )
}
/// Scale of the display a pin window sits on (1.0 = 96 DPI). The pin's content
/// is physical pixels, so the shadow metrics are scaled by this to keep the same
/// visual thickness on every display. A query that fails reports 100%.
unsafe fn window_dpi_scale(hwnd: HWND) -> f32 {
    let dpi = GetDpiForWindow(hwnd);
    if dpi == 0 {
        1.0
    } else {
        (dpi as f32 / 96.0).clamp(1.0, 4.0)
    }
}
struct NativeWindows;
impl WindowService for NativeWindows {
    type Window = HWND;
    type Error = Error;
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            capture: true,
            global_hotkeys: true,
            positioning: true,
            topmost: true,
            click_through: true,
        }
    }
    fn set_click_through(&self, window: &HWND, enabled: bool) -> Result<()> {
        unsafe {
            let ptr = GetWindowLongPtrW(*window, GWLP_USERDATA) as *mut Pin;
            if ptr.is_null() {
                return Err(Error::new(E_INVALIDARG, "unknown pin window"));
            }
            let pin = &mut *ptr;
            pin.set_through(enabled, false)
                .map_err(|e| Error::new(E_FAIL, e.to_string()))?;
            Ok(())
        }
    }
}
/// Shows the pin context menu at the pointer and applies the chosen command.
///
/// The menu runs its own modal message loop, so the pin can also be closed from
/// another path while it is open (a `CloseAll` command, or the destroy entry of
/// a second menu); the window is re-checked before it is touched.
unsafe fn show_pin_menu(pin: &mut Pin) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let entries = menu_entries(&pin.restore_hotkey, pin.shadow.enabled());
    let choice = popup_menu::show(&entries, None);
    if pin.closed || !IsWindow(pin.hwnd).as_bool() {
        return Ok(());
    }
    match choice {
        Some(pin_item::EDIT) => open_annotation(pin, false)?,
        Some(pin_item::THROUGH) => toggle_through(pin)?,
        Some(pin_item::RESET) => pin.zoom(1.0)?,
        Some(pin_item::SHADOW) => pin.set_shadow(!pin.shadow.enabled())?,
        Some(pin_item::DESTROY) => {
            PostMessageW(pin.hwnd, WM_CLOSE, WPARAM(0), LPARAM(0))?;
        }
        _ => {}
    }
    Ok(())
}
/// Flips one pin's click-through state and tells the control window, which owns
/// the held-shortcut timer, to re-evaluate that timer.
///
/// A pin only becomes click-through from its own window procedure, so this
/// signal is what starts the poll the held shortcut needs — and what stops it
/// once nothing is click-through any more. The control window cannot be reached
/// directly from here without re-entering the message loop, so it is asked.
unsafe fn toggle_through(pin: &mut Pin) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let control = pin.control;
    pin.toggle()?;
    PostMessageW(control, HELD_SYNC, WPARAM(0), LPARAM(0))?;
    Ok(())
}
/// Text of the prompt shown before a double-clicked pin is closed.
fn close_options() -> confirm::Options {
    confirm::Options {
        title: "关闭贴图".into(),
        message: "关闭后贴图会从桌面移除，截图文件仍保留。".into(),
        checkbox: Some("下次不再提醒".into()),
        confirm: "关闭贴图".into(),
        cancel: "取消".into(),
    }
}

/// Whether closing a pin asks first. Read from the configuration at the moment
/// of the double-click rather than at startup: the prompt writes the "don't ask
/// again" answer into this very field, and the settings switch has to take
/// effect without the pin process being restarted.
fn close_confirmation_enabled() -> bool {
    match snaplight_platform::load_config() {
        Ok(config) => config.pin_close_confirm,
        // A configuration that cannot be read must not silently drop a
        // confirmation, and closing a pin throws away what the user pinned, so
        // asking is the conservative answer.
        Err(error) => {
            eprintln!("pin close confirmation setting unreadable ({error}); asking anyway");
            true
        }
    }
}

/// Persists "don't ask again". The configuration is re-read first so this write
/// cannot roll back a change the settings window saved in the meantime.
fn remember_no_confirmation() {
    let result = (|| -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut config = snaplight_platform::load_config()?;
        if config.pin_close_confirm {
            config.pin_close_confirm = false;
            snaplight_platform::save_config(&config)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        // The pin still closes: the user asked for that, and a configuration
        // that cannot be written is not a reason to keep it on screen.
        eprintln!("could not remember the pin close confirmation: {error}");
    }
}

/// Closes the pin the user double-clicked, asking first unless the
/// configuration turned the prompt off. The prompt cannot be shown at all only
/// when the environment refuses the window; the pin then stays on screen, and
/// the context menu still closes it.
unsafe fn close_pin(pin: &mut Pin) -> std::result::Result<(), Box<dyn std::error::Error>> {
    if close_confirmation_enabled() {
        let mut rect = RECT::default();
        GetWindowRect(pin.hwnd, &mut rect)?;
        let anchor = ((rect.left + rect.right) / 2, (rect.top + rect.bottom) / 2);
        match confirm::ask(pin.hwnd, anchor, &close_options()) {
            Ok(answer) => {
                if !answer.accepted {
                    return Ok(());
                }
                if answer.remember {
                    remember_no_confirmation();
                }
            }
            Err(error) => {
                eprintln!("pin close confirmation failed: {error}");
                return Ok(());
            }
        }
    }
    if pin.closed || !IsWindow(pin.hwnd).as_bool() {
        return Ok(());
    }
    PostMessageW(pin.hwnd, WM_CLOSE, WPARAM(0), LPARAM(0))?;
    Ok(())
}

/// Pumps this thread's message queue for `timeout_ms`. An annotation session runs
/// in the editor process and reaches its pin as a posted message, so a smoke step
/// that waits for one has to keep this queue moving.
fn pump_messages(timeout_ms: i32) {
    let mut waited = 0;
    while waited < timeout_ms {
        let mut message = MSG::default();
        unsafe {
            if PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            } else {
                std::thread::sleep(std::time::Duration::from_millis(20));
                waited += 20;
            }
        }
    }
}
/// Presses or releases one key through `SendInput`, so the smoke can check that a
/// shortcut is really read back from the keyboard. Only opt-in smoke steps call
/// this: they press a key on the user's desktop.
unsafe fn send_key(
    key: VIRTUAL_KEY,
    down: bool,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: key,
                wScan: 0,
                dwFlags: if down {
                    KEYBD_EVENT_FLAGS(0)
                } else {
                    KEYEVENTF_KEYUP
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    if SendInput(&[input], std::mem::size_of::<INPUT>() as i32) != 1 {
        return Err("could not inject the shortcut key".into());
    }
    Ok(())
}
/// A synthetic raw keyboard event, for the smoke. `WM_INPUT` carries a handle to
/// hardware input that cannot be produced without pressing a key, so a check that
/// must not press one hands the decoded event to the very handler the message
/// reaches. Writing a union field is safe; only reading one needs `unsafe`.
fn keyboard_event(virtual_key: u16) -> RAWINPUT {
    RAWINPUT {
        header: RAWINPUTHEADER {
            dwType: RIM_TYPEKEYBOARD.0,
            ..Default::default()
        },
        data: RAWINPUT_0 {
            keyboard: RAWKEYBOARD {
                VKey: virtual_key,
                ..Default::default()
            },
        },
    }
}
/// A synthetic raw left-button press, for the smoke. Like [`keyboard_event`], it
/// hands the decoded event to the handler the message reaches: a press that
/// another window consumed cannot be produced without clicking on the user's
/// desktop, and the position and time the handler judges it by are passed
/// alongside. Writing a union field is safe; only reading one needs `unsafe`.
fn mouse_event() -> RAWINPUT {
    RAWINPUT {
        header: RAWINPUTHEADER {
            dwType: RIM_TYPEMOUSE.0,
            ..Default::default()
        },
        data: RAWINPUT_0 {
            mouse: RAWMOUSE {
                Anonymous: RAWMOUSE_0 {
                    Anonymous: RAWMOUSE_0_0 {
                        usButtonFlags: RI_MOUSE_LEFT_BUTTON_DOWN as u16,
                        ..Default::default()
                    },
                },
                ..Default::default()
            },
        },
    }
}
/// Answers the close prompt from another thread while the pin's window
/// procedure is blocked inside it. A modal prompt cannot be clicked by the
/// thread that is inside it, and sending keys to the prompt's procedure is also
/// what keeps this check independent of the prompt taking focus from the user.
///
/// Returns once `keys` were delivered or the pin closed on its own, and fails
/// when neither happened within a few seconds — a prompt that never appears is
/// exactly what this has to catch. The window handle arrives as an address and
/// the failure as a plain string, so nothing that is not `Send` crosses the
/// thread boundary.
fn answer_prompt(pin_address: usize, keys: &[u32]) -> std::result::Result<(), String> {
    let pin = HWND(pin_address as *mut std::ffi::c_void);
    for _ in 0..600 {
        unsafe {
            if !IsWindow(pin).as_bool() {
                return Ok(());
            }
        }
        if let Some(dialog) = unsafe { find_prompt(pin) } {
            for key in keys {
                unsafe { SendMessageW(dialog, WM_KEYDOWN, WPARAM(*key as usize), LPARAM(0)) };
            }
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // Never leave a prompt unanswered: the smoke's main thread is blocked inside
    // it, so a prompt nobody answers would hang the run instead of failing it.
    if let Some(dialog) = unsafe { find_prompt(pin) } {
        unsafe { SendMessageW(dialog, WM_CLOSE, WPARAM(0), LPARAM(0)) };
    }
    Err("the pin close confirmation did not appear".into())
}

/// Finds the prompt owned by `pin`. The windows are enumerated instead of asked
/// for by name: `FindWindowW` did not see a freshly created window on this
/// machine before (see the tray menu record), and the owner check also keeps two
/// pins' prompts apart.
unsafe fn find_prompt(pin: HWND) -> Option<HWND> {
    struct Search {
        pin: HWND,
        found: HWND,
    }
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let search = &mut *(lparam.0 as *mut Search);
        let mut class = [0u16; 128];
        let length = GetClassNameW(hwnd, &mut class);
        let is_prompt = length > 0
            && String::from_utf16_lossy(&class[..length as usize]) == confirm::WINDOW_CLASS;
        let owned = GetWindow(hwnd, GW_OWNER)
            .map(|owner| owner == search.pin)
            .unwrap_or(false);
        if is_prompt && owned {
            search.found = hwnd;
            return BOOL(0);
        }
        BOOL(1)
    }
    let mut search = Search {
        pin,
        found: HWND::default(),
    };
    let _ = EnumWindows(Some(visit), LPARAM((&mut search as *mut Search) as isize));
    (!search.found.0.is_null()).then_some(search.found)
}

/// Joins the thread that answers the prompt and reports its failure.
fn join_answer(
    worker: std::thread::JoinHandle<std::result::Result<(), String>>,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    worker
        .join()
        .map_err(|_| "the prompt answer thread panicked")??;
    Ok(())
}

/// Double-clicks `pin` the way the system reports a click on its caption, then
/// processes what that posted, and reports whether the pin window is gone.
unsafe fn double_click_close(pin: HWND) -> bool {
    let _ = SendMessageW(pin, WM_NCLBUTTONDBLCLK, WPARAM(0), LPARAM(0));
    let mut queued = MSG::default();
    while PeekMessageW(&mut queued, pin, 0, 0, PM_REMOVE).as_bool() {
        let _ = TranslateMessage(&queued);
        DispatchMessageW(&queued);
    }
    !IsWindow(pin).as_bool()
}

/// Window class of the plain window the shadow checks read the screen against.
const BACKDROP_CLASS: PCWSTR = w!("Snaplight.SmokeBackdrop");

/// Creates a solid-coloured window behind the pins. The smoke runs on a real
/// desktop, so putting a known colour under the pin is the only way to tell a
/// drawn shadow from whatever the user happens to have open there. It is
/// created before the pin so the pin stays above it.
unsafe fn backdrop(
    rect: RECT,
    color: COLORREF,
) -> std::result::Result<HWND, Box<dyn std::error::Error>> {
    let instance = GetModuleHandleW(None)?;
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        hInstance: instance.into(),
        lpszClassName: BACKDROP_CLASS,
        lpfnWndProc: Some(backdrop_proc),
        // The window paints nothing itself; the class brush is the colour.
        hbrBackground: CreateSolidBrush(color),
        ..Default::default()
    };
    let registered = RegisterClassW(&class);
    if registered == 0 {
        let error = Error::from_win32();
        if error.code() != HRESULT::from_win32(ERROR_CLASS_ALREADY_EXISTS.0) {
            return Err(error.into());
        }
    }
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        BACKDROP_CLASS,
        w!("Snaplight Smoke Backdrop"),
        WS_POPUP,
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
        None,
        None,
        instance,
        None,
    )?;
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    Ok(hwnd)
}

/// Requires the desktop pixel at `(x, y)` to satisfy `matches` within a second,
/// reporting what the desktop showed instead. The pins are layered windows on a
/// live desktop, so the label says what the pixel is supposed to be.
unsafe fn require_pixel(
    x: i32,
    y: i32,
    what: &str,
    matches: impl Fn((u8, u8, u8)) -> bool,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    match wait_for_pixel(x, y, matches) {
        Ok(_) => Ok(()),
        Err(pixel) => Err(format!("{what} at ({x}, {y}); the screen shows {pixel:?}").into()),
    }
}

/// The backdrop has no behaviour of its own: the class brush paints it.
unsafe extern "system" fn backdrop_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, msg, w, l)
}

/// Reads one pixel of the composed desktop as straight `(r, g, b)`.
unsafe fn screen_pixel(x: i32, y: i32) -> Option<(u8, u8, u8)> {
    let desktop = GetDC(None);
    let value = GetPixel(desktop, x, y);
    ReleaseDC(None, desktop);
    if value.0 == CLR_INVALID {
        return None;
    }
    Some((
        (value.0 & 0xff) as u8,
        ((value.0 >> 8) & 0xff) as u8,
        ((value.0 >> 16) & 0xff) as u8,
    ))
}

/// Waits for the desktop pixel at `(x, y)` to satisfy `matches`, for up to a
/// second. A composition pin presents on the compositor's own schedule, so a
/// single read can still show the frame before the change. `Err` carries the
/// last pixel that was read, so a failure says what was there instead.
unsafe fn wait_for_pixel(
    x: i32,
    y: i32,
    matches: impl Fn((u8, u8, u8)) -> bool,
) -> std::result::Result<(u8, u8, u8), (u8, u8, u8)> {
    let mut last = (0, 0, 0);
    for _ in 0..100 {
        if let Some(pixel) = screen_pixel(x, y) {
            if matches(pixel) {
                return Ok(pixel);
            }
            last = pixel;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Err(last)
}

/// Finds the popup menu window this process opened, if any. Only the pin's own
/// menu can be open at this point and it belongs to this process, so its class
/// name identifies it. Windows are enumerated instead of asked for by name:
/// `FindWindowW` did not see a freshly created window on this machine before
/// (see the tray menu record).
unsafe fn find_popup_menu() -> Option<HWND> {
    struct Search {
        found: HWND,
    }
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let search = &mut *(lparam.0 as *mut Search);
        let mut class = [0u16; 128];
        let length = GetClassNameW(hwnd, &mut class);
        if length > 0
            && String::from_utf16_lossy(&class[..length as usize]) == popup_menu::WINDOW_CLASS
        {
            search.found = hwnd;
            return BOOL(0);
        }
        BOOL(1)
    }
    let mut search = Search {
        found: HWND::default(),
    };
    let _ = EnumWindows(Some(visit), LPARAM((&mut search as *mut Search) as isize));
    (!search.found.0.is_null()).then_some(search.found)
}

/// Presses and releases one row of `menu`, in coordinates taken from the same
/// layout the menu draws with. The menu selects on the messages its own window
/// procedure receives, so posting them is enough — the user's mouse is not
/// moved and nothing is injected into the desktop.
unsafe fn click_menu_row(menu: HWND, item: usize, entries: &[Entry]) -> Result<()> {
    let mut window = RECT::default();
    GetWindowRect(menu, &mut window)?;
    let reference = popup_menu::render(entries, 1.0, None)?;
    let row = reference
        .layout
        .rows
        .iter()
        .find(|row| row.entry == item)
        .ok_or_else(|| Error::new(E_INVALIDARG, "the menu has no such row"))?;
    // The rows are laid out in logical pixels, so the window the menu really
    // opened at gives the scale (the monitor's) for every measurement.
    let scale = (window.bottom - window.top) as f32 / reference.layout.height as f32;
    let x = ((row.band.x as f32 + row.band.width as f32 / 2.0) * scale).round() as i32;
    let y = ((row.band.y as f32 + row.band.height as f32 / 2.0) * scale).round() as i32;
    let point = LPARAM((((x & 0xffff) | ((y & 0xffff) << 16)) as i32) as isize);
    PostMessageW(menu, WM_MOUSEMOVE, WPARAM(0), point)?;
    PostMessageW(menu, WM_LBUTTONDOWN, WPARAM(1), point)?;
    PostMessageW(menu, WM_LBUTTONUP, WPARAM(0), point)?;
    Ok(())
}

/// Picks one row of the pin menu, the way a user picks it.
///
/// It runs on its own thread because the caller is inside the menu's modal loop
/// (the menu runs it inside `SendMessage`). If the menu never appears, the
/// escape key is posted to the control window, which that loop reads, so a
/// missing menu fails the smoke instead of hanging it.
fn click_pin_menu(
    pin_address: usize,
    control_address: usize,
    item: usize,
    entries: Vec<Entry>,
) -> std::result::Result<(), String> {
    let pin = HWND(pin_address as *mut std::ffi::c_void);
    let control = HWND(control_address as *mut std::ffi::c_void);
    let escape = |control: HWND| unsafe {
        let _ = PostMessageW(control, WM_KEYDOWN, WPARAM(VK_ESCAPE.0 as usize), LPARAM(0));
    };
    for _ in 0..600 {
        unsafe {
            // The pin can be gone (a close command arrived); nothing left to pick.
            if !IsWindow(pin).as_bool() {
                return Ok(());
            }
            if let Some(menu) = find_popup_menu() {
                if let Err(error) = click_menu_row(menu, item, &entries) {
                    escape(control);
                    return Err(error.to_string());
                }
                return Ok(());
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    escape(control);
    Err("the pin menu did not open".into())
}

unsafe extern "system" fn pin_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Pin;
    if !ptr.is_null() {
        let pin = &mut *ptr;
        let action = (|| -> std::result::Result<Option<LRESULT>, Box<dyn std::error::Error>> {
            match msg {
                WM_PAINT => {
                    let _ = ValidateRect(hwnd, None);
                    return Ok(Some(LRESULT(0)));
                }
                WM_ERASEBKGND => return Ok(Some(LRESULT(1))),
                WM_MOUSEACTIVATE => return Ok(Some(LRESULT(MA_NOACTIVATE as isize))),
                WM_NCHITTEST => return Ok(Some(LRESULT(HTCAPTION as isize))),
                // Clicking a pin makes it the selected one: the host clears every
                // other pin, so exactly one frame is on screen and the annotation
                // shortcut knows which pin it opens. The click is reported to the
                // host rather than applied here because clearing the other pins
                // needs all of them. The window never takes focus, so this click
                // cannot be the same gesture as typing into another application.
                WM_NCLBUTTONDOWN => {
                    post_selected(pin, GetMessageTime() as u32)?;
                    return Ok(None);
                }
                WM_NCLBUTTONDBLCLK => {
                    close_pin(pin)?;
                    return Ok(Some(LRESULT(0)));
                }
                // A middle click restores the original size, the same command as
                // the context menu's reset entry, run through `zoom` so both
                // paths stay one implementation. The whole window hit-tests as
                // the caption, so a real middle click arrives in the
                // non-client form; the client form is answered as well so the
                // message cannot fall through to a default gesture. The two
                // button-down forms are swallowed: the reset is drawn on the
                // release only, so one click redraws the pin once, and a click
                // where it already has its original size redraws nothing.
                WM_NCMBUTTONDOWN | WM_MBUTTONDOWN | WM_NCMBUTTONDBLCLK | WM_MBUTTONDBLCLK => {
                    return Ok(Some(LRESULT(0)));
                }
                WM_NCMBUTTONUP | WM_MBUTTONUP => {
                    if pin.scale != 1.0 {
                        pin.zoom(1.0)?;
                    }
                    return Ok(Some(LRESULT(0)));
                }
                WM_MOUSEWHEEL => {
                    let delta = ((w.0 >> 16) as u16 as i16) as f32 / 120.0;
                    if GetKeyState(VK_CONTROL.0 as i32) < 0 {
                        pin.opacity = (pin.opacity + delta * 0.05).clamp(0.1, 1.0);
                        if let Some(surface) = &pin.surface {
                            surface.opacity(pin.opacity)?;
                        } else {
                            pin.update_layered()?;
                        }
                    } else {
                        pin.zoom(pin.scale * 1.1f32.powf(delta))?;
                    }
                    return Ok(Some(LRESULT(0)));
                }
                // Every pixel of the pin hit-tests as the caption so the window
                // stays draggable, which means a right click arrives as a
                // non-client message. `DefWindowProc` answers that with the
                // window's system menu — a popup window has none — and never
                // sends `WM_CONTEXTMENU`, so the menu has to be opened here.
                WM_NCRBUTTONDOWN => return Ok(Some(LRESULT(0))),
                WM_NCRBUTTONUP | WM_CONTEXTMENU => {
                    show_pin_menu(pin)?;
                    return Ok(Some(LRESULT(0)));
                }
                WM_SHOWWINDOW => {
                    if w.0 == 0 {
                        pin.surface = None;
                    } else if pin.surface.is_none() {
                        pin.restore()?;
                    }
                }
                WM_CLOSE => {
                    pin.surface = None;
                    pin.closed = true;
                    pin.selected = false;
                    let control = pin.control;
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    DestroyWindow(hwnd)?;
                    PostMessageW(control, CLOSED, WPARAM(0), LPARAM(0))?;
                    return Ok(Some(LRESULT(0)));
                }
                WM_DESTROY => {
                    pin.surface = None;
                    pin.closed = true;
                    pin.selected = false;
                    PostMessageW(pin.control, CLOSED, WPARAM(0), LPARAM(0))?;
                    return Ok(Some(LRESULT(0)));
                }
                _ => {}
            }
            Ok(None)
        })();
        match action {
            Ok(Some(result)) => return result,
            Err(e) => {
                snaplight_platform::native::show_error(&e.to_string());
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, w, l)
}
struct Host {
    gpu: Option<Gpu>,
    // HWND user data stores these addresses; vector growth must not move a Pin.
    #[allow(clippy::vec_box)]
    pins: Vec<Box<Pin>>,
    queue: Arc<Mutex<Vec<PinCommand>>>,
    /// Shortcut hint shown by every pin's context menu.
    restore_hotkey: String,
    /// Held shortcut that temporarily restores interaction for click-through
    /// pins, already canonical; the poll is disabled while it is empty.
    through_hotkey: String,
    /// Whether the held shortcut was down at the previous poll.
    held_down: bool,
    /// Window timer watching the held shortcut, armed only while a pin is
    /// click-through.
    held_timer: Option<usize>,
    /// Shadow metrics a new pin starts from, in logical pixels at 96 DPI. The
    /// settings window refreshes them with [`Host::reload`].
    shadow: shadow::Settings,
    /// Whether a new pin draws a shadow by default.
    shadow_default: bool,
    /// Whether the raw mouse sink is registered. It is only needed while a pin is
    /// selected — that sink is what sees a press another window consumed — so it
    /// is registered and dropped together with the selection, which keeps an idle
    /// pin process out of the raw input path.
    mouse_sink: bool,
    /// Message time (`GetMessageTime`) of the selection input that is in force.
    /// Raw mouse presses, the annotation shortcut, the keyboard cancel and
    /// selection signals all carry the time of the event they belong to, and a
    /// later one of them replaces it; anything older than this describes an
    /// event that happened before the current selection: it must neither drop
    /// the selection nor put an earlier pin back. Win32 message times wrap, so
    /// they are compared with [`predates_selection`] (and [`later_than`]), never
    /// with `<`.
    ///
    /// `None` until the first selectable input records a time. A bare `0` would
    /// read as a real time, so the first click of a machine that has been up
    /// longer than half the counter's range (`GetMessageTime` counts from boot)
    /// would be judged older than it and rejected.
    selected_time: Option<u32>,
    /// Smoke only: run the editor's scripted annotation session instead of an
    /// interactive one, so the shortcut's own path can be checked without a user.
    edit_demo: bool,
}
impl Host {
    unsafe fn open(
        &mut self,
        control: HWND,
        path: PathBuf,
        document: PathBuf,
        screen: ScreenPoint,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let doc: Document = serde_json::from_slice(&std::fs::read(document)?)?;
        if doc.version != 1 {
            return Err("unsupported pin document version".into());
        }
        // Only the size is needed to create the window and the shadow margin;
        // the pixels are read again when the pin presents itself, so a pin that
        // is never shown does not hold a copy of its capture.
        let (image, _) = read_pixels(&path)?;
        let instance = GetModuleHandleW(None)?;
        // The pin opens where the region was captured, so pinning does not move
        // the image. The only adjustment is the width clamp for regions wider
        // than the primary display, which cannot be shown unscaled.
        let width = image.width.min(GetSystemMetrics(SM_CXSCREEN) as u32);
        let scale = width as f32 / image.width as f32;
        let window_backend = if self.gpu.is_some() {
            WS_EX_NOREDIRECTIONBITMAP
        } else {
            WS_EX_LAYERED
        };
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | window_backend,
            w!("Snaplight.Pin"),
            w!("Snaplight Pin"),
            WS_POPUP,
            screen.x,
            screen.y,
            width as i32,
            (image.height as f32 * scale) as i32,
            None,
            None,
            instance,
            None,
        )?;
        let mut pin = Box::new(Pin {
            hwnd,
            control,
            gpu: self.gpu.clone(),
            surface: None,
            path,
            image,
            closed: false,
            through: false,
            temporary: false,
            selected: false,
            editing: false,
            scale,
            opacity: 1.0,
            shadow: Shadow {
                settings: self.shadow,
                // The shadow follows the display the pin opens on, so it reads
                // the same size as the rest of the interface there.
                dpi: window_dpi_scale(hwnd),
                default_on: self.shadow_default,
                choice: None,
            },
            restore_hotkey: self.restore_hotkey.clone(),
        });
        // The window was created at the captured position with the image's own
        // size, so it starts with no shadow margin: growing it here is what
        // leaves the image on the screen pixels it was captured from.
        let rendering = pin.place(0, SWP_NOACTIVATE).and_then(|()| pin.restore());
        if let Err(error) = rendering {
            pin.closed = true;
            let _ = DestroyWindow(hwnd);
            return Err(error);
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&mut *pin as *mut Pin) as isize);
        self.pins.push(pin);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        Ok(())
    }
    /// Re-reads the configuration and applies what a pin on screen can still
    /// change: the shadow and its metrics. The settings window sends this after
    /// a save, because nothing else tells this process that the file changed.
    ///
    /// The per-pin choice is left alone on purpose: a pin whose shadow the user
    /// turned off must not get it back because the metrics were edited. Only a
    /// pin that still follows the configured default follows it here.
    unsafe fn reload(&mut self, hwnd: HWND) {
        let config = match snaplight_platform::load_config() {
            Ok(config) => config,
            // An unreadable configuration leaves the pins exactly as they are
            // instead of guessing.
            Err(error) => {
                eprintln!("pin settings reload failed ({error}); keeping the current settings");
                return;
            }
        };
        // Re-register the restore shortcut in place. If the new combination is
        // unavailable, register_restore_hotkey falls back to the default; when
        // even that fails, put the previous one back.
        let previous_restore = self.restore_hotkey.clone();
        let _ = UnregisterHotKey(hwnd, 2);
        match register_restore_hotkey(hwnd, &config) {
            Ok(hotkey) => {
                self.restore_hotkey = hotkey.clone();
                for pin in &mut self.pins {
                    pin.restore_hotkey = hotkey.clone();
                }
            }
            Err(error) => {
                eprintln!(
                    "restore hotkey {} unavailable on reload ({error}); restoring {previous_restore}",
                    config.restore_hotkey
                );
                if let Ok(hotkey) = parse_hotkey(&previous_restore) {
                    let (modifiers, key) = snaplight_platform::native::hotkey_registration(&hotkey);
                    let _ = RegisterHotKey(hwnd, 2, modifiers, key);
                }
            }
        }
        // The held shortcut is polled, so storing the new value is enough; an
        // unusable value disables the poll instead of stranding the process.
        self.through_hotkey = match parse_held_shortcut(&config.through_hotkey) {
            Ok(Some(shortcut)) => shortcut.canonical(),
            Ok(None) | Err(_) => String::new(),
        };
        self.shadow = shadow::Settings::from_config(&config);
        self.shadow_default = config.pin_shadow;
        for pin in &mut self.pins {
            if pin.closed {
                continue;
            }
            let before = (pin.shadow.enabled(), pin.shadow.params());
            let previous = pin.margin();
            pin.shadow.settings = self.shadow;
            pin.shadow.default_on = self.shadow_default;
            // Nothing to redraw when both the state and the metrics are what
            // they were; a repaint would only flicker.
            if (pin.shadow.enabled(), pin.shadow.params()) == before {
                continue;
            }
            if let Err(error) = pin.refresh_shadow(previous) {
                snaplight_platform::native::show_error(&error.to_string());
            }
        }
    }
    unsafe fn close_all(&mut self) {
        for pin in &mut self.pins {
            if !pin.closed {
                pin.surface = None;
                pin.closed = true;
                pin.selected = false;
                SetWindowLongPtrW(pin.hwnd, GWLP_USERDATA, 0);
                let _ = DestroyWindow(pin.hwnd);
            }
        }
    }
    /// Makes one pin the only selected pin, or clears the selection when
    /// `address` is `None`.
    ///
    /// The host owns this because exclusivity spans every pin: the frame has to
    /// mark exactly one of them, and only the pin the user clicked last may answer
    /// the annotation shortcut. `address` is the address of a boxed pin, which is
    /// stable for the pin's whole lifetime, so a pin that was closed in the
    /// meantime simply matches nothing.
    unsafe fn set_selection(&mut self, control: HWND, address: Option<usize>) {
        for pin in &mut self.pins {
            if pin.closed {
                continue;
            }
            // Somebody the pointer cannot click must never hold the selection:
            // a click-through pin passes the press on, a pin whose annotation
            // session is running is not a target the shortcut may pick again,
            // and a closed one is gone. An `address` matching none of these —
            // including a stale signal from a pin that closed or became
            // click-through in the meantime — therefore selects nothing, rather
            // than a pin that could not be clicked away in the first place.
            let wanted = address.is_some_and(|address| pin_address(pin) == address)
                && !pin.editing
                && !pin.is_click_through();
            if let Err(error) = pin.set_selected(wanted) {
                snaplight_platform::native::show_error(&error.to_string());
            }
        }
        self.update_mouse_sink(control);
    }
    /// True while some pin is selected, which is exactly when the raw mouse sink
    /// is needed.
    fn any_pin_selected(&self) -> bool {
        self.pins.iter().any(|pin| pin.selected && !pin.closed)
    }
    /// Registers the raw mouse sink while a pin is selected and drops it again
    /// otherwise.
    ///
    /// The sink is what lets a press that another window consumed be seen here at
    /// all, and a pin process with nothing selected has no use for those presses,
    /// so it stays out of the raw input path entirely. The sink never swallows
    /// anything: it is registered for background delivery, not for capture, so the
    /// application the click belongs to still receives it.
    unsafe fn update_mouse_sink(&mut self, control: HWND) {
        let wanted = self.any_pin_selected();
        if wanted == self.mouse_sink {
            return;
        }
        match register_mouse_sink(control, wanted) {
            Ok(()) => self.mouse_sink = wanted,
            // Without the sink a press elsewhere is invisible, so a selection
            // made now could never be dropped by clicking. Keeping it would
            // leave a frame the user cannot clear, so the selection is given up
            // and the reason reported; the next selection tries to register
            // again.
            Err(error) if wanted => {
                self.mouse_sink = false;
                for pin in &mut self.pins {
                    if pin.selected && !pin.closed {
                        let _ = pin.set_selected(false);
                    }
                }
                snaplight_platform::native::show_error(&format!(
                    "选中贴图需要鼠标原始输入，但注册失败（{error}）；这次选中已取消"
                ));
            }
            // Dropping the sink failed with nothing selected: the stale
            // registration is harmless, and a press elsewhere stays unseen until
            // the next selection tries to register again.
            Err(error) => eprintln!("pin selection mouse sink unavailable ({error})"),
        }
    }
    /// Drops the selection when a left press landed somewhere that is not a pin
    /// the pointer can click: the desktop, another application, or a click-through
    /// pin, whose press is passed on to what is beneath it.
    ///
    /// The press is judged from the position the system recorded for that
    /// message (`GetMessagePos`), not from where the pointer happens to be when
    /// this runs: a queued press has to be read where it was posted, or the
    /// pointer having moved onto a pin in the meantime would keep a selection
    /// that click never made. The caller drops presses older than the selection
    /// before they reach here, which is what keeps the raw event of the very
    /// click that selected a pin from undoing it.
    unsafe fn press_outside(&mut self, control: HWND, point: POINT) {
        if self.point_over_pin(point) {
            return;
        }
        self.set_selection(control, None);
    }
    /// True when `point` is inside the window of a pin the pointer can click. A
    /// click-through pin deliberately does not count: its press goes to the window
    /// beneath, so it behaves exactly like a press outside every pin.
    ///
    /// The window under the point is asked for with `WindowFromPoint` and
    /// compared against the pins' own handles; the pins' rectangles are
    /// deliberately not used. Another window covering a pin is the window the
    /// press lands on, so clicking there must drop the selection exactly as a
    /// click on the desktop does — a rectangle test would keep it and read the
    /// click as if it had hit the pin.
    unsafe fn point_over_pin(&self, point: POINT) -> bool {
        let window = WindowFromPoint(point);
        if window.0.is_null() {
            return false;
        }
        self.pins
            .iter()
            .any(|pin| !pin.closed && !pin.is_click_through() && pin.hwnd == window)
    }
    /// Applies one raw input event: the annotation shortcut, the keyboard cancel,
    /// and the press that drops a selection made somewhere else.
    ///
    /// Split from the window procedure so the mapping from an event to what it
    /// does lives in one place and can be driven directly. The sinks carry input
    /// meant for the whole desktop, so everything that is not one of those three
    /// events — including every mouse move, which is the bulk of them — is dropped
    /// here without touching a pin.
    unsafe fn raw_event(&mut self, control: HWND, point: POINT, time: u32, input: &RAWINPUT) {
        if input.header.dwType == RIM_TYPEKEYBOARD.0 {
            let keyboard = input.data.keyboard;
            if key_pressed(keyboard.VKey, keyboard.Flags, VK_SPACE.0) {
                if self.predates(time) {
                    return;
                }
                self.selected_time = Some(time);
                if let Err(error) = self.open_selected(control) {
                    snaplight_platform::native::show_error(&error.to_string());
                }
            } else if key_pressed(keyboard.VKey, keyboard.Flags, VK_ESCAPE.0) {
                if self.predates(time) {
                    return;
                }
                self.selected_time = Some(time);
                self.set_selection(control, None);
            }
        } else if input.header.dwType == RIM_TYPEMOUSE.0
            && left_button_down(input.data.mouse.Anonymous.Anonymous.usButtonFlags)
        {
            // A press older than the selection in force belongs to a click that
            // happened before it — the pin's own click posts its raw event before
            // the selection signal, and `SELECTED` is applied later — so it must
            // not drop that selection. Equal times are the same click and are
            // judged on their position, which is where the press lands.
            if self.predates(time) {
                return;
            }
            // The press records its time even when it clears nothing, so a
            // selection signal posted behind it cannot revive what it dropped.
            self.selected_time = Some(time);
            self.press_outside(control, point);
        }
    }
    /// True when `time` predates the selection time in force. Every raw key or
    /// press the host acts on goes through this before it changes anything, so a
    /// late event that belongs to a click the selection already moved past
    /// neither drops the selection nor opens the wrong pin.
    fn predates(&self, time: u32) -> bool {
        predates_selection(self.selected_time, time)
    }
    /// True while the configured held shortcut is down.
    fn through_held(&self) -> bool {
        if self.through_hotkey.is_empty() {
            return false;
        }
        match parse_held_shortcut(&self.through_hotkey) {
            Ok(Some(shortcut)) => snaplight_platform::native::held_shortcut_down(&shortcut),
            // A shortcut that no longer parses was already rejected at load time.
            _ => false,
        }
    }
    /// Whether any pin is *meant* to be click-through, whether or not the held
    /// shortcut is lifting one at this instant. The poll follows this, not the
    /// current window state: if a lift counted as "no pin wants click-through",
    /// the next tick would undo it and the two states would trade places every
    /// 16 ms — which is what the user saw as the pin repainting constantly.
    fn any_pin_wants_click_through(&self) -> bool {
        self.pins.iter().any(|pin| pin.through && !pin.closed)
    }
    /// Arms the held-shortcut timer exactly while a pin is click-through. This
    /// keeps the poll event driven: with no click-through pin the process waits
    /// in the message loop and wakes up for nothing.
    unsafe fn update_held_poll(&mut self, control: HWND) {
        if self.any_pin_wants_click_through() {
            if self.held_timer.is_none() {
                let timer = SetTimer(control, HELD_TIMER, HELD_POLL_MS, None);
                self.held_timer = (timer != 0).then_some(timer);
            }
        } else {
            if let Some(timer) = self.held_timer.take() {
                let _ = KillTimer(control, timer);
            }
            // A pin that the held shortcut left open must not outlive the poll
            // that watches for the release.
            self.end_temporary();
        }
    }
    /// Applies one poll of the held shortcut, in either direction. The release
    /// path is not left to the next tick, so letting go of the key restores
    /// click-through immediately.
    unsafe fn poll_held(&mut self) {
        let down = self.through_held();
        self.poll_held_with(down);
    }
    /// One poll with the key state given explicitly, so the smoke can drive the
    /// poll itself instead of only the state change it produces.
    unsafe fn poll_held_with(&mut self, key_down: bool) {
        let held = self.any_pin_wants_click_through() && key_down;
        self.apply_held(held);
    }
    /// Applies a held-shortcut state. Split from the poll so the smoke can drive
    /// both directions without synthesising keyboard input.
    unsafe fn apply_held(&mut self, held: bool) {
        if held == self.held_down {
            return;
        }
        self.held_down = held;
        if held {
            for pin in &mut self.pins {
                if pin.through && !pin.temporary && !pin.closed {
                    // Keep the intent, only lift it: the pin still wants to be
                    // click-through once the key is released.
                    let _ = pin.set_through(true, true);
                }
            }
        } else {
            self.end_temporary();
        }
    }
    /// Restores click-through on every pin the held shortcut is holding open.
    unsafe fn end_temporary(&mut self) {
        self.held_down = false;
        for pin in &mut self.pins {
            if pin.temporary && !pin.closed {
                let _ = pin.set_through(true, false);
            }
        }
    }
    /// The registered restore shortcut: every click-through pin becomes
    /// interactive again, including one the held shortcut is holding open at
    /// this moment. The held state is cleared first so the next poll cannot undo
    /// the restore while the key is still down.
    unsafe fn restore_interaction(&mut self, control: HWND) {
        self.held_down = false;
        for pin in &mut self.pins {
            if pin.closed {
                continue;
            }
            // The window service clears the intent and the lift together, which
            // is what "restore interaction for good" means here.
            let _ = NativeWindows.set_click_through(&pin.hwnd, false);
        }
        self.update_held_poll(control);
    }
    /// Opens the annotation session of the selected pin, which is what the
    /// annotation shortcut does. Returns whether a session started.
    ///
    /// The selection deliberately outlives the pointer leaving the pin, so this
    /// does not look at where the pointer is: the user picked the pin with a
    /// click, and the only ways back are picking another pin, pressing elsewhere,
    /// Escape, or the pin becoming unusable.
    unsafe fn open_selected(
        &mut self,
        control: HWND,
    ) -> std::result::Result<bool, Box<dyn std::error::Error>> {
        let Some(index) = self
            .pins
            .iter()
            .position(|pin| pin.selected && !pin.closed && !pin.editing && !pin.is_click_through())
        else {
            return Ok(false);
        };
        open_annotation(&mut self.pins[index], self.edit_demo)?;
        // A pin whose session just started is no longer a valid selection, so the
        // sink that watches for a press elsewhere follows the selection here too.
        self.update_mouse_sink(control);
        Ok(true)
    }
    /// Reloads the picture of the pin whose annotation session just ended.
    unsafe fn finish_edit(
        &mut self,
        address: usize,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let Some(index) = self.pins.iter().position(|pin| pin_address(pin) == address) else {
            // The pin was closed while the session ran; the annotated picture it
            // wrote back stays on disk.
            return Ok(());
        };
        let pin = &mut self.pins[index];
        pin.editing = false;
        if pin.closed {
            return Ok(());
        }
        pin.refresh()
    }
}
/// The raw event behind one `WM_INPUT` message, or `None` when it cannot be read.
///
/// The device type comes from the `RAWINPUTHEADER` this fills in, never from the
/// message's wparam: `WM_INPUT`'s wparam only says whether the message was
/// delivered in the foreground or through the input sink, and those two values
/// overlap the device types. The handle to hand to `GetRawInputData` is the
/// message's lparam.
unsafe fn raw_input(l: LPARAM) -> Option<RAWINPUT> {
    let mut input = RAWINPUT::default();
    let mut size = std::mem::size_of::<RAWINPUT>() as u32;
    let read = GetRawInputData(
        HRAWINPUT(l.0 as *mut core::ffi::c_void),
        RID_INPUT,
        Some((&mut input as *mut RAWINPUT).cast()),
        &mut size,
        std::mem::size_of::<RAWINPUTHEADER>() as u32,
    );
    (read != u32::MAX).then_some(input)
}
/// True when this raw keyboard event is `wanted` being pressed rather than
/// released. The two keys the pin reads are both one-shot on the press: the
/// annotation shortcut opens one session, and the cancel drops the selection
/// once, so the release that follows must do nothing.
fn key_pressed(virtual_key: u16, flags: u16, wanted: u16) -> bool {
    virtual_key == wanted && flags & RI_KEY_BREAK as u16 == 0
}
/// True when this raw mouse event is the left button going down: the press that
/// decides whether a selection survives. Moves and releases carry no such flag,
/// and they are the bulk of what the sink delivers, so they are dropped by this
/// test before anything else looks at them.
fn left_button_down(flags: u16) -> bool {
    flags & RI_MOUSE_LEFT_BUTTON_DOWN as u16 != 0
}
/// True when Win32 message time `a` is later than `b`.
///
/// `GetMessageTime` counts milliseconds in a 32-bit counter that wraps about
/// every 49.7 days, so a plain `<` would read a wrapped value as ancient and
/// treat a current event as stale (or the other way round right after the wrap).
/// Comparing the wrapping difference as a signed value reads the shorter way
/// round the circle, which is what "later" means for a counter like this. Equal
/// times are not later in either direction, which is how the raw event and the
/// selection signal of one click compare.
fn later_than(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}
/// True when message time `time` predates the last time the host acted on a
/// selection, so the input it describes happened before the selection in force
/// and must neither drop it nor put an earlier pin back.
///
/// Nothing is recorded before the first selectable input, so that input is never
/// stale — which is what keeps a machine already up past half the counter's
/// range from rejecting the first click. The counter wraps, so the comparison is
/// [`later_than`], never `<`.
fn predates_selection(recorded: Option<u32>, time: u32) -> bool {
    recorded.is_some_and(|recorded| later_than(recorded, time))
}
/// Registers the raw keyboard sink that makes the annotation shortcut possible
/// without focus: only this process's control window receives the keys, and an
/// idle process does nothing with them.
unsafe fn register_input_sink(hwnd: HWND) -> Result<()> {
    let devices = [RAWINPUTDEVICE {
        usUsagePage: 0x01,
        // Generic desktop controls / keyboard.
        usUsage: 0x06,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: hwnd,
    }];
    RegisterRawInputDevices(&devices, std::mem::size_of::<RAWINPUTDEVICE>() as u32)
}
/// Registers or drops the raw mouse sink, which is what sees a press that another
/// window consumed.
///
/// It is registered for background delivery (`RIDEV_INPUTSINK`), never for
/// capture, so it does not take the click away from the application it belongs to
/// — it only tells this process about it. Dropping it needs no target window: the
/// system removes the registration for the null one.
unsafe fn register_mouse_sink(hwnd: HWND, listen: bool) -> Result<()> {
    let devices = [RAWINPUTDEVICE {
        usUsagePage: 0x01,
        // Generic desktop controls / mouse.
        usUsage: 0x02,
        dwFlags: if listen {
            RIDEV_INPUTSINK
        } else {
            RIDEV_REMOVE
        },
        hwndTarget: if listen { hwnd } else { HWND::default() },
    }];
    RegisterRawInputDevices(&devices, std::mem::size_of::<RAWINPUTDEVICE>() as u32)
}
unsafe extern "system" fn control_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Host;
    if !ptr.is_null() {
        let host = &mut *ptr;
        match msg {
            INCOMING => {
                let commands = std::mem::take(&mut *host.queue.lock().unwrap());
                for command in commands {
                    match command {
                        PinCommand::Open {
                            image,
                            document,
                            screen,
                        } => {
                            if let Err(e) = host.open(hwnd, image, document, screen) {
                                snaplight_platform::native::show_error(&e.to_string());
                            }
                        }
                        PinCommand::Reload => host.reload(hwnd),
                        PinCommand::CloseAll | PinCommand::Shutdown => {
                            host.close_all();
                            PostQuitMessage(0);
                        }
                    }
                }
                // A new pin may be the first click-through one, and destroying a
                // pin may have left none; both decide whether to poll.
                host.update_held_poll(hwnd);
                return LRESULT(0);
            }
            CLOSED => {
                // Closed pins stay allocated. A pin's window procedure can be on
                // the stack while it closes — the context menu runs a nested
                // modal loop, and `CloseAll` can arrive during it — so dropping
                // its state here would leave that frame with a dangling pointer.
                // The process exits once every pin is closed.
                if host.pins.iter().all(|pin| pin.closed) {
                    PostQuitMessage(0);
                } else {
                    host.update_held_poll(hwnd);
                    // The closed pin may have been the selected one.
                    host.update_mouse_sink(hwnd);
                }
                return LRESULT(0);
            }
            SELECTED => {
                // A pin was clicked; the selection moves to it and nothing else
                // stays selected. The signal carries the pin's address in the
                // wparam because the pin cannot clear its siblings itself, and the
                // click's message time in the lparam so a signal that arrives
                // after a newer selection cannot put the older pin back.
                let time = l.0 as u32;
                // Equal times are the same click — the press that started it and
                // this signal — so only a genuinely older signal is dropped.
                if !predates_selection(host.selected_time, time) {
                    host.selected_time = Some(time);
                    host.set_selection(hwnd, Some(w.0));
                }
                return LRESULT(0);
            }
            WM_INPUT => {
                // Pins never take focus, so the annotation shortcut, the keyboard
                // cancel and the press that lands elsewhere all arrive as raw
                // input: a key or mouse message would be delivered to whatever
                // application is in front instead. The click's own screen
                // position and time come from the message, not from the pointer
                // now: a queued press has to be read where it was posted, and the
                // time orders it against the selection it might otherwise drop.
                // `GetMessagePos` packs the two signed 16-bit screen coordinates
                // into one word, so each half is sign-extended again — a monitor
                // left of or above the primary one has negative coordinates.
                let position = GetMessagePos();
                let point = POINT {
                    x: (position as u16 as i16) as i32,
                    y: ((position >> 16) as u16 as i16) as i32,
                };
                let time = GetMessageTime() as u32;
                if let Some(input) = raw_input(l) {
                    host.raw_event(hwnd, point, time, &input);
                }
                return LRESULT(0);
            }
            EDIT_DONE => {
                // The annotation session's process has exited; the picture it
                // wrote back is on disk, so the pin reloads it. The pin's address
                // arrives as the message's wparam.
                if let Err(e) = host.finish_edit(w.0) {
                    snaplight_platform::native::show_error(&e.to_string());
                }
                return LRESULT(0);
            }
            WM_TIMER => {
                // The polling tick lasts only while a pin is click-through, so
                // it cannot spin in the background.
                host.poll_held();
                return LRESULT(0);
            }
            HELD_SYNC => {
                // A pin changed its click-through state from its own window
                // procedure; the poll follows the pins, not the caller. A pin that
                // went click-through is no longer a valid selection either.
                host.update_held_poll(hwnd);
                host.update_mouse_sink(hwnd);
                return LRESULT(0);
            }
            WM_HOTKEY => {
                // The registered shortcut is a second, explicit way back to
                // interaction, so it restores every click-through pin whether or
                // not the held shortcut is what opened it.
                host.restore_interaction(hwnd);
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, w, l)
}
/// Registers the configured pin-restore shortcut, falling back to the default
/// when another application already owns the configured combination. Returns the
/// canonical form of the shortcut that was actually registered, which is what the
/// context menu shows.
unsafe fn register_restore_hotkey(
    hwnd: HWND,
    config: &Config,
) -> std::result::Result<String, Box<dyn std::error::Error>> {
    let hotkey = parse_hotkey(&config.restore_hotkey)?;
    let (modifiers, key) = snaplight_platform::native::hotkey_registration(&hotkey);
    if let Err(error) = RegisterHotKey(hwnd, 2, modifiers, key) {
        let fallback = parse_hotkey(DEFAULT_RESTORE_HOTKEY).expect("default restore hotkey");
        let (modifiers, key) = snaplight_platform::native::hotkey_registration(&fallback);
        eprintln!(
            "restore hotkey {} unavailable ({error}); using {DEFAULT_RESTORE_HOTKEY}",
            config.restore_hotkey
        );
        RegisterHotKey(hwnd, 2, modifiers, key)?;
        return Ok(fallback.canonical());
    }
    Ok(hotkey.canonical())
}
/// `--confirm-demo [x y]` shows the close prompt at a fixed anchor and prints
/// what was answered, without needing a pin. Like the tray menu's demo mode it
/// exists so the prompt can be looked at (and screenshotted) on a real desktop.
unsafe fn confirm_demo() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut rest = std::env::args()
        .skip_while(|a| a != "--confirm-demo")
        .skip(1);
    let anchor = match (rest.next(), rest.next()) {
        (Some(x), Some(y)) => x.parse::<i32>().ok().zip(y.parse::<i32>().ok()),
        _ => None,
    };
    let anchor = match anchor {
        Some(anchor) => anchor,
        None => {
            let mut point = POINT::default();
            GetCursorPos(&mut point)?;
            (point.x, point.y)
        }
    };
    let answer = confirm::ask(HWND::default(), anchor, &close_options())?;
    println!(
        "confirm demo: accepted={} remember={}",
        answer.accepted, answer.remember
    );
    Ok(())
}

pub fn run() -> std::result::Result<(), Box<dyn std::error::Error>> {
    snaplight_platform::native::dpi_aware();
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        if std::env::args().any(|a| a == "--confirm-demo") {
            return confirm_demo();
        }
        let config = snaplight_platform::load_config()?;
        let use_gpu =
            config.gpu_pins || std::env::args().any(|a| a == "--composition" || a == "--smoke");
        let gpu = if use_gpu { Some(Gpu::new(None)?) } else { None };
        let instance = GetModuleHandleW(None)?;
        // Pin windows are frameless, but the class icon still shows up in
        // Alt-Tab previews and any dialog the process opens. `WNDCLASSW` has no
        // separate small icon; the system derives it from this one.
        let class_icon = snaplight_platform::native::app_icon(false).unwrap_or_default();
        for (name, proc) in [
            (
                w!("Snaplight.Pin"),
                pin_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
            ),
            (w!("Snaplight.PinHost"), control_proc),
        ] {
            let wc = WNDCLASSW {
                style: CS_DBLCLKS,
                hInstance: instance.into(),
                lpszClassName: name,
                lpfnWndProc: Some(proc),
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                hIcon: class_icon,
                ..Default::default()
            };
            if RegisterClassW(&wc) == 0 {
                return Err(Error::from_win32().into());
            }
        }
        let control = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("Snaplight.PinHost"),
            w!("Snaplight Pin Host"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            instance,
            None,
        )?;
        let queue = Arc::new(Mutex::new(vec![]));
        let restore_hotkey = register_restore_hotkey(control, &config)?;
        // The held shortcut is polled instead of registered: it must fire while
        // the key is down, and `RegisterHotKey` only reports a press. An
        // unusable value disables the poll rather than failing the process,
        // which would strand pins that are already on screen.
        let through_hotkey = match parse_held_shortcut(&config.through_hotkey) {
            Ok(Some(shortcut)) => shortcut.canonical(),
            Ok(None) | Err(_) => {
                eprintln!(
                    "held interaction shortcut {:?} unusable; click-through pins stay pass-through",
                    config.through_hotkey
                );
                String::new()
            }
        };
        let mut host = Box::new(Host {
            gpu,
            pins: vec![],
            queue: queue.clone(),
            restore_hotkey,
            through_hotkey,
            held_down: false,
            held_timer: None,
            shadow: shadow::Settings::from_config(&config),
            shadow_default: config.pin_shadow,
            // Nothing is selected yet, so the raw mouse sink is not registered.
            mouse_sink: false,
            selected_time: None,
            edit_demo: false,
        });
        SetWindowLongPtrW(control, GWLP_USERDATA, (&mut *host as *mut Host) as isize);
        // The annotation shortcut needs the keyboard while no pin has focus, which
        // raw input provides without a system-wide hook.
        if let Err(error) = register_input_sink(control) {
            eprintln!(
                "annotation shortcut unavailable ({error}); the context menu still annotates"
            );
        }
        let address = control.0 as usize;
        if std::env::args().any(|a| a == "--smoke" || a == "--smoke-low-memory") {
            let dir = snaplight_platform::data_dir();
            std::fs::create_dir_all(&dir)?;
            let image = ImageDescriptor {
                id: ImageId(99),
                width: 320,
                height: 200,
                stride: 1280,
                format: PixelFormat::Bgra8,
                alpha: AlphaMode::Opaque,
                color_space: ColorSpace::Srgb,
            };
            let mut bytes = vec![0; 320 * 200 * 4];
            for (index, p) in bytes.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                p.copy_from_slice(&[(index / 320) as u8, (index % 320) as u8, 180, 255]);
            }
            let path = dir.join("pin-smoke.bgra");
            snaplight_platform::write_pixels(&path, &image, &bytes)?;
            let doc = dir.join("pin-smoke.json");
            snaplight_platform::atomic_write(
                &doc,
                &serde_json::to_vec(&Document {
                    version: 1,
                    image: image.clone(),
                    selection: None,
                    annotations: vec![],
                })?,
            )?;
            // The pin shadow is presentation, so it is checked on the real
            // desktop: a known configuration, a known colour behind the pin and
            // the pixels the compositor actually puts on screen.
            let config_path = snaplight_platform::data_dir().join("config.json");
            let config_snapshot = std::fs::read(&config_path).ok();
            let mut smoke_config = snaplight_platform::load_config().unwrap_or_default();
            smoke_config.pin_shadow = true;
            smoke_config.pin_shadow_blur = DEFAULT_PIN_SHADOW_BLUR;
            smoke_config.pin_shadow_offset = DEFAULT_PIN_SHADOW_OFFSET;
            smoke_config.pin_shadow_opacity = DEFAULT_PIN_SHADOW_OPACITY;
            snaplight_platform::save_config(&smoke_config)?;
            host.reload(control);
            let magenta = COLORREF(0x00FF00FF);
            let background = backdrop(
                RECT {
                    left: 0,
                    top: 0,
                    right: 640,
                    bottom: 560,
                },
                magenta,
            )?;
            // The backdrop has to be up before anything is read from the screen;
            // its first frame arrives one composition pass after `ShowWindow`.
            require_pixel(
                600,
                550,
                "the smoke backdrop never reached the screen",
                |pixel| pixel.0 > 230 && pixel.2 > 230 && pixel.1 < 25,
            )?;
            // The pin must open exactly where the captured region was, not at the
            // pointer, so verify the capture against the requested position. The
            // window is bigger than the capture by the shadow margin.
            let requested = ScreenPoint { x: 60, y: 90 };
            host.open(control, path.clone(), doc.clone(), requested)?;
            let mut placed = RECT::default();
            GetWindowRect(host.pins[0].hwnd, &mut placed)?;
            let margin = host.pins[0].margin();
            if margin <= 0 {
                return Err(format!(
                    "a pin with the configured shadow reserved no margin ({margin})"
                )
                .into());
            }
            if (placed.left + margin, placed.top + margin) != (requested.x, requested.y) {
                return Err(format!(
                    "pin capture opened at ({}, {}) instead of ({}, {})",
                    placed.left + margin,
                    placed.top + margin,
                    requested.x,
                    requested.y
                )
                .into());
            }
            let size = (placed.right - placed.left, placed.bottom - placed.top);
            let expected_size = (
                image.width as i32 + 2 * margin,
                image.height as i32 + 2 * margin,
            );
            if size != expected_size {
                return Err(format!(
                    "pin window is {}x{} instead of {expected_size:?}",
                    size.0, size.1
                )
                .into());
            }
            // Just below the capture the backdrop has to be darkened. Geometry
            // alone would pass even if the shadow never reached the screen.
            let shadow_sample = (placed.left + margin + 160, placed.bottom - margin + 3);
            let shadowed = |pixel: (u8, u8, u8)| {
                pixel.0 > 60 && pixel.0 < 210 && pixel.2 > 60 && pixel.2 < 210 && pixel.1 < 40
            };
            require_pixel(
                shadow_sample.0,
                shadow_sample.1,
                "the shadow below the pin is not on screen",
                shadowed,
            )?;
            // Without a shadow the window is the capture again, on the same
            // pixels, and the backdrop shows through untouched. The smoke picks
            // the menu entry the user picks, on the real menu: a helper thread
            // presses the row while this thread sits inside the menu's modal
            // loop.
            let entries = menu_entries(&host.pins[0].restore_hotkey, true);
            let pin_window_address = host.pins[0].hwnd.0 as usize;
            let control_address = control.0 as usize;
            let worker = std::thread::spawn(move || {
                click_pin_menu(
                    pin_window_address,
                    control_address,
                    pin_item::SHADOW,
                    entries,
                )
            });
            SendMessageW(host.pins[0].hwnd, WM_NCRBUTTONUP, WPARAM(0), LPARAM(0));
            worker
                .join()
                .map_err(|_| "the menu click thread panicked")??;
            let mut bare = RECT::default();
            GetWindowRect(host.pins[0].hwnd, &mut bare)?;
            if (bare.left, bare.top) != (requested.x, requested.y)
                || (bare.right - bare.left, bare.bottom - bare.top)
                    != (image.width as i32, image.height as i32)
            {
                return Err(format!(
                    "without a shadow the pin is ({}, {}) {}x{} instead of the capture at ({}, {})",
                    bare.left,
                    bare.top,
                    bare.right - bare.left,
                    bare.bottom - bare.top,
                    requested.x,
                    requested.y
                )
                .into());
            }
            require_pixel(
                shadow_sample.0,
                shadow_sample.1,
                "the pin shadow is still on screen after it was turned off",
                |pixel| pixel.0 > 230 && pixel.2 > 230 && pixel.1 < 25,
            )?;
            // Turning it back on is what the other menu entry does, and it has
            // to restore the very window the pin had before.
            host.pins[0].set_shadow(true)?;
            let mut again = RECT::default();
            GetWindowRect(host.pins[0].hwnd, &mut again)?;
            if (again.left, again.top, again.right, again.bottom)
                != (placed.left, placed.top, placed.right, placed.bottom)
            {
                return Err(format!(
                    "turning the shadow back on gave ({}, {}) {}x{} instead of ({}, {}) {}x{}",
                    again.left,
                    again.top,
                    again.right - again.left,
                    again.bottom - again.top,
                    placed.left,
                    placed.top,
                    placed.right - placed.left,
                    placed.bottom - placed.top
                )
                .into());
            }
            require_pixel(
                shadow_sample.0,
                shadow_sample.1,
                "the shadow did not come back when it was turned on again",
                shadowed,
            )?;
            // A settings change has to reach a pin that is already open: the
            // window follows the new metrics and the capture stays put.
            smoke_config.pin_shadow_blur = 32;
            smoke_config.pin_shadow_offset = 0;
            smoke_config.pin_shadow_opacity = 100;
            snaplight_platform::save_config(&smoke_config)?;
            host.reload(control);
            let wide_margin = host.pins[0].margin();
            let mut wider = RECT::default();
            GetWindowRect(host.pins[0].hwnd, &mut wider)?;
            if wide_margin <= margin {
                return Err(format!(
                    "the reloaded shadow kept its old margin ({margin} instead of {wide_margin})"
                )
                .into());
            }
            if (wider.left + wide_margin, wider.top + wide_margin) != (requested.x, requested.y) {
                return Err(format!(
                    "reloading the shadow moved the capture to ({}, {})",
                    wider.left + wide_margin,
                    wider.top + wide_margin
                )
                .into());
            }
            // The reloaded metrics are opaque, so the same pixel is now black.
            require_pixel(
                shadow_sample.0,
                shadow_sample.1,
                "the reloaded shadow metrics did not reach the pin",
                |pixel| pixel.0 < 40 && pixel.2 < 40,
            )?;
            // The configured default only applies to pins that have no choice of
            // their own, so switching it off leaves this one alone.
            smoke_config.pin_shadow = false;
            snaplight_platform::save_config(&smoke_config)?;
            host.reload(control);
            if !host.pins[0].shadow.enabled() {
                return Err("reloading the settings cleared the pin's own shadow choice".into());
            }
            // One line per backend is enough to see what the checks above did
            // without digging into the result JSON.
            println!(
                "pin shadow: margin {margin}, menu row {} removed it, reload margin {wide_margin}",
                pin_item::SHADOW
            );
            let _ = DestroyWindow(background);
            host.pins[0].zoom(0.5)?;
            let mut zoomed = RECT::default();
            GetWindowRect(host.pins[0].hwnd, &mut zoomed)?;
            let zoomed_margin = host.pins[0].margin();
            if (zoomed.left + zoomed_margin, zoomed.top + zoomed_margin)
                != (requested.x, requested.y)
            {
                return Err(format!(
                    "scaling moved the capture to ({}, {})",
                    zoomed.left + zoomed_margin,
                    zoomed.top + zoomed_margin
                )
                .into());
            }
            // A middle click restores the original size — the same command as
            // the context menu's reset entry — and the smoke sends the real
            // button messages instead of calling `zoom`, so the handler is what
            // is under test. The whole window hit-tests as the caption, so the
            // system delivers the non-client form to a real click; the client
            // form has to be answered too.
            host.pins[0].opacity = 0.6;
            for (index, message) in [WM_NCMBUTTONUP, WM_MBUTTONUP].into_iter().enumerate() {
                // Start away from the original size, magnified once and shrunk
                // once, so each click really has a size to undo.
                host.pins[0].zoom(if index == 0 { 1.6 } else { 0.5 })?;
                SendMessageW(host.pins[0].hwnd, message, WPARAM(0), LPARAM(0));
                let mut reset = RECT::default();
                GetWindowRect(host.pins[0].hwnd, &mut reset)?;
                let reset_margin = host.pins[0].margin();
                let reset_size = (reset.right - reset.left, reset.bottom - reset.top);
                let want_size = (
                    image.width as i32 + 2 * reset_margin,
                    image.height as i32 + 2 * reset_margin,
                );
                if host.pins[0].scale != 1.0
                    || (reset.left + reset_margin, reset.top + reset_margin)
                        != (requested.x, requested.y)
                    || reset_size != want_size
                    || host.pins[0].opacity != 0.6
                {
                    return Err(format!(
                        "a middle click (message {message:#x}) left the pin at scale {} origin ({}, {}) size {reset_size:?} opacity {}, not the capture plus margin {want_size:?} at ({}, {})",
                        host.pins[0].scale,
                        reset.left + reset_margin,
                        reset.top + reset_margin,
                        host.pins[0].opacity,
                        requested.x,
                        requested.y
                    )
                    .into());
                }
            }
            // The reset is drawn on the release only: pressing the middle button
            // has to leave the size alone, so a click redraws the pin once.
            host.pins[0].zoom(2.0)?;
            SendMessageW(host.pins[0].hwnd, WM_NCMBUTTONDOWN, WPARAM(0), LPARAM(0));
            SendMessageW(host.pins[0].hwnd, WM_MBUTTONDOWN, WPARAM(0), LPARAM(0));
            if host.pins[0].scale != 2.0 {
                return Err(format!(
                    "pressing the middle button already resized the pin to {}",
                    host.pins[0].scale
                )
                .into());
            }
            // Clicking again where it already has its original size changes
            // nothing, and the capture stays where it is.
            SendMessageW(host.pins[0].hwnd, WM_NCMBUTTONUP, WPARAM(0), LPARAM(0));
            SendMessageW(host.pins[0].hwnd, WM_MBUTTONUP, WPARAM(0), LPARAM(0));
            let mut again = RECT::default();
            GetWindowRect(host.pins[0].hwnd, &mut again)?;
            let again_margin = host.pins[0].margin();
            let again_size = (again.right - again.left, again.bottom - again.top);
            if host.pins[0].scale != 1.0
                || (again.left + again_margin, again.top + again_margin)
                    != (requested.x, requested.y)
                || again_size
                    != (
                        image.width as i32 + 2 * again_margin,
                        image.height as i32 + 2 * again_margin,
                    )
            {
                return Err(format!(
                    "clicking the middle button again left the pin at scale {} origin ({}, {}) size {again_size:?}",
                    host.pins[0].scale,
                    again.left + again_margin,
                    again.top + again_margin
                )
                .into());
            }
            host.pins[0].opacity = 1.0;
            host.pins[0].toggle()?;
            host.pins[0].toggle()?;
            // Click-through is the state the held shortcut lifts, so drive that
            // state machine directly: the poll itself reads the real keyboard.
            // The last step goes through the same helper the context menu uses,
            // so the smoke covers the signal that arms the poll as well.
            toggle_through(&mut host.pins[0])?;
            let ex_style = GetWindowLongPtrW(host.pins[0].hwnd, GWL_EXSTYLE) as u32;
            if ex_style & WS_EX_TRANSPARENT.0 == 0 {
                return Err(format!(
                    "a click-through pin keeps the hit-testing style (ex style {ex_style:#x})"
                )
                .into());
            }
            // The style is only the mechanism; what the user needs is that the
            // mouse reaches the window underneath, so ask the system itself with
            // the same hit test the input stack uses.
            let mut through_rect = RECT::default();
            GetWindowRect(host.pins[0].hwnd, &mut through_rect)?;
            let center = POINT {
                x: (through_rect.left + through_rect.right) / 2,
                y: (through_rect.top + through_rect.bottom) / 2,
            };
            let hit = WindowFromPoint(center);
            if hit == host.pins[0].hwnd {
                return Err(format!(
                    "a click-through pin still receives the mouse at ({}, {})",
                    center.x, center.y
                )
                .into());
            }
            // Hold the shortcut through the poll itself, the way the timer
            // drives it, and require that state to stay put: when a lift counted
            // as "no pin wants click-through", the pin flipped between
            // interaction and click-through on every tick and repainted instead
            // of being usable while the key was down.
            host.poll_held_with(true);
            let held = (host.pins[0].through, host.pins[0].temporary, host.held_down);
            if held != (true, true, true) {
                return Err(format!(
                    "the held shortcut did not restore interaction (through {}, temporary {}, held {})",
                    held.0, held.1, held.2
                )
                .into());
            }
            // Every single tick has to keep that state. Sampling only after pairs
            // of ticks hid the bug: the flip had period two, so checking every
            // other tick always landed in the same phase again.
            for tick in 1..=3 {
                host.poll_held_with(true);
                let now = (host.pins[0].through, host.pins[0].temporary, host.held_down);
                if now != held {
                    return Err(format!(
                        "holding the shortcut oscillates on tick {tick}: {held:?} became {now:?}"
                    )
                    .into());
                }
            }
            let ex_style = GetWindowLongPtrW(host.pins[0].hwnd, GWL_EXSTYLE) as u32;
            if ex_style & WS_EX_TRANSPARENT.0 != 0 {
                return Err(format!(
                    "the temporarily interactive pin ignores the mouse (ex style {ex_style:#x})"
                )
                .into());
            }
            if WindowFromPoint(center) != host.pins[0].hwnd {
                return Err(
                    "the temporarily interactive pin is not the window at its own centre".into(),
                );
            }
            host.poll_held_with(false);
            let released = (host.pins[0].through, host.pins[0].temporary, host.held_down);
            if released != (true, false, false) {
                return Err(format!(
                    "releasing the held shortcut did not restore click-through (through {}, temporary {}, held {})",
                    released.0, released.1, released.2
                )
                .into());
            }
            for tick in 1..=2 {
                host.poll_held_with(false);
                let now = (host.pins[0].through, host.pins[0].temporary, host.held_down);
                if now != released {
                    return Err(format!(
                        "the released state oscillates on tick {tick}: {released:?} became {now:?}"
                    )
                    .into());
                }
            }
            // Every check above hands the poll a key state, so the keyboard read
            // itself is still untested — and that is exactly where the feature
            // silently stopped working before. With the environment variable set
            // the shortcut is pressed for real: `SendInput` puts the key down for
            // the system and the real poll has to read it back through
            // `GetAsyncKeyState`. Opt-in, because it presses a key on whatever
            // the user is doing at that moment.
            if std::env::var_os("SNAPLIGHT_SMOKE_HOLD_KEY").is_some() {
                send_key(VK_CONTROL, true)?;
                let mut lifted = false;
                for _ in 0..100 {
                    host.poll_held();
                    if host.pins[0].temporary {
                        lifted = true;
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                if !lifted {
                    return Err(
                        "pressing the held shortcut for real did not restore interaction".into(),
                    );
                }
                send_key(VK_CONTROL, false)?;
                let mut back = false;
                for _ in 0..100 {
                    host.poll_held();
                    if host.pins[0].is_click_through() {
                        back = true;
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                if !back {
                    return Err(
                        "releasing the held shortcut for real did not restore click-through".into(),
                    );
                }
                if host.held_down {
                    return Err("the released shortcut left the held state set".into());
                }
            }
            // A pin becomes click-through from its own window procedure, so the
            // poll is armed by the signal that procedure posts. Drain the queue
            // and require that signal to have been there: losing it is exactly
            // how holding the shortcut silently stopped working, and a direct
            // call to `update_held_poll` would paper over it.
            let mut queued = MSG::default();
            let mut notified = false;
            while PeekMessageW(&mut queued, control, 0, 0, PM_REMOVE).as_bool() {
                notified |= queued.message == HELD_SYNC;
                let _ = TranslateMessage(&queued);
                DispatchMessageW(&queued);
            }
            if !notified {
                return Err("a pin went click-through without waking the held poll".into());
            }
            if host.held_timer.is_none() {
                return Err(
                    "no timer watches the held shortcut while a pin is click-through".into(),
                );
            }
            if !host.pins[0].is_click_through() {
                return Err("the pin stopped being click-through before the restore check".into());
            }
            // The registered shortcut is the explicit way back and must restore
            // a plain click-through pin too, one the held shortcut never opened,
            // and stop the poll with it.
            SendMessageW(control, WM_HOTKEY, WPARAM(2), LPARAM(0));
            if host.pins[0].through || host.pins[0].temporary {
                return Err("the restore shortcut did not bring the pin back".into());
            }
            if host.held_timer.is_some() {
                return Err("the held shortcut is still polled with no click-through pin".into());
            }
            if WindowFromPoint(center) != host.pins[0].hwnd {
                return Err("the restored pin is not the window at its own centre".into());
            }
            host.pins[0].opacity = 0.8;
            host.pins[0].restore()?;
            // Annotating a pinned image again runs in the editor process: it loads
            // the pin's files, writes the annotated picture back, and the pin
            // picks that up when that process exits. The demo flag makes the
            // session annotate one rectangle and exit, so the handoff is checked
            // here without a user. The pin is selected the way clicking it selects
            // it, and the session has to drop that selection: it is no longer a
            // pin the shortcut may pick.
            let selected = pin_address(&host.pins[0]);
            host.set_selection(control, Some(selected));
            let picture = host.pins[0].picture_rect();
            if (picture.left, picture.top) != (requested.x, requested.y) {
                return Err(format!(
                    "an annotation session would cover ({}, {}) instead of the capture at ({}, {})",
                    picture.left, picture.top, requested.x, requested.y
                )
                .into());
            }
            open_annotation(&mut host.pins[0], true)?;
            if !host.pins[0].editing || host.pins[0].selected {
                return Err(
                    "starting a session did not mark the pin as busy and unselected".into(),
                );
            }
            let mut waited = 0;
            while host.pins[0].editing && waited < 30_000 {
                pump_messages(20);
                waited += 20;
            }
            if host.pins[0].editing {
                return Err("the pin's annotation session did not finish".into());
            }
            // The session paints one filled rectangle a quarter of the way in, and
            // the pin reloads the file it wrote, so the pixel proves both ends.
            let (_, painted) = read_pixels(&host.pins[0].path)?;
            let (px, py) = (
                (image.width / 4 + 20) as usize,
                (image.height / 4 + 15) as usize,
            );
            let n = (py * host.pins[0].image.stride as usize + px * 4) as usize;
            let pixel = [painted[n], painted[n + 1], painted[n + 2]];
            if pixel != [76, 204, 25] {
                return Err(format!(
                    "the pinned picture holds {pixel:?} where the session painted, expected the fill [76, 204, 25]"
                )
                .into());
            }
            // The other way into a session is the annotation shortcut, which runs
            // through raw keyboard input: nothing that passes values around can
            // cover it, so the key is pressed for real after a pin was selected
            // and the pointer has left it — and that press must still open this
            // pin's session. Opt-in, like the held shortcut, because it presses a
            // key and moves the pointer while the user may be working somewhere
            // else.
            if std::env::var_os("SNAPLIGHT_SMOKE_PIN_KEY").is_some() {
                // The shortcut's own path is what is under test, so it runs the
                // editor's scripted session: an interactive one would wait here
                // for a user who is not there.
                host.edit_demo = true;
                let mut previous = POINT::default();
                GetCursorPos(&mut previous)?;
                post_selected(&host.pins[0], 1_000)?;
                pump_messages(20);
                if !host.pins[0].selected {
                    return Err("the pin was not selected before the shortcut was pressed".into());
                }
                // Away from every pin: the selection has to outlive that, and the
                // key is what opens the session from there.
                let away = POINT { x: 10, y: 10 };
                SetCursorPos(away.x, away.y)?;
                if host.point_over_pin(away) {
                    return Err("the shortcut check cannot move the pointer off every pin".into());
                }
                send_key(VK_SPACE, true)?;
                send_key(VK_SPACE, false)?;
                let mut started = false;
                let mut waited = 0;
                while waited < 3_000 {
                    pump_messages(20);
                    waited += 20;
                    if host.pins[0].editing {
                        started = true;
                        break;
                    }
                }
                let _ = SetCursorPos(previous.x, previous.y);
                if !started {
                    return Err(
                        "pressing the annotation shortcut over a pin started nothing".into(),
                    );
                }
                // Let that session finish too, so the pin is not left mid-edit and
                // the second write-back is queued behind the first.
                let mut waited = 0;
                while host.pins[0].editing && waited < 30_000 {
                    pump_messages(20);
                    waited += 20;
                }
                if host.pins[0].editing {
                    return Err("the shortcut's annotation session did not finish".into());
                }
            }
            // The pin menu is drawn by the same component as the tray menu, so the
            // smoke only has to prove that the entries are wired to the commands
            // the handler maps and that 销毁 renders as the destructive row.
            let entries = menu_entries(DEFAULT_RESTORE_HOTKEY, host.shadow_default);
            let menu = popup_menu::render(&entries, 1.0, None)?;
            let destroy = menu
                .layout
                .rows
                .iter()
                .find(|row| row.entry == pin_item::DESTROY)
                .ok_or("pin menu is missing the destroy row")?;
            let destructive = (destroy.band.y..destroy.band.bottom()).any(|y| {
                (destroy.text_left..destroy.text_right).any(|x| {
                    menu.pixel(x, y)
                        .is_some_and(|p| p[2] as i32 > 150 && p[2] as i32 > p[0] as i32 + 60)
                })
            });
            if !destructive {
                return Err("the destroy entry is not drawn as destructive".into());
            }
            // Closing by double-click runs the whole chain: the double-click
            // message enters the window procedure, the prompt creates its own
            // window and handles keys, the answer reaches the configuration and
            // only then is the pin closed. The switch is read when the pin is
            // double-clicked, so the smoke turns it on explicitly.
            let mut confirm_on = snaplight_platform::load_config().unwrap_or_default();
            confirm_on.pin_close_confirm = true;
            snaplight_platform::save_config(&confirm_on)?;
            let first = host.pins[0].hwnd;
            let first_address = first.0 as usize;

            // Cancelling leaves the pin exactly where it was.
            let worker =
                std::thread::spawn(move || answer_prompt(first_address, &[VK_ESCAPE.0 as u32]));
            if double_click_close(first) {
                return Err("cancelling the close prompt closed the pin".into());
            }
            join_answer(worker)?;

            // Confirming with the box ticked closes the pin and remembers it.
            let worker = std::thread::spawn(move || {
                answer_prompt(first_address, &[VK_SPACE.0 as u32, VK_RETURN.0 as u32])
            });
            if !double_click_close(first) {
                return Err("confirming the close prompt left the pin open".into());
            }
            join_answer(worker)?;
            if snaplight_platform::load_config()?.pin_close_confirm {
                return Err("the prompt's 下次不再提醒 box did not reach the configuration".into());
            }

            // With the switch off the pin closes at once. A prompt that appeared
            // anyway is cancelled by the answering thread, so the check below
            // fails instead of hanging on a prompt nobody answers. The files are
            // passed as copies: the selection checks further down open two more
            // pins on the same pinned picture.
            host.open(
                control,
                path.clone(),
                doc.clone(),
                ScreenPoint { x: 400, y: 90 },
            )?;
            // A new pin follows the configured default, which the shadow check
            // switched off: it must have no margin, and its window is then the
            // capture itself again.
            let mut bare_default = RECT::default();
            GetWindowRect(host.pins[1].hwnd, &mut bare_default)?;
            if host.pins[1].shadow.enabled() || host.pins[1].margin() != 0 {
                return Err("a new pin ignored the configured shadow default".into());
            }
            let bare_size = (
                bare_default.right - bare_default.left,
                bare_default.bottom - bare_default.top,
            );
            if (bare_default.left, bare_default.top) != (400, 90)
                || bare_size != (image.width as i32, image.height as i32)
            {
                return Err(format!(
                    "a pin without a shadow is at ({}, {}) {}x{} instead of the capture at (400, 90)",
                    bare_default.left,
                    bare_default.top,
                    bare_default.right - bare_default.left,
                    bare_default.bottom - bare_default.top
                )
                .into());
            }
            let second = host.pins[1].hwnd;
            let second_address = second.0 as usize;
            let worker =
                std::thread::spawn(move || answer_prompt(second_address, &[VK_ESCAPE.0 as u32]));
            if !double_click_close(second) {
                return Err("a pin did not close although the confirmation was off".into());
            }
            join_answer(worker)?;

            // Selection: clicking a pin makes it the only selected one, the frame
            // is a fixed blue whatever the theme is, the pointer leaving does not
            // drop it, and the annotation shortcut opens the selected pin from
            // there. The click is delivered as the posted signal the pin's window
            // procedure sends — that is the path a real click takes, from the
            // window procedure to the host — and the press of a click that landed
            // elsewhere is handed in as a position, because moving the user's
            // cursor belongs to the opt-in checks.
            //
            // A previous opt-in check (SNAPLIGHT_SMOKE_PIN_KEY) may have moved the
            // real cursor, whose WM_INPUT set selected_time to the system clock.
            // The posted signals below carry timestamps from their own synthetic
            // timeline, so that stale stamp would reject them as older events.
            // Start the timeline clean here; the product's own time logic is
            // untouched.
            host.selected_time = None;
            let upper = host.pins.len();
            host.open(
                control,
                path.clone(),
                doc.clone(),
                ScreenPoint { x: 60, y: 60 },
            )?;
            let lower = host.pins.len();
            host.open(
                control,
                path.clone(),
                doc.clone(),
                ScreenPoint { x: 60, y: 320 },
            )?;
            for index in [upper, lower] {
                if host.pins[index].margin() != 0 {
                    return Err(
                        "a selection pin has a shadow margin; the frame check reads the picture edge"
                            .into(),
                    );
                }
            }
            let frame_blue = |pixel: (u8, u8, u8)| pixel.2 > 200 && pixel.1 > 100 && pixel.0 < 90;
            let mut upper_rect = RECT::default();
            GetWindowRect(host.pins[upper].hwnd, &mut upper_rect)?;
            let upper_frame = (
                upper_rect.left + 1,
                upper_rect.top + (upper_rect.bottom - upper_rect.top) / 2,
            );
            let mut lower_rect = RECT::default();
            GetWindowRect(host.pins[lower].hwnd, &mut lower_rect)?;
            let lower_frame = (
                lower_rect.left + 1,
                lower_rect.top + (lower_rect.bottom - lower_rect.top) / 2,
            );
            let t_upper = 2_000u32;
            let t_lower = 3_000u32;
            post_selected(&host.pins[upper], t_upper)?;
            pump_messages(20);
            if !host.pins[upper].selected || host.pins[lower].selected {
                return Err("clicking a pin did not select exactly that pin".into());
            }
            if !host.mouse_sink {
                return Err("the raw mouse sink is not registered while a pin is selected".into());
            }
            // `point_over_pin` asks the system which window is under the point,
            // so the pin has to be that window at its own centre, and a point on
            // the desktop must not count.
            let upper_center = POINT {
                x: upper_rect.left + (upper_rect.right - upper_rect.left) / 2,
                y: upper_rect.top + (upper_rect.bottom - upper_rect.top) / 2,
            };
            if !host.point_over_pin(upper_center) {
                return Err("the pin is not the window at its own centre".into());
            }
            if host.point_over_pin(POINT { x: 2, y: 2 }) {
                return Err("an empty point counted as being over a pin".into());
            }
            // A raw press older than the selection describes a click that landed
            // before it; it must not drop the selection the pointer just made.
            host.raw_event(control, POINT { x: 2, y: 2 }, t_upper - 1, &mouse_event());
            if !host.pins[upper].selected {
                return Err("a raw press older than the selection dropped it".into());
            }
            require_pixel(
                upper_frame.0,
                upper_frame.1,
                "the selected pin is not framed in the fixed blue",
                frame_blue,
            )?;
            require_pixel(
                lower_frame.0,
                lower_frame.1,
                "a pin that was not clicked drew the selection frame",
                |pixel| !frame_blue(pixel),
            )?;
            // The pointer leaving the pin — the message Windows sends when it
            // does — must not drop the selection or its frame.
            SendMessageW(host.pins[upper].hwnd, WM_NCMOUSELEAVE, WPARAM(0), LPARAM(0));
            if !host.pins[upper].selected {
                return Err("leaving the pin dropped the selection".into());
            }
            require_pixel(
                upper_frame.0,
                upper_frame.1,
                "the frame went away although the pin is still selected",
                frame_blue,
            )?;
            // Selecting another pin takes the selection away from the first one,
            // frame included.
            post_selected(&host.pins[lower], t_lower)?;
            pump_messages(20);
            if host.pins[upper].selected || !host.pins[lower].selected {
                return Err(
                    "selecting a second pin did not take the selection away from the first".into(),
                );
            }
            require_pixel(
                lower_frame.0,
                lower_frame.1,
                "the newly selected pin is not framed",
                frame_blue,
            )?;
            require_pixel(
                upper_frame.0,
                upper_frame.1,
                "the pin that lost the selection kept its frame",
                |pixel| !frame_blue(pixel),
            )?;
            // A selection signal older than the current one — a click queued
            // behind the newer one, or one that arrived late — must not put the
            // earlier pin back.
            post_selected(&host.pins[upper], t_upper)?;
            pump_messages(20);
            if host.pins[upper].selected || !host.pins[lower].selected {
                return Err("a stale selection signal moved the selection back".into());
            }
            // Escape drops the selection through the same raw input path the
            // keyboard sink delivers keys on.
            host.raw_event(
                control,
                POINT { x: 2, y: 2 },
                t_lower + 1,
                &keyboard_event(VK_ESCAPE.0),
            );
            if host.pins.iter().any(|pin| pin.selected) {
                return Err("Escape did not drop the selection".into());
            }
            if host.mouse_sink {
                return Err("the raw mouse sink stayed registered with nothing selected".into());
            }
            // A press where no pin is clears it as well, through the raw path a
            // real click takes: its recorded position is on the desktop.
            post_selected(&host.pins[upper], 4_000)?;
            pump_messages(20);
            host.raw_event(control, POINT { x: 2, y: 2 }, 4_100, &mouse_event());
            if host.pins.iter().any(|pin| pin.selected) {
                return Err("a press outside every pin kept the selection".into());
            }
            // Invalid selections select nothing: an address matching no pin, and
            // a pin the pointer can no longer click.
            host.set_selection(control, Some(usize::MAX));
            if host.pins.iter().any(|pin| pin.selected) {
                return Err("an address matching no pin selected one".into());
            }
            host.pins[upper].through = true;
            host.set_selection(control, Some(pin_address(&host.pins[upper])));
            if host.pins.iter().any(|pin| pin.selected) {
                return Err("a click-through pin was selected".into());
            }
            if host.mouse_sink {
                return Err("the raw mouse sink stayed registered with nothing selected".into());
            }
            host.pins[upper].through = false;
            host.raw_event(
                control,
                POINT { x: 2, y: 2 },
                4_200,
                &keyboard_event(VK_SPACE.0),
            );
            if host.pins.iter().any(|pin| pin.editing) {
                return Err(
                    "the annotation shortcut started a session with nothing selected".into(),
                );
            }
            // With a selection the shortcut opens that pin's session even though
            // the pointer left it, which is what the 空格 hint on the 编辑 entry
            // promises. The session runs the editor's scripted mode, so no window
            // waits for a user.
            host.edit_demo = true;
            post_selected(&host.pins[lower], 6_000)?;
            pump_messages(20);
            SendMessageW(host.pins[lower].hwnd, WM_NCMOUSELEAVE, WPARAM(0), LPARAM(0));
            if !host.pins[lower].selected {
                return Err("leaving the pin dropped the selection before the shortcut ran".into());
            }
            host.raw_event(
                control,
                POINT { x: 2, y: 2 },
                6_100,
                &keyboard_event(VK_SPACE.0),
            );
            if !host.pins[lower].editing {
                return Err(
                    "the annotation shortcut did not open the selected pin's session".into(),
                );
            }
            if host.pins.iter().any(|pin| pin.selected) {
                return Err("a pin whose annotation session started stayed selected".into());
            }
            let mut waited = 0;
            while host.pins[lower].editing && waited < 30_000 {
                pump_messages(20);
                waited += 20;
            }
            if host.pins[lower].editing {
                return Err("the selected pin's annotation session did not finish".into());
            }
            // Escape and a press elsewhere record their own time, so a selection
            // signal posted behind them — one that was queued before the event
            // that dropped the selection — must not revive the pin.
            post_selected(&host.pins[upper], 7_000)?;
            pump_messages(20);
            if !host.pins[upper].selected {
                return Err("the selection for the stale-Escape check did not take".into());
            }
            host.raw_event(
                control,
                POINT { x: 2, y: 2 },
                7_100,
                &keyboard_event(VK_ESCAPE.0),
            );
            if host.pins.iter().any(|pin| pin.selected) {
                return Err("Escape did not drop the selection before the stale signal".into());
            }
            post_selected(&host.pins[upper], 7_050)?;
            pump_messages(20);
            if host.pins[upper].selected {
                return Err(
                    "a selection signal older than the Escape revived the selection".into(),
                );
            }
            post_selected(&host.pins[upper], 8_000)?;
            pump_messages(20);
            if !host.pins[upper].selected {
                return Err("the selection for the stale-press check did not take".into());
            }
            host.raw_event(control, POINT { x: 2, y: 2 }, 8_100, &mouse_event());
            if host.pins.iter().any(|pin| pin.selected) {
                return Err("a press outside every pin kept the selection".into());
            }
            post_selected(&host.pins[upper], 8_050)?;
            pump_messages(20);
            if host.pins[upper].selected {
                return Err("a selection signal older than a press revived the selection".into());
            }
            println!(
                "pin selection: one blue frame on the {} backend, kept while the pointer left and against a stale press, dropped by a press elsewhere, Escape and a stale address",
                if use_gpu { "composition" } else { "layered" }
            );

            match config_snapshot {
                Some(bytes) => snaplight_platform::atomic_write(&config_path, &bytes)?,
                None => {
                    let _ = std::fs::remove_file(&config_path);
                }
            }

            host.close_all();
            snaplight_platform::atomic_write(
                &dir.join(if use_gpu {
                    "pin-smoke-result.json"
                } else {
                    "pin-smoke-low-memory-result.json"
                }),
                &serde_json::to_vec(
                    &serde_json::json!({"composition":use_gpu,"layered_transition":true,"scale":true,"opacity":true,"middle_reset":true,"menu_destroy":true,"menu_reset":true,"close_confirm":true,"held_interaction":true,"pin_shadow":true,"shadow_geometry":true,"shadow_pixels":true,"shadow_menu":true,"shadow_toggle":true,"shadow_reload":true,"shadow_default":true,"selection_single":true,"selection_frame":true,"selection_leave":true,"selection_clear":true,"selection_shortcut":true,"selection_timing":true,"selection_invalid":true,"position":[placed.left,placed.top],"capture":[placed.left+margin,placed.top+margin],"shadow_margin":margin,"size":[placed.right-placed.left,placed.bottom-placed.top]}),
                )?,
            )?;
        } else {
            std::thread::spawn(move || {
                for line in BufReader::new(std::io::stdin())
                    .lines()
                    .map_while(std::result::Result::ok)
                {
                    if let Ok(e) = serde_json::from_str::<Envelope<PinCommand>>(&line) {
                        if e.version == PROTOCOL_VERSION {
                            queue.lock().unwrap().push(e.payload);
                            let _ =
                                PostMessageW(HWND(address as _), INCOMING, WPARAM(0), LPARAM(0));
                        }
                    }
                }
                queue.lock().unwrap().push(PinCommand::Shutdown);
                let _ = PostMessageW(HWND(address as _), INCOMING, WPARAM(0), LPARAM(0));
            });
            snaplight_platform::native::message_loop()?;
        }
        host.close_all();
        SetWindowLongPtrW(control, GWLP_USERDATA, 0);
        let _ = UnregisterHotKey(control, 2);
        DestroyWindow(control)?;
        drop(host);
        CoUninitialize();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    /// The menu indices are what the window procedure matches on, so they must
    /// keep pointing at the entry that performs that command.
    #[test]
    fn pin_menu_entries_match_the_commands_the_handler_maps() {
        let entries = menu_entries(DEFAULT_RESTORE_HOTKEY, true);
        assert_eq!(entries.len(), 6);
        let divider = entries
            .iter()
            .position(|entry| matches!(entry, Entry::Separator))
            .expect("the pin menu needs a divider");
        assert_eq!(
            divider, 4,
            "the divider separates 销毁 from the view commands"
        );
        for index in [
            pin_item::EDIT,
            pin_item::THROUGH,
            pin_item::RESET,
            pin_item::SHADOW,
            pin_item::DESTROY,
        ] {
            assert_ne!(index, divider, "commands never point at the divider");
            assert!(matches!(entries[index], Entry::Item(_)));
        }
        assert_eq!(pin_item::DESTROY, entries.len() - 1);
        let Entry::Item(edit) = &entries[pin_item::EDIT] else {
            panic!("edit entry must be an item");
        };
        assert_eq!(
            edit.label, "编辑",
            "the main label must be just the command; the key sits on its right"
        );
        assert_eq!(
            edit.shortcut.as_deref(),
            Some("空格"),
            "the menu must teach the shortcut that opens the same session"
        );
        assert!(!edit.danger);
        let Entry::Item(through) = &entries[pin_item::THROUGH] else {
            panic!("through entry must be an item");
        };
        // The held-interaction key has no visible state, but the entry that turns
        // click-through on must not carry a second explanation either: the card
        // stays as short as the tray menu's rows.
        assert_eq!(through.label, "鼠标穿透");
        assert_eq!(through.shortcut.as_deref(), Some(DEFAULT_RESTORE_HOTKEY));
        assert!(!through.danger);
        let Entry::Item(reset) = &entries[pin_item::RESET] else {
            panic!("reset entry must be an item");
        };
        assert_eq!(
            reset.label, "恢复原始大小",
            "the size reset moved off the double-click, which now closes the pin"
        );
        assert!(!reset.danger);
        let Entry::Item(shadow) = &entries[pin_item::SHADOW] else {
            panic!("shadow entry must be an item");
        };
        assert_eq!(
            shadow.label, "关闭阴影",
            "the entry names the action, since the card cannot show a checkmark"
        );
        assert!(!shadow.danger);
        let Entry::Item(destroy) = &entries[pin_item::DESTROY] else {
            panic!("destroy entry must be an item");
        };
        assert_eq!(destroy.label, "销毁");
        assert!(destroy.danger, "destroy closes the pin and must read red");
    }
    /// The annotation shortcut is the spacebar press, not its release. The key
    /// arrives as raw keyboard input so a pin that never takes focus can still
    /// open a session, and the release that follows must not open a second one.
    #[test]
    fn the_annotation_shortcut_is_the_spacebar_press() {
        assert!(key_pressed(VK_SPACE.0, 0, VK_SPACE.0));
        assert!(!key_pressed(VK_SPACE.0, RI_KEY_BREAK as u16, VK_SPACE.0));
        // Any other key is ignored, pressed or released.
        assert!(!key_pressed(VK_RETURN.0, 0, VK_SPACE.0));
        assert!(!key_pressed(VK_RETURN.0, RI_KEY_BREAK as u16, VK_SPACE.0));
    }
    /// Message times order a raw press against the selection it might drop, and
    /// `GetMessageTime`'s counter wraps, so "later" has to read the shorter way
    /// round the circle rather than a plain `<`.
    #[test]
    fn the_message_time_comparison_survives_the_counter_wrapping() {
        assert!(later_than(10, 5));
        assert!(!later_than(5, 10));
        // Equal times are the same click: neither is later.
        assert!(!later_than(5, 5));
        // Just after the wrap is later than just before it.
        assert!(later_than(1, u32::MAX));
        assert!(!later_than(u32::MAX, 1));
    }
    /// Nothing is recorded before the first selectable input, so that input can
    /// never be stale: `GetMessageTime` counts from boot, so a machine up for
    /// longer than half the counter's range hands out a first time past
    /// `0x8000_0000`, which a default of `0` would have read as ancient. Once a
    /// time is recorded, only a genuinely older one — wrapped counter or not —
    /// is stale, and an equal one is the same click.
    #[test]
    fn the_first_selection_time_is_never_stale() {
        assert!(!predates_selection(None, 0x8000_0001));
        // Which is what a default of `0` would have wrongly rejected.
        assert!(predates_selection(Some(0), 0x8000_0001));
        assert!(predates_selection(Some(0x8000_0001), 0x8000_0000));
        assert!(!predates_selection(Some(0x8000_0001), 0x8000_0002));
        assert!(predates_selection(Some(1), u32::MAX));
        assert!(!predates_selection(Some(u32::MAX), 1));
        // Equal times are the same click: a press and its selection signal.
        assert!(!predates_selection(Some(5), 5));
    }
    /// The shadow entry has to read as the action it performs, so a pin without
    /// a shadow offers to turn one on.
    #[test]
    fn the_shadow_entry_names_the_action_it_performs() {
        let entries = menu_entries(DEFAULT_RESTORE_HOTKEY, false);
        let Entry::Item(shadow) = &entries[pin_item::SHADOW] else {
            panic!("shadow entry must be an item");
        };
        assert_eq!(shadow.label, "开启阴影");
    }
    /// A pin's shadow follows the configuration until the context menu makes a
    /// choice, and that choice is what a later settings change must not undo.
    #[test]
    fn the_per_pin_choice_outlives_a_settings_reload() {
        let settings = shadow::Settings {
            blur: 12,
            offset_y: 4,
            opacity: 40,
        };
        let mut shadow = Shadow {
            settings,
            dpi: 1.0,
            default_on: true,
            choice: None,
        };
        assert!(shadow.enabled(), "a new pin is shadowed by default");
        assert_eq!(shadow.params().map(|p| p.margin()), Some(16));
        shadow.choice = Some(false);
        assert!(!shadow.enabled());
        assert_eq!(shadow.params(), None);
        // The settings change the metrics, not the user's choice for this pin.
        shadow.settings = shadow::Settings {
            blur: 24,
            offset_y: 0,
            opacity: 100,
        };
        assert!(!shadow.enabled());
        shadow.choice = Some(true);
        assert_eq!(shadow.margin(), 24);
        // Turning the configured default off leaves an explicit choice alone.
        shadow.default_on = false;
        assert!(shadow.enabled());
    }
}

use snaplight_domain::*;
use snaplight_platform::popup_menu::{Entry, Item};
use snaplight_platform::{popup_menu, read_pixels, Capabilities, WindowService};
use snaplight_render_win::{Gpu, StaticSurface};
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{Com::*, LibraryLoader::*},
        UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
};

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
/// Entry indices of the pin context menu, in display order.
mod pin_item {
    pub const THROUGH: usize = 0;
    pub const DESTROY: usize = 2;
}
/// Pin context menu content, drawn by the same owner-drawn card as the tray
/// menu. `restore_hotkey` is the shortcut that brings interaction back after the
/// pin goes click-through, and it is shown on the right of that entry; the entry
/// itself stays a plain label so the card never has to explain the held key.
/// Destroying the pin discards the image the user pinned, so it is drawn in red.
fn menu_entries(restore_hotkey: &str) -> Vec<Entry> {
    vec![
        Entry::Item(Item::new("鼠标穿透").with_shortcut(restore_hotkey)),
        Entry::Separator,
        Entry::Item(Item::new("销毁").danger()),
    ]
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
    scale: f32,
    opacity: f32,
    /// Shortcut shown next to the context menu entry, resolved at startup.
    restore_hotkey: String,
}
impl Pin {
    unsafe fn restore(&mut self) -> std::result::Result<(), Box<dyn std::error::Error>> {
        if self.is_click_through() || self.gpu.is_none() {
            return self.update_layered();
        }
        if let Some(surface) = &self.surface {
            surface.scale(self.scale)?;
            surface.opacity(self.opacity)?;
            return Ok(());
        }
        let (image, bytes) = read_pixels(&self.path)?;
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
        let (image, mut bytes) = read_pixels(&self.path)?;
        let width = (image.width as f32 * self.scale).max(1.0) as u32;
        let height = (image.height as f32 * self.scale).max(1.0) as u32;
        if width != image.width || height != image.height {
            let size = (width as usize)
                .checked_mul(height as usize)
                .and_then(|n| n.checked_mul(4))
                .ok_or("scaled image overflow")?;
            let mut scaled = vec![0; size];
            for y in 0..height {
                for x in 0..width {
                    let sx = (x as u64 * image.width as u64 / width as u64) as usize;
                    let sy = (y as u64 * image.height as u64 / height as u64) as usize;
                    let src = sy * image.stride as usize + sx * 4;
                    let dest = (y as usize * width as usize + x as usize) * 4;
                    scaled[dest..dest + 4].copy_from_slice(&bytes[src..src + 4]);
                }
            }
            bytes = scaled;
        }
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
        SetWindowPos(
            self.hwnd,
            HWND_TOPMOST,
            0,
            0,
            (self.image.width as f32 * self.scale) as i32,
            (self.image.height as f32 * self.scale) as i32,
            SWP_NOMOVE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )?;
        self.restore()
    }
    unsafe fn zoom(&mut self, value: f32) -> std::result::Result<(), Box<dyn std::error::Error>> {
        if self.is_click_through() {
            return Ok(());
        }
        self.scale = value.clamp(0.1, 4.0);
        if let Some(surface) = &self.surface {
            surface.scale(self.scale)?;
        } else {
            self.update_layered()?;
        }
        SetWindowPos(
            self.hwnd,
            HWND_TOPMOST,
            0,
            0,
            (self.image.width as f32 * self.scale).max(1.0) as i32,
            (self.image.height as f32 * self.scale).max(1.0) as i32,
            SWP_NOMOVE | SWP_NOACTIVATE,
        )?;
        Ok(())
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
    let entries = menu_entries(&pin.restore_hotkey);
    let choice = popup_menu::show(&entries, None);
    if pin.closed || !IsWindow(pin.hwnd).as_bool() {
        return Ok(());
    }
    match choice {
        Some(pin_item::THROUGH) => toggle_through(pin)?,
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
/// Presses or releases the control key through `SendInput`, so the smoke can
/// check that the held shortcut is really read back from the keyboard. Only the
/// opt-in smoke step calls this: it presses a key on the user's desktop.
unsafe fn send_control(down: bool) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VK_CONTROL,
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
        return Err("could not inject the held shortcut".into());
    }
    Ok(())
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
                WM_NCLBUTTONDBLCLK => {
                    pin.zoom(1.0)?;
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
                    let control = pin.control;
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    DestroyWindow(hwnd)?;
                    PostMessageW(control, CLOSED, WPARAM(0), LPARAM(0))?;
                    return Ok(Some(LRESULT(0)));
                }
                WM_DESTROY => {
                    pin.surface = None;
                    pin.closed = true;
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
        let (image, bytes) = read_pixels(&path)?;
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
        let surface = if let Some(gpu) = &self.gpu {
            match StaticSurface::new(gpu, hwnd, &image, &bytes) {
                Ok(v) => {
                    v.scale(scale)?;
                    Some(v)
                }
                Err(e) => {
                    let _ = DestroyWindow(hwnd);
                    return Err(e.into());
                }
            }
        } else {
            None
        };
        drop(bytes);
        let mut pin = Box::new(Pin {
            hwnd,
            control,
            gpu: self.gpu.clone(),
            surface,
            path,
            image,
            closed: false,
            through: false,
            temporary: false,
            scale,
            opacity: 1.0,
            restore_hotkey: self.restore_hotkey.clone(),
        });
        if pin.gpu.is_none() {
            pin.update_layered()?;
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&mut *pin as *mut Pin) as isize);
        self.pins.push(pin);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        Ok(())
    }
    unsafe fn close_all(&mut self) {
        for pin in &mut self.pins {
            if !pin.closed {
                pin.surface = None;
                pin.closed = true;
                SetWindowLongPtrW(pin.hwnd, GWLP_USERDATA, 0);
                let _ = DestroyWindow(pin.hwnd);
            }
        }
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
}
unsafe extern "system" fn control_proc(hwnd: HWND, msg: u32, _w: WPARAM, l: LPARAM) -> LRESULT {
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
                // procedure; the poll follows the pins, not the caller.
                host.update_held_poll(hwnd);
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
    DefWindowProcW(hwnd, msg, _w, l)
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
pub fn run() -> std::result::Result<(), Box<dyn std::error::Error>> {
    snaplight_platform::native::dpi_aware();
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
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
        });
        SetWindowLongPtrW(control, GWLP_USERDATA, (&mut *host as *mut Host) as isize);
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
                    image,
                    selection: None,
                    annotations: vec![],
                })?,
            )?;
            // The pin must open exactly where the captured region was, not at the
            // pointer, so verify the window rect against the requested position.
            let requested = ScreenPoint { x: 60, y: 90 };
            host.open(control, path, doc, requested)?;
            let mut placed = RECT::default();
            GetWindowRect(host.pins[0].hwnd, &mut placed)?;
            if (placed.left, placed.top) != (requested.x, requested.y) {
                return Err(format!(
                    "pin opened at ({}, {}) instead of ({}, {})",
                    placed.left, placed.top, requested.x, requested.y
                )
                .into());
            }
            host.pins[0].zoom(0.5)?;
            let mut zoomed = RECT::default();
            GetWindowRect(host.pins[0].hwnd, &mut zoomed)?;
            if (zoomed.left, zoomed.top) != (placed.left, placed.top) {
                return Err("scaling moved the pin window".into());
            }
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
                send_control(true)?;
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
                send_control(false)?;
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
            // The pin menu is drawn by the same component as the tray menu, so the
            // smoke only has to prove that the entries are wired to the commands
            // the handler maps and that 销毁 renders as the destructive row.
            let entries = menu_entries(DEFAULT_RESTORE_HOTKEY);
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
            host.close_all();
            snaplight_platform::atomic_write(
                &dir.join(if use_gpu {
                    "pin-smoke-result.json"
                } else {
                    "pin-smoke-low-memory-result.json"
                }),
                &serde_json::to_vec(
                    &serde_json::json!({"composition":use_gpu,"layered_transition":true,"scale":true,"opacity":true,"menu_destroy":true,"held_interaction":true,"position":[placed.left,placed.top],"size":[placed.right-placed.left,placed.bottom-placed.top]}),
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
        let entries = menu_entries(DEFAULT_RESTORE_HOTKEY);
        assert_eq!(entries.len(), 3);
        let divider = entries
            .iter()
            .position(|entry| matches!(entry, Entry::Separator))
            .expect("the pin menu needs a divider");
        assert_eq!(
            divider, 1,
            "the divider separates 销毁 from the view command"
        );
        for index in [pin_item::THROUGH, pin_item::DESTROY] {
            assert_ne!(index, divider, "commands never point at the divider");
            assert!(matches!(entries[index], Entry::Item(_)));
        }
        assert_eq!(pin_item::DESTROY, entries.len() - 1);
        let Entry::Item(through) = &entries[pin_item::THROUGH] else {
            panic!("through entry must be an item");
        };
        // The held-interaction key has no visible state, but the entry that turns
        // click-through on must not carry a second explanation either: the card
        // stays as short as the tray menu's rows.
        assert_eq!(through.label, "鼠标穿透");
        assert_eq!(through.shortcut.as_deref(), Some(DEFAULT_RESTORE_HOTKEY));
        assert!(!through.danger);
        let Entry::Item(destroy) = &entries[pin_item::DESTROY] else {
            panic!("destroy entry must be an item");
        };
        assert_eq!(destroy.label, "销毁");
        assert!(destroy.danger, "destroy closes the pin and must read red");
    }
}

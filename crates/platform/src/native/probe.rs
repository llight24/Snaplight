//! UI Automation window detection: ancestor-chain probing, region policy and
//! the overlay point-hole used to reach the window below a full-screen overlay.

use snaplight_domain::{ScreenPoint, ScreenRect};
use windows::{
    core::*,
    Win32::{
        Foundation::{E_FAIL, HWND, POINT, RECT, RPC_E_CHANGED_MODE},
        Graphics::Gdi::{
            CombineRgn, CreateRectRgn, DeleteObject, SetWindowRgn, HGDIOBJ, HRGN, RGN_DIFF,
            RGN_ERROR,
        },
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
            COINIT_MULTITHREADED,
        },
        UI::{
            Accessibility::{
                CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTreeWalker,
                UIA_WindowControlTypeId,
            },
            WindowsAndMessaging::GetWindowRect,
        },
    },
};

/// Desktop window class. It is the root of every desktop and taskbar
/// element chain, so it marks where the application content ends.
const DESKTOP_CLASS: &str = "#32769";
/// Shell windows that own the desktop or the taskbar. Detecting them would
/// only ever produce the whole screen or a taskbar button, so hovering them
/// reports no window at all and the user draws the region by hand.
const SHELL_CLASSES: &[&str] = &[
    "Progman",
    "WorkerW",
    "SysListView32",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
];
/// Smallest usable region: elements that report no usable size (separators,
/// zero-sized containers) climb to a larger ancestor instead.
const MIN_ELEMENT_SIZE: i32 = 8;
/// Levels walked from the element under the cursor, element included.
const MAX_CHAIN: usize = 10;
/// One level of the UI Automation ancestor chain of the element under the
/// cursor, in screen physical pixels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElementBounds {
    /// `UIA_*ControlTypeId`; `UIA_WindowControlTypeId` marks a top-level window.
    pub control_type: i32,
    pub class_name: String,
    pub rect: ScreenRect,
}
/// Picks the region to capture from an element chain ordered from the
/// element under the cursor up to its ancestors.
///
/// Returns `None` when the chain belongs to the desktop shell (wallpaper
/// icons, taskbar) or is empty: those are not capturable windows, and
/// highlighting the whole screen instead would be misleading.
pub fn pick_bounds(chain: &[ElementBounds]) -> Option<ScreenRect> {
    // Everything at and above the desktop root is shell chrome, not content.
    let end = chain
        .iter()
        .rposition(|entry| entry.class_name == DESKTOP_CLASS)
        .unwrap_or(chain.len());
    let content = &chain[..end];
    // The window is the innermost ancestor reported as a window; without one
    // the outermost element is the best available candidate, unless it is a
    // shell window itself.
    let top = content
        .iter()
        .position(|entry| entry.control_type == UIA_WindowControlTypeId.0)
        .or_else(|| content.len().checked_sub(1))?;
    if SHELL_CLASSES.contains(&content[top].class_name.as_str()) {
        return None;
    }
    let window = content[top].rect;
    let candidate = content[..=top]
        .iter()
        .find(|entry| entry.rect.width >= MIN_ELEMENT_SIZE && entry.rect.height >= MIN_ELEMENT_SIZE)
        .map(|entry| entry.rect)
        .unwrap_or(window);
    let region = candidate.intersect(window).unwrap_or(window);
    (region.width > 0 && region.height > 0).then_some(region)
}
/// UI Automation client bound to the thread that created it, which must be
/// the thread that runs the queries.
pub struct ElementProbe {
    automation: IUIAutomation,
    walker: IUIAutomationTreeWalker,
    /// True when this probe initialized the thread's apartment itself and
    /// therefore has to uninitialize it.
    owns_apartment: bool,
}
impl ElementProbe {
    /// Initializes UI Automation on the calling thread. A thread that
    /// already has an apartment (the overlay thread is single-threaded) keeps
    /// it; the client works there, but the apartment is not ours to drop.
    pub fn new() -> Result<Self> {
        unsafe {
            let initialized = CoInitializeEx(None, COINIT_MULTITHREADED);
            let owns_apartment = if initialized.is_ok() {
                true
            } else if initialized == RPC_E_CHANGED_MODE {
                false
            } else {
                return Err(initialized.into());
            };
            let probe = (|| -> Result<Self> {
                let automation: IUIAutomation =
                    CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
                let walker = automation.ControlViewWalker()?;
                Ok(Self {
                    automation,
                    walker,
                    owns_apartment,
                })
            })();
            if probe.is_err() && owns_apartment {
                CoUninitialize();
            }
            probe
        }
    }
    /// Ancestor chain of the element at `point` (element first).
    pub fn chain_at(&self, point: ScreenPoint) -> Vec<ElementBounds> {
        let mut chain = Vec::new();
        unsafe {
            let point = POINT {
                x: point.x,
                y: point.y,
            };
            let Ok(mut current) = self.automation.ElementFromPoint(point) else {
                return chain;
            };
            for _ in 0..MAX_CHAIN {
                match describe(&current) {
                    Some(entry) => chain.push(entry),
                    None => break,
                }
                match self.walker.GetParentElement(&current) {
                    Ok(parent) => current = parent,
                    Err(_) => break,
                }
            }
        }
        chain
    }
    /// Region of the element under `point`, or `None` when the point is over
    /// the desktop shell or no element reports a usable rectangle.
    pub fn bounds_at(&self, point: ScreenPoint) -> Option<ScreenRect> {
        pick_bounds(&self.chain_at(point))
    }
}
impl Drop for ElementProbe {
    fn drop(&mut self) {
        if self.owns_apartment {
            unsafe { CoUninitialize() };
        }
    }
}
fn describe(element: &IUIAutomationElement) -> Option<ElementBounds> {
    unsafe {
        let rect = element.CurrentBoundingRectangle().ok()?;
        Some(ElementBounds {
            control_type: element.CurrentControlType().map(|t| t.0).unwrap_or(0),
            class_name: element
                .CurrentClassName()
                .map(|name| name.to_string())
                .unwrap_or_default(),
            rect: ScreenRect {
                x: rect.left,
                y: rect.top,
                width: rect.right - rect.left,
                height: rect.bottom - rect.top,
            },
        })
    }
}
/// Window region that punches a one-pixel hole at a screen point.
///
/// A full-screen overlay hides everything below it from UI Automation
/// hit-testing, and neither `WS_EX_TRANSPARENT` nor `WS_DISABLED` changes
/// that; only the window region does. While the hole is open the pixel does
/// not hit-test to the window, so the caller must keep it open only for the
/// duration of a query and drop the guard immediately afterwards.
pub struct PointHole {
    hwnd: HWND,
}
impl PointHole {
    pub fn punch(hwnd: HWND, point: ScreenPoint) -> Result<Self> {
        unsafe {
            let mut window = RECT::default();
            GetWindowRect(hwnd, &mut window)?;
            let region =
                CreateRectRgn(0, 0, window.right - window.left, window.bottom - window.top);
            if region.is_invalid() {
                return Err(Error::from_win32());
            }
            let cut = CreateRectRgn(
                point.x - window.left,
                point.y - window.top,
                point.x - window.left + 1,
                point.y - window.top + 1,
            );
            let combined = CombineRgn(region, region, cut, RGN_DIFF);
            let _ = DeleteObject(HGDIOBJ(cut.0));
            if combined == RGN_ERROR {
                let _ = DeleteObject(HGDIOBJ(region.0));
                return Err(Error::new(E_FAIL, "empty probe region"));
            }
            // A successful SetWindowRgn hands the region over to the system.
            if SetWindowRgn(hwnd, region, true) == 0 {
                let _ = DeleteObject(HGDIOBJ(region.0));
                return Err(Error::from_win32());
            }
            Ok(Self { hwnd })
        }
    }
}
impl Drop for PointHole {
    fn drop(&mut self) {
        unsafe {
            // Restores the default full-window shape; the overlay never has a
            // region of its own.
            let _ = SetWindowRgn(self.hwnd, HRGN::default(), true);
        }
    }
}

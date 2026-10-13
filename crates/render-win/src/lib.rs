#[cfg(windows)]
mod capture;
#[cfg(windows)]
mod composition;
#[cfg(windows)]
mod d2d;
#[cfg(windows)]
mod gpu;
#[cfg(windows)]
pub mod gpu_share;
#[cfg(windows)]
mod text_outline;
#[cfg(windows)]
mod windows_backend;
#[cfg(windows)]
pub use capture::{capture_agreement, screen_pixels, DesktopCapture, FrozenFrame};
#[cfg(windows)]
pub use composition::StaticSurface;
#[cfg(windows)]
pub use gpu::Gpu;
#[cfg(windows)]
pub use windows_backend::{is_recoverable_device_error, ComposedFrame, OverlayRenderer};

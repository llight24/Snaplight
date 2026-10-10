use serde::{Deserialize, Serialize};
use snaplight_domain::ImageDescriptor;
use std::ffi::c_void;

pub const ABI_VERSION: u32 = 1;
pub const STATUS_OK: i32 = 0;
pub const STATUS_CANCELLED: i32 = 1;
pub const STATUS_ERROR: i32 = -1;
pub const CAP_INSPECT: u64 = 1;
pub const CAP_OCR: u64 = 2;
pub const CAP_SCROLL_CAPTURE: u64 = 4;
pub const CAP_RECORD: u64 = 8;
/// The plugin can open the named D3D11 shared texture transport instead of a
/// read-only CPU mapping.
pub const CAP_GPU_SHARED: u64 = 16;
/// Key the producer releases after publishing an image, so that it is the key a
/// consumer must acquire. The shared surface is created unowned and only key 0
/// can be acquired on the creating device, which is why the handshake starts at
/// `GPU_RELEASE_KEY` and alternates.
pub const GPU_ACQUIRE_KEY: u64 = 1;
/// Key a consumer releases the image with, which is the key the producer waits
/// for before writing the next image into the same texture.
pub const GPU_RELEASE_KEY: u64 = 0;
/// Upper bound for the consumer's keyed-mutex acquire timeout, so a producer
/// that died while holding the mutex cannot block a plugin indefinitely.
pub const GPU_ACQUIRE_TIMEOUT_MAX_MS: u64 = 60_000;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct BorrowedBytes {
    pub ptr: *const u8,
    pub len: usize,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct OwnedBytes {
    pub ptr: *mut u8,
    pub len: usize,
}
impl Default for OwnedBytes {
    fn default() -> Self {
        Self {
            ptr: std::ptr::null_mut(),
            len: 0,
        }
    }
}
#[repr(C)]
pub struct Cancellation {
    pub context: *const c_void,
    pub is_cancelled: unsafe extern "C" fn(*const c_void) -> bool,
}
#[repr(C)]
pub struct PluginApi {
    pub abi_version: u32,
    pub struct_size: u32,
    pub capabilities: u64,
    pub run: unsafe extern "C" fn(
        BorrowedBytes,
        BorrowedBytes,
        *const Cancellation,
        *mut OwnedBytes,
    ) -> i32,
    pub free: unsafe extern "C" fn(OwnedBytes),
}
pub type Entry = unsafe extern "C" fn() -> *const PluginApi;
#[derive(Debug, Serialize, Deserialize)]
pub struct RunRequest {
    pub library: String,
    pub operation: String,
    pub image: ImageDescriptor,
    /// Name of the read-only shared-memory mapping that carries `bytes` of CPU
    /// pixels. Empty when the request uses the GPU transport.
    pub mapping: String,
    /// Length of the shared-memory mapping. Zero when the request uses the GPU
    /// transport.
    pub bytes: usize,
    pub timeout_ms: u64,
    /// GPU transport descriptor. When present, the CPU mapping is empty and the
    /// plugin must open the named shared texture itself.
    #[serde(default)]
    pub gpu: Option<GpuImage>,
}
impl RunRequest {
    /// Checks the transport metadata before the host opens a mapping, loads a
    /// library, or builds any GPU object. The image descriptor is always
    /// validated; exactly one transport must be described.
    pub fn validate(&self) -> Result<(), &'static str> {
        self.image.byte_len()?;
        match &self.gpu {
            Some(gpu) => {
                if !self.mapping.is_empty() || self.bytes != 0 {
                    return Err("gpu image cannot include a cpu mapping");
                }
                gpu.validate(&self.image)
            }
            None => {
                if self.mapping.is_empty() {
                    return Err("missing shared memory mapping");
                }
                if self.image.byte_len()? != self.bytes {
                    return Err("mapping/image size mismatch");
                }
                Ok(())
            }
        }
    }
}
/// Names a D3D11 shared texture published by the caller. The producer keeps the
/// NT handle and the texture alive for as long as consumers may open it, and
/// closes both when the image is no longer published. Handles and keyed mutexes
/// travel as names and keys, never as GPU objects across the ABI.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GpuImage {
    /// NT named shared handle created by `IDXGIResource1::CreateSharedHandle`.
    pub name: String,
    /// `LUID` of the adapter that produced the texture, so consumers open it on
    /// the matching device.
    pub adapter_luid: u64,
    /// Keyed-mutex key a consumer acquires before reading the texture.
    pub acquire_key: u64,
    /// Keyed-mutex key a consumer releases with when it is done reading.
    pub release_key: u64,
    /// Bounded acquire timeout; a producer that died while holding the mutex
    /// must not stall the plugin forever.
    pub acquire_timeout_ms: u64,
}
impl GpuImage {
    pub fn validate(&self, image: &ImageDescriptor) -> Result<(), &'static str> {
        if self.name.is_empty() || self.name.len() > 255 || self.name.as_bytes().contains(&0) {
            return Err("invalid shared texture name");
        }
        if self.adapter_luid == 0 {
            return Err("missing adapter luid");
        }
        if self.acquire_key == self.release_key {
            return Err("gpu sync keys must differ");
        }
        if self.acquire_timeout_ms == 0 || self.acquire_timeout_ms > GPU_ACQUIRE_TIMEOUT_MAX_MS {
            return Err("gpu acquire timeout out of range");
        }
        // A shared texture has no row padding, so the descriptor must describe a
        // tightly packed image; anything else cannot be read back row by row.
        if image.stride != image.width.checked_mul(4).ok_or("image size overflow")? {
            return Err("gpu image must be tightly packed");
        }
        Ok(())
    }
}
#[derive(Debug, Serialize, Deserialize)]
pub struct RunResponse {
    pub status: i32,
    pub result: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use snaplight_domain::{AlphaMode, ColorSpace, ImageId, PixelFormat};

    fn request() -> RunRequest {
        RunRequest {
            library: "plugin.dll".into(),
            operation: "inspect".into(),
            image: ImageDescriptor {
                id: ImageId(1),
                width: 8,
                height: 4,
                stride: 32,
                format: PixelFormat::Bgra8,
                alpha: AlphaMode::Opaque,
                color_space: ColorSpace::Srgb,
            },
            mapping: "Local\\Snaplight.Test".into(),
            bytes: 128,
            timeout_ms: 1000,
            gpu: None,
        }
    }
    fn gpu() -> GpuImage {
        GpuImage {
            name: "Local\\Snaplight.Test".into(),
            adapter_luid: 0x1234_5678_9abc,
            acquire_key: GPU_ACQUIRE_KEY,
            release_key: GPU_RELEASE_KEY,
            acquire_timeout_ms: 5000,
        }
    }
    #[test]
    fn cpu_transport_requires_a_matching_mapping() {
        let mut r = request();
        assert!(r.validate().is_ok());
        r.bytes = 64;
        assert_eq!(r.validate(), Err("mapping/image size mismatch"));
        r = request();
        r.mapping.clear();
        assert_eq!(r.validate(), Err("missing shared memory mapping"));
        r = request();
        r.image.stride = 16;
        assert_eq!(r.validate(), Err("invalid image dimensions/stride"));
    }
    #[test]
    fn gpu_transport_excludes_the_cpu_mapping() {
        let mut r = request();
        r.gpu = Some(gpu());
        r.mapping.clear();
        r.bytes = 0;
        assert!(r.validate().is_ok());
        // Neither transport may be described twice.
        r.bytes = 128;
        assert_eq!(r.validate(), Err("gpu image cannot include a cpu mapping"));
        r.bytes = 0;
        r.mapping = "Local\\Snaplight.Test".into();
        assert_eq!(r.validate(), Err("gpu image cannot include a cpu mapping"));
    }
    #[test]
    fn gpu_descriptor_rejects_ambiguous_sync_and_padding() {
        let mut r = request();
        r.mapping.clear();
        r.bytes = 0;
        r.gpu = Some(gpu());
        assert!(r.validate().is_ok());
        r.gpu.as_mut().unwrap().acquire_key = GPU_RELEASE_KEY;
        assert_eq!(r.validate(), Err("gpu sync keys must differ"));
        r.gpu = Some(gpu());
        r.gpu.as_mut().unwrap().acquire_timeout_ms = 0;
        assert_eq!(r.validate(), Err("gpu acquire timeout out of range"));
        r.gpu = Some(gpu());
        r.gpu.as_mut().unwrap().adapter_luid = 0;
        assert_eq!(r.validate(), Err("missing adapter luid"));
        r.gpu = Some(gpu());
        r.gpu.as_mut().unwrap().name.clear();
        assert_eq!(r.validate(), Err("invalid shared texture name"));
        r.gpu = Some(gpu());
        r.image.stride = 64;
        assert_eq!(r.validate(), Err("gpu image must be tightly packed"));
    }
}

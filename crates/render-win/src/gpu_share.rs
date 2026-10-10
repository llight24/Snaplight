//! Producer side of the plugin GPU shared-texture transport.
//!
//! The producer creates a D3D11 texture with NT shared handles and a keyed
//! mutex, uploads one tightly packed image, and publishes the handle name. A
//! consumer opens that name on the adapter that produced the texture and
//! synchronizes through the keyed mutex.
//!
//! Ownership and lifetime contract:
//!
//! - The producer owns the NT handle and the texture. Both must stay alive for
//!   as long as a consumer may open or read the image, and both are released
//!   when the image is dropped. Closing the handle removes the name for future
//!   opens; it does not invalidate handles consumers already opened.
//! - Consumers never close the producer's handle. They release the resource
//!   they opened themselves.
//! - A freshly created surface is unowned and only key `0` can be acquired, so
//!   the producer handshake starts with an acquire of key `0`. The producer
//!   then writes and releases the key the request advertises as the consumer's
//!   acquire key; the consumer releases with key `0` to hand the image back.
//! - Dropping the producer while a consumer still waits on the mutex leaves the
//!   mutex owned by a dead device, which surfaces as `WAIT_ABANDONED` to that
//!   consumer. Consumers must treat it as a fatal handshake failure and must
//!   never wait without a timeout.

use crate::Gpu;
use snaplight_domain::{ImageDescriptor, PixelFormat};
use std::sync::atomic::{AtomicU64, Ordering};
use windows::{
    core::*,
    Win32::{
        Foundation::{CloseHandle, E_FAIL, E_INVALIDARG, E_UNEXPECTED, HANDLE, LUID},
        Graphics::{
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
    },
};

/// `AcquireSync` reports these two non-negative HRESULTs, which `windows` maps
/// to `Ok`; they must be checked explicitly to keep the handshake honest.
const WAIT_ABANDONED: i32 = 128;
const WAIT_TIMEOUT: i32 = 258;

static NEXT_NAME: AtomicU64 = AtomicU64::new(1);

/// Session-local name that is unique per process and publication. Random or
/// process-unique names keep an unrelated process from squatting on the name
/// before the producer publishes it.
pub fn unique_name() -> String {
    format!(
        "Local\\Snaplight.GpuImage.{}.{}",
        std::process::id(),
        NEXT_NAME.fetch_add(1, Ordering::Relaxed)
    )
}

/// Encodes an adapter `LUID` into the portable value used in plugin requests.
pub fn adapter_luid_key(luid: LUID) -> u64 {
    ((luid.HighPart as u32 as u64) << 32) | luid.LowPart as u64
}

fn dxgi_format(format: PixelFormat) -> DXGI_FORMAT {
    match format {
        PixelFormat::Bgra8 => DXGI_FORMAT_B8G8R8A8_UNORM,
        PixelFormat::Rgba8 => DXGI_FORMAT_R8G8B8A8_UNORM,
    }
}

/// Keyed-mutex wait that does not hide `WAIT_TIMEOUT`/`WAIT_ABANDONED`.
fn wait_result(hr: HRESULT, operation: &str) -> Result<()> {
    if hr == HRESULT(WAIT_TIMEOUT) {
        return Err(Error::new(
            E_FAIL,
            format!("{operation} timed out waiting for the keyed mutex"),
        ));
    }
    if hr == HRESULT(WAIT_ABANDONED) {
        return Err(Error::new(
            E_FAIL,
            format!("{operation} found an abandoned keyed mutex; republish the image"),
        ));
    }
    hr.ok()
}

fn acquire(keyed: &IDXGIKeyedMutex, key: u64, timeout_ms: u32) -> Result<()> {
    let hr = unsafe {
        (Interface::vtable(keyed).AcquireSync)(Interface::as_raw(keyed), key, timeout_ms)
    };
    wait_result(hr, "AcquireSync")
}

fn release(keyed: &IDXGIKeyedMutex, key: u64) -> Result<()> {
    let hr = unsafe { (Interface::vtable(keyed).ReleaseSync)(Interface::as_raw(keyed), key) };
    wait_result(hr, "ReleaseSync")
}

/// A published image: one shareable texture plus the named NT handle that lets
/// another process open it. The keyed mutex is owned by this side until the
/// consumer releases it.
pub struct SharedImage {
    name: String,
    adapter_luid: u64,
    image: ImageDescriptor,
    texture: ID3D11Texture2D,
    keyed: IDXGIKeyedMutex,
    handle: HANDLE,
}

impl SharedImage {
    /// Creates a shareable texture on `gpu`, uploads `pixels`, and publishes
    /// `name`. `acquire_key` is the key the consumer acquires after the upload;
    /// the consumer must release with key `0`.
    pub fn publish(
        gpu: &Gpu,
        name: String,
        image: ImageDescriptor,
        pixels: &[u8],
        acquire_key: u64,
    ) -> Result<Self> {
        unsafe {
            let length = image.byte_len().map_err(|e| Error::new(E_INVALIDARG, e))?;
            if length != pixels.len() {
                return Err(Error::new(E_INVALIDARG, "pixel length mismatch"));
            }
            let packed = image
                .width
                .checked_mul(4)
                .ok_or_else(|| Error::new(E_INVALIDARG, "image size overflow"))?;
            if image.stride != packed {
                return Err(Error::new(E_INVALIDARG, "gpu image must be tightly packed"));
            }
            if acquire_key == 0 {
                return Err(Error::new(
                    E_INVALIDARG,
                    "acquire key must differ from the release key",
                ));
            }
            let desc = D3D11_TEXTURE2D_DESC {
                Width: image.width,
                Height: image.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: dxgi_format(image.format),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE
                    | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX)
                    .0 as u32,
                ..Default::default()
            };
            let mut texture = None;
            gpu.device
                .CreateTexture2D(&desc, None, Some(&mut texture))?;
            let texture = texture.ok_or_else(|| Error::from(E_UNEXPECTED))?;
            let keyed: IDXGIKeyedMutex = texture.cast()?;
            // The surface is created unowned; acquiring key 0 cannot block.
            acquire(&keyed, 0, 0)?;
            gpu.context.UpdateSubresource(
                &texture,
                0,
                None,
                pixels.as_ptr().cast(),
                image.stride,
                0,
            );
            release(&keyed, acquire_key)?;
            let resource: IDXGIResource1 = texture.cast()?;
            let handle = resource.CreateSharedHandle(
                None,
                DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                &HSTRING::from(&name),
            )?;
            let luid = gpu.dxgi.GetAdapter()?.GetDesc()?.AdapterLuid;
            Ok(Self {
                name,
                adapter_luid: adapter_luid_key(luid),
                image,
                texture,
                keyed,
                handle,
            })
        }
    }
    /// Name consumers pass to `ID3D11Device1::OpenSharedResourceByName`.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Adapter the texture was created on; consumers must open it there.
    pub fn adapter_luid(&self) -> u64 {
        self.adapter_luid
    }
    /// Descriptor of the published image.
    pub fn descriptor(&self) -> &ImageDescriptor {
        &self.image
    }
    /// The published texture. The producer may render into it after
    /// [`Self::acquire_from_consumer`] and must release it again before
    /// consumers read it.
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }
    /// Acquires the image back from a consumer, so the producer can rewrite it.
    /// The caller must release it again with the acquire key it published.
    pub fn acquire_from_consumer(&self, release_key: u64, timeout_ms: u32) -> Result<()> {
        acquire(&self.keyed, release_key, timeout_ms)
    }
    /// Releases a rewritten image to consumers with `acquire_key`.
    pub fn release_to_consumer(&self, acquire_key: u64) -> Result<()> {
        release(&self.keyed, acquire_key)
    }
}

impl Drop for SharedImage {
    fn drop(&mut self) {
        unsafe {
            // Closing the handle removes the name for future opens. The texture
            // and its keyed mutex are released with this struct; consumers
            // holding their own opened resource are unaffected.
            let _ = CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_session_local() {
        let a = unique_name();
        let b = unique_name();
        assert_ne!(a, b);
        assert!(a.starts_with("Local\\Snaplight.GpuImage."));
    }

    #[test]
    fn luid_round_trips_through_the_portable_key() {
        let luid = LUID {
            LowPart: 0xdead_beef,
            HighPart: 0x1234_5678,
        };
        assert_eq!(adapter_luid_key(luid), 0x1234_5678_dead_beef);
    }
}

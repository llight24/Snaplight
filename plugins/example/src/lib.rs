//! Diagnostic plugin used to exercise the ABI, cancellation, and both image
//! transports. It is not an OCR engine: it only reports a byte count and a
//! checksum.
//!
//! The shared-memory transport reads the borrowed CPU pixels the host mapped.
//! The GPU transport opens the named D3D11 shared texture on the adapter that
//! produced it, waits for the keyed-mutex key the request advertises, reads the
//! texture back into a staging resource, and hands the key back before it
//! returns. It never closes the producer's handle.

use snaplight_domain::{ImageDescriptor, PixelFormat};
use snaplight_plugin_api::*;
use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    slice,
};

/// Writes `payload` as the owned result buffer.
unsafe fn respond(out: *mut OwnedBytes, payload: &serde_json::Value) -> i32 {
    let bytes = serde_json::to_vec(payload)
        .unwrap_or_default()
        .into_boxed_slice();
    let len = bytes.len();
    *out = OwnedBytes {
        ptr: Box::into_raw(bytes) as *mut u8,
        len,
    };
    STATUS_OK
}

/// Reports a diagnostic message together with a failing status.
unsafe fn respond_error(out: *mut OwnedBytes, message: &str) -> i32 {
    respond(out, &serde_json::json!({ "error": message }));
    STATUS_ERROR
}

enum GpuError {
    Cancelled,
    Failed(String),
}

unsafe extern "C" fn run(
    request: BorrowedBytes,
    pixels: BorrowedBytes,
    cancel: *const Cancellation,
    out: *mut OwnedBytes,
) -> i32 {
    if out.is_null()
        || request.ptr.is_null()
        || cancel.is_null()
        || (pixels.len > 0 && pixels.ptr.is_null())
    {
        return STATUS_ERROR;
    }
    *out = OwnedBytes::default();
    catch_unwind(AssertUnwindSafe(|| {
        let cancel = &*cancel;
        let request: RunRequest =
            match serde_json::from_slice(slice::from_raw_parts(request.ptr, request.len)) {
                Ok(request) => request,
                Err(_) => return STATUS_ERROR,
            };
        if request.operation != "inspect" {
            return STATUS_ERROR;
        }
        if let Err(message) = request.validate() {
            return respond_error(out, message);
        }
        match &request.gpu {
            None => {
                let mut checksum = 0u64;
                for chunk in slice::from_raw_parts(pixels.ptr, pixels.len).chunks(4096) {
                    if (cancel.is_cancelled)(cancel.context) {
                        return STATUS_CANCELLED;
                    }
                    for &b in chunk {
                        checksum = checksum.wrapping_mul(31).wrapping_add(b as u64);
                    }
                }
                respond(
                    out,
                    &serde_json::json!({
                        "bytes": pixels.len,
                        "checksum": checksum,
                        "plugin": "example-inspect",
                        "transport": "shared-memory",
                    }),
                )
            }
            Some(gpu) => match inspect_shared_texture(gpu, &request.image, cancel) {
                Ok((checksum, bytes)) => respond(
                    out,
                    &serde_json::json!({
                        "bytes": bytes,
                        "checksum": checksum,
                        "plugin": "example-inspect",
                        "transport": "gpu",
                    }),
                ),
                Err(GpuError::Cancelled) => STATUS_CANCELLED,
                Err(GpuError::Failed(message)) => respond_error(out, &message),
            },
        }
    }))
    .unwrap_or(STATUS_ERROR)
}

#[cfg(windows)]
fn adapter_luid_key(luid: windows::Win32::Foundation::LUID) -> u64 {
    ((luid.HighPart as u32 as u64) << 32) | luid.LowPart as u64
}

#[cfg(windows)]
fn dxgi_format(format: PixelFormat) -> windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT {
    use windows::Win32::Graphics::Dxgi::Common::*;
    match format {
        PixelFormat::Bgra8 => DXGI_FORMAT_B8G8R8A8_UNORM,
        PixelFormat::Rgba8 => DXGI_FORMAT_R8G8B8A8_UNORM,
    }
}

#[cfg(windows)]
unsafe fn inspect_shared_texture(
    gpu: &GpuImage,
    image: &ImageDescriptor,
    cancel: &Cancellation,
) -> Result<(u64, usize), GpuError> {
    use windows::{
        core::*,
        Win32::{
            Foundation::*,
            Graphics::{
                Direct3D::*,
                Direct3D11::*,
                Dxgi::{Common::*, *},
            },
        },
    };
    let failed = |e: Error| GpuError::Failed(e.to_string());
    let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(failed)?;
    let mut adapter = None;
    for index in 0.. {
        let Ok(candidate) = factory.EnumAdapters(index) else {
            break;
        };
        if adapter_luid_key(candidate.GetDesc().map_err(failed)?.AdapterLuid) == gpu.adapter_luid {
            adapter = Some(candidate);
            break;
        }
    }
    let adapter =
        adapter.ok_or_else(|| GpuError::Failed("no adapter matches the shared image".into()))?;
    let mut device = None;
    let mut context = None;
    D3D11CreateDevice(
        &adapter,
        D3D_DRIVER_TYPE_UNKNOWN,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        None,
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        Some(&mut context),
    )
    .map_err(failed)?;
    let device = device.unwrap();
    let context = context.unwrap();
    let device1: ID3D11Device1 = device.cast().map_err(failed)?;
    let texture: ID3D11Texture2D = device1
        .OpenSharedResourceByName(
            &HSTRING::from(gpu.name.as_str()),
            DXGI_SHARED_RESOURCE_READ.0,
        )
        .map_err(failed)?;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    texture.GetDesc(&mut desc);
    if desc.Width != image.width
        || desc.Height != image.height
        || desc.ArraySize != 1
        || desc.MipLevels != 1
        || desc.Format != dxgi_format(image.format)
    {
        return Err(GpuError::Failed(
            "shared texture does not match the request descriptor".into(),
        ));
    }
    let keyed: IDXGIKeyedMutex = texture.cast().map_err(failed)?;
    wait_result(
        (Interface::vtable(&keyed).AcquireSync)(
            Interface::as_raw(&keyed),
            gpu.acquire_key,
            gpu.acquire_timeout_ms.min(u32::MAX as u64) as u32,
        ),
        "AcquireSync",
    )?;
    let read = (|| -> std::result::Result<(u64, bool), GpuError> {
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: image.width,
            Height: image.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: dxgi_format(image.format),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        device
            .CreateTexture2D(&staging_desc, None, Some(&mut staging))
            .map_err(failed)?;
        let staging = staging.unwrap();
        context.CopyResource(&staging, &texture);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(failed)?;
        let row_bytes = image.width as usize * 4;
        let mut checksum = 0u64;
        let mut cancelled = false;
        for row in 0..image.height as usize {
            if (cancel.is_cancelled)(cancel.context) {
                cancelled = true;
                break;
            }
            let start = mapped
                .pData
                .cast::<u8>()
                .add(row * mapped.RowPitch as usize);
            for &b in slice::from_raw_parts(start, row_bytes) {
                checksum = checksum.wrapping_mul(31).wrapping_add(b as u64);
            }
        }
        context.Unmap(&staging, 0);
        Ok((checksum, cancelled))
    })();
    // The key must go back to the producer on every path, including aborts.
    let released = wait_result(
        (Interface::vtable(&keyed).ReleaseSync)(Interface::as_raw(&keyed), gpu.release_key),
        "ReleaseSync",
    );
    let (checksum, cancelled) = read?;
    released?;
    if cancelled {
        return Err(GpuError::Cancelled);
    }
    Ok((checksum, image.width as usize * 4 * image.height as usize))
}

#[cfg(not(windows))]
unsafe fn inspect_shared_texture(
    _gpu: &GpuImage,
    _image: &ImageDescriptor,
    _cancel: &Cancellation,
) -> Result<(u64, usize), GpuError> {
    Err(GpuError::Failed(
        "gpu shared textures require a windows host".into(),
    ))
}

/// `AcquireSync` reports `WAIT_TIMEOUT` and `WAIT_ABANDONED` as non-negative
/// HRESULTs, which `windows` maps to `Ok`; they must be checked explicitly so a
/// missing or abandoned handshake is never read as success.
fn wait_result(hr: windows::core::HRESULT, operation: &str) -> Result<(), GpuError> {
    const WAIT_ABANDONED: i32 = 128;
    const WAIT_TIMEOUT: i32 = 258;
    if hr == windows::core::HRESULT(WAIT_TIMEOUT) {
        return Err(GpuError::Failed(format!(
            "{operation} timed out waiting for the shared image"
        )));
    }
    if hr == windows::core::HRESULT(WAIT_ABANDONED) {
        return Err(GpuError::Failed(format!(
            "{operation} found an abandoned shared image"
        )));
    }
    hr.ok().map_err(|e| GpuError::Failed(e.to_string()))
}

unsafe extern "C" fn free(bytes: OwnedBytes) {
    if !bytes.ptr.is_null() {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
            bytes.ptr, bytes.len,
        )));
    }
}
static API: PluginApi = PluginApi {
    abi_version: ABI_VERSION,
    struct_size: std::mem::size_of::<PluginApi>() as u32,
    capabilities: CAP_INSPECT | CAP_GPU_SHARED,
    run,
    free,
};
#[no_mangle]
pub extern "C" fn snaplight_plugin_entry() -> *const PluginApi {
    &API
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;
    unsafe extern "C" fn yes(_: *const c_void) -> bool {
        true
    }
    unsafe extern "C" fn no(_: *const c_void) -> bool {
        false
    }
    fn request() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "library": "unused.dll",
            "operation": "inspect",
            "image": {
                "id": 1,
                "width": 2,
                "height": 2,
                "stride": 8,
                "format": "Bgra8",
                "alpha": "Opaque",
                "color_space": "Srgb",
            },
            "mapping": "Local\\Snaplight.PluginTest",
            "bytes": 16,
            "timeout_ms": 1000,
        }))
        .unwrap()
    }
    #[test]
    fn inspection_and_cancel_release_owned_buffers() {
        unsafe {
            let request = request();
            let pixels = [1u8; 16];
            let input = BorrowedBytes {
                ptr: request.as_ptr(),
                len: request.len(),
            };
            let data = BorrowedBytes {
                ptr: pixels.as_ptr(),
                len: pixels.len(),
            };
            let mut out = OwnedBytes::default();
            let c = Cancellation {
                context: std::ptr::null(),
                is_cancelled: no,
            };
            assert_eq!(run(input, data, &c, &mut out), STATUS_OK);
            let result: serde_json::Value =
                serde_json::from_slice(slice::from_raw_parts(out.ptr, out.len)).unwrap();
            assert_eq!(result["bytes"], 16);
            assert_eq!(result["transport"], "shared-memory");
            free(out);
            let c = Cancellation {
                context: std::ptr::null(),
                is_cancelled: yes,
            };
            assert_eq!(run(input, data, &c, &mut out), STATUS_CANCELLED);
            assert!(out.ptr.is_null());
        }
    }
    #[test]
    fn invalid_json_and_null_output_are_rejected() {
        unsafe {
            let c = Cancellation {
                context: std::ptr::null(),
                is_cancelled: no,
            };
            let input = BorrowedBytes {
                ptr: b"x".as_ptr(),
                len: 1,
            };
            let mut out = OwnedBytes::default();
            assert_eq!(run(input, input, &c, &mut out), STATUS_ERROR);
            assert_eq!(run(input, input, &c, std::ptr::null_mut()), STATUS_ERROR);
        }
    }
    #[test]
    fn gpu_transport_without_a_cpu_mapping_is_accepted_but_not_reachable() {
        unsafe {
            // The GPU path must be described without CPU pixels; a plugin that
            // cannot open the texture reports a diagnostic instead of reading
            // the (empty) borrowed buffer.
            let request = serde_json::to_vec(&serde_json::json!({
                "library": "unused.dll",
                "operation": "inspect",
                "image": {
                    "id": 1,
                    "width": 2,
                    "height": 2,
                    "stride": 8,
                    "format": "Bgra8",
                    "alpha": "Opaque",
                    "color_space": "Srgb",
                },
                "mapping": "",
                "bytes": 0,
                "timeout_ms": 1000,
                "gpu": {
                    "name": "Local\\Snaplight.Missing",
                    "adapter_luid": 1,
                    "acquire_key": 1,
                    "release_key": 0,
                    "acquire_timeout_ms": 50,
                },
            }))
            .unwrap();
            let input = BorrowedBytes {
                ptr: request.as_ptr(),
                len: request.len(),
            };
            let empty = BorrowedBytes {
                ptr: std::ptr::null(),
                len: 0,
            };
            let mut out = OwnedBytes::default();
            let c = Cancellation {
                context: std::ptr::null(),
                is_cancelled: no,
            };
            assert_eq!(run(input, empty, &c, &mut out), STATUS_ERROR);
            let result: serde_json::Value =
                serde_json::from_slice(slice::from_raw_parts(out.ptr, out.len)).unwrap();
            assert!(result["error"].is_string(), "{result}");
            free(out);
        }
    }
}

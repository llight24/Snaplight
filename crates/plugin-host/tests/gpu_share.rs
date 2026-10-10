#![cfg(windows)]
//! End-to-end checks of the GPU shared-texture transport. The producer lives in
//! this test process, while the host and the plugin that opens the texture live
//! in a child process, so the named handle and the keyed mutex cross a real
//! process boundary.
mod common;
use common::{example_plugin, run};
use snaplight_domain::*;
use snaplight_plugin_api::*;
use snaplight_render_win::{gpu_share::SharedImage, Gpu};
use windows::Win32::{
    Graphics::Direct3D11::D3D11_TEXTURE2D_DESC,
    Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
    System::Com::{CoInitializeEx, COINIT_MULTITHREADED},
};

fn image() -> ImageDescriptor {
    ImageDescriptor {
        id: ImageId(11),
        width: 64,
        height: 48,
        stride: 64 * 4,
        format: PixelFormat::Bgra8,
        alpha: AlphaMode::Opaque,
        color_space: ColorSpace::Srgb,
    }
}

fn pixels(image: &ImageDescriptor) -> Vec<u8> {
    (0..image.byte_len().unwrap())
        .map(|i| (i * 31 % 251) as u8)
        .collect()
}

fn checksum(bytes: &[u8]) -> u64 {
    let mut checksum = 0u64;
    for &b in bytes {
        checksum = checksum.wrapping_mul(31).wrapping_add(b as u64);
    }
    checksum
}

fn gpu_request(library: String, image: ImageDescriptor, gpu: GpuImage) -> RunRequest {
    RunRequest {
        library,
        operation: "inspect".into(),
        image,
        mapping: String::new(),
        bytes: 0,
        timeout_ms: 10_000,
        gpu: Some(gpu),
    }
}

#[test]
fn gpu_shared_texture_transport_reads_the_producer_pixels() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let gpu = Gpu::new(None).expect("D3D11 device");
    let image = image();
    let pixels = pixels(&image);
    let name = snaplight_render_win::gpu_share::unique_name();
    let shared = SharedImage::publish(&gpu, name.clone(), image.clone(), &pixels, GPU_ACQUIRE_KEY)
        .expect("publish shared image");
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { shared.texture().GetDesc(&mut desc) };
    assert_eq!(
        (desc.Width, desc.Height, desc.Format),
        (image.width, image.height, DXGI_FORMAT_B8G8R8A8_UNORM)
    );
    assert_eq!(shared.descriptor().stride, image.stride);
    let request = gpu_request(
        example_plugin().to_string_lossy().into_owned(),
        image.clone(),
        GpuImage {
            name: shared.name().into(),
            adapter_luid: shared.adapter_luid(),
            acquire_key: GPU_ACQUIRE_KEY,
            release_key: GPU_RELEASE_KEY,
            acquire_timeout_ms: 5_000,
        },
    );
    let output = run(&Envelope::new(request));
    assert!(
        output.status.success(),
        "host failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Envelope<RunResponse> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        response.payload.status, STATUS_OK,
        "plugin failed: {}",
        response.payload.result
    );
    let result: serde_json::Value = serde_json::from_str(&response.payload.result).unwrap();
    assert_eq!(result["transport"], "gpu");
    assert_eq!(result["bytes"], pixels.len());
    assert_eq!(result["checksum"], checksum(&pixels));
    // The consumer must hand the keyed mutex back, otherwise the producer can
    // never rewrite the image.
    shared
        .acquire_from_consumer(GPU_RELEASE_KEY, 5_000)
        .expect("plugin released the shared image");
    shared.release_to_consumer(GPU_ACQUIRE_KEY).unwrap();
    drop(shared);
    // Dropping the producer closes the named handle, so the name is reusable.
    let republished = SharedImage::publish(&gpu, name, image, &pixels, GPU_ACQUIRE_KEY);
    assert!(
        republished.is_ok(),
        "producer kept the shared handle: {:?}",
        republished.err().map(|e| e.to_string())
    );
}

#[test]
fn gpu_request_that_also_sends_a_cpu_mapping_is_rejected() {
    let mut request = gpu_request(
        "missing.dll".into(),
        image(),
        GpuImage {
            name: "Local\\Snaplight.Nonexistent.GpuTest".into(),
            adapter_luid: 0x1234,
            acquire_key: GPU_ACQUIRE_KEY,
            release_key: GPU_RELEASE_KEY,
            acquire_timeout_ms: 1000,
        },
    );
    request.mapping = "Local\\Snaplight.Nonexistent.GpuTest".into();
    request.bytes = 4;
    let output = run(&Envelope::new(request));
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("gpu image cannot include a cpu mapping")
    );
}

#![cfg(windows)]
mod common;
use common::run;
use snaplight_domain::*;
use snaplight_plugin_api::*;
fn request() -> RunRequest {
    RunRequest {
        library: "missing.dll".into(),
        operation: "inspect".into(),
        image: ImageDescriptor {
            id: ImageId(1),
            width: 1,
            height: 1,
            stride: 4,
            format: PixelFormat::Bgra8,
            alpha: AlphaMode::Opaque,
            color_space: ColorSpace::Srgb,
        },
        mapping: "Local\\Snaplight.Nonexistent.Test".into(),
        bytes: 4,
        timeout_ms: 50,
        gpu: None,
    }
}
#[test]
fn incompatible_protocol_is_rejected_before_mapping() {
    let output = run(&Envelope {
        version: 99,
        payload: request(),
    });
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("IPC version mismatch"));
}
#[test]
fn inconsistent_image_size_is_rejected_before_mapping() {
    let mut request = request();
    request.bytes = 16;
    let output = run(&Envelope::new(request));
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("mapping/image size mismatch"));
}
#[test]
fn failed_library_load_does_not_crash_the_caller() {
    let mut request = request();
    request.mapping = format!("Local\\Snaplight.HostTest.{}", std::process::id());
    let pixels =
        snaplight_platform::native::SharedPixels::create(request.mapping.clone(), &[0; 4]).unwrap();
    let output = run(&Envelope::new(request));
    assert!(!output.status.success());
    assert!(!output.stderr.is_empty());
    assert_eq!(pixels.bytes(), &[0; 4]);
}

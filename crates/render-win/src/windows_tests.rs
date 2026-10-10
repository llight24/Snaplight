use super::*;
#[test]
fn device_error_classification() {
    // Device loss and a lost D2D target must trigger a rebuild; unrelated failures must not.
    for recoverable in [
        DXGI_ERROR_DEVICE_REMOVED,
        DXGI_ERROR_DEVICE_RESET,
        D2DERR_RECREATE_TARGET,
    ] {
        let error = Error::new(recoverable, "device error");
        assert!(
            is_recoverable_device_error(&error),
            "{:#x} should request a rebuild",
            recoverable.0
        );
    }
    let unrelated = Error::new(E_INVALIDARG, "bad argument");
    assert!(!is_recoverable_device_error(&unrelated));
}
#[test]
fn hdr_tone_map_keeps_luminance_order_and_opaque_alpha() {
    unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        )
        .ok()
        .unwrap();
        let gpu = Gpu::new(None).unwrap();
        let dc = gpu.dc().unwrap();
        let pixels: [u16; 12] = [
            0x3000, 0x3000, 0x3000, 0x3c00, 0x3800, 0x3800, 0x3800, 0x3c00, 0x4400, 0x4400, 0x4400,
            0x3c00,
        ];
        let mut input_props = properties(D2D1_BITMAP_OPTIONS_NONE, D2D1_ALPHA_MODE_IGNORE);
        input_props.pixelFormat.format = DXGI_FORMAT_R16G16B16A16_FLOAT;
        let size = D2D_SIZE_U {
            width: 3,
            height: 1,
        };
        let input = dc
            .CreateBitmap(size, Some(pixels.as_ptr().cast()), 24, &input_props)
            .unwrap();
        let image =
            hdr_to_sdr(&dc, &input, DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709, 1000.0).unwrap();
        let target = dc
            .CreateBitmap(
                size,
                None,
                0,
                &properties(D2D1_BITMAP_OPTIONS_TARGET, D2D1_ALPHA_MODE_PREMULTIPLIED),
            )
            .unwrap();
        dc.SetTarget(&target);
        dc.BeginDraw();
        dc.Clear(Some(&D2D1_COLOR_F {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        }));
        dc.DrawImage(
            &image,
            None,
            None,
            D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
            D2D1_COMPOSITE_MODE_SOURCE_OVER,
        );
        dc.EndDraw(None, None).unwrap();
        dc.SetTarget(None);
        let read = dc
            .CreateBitmap(
                size,
                None,
                0,
                &properties(
                    D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                    D2D1_ALPHA_MODE_PREMULTIPLIED,
                ),
            )
            .unwrap();
        read.CopyFromBitmap(None, &target, None).unwrap();
        let mapped = read.Map(D2D1_MAP_OPTIONS_READ).unwrap();
        let bytes = std::slice::from_raw_parts(mapped.bits, 12);
        assert!(
            bytes[2] < bytes[6] && bytes[6] < bytes[10],
            "tone-mapped luminance is not monotonic: {bytes:?}"
        );
        assert_eq!([bytes[3], bytes[7], bytes[11]], [255; 3]);
        read.Unmap().unwrap();
        drop(read);
        drop(target);
        drop(image);
        drop(input);
        drop(dc);
        drop(gpu);
        windows::Win32::System::Com::CoUninitialize();
    }
}

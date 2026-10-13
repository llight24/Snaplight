//! Writes the artefacts a finished capture leaves behind: the PNG export, the
//! clipboard bitmap and the pixel/document bundles that a normal capture, a
//! finished long capture and a pin all share.

use snaplight_domain::{Document, Envelope, ImageDescriptor, PinCommand, ScreenPoint};
use snaplight_platform::{atomic_write, write_pixels};
use std::{
    io::Write,
    time::{SystemTime, UNIX_EPOCH},
};
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{DataExchange::*, Memory::*},
        UI::WindowsAndMessaging::*,
    },
};

pub(super) fn write_png(
    path: &std::path::Path,
    image: &ImageDescriptor,
    bytes: &[u8],
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut rgba = bytes.to_vec();
    for p in rgba.as_chunks_mut::<4>().0.iter_mut() {
        p.swap(0, 2);
        p[3] = 255;
    }
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(file, image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&rgba)?;
    Ok(())
}
/// Writes a Slint window snapshot, which is already RGBA8.
pub(super) fn write_rgba_png(
    path: &std::path::Path,
    width: u32,
    height: u32,
    bytes: &[u8],
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(bytes)?;
    Ok(())
}
pub(super) fn clipboard(hwnd: HWND, image: &ImageDescriptor, bytes: &[u8]) -> Result<()> {
    unsafe {
        OpenClipboard(hwnd)?;
        let result = (|| {
            EmptyClipboard()?;
            let size = std::mem::size_of::<BITMAPINFOHEADER>();
            let memory = GlobalAlloc(GMEM_MOVEABLE, size + bytes.len())?;
            let ptr = GlobalLock(memory);
            if ptr.is_null() {
                let _ = GlobalFree(memory);
                return Err(Error::from_win32());
            }
            let header = BITMAPINFOHEADER {
                biSize: size as u32,
                biWidth: image.width as i32,
                biHeight: -(image.height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: bytes.len() as u32,
                ..Default::default()
            };
            std::ptr::copy_nonoverlapping(
                (&header as *const BITMAPINFOHEADER).cast::<u8>(),
                ptr.cast(),
                size,
            );
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>().add(size), bytes.len());
            let _ = GlobalUnlock(memory);
            if let Err(e) = SetClipboardData(8, HANDLE(memory.0)) {
                let _ = GlobalFree(memory);
                return Err(e);
            }
            Ok(())
        })();
        let closed = CloseClipboard();
        result?;
        closed
    }
}
/// Writes the two files every saved capture shares: the pristine pixels and the
/// document that describes them.
pub(super) fn write_capture_source(
    stem: &std::path::Path,
    document: &Document,
    original: &[u8],
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    write_pixels(
        &stem.with_extension("source.bgra"),
        &document.image,
        original,
    )?;
    atomic_write(
        &stem.with_extension("document.json"),
        &serde_json::to_vec_pretty(document)?,
    )?;
    Ok(())
}

/// Writes a pin bundle into `folder` — the pristine pixels, the document, the
/// flattened image and the pin document — and hands the pin to the pins process,
/// which takes over and quits this editor. A normal capture and a finished long
/// capture both go through here, so the two leave the same library behind.
pub(super) fn write_pin_bundle(
    folder: &std::path::Path,
    document: &Document,
    original: &[u8],
    image: ImageDescriptor,
    bytes: &[u8],
    screen: ScreenPoint,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(folder)?;
    let key = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let stem = folder.join(format!("{key}"));
    write_capture_source(&stem, document, original)?;
    let path = stem.with_extension("bgra");
    write_pixels(&path, &image, bytes)?;
    let flattened = Document {
        version: 1,
        image,
        selection: None,
        annotations: vec![],
    };
    let doc_path = stem.with_extension("pin.json");
    atomic_write(&doc_path, &serde_json::to_vec(&flattened)?)?;
    println!(
        "{}",
        serde_json::to_string(&Envelope::new(PinCommand::Open {
            image: path,
            document: doc_path,
            screen,
        }))?
    );
    std::io::stdout().flush()?;
    unsafe {
        PostQuitMessage(0);
    }
    Ok(())
}

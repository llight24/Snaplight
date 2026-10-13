use snaplight_domain::{Envelope, PROTOCOL_VERSION};
use snaplight_plugin_api::*;
use std::{
    ffi::c_void,
    io::{self, BufRead, Read},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
unsafe extern "C" fn cancelled(context: *const c_void) -> bool {
    (*(context as *const AtomicBool)).load(Ordering::Acquire)
}
#[cfg(windows)]
pub(super) fn execute() -> Result<(), Box<dyn std::error::Error>> {
    let mut line = String::new();
    io::stdin().lock().take(65537).read_line(&mut line)?;
    if line.len() > 65536 {
        return Err("request exceeds limit".into());
    }
    let envelope: Envelope<RunRequest> = serde_json::from_str(&line)?;
    if envelope.version != PROTOCOL_VERSION {
        return Err("IPC version mismatch".into());
    }
    let req = envelope.payload;
    req.validate()?;
    let mapping = match &req.gpu {
        // The GPU transport carries no CPU mapping; the plugin opens the named
        // shared texture itself.
        Some(_) => None,
        None => Some(snaplight_platform::native::SharedPixels::open(
            req.mapping.clone(),
            req.bytes,
        )?),
    };
    let library = unsafe { libloading::Library::new(&req.library)? };
    let entry: libloading::Symbol<Entry> = unsafe { library.get(b"snaplight_plugin_entry\0")? };
    let ptr = unsafe { entry() };
    if ptr.is_null() {
        return Err("null plugin API".into());
    }
    // Read the fixed header before accessing the version-specific function table.
    let version = unsafe { std::ptr::read(ptr.cast::<u32>()) };
    let size = unsafe { std::ptr::read(ptr.cast::<u32>().add(1)) };
    if version != ABI_VERSION || size < (std::mem::size_of::<PluginApi>() as u32) {
        return Err("plugin ABI mismatch".into());
    }
    let api = unsafe { &*ptr };
    let capability = match req.operation.as_str() {
        "inspect" => CAP_INSPECT,
        "ocr" => CAP_OCR,
        "scroll-capture" => CAP_SCROLL_CAPTURE,
        "record" => CAP_RECORD,
        _ => return Err("unknown operation".into()),
    };
    if api.capabilities & capability == 0 {
        return Err("plugin does not support operation".into());
    }
    if req.gpu.is_some() && api.capabilities & CAP_GPU_SHARED == 0 {
        return Err("plugin does not support gpu-shared transport".into());
    }
    let flag = std::sync::Arc::new(AtomicBool::new(false));
    let timeout = req.timeout_ms.clamp(1, 300_000);
    let timer_flag = flag.clone();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let watchdog = std::thread::spawn(move || {
        if done_rx
            .recv_timeout(Duration::from_millis(timeout))
            .is_err()
        {
            timer_flag.store(true, Ordering::Release);
            if done_rx.recv_timeout(Duration::from_secs(2)).is_err() {
                std::process::exit(124);
            }
        }
    });
    let cancellation = Cancellation {
        context: std::sync::Arc::as_ptr(&flag) as _,
        is_cancelled: cancelled,
    };
    let request = serde_json::to_vec(&req)?;
    let pixels = match &mapping {
        Some(mapping) => BorrowedBytes {
            ptr: mapping.bytes().as_ptr(),
            len: req.bytes,
        },
        None => BorrowedBytes {
            ptr: std::ptr::null(),
            len: 0,
        },
    };
    let mut out = OwnedBytes::default();
    let status = unsafe {
        (api.run)(
            BorrowedBytes {
                ptr: request.as_ptr(),
                len: request.len(),
            },
            pixels,
            &cancellation,
            &mut out,
        )
    };
    let _ = done_tx.send(());
    let _ = watchdog.join();
    if out.len > 16 * 1024 * 1024 || (out.len > 0 && out.ptr.is_null()) {
        return Err("invalid plugin result buffer".into());
    }
    let result = if out.len == 0 {
        String::new()
    } else {
        let bytes = unsafe { std::slice::from_raw_parts(out.ptr, out.len) };
        String::from_utf8_lossy(bytes).into_owned()
    };
    unsafe { (api.free)(out) };
    println!(
        "{}",
        serde_json::to_string(&Envelope::new(RunResponse { status, result }))?
    );
    Ok(())
}
#[cfg(not(windows))]
pub(super) fn execute() -> Result<(), Box<dyn std::error::Error>> {
    Err("shared-memory backend not implemented on this platform".into())
}

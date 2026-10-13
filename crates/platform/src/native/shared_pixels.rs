//! Named shared-memory buffer carrying a pixel payload between processes: the
//! capture side (or plugin host caller) creates it read-write, the receiver
//! opens it read-only.

use std::ptr::NonNull;
use windows::{
    core::*,
    Win32::{Foundation::*, System::Memory::*},
};

/// A mapped view of a named shared-memory section.
pub struct SharedPixels {
    handle: HANDLE,
    address: NonNull<u8>,
    len: usize,
    name: String,
}

impl SharedPixels {
    pub fn create(name: String, bytes: &[u8]) -> Result<Self> {
        unsafe {
            if bytes.is_empty() {
                return Err(Error::new(E_INVALIDARG, "empty shared image"));
            }
            let n = bytes.len() as u64;
            let handle = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                (n >> 32) as u32,
                n as u32,
                &HSTRING::from(&name),
            )?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(handle);
                return Err(Error::new(E_FAIL, "mapping name collision"));
            }
            let view = MapViewOfFile(handle, FILE_MAP_WRITE, 0, 0, bytes.len());
            let Some(address) = NonNull::new(view.Value as *mut u8) else {
                let _ = CloseHandle(handle);
                return Err(Error::from_win32());
            };
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), address.as_ptr(), bytes.len());
            Ok(Self {
                handle,
                address,
                len: bytes.len(),
                name,
            })
        }
    }
    pub fn open(name: String, len: usize) -> Result<Self> {
        unsafe {
            if len == 0 || len > 1024 * 1024 * 1024 {
                return Err(Error::new(E_INVALIDARG, "invalid mapping size"));
            }
            let handle = OpenFileMappingW(FILE_MAP_READ.0, false, &HSTRING::from(&name))?;
            let view = MapViewOfFile(handle, FILE_MAP_READ, 0, 0, len);
            let Some(address) = NonNull::new(view.Value as *mut u8) else {
                let _ = CloseHandle(handle);
                return Err(Error::from_win32());
            };
            Ok(Self {
                handle,
                address,
                len,
                name,
            })
        }
    }
    pub fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.address.as_ptr(), self.len) }
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn name(&self) -> &str {
        &self.name
    }
}
impl Drop for SharedPixels {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.address.as_ptr() as _,
            });
            let _ = CloseHandle(self.handle);
        }
    }
}

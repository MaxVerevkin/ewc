use std::ffi::CStr;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::{Error, Fourcc, Result};

#[derive(Debug)]
pub struct Device {
    raw: *mut gbm_sys::gbm_device,
    _fd: Option<OwnedFd>,
}

impl Device {
    pub fn open(path: &CStr) -> io::Result<Self> {
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        let raw = unsafe { gbm_sys::gbm_create_device(fd.as_raw_fd()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }

        Ok(Self { raw, _fd: Some(fd) })
    }

    pub fn with_drm_fd(fd: RawFd) -> io::Result<Self> {
        let raw = unsafe { gbm_sys::gbm_create_device(fd) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { raw, _fd: None })
    }

    pub fn as_raw(&self) -> *mut gbm_sys::gbm_device {
        self.raw
    }

    pub fn alloc_buffer(
        &self,
        width: u32,
        height: u32,
        fourcc: Fourcc,
        modifiers: &[u64],
        scan_out: bool,
    ) -> Result<Buffer> {
        let mut flags = gbm_sys::gbm_bo_flags::GBM_BO_USE_RENDERING;
        if scan_out {
            flags |= gbm_sys::gbm_bo_flags::GBM_BO_USE_SCANOUT;
        }
        let ptr = unsafe {
            gbm_sys::gbm_bo_create_with_modifiers2(
                self.raw,
                width,
                height,
                fourcc.0,
                modifiers.as_ptr(),
                modifiers.len() as u32,
                flags,
            )
        };
        if ptr.is_null() {
            Err(Error::BadGbmAlloc)
        } else {
            Ok(Buffer(ptr))
        }
    }

    pub fn is_format_supported(&self, fourcc: Fourcc) -> bool {
        unsafe {
            gbm_sys::gbm_device_is_format_supported(
                self.raw,
                fourcc.0,
                gbm_sys::gbm_bo_flags::GBM_BO_USE_RENDERING,
            ) != 0
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe { gbm_sys::gbm_device_destroy(self.raw) };
    }
}

#[derive(Debug)]
pub struct Buffer(*mut gbm_sys::gbm_bo);

impl Buffer {
    pub fn export(&self) -> BufferExport {
        let width = unsafe { gbm_sys::gbm_bo_get_width(self.0) };
        let height = unsafe { gbm_sys::gbm_bo_get_height(self.0) };
        let num_planes = unsafe { gbm_sys::gbm_bo_get_plane_count(self.0) };
        let modifier = unsafe { gbm_sys::gbm_bo_get_modifier(self.0) };
        let format = unsafe { gbm_sys::gbm_bo_get_format(self.0) };
        let mut planes = Vec::with_capacity(num_planes as usize);

        for i in 0..num_planes {
            let fd = unsafe { gbm_sys::gbm_bo_get_fd_for_plane(self.0, i) };
            let offset = unsafe { gbm_sys::gbm_bo_get_offset(self.0, i) };
            let stride = unsafe { gbm_sys::gbm_bo_get_stride_for_plane(self.0, i) };
            let handle = unsafe { gbm_sys::gbm_bo_get_handle_for_plane(self.0, i).u32_ };

            assert!(fd >= 0);

            planes.push(BufferPlane {
                dmabuf: unsafe { OwnedFd::from_raw_fd(fd) },
                handle,
                offset,
                stride,
            });
        }

        BufferExport {
            width,
            height,
            modifier,
            format: Fourcc(format),
            planes,
        }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        unsafe { gbm_sys::gbm_bo_destroy(self.0) };
    }
}

#[derive(Debug)]
pub struct BufferExport {
    pub width: u32,
    pub height: u32,
    pub format: Fourcc,
    pub modifier: u64,
    pub planes: Vec<BufferPlane>,
}

#[derive(Debug)]
pub struct BufferPlane {
    pub dmabuf: OwnedFd,
    pub handle: u32,
    pub offset: u32,
    pub stride: u32,
}

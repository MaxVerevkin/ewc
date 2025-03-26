use libc::dev_t;
use std::ffi::{CStr, c_int};
use std::io;

// use crate::xf86drm_ffi;

/// A DRM device
///
/// # Intended usage
///
/// `zwp_linux_dmabuf_feedback_v1` advertises devices using `dev_t` integers. Use this struct to
/// compare devices (do not compare raw `dev_t` values) and get node paths, which should be passed
/// to [`EglDisplay::new`](crate::EglDisplay::new).
pub struct DrmDevice(ffi::drmDevicePtr);

impl DrmDevice {
    /// Try to create DRM device from its `dev_t`
    pub fn new_from_id(id: dev_t) -> io::Result<Self> {
        let mut dev_ptr = std::ptr::null_mut();
        let result = unsafe { ffi::drmGetDeviceFromDevId(id, 0, &mut dev_ptr) };
        if result < 0 {
            Err(io::Error::from_raw_os_error(-result as _))
        } else {
            assert!(!dev_ptr.is_null());
            Ok(Self(dev_ptr))
        }
    }

    /// Get a render node path, if supported.
    pub fn render_node(&self) -> Option<&CStr> {
        self.get_node(ffi::DRM_NODE_RENDER)
    }

    fn get_node(&self, node: c_int) -> Option<&CStr> {
        if self.as_ref().available_nodes & (1 << node) == 0 {
            None
        } else {
            Some(unsafe { CStr::from_ptr(*self.as_ref().nodes.offset(node as isize)) })
        }
    }

    fn as_ref(&self) -> &ffi::drmDevice {
        unsafe { &*self.0 }
    }
}

impl Drop for DrmDevice {
    fn drop(&mut self) {
        unsafe {
            ffi::drmFreeDevice(&mut self.0);
        }
    }
}

impl PartialEq for DrmDevice {
    fn eq(&self, other: &Self) -> bool {
        unsafe { ffi::drmDevicesEqual(self.0, other.0) != 0 }
    }
}

impl Eq for DrmDevice {}

mod ffi {
    #![allow(non_camel_case_types)]

    use std::ffi::{c_char, c_int};

    pub const DRM_NODE_RENDER: c_int = 2;

    #[derive(Copy, Clone)]
    #[repr(C)]
    pub struct drmDevice {
        pub nodes: *mut *mut c_char,
        pub available_nodes: c_int,
        // some fields omitted
    }

    pub type drmDevicePtr = *mut drmDevice;

    unsafe extern "C" {
        pub fn drmGetDeviceFromDevId(
            dev_id: libc::dev_t,
            flags: u32,
            device: *mut drmDevicePtr,
        ) -> c_int;

        pub fn drmFreeDevice(device: *mut drmDevicePtr);

        pub fn drmDevicesEqual(a: drmDevicePtr, b: drmDevicePtr) -> c_int;
    }
}

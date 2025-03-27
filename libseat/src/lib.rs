use libseat_sys as sys;

use std::{
    cell::RefCell,
    collections::VecDeque,
    ffi::{CString, c_void},
    io,
    os::{
        fd::{FromRawFd, OwnedFd},
        unix::io::BorrowedFd,
    },
    path::Path,
    ptr::NonNull,
};

extern "C" fn enable_seat(_seat: *mut sys::libseat, data: *mut c_void) {
    let data = data.cast::<RefCell<VecDeque<Event>>>();
    let data = unsafe { &*data };
    data.borrow_mut().push_back(Event::Enable);
}

extern "C" fn disable_seat(_seat: *mut sys::libseat, data: *mut c_void) {
    let data = data.cast::<RefCell<VecDeque<Event>>>();
    let data = unsafe { &*data };
    data.borrow_mut().push_back(Event::Disable);
}

static mut FFI_SEAT_LISTENER: sys::libseat_seat_listener = sys::libseat_seat_listener {
    enable_seat: Some(enable_seat),
    disable_seat: Some(disable_seat),
};

#[derive(Debug, Clone, Copy)]
pub enum Event {
    Enable,
    Disable,
}

#[derive(Debug)]
pub struct Seat {
    ptr: NonNull<sys::libseat>,
    events: Box<RefCell<VecDeque<Event>>>,
}

impl Drop for Seat {
    fn drop(&mut self) {
        unsafe { sys::libseat_close_seat(self.ptr.as_ptr()) };
    }
}

impl Seat {
    /// Opens a seat, taking control of it if possible and returning a pointer to
    /// the libseat instance. If LIBSEAT_BACKEND is set, the specified backend is
    /// used. Otherwise, the first successful backend will be used.
    pub fn open() -> io::Result<Self> {
        let events = Box::new(RefCell::new(VecDeque::new()));

        let seat = unsafe {
            sys::libseat_open_seat(
                std::ptr::addr_of_mut!(FFI_SEAT_LISTENER),
                events.as_ref() as *const _ as *mut _,
            )
        };

        NonNull::new(seat)
            .map(|ptr| Self { ptr, events })
            .ok_or_else(io::Error::last_os_error)
    }

    pub fn next_event(&self) -> Option<Event> {
        self.events.borrow_mut().pop_front()
    }

    /// Disables a seat, used in response to a disable_seat event. After disabling
    /// the seat, the seat devices must not be used until enable_seat is received,
    /// and all requests on the seat will fail during this period.
    pub fn disable(&self) -> io::Result<()> {
        if unsafe { sys::libseat_disable_seat(self.ptr.as_ptr()) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Opens a device on the seat, returning its device ID and fd
    ///
    /// This will only succeed if the seat is active and the device is of a type
    /// permitted for opening on the backend, such as drm and evdev.
    ///
    /// The device may be revoked in some situations, such as in situations where a
    /// seat session switch is being forced.
    pub fn open_device<P: AsRef<Path>>(&self, path: P) -> io::Result<(OwnedFd, DeviceId)> {
        let path_bytes = path.as_ref().as_os_str().as_encoded_bytes();
        let cstring = CString::new(path_bytes).expect("path contains null bytes");

        let mut fd = -1;
        let id = unsafe { sys::libseat_open_device(self.ptr.as_ptr(), cstring.as_ptr(), &mut fd) };

        if id != -1 {
            assert_ne!(fd, -1);
            Ok((unsafe { OwnedFd::from_raw_fd(fd) }, DeviceId(id)))
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Closes a device that has been opened on the seat using the device_id from
    /// libseat_open_device.
    pub fn close_device(&self, device_id: DeviceId) -> io::Result<()> {
        if unsafe { sys::libseat_close_device(self.ptr.as_ptr(), device_id.0) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Retrieves the name of the seat that is currently made available through the
    /// provided libseat instance.
    pub fn name(&self) -> &str {
        unsafe {
            let cstr = sys::libseat_seat_name(self.ptr.as_ptr());
            let cstr = std::ffi::CStr::from_ptr(cstr as *const _);
            cstr.to_str().unwrap()
        }
    }

    /// Requests that the seat switches session to the specified session number.
    /// For seats that are VT-bound, the session number matches the VT number, and
    /// switching session results in a VT switch.
    ///
    /// A call to libseat_switch_session does not imply that a switch will occur,
    /// and the caller should assume that the session continues unaffected.
    pub fn switch_session(&self, session: i32) -> io::Result<()> {
        if unsafe { sys::libseat_switch_session(self.ptr.as_ptr(), session) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Retrieve the pollable connection fd for a given libseat instance. Used to
    /// poll the libseat connection for events that need to be dispatched.
    ///
    /// Returns a pollable fd on success.
    pub fn get_fd(&self) -> io::Result<BorrowedFd> {
        let fd = unsafe { sys::libseat_get_fd(self.ptr.as_ptr()) };
        if fd == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { BorrowedFd::borrow_raw(fd) })
        }
    }

    /// Reads and dispatches events on the libseat connection fd.
    ///
    /// The specified timeout dictates how long libseat might wait for data if none
    /// is available: 0 means that no wait will occur, -1 means that libseat might
    /// wait indefinitely for data to arrive, while > 0 is the maximum wait in
    /// milliseconds that might occur.
    ///
    /// Returns a positive number signifying processed internal messages on success.
    /// Returns 0 if no messages were processed. Returns -1 and sets errno on error.
    pub fn dispatch(&self, timeout: i32) -> io::Result<i32> {
        let v = unsafe { sys::libseat_dispatch(self.ptr.as_ptr(), timeout) };
        if v == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(v)
        }
    }
}

#[derive(Debug)]
#[must_use]
pub struct DeviceId(i32);

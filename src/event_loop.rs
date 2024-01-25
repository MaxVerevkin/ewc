use std::collections::HashMap;
use std::ffi::c_int;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::client::ClientId;

pub struct EventLoop {
    epoll: OwnedFd,
    next_id: u64,
    data_map: HashMap<u64, Event>,
    event_buf: [libc::epoll_event; 32],
    event_cnt: usize,
    event_head: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Event {
    Socket,
    Backend(u32),
    Quit,
    Client(ClientId),
    MayGoIdle,
}

impl EventLoop {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            epoll: epoll_create1(libc::EPOLL_CLOEXEC)?,
            next_id: 0,
            data_map: HashMap::new(),
            event_buf: unsafe { std::mem::zeroed() },
            event_cnt: 0,
            event_head: 0,
        })
    }

    pub fn add_fd(&mut self, fd: RawFd, event: Event) -> io::Result<()> {
        let mut epoll_event = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: self.next_id,
        };

        if unsafe {
            libc::epoll_ctl(
                self.epoll.as_raw_fd(),
                libc::EPOLL_CTL_ADD,
                fd,
                &mut epoll_event,
            )
        } == -1
        {
            return Err(io::Error::last_os_error());
        }

        self.data_map.insert(self.next_id, event);
        self.next_id = self.next_id.checked_add(1).unwrap();

        Ok(())
    }

    pub fn remove(&mut self, fd: RawFd) -> io::Result<()> {
        if unsafe {
            libc::epoll_ctl(
                self.epoll.as_raw_fd(),
                libc::EPOLL_CTL_DEL,
                fd,
                std::ptr::null_mut(),
            )
        } == -1
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn poll(&mut self) -> io::Result<Event> {
        loop {
            if self.event_cnt > 0 {
                let event = self.event_buf[self.event_head];
                let id = event.u64;
                self.event_cnt -= 1;
                self.event_head += 1;
                return Ok(*self.data_map.get(&id).unwrap());
            } else if self.event_head != 0 {
                self.event_head = 0;
                return Ok(Event::MayGoIdle);
            }

            let wait_result = unsafe {
                libc::epoll_wait(
                    self.epoll.as_raw_fd(),
                    self.event_buf.as_mut_ptr(),
                    self.event_buf.len() as i32,
                    -1,
                )
            };
            if wait_result == -1 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            assert_ne!(wait_result, 0, "timeout is -1, zero is impossible");

            self.event_cnt = wait_result as usize;
            self.event_head = 0;
        }
    }
}

fn epoll_create1(flags: c_int) -> io::Result<OwnedFd> {
    match unsafe { libc::epoll_create1(flags) } {
        -1 => Err(io::Error::last_os_error()),
        fd => Ok(unsafe { OwnedFd::from_raw_fd(fd) }),
    }
}

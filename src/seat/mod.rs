use std::cell::RefCell;
use std::io;

use crate::globals::{Global, IsGlobal};
use crate::protocol::*;
use crate::wayland_core::Proxy;
use crate::{Client, State};

mod keyboard;
pub mod pointer;

pub struct Seat {
    pub keyboard: keyboard::Keyboard,
    pub pointer: pointer::Pointer,
}

pub struct ClientSeat {
    pub keyboards: RefCell<Vec<WlKeyboard>>,
    pub pointers: RefCell<Vec<WlPointer>>,
}

impl Seat {
    pub fn global(name: u32) -> Global {
        Global::new::<WlSeat>(name, 5)
    }

    pub fn new() -> Self {
        Self {
            keyboard: keyboard::Keyboard::new(),
            pointer: pointer::Pointer::new(),
        }
    }

    pub fn unfocus_surface(&mut self, wl_surface: &WlSurface) {
        self.keyboard.unfocus_surface(wl_surface);
        self.pointer.unfocus_surface(wl_surface);
    }
}

impl ClientSeat {
    pub fn new() -> Self {
        Self {
            keyboards: RefCell::new(Vec::new()),
            pointers: RefCell::new(Vec::new()),
        }
    }
}

impl IsGlobal for WlSeat {
    fn on_bind(&self, _client: &mut Client, _state: &mut State) {
        self.capabilities(wl_seat::Capability::Keyboard | wl_seat::Capability::Pointer);
        self.set_callback(|ctx| {
            use wl_seat::Request;
            match ctx.request {
                Request::GetPointer(wl_pointer) => {
                    ctx.state.seat.pointer.init_pointer(&wl_pointer);
                    ctx.client.conn.seat.pointers.borrow_mut().push(wl_pointer);
                }
                Request::GetKeyboard(wl_keyboard) => {
                    ctx.state.seat.keyboard.init_keyboard(&wl_keyboard)?;
                    ctx.client
                        .conn
                        .seat
                        .keyboards
                        .borrow_mut()
                        .push(wl_keyboard);
                }
                Request::GetTouch(_) => {
                    return Err(io::Error::other("touch input not supporetd"));
                }
                Request::Release => (),
            }
            Ok(())
        });
    }
}

use std::io;
use std::rc::{Rc, Weak};

use crate::client::RequestCtx;
use crate::focus_stack::FocusStack;
use crate::globals::compositor::SurfaceRole;
use crate::globals::xdg_shell::XdgToplevelRole;
use crate::protocol::*;
use crate::wayland_core::{Fixed, Proxy};

// pub const BTN_MOUSE: u32 = 0x110;
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
// pub const BTN_MIDDLE: u32 = 0x112;
// pub const BTN_SIDE: u32 = 0x113;
// pub const BTN_EXTRA: u32 = 0x114;
// pub const BTN_FORWARD: u32 = 0x115;
// pub const BTN_BACK: u32 = 0x116;
// pub const BTN_TASK: u32 = 0x117;

pub struct Pointer {
    pub state: PtrState,
    pub x: f32,
    pub y: f32,
}

pub enum PtrState {
    None,
    Focused {
        surface: WlSurface,
        x: f32,
        y: f32,
    },
    Moving {
        toplevel: Weak<XdgToplevelRole>,
        ptr_start_x: f32,
        ptr_start_y: f32,
        toplevel_start_x: i32,
        toplevel_start_y: i32,
    },
    Resizing {
        toplevel: Weak<XdgToplevelRole>,
        edge: xdg_toplevel::ResizeEdge,
        ptr_start_x: f32,
        ptr_start_y: f32,
        toplevel_start_width: u32,
        toplevel_start_height: u32,
    },
}

impl Pointer {
    pub fn new() -> Self {
        Self {
            state: PtrState::None,
            x: 0.0,
            y: 0.0,
        }
    }

    pub fn init_pointer(&self, wl_pointer: &WlPointer) {
        wl_pointer.set_callback(wl_pointer_cb);
        if let Some(focused) = self.get_focused_surface() {
            if focused.is_alive() && focused.client_id() == wl_pointer.client_id() {
                self.enter(wl_pointer);
            }
        }
    }

    pub fn forward_pointer(&mut self, surface: Option<(WlSurface, f32, f32)>) {
        if let Some((surface, x, y)) = &surface {
            if let Some(fsurf) = self.get_focused_surface() {
                if surface == fsurf {
                    for ptr in surface.conn().seat.pointers.borrow().iter() {
                        ptr.motion(0, Fixed::from(*x), Fixed::from(*y));
                    }
                    return;
                }
            }
        }

        if let Some(old_surface) = self.get_focused_surface() {
            if old_surface.is_alive() {
                for ptr in old_surface.conn().seat.pointers.borrow().iter() {
                    ptr.leave(1, old_surface);
                }
            }
        }

        self.state = surface
            .map(|(surface, x, y)| PtrState::Focused { surface, x, y })
            .unwrap_or(PtrState::None);
        if let Some(new_surface) = self.get_focused_surface() {
            for ptr in new_surface.conn().seat.pointers.borrow().iter() {
                self.enter(ptr);
            }
        }
    }

    pub fn forward_btn(&mut self, btn: u32, pressed: bool) {
        let state = if pressed {
            wl_pointer::ButtonState::Pressed
        } else {
            wl_pointer::ButtonState::Released
        };
        if let Some(surface) = self.get_focused_surface() {
            if surface.is_alive() {
                for ptr in surface.conn().seat.pointers.borrow().iter() {
                    ptr.button(1, 0, btn, state);
                }
            } else {
                self.state = PtrState::None;
            }
        }
    }

    pub fn axis_vertical(&mut self, value: f32) {
        if let Some(surface) = self.get_focused_surface() {
            if surface.is_alive() {
                for ptr in surface.conn().seat.pointers.borrow().iter() {
                    if value != 0.0 {
                        ptr.axis(0, wl_pointer::Axis::VerticalScroll, Fixed::from(value));
                    }
                }
            } else {
                self.state = PtrState::None;
            }
        }
    }

    pub fn start_move(&mut self, focus_stack: &mut FocusStack, toplevel_i: Option<usize>) {
        let Some(toplevel_i) = toplevel_i.or_else(|| focus_stack.toplevel_at(self.x, self.y))
        else {
            return;
        };
        self.forward_pointer(None);
        let toplevel = focus_stack.get_i(toplevel_i).unwrap();
        self.state = PtrState::Moving {
            toplevel: Rc::downgrade(&toplevel),
            ptr_start_x: self.x,
            ptr_start_y: self.y,
            toplevel_start_x: toplevel.x.get(),
            toplevel_start_y: toplevel.y.get(),
        };
        focus_stack.focus_i(toplevel_i);
    }

    pub fn start_resize(
        &mut self,
        focus_stack: &mut FocusStack,
        edge: xdg_toplevel::ResizeEdge,
        toplevel_i: Option<usize>,
    ) {
        let Some(toplevel_i) = toplevel_i.or_else(|| focus_stack.toplevel_at(self.x, self.y))
        else {
            return;
        };
        self.forward_pointer(None);
        let toplevel = focus_stack.get_i(toplevel_i).unwrap();
        let start_geom = toplevel
            .xdg_surface
            .upgrade()
            .unwrap()
            .get_window_geometry()
            .unwrap();
        self.state = PtrState::Resizing {
            toplevel: Rc::downgrade(&toplevel),
            edge,
            ptr_start_x: self.x,
            ptr_start_y: self.y,
            toplevel_start_width: start_geom.width.get(),
            toplevel_start_height: start_geom.height.get(),
        };
        focus_stack.focus_i(toplevel_i);
    }

    fn enter(&self, wl_pointer: &WlPointer) {
        if let PtrState::Focused { surface, x, y } = &self.state {
            wl_pointer.enter(1, surface, Fixed::from(*x), Fixed::from(*y));
        }
    }

    pub fn get_focused_surface(&self) -> Option<&WlSurface> {
        match &self.state {
            PtrState::Focused { surface, .. } => Some(surface),
            _ => None,
        }
    }

    pub fn unfocus_surface(&mut self, wl_surface: &WlSurface) {
        if self.get_focused_surface() == Some(wl_surface) {
            self.forward_pointer(None);
        }
    }
}

fn wl_pointer_cb(ctx: RequestCtx<WlPointer>) -> io::Result<()> {
    use wl_pointer::Request;
    match ctx.request {
        Request::SetCursor(args) => match args.surface {
            None => ctx.state.cursor.hide(),
            Some(surf) => {
                let surface = ctx.client.compositor.surfaces.get(&surf).unwrap();
                match &mut *surface.role.borrow_mut() {
                    x @ SurfaceRole::None => *x = SurfaceRole::Cursor,
                    SurfaceRole::Cursor => (),
                    _ => return Err(io::Error::other("surface already has a role")),
                }
                ctx.state
                    .cursor
                    .set_surface(surface.clone(), args.hotspot_x, args.hotspot_y);
            }
        },
        Request::Release => {
            ctx.client
                .conn
                .seat
                .pointers
                .borrow_mut()
                .retain(|p| *p != ctx.proxy);
        }
    }
    Ok(())
}

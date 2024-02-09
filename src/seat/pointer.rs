use std::io;
use std::rc::{Rc, Weak};

use crate::backend::InputTimestamp;
use crate::client::RequestCtx;
use crate::globals::compositor::{Surface, SurfaceRole};
use crate::globals::xdg_shell::toplevel::XdgToplevelRole;
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
    pressed_buttons: Vec<u32>,
}

pub struct SurfacePointer {
    surface: Rc<Surface>,
    pressed_buttons: Vec<u32>,
    x: Fixed,
    y: Fixed,
}

pub enum PtrState {
    None,
    Entered(SurfacePointer),
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
            pressed_buttons: Vec::new(),
        }
    }

    pub fn init_new_resource(&self, wl_pointer: &WlPointer) {
        wl_pointer.set_callback(wl_pointer_cb);
        if let PtrState::Entered(sp) = &self.state {
            if sp.surface.wl.client_id() == wl_pointer.client_id() {
                wl_pointer.enter(1, &sp.surface.wl, sp.x, sp.y);
            }
        }
    }

    pub fn leave_any_surface(&mut self) {
        if let PtrState::Entered(sp) = &self.state {
            for ptr in sp.surface.wl.conn().seat.pointers.borrow().iter() {
                ptr.leave(1, &sp.surface.wl);
                if ptr.version() >= 5 {
                    ptr.frame();
                }
            }
        }
        self.state = PtrState::None;
    }

    pub fn forward_pointer(
        &mut self,
        surface: Rc<Surface>,
        timestamp: InputTimestamp,
        x: f32,
        y: f32,
    ) {
        let x = Fixed::from(x);
        let y = Fixed::from(y);

        if let PtrState::Entered(sp) = &mut self.state {
            if surface.wl == sp.surface.wl {
                for ptr in surface.wl.conn().seat.pointers.borrow().iter() {
                    ptr.motion(timestamp.get(), x, y);
                    if ptr.version() >= 5 {
                        ptr.frame()
                    }
                }
                return;
            }

            for ptr in sp.surface.wl.conn().seat.pointers.borrow().iter() {
                ptr.leave(1, &sp.surface.wl);
                if ptr.version() >= 5 {
                    ptr.frame();
                }
            }
        }

        self.state = PtrState::Entered(SurfacePointer {
            surface: surface.clone(),
            pressed_buttons: Vec::new(),
            x,
            y,
        });

        for ptr in surface.wl.conn().seat.pointers.borrow().iter() {
            ptr.enter(1, &surface.wl, x, y);
            if ptr.version() >= 5 {
                ptr.frame();
            }
        }
    }

    pub fn update_button(
        &mut self,
        btn: u32,
        timestamp: InputTimestamp,
        pressed: bool,
        forward: bool,
    ) {
        if pressed {
            self.pressed_buttons.push(btn);
        } else {
            self.pressed_buttons.retain(|x| *x != btn);
        }

        if forward {
            if let PtrState::Entered(sp) = &mut self.state {
                if pressed && !sp.pressed_buttons.contains(&btn) {
                    sp.pressed_buttons.push(btn);
                    for ptr in sp.surface.wl.conn().seat.pointers.borrow().iter() {
                        ptr.button(1, timestamp.get(), btn, wl_pointer::ButtonState::Pressed);
                        if ptr.version() >= 5 {
                            ptr.frame()
                        }
                    }
                } else if !pressed && sp.pressed_buttons.contains(&btn) {
                    sp.pressed_buttons.retain(|x| *x != btn);
                    for ptr in sp.surface.wl.conn().seat.pointers.borrow().iter() {
                        ptr.button(1, timestamp.get(), btn, wl_pointer::ButtonState::Released);
                        if ptr.version() >= 5 {
                            ptr.frame()
                        }
                    }
                }
            }
        }
    }

    pub fn number_of_pressed_buttons(&self) -> usize {
        self.pressed_buttons.len()
    }

    pub fn axis_vertical(&mut self, value: f32, timestamp: InputTimestamp) {
        if let Some(surface) = self.get_focused_surface() {
            for ptr in surface.wl.conn().seat.pointers.borrow().iter() {
                if value != 0.0 {
                    ptr.axis(
                        timestamp.get(),
                        wl_pointer::Axis::VerticalScroll,
                        Fixed::from(value),
                    );
                    if ptr.version() >= 5 {
                        ptr.frame()
                    }
                }
            }
        }
    }

    pub fn start_move(&mut self, toplevel: Rc<XdgToplevelRole>) {
        self.leave_any_surface();
        self.state = PtrState::Moving {
            toplevel: Rc::downgrade(&toplevel),
            ptr_start_x: self.x,
            ptr_start_y: self.y,
            toplevel_start_x: toplevel.x.get(),
            toplevel_start_y: toplevel.y.get(),
        };
    }

    pub fn start_resize(&mut self, edge: xdg_toplevel::ResizeEdge, toplevel: Rc<XdgToplevelRole>) {
        self.leave_any_surface();
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
    }

    pub fn get_focused_surface(&self) -> Option<Rc<Surface>> {
        match &self.state {
            PtrState::Entered(sp) => Some(sp.surface.clone()),
            _ => None,
        }
    }

    pub fn surface_unmapped(&mut self, wl_surface: &WlSurface) {
        let mut should_leave = false;
        match &self.state {
            PtrState::None => (),
            PtrState::Entered(surf) => should_leave = surf.surface.wl == *wl_surface,
            PtrState::Moving { toplevel, .. } | PtrState::Resizing { toplevel, .. } => {
                should_leave =
                    toplevel.upgrade().unwrap().wl_surface.upgrade().unwrap().wl == *wl_surface
            }
        }
        if should_leave {
            self.leave_any_surface();
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

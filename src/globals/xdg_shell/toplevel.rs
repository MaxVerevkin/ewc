use std::cell::{Cell, RefCell};
use std::ffi::CString;
use std::io;
use std::num::NonZeroU32;
use std::rc::{Rc, Weak};

use crate::client::RequestCtx;
use crate::globals::compositor::Surface;
use crate::protocol::xdg_toplevel::ResizeEdge;
use crate::State;
use crate::{protocol::*, Proxy};

use super::{SpecificRole, XdgSurfaceRole};

pub struct XdgToplevelRole {
    pub wl: XdgToplevel,
    pub xdg_surface: Weak<XdgSurfaceRole>,
    pub wl_surface: Weak<Surface>,

    pub x: Cell<i32>,
    pub y: Cell<i32>,
    resizing: Cell<Option<(ResizeEdge, i32, i32, u32)>>,

    cur_configure: Cell<ToplevelConfigure>,
    pending_configure: Cell<Option<ToplevelConfigure>>,

    cur: RefCell<XdgToplevelState>,
    pending: RefCell<XdgToplevelState>,
    dirty_app_id: Cell<bool>,
    dirty_title: Cell<bool>,
    dirty_min_size: Cell<bool>,
    dirty_max_size: Cell<bool>,
}

#[derive(Clone, Copy, Default)]
struct ToplevelConfigure {
    serial: u32,
    width: u32,
    heinght: u32,
    activated: bool,
}

impl XdgToplevelRole {
    pub fn new(xdg_toplevel: XdgToplevel, xdg_surface: &Rc<XdgSurfaceRole>) -> Self {
        xdg_toplevel.set_callback(xdg_toplevel_cb);
        Self {
            wl: xdg_toplevel,
            xdg_surface: Rc::downgrade(xdg_surface),
            wl_surface: xdg_surface.wl_surface.clone(),

            x: Cell::new(0),
            y: Cell::new(0),
            resizing: Cell::new(None),

            cur_configure: Cell::new(ToplevelConfigure::default()),
            pending_configure: Cell::new(None),

            cur: RefCell::new(XdgToplevelState::default()),
            pending: RefCell::new(XdgToplevelState::default()),
            dirty_app_id: Cell::new(false),
            dirty_title: Cell::new(false),
            dirty_min_size: Cell::new(false),
            dirty_max_size: Cell::new(false),
        }
    }

    pub fn apply_pending_configure(&self) {
        if let Some(configure) = self.pending_configure.take() {
            self.cur_configure.set(configure);
            let mut states = Vec::new();
            if configure.activated {
                states.extend_from_slice(&(xdg_toplevel::State::Activated as u32).to_ne_bytes());
            }
            self.wl
                .configure(configure.width as i32, configure.heinght as i32, states);
            self.xdg_surface
                .upgrade()
                .unwrap()
                .wl
                .configure(configure.serial);
        }
    }

    pub fn set_activated(&self, value: bool) {
        if self.cur_configure.get().activated != value {
            let mut configure = self.pending_configure.get().unwrap_or_else(|| {
                let mut conf = self.cur_configure.get();
                conf.serial += 1;
                conf
            });
            configure.activated = value;
            self.pending_configure.set(Some(configure));
        }
    }

    pub fn request_size(&self, edge: ResizeEdge, mut width: NonZeroU32, mut height: NonZeroU32) {
        if !self.wl_surface.upgrade().unwrap().mapped.get() {
            return;
        }

        let cur = self.cur.borrow();
        if let Some((max_w, max_h)) = cur.max_size {
            if max_w != 0 && width.get() > max_w {
                width = NonZeroU32::new(max_w).unwrap();
            }
            if max_h != 0 && height.get() > max_h {
                height = NonZeroU32::new(max_h).unwrap();
            }
        }
        if let Some((min_w, min_h)) = cur.min_size {
            if min_w != 0 && width.get() < min_w {
                width = NonZeroU32::new(min_w).unwrap();
            }
            if min_h != 0 && height.get() < min_h {
                height = NonZeroU32::new(min_h).unwrap();
            }
        }

        let mut configure = self.pending_configure.take().unwrap_or_else(|| {
            let mut c = self.cur_configure.get();
            c.serial += 1;
            c
        });

        configure.width = width.get();
        configure.heinght = height.get();
        let serial = configure.serial;
        self.pending_configure.set(Some(configure));

        match self.resizing.get() {
            None => {
                let geom = self
                    .xdg_surface
                    .upgrade()
                    .unwrap()
                    .get_window_geometry()
                    .unwrap();
                let (nx, ny) = geom.get_opposite_edge_point(edge);
                self.resizing
                    .set(Some((edge, nx + self.x.get(), ny + self.y.get(), serial)));
            }
            Some((oe, onx, ony, _oserial)) => {
                assert_eq!(oe, edge);
                self.resizing.set(Some((edge, onx, ony, serial)));
            }
        }
    }

    pub fn committed(self: &Rc<Self>, state: &mut State) -> io::Result<()> {
        if self.dirty_app_id.get() {
            self.dirty_app_id.set(false);
            self.cur.borrow_mut().app_id = std::mem::take(&mut self.pending.borrow_mut().app_id);
        }
        if self.dirty_title.get() {
            self.dirty_app_id.set(false);
            self.cur.borrow_mut().title = std::mem::take(&mut self.pending.borrow_mut().title);
        }
        if self.dirty_min_size.get() {
            self.dirty_min_size.set(false);
            self.cur.borrow_mut().min_size = self.pending.borrow_mut().min_size;
        }
        if self.dirty_max_size.get() {
            self.dirty_max_size.set(false);
            self.cur.borrow_mut().max_size = self.pending.borrow_mut().max_size;
        }

        let surface = self.wl_surface.upgrade().unwrap();
        let xdg_surface = self.xdg_surface.upgrade().unwrap();

        if !surface.configured.get() {
            if surface.cur.borrow().buffer.is_some() {
                return Err(io::Error::other("unmapped surface commited a buffer"));
            }
            let serial = self.cur_configure.get().serial + 1;
            self.wl.configure(0, 0, Vec::new());
            xdg_surface.wl.configure(serial);
            self.pending_configure.set(None);
            self.cur_configure.set(ToplevelConfigure {
                serial,
                width: 0,
                heinght: 0,
                activated: false,
            });
            surface.configured.set(true);
        } else if !surface.mapped.get() {
            if Some(self.cur_configure.get().serial) != xdg_surface.last_acked_configure.get() {
                return Err(io::Error::other("did not ack the initial config"));
            }
            if surface.cur.borrow().buffer.is_some() {
                let (x, y) = state
                    .focus_stack
                    .top()
                    .map(|t| (t.x.get() + 50, t.y.get() + 50))
                    .unwrap_or((20, 20));
                self.x.set(x);
                self.y.set(y);
                state.focus_stack.push(self);
                surface.mapped.set(true);
            }
        } else {
            if surface.cur.borrow().buffer.is_none() {
                surface.unmap(state);
            } else if let Some((edge, x, y, serial)) = self.resizing.get() {
                let geom = xdg_surface.get_window_geometry().unwrap();
                let (nx, ny) = geom.get_opposite_edge_point(edge);
                self.x.set(x - nx);
                self.y.set(y - ny);
                if xdg_surface
                    .last_acked_configure
                    .get()
                    .is_some_and(|acked| acked.wrapping_sub(serial) as i32 >= 0)
                {
                    self.resizing.set(None);
                }
            }
        }

        Ok(())
    }
}

#[derive(Default)]
pub struct XdgToplevelState {
    pub app_id: Option<CString>,
    pub title: Option<CString>,
    pub min_size: Option<(u32, u32)>,
    pub max_size: Option<(u32, u32)>,
}

fn xdg_toplevel_cb(ctx: RequestCtx<XdgToplevel>) -> io::Result<()> {
    let toplevel = ctx.client.compositor.xdg_toplevels.get(&ctx.proxy).unwrap();
    let surface = toplevel.wl_surface.upgrade().unwrap();

    use xdg_toplevel::Request;
    match ctx.request {
        Request::Destroy => {
            surface.unmap(ctx.state);
            *toplevel
                .xdg_surface
                .upgrade()
                .unwrap()
                .specific
                .borrow_mut() = SpecificRole::None;
            ctx.client.compositor.xdg_toplevels.remove(&ctx.proxy);
        }
        Request::SetParent(parent) => {
            if parent.is_some() {
                eprintln!("set_parent is ignored");
            }
        }
        Request::SetTitle(title) => {
            toplevel.dirty_title.set(true);
            toplevel.pending.borrow_mut().title = Some(title);
        }
        Request::SetAppId(app_id) => {
            toplevel.dirty_app_id.set(true);
            toplevel.pending.borrow_mut().app_id = Some(app_id);
        }
        Request::ShowWindowMenu(_) => (),
        Request::Move(_args) => {
            ctx.state.seat.pointer.start_move(toplevel.clone());
        }
        Request::Resize(args) => {
            ctx.state
                .seat
                .pointer
                .start_resize(args.edges, toplevel.clone());
        }
        Request::SetMaxSize(args) => {
            if args.width < 0 || args.height < 0 {
                return Err(io::Error::other("max size cannot be negative"));
            }
            toplevel.dirty_max_size.set(true);
            toplevel.pending.borrow_mut().max_size = Some((args.width as u32, args.height as u32));
        }
        Request::SetMinSize(args) => {
            if args.width < 0 || args.height < 0 {
                return Err(io::Error::other("min size cannot be negative"));
            }
            toplevel.dirty_min_size.set(true);
            toplevel.pending.borrow_mut().min_size = Some((args.width as u32, args.height as u32));
        }
        Request::SetMaximized => (),
        Request::UnsetMaximized => (),
        Request::SetFullscreen(_) => (), // Note: update the wm_capabilities event when implemented
        Request::UnsetFullscreen => (),
        Request::SetMinimized => (),
    }
    Ok(())
}

use std::cell::Cell;
use std::io;
use std::rc::{Rc, Weak};

use crate::client::RequestCtx;
use crate::globals::compositor::Surface;
use crate::State;
use crate::{protocol::*, Proxy};

use super::positioner::Positioner;
use super::{SpecificRole, XdgSurfaceRole};

pub struct XdgPopupRole {
    pub wl: XdgPopup,
    pub xdg_surface: Weak<XdgSurfaceRole>,
    pub wl_surface: Weak<Surface>,
    pub parent: Weak<XdgSurfaceRole>,
    pub positioner: Cell<Positioner>,
    pub grab: Cell<bool>,
    next_configure_serial: Cell<u32>,
    last_serial: Cell<u32>,

    pub x: Cell<i32>,
    pub y: Cell<i32>,
}

impl XdgPopupRole {
    pub fn new(
        xdg_popup: XdgPopup,
        xdg_surface: &Rc<XdgSurfaceRole>,
        parent: &Rc<XdgSurfaceRole>,
        positioner: Positioner,
    ) -> Self {
        xdg_popup.set_callback(xdg_popup_cb);
        Self {
            wl: xdg_popup,
            xdg_surface: Rc::downgrade(xdg_surface),
            wl_surface: xdg_surface.wl_surface.clone(),
            parent: Rc::downgrade(parent),
            positioner: Cell::new(positioner),
            grab: Cell::new(false),
            next_configure_serial: Cell::new(0),
            last_serial: Cell::new(0),

            x: Cell::new(0),
            y: Cell::new(0),
        }
    }

    //     fn unmap(&self, state: &mut State) {
    //         self.map_state.set(MapState::Unmapped);
    //         state.focus_stack.remove(self);
    //         state
    //             .seat
    //             .unfocus_surface(&self.wl_surface.upgrade().unwrap().wl);
    //     }

    //     pub fn apply_pending_configure(&self) {
    //         if let Some(configure) = self.pending_configure.take() {
    //             self.cur_configure.set(configure);
    //             let mut states = Vec::new();
    //             if configure.activated {
    //                 states.extend_from_slice(&(xdg_toplevel::State::Activated as u32).to_ne_bytes());
    //             }
    //             self.wl
    //                 .configure(configure.width as i32, configure.heinght as i32, states);
    //             self.xdg_surface
    //                 .upgrade()
    //                 .unwrap()
    //                 .wl
    //                 .configure(configure.serial);
    //         }
    //     }

    //     pub fn set_activated(&self, value: bool) {
    //         if self.cur_configure.get().activated != value {
    //             let mut configure = self.pending_configure.get().unwrap_or_else(|| {
    //                 let mut conf = self.cur_configure.get();
    //                 conf.serial += 1;
    //                 conf
    //             });
    //             configure.activated = value;
    //             self.pending_configure.set(Some(configure));
    //         }
    //     }

    //     pub fn request_size(&self, edge: ResizeEdge, mut width: NonZeroU32, mut height: NonZeroU32) {
    //         if self.map_state.get() == MapState::Mapped {
    //             let cur = self.cur.borrow();
    //             if let Some((max_w, max_h)) = cur.max_size {
    //                 if max_w != 0 && width.get() > max_w {
    //                     width = NonZeroU32::new(max_w).unwrap();
    //                 }
    //                 if max_h != 0 && height.get() > max_h {
    //                     height = NonZeroU32::new(max_h).unwrap();
    //                 }
    //             }
    //             if let Some((min_w, min_h)) = cur.min_size {
    //                 if min_w != 0 && width.get() < min_w {
    //                     width = NonZeroU32::new(min_w).unwrap();
    //                 }
    //                 if min_h != 0 && height.get() < min_h {
    //                     height = NonZeroU32::new(min_h).unwrap();
    //                 }
    //             }

    //             let mut configure = self.pending_configure.take().unwrap_or_else(|| {
    //                 let mut c = self.cur_configure.get();
    //                 c.serial += 1;
    //                 c
    //             });

    //             configure.width = width.get();
    //             configure.heinght = height.get();
    //             let serial = configure.serial;
    //             self.pending_configure.set(Some(configure));

    //             match self.resizing.get() {
    //                 None => {
    //                     let geom = self
    //                         .xdg_surface
    //                         .upgrade()
    //                         .unwrap()
    //                         .get_window_geometry()
    //                         .unwrap();
    //                     let (nx, ny) = geom.get_opposite_edge_point(edge);
    //                     self.resizing
    //                         .set(Some((edge, nx + self.x.get(), ny + self.y.get(), serial)));
    //                 }
    //                 Some((oe, onx, ony, _oserial)) => {
    //                     assert_eq!(oe, edge);
    //                     self.resizing.set(Some((edge, onx, ony, serial)));
    //                 }
    //             }
    //         }
    //     }

    fn configure(&self) {
        let serial = self.next_configure_serial.get();
        self.last_serial.set(serial);
        self.next_configure_serial.set(serial.wrapping_add(1));
        let positioner = self.positioner.get();
        let width = positioner.size.0.get();
        let height = positioner.size.1.get();
        let (x, y) = positioner.get_position();
        self.x.set(x);
        self.y.set(y);
        self.wl.configure(x, y, width as i32, height as i32);
        self.xdg_surface.upgrade().unwrap().wl.configure(serial);
    }

    pub fn committed(self: &Rc<Self>, state: &mut State) -> io::Result<()> {
        let surface = self.wl_surface.upgrade().unwrap();
        let xdg_surface = self.xdg_surface.upgrade().unwrap();

        if !surface.configured.get() {
            if surface.cur.borrow().buffer.is_some() {
                return Err(io::Error::other("unmapped surface commited a buffer"));
            }
            self.configure();
            surface.configured.set(true);
        } else if !surface.mapped.get() {
            if surface.cur.borrow().buffer.is_none() {
                return Err(io::Error::other("did not submit initial buffer"));
            }
            if Some(self.last_serial.get()) != xdg_surface.last_acked_configure.get() {
                return Err(io::Error::other("did not ack the initial config"));
            }
            *self.parent.upgrade().unwrap().popup.borrow_mut() = Some(self.clone());
            state.popup_stack.push(self.clone());
            surface.mapped.set(true);
        } else {
            assert!(surface.cur.borrow().buffer.is_some(), "unimplemented");
        }

        Ok(())
    }
}

fn xdg_popup_cb(ctx: RequestCtx<XdgPopup>) -> io::Result<()> {
    let popup = ctx
        .client
        .compositor
        .xdg_popups
        .get(&ctx.proxy)
        .unwrap()
        .clone();
    let surface = popup.wl_surface.upgrade().unwrap();

    use xdg_popup::Request;
    match ctx.request {
        Request::Destroy => {
            let parent = popup.parent.upgrade().unwrap();
            *parent.popup.borrow_mut() = None;
            *popup.xdg_surface.upgrade().unwrap().specific.borrow_mut() = SpecificRole::None;
            ctx.client.compositor.xdg_popups.remove(&ctx.proxy);
            surface.unmap(ctx.state);
            if !ctx
                .state
                .popup_stack
                .last()
                .is_some_and(|p| p.wl == popup.wl)
            {
                return Err(io::Error::other("destroyed popup must be the top one"));
            }
            ctx.state.popup_stack.pop();
        }
        Request::Grab(_args) => {
            popup.grab.set(true);
            ctx.state.seat.kbd_focus_surface(Some(surface.wl.clone()));
        }
        Request::Reposition(args) => {
            ctx.proxy.repositioned(args.token);
            let positioner = Positioner::from_raw(
                *ctx.client
                    .compositor
                    .xdg_positioners
                    .get(&args.positioner)
                    .unwrap(),
            )?;
            popup.positioner.set(positioner);
            popup.configure();
        }
    }

    //     use xdg_toplevel::Request;
    //     match ctx.request {
    //         Request::Destroy => {
    //             toplevel.unmap(ctx.state);
    //             *toplevel
    //                 .xdg_surface
    //                 .upgrade()
    //                 .unwrap()
    //                 .specific
    //                 .borrow_mut() = SpecificRole::None;
    //             ctx.client.compositor.xdg_toplevels.remove(&ctx.proxy);
    //         }
    //         Request::SetParent(parent) => {
    //             assert_eq!(parent, None, "unimplemented");
    //         }
    //         Request::SetTitle(title) => {
    //             toplevel.dirty_title.set(true);
    //             toplevel.pending.borrow_mut().title = Some(title);
    //         }
    //         Request::SetAppId(app_id) => {
    //             toplevel.dirty_app_id.set(true);
    //             toplevel.pending.borrow_mut().app_id = Some(app_id);
    //         }
    //         Request::ShowWindowMenu(_) => (),
    //         Request::Move(_args) => {
    //             ctx.state.seat.pointer.start_move(toplevel.clone());
    //         }
    //         Request::Resize(args) => {
    //             ctx.state
    //                 .seat
    //                 .pointer
    //                 .start_resize(args.edges, toplevel.clone());
    //         }
    //         Request::SetMaxSize(args) => {
    //             if args.width < 0 || args.height < 0 {
    //                 return Err(io::Error::other("max size cannot be negative"));
    //             }
    //             toplevel.dirty_max_size.set(true);
    //             toplevel.pending.borrow_mut().max_size = Some((args.width as u32, args.height as u32));
    //         }
    //         Request::SetMinSize(args) => {
    //             if args.width < 0 || args.height < 0 {
    //                 return Err(io::Error::other("min size cannot be negative"));
    //             }
    //             toplevel.dirty_min_size.set(true);
    //             toplevel.pending.borrow_mut().min_size = Some((args.width as u32, args.height as u32));
    //         }
    //         Request::SetMaximized => (),
    //         Request::UnsetMaximized => (),
    //         Request::SetFullscreen(_) => (), // Note: update the wm_capabilities event when implemented
    //         Request::UnsetFullscreen => (),
    //         Request::SetMinimized => (),
    //     }
    Ok(())
}

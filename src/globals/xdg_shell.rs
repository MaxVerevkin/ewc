use std::cell::{Cell, RefCell};
use std::ffi::CString;
use std::io;
use std::num::NonZeroU32;
use std::rc::{Rc, Weak};

use super::compositor::{Surface, SurfaceRole};
use super::IsGlobal;
use crate::client::RequestCtx;
use crate::protocol::*;
use crate::wayland_core::Proxy;
use crate::{Client, State};

pub struct XdgSurfaceRole {
    pub wl: XdgSurface,
    pub wl_surface: Weak<Surface>,
    specific: RefCell<SpecificRole>,
    last_acked_configure: Cell<Option<u32>>,

    cur: XdgSurfaceState,
    pending: XdgSurfaceState,

    effective_window_geometry: Cell<Option<WindowGeometry>>,
}

impl XdgSurfaceRole {
    pub fn get_window_geometry(&self) -> Option<WindowGeometry> {
        // TODO: check if surface is mapped
        self.effective_window_geometry.get()
    }

    pub fn get_toplevel(&self) -> Option<Rc<XdgToplevelRole>> {
        match &*self.specific.borrow() {
            SpecificRole::Toplevel(tl) => Some(tl.clone()),
            _ => None,
        }
    }
}

#[derive(Default)]
pub struct XdgSurfaceState {
    window_geometry: Cell<Option<WindowGeometry>>,
}

#[derive(Debug, Clone, Copy)]
pub struct WindowGeometry {
    pub x: i32,
    pub y: i32,
    pub width: NonZeroU32,
    pub height: NonZeroU32,
}

impl TryFrom<pixman::Box32> for WindowGeometry {
    type Error = ();

    fn try_from(bbox: pixman::Box32) -> Result<Self, Self::Error> {
        Ok(WindowGeometry {
            x: bbox.x1,
            y: bbox.y1,
            width: NonZeroU32::new((bbox.x2 - bbox.x1).try_into().map_err(|_| ())?).ok_or(())?,
            height: NonZeroU32::new((bbox.y2 - bbox.y1).try_into().map_err(|_| ())?).ok_or(())?,
        })
    }
}

impl From<WindowGeometry> for pixman::Box32 {
    fn from(geom: WindowGeometry) -> Self {
        Self {
            x1: geom.x,
            y1: geom.y,
            x2: geom.x + geom.width.get() as i32,
            y2: geom.y + geom.height.get() as i32,
        }
    }
}

pub enum SpecificRole {
    None,
    Toplevel(Rc<XdgToplevelRole>),
    _Popup,
}

impl SpecificRole {
    pub fn _get_toplevel(&self) -> Option<&XdgToplevelRole> {
        match self {
            SpecificRole::Toplevel(tl) => Some(tl),
            _ => None,
        }
    }
}

pub struct XdgToplevelRole {
    pub wl: XdgToplevel,
    pub xdg_surface: Weak<XdgSurfaceRole>,
    pub wl_surface: Weak<Surface>,
    pub map_state: Cell<MapState>,
    pub pending_configure: Cell<Option<u32>>,

    pub x: Cell<i32>,
    pub y: Cell<i32>,

    pub cur: RefCell<XdgToplevelState>,
    pub pending: RefCell<XdgToplevelState>,
    pub dirty_app_id: Cell<bool>,
    pub dirty_title: Cell<bool>,
    pub dirty_min_size: Cell<bool>,
}

impl XdgToplevelRole {
    fn unmap(&self, state: &mut State) {
        self.map_state.set(MapState::Unmapped);
        state
            .focus_stack
            .retain(|s| s.upgrade().unwrap().wl != self.wl);
        state
            .seat
            .unfocus_surface(&self.wl_surface.upgrade().unwrap().wl);
    }
}

#[derive(Default)]
pub struct XdgToplevelState {
    pub app_id: Option<CString>,
    pub title: Option<CString>,
    pub min_size: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapState {
    Unmapped,
    WaitingFirstBuffer,
    Mapped,
}

impl IsGlobal for XdgWmBase {
    fn on_bind(&self, _client: &mut crate::Client) {
        self.set_callback(|ctx| {
            use xdg_wm_base::Request;
            match ctx.request {
                Request::Destroy => {
                    if ctx
                        .client
                        .compositor
                        .surfaces
                        .values()
                        .any(|s| s.get_xdg_surface().is_some())
                    {
                        return Err(io::Error::other(
                            "destroying xdg_wm_base when there are still oplevels",
                        ));
                    }
                }
                Request::CreatePositioner(_) => todo!(),
                Request::GetXdgSurface(args) => {
                    let surface = ctx
                        .client
                        .compositor
                        .surfaces
                        .get(&args.surface)
                        .ok_or_else(|| io::Error::other("invalid surface id"))?;
                    if surface.has_role() {
                        return Err(io::Error::other("surface already has a role"));
                    }
                    args.id.set_callback(xdg_surface_cb);
                    let xdg_surface = Rc::new(XdgSurfaceRole {
                        wl: args.id.clone(),
                        wl_surface: Rc::downgrade(surface),
                        specific: RefCell::new(SpecificRole::None),
                        last_acked_configure: Cell::new(None),

                        cur: XdgSurfaceState::default(),
                        pending: XdgSurfaceState::default(),

                        effective_window_geometry: Cell::new(None),
                    });
                    ctx.client
                        .compositor
                        .xdg_surfaces
                        .insert(args.id.id(), xdg_surface.clone());
                    *surface.role.borrow_mut() = SurfaceRole::Xdg(xdg_surface);
                }
                Request::Pong(_) => todo!(),
            }
            Ok(())
        });
    }
}

fn xdg_surface_cb(ctx: RequestCtx<XdgSurface>) -> io::Result<()> {
    let xdg_surface = ctx
        .client
        .compositor
        .xdg_surfaces
        .get(&ctx.proxy.id())
        .unwrap();

    use xdg_surface::Request;
    match ctx.request {
        Request::Destroy => {
            if !matches!(&*xdg_surface.specific.borrow(), SpecificRole::None) {
                return Err(io::Error::other("xdg_surface destroyed before role"));
            }
            *xdg_surface.wl_surface.upgrade().unwrap().role.borrow_mut() = SurfaceRole::None;
            ctx.client.compositor.xdg_surfaces.remove(&ctx.proxy.id());
        }
        Request::GetToplevel(toplevel) => {
            if !matches!(&*xdg_surface.specific.borrow(), SpecificRole::None) {
                return Err(io::Error::other("xdg surface already has a role"));
            }
            toplevel.set_callback(xdg_toplevel_cb);
            let toplevel = Rc::new(XdgToplevelRole {
                wl: toplevel,
                xdg_surface: Rc::downgrade(xdg_surface),
                wl_surface: xdg_surface.wl_surface.clone(),
                map_state: Cell::new(MapState::Unmapped),
                pending_configure: Cell::new(None),

                x: Cell::new(0),
                y: Cell::new(0),

                cur: RefCell::new(XdgToplevelState::default()),
                pending: RefCell::new(XdgToplevelState::default()),
                dirty_app_id: Cell::new(false),
                dirty_title: Cell::new(false),
                dirty_min_size: Cell::new(false),
            });
            ctx.client
                .compositor
                .xdg_toplevels
                .insert(toplevel.wl.id(), toplevel.clone());
            *xdg_surface.specific.borrow_mut() = SpecificRole::Toplevel(toplevel);
        }
        Request::GetPopup(_) => todo!(),
        Request::SetWindowGeometry(args) => {
            if args.width <= 0 || args.height <= 0 {
                return Err(io::Error::other(
                    "window geometry with non-positive dimensions",
                ));
            }
            xdg_surface
                .pending
                .window_geometry
                .set(Some(WindowGeometry {
                    x: args.x,
                    y: args.y,
                    width: NonZeroU32::new(args.width as u32).unwrap(),
                    height: NonZeroU32::new(args.height as u32).unwrap(),
                }));
        }
        Request::AckConfigure(serial) => {
            xdg_surface.last_acked_configure.set(Some(serial));
        }
    }
    Ok(())
}

fn xdg_toplevel_cb(ctx: RequestCtx<XdgToplevel>) -> io::Result<()> {
    let toplevel = ctx
        .client
        .compositor
        .xdg_toplevels
        .get(&ctx.proxy.id())
        .unwrap();

    use xdg_toplevel::Request;
    match ctx.request {
        Request::Destroy => {
            toplevel.unmap(ctx.state);
            *toplevel
                .xdg_surface
                .upgrade()
                .unwrap()
                .specific
                .borrow_mut() = SpecificRole::None;
            ctx.client.compositor.xdg_toplevels.remove(&ctx.proxy.id());
        }
        Request::SetParent(_) => todo!(),
        Request::SetTitle(title) => {
            toplevel.dirty_title.set(true);
            toplevel.pending.borrow_mut().title = Some(title);
        }
        Request::SetAppId(app_id) => {
            toplevel.dirty_app_id.set(true);
            toplevel.pending.borrow_mut().app_id = Some(app_id);
        }
        Request::ShowWindowMenu(_) => todo!(),
        Request::Move(_) => todo!(),
        Request::Resize(_) => todo!(),
        Request::SetMaxSize(args) => {
            dbg!(args);
            eprintln!("TODO: set max size");
        }
        Request::SetMinSize(args) => {
            if args.width < 0 || args.height < 0 {
                return Err(io::Error::other("min size cannot be negative"));
            }
            toplevel.dirty_min_size.set(true);
            toplevel.pending.borrow_mut().min_size = Some((args.width as u32, args.height as u32));
        }
        Request::SetMaximized => todo!(),
        Request::UnsetMaximized => todo!(),
        Request::SetFullscreen(_) => todo!(),
        Request::UnsetFullscreen => todo!(),
        Request::SetMinimized => todo!(),
    }
    Ok(())
}

pub fn surface_commit(
    client: &mut Client,
    state: &mut State,
    xdg_surface: &XdgSurfaceRole,
) -> io::Result<()> {
    let surface = xdg_surface.wl_surface.upgrade().unwrap();
    if let Some(geom) = xdg_surface.pending.window_geometry.take() {
        let mut geom = pixman::Box32::from(geom);
        let bbox = surface.get_bounding_box().unwrap();
        geom.x1 = geom.x1.max(bbox.x1);
        geom.x2 = geom.x2.min(bbox.x2);
        geom.y1 = geom.y1.max(bbox.y1);
        geom.y2 = geom.y2.min(bbox.y2);
        let geom = WindowGeometry::try_from(geom).unwrap();
        xdg_surface.effective_window_geometry.set(Some(geom));
        xdg_surface.cur.window_geometry.set(Some(geom));
    }

    if xdg_surface.cur.window_geometry.get().is_none() {
        xdg_surface
            .effective_window_geometry
            .set(surface.get_bounding_box().and_then(|x| x.try_into().ok()))
    }

    match &*xdg_surface.specific.borrow() {
        SpecificRole::None => (),
        SpecificRole::Toplevel(toplevel) => {
            if toplevel.dirty_app_id.get() {
                toplevel.dirty_app_id.set(false);
                toplevel.cur.borrow_mut().app_id =
                    std::mem::take(&mut toplevel.pending.borrow_mut().app_id);
            }
            if toplevel.dirty_title.get() {
                toplevel.dirty_app_id.set(false);
                toplevel.cur.borrow_mut().title =
                    std::mem::take(&mut toplevel.pending.borrow_mut().title);
            }
            if toplevel.dirty_min_size.get() {
                toplevel.dirty_min_size.set(false);
                toplevel.cur.borrow_mut().min_size = toplevel.pending.borrow_mut().min_size;
            }

            match toplevel.map_state.get() {
                MapState::Unmapped => {
                    if surface.cur.borrow().buffer.is_some() {
                        return Err(io::Error::other("unmapped surface commited a buffer"));
                    }
                    let serial = client.compositor.next_configure_serial.0;
                    client.compositor.next_configure_serial += 1;
                    toplevel.wl.configure(1000, 800, Vec::new());
                    xdg_surface.wl.configure(serial);
                    toplevel.map_state.set(MapState::WaitingFirstBuffer);
                    toplevel.pending_configure.set(Some(serial));
                }
                MapState::WaitingFirstBuffer => {
                    if surface.cur.borrow().buffer.is_none() {
                        return Err(io::Error::other("did not submit initial buffer"));
                    }
                    if toplevel.pending_configure.get() != xdg_surface.last_acked_configure.get() {
                        return Err(io::Error::other("did not ack the initial config"));
                    }
                    let (x, y) = state
                        .focus_stack
                        .last()
                        .map(|t| t.upgrade().unwrap())
                        .map(|t| (t.x.get() + 50, t.y.get() + 50))
                        .unwrap_or((20, 20));
                    toplevel.map_state.set(MapState::Mapped);
                    toplevel.x.set(x);
                    toplevel.y.set(y);
                    state.focus_stack.push(Rc::downgrade(toplevel));
                }
                MapState::Mapped => {
                    if surface.cur.borrow().buffer.is_none() {
                        toplevel.unmap(state);
                    }
                }
            }
        }
        SpecificRole::_Popup => todo!(),
    }

    Ok(())
}

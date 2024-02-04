use std::cell::{Cell, RefCell};
use std::io;
use std::num::NonZeroU32;
use std::rc::{Rc, Weak};

use super::compositor::{Surface, SurfaceRole};
use super::IsGlobal;
use crate::client::RequestCtx;
use crate::protocol::xdg_toplevel::ResizeEdge;
use crate::protocol::*;
use crate::wayland_core::Proxy;
use crate::State;

pub mod popup;
pub mod positioner;
pub mod toplevel;

use popup::XdgPopupRole;
use positioner::{Positioner, RawPositioner};
use toplevel::XdgToplevelRole;

pub struct XdgSurfaceRole {
    pub wl: XdgSurface,
    pub wl_surface: Weak<Surface>,
    pub specific: RefCell<SpecificRole>,
    pub popup: RefCell<Option<Rc<XdgPopupRole>>>,
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

    pub fn committed(&self, state: &mut State) -> io::Result<()> {
        let surface = self.wl_surface.upgrade().unwrap();
        if let Some(geom) = self.pending.window_geometry.take() {
            let mut geom = pixman::Box32::from(geom);
            if let Some(bbox) = surface.get_bounding_box() {
                geom.x1 = geom.x1.max(bbox.x1);
                geom.x2 = geom.x2.min(bbox.x2);
                geom.y1 = geom.y1.max(bbox.y1);
                geom.y2 = geom.y2.min(bbox.y2);
                let geom = WindowGeometry::try_from(geom).unwrap();
                self.effective_window_geometry.set(Some(geom));
                self.cur.window_geometry.set(Some(geom));
            } else {
                self.effective_window_geometry.set(None);
                self.cur.window_geometry.set(None);
            }
        }

        if self.cur.window_geometry.get().is_none() {
            self.effective_window_geometry
                .set(surface.get_bounding_box().and_then(|x| x.try_into().ok()))
        }

        match &*self.specific.borrow() {
            SpecificRole::None => Ok(()),
            SpecificRole::Toplevel(toplevel) => toplevel.committed(state),
            SpecificRole::Popup(popup) => popup.committed(state),
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

impl WindowGeometry {
    pub fn get_opposite_edge_point(&self, edge: ResizeEdge) -> (i32, i32) {
        let mut nx = 0;
        let mut ny = 0;
        if edge as u32 & ResizeEdge::Top as u32 != 0 {
            ny = self.height.get() as i32;
        }
        if edge as u32 & ResizeEdge::Left as u32 != 0 {
            nx = self.width.get() as i32;
        }
        (nx, ny)
    }
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
    Popup(Rc<XdgPopupRole>),
}

impl SpecificRole {
    pub fn get_toplevel(&self) -> Option<Rc<XdgToplevelRole>> {
        match self {
            SpecificRole::Toplevel(tl) => Some(tl.clone()),
            _ => None,
        }
    }
}

impl IsGlobal for XdgWmBase {
    fn on_bind(&self, _client: &mut crate::Client, _state: &mut State) {
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
                Request::CreatePositioner(positioner) => {
                    positioner.set_callback(positioner::xdg_positioner_cb);
                    ctx.client
                        .compositor
                        .xdg_positioners
                        .insert(positioner, RawPositioner::default());
                }
                Request::GetXdgSurface(args) => {
                    let surface = ctx.client.compositor.surfaces.get(&args.surface).unwrap();
                    if surface.has_role() {
                        return Err(io::Error::other("surface already has a role"));
                    }
                    if surface.buf_transform().is_some() || surface.pending_buffer.take().is_some()
                    {
                        return Err(io::Error::other(
                            "surface has a buffer attached or committed",
                        ));
                    }
                    args.id.set_callback(xdg_surface_cb);
                    let xdg_surface = Rc::new(XdgSurfaceRole {
                        wl: args.id.clone(),
                        wl_surface: Rc::downgrade(surface),
                        specific: RefCell::new(SpecificRole::None),
                        popup: RefCell::new(None),
                        last_acked_configure: Cell::new(None),

                        cur: XdgSurfaceState::default(),
                        pending: XdgSurfaceState::default(),

                        effective_window_geometry: Cell::new(None),
                    });
                    ctx.client
                        .compositor
                        .xdg_surfaces
                        .insert(args.id, xdg_surface.clone());
                    *surface.role.borrow_mut() = SurfaceRole::Xdg(xdg_surface);
                }
                Request::Pong(_) => todo!(),
            }
            Ok(())
        });
    }
}

fn xdg_surface_cb(ctx: RequestCtx<XdgSurface>) -> io::Result<()> {
    let xdg_surface = ctx.client.compositor.xdg_surfaces.get(&ctx.proxy).unwrap();

    use xdg_surface::Request;
    match ctx.request {
        Request::Destroy => {
            if !matches!(&*xdg_surface.specific.borrow(), SpecificRole::None) {
                return Err(io::Error::other("xdg_surface destroyed before role"));
            }
            *xdg_surface.wl_surface.upgrade().unwrap().role.borrow_mut() = SurfaceRole::None;
            ctx.client.compositor.xdg_surfaces.remove(&ctx.proxy);
        }
        Request::GetToplevel(toplevel) => {
            if !matches!(&*xdg_surface.specific.borrow(), SpecificRole::None) {
                return Err(io::Error::other("xdg surface already has a role"));
            }
            if toplevel.version() >= 5 {
                toplevel.wm_capabilities(Vec::new());
            }
            let toplevel = Rc::new(XdgToplevelRole::new(toplevel, xdg_surface));
            ctx.client
                .compositor
                .xdg_toplevels
                .insert(toplevel.wl.clone(), toplevel.clone());
            *xdg_surface.specific.borrow_mut() = SpecificRole::Toplevel(toplevel);
        }
        Request::GetPopup(args) => {
            let parent = args
                .parent
                .ok_or_else(|| io::Error::other("get_popup with null parent"))?;
            let parent = ctx.client.compositor.xdg_surfaces.get(&parent).unwrap();
            let positioner = Positioner::from_raw(
                *ctx.client
                    .compositor
                    .xdg_positioners
                    .get(&args.positioner)
                    .unwrap(),
            )?;
            let popup = Rc::new(XdgPopupRole::new(args.id, xdg_surface, parent, positioner));
            ctx.client
                .compositor
                .xdg_popups
                .insert(popup.wl.clone(), popup.clone());
            *xdg_surface.specific.borrow_mut() = SpecificRole::Popup(popup);
        }
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

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;
use std::rc::{Rc, Weak};

use super::xdg_shell;
use crate::backend::{Backend, BufferId};
use crate::buffer_transform::BufferTransform;
use crate::client::RequestCtx;
use crate::globals::{GlobalsManager, IsGlobal};
use crate::protocol::*;
use crate::wayland_core::{Fixed, Proxy};
use crate::{Client, State};

#[derive(Default)]
pub struct Compositor {
    pub regions: HashMap<WlRegion, pixman::Region32>,
    pub surfaces: HashMap<WlSurface, Rc<Surface>>,
    pub subsurfaces: HashMap<WlSubsurface, Rc<SubsurfaceRole>>,
    pub xdg_surfaces: HashMap<XdgSurface, Rc<xdg_shell::XdgSurfaceRole>>,
    pub xdg_toplevels: HashMap<XdgToplevel, Rc<xdg_shell::toplevel::XdgToplevelRole>>,
    pub xdg_popups: HashMap<XdgPopup, Rc<xdg_shell::popup::XdgPopupRole>>,
    pub xdg_positioners: HashMap<XdgPositioner, xdg_shell::positioner::RawPositioner>,
    pub viewporters: HashMap<WpViewport, Rc<Surface>>,
}

impl Compositor {
    pub fn register_globals(globals: &mut GlobalsManager) {
        globals.add_global::<WlCompositor>(6);
        globals.add_global::<WlSubcompositor>(1);
        globals.add_global::<XdgWmBase>(5);
        globals.add_global::<WpViewporter>(1);
    }

    pub fn release_buffers(self, backend: &mut dyn Backend) {
        for surface in self.surfaces.values() {
            if let Some(subsurf) = surface.get_subsurface() {
                if let Some(buf_id) = subsurf.cached_state.borrow().buffer {
                    backend.renderer_state().buffer_unlock(buf_id);
                }
            }
            if let Some(buf_id) = surface.cur.borrow().buffer {
                backend.renderer_state().buffer_unlock(buf_id);
            }
        }
    }
}

pub struct Surface {
    pub wl: WlSurface,
    pub role: RefCell<SurfaceRole>,
    pub cur: RefCell<SurfaceState>,
    pending: RefCell<SurfaceState>,
    pub pending_buffer: Cell<Option<WlBuffer>>,
    viewport: Cell<Option<WpViewport>>,
    buf_transform: Cell<Option<BufferTransform>>,

    pub mapped: Cell<bool>,
    pub configured: Cell<bool>,
}

#[derive(Default)]
pub struct SurfaceState {
    pub mask: CommitedMask,

    pub buffer: Option<BufferId>,
    pub transform: Option<wl_output::Transform>,
    pub scale: Option<i32>,
    pub opaque_region: Option<pixman::Region32>,
    pub input_region: Option<pixman::Region32>,
    pub subsurfaces: Vec<SubsurfaceNode>,
    pub frame_cbs: Vec<WlCallback>,

    pub viewport_src: Option<(f64, f64, Fixed, Fixed)>,
    pub viewport_dst: Option<(u32, u32)>,
}

impl SurfaceState {
    pub fn apply_to_and_clear(&mut self, dst: &mut Self, state: &mut State) {
        if self.mask.empty() {
            return;
        }
        dst.mask.0 |= self.mask.0;
        if self.mask.contains(CommittedMaskBit::Buffer) {
            if let Some(old_buf) = dst.buffer {
                state.backend.renderer_state().buffer_unlock(old_buf);
            }
            dst.buffer = self.buffer.take();
        }
        if self.mask.contains(CommittedMaskBit::OpaqueRegion) {
            dst.opaque_region = self.opaque_region.take();
        }
        if self.mask.contains(CommittedMaskBit::InputRegion) {
            dst.input_region = self.input_region.take();
        }
        if self.mask.contains(CommittedMaskBit::Subsurfaces) {
            dst.subsurfaces = self.subsurfaces.clone();
        }
        if self.mask.contains(CommittedMaskBit::FrameCb) {
            dst.frame_cbs.extend_from_slice(&self.frame_cbs);
            self.frame_cbs.clear();
        }
        if self.mask.contains(CommittedMaskBit::Transform) {
            dst.transform = self.transform.take();
        }
        if self.mask.contains(CommittedMaskBit::ViewportSrc) {
            dst.viewport_src = self.viewport_src.take();
        }
        if self.mask.contains(CommittedMaskBit::ViewportDst) {
            dst.viewport_dst = self.viewport_dst.take();
        }
        if self.mask.contains(CommittedMaskBit::Scale) {
            dst.scale = self.scale.take();
        }
        self.mask.clear();
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(u32)]
pub enum CommittedMaskBit {
    Buffer = 1 << 0,
    OpaqueRegion = 1 << 1,
    InputRegion = 1 << 2,
    Subsurfaces = 1 << 3,
    FrameCb = 1 << 4,
    Transform = 1 << 5,
    ViewportSrc = 1 << 6,
    ViewportDst = 1 << 7,
    Scale = 1 << 8,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CommitedMask(u32);

impl CommitedMask {
    pub fn clear(&mut self) {
        self.0 = 0;
    }

    pub fn set(&mut self, bit: CommittedMaskBit) {
        self.0 |= bit as u32;
    }

    pub fn empty(self) -> bool {
        self.0 == 0
    }

    pub fn contains(self, bit: CommittedMaskBit) -> bool {
        self.0 & bit as u32 != 0
    }
}

#[derive(Clone)]
pub struct SubsurfaceNode {
    pub surface: Rc<Surface>,
    pub position: (i32, i32),
}

impl Surface {
    fn new(wl: WlSurface) -> Self {
        Self {
            wl,
            role: RefCell::new(SurfaceRole::None),
            cur: RefCell::new(SurfaceState::default()),
            pending: RefCell::new(SurfaceState::default()),
            pending_buffer: Cell::new(None),
            viewport: Cell::new(None),
            buf_transform: Cell::new(None),

            mapped: Cell::new(false),
            configured: Cell::new(false),
        }
    }

    pub fn unmap(&self, state: &mut State) {
        if self.mapped.get() {
            if let Some(toplevel) = self.get_xdg_toplevel() {
                state.focus_stack.remove(&toplevel);
            }
            state.seat.unfocus_surface(&self.wl);
            for sub in &self.cur.borrow().subsurfaces {
                sub.surface.unmap(state);
            }
        }
        self.mapped.set(false);
        self.configured.set(false);
    }

    fn validate_and_update_buf_transform(&self, backend: &mut dyn Backend) -> io::Result<()> {
        let cur = self.cur.borrow();
        match cur.buffer {
            Some(buf_id) => {
                self.buf_transform.set(Some(BufferTransform::new(
                    buf_id,
                    backend,
                    cur.transform.unwrap_or(wl_output::Transform::Normal),
                    cur.scale.unwrap_or(1),
                    cur.viewport_src,
                    cur.viewport_dst,
                )?));
            }
            None => self.buf_transform.set(None),
        }
        Ok(())
    }

    pub fn buf_transform(&self) -> Option<BufferTransform> {
        self.buf_transform.get()
    }

    pub fn get_bounding_box(&self) -> Option<pixman::Box32> {
        let buf_transfom = self.buf_transform()?;
        let mut bbox = pixman::Box32 {
            x1: 0,
            y1: 0,
            x2: buf_transfom.dst_width() as i32,
            y2: buf_transfom.dst_height() as i32,
        };
        for sub in &self.cur.borrow().subsurfaces {
            if let Some(sub_box) = sub.surface.get_bounding_box() {
                bbox.x1 = bbox.x1.min(sub.position.0 + sub_box.x1);
                bbox.x2 = bbox.x2.max(sub.position.0 + sub_box.x2);
                bbox.y1 = bbox.y1.min(sub.position.1 + sub_box.y1);
                bbox.y2 = bbox.y2.max(sub.position.1 + sub_box.y2);
            }
        }
        Some(bbox)
    }

    pub fn get_pos(&self) -> Option<(i32, i32)> {
        match &*self.role.borrow() {
            SurfaceRole::None => None,
            SurfaceRole::Cursor => None,
            SurfaceRole::Subsurface(sub) => {
                let parent = sub.parent.upgrade().unwrap();
                let (px, py) = parent
                    .cur
                    .borrow()
                    .subsurfaces
                    .iter()
                    .find(|node| node.surface.wl == self.wl)?
                    .position;
                parent.get_pos().map(|(x, y)| (x + px, y + py))
            }
            SurfaceRole::Xdg(xdg) => match &*xdg.specific.borrow() {
                xdg_shell::SpecificRole::None => None,
                xdg_shell::SpecificRole::Toplevel(toplevel) => xdg
                    .get_window_geometry()
                    .map(|geom| (toplevel.x.get() - geom.x, toplevel.y.get() - geom.y)),
                xdg_shell::SpecificRole::Popup(popup) => {
                    let parent = popup.parent.upgrade().unwrap();
                    let (parent_x, parent_y) = parent.wl_surface.upgrade().unwrap().get_pos()?;
                    let parent_geom = parent.get_window_geometry()?;
                    let popup_geom = popup.xdg_surface.upgrade().unwrap().get_window_geometry()?;
                    Some((
                        parent_x + parent_geom.x + popup.x.get() - popup_geom.x,
                        parent_y + parent_geom.y + popup.y.get() - popup_geom.y,
                    ))
                }
            },
        }
    }

    pub fn effective_is_sync(&self) -> bool {
        self.get_subsurface().is_some_and(|sub| {
            sub.is_sync.get() || sub.parent.upgrade().unwrap().effective_is_sync()
        })
    }

    pub fn has_role(&self) -> bool {
        !matches!(&*self.role.borrow(), SurfaceRole::None)
    }

    pub fn get_subsurface(&self) -> Option<Rc<SubsurfaceRole>> {
        match &*self.role.borrow() {
            SurfaceRole::Subsurface(sub) => Some(sub.clone()),
            _ => None,
        }
    }

    pub fn get_xdg_surface(&self) -> Option<Rc<xdg_shell::XdgSurfaceRole>> {
        match &*self.role.borrow() {
            SurfaceRole::Xdg(xdg) => Some(xdg.clone()),
            _ => None,
        }
    }

    pub fn get_xdg_toplevel(&self) -> Option<Rc<xdg_shell::toplevel::XdgToplevelRole>> {
        match &*self.role.borrow() {
            SurfaceRole::Xdg(xdg) => xdg.get_toplevel(),
            _ => None,
        }
    }

    fn apply_cache(&self, state: &mut State) -> io::Result<()> {
        if let Some(subs) = self.get_subsurface() {
            subs.cached_state
                .borrow_mut()
                .apply_to_and_clear(&mut self.cur.borrow_mut(), state);
        }
        self.validate_and_update_buf_transform(state.backend.as_mut())?; // todo: run only if relevant data was updated
        for subs in &self.cur.borrow().subsurfaces {
            subs.surface.apply_cache(state)?;
        }
        Ok(())
    }
}

pub enum SurfaceRole {
    None,
    Cursor,
    Subsurface(Rc<SubsurfaceRole>),
    Xdg(Rc<xdg_shell::XdgSurfaceRole>),
}

pub struct SubsurfaceRole {
    pub wl: WlSubsurface,
    pub surface: Weak<Surface>,
    pub parent: Weak<Surface>,
    pub is_sync: Cell<bool>,
    pub cached_state: RefCell<SurfaceState>,
}

impl IsGlobal for WlCompositor {
    fn on_bind(&self, _client: &mut Client, _state: &mut State) {
        self.set_callback(|ctx| {
            use wl_compositor::Request;
            match ctx.request {
                Request::CreateSurface(wl) => {
                    wl.set_callback(wl_surface_cb);
                    ctx.client
                        .compositor
                        .surfaces
                        .insert(wl.clone(), Rc::new(Surface::new(wl)));
                }
                Request::CreateRegion(wl) => {
                    wl.set_callback(wl_region_cb);
                    ctx.client
                        .compositor
                        .regions
                        .insert(wl, pixman::Region32::default());
                }
            }
            Ok(())
        });
    }
}

impl IsGlobal for WlSubcompositor {
    fn on_bind(&self, _client: &mut Client, _state: &mut State) {
        self.set_callback(|ctx| {
            use wl_subcompositor::Request;
            match ctx.request {
                Request::Destroy => (),
                Request::GetSubsurface(args) => {
                    args.id.set_callback(wl_subsurface_cb);
                    let surface = ctx.client.compositor.surfaces.get(&args.surface).unwrap();
                    let parent = ctx.client.compositor.surfaces.get(&args.parent).unwrap();
                    if surface.has_role() {
                        return Err(io::Error::other("surface already has a role"));
                    }
                    let subsurface = Rc::new(SubsurfaceRole {
                        wl: args.id.clone(),
                        surface: Rc::downgrade(surface),
                        parent: Rc::downgrade(parent),
                        is_sync: Cell::new(true),
                        cached_state: RefCell::new(SurfaceState::default()),
                    });
                    *surface.role.borrow_mut() = SurfaceRole::Subsurface(subsurface.clone());
                    ctx.client
                        .compositor
                        .subsurfaces
                        .insert(args.id, subsurface);
                    parent
                        .pending
                        .borrow_mut()
                        .subsurfaces
                        .push(SubsurfaceNode {
                            surface: surface.clone(),
                            position: (0, 0),
                        });
                    parent
                        .pending
                        .borrow_mut()
                        .mask
                        .set(CommittedMaskBit::Subsurfaces);
                }
            }
            Ok(())
        });
    }
}

impl IsGlobal for WpViewporter {
    fn on_bind(&self, _client: &mut Client, _state: &mut State) {
        self.set_callback(|ctx| {
            use wp_viewporter::Request;
            match ctx.request {
                Request::Destroy => (),
                Request::GetViewport(args) => {
                    args.id.set_callback(wp_viewport_cb);
                    let surf = ctx.client.compositor.surfaces.get(&args.surface).unwrap();
                    if surf.viewport.take().is_some() {
                        return Err(io::Error::other("surface already has a viewport"));
                    }
                    surf.viewport.set(Some(args.id.clone()));
                    ctx.client
                        .compositor
                        .viewporters
                        .insert(args.id, surf.clone());
                }
            }
            Ok(())
        });
    }
}

fn wl_surface_cb(ctx: RequestCtx<WlSurface>) -> io::Result<()> {
    let surface = ctx
        .client
        .compositor
        .surfaces
        .get(&ctx.proxy)
        .unwrap()
        .clone();

    use wl_surface::Request;
    match ctx.request {
        Request::Destroy => {
            if !matches!(
                &*surface.role.borrow(),
                SurfaceRole::None | SurfaceRole::Cursor,
            ) {
                return Err(io::Error::other("destroying wl_surface before role object"));
            }
            for sub in &surface.pending.borrow().subsurfaces {
                sub.surface.unmap(ctx.state);
            }
        }
        Request::Attach(args) => {
            if ctx.proxy.version() >= 5 && (args.x != 0 || args.y != 0) {
                return Err(io::Error::other(
                    "attach on wl_surface version >=5 must have x,y=0",
                ));
            }
            assert_eq!(args.x, 0, "unimplemented");
            assert_eq!(args.y, 0, "unimplemented");
            surface.pending_buffer.set(args.buffer);
            surface
                .pending
                .borrow_mut()
                .mask
                .set(CommittedMaskBit::Buffer);
        }
        Request::Damage(_) => (),
        Request::Frame(cb) => {
            surface.pending.borrow_mut().frame_cbs.push(cb);
            surface
                .pending
                .borrow_mut()
                .mask
                .set(CommittedMaskBit::FrameCb);
        }
        Request::SetOpaqueRegion(reg_id) => {
            surface.pending.borrow_mut().opaque_region = match reg_id {
                Some(reg) => Some(ctx.client.compositor.regions.get(&reg).unwrap().clone()),
                None => None,
            };
            surface
                .pending
                .borrow_mut()
                .mask
                .set(CommittedMaskBit::OpaqueRegion);
        }
        Request::SetInputRegion(reg_id) => {
            surface.pending.borrow_mut().input_region = match reg_id {
                Some(reg) => Some(ctx.client.compositor.regions.get(&reg).unwrap().clone()),
                None => None,
            };
            surface
                .pending
                .borrow_mut()
                .mask
                .set(CommittedMaskBit::InputRegion);
        }
        Request::Commit => {
            if surface
                .pending
                .borrow()
                .mask
                .contains(CommittedMaskBit::Buffer)
            {
                surface.pending.borrow_mut().buffer =
                    surface.pending_buffer.take().and_then(|pending_buffer| {
                        if pending_buffer.is_alive() {
                            Some(
                                ctx.state
                                    .backend
                                    .renderer_state()
                                    .buffer_commited(pending_buffer),
                            )
                        } else {
                            None
                        }
                    });
            }

            if surface.effective_is_sync() {
                surface.pending.borrow_mut().apply_to_and_clear(
                    &mut surface.get_subsurface().unwrap().cached_state.borrow_mut(),
                    ctx.state,
                );
            } else {
                surface
                    .pending
                    .borrow_mut()
                    .apply_to_and_clear(&mut surface.cur.borrow_mut(), ctx.state);
                surface.apply_cache(ctx.state)?;

                match &*surface.role.borrow() {
                    SurfaceRole::None => (),
                    SurfaceRole::Xdg(xdg) => xdg.committed(ctx.state)?,
                    SurfaceRole::Cursor => (),
                    SurfaceRole::Subsurface(_) => (),
                }
            }
        }
        Request::SetBufferTransform(transform) => {
            let mut pending = surface.pending.borrow_mut();
            pending.transform = Some(transform);
            pending.mask.set(CommittedMaskBit::Transform);
        }
        Request::SetBufferScale(scale) => {
            let mut pending = surface.pending.borrow_mut();
            pending.scale = Some(scale);
            pending.mask.set(CommittedMaskBit::Scale);
        }
        Request::DamageBuffer(_) => (),
        Request::Offset(args) => {
            assert_eq!(args.x, 0, "unimplemnted");
            assert_eq!(args.y, 0, "unimplemnted");
        }
    }
    Ok(())
}

fn wl_subsurface_cb(ctx: RequestCtx<WlSubsurface>) -> io::Result<()> {
    let subsurface = ctx.client.compositor.subsurfaces.get(&ctx.proxy).unwrap();
    let surface = subsurface.surface.upgrade().unwrap();

    use wl_subsurface::Request;
    match ctx.request {
        Request::Destroy => {
            *surface.role.borrow_mut() = SurfaceRole::None;
            let subsurface = ctx
                .client
                .compositor
                .subsurfaces
                .remove(&ctx.proxy)
                .unwrap();
            if let Some(parent) = subsurface.parent.upgrade() {
                parent
                    .cur
                    .borrow_mut()
                    .subsurfaces
                    .retain(|node| node.surface.wl != surface.wl);
                parent
                    .pending
                    .borrow_mut()
                    .subsurfaces
                    .retain(|node| node.surface.wl != surface.wl);
                if let Some(parent_sub) = parent.get_subsurface() {
                    parent_sub
                        .cached_state
                        .borrow_mut()
                        .subsurfaces
                        .retain(|node| node.surface.wl != surface.wl);
                }
            }
        }
        Request::SetPosition(args) => {
            let parent = subsurface.parent.upgrade().unwrap();
            let mut parent_pending = parent.pending.borrow_mut();
            parent_pending
                .subsurfaces
                .iter_mut()
                .find(|n| n.surface.wl == surface.wl)
                .unwrap()
                .position = (args.x, args.y);
            parent_pending.mask.set(CommittedMaskBit::Subsurfaces)
        }
        Request::PlaceAbove(sibling) => {
            let parent = subsurface.parent.upgrade().unwrap();
            let mut parent_pending = parent.pending.borrow_mut();
            let old_i = parent_pending
                .subsurfaces
                .iter()
                .position(|x| x.surface.wl == surface.wl)
                .unwrap();
            let node = parent_pending.subsurfaces.remove(old_i);
            let sibling_i = parent_pending
                .subsurfaces
                .iter()
                .position(|x| x.surface.wl == sibling)
                .ok_or_else(|| io::Error::other("place_above: surface not a sibling"))?;
            parent_pending.subsurfaces.insert(sibling_i + 1, node);
        }
        Request::PlaceBelow(sibling) => {
            let parent = subsurface.parent.upgrade().unwrap();
            let mut parent_pending = parent.pending.borrow_mut();
            let old_i = parent_pending
                .subsurfaces
                .iter()
                .position(|x| x.surface.wl == surface.wl)
                .unwrap();
            let node = parent_pending.subsurfaces.remove(old_i);
            let sibling_i = parent_pending
                .subsurfaces
                .iter()
                .position(|x| x.surface.wl == sibling)
                .ok_or_else(|| io::Error::other("place_above: surface not a sibling"))?;
            parent_pending.subsurfaces.insert(sibling_i, node);
        }
        Request::SetSync => subsurface.is_sync.set(true),
        Request::SetDesync => {
            subsurface.is_sync.set(false);
            if subsurface.parent.upgrade().unwrap().effective_is_sync() {
                surface.apply_cache(ctx.state)?;
            }
        }
    }
    Ok(())
}

fn wl_region_cb(ctx: RequestCtx<WlRegion>) -> io::Result<()> {
    use wl_region::Request;
    match ctx.request {
        Request::Destroy => {
            ctx.client.compositor.regions.remove(&ctx.proxy);
        }
        Request::Add(args) => {
            let region = ctx.client.compositor.regions.get_mut(&ctx.proxy).unwrap();
            *region = region.union_rect(args.x, args.y, args.width as u32, args.height as u32);
        }
        Request::Subtract(args) => {
            let region = ctx.client.compositor.regions.get_mut(&ctx.proxy).unwrap();
            let other =
                pixman::Region32::init_rect(args.x, args.y, args.width as u32, args.height as u32);
            *region = region.subtract(&other);
        }
    }
    Ok(())
}

fn wp_viewport_cb(ctx: RequestCtx<WpViewport>) -> io::Result<()> {
    let surf = ctx
        .client
        .compositor
        .viewporters
        .get(&ctx.proxy)
        .cloned()
        .ok_or_else(|| io::Error::other("viweport: surface is dead"))?;
    let mut pending = surf.pending.borrow_mut();

    use wp_viewport::Request;
    match ctx.request {
        Request::Destroy => {
            pending.viewport_src = None;
            pending.viewport_dst = None;
            surf.viewport.set(None);
            ctx.client.compositor.viewporters.remove(&ctx.proxy);
        }
        Request::SetSource(args) => {
            pending.mask.set(CommittedMaskBit::ViewportSrc);
            if args.x == Fixed::MINUS_ONE
                && args.y == Fixed::MINUS_ONE
                && args.width == Fixed::MINUS_ONE
                && args.height == Fixed::MINUS_ONE
            {
                pending.viewport_src = None;
            } else if args.x >= Fixed::ZERO
                && args.y >= Fixed::ZERO
                && args.width > Fixed::ZERO
                && args.height > Fixed::ZERO
            {
                pending.viewport_src =
                    Some((args.x.as_f64(), args.y.as_f64(), args.width, args.height));
            } else {
                return Err(io::Error::other("invalid viewport src"));
            }
        }
        Request::SetDestination(args) => {
            if args.width == -1 && args.height == -1 {
                pending.mask.set(CommittedMaskBit::ViewportDst);
                pending.viewport_dst = None;
            } else if args.width > 0 && args.height > 0 {
                pending.mask.set(CommittedMaskBit::ViewportDst);
                pending.viewport_dst = Some((args.width as u32, args.height as u32));
            } else {
                return Err(io::Error::other("invalid viewport dst"));
            }
        }
    }
    Ok(())
}

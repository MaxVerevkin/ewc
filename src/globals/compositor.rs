use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;
use std::rc::{Rc, Weak};

use super::xdg_shell;
use crate::backend::{Backend, BufferId};
use crate::client::RequestCtx;
use crate::globals::{GlobalsManager, IsGlobal};
use crate::wayland_core::Proxy;
use crate::{protocol::*, Fixed};
use crate::{Client, State};

#[derive(Default)]
pub struct Compositor {
    pub regions: HashMap<WlRegion, pixman::Region32>,
    pub surfaces: HashMap<WlSurface, Rc<Surface>>,
    pub subsurfaces: HashMap<WlSubsurface, Rc<SubsurfaceRole>>,
    pub xdg_surfaces: HashMap<XdgSurface, Rc<xdg_shell::XdgSurfaceRole>>,
    pub xdg_toplevels: HashMap<XdgToplevel, Rc<xdg_shell::XdgToplevelRole>>,
    pub viewporters: HashMap<WpViewport, Weak<Surface>>,
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
                if let Some((buf_id, _, _)) = subsurf.cached_state.borrow().buffer {
                    backend.renderer_state().buffer_unlock(buf_id);
                }
            }
            if let Some((buf_id, _, _)) = surface.cur.borrow().buffer {
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
    pending_buffer: RefCell<Option<WlBuffer>>,
    viewport: Cell<Option<WpViewport>>,
}

#[derive(Default, Clone)]
pub struct SurfaceState {
    pub mask: CommitedMask,

    pub buffer: Option<(BufferId, u32, u32)>,
    pub transform: Option<wl_output::Transform>,
    pub opaque_region: Option<pixman::Region32>,
    pub input_region: Option<pixman::Region32>,
    pub subsurfaces: Vec<SubsurfaceNode>,
    pub frame_cbs: Vec<WlCallback>,

    pub viewport_src: Option<(f64, f64, f64, f64)>,
    pub viewport_dst: Option<(u32, u32)>,
}

impl SurfaceState {
    pub fn apply_to_and_clear(&mut self, dst: &mut Self, state: &mut State) {
        if self.mask.empty() {
            return;
        }
        dst.mask.0 |= self.mask.0;
        if self.mask.contains(CommittedMaskBit::Buffer) {
            if let Some((old_buf, _, _)) = dst.buffer {
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
            pending_buffer: RefCell::new(None),
            viewport: Cell::new(None),
        }
    }

    pub fn effective_buffer_size(&self) -> Option<(u32, u32)> {
        let cur = self.cur.borrow();
        let (_id, w, h) = cur.buffer?;

        if let Some((w, h)) = cur.viewport_dst {
            Some((w, h))
        } else if let Some((_x, _y, w, h)) = cur.viewport_src {
            Some((w.ceil() as u32, h.ceil() as u32))
        } else {
            use wl_output::Transform;
            Some(match cur.transform.unwrap_or(Transform::Normal) {
                Transform::Normal
                | Transform::_180
                | Transform::Flipped
                | Transform::Flipped180 => (w, h),
                Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270 => {
                    (h, w)
                }
            })
        }
    }

    pub fn buffer_transform(&self) -> Option<BufferTransform> {
        let (dst_width, dst_height) = self.effective_buffer_size()?;
        let cur = self.cur.borrow();
        let (src_x, src_y, src_width, src_height) = match cur.viewport_src {
            None => (0.0, 0.0, dst_width as f64, dst_height as f64),
            Some(src) => src,
        };
        Some(BufferTransform {
            transform: cur.transform.unwrap_or(wl_output::Transform::Normal),
            scale: 1,
            src_x,
            src_y,
            src_width,
            src_height,
            dst_width,
            dst_height,
        })
    }

    pub fn get_bounding_box(&self) -> Option<pixman::Box32> {
        let (w, h) = self.effective_buffer_size()?;
        let mut bbox = pixman::Box32 {
            x1: 0,
            y1: 0,
            x2: w as i32,
            y2: h as i32,
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

    pub fn get_pos(self: &Rc<Self>) -> Option<(i32, i32)> {
        let mut s = self.clone();
        let mut sub_x = 0;
        let mut sub_y = 0;
        while let Some(sub) = s.get_subsurface() {
            let parent = sub.parent.upgrade().unwrap();
            let (px, py) = parent
                .cur
                .borrow()
                .subsurfaces
                .iter()
                .find(|node| node.surface.wl == s.wl)?
                .position;
            sub_x += px;
            sub_y += py;
            s = parent;
        }
        if let Some(xdg) = s.get_xdg_surface() {
            if let Some(toplevel) = xdg.get_toplevel() {
                if let Some(geom) = xdg.get_window_geometry() {
                    return Some((
                        toplevel.x.get() + sub_x - geom.x,
                        toplevel.y.get() + sub_y - geom.y,
                    ));
                }
            }
        }
        None
    }

    pub fn effective_is_sync(&self) -> bool {
        if let Some(subsurface) = self.get_subsurface() {
            match subsurface.is_sync.get() {
                true => true,
                false => subsurface.parent.upgrade().unwrap().effective_is_sync(),
            }
        } else {
            false
        }
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

    fn apply_cache(&self, state: &mut State) {
        if let Some(subs) = self.get_subsurface() {
            subs.cached_state
                .borrow_mut()
                .apply_to_and_clear(&mut self.cur.borrow_mut(), state);
        }
        for subs in &self.cur.borrow().subsurfaces {
            subs.surface.apply_cache(state);
        }
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
                        .insert(args.id, Rc::downgrade(surf));
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
        }
        Request::Attach(args) => {
            if ctx.proxy.version() >= 5 && (args.x != 0 || args.y != 0) {
                return Err(io::Error::other(
                    "attach on wl_surface version >=5 must have x,y=0",
                ));
            }
            assert_eq!(args.x, 0, "unimplemented");
            assert_eq!(args.y, 0, "unimplemented");
            *surface.pending_buffer.borrow_mut() = args.buffer;
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
                            let buf_id = ctx
                                .state
                                .backend
                                .renderer_state()
                                .buffer_commited(pending_buffer);
                            let (width, height) =
                                ctx.state.backend.renderer_state().get_buffer_size(buf_id);
                            Some((buf_id, width, height))
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
                surface.apply_cache(ctx.state);
                if let Some(xdg) = surface.get_xdg_surface() {
                    xdg_shell::surface_commit(ctx.state, &xdg)?;
                }
            }
        }
        Request::SetBufferTransform(transform) => {
            let mut pending = surface.pending.borrow_mut();
            pending.transform = Some(transform);
            pending.mask.set(CommittedMaskBit::Transform);
        }
        Request::SetBufferScale(scale) => {
            assert_eq!(scale, 1, "unimplemented");
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

    use wl_subsurface::Request;
    match ctx.request {
        Request::Destroy => {
            *subsurface.surface.upgrade().unwrap().role.borrow_mut() = SurfaceRole::None;
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
                    .retain(|node| node.surface.wl != subsurface.surface.upgrade().unwrap().wl);
                parent
                    .pending
                    .borrow_mut()
                    .subsurfaces
                    .retain(|node| node.surface.wl != subsurface.surface.upgrade().unwrap().wl);
                if let Some(parent_sub) = parent.get_subsurface() {
                    parent_sub
                        .cached_state
                        .borrow_mut()
                        .subsurfaces
                        .retain(|node| node.surface.wl != subsurface.surface.upgrade().unwrap().wl);
                }
            }
        }
        Request::SetPosition(args) => {
            subsurface
                .parent
                .upgrade()
                .unwrap()
                .pending
                .borrow_mut()
                .subsurfaces
                .iter_mut()
                .find(|n| n.surface.wl == subsurface.surface.upgrade().unwrap().wl)
                .unwrap()
                .position = (args.x, args.y);
            subsurface
                .parent
                .upgrade()
                .unwrap()
                .pending
                .borrow_mut()
                .mask
                .set(CommittedMaskBit::Subsurfaces)
        }
        Request::PlaceAbove(_) => todo!(),
        Request::PlaceBelow(_) => todo!(),
        Request::SetSync => subsurface.is_sync.set(true),
        Request::SetDesync => {
            subsurface.is_sync.set(false);
            if subsurface.parent.upgrade().unwrap().effective_is_sync() {
                subsurface.surface.upgrade().unwrap().apply_cache(ctx.state);
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
        .unwrap()
        .upgrade()
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
            dbg!(&args);
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
                pending.viewport_src = Some((
                    args.x.as_f64(),
                    args.y.as_f64(),
                    args.width.as_f64(),
                    args.height.as_f64(),
                ));
            } else {
                return Err(io::Error::other("invalid viewport src"));
            }
        }
        Request::SetDestination(args) => {
            dbg!(&args);
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

pub struct BufferTransform {
    pub transform: wl_output::Transform,
    pub scale: u32,
    pub src_x: f64,
    pub src_y: f64,
    pub src_width: f64,
    pub src_height: f64,
    pub dst_width: u32,
    pub dst_height: u32,
}

impl BufferTransform {
    pub fn surface_to_buffer(&self, buf_width: u32, buf_height: u32) -> Option<pixman::FTransform> {
        let mut mat = pixman::FTransform::identity()
            .scale(
                self.src_width / self.dst_width as f64,
                self.src_height / self.dst_height as f64,
                false,
            )?
            .translate(self.src_x, self.src_y, false)?
            .scale(self.scale as f64, self.scale as f64, false)?;
        if self.transform as u32 & 4 != 0 {
            mat = mat
                .scale(-1.0, 1.0, false)?
                .translate(buf_width as f64, 0.0, false)?;
        }
        if self.transform as u32 & 1 != 0 {
            mat = mat
                .rotate(0.0, -1.0, false)?
                .translate(0.0, buf_height as f64, false)?;
        }
        if self.transform as u32 & 2 != 0 {
            mat = mat.rotate(-1.0, 0.0, false)?.translate(
                buf_width as f64,
                buf_height as f64,
                false,
            )?;
        }
        Some(mat)
    }

    pub fn surface_to_uv(&self, buf_width: u32, buf_height: u32) -> Option<pixman::FTransform> {
        self.surface_to_buffer(buf_width, buf_height)
            .and_then(|m| m.scale(1.0 / buf_width as f64, 1.0 / buf_height as f64, false))
    }
}

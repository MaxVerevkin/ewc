use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;
use std::num::Wrapping;
use std::rc::{Rc, Weak};

use super::xdg_shell;
use super::IsGlobal;
use crate::backend::BufferId;
use crate::client::RequestCtx;
use crate::protocol::*;
use crate::wayland_core::{ObjectId, Proxy};
use crate::State;
use crate::{Client, Global};

pub struct Compositor {
    pub next_configure_serial: Wrapping<u32>,
    pub regions: HashMap<ObjectId, pixman::Region32>,
    pub surfaces: HashMap<ObjectId, Rc<Surface>>,
    pub subsurfaces: HashMap<ObjectId, Rc<SubsurfaceRole>>,
    pub xdg_surfaces: HashMap<ObjectId, Rc<xdg_shell::XdgSurfaceRole>>,
    pub xdg_toplevels: HashMap<ObjectId, Rc<xdg_shell::XdgToplevelRole>>,
}

impl Compositor {
    pub fn global(name: u32) -> Global {
        Global::new::<WlCompositor>(name, 6)
    }

    pub fn new() -> Self {
        Self {
            next_configure_serial: Wrapping(0),
            regions: HashMap::new(),
            surfaces: HashMap::new(),
            subsurfaces: HashMap::new(),
            xdg_surfaces: HashMap::new(),
            xdg_toplevels: HashMap::new(),
        }
    }
}

pub struct Surface {
    pub wl: WlSurface,
    pub role: RefCell<SurfaceRole>,
    pub cur: RefCell<SurfaceState>,
    pending: RefCell<SurfaceState>,
    pending_buffer: RefCell<Option<WlBuffer>>,
}

#[derive(Default, Clone)]
pub struct SurfaceState {
    pub mask: CommitedMask,

    pub buffer: Option<(BufferId, u32, u32)>,
    pub opaque_region: Option<pixman::Region32>,
    pub input_region: Option<pixman::Region32>,
    pub subsurfaces: Vec<SubsurfaceNode>,
    pub frame_cbs: Vec<WlCallback>,
}

impl SurfaceState {
    pub fn apply_to_and_clear(&mut self, dst: &mut Self, state: &mut State) {
        if self.mask.empty() {
            return;
        }
        dst.mask.0 |= self.mask.0;
        if self.mask.contains(CommittedMaskBit::Buffer) {
            if let Some((old_buf, _, _)) = dst.buffer {
                state.backend.buffer_unlock(old_buf);
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
        }
    }

    pub fn get_bounding_box(&self) -> Option<pixman::Box32> {
        let cur = self.cur.borrow();
        let (_buf, w, h) = cur.buffer?;
        let mut bbox = pixman::Box32 {
            x1: 0,
            y1: 0,
            x2: w as i32,
            y2: h as i32,
        };
        for sub in &cur.subsurfaces {
            if let Some(sub_box) = sub.surface.get_bounding_box() {
                bbox.x1 = bbox.x1.min(sub.position.0 + sub_box.x1);
                bbox.x2 = bbox.x2.max(sub.position.0 + sub_box.x2);
                bbox.y1 = bbox.y1.min(sub.position.1 + sub_box.y1);
                bbox.y2 = bbox.y2.max(sub.position.1 + sub_box.y2);
            }
        }
        Some(bbox)
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
    fn on_bind(&self, _client: &mut Client) {
        self.set_callback(|ctx| {
            use wl_compositor::Request;
            match ctx.request {
                Request::CreateSurface(wl) => {
                    wl.set_callback(wl_surface_cb);
                    ctx.client
                        .compositor
                        .surfaces
                        .insert(wl.id(), Rc::new(Surface::new(wl)));
                }
                Request::CreateRegion(wl) => {
                    wl.set_callback(wl_region_cb);
                    ctx.client
                        .compositor
                        .regions
                        .insert(wl.id(), pixman::Region32::default());
                }
            }
            Ok(())
        });
    }
}

impl IsGlobal for WlSubcompositor {
    fn on_bind(&self, _client: &mut Client) {
        self.set_callback(|ctx| {
            use wl_subcompositor::Request;
            match ctx.request {
                Request::Destroy => (),
                Request::GetSubsurface(args) => {
                    args.id.set_callback(wl_subsurface_cb);
                    let surface = ctx
                        .client
                        .compositor
                        .surfaces
                        .get(&args.surface)
                        .ok_or_else(|| io::Error::other("invalid id in get_subsurface"))?;
                    let parent = ctx
                        .client
                        .compositor
                        .surfaces
                        .get(&args.parent)
                        .ok_or_else(|| io::Error::other("invalid id in get_subsurface"))?;
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
                        .insert(args.id.id(), subsurface);
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

fn wl_surface_cb(ctx: RequestCtx<WlSurface>) -> io::Result<()> {
    let surface = ctx
        .client
        .compositor
        .surfaces
        .get(&(ctx.proxy.id()))
        .unwrap()
        .clone();

    use wl_surface::Request;
    match ctx.request {
        Request::Destroy => {
            eprintln!("destroying {:?}", ctx.proxy);
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
            *surface.pending_buffer.borrow_mut() = args
                .buffer
                .map(|id| ctx.client.conn.get_object(id).unwrap().try_into().unwrap());
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
                Some(reg_id) => Some(
                    ctx.client
                        .compositor
                        .regions
                        .get(&reg_id)
                        .ok_or_else(|| io::Error::other("invalid region id"))?
                        .clone(),
                ),
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
                Some(reg_id) => Some(
                    ctx.client
                        .compositor
                        .regions
                        .get(&reg_id)
                        .ok_or_else(|| io::Error::other("invalid region id"))?
                        .clone(),
                ),
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
                                .client
                                .shm
                                .wl_id_to_buffer_id
                                .get(&pending_buffer.id())
                                .copied()
                                .unwrap();
                            ctx.state.backend.buffer_lock(buf_id);
                            let (width, height) = ctx.state.backend.get_buffer_size(buf_id);
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
                    xdg_shell::surface_commit(ctx.client, ctx.state, &xdg)?;
                }
            }
        }
        Request::SetBufferTransform(_) => todo!(),
        Request::SetBufferScale(scale) => {
            assert_eq!(scale, 1);
        }
        Request::DamageBuffer(_) => (),
        Request::Offset(_) => todo!(),
    }
    Ok(())
}

fn wl_subsurface_cb(ctx: RequestCtx<WlSubsurface>) -> io::Result<()> {
    let subsurface = ctx
        .client
        .compositor
        .subsurfaces
        .get(&ctx.proxy.id())
        .unwrap();

    use wl_subsurface::Request;
    match ctx.request {
        Request::Destroy => {
            eprintln!("destroying {:?}", ctx.proxy);
            *subsurface.surface.upgrade().unwrap().role.borrow_mut() = SurfaceRole::None;
            let subsurface = ctx
                .client
                .compositor
                .subsurfaces
                .remove(&ctx.proxy.id())
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
            ctx.client.compositor.regions.remove(&ctx.proxy.id());
        }
        Request::Add(args) => {
            let region = ctx
                .client
                .compositor
                .regions
                .get_mut(&ctx.proxy.id())
                .unwrap();
            *region = region.union_rect(args.x, args.y, args.width as u32, args.height as u32);
        }
        Request::Subtract(args) => {
            let region = ctx
                .client
                .compositor
                .regions
                .get_mut(&ctx.proxy.id())
                .unwrap();
            let other =
                pixman::Region32::init_rect(args.x, args.y, args.width as u32, args.height as u32);
            *region = region.subtract(&other);
        }
    }
    Ok(())
}

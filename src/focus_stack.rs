use std::rc::{Rc, Weak};

use crate::client::ClientId;
use crate::globals::compositor::Surface;
use crate::globals::xdg_shell::toplevel::XdgToplevelRole;
use crate::seat::Seat;
use crate::wayland_core::Proxy;

#[derive(Default)]
pub struct FocusStack {
    inner: Vec<Weak<XdgToplevelRole>>,
}

pub struct SurfaceUnderCursor {
    pub sx: f32,
    pub sy: f32,
    pub surf: Rc<Surface>,
    pub toplevel_idx: usize,
}

impl FocusStack {
    pub fn surface_at(&self, x: f32, y: f32) -> Option<SurfaceUnderCursor> {
        fn surface_at(surf: Rc<Surface>, x: f32, y: f32) -> Option<(Rc<Surface>, f32, f32)> {
            if let Some(xdg) = surf.get_xdg_surface() {
                if let Some(popup) = &*xdg.popup.borrow() {
                    let par_geom = xdg.get_window_geometry().unwrap();
                    let pop_geom = popup
                        .xdg_surface
                        .upgrade()
                        .unwrap()
                        .get_window_geometry()
                        .unwrap();
                    if let Some(res) = surface_at(
                        popup.wl_surface.upgrade().unwrap(),
                        x - (popup.x.get() + par_geom.x - pop_geom.x) as f32,
                        y - (popup.y.get() + par_geom.y - pop_geom.y) as f32,
                    ) {
                        return Some(res);
                    }
                }
            }
            for subs in surf.cur.borrow().subsurfaces.iter().rev() {
                if let Some(res) = surface_at(
                    subs.surface.clone(),
                    x - subs.position.0 as f32,
                    y - subs.position.1 as f32,
                ) {
                    return Some(res);
                }
            }
            let buf_transform = surf.buf_transform()?;
            let ok = x >= 0.0
                && y >= 0.0
                && x < buf_transform.dst_width() as f32
                && y < buf_transform.dst_height() as f32
                && surf.cur.borrow().input_region.as_ref().is_none_or(|reg| {
                    reg.contains_point(x.round() as i32, y.round() as i32)
                        .is_some()
                });
            ok.then_some((surf, x, y))
        }
        for (toplevel_idx, toplevel) in self.inner.iter().enumerate().rev() {
            let tl = toplevel.upgrade().unwrap();
            let xdg = tl.xdg_surface.upgrade().unwrap();
            let Some(geom) = xdg.get_window_geometry() else { continue };
            if let Some((surf, sx, sy)) = surface_at(
                tl.wl_surface.upgrade().unwrap(),
                x - (tl.x.get() - geom.x) as f32,
                y - (tl.y.get() - geom.y) as f32,
            ) {
                return Some(SurfaceUnderCursor {
                    sx,
                    sy,
                    surf,
                    toplevel_idx,
                });
            }
        }
        None
    }

    pub fn top(&self) -> Option<Rc<XdgToplevelRole>> {
        self.inner.last().map(|x| x.upgrade().unwrap())
    }

    pub fn focus_i(&mut self, i: usize, seat: &mut Seat) {
        let tl = self.inner.remove(i).upgrade().unwrap();
        seat.keyboard
            .focus_surface(Some(tl.wl_surface.upgrade().unwrap().wl.clone()));
        self.inner.push(Rc::downgrade(&tl));
    }

    pub fn get_i(&mut self, i: usize) -> Option<Rc<XdgToplevelRole>> {
        self.inner.get(i).map(|x| x.upgrade().unwrap())
    }

    pub fn remove(&mut self, toplevel: &XdgToplevelRole) {
        self.inner
            .retain(|s| s.upgrade().unwrap().wl != toplevel.wl);
    }

    pub fn remove_client(&mut self, client_id: ClientId) {
        self.inner
            .retain(|s| s.upgrade().unwrap().wl.client_id() != client_id);
    }

    pub fn push(&mut self, toplevel: &Rc<XdgToplevelRole>) {
        self.inner.push(Rc::downgrade(toplevel));
    }

    pub fn inner(&self) -> &[Weak<XdgToplevelRole>] {
        &self.inner
    }
}

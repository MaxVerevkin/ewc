use std::rc::{Rc, Weak};

use crate::client::ClientId;
use crate::globals::compositor::Surface;
use crate::globals::xdg_shell::XdgToplevelRole;
use crate::wayland_core::Proxy;

#[derive(Default)]
pub struct FocusStack {
    inner: Vec<Weak<XdgToplevelRole>>,
}

impl FocusStack {
    pub fn toplevel_at(&self, x: f32, y: f32) -> Option<usize> {
        for (toplevel_i, toplevel) in self.inner.iter().enumerate().rev() {
            let tl = toplevel.upgrade().unwrap();
            let xdg = tl.xdg_surface.upgrade().unwrap();
            let Some(geom) = xdg.get_window_geometry() else { continue };
            let tlx = x - tl.x.get() as f32;
            let tly = y - tl.y.get() as f32;
            if tlx >= 0.0
                && tly >= 0.0
                && tlx < geom.width.get() as f32
                && tly < geom.height.get() as f32
            {
                return Some(toplevel_i);
            }
        }
        None
    }

    pub fn surface_at(&self, x: i32, y: i32) -> Option<(usize, Rc<Surface>, f32, f32)> {
        fn surface_at(surf: Rc<Surface>, x: i32, y: i32) -> Option<(Rc<Surface>, i32, i32)> {
            for subs in surf.cur.borrow().subsurfaces.iter().rev() {
                if let Some(res) = surface_at(
                    subs.surface.clone(),
                    x - subs.position.0,
                    y - subs.position.1,
                ) {
                    return Some(res);
                }
            }
            let (_, w, h) = surf.cur.borrow().buffer?;
            let ok = x >= 0
                && y >= 0
                && x < w as i32
                && y < h as i32
                && surf
                    .cur
                    .borrow()
                    .input_region
                    .as_ref()
                    .map_or(true, |reg| reg.contains_point(x, y).is_some());
            ok.then_some((surf, x, y))
        }
        for (toplevel_i, toplevel) in self.inner.iter().enumerate().rev() {
            let tl = toplevel.upgrade().unwrap();
            let xdg = tl.xdg_surface.upgrade().unwrap();
            let Some(geom) = xdg.get_window_geometry() else { continue };
            if let Some((surf, sx, sy)) = surface_at(
                tl.wl_surface.upgrade().unwrap(),
                x - tl.x.get() + geom.x,
                y - tl.y.get() + geom.y,
            ) {
                return Some((toplevel_i, surf, sx as f32, sy as f32));
            }
        }
        None
    }

    pub fn top(&self) -> Option<Rc<XdgToplevelRole>> {
        self.inner.last().map(|x| x.upgrade().unwrap())
    }

    pub fn focus_i(&mut self, i: usize) {
        let tl = self.inner.remove(i);
        self.inner.push(tl);
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

use std::io;
use std::num::NonZeroU32;

use crate::client::RequestCtx;
use crate::protocol::xdg_positioner::ConstraintAdjustment;
use crate::protocol::*;

#[derive(Debug, Clone, Copy)]
pub struct Positioner {
    pub size: (NonZeroU32, NonZeroU32),
    pub anchor_rect: (i32, i32, i32, i32),
    pub offset: (i32, i32),
    pub anchor: Option<xdg_positioner::Anchor>,
    pub gravity: Option<xdg_positioner::Gravity>,
    pub contraint_adjustment: ConstraintAdjustment,
    pub reactive: bool,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RawPositioner {
    size: Option<(NonZeroU32, NonZeroU32)>,
    anchor_rect: Option<(i32, i32, i32, i32)>,
    offset: (i32, i32),
    anchor: Option<xdg_positioner::Anchor>,
    gravity: Option<xdg_positioner::Gravity>,
    contraint_adjustment: ConstraintAdjustment,
    reactive: bool,
}

impl Positioner {
    pub fn from_raw(raw: RawPositioner) -> io::Result<Self> {
        let size = raw
            .size
            .ok_or_else(|| io::Error::other("positioner size not set"))?;
        let anchor_rect = raw
            .anchor_rect
            .ok_or_else(|| io::Error::other("positioner anchor rect not set"))?;
        Ok(Self {
            size,
            anchor_rect,
            offset: raw.offset,
            anchor: raw.anchor,
            gravity: raw.gravity,
            contraint_adjustment: raw.contraint_adjustment,
            reactive: raw.reactive,
        })
    }

    pub fn get_position(&self) -> (i32, i32) {
        let (ax, ay, aw, ah) = self.anchor_rect;
        let (x, y) = match self.anchor.unwrap_or(xdg_positioner::Anchor::None) {
            xdg_positioner::Anchor::None => (ax + aw / 2, ay + ah / 2),
            xdg_positioner::Anchor::Top => (ax + aw / 2, ay),
            xdg_positioner::Anchor::Bottom => (ax + aw / 2, ay + ah),
            xdg_positioner::Anchor::Left => (ax, ay + ah / 2),
            xdg_positioner::Anchor::Right => (ax + aw, ay + ah / 2),
            xdg_positioner::Anchor::TopLeft => (ax, ay),
            xdg_positioner::Anchor::BottomLeft => (ax, ay + ah),
            xdg_positioner::Anchor::TopRight => (ax + aw, ay),
            xdg_positioner::Anchor::BottomRight => (ax + aw, ay + ah),
        };
        let w = self.size.0.get() as i32;
        let h = self.size.1.get() as i32;
        match self.gravity.unwrap_or(xdg_positioner::Gravity::None) {
            xdg_positioner::Gravity::None => (x - w / 2, y - h / 2),
            xdg_positioner::Gravity::Top => (x - w / 2, y - h),
            xdg_positioner::Gravity::Bottom => (x - w / 2, y),
            xdg_positioner::Gravity::Left => (x - w, y - h / 2),
            xdg_positioner::Gravity::Right => (x, y - h / 2),
            xdg_positioner::Gravity::TopLeft => (x - w, y - h),
            xdg_positioner::Gravity::BottomLeft => (x - w, y),
            xdg_positioner::Gravity::TopRight => (x, y - h),
            xdg_positioner::Gravity::BottomRight => (x, y),
        }
    }
}

pub(super) fn xdg_positioner_cb(ctx: RequestCtx<XdgPositioner>) -> io::Result<()> {
    let positioner = ctx
        .client
        .compositor
        .xdg_positioners
        .get_mut(&ctx.proxy)
        .unwrap();

    use xdg_positioner::Request;
    match ctx.request {
        Request::Destroy => {
            ctx.client.compositor.xdg_positioners.remove(&ctx.proxy);
        }
        Request::SetSize(args) => {
            let w = u32::try_from(args.width)
                .ok()
                .and_then(NonZeroU32::new)
                .ok_or_else(|| io::Error::other("positioner: invalid size"))?;
            let h = u32::try_from(args.height)
                .ok()
                .and_then(NonZeroU32::new)
                .ok_or_else(|| io::Error::other("positioner: invalid size"))?;
            positioner.size = Some((w, h));
        }
        Request::SetAnchorRect(args) => {
            positioner.anchor_rect = Some((args.x, args.y, args.width, args.height));
        }
        Request::SetAnchor(anchor) => {
            positioner.anchor = Some(anchor);
        }
        Request::SetGravity(gravity) => {
            positioner.gravity = Some(gravity);
        }
        Request::SetConstraintAdjustment(adjustment) => {
            positioner.contraint_adjustment = adjustment;
        }
        Request::SetOffset(args) => {
            positioner.offset = (args.x, args.y);
        }
        Request::SetReactive => {
            positioner.reactive = true;
        }
        Request::SetParentSize(args) => {
            eprintln!(
                "set_parent_size is ignored (got {}x{})",
                args.parent_width, args.parent_height
            );
        }
        Request::SetParentConfigure(serial) => {
            eprintln!("set_parent_configure is ignored (got {serial})");
        }
    }
    Ok(())
}

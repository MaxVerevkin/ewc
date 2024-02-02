use std::io;

use super::{GlobalsManager, IsGlobal};
use crate::backend::Color;
use crate::client::{Client, RequestCtx};
use crate::protocol::*;
use crate::{Proxy, State};

#[derive(Default)]
pub struct SinglePixelBufferManager {
    buffers: Vec<WlBuffer>,
}

impl SinglePixelBufferManager {
    pub fn register_global(globals: &mut GlobalsManager) {
        globals.add_global::<WpSinglePixelBufferManagerV1>(1);
    }

    pub fn destroy(self, state: &mut State) {
        for buffer in self.buffers {
            state
                .backend
                .renderer_state()
                .buffer_resource_destroyed(buffer);
        }
    }
}

impl IsGlobal for WpSinglePixelBufferManagerV1 {
    fn on_bind(&self, _client: &mut Client, _state: &mut State) {
        self.set_callback(|ctx| {
            use wp_single_pixel_buffer_manager_v1::Request;
            match ctx.request {
                Request::Destroy => (),
                Request::CreateU32RgbaBuffer(args) => {
                    args.id.set_callback(wl_buffer_cb);
                    ctx.client
                        .single_pixel_buffer_manager
                        .buffers
                        .push(args.id.clone());
                    ctx.state.backend.renderer_state().create_single_pix_buffer(
                        Color::from_rgba32(args.r, args.g, args.b, args.a),
                        args.id,
                    );
                }
            }
            Ok(())
        });
    }
}

fn wl_buffer_cb(ctx: RequestCtx<WlBuffer>) -> io::Result<()> {
    let wl_buffer::Request::Destroy = ctx.request;
    ctx.client
        .single_pixel_buffer_manager
        .buffers
        .retain(|x| *x != ctx.proxy);
    ctx.state
        .backend
        .renderer_state()
        .buffer_resource_destroyed(ctx.proxy);
    Ok(())
}

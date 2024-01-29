use std::collections::HashMap;
use std::io;

use super::IsGlobal;
use crate::backend::{ShmBufferSpec, ShmPoolId};
use crate::client::RequestCtx;
use crate::protocol::*;
use crate::wayland_core::{ObjectId, Proxy};
use crate::{Client, State};

pub struct Shm {
    pub wl_id_to_shm_id: HashMap<ObjectId, ShmPoolId>,
    pub wl_buffers: Vec<WlBuffer>,
}

impl Shm {
    pub fn new() -> Self {
        Self {
            wl_id_to_shm_id: HashMap::new(),
            wl_buffers: Vec::new(),
        }
    }

    pub fn destroy(self, state: &mut State) {
        for buffer in self.wl_buffers {
            state
                .backend
                .renderer_state()
                .buffer_resource_destroyed(buffer);
        }
        for &pool_id in self.wl_id_to_shm_id.values() {
            state
                .backend
                .renderer_state()
                .shm_pool_resource_destroyed(pool_id);
        }
    }
}

impl IsGlobal for WlShm {
    fn on_bind(&self, _client: &mut Client, _state: &mut State) {
        self.format(wl_shm::Format::Argb8888);
        self.format(wl_shm::Format::Abgr8888);
        self.set_callback(wl_shm_cb);
    }
}

fn wl_shm_cb(ctx: RequestCtx<WlShm>) -> io::Result<()> {
    use wl_shm::Request;
    match ctx.request {
        Request::CreatePool(args) => {
            args.id.set_callback(wl_shm_pool_cb);
            if args.size <= 0 {
                return Err(io::Error::other("poll must be greater than zero"));
            }
            let shm_id = ctx
                .state
                .backend
                .renderer_state()
                .create_shm_pool(args.fd, args.size as usize);
            ctx.client.shm.wl_id_to_shm_id.insert(args.id.id(), shm_id);
        }
    }
    Ok(())
}

fn wl_shm_pool_cb(ctx: RequestCtx<WlShmPool>) -> io::Result<()> {
    use wl_shm_pool::Request;
    match ctx.request {
        Request::CreateBuffer(args) => {
            args.id.set_callback(wl_buffer_cb);
            ctx.client.shm.wl_buffers.push(args.id.clone());
            let pool_id = ctx.client.shm.wl_id_to_shm_id[&ctx.proxy.id()];
            ctx.state.backend.renderer_state().create_shm_buffer(
                ShmBufferSpec {
                    pool_id,
                    offset: args.offset as u32,
                    width: args.width as u32,
                    height: args.height as u32,
                    stride: args.stride as u32,
                    wl_format: args.format as u32,
                },
                args.id,
            );
        }
        Request::Destroy => {
            let pool_id = ctx
                .client
                .shm
                .wl_id_to_shm_id
                .remove(&ctx.proxy.id())
                .unwrap();
            ctx.state
                .backend
                .renderer_state()
                .shm_pool_resource_destroyed(pool_id);
        }
        Request::Resize(new_size) => {
            if new_size > 0 {
                let pool_id = ctx.client.shm.wl_id_to_shm_id[&ctx.proxy.id()];
                ctx.state
                    .backend
                    .renderer_state()
                    .resize_shm_pool(pool_id, new_size as usize);
            }
        }
    }
    Ok(())
}

fn wl_buffer_cb(ctx: RequestCtx<WlBuffer>) -> io::Result<()> {
    let wl_buffer::Request::Destroy = ctx.request;
    ctx.client.shm.wl_buffers.retain(|x| *x != ctx.proxy);
    ctx.state
        .backend
        .renderer_state()
        .buffer_resource_destroyed(ctx.proxy);
    Ok(())
}

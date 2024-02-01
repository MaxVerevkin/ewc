use std::collections::hash_map;
use std::io;
use std::os::fd::OwnedFd;

use super::IsGlobal;
use crate::client::RequestCtx;
use crate::protocol::*;
use crate::wayland_core::Proxy;
use crate::{Client, State};

#[derive(Default)]
pub struct Shm {
    pub shm_pools: Vec<WlShmPool>,
    pub wl_buffers: Vec<WlBuffer>,
}

pub struct ShmPool {
    pub memmap: memmap2::Mmap,
    pub size: usize,
    pub refcnt: usize,
}

pub struct ShmBufferSpec {
    pub pool: WlShmPool,
    pub offset: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub wl_format: wl_shm::Format,
}

impl ShmPool {
    fn new(fd: OwnedFd, size: usize) -> io::Result<Self> {
        Ok(Self {
            memmap: unsafe { memmap2::MmapOptions::new().len(size).map(&fd)? },
            size,
            refcnt: 0,
        })
    }
}

impl Shm {
    pub fn destroy(self, state: &mut State) {
        for buffer in self.wl_buffers {
            state
                .backend
                .renderer_state()
                .buffer_resource_destroyed(buffer);
        }
        for pool in self.shm_pools {
            state.backend.renderer_state().get_shm_state().remove(&pool);
        }
    }
}

impl IsGlobal for WlShm {
    fn on_bind(&self, _client: &mut Client, state: &mut State) {
        for &format in state.backend.renderer_state().supported_shm_formats() {
            self.format(format);
        }
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
            ctx.state
                .backend
                .renderer_state()
                .get_shm_state()
                .insert(args.id.clone(), ShmPool::new(args.fd, args.size as usize)?);
            ctx.client.shm.shm_pools.push(args.id);
        }
    }
    Ok(())
}

fn wl_shm_pool_cb(ctx: RequestCtx<WlShmPool>) -> io::Result<()> {
    use wl_shm_pool::Request;
    match ctx.request {
        Request::CreateBuffer(args) => {
            if !ctx
                .state
                .backend
                .renderer_state()
                .supported_shm_formats()
                .contains(&args.format)
            {
                return Err(io::Error::other("provided unsupported shm format"));
            }
            args.id.set_callback(wl_buffer_cb);
            ctx.client.shm.wl_buffers.push(args.id.clone());
            ctx.state.backend.renderer_state().create_shm_buffer(
                ShmBufferSpec {
                    pool: ctx.proxy,
                    offset: args.offset as u32,
                    width: args.width as u32,
                    height: args.height as u32,
                    stride: args.stride as u32,
                    wl_format: args.format,
                },
                args.id,
            );
        }
        Request::Destroy => {
            ctx.client.shm.shm_pools.retain(|x| *x != ctx.proxy);
            match ctx
                .state
                .backend
                .renderer_state()
                .get_shm_state()
                .entry(ctx.proxy)
            {
                hash_map::Entry::Vacant(_) => unreachable!(),
                hash_map::Entry::Occupied(shm_pool) => {
                    if shm_pool.get().refcnt == 0 {
                        shm_pool.remove();
                    }
                }
            }
        }
        Request::Resize(new_size) => {
            if new_size > 0 {
                let new_size = new_size as usize;
                let pool = ctx
                    .state
                    .backend
                    .renderer_state()
                    .get_shm_state()
                    .get_mut(&ctx.proxy)
                    .unwrap();
                if new_size > pool.size {
                    pool.size = new_size;
                    unsafe {
                        pool.memmap
                            .remap(new_size, memmap2::RemapOptions::new().may_move(true))?
                    };
                }
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

use std::collections::HashMap;
use std::io;
use std::os::fd::OwnedFd;

use eglgbm::Fourcc;

use super::{GlobalsManager, IsGlobal};
use crate::client::{Client, RequestCtx};
use crate::protocol::*;
use crate::{Proxy, State};

#[derive(Default)]
pub struct LinuxDmabuf {
    params: HashMap<ZwpLinuxBufferParamsV1, Params>,
    buffers: Vec<WlBuffer>,
}

struct Params {
    used: bool,
    planes: [Option<Plane>; 4],
}

#[derive(Debug)]
pub struct Plane {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
    pub modifier: u64,
}

#[derive(Debug)]
pub struct DmaBufSpec {
    pub width: u32,
    pub height: u32,
    pub format: Fourcc,
    pub planes: Vec<Plane>,
}

impl LinuxDmabuf {
    pub fn register_global(globals: &mut GlobalsManager) {
        globals.add_global::<ZwpLinuxDmabufV1>(3);
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

impl IsGlobal for ZwpLinuxDmabufV1 {
    fn on_bind(&self, _client: &mut Client, state: &mut State) {
        self.set_callback(linux_dmabuf_cb);
        if self.version() < 4 {
            for (format, mods) in state
                .backend
                .renderer_state()
                .supported_dma_buf_formats()
                .unwrap()
            {
                self.format(format.0);
                if self.version() == 3 {
                    for &modifier in mods {
                        self.modifier(format.0, (modifier >> 32) as u32, modifier as u32);
                    }
                }
            }
        }
    }
}

fn linux_dmabuf_cb(ctx: RequestCtx<ZwpLinuxDmabufV1>) -> io::Result<()> {
    use zwp_linux_dmabuf_v1::Request;
    match ctx.request {
        Request::Destroy => (),
        Request::CreateParams(params) => {
            params.set_callback(params_cb);
            ctx.client.linux_dambuf.params.insert(
                params,
                Params {
                    used: false,
                    planes: [None, None, None, None],
                },
            );
        }
        Request::GetDefaultFeedback(_) => todo!(),
        Request::GetSurfaceFeedback(_) => todo!(),
    }
    Ok(())
}

fn params_cb(ctx: RequestCtx<ZwpLinuxBufferParamsV1>) -> io::Result<()> {
    use zwp_linux_buffer_params_v1::Request;
    match ctx.request {
        Request::Destroy => {
            ctx.client.linux_dambuf.params.remove(&ctx.proxy);
        }
        Request::Add(args) => {
            let params = ctx.client.linux_dambuf.params.get_mut(&ctx.proxy).unwrap();
            if params.used {
                return Err(io::Error::other("params already used"));
            }
            if args.plane_idx > 3 {
                return Err(io::Error::other("plane index out of bounds"));
            }
            if params.planes[args.plane_idx as usize].is_some() {
                return Err(io::Error::other("plane with this index already set"));
            }
            params.planes[args.plane_idx as usize] = Some(Plane {
                fd: args.fd,
                offset: args.offset,
                stride: args.stride,
                modifier: ((args.modifier_hi as u64) << 32) | args.modifier_lo as u64,
            });
        }
        Request::Create(_) => todo!(),
        Request::CreateImmed(args) => {
            args.buffer_id.set_callback(wl_buffer_cb);
            let params = ctx.client.linux_dambuf.params.get_mut(&ctx.proxy).unwrap();
            assert_eq!(
                args.flags,
                zwp_linux_buffer_params_v1::Flags::empty(),
                "not implemented"
            );
            if params.used {
                return Err(io::Error::other("params already used"));
            }
            if params.planes.iter().all(|x| x.is_none()) {
                return Err(io::Error::other("params with zero planes"));
            }
            if args.width < 1 || args.height < 1 {
                return Err(io::Error::other("invalid buffer size"));
            }
            params.used = true;
            let spec = DmaBufSpec {
                width: args.width as u32,
                height: args.height as u32,
                format: Fourcc(args.format),
                planes: params.planes.iter_mut().flat_map(|x| x.take()).collect(),
            };
            ctx.client.linux_dambuf.buffers.push(args.buffer_id.clone());
            ctx.state
                .backend
                .renderer_state()
                .create_dma_buffer(spec, args.buffer_id);
        }
    }
    Ok(())
}

fn wl_buffer_cb(ctx: RequestCtx<WlBuffer>) -> io::Result<()> {
    let wl_buffer::Request::Destroy = ctx.request;
    ctx.client.linux_dambuf.buffers.retain(|x| *x != ctx.proxy);
    ctx.state
        .backend
        .renderer_state()
        .buffer_resource_destroyed(ctx.proxy);
    Ok(())
}

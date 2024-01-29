use std::collections::HashMap;
use std::io;
use std::os::fd::OwnedFd;

use drm::buffer::{DrmFourcc, DrmModifier};

use crate::client::{Client, RequestCtx};
use crate::globals::IsGlobal;
use crate::protocol::*;
use crate::{Proxy, State};

#[derive(Default)]
pub struct LinuxDmabufState {
    params: HashMap<ZwpLinuxBufferParamsV1, Params>,
}

#[derive(Debug)]
struct Params {
    planes: Vec<Plane>,
}

#[derive(Debug)]
pub struct Plane {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
    pub modifier: u64,
}

#[derive(Debug)]
pub struct DmaBufferSpec {
    pub planes: Vec<Plane>,
    pub width: u32,
    pub height: u32,
    pub fourcc_format: u32,
    pub flags: zwp_linux_buffer_params_v1::Flags,
}

impl IsGlobal for ZwpLinuxDmabufV1 {
    fn on_bind(&self, _client: &mut Client, _state: &mut State) {
        if self.version() < 4 {
            for format in [DrmFourcc::Xrgb8888] {
                self.format(format as u32);
                if self.version() == 3 {
                    for modif in [DrmModifier::Linear, DrmModifier::Invalid] {
                        let modif = u64::from(modif);
                        let hi = (modif >> 32) & 0xFF_FF_FF_FF;
                        let lo = modif & 0xFF_FF_FF_FF;
                        self.modifier(format as u32, hi as u32, lo as u32);
                    }
                }
            }
        }
        self.set_callback(|ctx| {
            use zwp_linux_dmabuf_v1::Request;
            match ctx.request {
                Request::Destroy => (),
                Request::CreateParams(params) => {
                    params.set_callback(params_cb);
                    ctx.client
                        .linux_dmabuf_state
                        .params
                        .insert(params, Params { planes: Vec::new() });
                }
                Request::GetDefaultFeedback(_) => unreachable!(),
                Request::GetSurfaceFeedback(_) => unreachable!(),
            }
            Ok(())
        });
    }
}

fn params_cb(ctx: RequestCtx<ZwpLinuxBufferParamsV1>) -> io::Result<()> {
    use zwp_linux_buffer_params_v1::Request;
    match ctx.request {
        Request::Destroy => (),
        Request::Add(args) => {
            let params = ctx
                .client
                .linux_dmabuf_state
                .params
                .get_mut(&ctx.proxy)
                .unwrap();
            assert_eq!(args.plane_idx, params.planes.len() as u32);
            params.planes.push(Plane {
                fd: args.fd,
                offset: args.offset,
                stride: args.stride,
                modifier: ((args.modifier_hi as u64) << 32) | args.modifier_lo as u64,
            });
        }
        Request::Create(_) => todo!(),
        Request::CreateImmed(args) => {
            args.buffer_id.set_callback(wl_buffer_cb);
            let params = ctx
                .client
                .linux_dmabuf_state
                .params
                .remove(&ctx.proxy)
                .unwrap();
            let buffer_id = ctx.state.backend.create_dma_buffer(
                DmaBufferSpec {
                    planes: params.planes,
                    width: args.width as u32,
                    height: args.height as u32,
                    fourcc_format: args.format,
                    flags: args.flags,
                },
                args.buffer_id.clone(),
            );
            ctx.client.buffer_map.insert(args.buffer_id, buffer_id);
        }
    }
    Ok(())
}

fn wl_buffer_cb(ctx: RequestCtx<WlBuffer>) -> io::Result<()> {
    let wl_buffer::Request::Destroy = ctx.request;
    todo!();
    Ok(())
}

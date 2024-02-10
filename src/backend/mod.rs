use std::any::Any;
use std::collections::HashMap;
use std::io;
use std::num::NonZeroU64;
use std::os::fd::{OwnedFd, RawFd};

pub mod drmkms;
mod gl46_renderer;
mod pixman_renderer;
pub mod wayland;

use crate::buffer_transform::BufferTransform;
use crate::config::PointerConfig;
use crate::globals::linux_dmabuf::DmaBufSpec;
use crate::globals::shm::{ShmBufferSpec, ShmPool};
use crate::protocol;

pub trait Backend {
    fn register_fds_with(
        &self,
        reg: &'_ mut dyn FnMut(RawFd, u32) -> io::Result<()>,
    ) -> io::Result<()>;
    fn poll(&mut self, data: u32) -> io::Result<()>;
    fn next_event(&mut self) -> Option<BackendEvent>;
    fn switch_vt(&mut self, vt: u32);
    fn pointer_get_name(&self, id: PointerId) -> Option<&str>;
    fn pointer_configure(&mut self, id: PointerId, config: &PointerConfig);
    fn renderer_state(&mut self) -> &mut dyn RendererState;
    fn render_frame(&mut self, clear: Color, render_list: &[RenderNode], time: u32);
}

pub trait RendererState: Any {
    fn supported_shm_formats(&self) -> &[protocol::wl_shm::Format];
    fn supported_dma_buf_formats(&self) -> Option<&eglgbm::FormatTable>;
    fn get_shm_state(&mut self) -> &mut HashMap<protocol::WlShmPool, ShmPool>;
    fn create_argb8_texture(&mut self, width: u32, height: u32, bytes: &[u8]) -> BufferId;
    fn create_shm_buffer(&mut self, spec: ShmBufferSpec, resource: protocol::WlBuffer);
    fn create_dma_buffer(&mut self, spec: DmaBufSpec, resource: protocol::WlBuffer);
    fn create_single_pix_buffer(&mut self, color: Color, resource: protocol::WlBuffer);
    fn buffer_commited(&mut self, buffer_resource: protocol::WlBuffer) -> BufferId;
    fn get_buffer_size(&self, buffer_id: BufferId) -> (u32, u32);
    fn buffer_unlock(&mut self, buffer_id: BufferId);
    fn buffer_resource_destroyed(&mut self, resource: protocol::WlBuffer);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BufferId(NonZeroU64);
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyboardId(NonZeroU64);
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PointerId(NonZeroU64);
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InputTimestamp(u32);

impl InputTimestamp {
    pub fn get(self) -> u32 {
        self.0
    }
}

trait Frame {
    fn clear(&mut self, r: f32, g: f32, b: f32);
    fn render_buffer(
        &mut self,
        opaque_region: Option<&pixman::Region32>,
        alpha: f32,
        buf_transform: BufferTransform,
        x: i32,
        y: i32,
    );
    fn render_rect(&mut self, color: Color, rect: pixman::Rectangle32);

    fn render(&mut self, render_list: &[RenderNode], time: u32) {
        for node in render_list {
            match node {
                RenderNode::Rect(rect, color) => self.render_rect(*color, *rect),
                RenderNode::Buffer {
                    x,
                    y,
                    opaque_region,
                    alpha,
                    buf_transform,
                    frame_callbacks,
                } => {
                    self.render_buffer(opaque_region.as_ref(), *alpha, *buf_transform, *x, *y);
                    for cb in frame_callbacks {
                        cb.done(time);
                    }
                }
            }
        }
    }
}

/// Pre-multiplied RGBA color _or_ a texture coordinate. Negative `a` denotes that this is a texture
/// coordinate, in which case `r` and `g` are the coordinates into a texture at index `b`, and alpha
/// is `-a`. This is done to use a single shader for both colored and textured quads.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct Color {
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

impl Color {
    pub fn from_rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    pub fn from_rgba32(r: u32, g: u32, b: u32, a: u32) -> Self {
        Self {
            r: (r as f64 / u32::MAX as f64) as f32,
            g: (g as f64 / u32::MAX as f64) as f32,
            b: (b as f64 / u32::MAX as f64) as f32,
            a: (a as f64 / u32::MAX as f64) as f32,
        }
    }

    pub fn from_tex_uv(u: f32, v: f32, tex_i: u32, a: f32) -> Self {
        Self {
            r: u,
            g: v,
            b: tex_i as f32,
            a: -a,
        }
    }
}

impl std::ops::Mul<f32> for Color {
    type Output = Self;
    fn mul(mut self, rhs: f32) -> Self::Output {
        self.r *= rhs;
        self.g *= rhs;
        self.b *= rhs;
        self.a *= rhs;
        self
    }
}

pub enum BackendEvent {
    ShutDown,
    Frame,

    NewKeyboard(KeyboardId),
    KeyPressed(KeyboardId, InputTimestamp, u32),
    KeyReleased(KeyboardId, InputTimestamp, u32),
    KeyboardRemoved(KeyboardId),

    NewPointer(PointerId),
    PointerMotionAbsolute(PointerId, InputTimestamp, f32, f32),
    PointerMotionRelative(PointerId, InputTimestamp, f32, f32),
    PointerBtnPress(PointerId, InputTimestamp, u32),
    PointerBtnRelease(PointerId, InputTimestamp, u32),
    PointerAxisVertial(PointerId, InputTimestamp, f32),
    PointerRemoved(PointerId),
}

pub enum RenderNode {
    Rect(pixman::Rectangle32, Color),
    Buffer {
        x: i32,
        y: i32,
        opaque_region: Option<pixman::Region32>,
        alpha: f32,
        buf_transform: BufferTransform,
        frame_callbacks: Vec<protocol::WlCallback>,
    },
}

#[must_use]
fn next_id(id: &mut NonZeroU64) -> NonZeroU64 {
    let val = *id;
    *id = id.checked_add(1).expect("id overflow");
    val
}

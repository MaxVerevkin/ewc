use std::any::Any;
use std::io;
use std::num::NonZeroU64;
use std::os::fd::{OwnedFd, RawFd};

pub mod drmkms;
mod gl46_renderer;
mod pixman_renderer;
pub mod wayland;

use crate::protocol;

pub trait Backend {
    fn register_fds_with(
        &self,
        reg: &'_ mut dyn FnMut(RawFd, u32) -> io::Result<()>,
    ) -> io::Result<()>;
    fn poll(&mut self, data: u32) -> io::Result<()>;
    fn next_event(&mut self) -> Option<BackendEvent>;
    fn switch_vt(&mut self, vt: u32);
    fn renderer_state(&mut self) -> &mut dyn RendererState;
    fn render_frame(&mut self, f: &mut dyn FnMut(&mut dyn Frame));
}

pub trait RendererState: Any {
    fn supported_shm_formats(&self) -> &[protocol::wl_shm::Format];
    fn create_shm_pool(&mut self, fd: OwnedFd, size: usize, resource: protocol::WlShmPool);
    fn resize_shm_pool(&mut self, pool: protocol::WlShmPool, new_size: usize);
    fn shm_pool_resource_destroyed(&mut self, pool: protocol::WlShmPool);
    fn create_shm_buffer(&mut self, spec: ShmBufferSpec, resource: protocol::WlBuffer);
    fn buffer_commited(&mut self, buffer_resource: protocol::WlBuffer) -> BufferId;
    fn get_buffer_size(&self, buffer_id: BufferId) -> (u32, u32);
    fn buffer_lock(&mut self, buffer_id: BufferId);
    fn buffer_unlock(&mut self, buffer_id: BufferId);
    fn buffer_resource_destroyed(&mut self, resource: protocol::WlBuffer);
}

pub struct ShmBufferSpec {
    pub pool: protocol::WlShmPool,
    pub offset: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub wl_format: protocol::wl_shm::Format,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BufferId(pub NonZeroU64);
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyboardId(pub NonZeroU64);
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PointerId(pub NonZeroU64);

pub trait Frame {
    fn time(&self) -> u32;
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    fn clear(&mut self, r: f32, g: f32, b: f32);
    fn render_buffer(
        &mut self,
        buf: BufferId,
        opaque_region: Option<&pixman::Region32>,
        alpha: f32,
        x: i32,
        y: i32,
    );
    fn render_rect(&mut self, color: Color, rect: pixman::Rectangle32);
}

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
    KeyPressed(KeyboardId, u32),
    KeyReleased(KeyboardId, u32),
    KeyboardRemoved(KeyboardId),

    NewPointer(PointerId),
    PointerMotionAbsolute(PointerId, f32, f32),
    PointerMotionRelative(PointerId, f32, f32),
    PointerBtnPress(PointerId, u32),
    PointerBtnRelease(PointerId, u32),
    PointerAxisVertial(PointerId, f32),
    PointerRemoved(PointerId),
}

#[must_use]
fn next_id(id: &mut NonZeroU64) -> NonZeroU64 {
    let val = *id;
    *id = id.checked_add(1).expect("id overflow");
    val
}

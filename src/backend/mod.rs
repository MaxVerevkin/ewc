use std::io;
use std::num::NonZeroU64;
use std::os::fd::{OwnedFd, RawFd};

pub mod drmkms;
mod pixman_renderer;
pub mod wayland;

pub trait Backend {
    fn register_fds_with(
        &self,
        reg: &'_ mut dyn FnMut(RawFd, u32) -> io::Result<()>,
    ) -> io::Result<()>;
    fn poll(&mut self, data: u32) -> io::Result<()>;
    fn next_event(&mut self) -> Option<BackendEvent>;
    fn switch_vt(&mut self, vt: u32);
    fn create_shm_pool(&mut self, fd: OwnedFd, size: usize) -> ShmPoolId;
    fn resize_shm_pool(&mut self, pool_id: ShmPoolId, new_size: usize);
    fn shm_pool_resource_destroyed(&mut self, pool_id: ShmPoolId);
    fn create_shm_buffer(
        &mut self,
        spec: ShmBufferSpec,
        resource: crate::protocol::WlBuffer,
    ) -> BufferId;
    fn get_buffer_size(&self, buffer_id: BufferId) -> (u32, u32);
    fn buffer_lock(&mut self, buffer_id: BufferId);
    fn buffer_unlock(&mut self, buffer_id: BufferId);
    fn buffer_resource_destroyed(&mut self, buffer_id: BufferId);
    fn render_frame(&mut self, f: &mut dyn FnMut(&mut dyn Frame));
}

pub struct ShmBufferSpec {
    pub pool_id: ShmPoolId,
    pub offset: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub wl_format: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BufferId(pub NonZeroU64);
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShmPoolId(pub NonZeroU64);
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
    fn render_rect(&mut self, r: f32, g: f32, b: f32, a: f32, rect: pixman::Rectangle32);
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

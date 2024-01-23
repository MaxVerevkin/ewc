use std::io;
use std::num::NonZeroU64;
use std::os::fd::{OwnedFd, RawFd};

mod pixman_renderer;
pub mod wayland;

pub trait Backend {
    fn get_fd(&self) -> RawFd;
    fn poll(&mut self) -> io::Result<()>;
    fn next_event(&mut self) -> Option<BackendEvent>;
    fn create_shm_pool(&mut self, fd: OwnedFd, size: usize) -> ShmPoolId;
    fn resize_shm_pool(&mut self, pool_id: ShmPoolId, new_size: usize);
    fn shm_pool_resource_destroyed(&mut self, pool_id: ShmPoolId);
    fn create_shm_buffer(
        &mut self,
        pool_id: ShmPoolId,
        offset: usize,
        wl_format: u32,
        width: u32,
        height: u32,
        stide: u32,
        resource: crate::protocol::WlBuffer,
    ) -> BufferId;
    fn get_buffer_size(&self, buffer_id: BufferId) -> (u32, u32);
    fn buffer_lock(&mut self, buffer_id: BufferId);
    fn buffer_unlock(&mut self, buffer_id: BufferId);
    fn buffer_resource_destroyed(&mut self, buffer_id: BufferId);
    fn render_frame(&mut self, f: &mut dyn FnMut(&mut dyn Frame));
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
    fn render_rect(&mut self, r: f32, g: f32, b: f32, a: f32, x: i32, y: i32, w: u32, h: u32);
}

pub enum BackendEvent {
    ShutDown,
    Frame,

    NewKeyboard(KeyboardId),
    KeyPressed(KeyboardId, u32),
    KeyReleased(KeyboardId, u32),
    KeyboardRemoved(KeyboardId),

    NewPointer(PointerId),
    PointerMotion(PointerId, f32, f32),
    PointerBtnPress(PointerId, u32),
    PointerBtnRelease(PointerId, u32),
    PointerRemoved(PointerId),
}

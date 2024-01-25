use std::collections::HashMap;
use std::num::NonZeroU64;

use super::*;

pub struct RendererState {
    shm_pools: HashMap<ShmPoolId, ShmPool>,
    shm_buffers: HashMap<BufferId, ShmBuffer>,
    next_shm_pool_id: NonZeroU64,
    next_buffer_id: NonZeroU64,
}

pub struct ShmPool {
    memmap: memmap2::Mmap,
    size: usize,
    resource_alive: bool,
}

pub struct ShmBuffer {
    pool_id: ShmPoolId,
    offset: usize,
    width: u32,
    height: u32,
    stride: u32,
    wl_format: u32,
    locks: u32,
    resource: Option<crate::protocol::WlBuffer>,
}

impl RendererState {
    pub fn new() -> Self {
        Self {
            shm_pools: HashMap::new(),
            shm_buffers: HashMap::new(),
            next_shm_pool_id: NonZeroU64::MIN,
            next_buffer_id: NonZeroU64::MIN,
        }
    }

    pub fn create_shm_pool(&mut self, fd: OwnedFd, size: usize) -> ShmPoolId {
        let id = ShmPoolId(next_id(&mut self.next_shm_pool_id));
        self.shm_pools.insert(
            id,
            ShmPool {
                memmap: unsafe { memmap2::MmapOptions::new().len(size).map(&fd).unwrap() },
                size,
                resource_alive: true,
            },
        );
        id
    }

    pub fn resize_shm_pool(&mut self, pool_id: ShmPoolId, new_size: usize) {
        let pool = self.shm_pools.get_mut(&pool_id).unwrap();
        if new_size > pool.size {
            pool.size = new_size;
            unsafe {
                pool.memmap
                    .remap(new_size, memmap2::RemapOptions::new().may_move(true))
                    .unwrap()
            };
        }
    }

    pub fn shm_pool_resource_destroyed(&mut self, pool_id: ShmPoolId) {
        self.shm_pools.get_mut(&pool_id).unwrap().resource_alive = false;
        self.consider_dropping_shm_pool(pool_id);
    }

    pub fn create_shm_buffer(
        &mut self,
        pool_id: ShmPoolId,
        offset: usize,
        wl_format: u32,
        width: u32,
        height: u32,
        stride: u32,
        resource: crate::protocol::WlBuffer,
    ) -> BufferId {
        let id = BufferId(next_id(&mut self.next_buffer_id));
        self.shm_buffers.insert(
            id,
            ShmBuffer {
                pool_id,
                offset,
                width,
                height,
                stride,
                wl_format,
                resource: Some(resource),
                locks: 0,
            },
        );
        id
    }

    pub fn get_buffer_size(&self, buffer_id: BufferId) -> (u32, u32) {
        let buf = &self.shm_buffers[&buffer_id];
        (buf.width, buf.height)
    }

    pub fn buffer_lock(&mut self, buffer_id: BufferId) {
        let buf = self.shm_buffers.get_mut(&buffer_id).unwrap();
        buf.locks += 1;
        // eprintln!("locking {buffer_id:?} (locks = {})", buf.locks);
    }

    pub fn buffer_unlock(&mut self, buffer_id: BufferId) {
        let buf = self.shm_buffers.get_mut(&buffer_id).unwrap();
        buf.locks -= 1;
        // eprintln!("unlocking {buffer_id:?} (locks = {})", buf.locks);
        if buf.locks == 0 {
            if let Some(resource) = &buf.resource {
                resource.release();
            } else {
                self.drop_buffer(buffer_id);
            }
        }
    }

    pub fn buffer_resource_destroyed(&mut self, buffer_id: BufferId) {
        let buf = self.shm_buffers.get_mut(&buffer_id).unwrap();
        buf.resource = None;
        if buf.locks == 0 {
            self.drop_buffer(buffer_id);
        }
    }

    fn consider_dropping_shm_pool(&mut self, pool_id: ShmPoolId) {
        let shm_pool = self.shm_pools.get(&pool_id).unwrap();
        if !shm_pool.resource_alive && self.shm_buffers.values().all(|buf| buf.pool_id != pool_id) {
            self.shm_pools.remove(&pool_id);
        }
    }

    fn drop_buffer(&mut self, buffer_id: BufferId) {
        let buffer = self.shm_buffers.remove(&buffer_id).unwrap();
        assert_eq!(buffer.locks, 0);
        assert!(buffer.resource.is_none());
        self.consider_dropping_shm_pool(buffer.pool_id);
    }
}

pub struct Renderer<'a> {
    image: pixman::Image<'a, 'static>,
    state: &'a RendererState,
}

impl<'a> Renderer<'a> {
    pub fn new(
        state: &'a RendererState,
        bytes: &'a mut [u8],
        width: u32,
        height: u32,
        wl_format: u32,
    ) -> Self {
        Self {
            image: pixman::Image::from_slice_mut(
                wl_format_to_pixman(wl_format).unwrap(),
                width as usize,
                height as usize,
                bytes_to_ints(bytes),
                width as usize * 4,
                false,
            )
            .unwrap(),
            state,
        }
    }
}

impl Renderer<'_> {
    pub fn clear(&mut self, r: f32, g: f32, b: f32) {
        // eprintln!("fill {r} {g} {b}");
        self.image
            .fill_boxes(
                pixman::Operation::Src,
                pixman::Color::from_f32(r, g, b, 1.0),
                &[pixman::Box32 {
                    x1: 0,
                    y1: 0,
                    x2: self.image.width() as i32,
                    y2: self.image.height() as i32,
                }],
            )
            .unwrap();
    }

    pub fn render_buffer(
        &mut self,
        // bytes: &[u8],
        // wl_format: u32,
        // width: u32,
        // height: u32,
        // stride: u32,
        // opaque_region: Option<&pixman::Region32>,
        // alpha: f32,
        // x: i32,
        // y: i32,
        buf: BufferId,
        opaque_region: Option<&pixman::Region32>,
        alpha: f32,
        x: i32,
        y: i32,
    ) {
        let buf = &self.state.shm_buffers[&buf];
        let pool = &self.state.shm_pools[&buf.pool_id];
        let bytes = &pool.memmap[buf.offset..][..buf.stride as usize * buf.height as usize];
        let wl_format = buf.wl_format;

        // eprintln!("render_buffer at {x},{y}");
        let src = unsafe {
            pixman::Image::from_raw_mut(
                wl_format_to_pixman(wl_format).unwrap(),
                buf.width as usize,
                buf.height as usize,
                bytes.as_ptr().cast_mut().cast(),
                buf.stride as usize,
                false,
            )
            .unwrap()
        };

        let buf_rect = pixman::Box32 {
            x1: 0,
            y1: 0,
            x2: buf.width as i32,
            y2: buf.height as i32,
        };
        let op = if opaque_region.is_some_and(|reg| {
            reg.contains_rectangle(buf_rect) == pixman::Overlap::In && alpha == 1.0
        }) {
            pixman::Operation::Src
        } else {
            pixman::Operation::Over
        };

        let mask = (alpha != 1.0).then(|| {
            pixman::Solid::new(pixman::Color::from_f32(alpha, alpha, alpha, alpha)).unwrap()
        });

        self.image.composite32(
            op,
            &src,
            mask.as_deref(),
            (0, 0),
            (0, 0),
            (x, y),
            (buf.width as i32, buf.height as i32),
        );
    }

    pub fn render_rect(&mut self, r: f32, g: f32, b: f32, a: f32, x: i32, y: i32, w: u32, h: u32) {
        let op = if a == 1.0 {
            pixman::Operation::Src
        } else {
            pixman::Operation::Over
        };
        let src = pixman::Solid::new(pixman::Color::from_f32(r, g, b, a)).unwrap();
        self.image
            .composite32(op, &src, None, (0, 0), (0, 0), (x, y), (w as i32, h as i32));
    }
}

fn bytes_to_ints(bytes: &mut [u8]) -> &mut [u32] {
    let ptr = bytes.as_mut_ptr().cast::<u32>();
    assert!(ptr.is_aligned());
    unsafe { std::slice::from_raw_parts_mut(bytes.as_mut_ptr().cast(), bytes.len() / 4) }
}

fn wl_format_to_pixman(format: u32) -> Option<pixman::FormatCode> {
    use pixman::FormatCode as Pix;
    match format {
        0 => Some(Pix::A8R8G8B8),
        0x34324241 => Some(Pix::A8B8G8R8),
        _ => None,
    }
}

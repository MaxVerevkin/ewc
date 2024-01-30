use std::collections::HashMap;
use std::num::NonZeroU64;

use super::*;
use crate::{protocol::*, Proxy};

pub struct RendererStateImp {
    shm_pools: HashMap<WlShmPool, ShmPool>,
    resource_mapping: HashMap<WlBuffer, BufferId>,
    shm_buffers: HashMap<BufferId, ShmBuffer>,
    next_id: NonZeroU64,
}

struct ShmPool {
    memmap: memmap2::Mmap,
    size: usize,
}

struct ShmBuffer {
    spec: ShmBufferSpec,
    locks: u32,
    resource: Option<WlBuffer>,
}

impl RendererStateImp {
    pub fn new() -> Self {
        Self {
            shm_pools: HashMap::new(),
            resource_mapping: HashMap::new(),
            shm_buffers: HashMap::new(),
            next_id: NonZeroU64::MIN,
        }
    }

    pub fn frame<'a>(
        &'a self,
        bytes: &'a mut [u8],
        width: u32,
        height: u32,
        wl_format: u32,
    ) -> Box<dyn Frame + 'a> {
        Box::new(FrameImp {
            time: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u32,
            width,
            height,
            image: pixman::Image::from_slice_mut(
                wl_format_to_pixman(wl_format).unwrap(),
                width as usize,
                height as usize,
                bytes_to_ints(bytes),
                width as usize * 4,
                false,
            )
            .unwrap(),
            state: self,
        })
    }

    fn drop_buffer(&mut self, buffer_id: BufferId) {
        let buffer = self.shm_buffers.remove(&buffer_id).unwrap();
        assert_eq!(buffer.locks, 0);
        assert!(buffer.resource.is_none());
        if !buffer.spec.pool.is_alive()
            && self
                .shm_buffers
                .values()
                .all(|buf| buf.spec.pool != buffer.spec.pool)
        {
            self.shm_pools.remove(&buffer.spec.pool);
        }
    }
}

impl RendererState for RendererStateImp {
    fn create_shm_pool(&mut self, fd: OwnedFd, size: usize, resource: WlShmPool) {
        self.shm_pools.insert(
            resource,
            ShmPool {
                memmap: unsafe { memmap2::MmapOptions::new().len(size).map(&fd).unwrap() },
                size,
            },
        );
    }

    fn resize_shm_pool(&mut self, pool: WlShmPool, new_size: usize) {
        let pool = self.shm_pools.get_mut(&pool).unwrap();
        if new_size > pool.size {
            pool.size = new_size;
            unsafe {
                pool.memmap
                    .remap(new_size, memmap2::RemapOptions::new().may_move(true))
                    .unwrap()
            };
        }
    }

    fn shm_pool_resource_destroyed(&mut self, pool: WlShmPool) {
        if self.shm_buffers.values().all(|buf| buf.spec.pool != pool) {
            self.shm_pools.remove(&pool);
        }
    }

    fn create_shm_buffer(&mut self, spec: ShmBufferSpec, resource: WlBuffer) {
        let id = BufferId(next_id(&mut self.next_id));
        self.resource_mapping.insert(resource.clone(), id);
        self.shm_buffers.insert(
            id,
            ShmBuffer {
                spec,
                resource: Some(resource),
                locks: 0,
            },
        );
    }

    fn buffer_commited(&mut self, resource: WlBuffer) -> BufferId {
        let buffer_id = *self.resource_mapping.get(&resource).unwrap();
        self.buffer_lock(buffer_id);
        buffer_id
    }

    fn get_buffer_size(&self, buffer_id: BufferId) -> (u32, u32) {
        let spec = &self.shm_buffers[&buffer_id].spec;
        (spec.width, spec.height)
    }

    fn buffer_lock(&mut self, buffer_id: BufferId) {
        let buf = self.shm_buffers.get_mut(&buffer_id).unwrap();
        buf.locks += 1;
        // eprintln!("locking {buffer_id:?} (locks = {})", buf.locks);
    }

    fn buffer_unlock(&mut self, buffer_id: BufferId) {
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

    fn buffer_resource_destroyed(&mut self, resource: WlBuffer) {
        let buffer_id = self.resource_mapping.remove(&resource).unwrap();
        let buf = self.shm_buffers.get_mut(&buffer_id).unwrap();
        buf.resource = None;
        if buf.locks == 0 {
            self.drop_buffer(buffer_id);
        }
    }
}

struct FrameImp<'a> {
    time: u32,
    width: u32,
    height: u32,
    image: pixman::Image<'a, 'static>,
    state: &'a RendererStateImp,
}

impl Frame for FrameImp<'_> {
    fn time(&self) -> u32 {
        self.time
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn clear(&mut self, r: f32, g: f32, b: f32) {
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

    fn render_buffer(
        &mut self,
        buf: BufferId,
        opaque_region: Option<&pixman::Region32>,
        alpha: f32,
        x: i32,
        y: i32,
    ) {
        let buf = &self.state.shm_buffers[&buf];
        let pool = &self.state.shm_pools[&buf.spec.pool];
        let spec = &buf.spec;
        let bytes =
            &pool.memmap[spec.offset as usize..][..spec.stride as usize * spec.height as usize];
        let wl_format = spec.wl_format;

        let src = unsafe {
            pixman::Image::from_raw_mut(
                wl_format_to_pixman(wl_format).unwrap(),
                spec.width as usize,
                spec.height as usize,
                bytes.as_ptr().cast_mut().cast(),
                spec.stride as usize,
                false,
            )
            .unwrap()
        };

        let buf_rect = pixman::Box32 {
            x1: 0,
            y1: 0,
            x2: spec.width as i32,
            y2: spec.height as i32,
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
            (spec.width as i32, spec.height as i32),
        );
    }

    fn render_rect(&mut self, r: f32, g: f32, b: f32, a: f32, rect: pixman::Rectangle32) {
        let op = if a == 1.0 {
            pixman::Operation::Src
        } else {
            pixman::Operation::Over
        };
        let src = pixman::Solid::new(pixman::Color::from_f32(r, g, b, a)).unwrap();
        self.image.composite32(
            op,
            &src,
            None,
            (0, 0),
            (0, 0),
            (rect.x, rect.y),
            (rect.width as i32, rect.height as i32),
        );
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
        1 => Some(Pix::X8R8G8B8),
        0x34324241 => Some(Pix::A8B8G8R8),
        _ => None,
    }
}

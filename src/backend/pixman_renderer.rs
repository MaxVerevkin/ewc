use super::*;
use crate::protocol::*;
use crate::Proxy;

pub struct RendererStateImp {
    shm_pools: HashMap<WlShmPool, ShmPool>,
    resource_mapping: HashMap<WlBuffer, BufferId>,
    buffers: HashMap<BufferId, Buffer>,
    next_id: NonZeroU64,
}

struct Buffer {
    locks: u32,
    kind: BufferKind,
}

enum BufferKind {
    Shm(ShmBuffer),
    Argb8Texture(u32, u32, Vec<u8>),
    SinglePix(Color, Option<WlBuffer>),
}

struct ShmBuffer {
    spec: ShmBufferSpec,
    resource: Option<WlBuffer>,
}

impl RendererStateImp {
    pub fn new() -> Self {
        Self {
            shm_pools: HashMap::new(),
            resource_mapping: HashMap::new(),
            buffers: HashMap::new(),
            next_id: NonZeroU64::MIN,
        }
    }

    pub fn frame<'a>(
        &'a self,
        bytes: &'a mut [u8],
        width: u32,
        height: u32,
        wl_format: wl_shm::Format,
    ) -> Box<dyn Frame + 'a> {
        Box::new(FrameImp {
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
        let buffer = self.buffers.remove(&buffer_id).unwrap();
        assert_eq!(buffer.locks, 0);
        match buffer.kind {
            BufferKind::Shm(shm) => {
                assert!(shm.resource.is_none());
                let pool = self.shm_pools.get_mut(&shm.spec.pool).unwrap();
                pool.refcnt -= 1;
                if !shm.spec.pool.is_alive() && pool.refcnt == 0 {
                    self.shm_pools.remove(&shm.spec.pool);
                }
            }
            BufferKind::Argb8Texture(_, _, _) => (),
            BufferKind::SinglePix(_, res) => {
                assert!(res.is_none());
            }
        }
    }
}

impl RendererState for RendererStateImp {
    fn supported_shm_formats(&self) -> &[protocol::wl_shm::Format] {
        &[wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888]
    }

    fn supported_dma_buf_formats(&self) -> Option<&eglgbm::FormatTable> {
        None
    }

    fn get_shm_state(&mut self) -> &mut HashMap<protocol::WlShmPool, ShmPool> {
        &mut self.shm_pools
    }

    fn create_argb8_texture(&mut self, width: u32, height: u32, bytes: &[u8]) -> BufferId {
        let id = BufferId(next_id(&mut self.next_id));
        self.buffers.insert(
            id,
            Buffer {
                locks: 1,
                kind: BufferKind::Argb8Texture(width, height, bytes.to_vec()),
            },
        );
        id
    }

    fn create_shm_buffer(&mut self, spec: ShmBufferSpec, resource: WlBuffer) {
        let id = BufferId(next_id(&mut self.next_id));
        self.resource_mapping.insert(resource.clone(), id);
        let pool = self.shm_pools.get_mut(&spec.pool).unwrap();
        pool.refcnt += 1;
        self.buffers.insert(
            id,
            Buffer {
                locks: 0,
                kind: BufferKind::Shm(ShmBuffer {
                    spec,
                    resource: Some(resource),
                }),
            },
        );
    }

    fn create_dma_buffer(&mut self, _spec: DmaBufSpec, _resource: protocol::WlBuffer) {
        panic!("not supproted");
    }

    fn create_single_pix_buffer(&mut self, color: Color, resource: protocol::WlBuffer) {
        let id = BufferId(next_id(&mut self.next_id));
        self.resource_mapping.insert(resource.clone(), id);
        self.buffers.insert(
            id,
            Buffer {
                locks: 0,
                kind: BufferKind::SinglePix(color, Some(resource)),
            },
        );
    }

    fn buffer_commited(&mut self, resource: WlBuffer) -> BufferId {
        let buffer_id = *self.resource_mapping.get(&resource).unwrap();
        let buf = self.buffers.get_mut(&buffer_id).unwrap();
        buf.locks += 1;
        match &buf.kind {
            BufferKind::Shm(_) => (),
            BufferKind::Argb8Texture(_, _, _) => (),
            BufferKind::SinglePix(_, res) => {
                assert_eq!(res.as_ref().unwrap(), &resource);
                resource.release();
            }
        }
        buffer_id
    }

    fn get_buffer_size(&self, buffer_id: BufferId) -> (u32, u32) {
        let buf = &self.buffers[&buffer_id];
        match &buf.kind {
            BufferKind::Shm(shm) => (shm.spec.width, shm.spec.height),
            BufferKind::Argb8Texture(w, h, _) => (*w, *h),
            BufferKind::SinglePix(_, _) => (1, 1),
        }
    }

    fn buffer_unlock(&mut self, buffer_id: BufferId) {
        let buf = self.buffers.get_mut(&buffer_id).unwrap();
        buf.locks -= 1;
        if buf.locks == 0 {
            match &buf.kind {
                BufferKind::Shm(shm) => {
                    if let Some(resource) = &shm.resource {
                        resource.release();
                    } else {
                        self.drop_buffer(buffer_id);
                    }
                }
                BufferKind::Argb8Texture(_, _, _) => {
                    self.buffers.remove(&buffer_id);
                }
                BufferKind::SinglePix(_, res) => {
                    if res.is_none() {
                        self.drop_buffer(buffer_id);
                    }
                }
            }
        }
    }

    fn buffer_resource_destroyed(&mut self, resource: WlBuffer) {
        let buffer_id = self.resource_mapping.remove(&resource).unwrap();
        let buf = self.buffers.get_mut(&buffer_id).unwrap();
        match &mut buf.kind {
            BufferKind::Shm(shm) => {
                shm.resource = None;
                if buf.locks == 0 {
                    self.drop_buffer(buffer_id);
                }
            }
            BufferKind::Argb8Texture(_, _, _) => unreachable!(),
            BufferKind::SinglePix(_, res) => {
                *res = None;
                if buf.locks == 0 {
                    self.drop_buffer(buffer_id);
                }
            }
        }
    }
}

struct FrameImp<'a> {
    image: pixman::Image<'a, 'static>,
    state: &'a RendererStateImp,
}

impl Frame for FrameImp<'_> {
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
        opaque_region: Option<&pixman::Region32>,
        alpha: f32,
        buf_transform: BufferTransform,
        x: i32,
        y: i32,
    ) {
        let t;
        let t2;

        let buf = &self.state.buffers[&buf_transform.buf_id()];
        let (src, tex_width, tex_height) = match &buf.kind {
            BufferKind::Shm(shm) => {
                let spec = &shm.spec;
                let pool = &self.state.shm_pools[&spec.pool];
                let bytes = &pool.memmap[spec.offset as usize..]
                    [..spec.stride as usize * spec.height as usize];
                t = unsafe {
                    pixman::Image::from_raw_mut(
                        wl_format_to_pixman(spec.wl_format).unwrap(),
                        spec.width as usize,
                        spec.height as usize,
                        bytes.as_ptr().cast_mut().cast(),
                        spec.stride as usize,
                        false,
                    )
                    .unwrap()
                };
                (&*t, spec.width, spec.height)
            }
            BufferKind::Argb8Texture(w, h, bytes) => {
                t = unsafe {
                    pixman::Image::from_raw_mut(
                        wl_format_to_pixman(wl_shm::Format::Argb8888).unwrap(),
                        *w as usize,
                        *h as usize,
                        bytes.as_ptr().cast_mut().cast(),
                        (*w * 4) as usize,
                        false,
                    )
                    .unwrap()
                };
                (&*t, *w, *h)
            }
            BufferKind::SinglePix(color, _) => {
                t2 =
                    pixman::Solid::new(pixman::Color::from_f32(color.r, color.g, color.b, color.a))
                        .unwrap();
                (&*t2, 1, 1)
            }
        };

        let mat = buf_transform.surface_to_buffer().unwrap();
        src.set_transform(pixman::Transform::try_from(mat).unwrap())
            .unwrap();

        let buf_rect = pixman::Box32 {
            x1: 0,
            y1: 0,
            x2: tex_width as i32,
            y2: tex_height as i32,
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
            src,
            mask.as_deref(),
            (0, 0),
            (0, 0),
            (x, y),
            (
                buf_transform.dst_width() as i32,
                buf_transform.dst_height() as i32,
            ),
        );
    }

    fn render_rect(&mut self, color: Color, rect: pixman::Rectangle32) {
        let op = if color.a == 1.0 {
            pixman::Operation::Src
        } else {
            pixman::Operation::Over
        };
        let src = pixman::Solid::new(pixman::Color::from_f32(color.r, color.g, color.b, color.a))
            .unwrap();
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
    let bytes_len = bytes.len();
    let ptr = bytes.as_mut_ptr().cast::<u32>();
    assert!(ptr.is_aligned());
    unsafe { std::slice::from_raw_parts_mut(ptr, bytes_len / 4) }
}

fn wl_format_to_pixman(format: wl_shm::Format) -> Option<pixman::FormatCode> {
    use pixman::FormatCode as Pix;
    use wl_shm::Format as Wl;
    match format {
        Wl::Argb8888 => Some(Pix::A8R8G8B8),
        Wl::Xrgb8888 => Some(Pix::X8R8G8B8),
        _ => None,
    }
}

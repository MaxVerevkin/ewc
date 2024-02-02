use std::io;
use std::num::NonZeroU32;

use crate::backend::{Backend, BufferId};
use crate::protocol::wl_output;
use crate::Fixed;

#[derive(Clone, Copy)]
pub struct BufferTransform {
    buf_id: BufferId,
    buf_width: u32,
    buf_height: u32,
    transform: wl_output::Transform,
    scale: NonZeroU32,
    src_x: f64,
    src_y: f64,
    src_width: f64,
    src_height: f64,
    dst_width: u32,
    dst_height: u32,
}

impl BufferTransform {
    pub fn new(
        buf_id: BufferId,
        backend: &mut dyn Backend,
        transform: wl_output::Transform,
        scale: i32,
        viewport_src: Option<(f64, f64, Fixed, Fixed)>,
        viewport_dst: Option<(u32, u32)>,
    ) -> io::Result<Self> {
        let scale = u32::try_from(scale)
            .ok()
            .and_then(NonZeroU32::new)
            .ok_or_else(|| io::Error::other("invalid buffer scale"))?;

        let (buf_width, buf_height) = backend.renderer_state().get_buffer_size(buf_id);
        if buf_width % scale.get() != 0 || buf_height % scale.get() != 0 {
            return Err(io::Error::other("buffer size not a multiple of scale"));
        }

        let (transformed_w, transformed_h) = {
            let mut w = buf_width / scale.get();
            let mut h = buf_height / scale.get();
            if transform as u32 & 1 != 0 {
                std::mem::swap(&mut w, &mut h);
            }
            (w, h)
        };

        if let Some((x, y, w, h)) = viewport_src {
            if x + w.as_f64() > transformed_w as f64 || y + h.as_f64() > transformed_h as f64 {
                return Err(io::Error::other("viewport src out of buffer"));
            }
        }

        let (dst_width, dst_height) = if let Some((w, h)) = viewport_dst {
            (w, h)
        } else if let Some((_x, _y, w, h)) = viewport_src {
            if !h.is_int() || !w.is_int() {
                return Err(io::Error::other("viewport dst not set, so src must by int"));
            }
            (w.as_int() as u32, h.as_int() as u32)
        } else {
            (transformed_w, transformed_h)
        };

        let (src_x, src_y, src_width, src_height) = match viewport_src {
            None => (0.0, 0.0, dst_width as f64, dst_height as f64),
            Some((x, y, w, h)) => (x, y, w.as_f64(), h.as_f64()),
        };

        Ok(Self {
            buf_id,
            buf_width,
            buf_height,
            transform,
            scale,
            src_x,
            src_y,
            src_width,
            src_height,
            dst_width,
            dst_height,
        })
    }

    pub fn surface_to_buffer(&self) -> Option<pixman::FTransform> {
        let mut mat = pixman::FTransform::identity()
            .scale(
                self.src_width / self.dst_width as f64,
                self.src_height / self.dst_height as f64,
                false,
            )?
            .translate(self.src_x, self.src_y, false)?
            .scale(self.scale.get() as f64, self.scale.get() as f64, false)?;
        if self.transform as u32 & 4 != 0 {
            mat = mat
                .scale(-1.0, 1.0, false)?
                .translate(self.buf_width as f64, 0.0, false)?;
        }
        if self.transform as u32 & 1 != 0 {
            mat = mat
                .rotate(0.0, -1.0, false)?
                .translate(0.0, self.buf_height as f64, false)?;
        }
        if self.transform as u32 & 2 != 0 {
            mat = mat.rotate(-1.0, 0.0, false)?.translate(
                self.buf_width as f64,
                self.buf_height as f64,
                false,
            )?;
        }
        Some(mat)
    }

    pub fn surface_to_uv(&self) -> Option<pixman::FTransform> {
        self.surface_to_buffer().and_then(|m| {
            m.scale(
                1.0 / self.buf_width as f64,
                1.0 / self.buf_height as f64,
                false,
            )
        })
    }

    pub fn buf_id(&self) -> BufferId {
        self.buf_id
    }

    pub fn dst_width(&self) -> u32 {
        self.dst_width
    }

    pub fn dst_height(&self) -> u32 {
        self.dst_height
    }
}

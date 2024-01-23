pub struct Renderer<'a> {
    image: pixman::Image<'a, 'static>,
}

impl<'a> Renderer<'a> {
    pub fn new(bytes: &'a mut [u8], width: u32, height: u32, wl_format: u32) -> Self {
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
        bytes: &[u8],
        wl_format: u32,
        width: u32,
        height: u32,
        stride: u32,
        opaque_region: Option<&pixman::Region32>,
        alpha: f32,
        x: i32,
        y: i32,
    ) {
        // eprintln!("render_buffer at {x},{y}");
        let src = unsafe {
            pixman::Image::from_raw_mut(
                wl_format_to_pixman(wl_format).unwrap(),
                width as usize,
                height as usize,
                bytes.as_ptr().cast_mut().cast(),
                stride as usize,
                false,
            )
            .unwrap()
        };

        let buf_rect = pixman::Box32 {
            x1: 0,
            y1: 0,
            x2: width as i32,
            y2: height as i32,
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
            (width as i32, height as i32),
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

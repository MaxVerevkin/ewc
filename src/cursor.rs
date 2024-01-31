use std::rc::Rc;

use crate::backend::{Backend, BufferId};
use crate::globals::compositor::Surface;

pub struct Cursor {
    kind: Kind,
    default_image: (BufferId, i32, i32),
}

enum Kind {
    Hidden,
    Surface {
        surface: Rc<Surface>,
        hx: i32,
        hy: i32,
    },
    Texture {
        id: BufferId,
        hx: i32,
        hy: i32,
    },
}

impl Cursor {
    pub fn new(backend: &mut dyn Backend) -> Self {
        let theme = xcursor::CursorTheme::load(
            std::env::var("XCURSOR_THEME")
                .as_deref()
                .unwrap_or("default"),
        );

        let path = theme.load_icon("default").expect("no cursor");
        let content = std::fs::read(path).expect("failed to read cursor");
        let mut images = xcursor::parser::parse_xcursor(&content).expect("parse image");
        images.sort_by(|a, b| a.size.cmp(&b.size));
        let (Ok(i) | Err(i)) = images.binary_search_by_key(&24, |x| x.size);
        let image = &images[i];
        let default_image = (
            backend.renderer_state().create_argb8_texture(
                image.width,
                image.height,
                &image.pixels_rgba,
            ),
            image.xhot as i32,
            image.yhot as i32,
        );

        Self {
            kind: Kind::Hidden,
            default_image,
        }
    }

    pub fn set_normal(&mut self) {
        self.kind = Kind::Texture {
            id: self.default_image.0,
            hx: self.default_image.1,
            hy: self.default_image.2,
        }
    }

    pub fn hide(&mut self) {
        self.kind = Kind::Hidden;
    }

    pub fn set_surface(&mut self, surface: Rc<Surface>, hx: i32, hy: i32) {
        self.kind = Kind::Surface { surface, hx, hy }
    }

    pub fn get_buffer(&self) -> Option<(BufferId, i32, i32)> {
        match &self.kind {
            Kind::Hidden => None,
            Kind::Surface { surface, hx, hy } => surface
                .cur
                .borrow()
                .buffer
                .map(|(buf, _, _)| (buf, *hx, *hy)),
            Kind::Texture { id, hx, hy } => Some((*id, *hx, *hy)),
        }
    }
}

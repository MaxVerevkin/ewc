use std::collections::HashMap;
use std::rc::Rc;

use crate::Proxy;
use crate::backend::Backend;
use crate::buffer_transform::BufferTransform;
use crate::client::ClientId;
use crate::globals::compositor::Surface;
use crate::protocol::wl_output;
use crate::protocol::wp_cursor_shape_device_v1::Shape;

pub struct Cursor {
    kind: Kind,
    shapes: HashMap<Shape, Texture>,
}

#[derive(Clone, Copy)]
struct Texture {
    buf_transform: BufferTransform,
    hx: i32,
    hy: i32,
}

enum Kind {
    Hidden,
    Surface {
        surface: Rc<Surface>,
        hx: i32,
        hy: i32,
    },
    Texture(Texture),
}

impl Cursor {
    pub fn new(backend: &mut dyn Backend) -> Self {
        let theme = xcursor::CursorTheme::load(
            std::env::var("XCURSOR_THEME")
                .as_deref()
                .unwrap_or("default"),
        );

        let mut shapes = HashMap::new();

        for &(shape, str) in TO_STR_MAPPING {
            if let Some(tex) = get_texture(&theme, backend, str) {
                shapes.insert(shape, tex);
            } else {
                eprintln!("cursor theme does not contain '{str}");
            }
        }

        Self {
            kind: Kind::Hidden,
            shapes,
        }
    }

    pub fn hide(&mut self) {
        self.kind = Kind::Hidden;
    }

    pub fn set_surface(&mut self, surface: Rc<Surface>, hx: i32, hy: i32) {
        self.kind = Kind::Surface { surface, hx, hy }
    }

    pub fn set_shape(&mut self, shape: Shape) {
        if let Some(tex) = self.shapes.get(&shape) {
            self.kind = Kind::Texture(*tex);
        } else if let Some(default) = self.shapes.get(&Shape::Default) {
            self.kind = Kind::Texture(*default);
        }
    }

    pub fn get_buffer(&self) -> Option<(BufferTransform, i32, i32)> {
        match &self.kind {
            Kind::Hidden => None,
            Kind::Surface { surface, hx, hy } => {
                let buf_transform = surface.buf_transform()?;
                Some((buf_transform, *hx, *hy))
            }
            Kind::Texture(tex) => Some((tex.buf_transform, tex.hx, tex.hy)),
        }
    }

    pub fn remove_client(&mut self, client_id: ClientId) {
        match &self.kind {
            Kind::Surface { surface, .. } if surface.wl.client_id() == client_id => {
                self.hide();
            }
            _ => (),
        }
    }
}

fn get_texture(
    theme: &xcursor::CursorTheme,
    backend: &mut dyn Backend,
    name: &str,
) -> Option<Texture> {
    let path = theme.load_icon(name)?;
    let content = std::fs::read(path).ok()?;
    let mut images = xcursor::parser::parse_xcursor(&content)?;
    images.sort_by(|a, b| a.size.cmp(&b.size));
    let (Ok(i) | Err(i)) = images.binary_search_by_key(&24, |x| x.size);
    let image = images.get(i).or_else(|| images.last())?;
    let buf_id = backend.renderer_state().create_argb8_texture(
        image.width,
        image.height,
        &image.pixels_rgba,
    );
    let buf_transform =
        BufferTransform::new(buf_id, backend, wl_output::Transform::Normal, 1, None, None).unwrap();
    Some(Texture {
        buf_transform,
        hx: image.xhot as i32,
        hy: image.yhot as i32,
    })
}

const TO_STR_MAPPING: &[(Shape, &str)] = &[
    (Shape::Default, "default"),
    (Shape::ContextMenu, "context-menu"),
    (Shape::Help, "help"),
    (Shape::Pointer, "pointer"),
    (Shape::Progress, "progress"),
    (Shape::Wait, "wait"),
    (Shape::Cell, "cell"),
    (Shape::Crosshair, "crosshair"),
    (Shape::Text, "text"),
    (Shape::VerticalText, "vertical-text"),
    (Shape::Alias, "alias"),
    (Shape::Copy, "copy"),
    (Shape::Move, "move"),
    (Shape::NoDrop, "no-drop"),
    (Shape::NotAllowed, "not-allowed"),
    (Shape::Grab, "grab"),
    (Shape::Grabbing, "grabbing"),
    (Shape::EResize, "e-resize"),
    (Shape::NResize, "n-resize"),
    (Shape::NeResize, "ne-resize"),
    (Shape::NwResize, "nw-resize"),
    (Shape::SResize, "s-resize"),
    (Shape::SeResize, "se-resize"),
    (Shape::SwResize, "sw-resize"),
    (Shape::WResize, "w-resize"),
    (Shape::EwResize, "ew-resize"),
    (Shape::NsResize, "ns-resize"),
    (Shape::NeswResize, "nesw-resize"),
    (Shape::NwseResize, "nwse-resize"),
    (Shape::ColResize, "col-resize"),
    (Shape::RowResize, "row-resize"),
    (Shape::AllScroll, "all-scroll"),
    (Shape::ZoomIn, "zoom-in"),
    (Shape::ZoomOut, "zoom-out"),
];

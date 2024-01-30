//! A more generenal purpose version of `wayrs-egl`. May become its own crate someday.
//!
//! Allows to create a GL context, query supported buffer formats/modifiers, allocate buffers, and
//! link them to GL renderbuffers. Powered by GBM and EGL.

#![deny(unsafe_op_in_unsafe_fn)]

use std::collections::HashMap;
use std::fmt;

mod drm;
mod egl;
mod errors;
mod gbm;

pub mod egl_ffi;
pub use drm::DrmDevice;
pub use egl::{EglContext, EglContextBuilder, EglDisplay, EglExtensions, EglImage};
pub use errors::*;
pub use gbm::{Buffer as GbmBuffer, BufferExport, BufferPlane};

#[derive(Debug, Clone, Copy)]
pub enum GraphicsApi {
    OpenGl,
    OpenGlEs,
    OpenVg,
}

/// A DRM fourcc format wrapper with nice `Debug` formatting
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fourcc(pub u32);

/// A mapping from fourcc drm format to a list of modifiers
pub type FormatTable = HashMap<Fourcc, Vec<u64>>;

impl fmt::Debug for Fourcc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d] = self.0.to_le_bytes();
        write!(
            f,
            "{}{}{}{}",
            a.escape_ascii(),
            b.escape_ascii(),
            c.escape_ascii(),
            d.escape_ascii()
        )
    }
}

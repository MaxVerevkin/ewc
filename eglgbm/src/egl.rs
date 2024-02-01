use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::fmt;
use std::os::fd::{AsRawFd, RawFd};

use crate::{egl_ffi, gbm, BufferExport, Error, FormatTable, Fourcc, GraphicsApi, Result};

/// GBM-based EGL display
///
/// Dropping this struct terminates the EGL display.
// TODO: derive Debug when MSRV is >= 1.70
pub struct EglDisplay {
    raw: egl_ffi::EGLDisplay,
    gbm_device: gbm::Device,

    major_version: u32,
    minor_version: u32,

    extensions: EglExtensions,
    supported_formats: FormatTable,

    egl_image_target_renderbuffer_starage_oes: egl_ffi::EglImageTargetRenderbufferStorageOesProc,
    egl_image_target_texture_2d_oes: egl_ffi::EglImageTargetTexture2dOesProc,
}

impl EglDisplay {
    /// Create a new EGL display for a given DRM render node.
    pub fn new(drm_render_node: &CStr) -> Result<Self> {
        let gbm_device = gbm::Device::open(drm_render_node)?;
        Self::with_gbm_device(gbm_device)
    }

    /// Create a new EGL display for a given open DRM FD.
    ///
    /// FD must be kept open for the entire lifetime of this display.
    pub fn with_drm_fd(fd: RawFd) -> Result<Self> {
        let gbm_device = gbm::Device::with_drm_fd(fd)?;
        Self::with_gbm_device(gbm_device)
    }

    fn with_gbm_device(gbm_device: gbm::Device) -> Result<Self> {
        EglExtensions::query(egl_ffi::EGL_NO_DISPLAY)?.require("EGL_KHR_platform_gbm")?;

        let raw = unsafe {
            egl_ffi::eglGetPlatformDisplay(
                egl_ffi::EGL_PLATFORM_GBM_KHR,
                gbm_device.as_raw() as *mut _,
                std::ptr::null(),
            )
        };

        if raw == egl_ffi::EGL_NO_DISPLAY {
            return Err(Error::last_egl());
        }

        let mut major_version = 0;
        let mut minor_version = 0;

        if unsafe { egl_ffi::eglInitialize(raw, &mut major_version, &mut minor_version) }
            != egl_ffi::EGL_TRUE
        {
            return Err(Error::last_egl());
        }

        if major_version <= 1 && minor_version < 5 {
            return Err(Error::OldEgl(major_version as u32, minor_version as u32));
        }

        let extensions = EglExtensions::query(raw)?;
        extensions.require("EGL_EXT_image_dma_buf_import_modifiers")?;
        extensions.require("EGL_KHR_no_config_context")?;
        extensions.require("EGL_KHR_surfaceless_context")?;

        let egl_query_dmabuf_formats_ext = unsafe {
            std::mem::transmute::<_, Option<egl_ffi::EglQueryDmabufFormatsExtProc>>(
                egl_ffi::eglGetProcAddress(b"eglQueryDmaBufFormatsEXT\0".as_ptr() as *const _),
            )
            .ok_or(Error::ExtensionUnsupported(
                "EGL_EXT_image_dma_buf_import_modifiers",
            ))?
        };

        let egl_query_dmabuf_modifiers_ext = unsafe {
            std::mem::transmute::<_, Option<egl_ffi::EglQueryDmabufModifiersExtProc>>(
                egl_ffi::eglGetProcAddress(b"eglQueryDmaBufModifiersEXT\0".as_ptr() as *const _),
            )
            .ok_or(Error::ExtensionUnsupported(
                "EGL_EXT_image_dma_buf_import_modifiers",
            ))?
        };

        // NOTE: eglGetProcAddress may return non-null pointer even if the extension is not supported.
        // Since this is a OpenGL/GLES extention, we cannot check it's presence now.
        let egl_image_target_renderbuffer_starage_oes = unsafe {
            std::mem::transmute::<_, Option<egl_ffi::EglImageTargetRenderbufferStorageOesProc>>(
                egl_ffi::eglGetProcAddress(
                    b"glEGLImageTargetRenderbufferStorageOES\0".as_ptr() as *const _
                ),
            )
            .ok_or(Error::ExtensionUnsupported("GL_OES_EGL_image"))?
        };
        let egl_image_target_texture_2d_oes = unsafe {
            std::mem::transmute::<_, Option<egl_ffi::EglImageTargetRenderbufferStorageOesProc>>(
                egl_ffi::eglGetProcAddress(b"glEGLImageTargetTexture2DOES\0".as_ptr() as *const _),
            )
            .ok_or(Error::ExtensionUnsupported("GL_OES_EGL_image"))?
        };

        let supported_formats = unsafe {
            get_supported_formats(
                raw,
                &gbm_device,
                egl_query_dmabuf_formats_ext,
                egl_query_dmabuf_modifiers_ext,
            )?
        };

        Ok(Self {
            raw,
            gbm_device,

            major_version: major_version as u32,
            minor_version: minor_version as u32,

            extensions,
            supported_formats,

            egl_image_target_renderbuffer_starage_oes,
            egl_image_target_texture_2d_oes,
        })
    }

    pub(crate) fn gbm_device(&self) -> &gbm::Device {
        &self.gbm_device
    }

    /// Major EGL version
    pub fn major_version(&self) -> u32 {
        self.major_version
    }

    /// Minor EGL version
    pub fn minor_version(&self) -> u32 {
        self.minor_version
    }

    /// The set of extensions this EGL display supports
    pub fn extensions(&self) -> &EglExtensions {
        &self.extensions
    }

    /// Get a set of supported buffer formats, in a form of fourcc -> modifiers mapping
    pub fn supported_formats(&self) -> &FormatTable {
        &self.supported_formats
    }

    /// Check whether a fourcc/modifier pair is supported
    pub fn is_format_supported(&self, fourcc: Fourcc, modifier: u64) -> bool {
        match self.supported_formats.get(&fourcc) {
            Some(mods) => mods.contains(&modifier),
            None => false,
        }
    }

    /// Allocate a new buffer
    pub fn alloc_buffer(
        &self,
        width: u32,
        height: u32,
        fourcc: Fourcc,
        modifiers: &[u64],
        scan_out: bool,
    ) -> Result<(EglImage, BufferExport)> {
        let buf_parts = self
            .gbm_device()
            .alloc_buffer(width, height, fourcc, modifiers, scan_out)?
            .export();
        let egl_image = self.import_as_egl_image(&buf_parts)?;
        Ok((egl_image, buf_parts))
    }

    /// Import a buffer as an EglImage
    pub fn import_as_egl_image(&self, buf_parts: &BufferExport) -> Result<EglImage> {
        let mut egl_image_attrs = Vec::with_capacity(7 + 10 * buf_parts.planes.len());
        egl_image_attrs.push(egl_ffi::EGL_WIDTH as _);
        egl_image_attrs.push(buf_parts.width as _);
        egl_image_attrs.push(egl_ffi::EGL_HEIGHT as _);
        egl_image_attrs.push(buf_parts.height as _);
        egl_image_attrs.push(egl_ffi::EGL_LINUX_DRM_FOURCC_EXT as _);
        egl_image_attrs.push(buf_parts.format.0 as _);
        for (i, plane) in buf_parts.planes.iter().enumerate() {
            egl_image_attrs.push(egl_ffi::EGL_DMA_BUF_PLANE_FD_EXT[i] as _);
            egl_image_attrs.push(plane.dmabuf.as_raw_fd() as _);
            egl_image_attrs.push(egl_ffi::EGL_DMA_BUF_PLANE_OFFSET_EXT[i] as _);
            egl_image_attrs.push(plane.offset as _);
            egl_image_attrs.push(egl_ffi::EGL_DMA_BUF_PLANE_PITCH_EXT[i] as _);
            egl_image_attrs.push(plane.stride as _);
            egl_image_attrs.push(egl_ffi::EGL_DMA_BUF_PLANE_MODIFIER_LO_EXT[i] as _);
            egl_image_attrs.push((buf_parts.modifier & 0xFFFF_FFFF) as _);
            egl_image_attrs.push(egl_ffi::EGL_DMA_BUF_PLANE_MODIFIER_HI_EXT[i] as _);
            egl_image_attrs.push((buf_parts.modifier >> 32) as _);
        }
        egl_image_attrs.push(egl_ffi::EGL_NONE as _);

        let egl_image = unsafe {
            egl_ffi::eglCreateImage(
                self.raw,
                egl_ffi::EGL_NO_CONTEXT,
                egl_ffi::EGL_LINUX_DMA_BUF_EXT,
                egl_ffi::EGLClientBuffer(std::ptr::null_mut()),
                egl_image_attrs.as_ptr(),
            )
        };
        if egl_image == egl_ffi::EGL_NO_IMAGE {
            return Err(Error::last_egl());
        }

        Ok(EglImage {
            egl_display: self.raw,
            egl_image,
            egl_image_target_renderbuffer_starage_oes: self
                .egl_image_target_renderbuffer_starage_oes,
            egl_image_target_texture_2d_oes: self.egl_image_target_texture_2d_oes,
        })
    }
}

impl Drop for EglDisplay {
    fn drop(&mut self) {
        // SAFETY: terminating EGL display does not invalidate the display pointer, so objects
        // created from this display may outlive this struct and still reference this EGLDisplay.
        //
        // NOTE: `glutin` crate does not terminate EGL displays on drop because
        // eglGetPlatformDisplay returns the same pointer each time it is called with the same
        // arguments. This is a problem because two EglDisplay objects may be created referencing
        // the same EGLDisplay pointer, so dropping one display terminates another. However, this
        // is not a problem in our particular case because each time EglDisplay::new is called, a
        // new GBM device pointer is created. Even if two GMB devices represent the same resource,
        // the pointers are different, so eglGetPlatformDisplay must return a new EGLDisplay. GBM
        // device pointer probably may be reused after the device is freed, but this is again not
        // a problem because GBM device is kept alive for the lifetime of EglDisplay.
        unsafe { egl_ffi::eglTerminate(self.raw) };
    }
}

unsafe fn get_supported_formats(
    dpy: egl_ffi::EGLDisplay,
    gbm_device: &gbm::Device,
    qf: egl_ffi::EglQueryDmabufFormatsExtProc,
    qm: egl_ffi::EglQueryDmabufModifiersExtProc,
) -> Result<FormatTable> {
    let mut retval = HashMap::new();

    let mut formats_len = 0;
    if unsafe { qf(dpy, 0, std::ptr::null_mut(), &mut formats_len) } != egl_ffi::EGL_TRUE {
        return Err(Error::last_egl());
    }

    let mut formats_buf = Vec::with_capacity(formats_len as usize);
    if unsafe { qf(dpy, formats_len, formats_buf.as_mut_ptr(), &mut formats_len) }
        != egl_ffi::EGL_TRUE
    {
        return Err(Error::last_egl());
    }
    unsafe { formats_buf.set_len(formats_len as usize) };

    for &format in formats_buf
        .iter()
        .filter(|&&fmt| gbm_device.is_format_supported(Fourcc(fmt as u32)))
    {
        let mut mods_len = 0;
        if unsafe {
            qm(
                dpy,
                format,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut mods_len,
            )
        } != egl_ffi::EGL_TRUE
        {
            return Err(Error::last_egl());
        }

        let mut mods_buf = Vec::with_capacity(mods_len as usize);
        if unsafe {
            qm(
                dpy,
                format,
                mods_len,
                mods_buf.as_mut_ptr(),
                std::ptr::null_mut(),
                &mut mods_len,
            )
        } != egl_ffi::EGL_TRUE
        {
            return Err(Error::last_egl());
        }
        unsafe { mods_buf.set_len(mods_len as usize) };

        retval.insert(Fourcc(format as u32), mods_buf);
    }

    Ok(retval)
}

/// [`EglContext`] builder
pub struct EglContextBuilder {
    api: GraphicsApi,
    major_v: u32,
    minor_v: u32,
    debug: bool,
}

impl EglContextBuilder {
    /// Create a new [`EglContext`] builder
    pub fn new(api: GraphicsApi) -> Self {
        Self {
            api,
            major_v: 1,
            minor_v: 0,
            debug: false,
        }
    }

    /// Set the required API version. Default is `1.0`.
    pub fn version(mut self, major: u32, minor: u32) -> Self {
        self.major_v = major;
        self.minor_v = minor;
        self
    }

    /// Enable/disable debugging. Default is `false`.
    pub fn debug(mut self, enable: bool) -> Self {
        self.debug = enable;
        self
    }

    /// Create a new graphics API context
    ///
    /// Call [`EglContext::make_current`] to activate the context.
    pub fn build(self, display: &EglDisplay) -> Result<EglContext> {
        let api = match self.api {
            GraphicsApi::OpenGl => egl_ffi::EGL_OPENGL_API,
            GraphicsApi::OpenGlEs => egl_ffi::EGL_OPENGL_ES_API,
            GraphicsApi::OpenVg => egl_ffi::EGL_OPENVG_API,
        };

        if unsafe { egl_ffi::eglBindAPI(api) } != egl_ffi::EGL_TRUE {
            return Err(Error::last_egl());
        }

        let context_attrs = [
            egl_ffi::EGL_CONTEXT_MAJOR_VERSION,
            self.major_v as _,
            egl_ffi::EGL_CONTEXT_MINOR_VERSION,
            self.minor_v as _,
            egl_ffi::EGL_CONTEXT_OPENGL_DEBUG,
            self.debug as _,
            egl_ffi::EGL_NONE,
        ];

        let raw = unsafe {
            egl_ffi::eglCreateContext(
                display.raw,
                egl_ffi::EGL_NO_CONFIG,
                egl_ffi::EGL_NO_CONTEXT,
                context_attrs.as_ptr(),
            )
        };

        if raw == egl_ffi::EGL_NO_CONTEXT {
            return Err(Error::last_egl());
        }

        Ok(EglContext {
            raw,
            api,
            egl_display: display.raw,
        })
    }
}

/// EGL graphics API context
///
/// Call [`make_current`](Self::make_current) to activate the context. Dropping this struct will destroy the context if
/// it is not current on any thread. Otherwise it will be destroyed when it stops being current.
#[derive(Debug)]
pub struct EglContext {
    raw: egl_ffi::EGLContext,
    api: egl_ffi::EGLenum,
    egl_display: egl_ffi::EGLDisplay,
}

impl EglContext {
    /// Make this context current on the current therad.
    ///
    /// The context is [surfaceless][1].
    ///
    /// [1]: https://registry.khronos.org/EGL/extensions/KHR/EGL_KHR_surfaceless_context.txt
    pub fn make_current(&self) -> Result<()> {
        if unsafe {
            egl_ffi::eglMakeCurrent(
                self.egl_display,
                egl_ffi::EGL_NO_SURFACE,
                egl_ffi::EGL_NO_SURFACE,
                self.raw,
            )
        } != egl_ffi::EGL_TRUE
        {
            Err(Error::last_egl())
        } else {
            Ok(())
        }
    }

    /// Releases the current API context.
    ///
    /// If this context is not current on this thread, `Err(Error::NotCurrentContext)` is returned.
    pub fn release(&self) -> Result<()> {
        if unsafe { egl_ffi::eglGetCurrentContext() } != self.raw {
            return Err(Error::NotCurrentContext);
        }

        if unsafe { egl_ffi::eglBindAPI(self.api) } != egl_ffi::EGL_TRUE {
            return Err(Error::last_egl());
        }

        if unsafe {
            egl_ffi::eglMakeCurrent(
                self.egl_display,
                egl_ffi::EGL_NO_SURFACE,
                egl_ffi::EGL_NO_SURFACE,
                egl_ffi::EGL_NO_CONTEXT,
            )
        } != egl_ffi::EGL_TRUE
        {
            return Err(Error::last_egl());
        }

        Ok(())
    }
}

impl Drop for EglContext {
    fn drop(&mut self) {
        unsafe { egl_ffi::eglDestroyContext(self.egl_display, self.raw) };
    }
}

/// A set of EGL extensions
pub struct EglExtensions(HashSet<&'static [u8]>);

impl EglExtensions {
    pub(crate) fn query(display: egl_ffi::EGLDisplay) -> Result<Self> {
        let ptr = unsafe { egl_ffi::eglQueryString(display, egl_ffi::EGL_EXTENSIONS) };

        if ptr.is_null() {
            return Err(Error::last_egl());
        }

        let bytes = unsafe { CStr::from_ptr::<'static>(ptr) }.to_bytes();
        Ok(Self(bytes.split(|b| *b == b' ').collect()))
    }

    /// Check whether a given extension is supported
    pub fn contains(&self, ext: &str) -> bool {
        self.0.contains(ext.as_bytes())
    }

    /// Returns `Err(Error::ExtensionUnsupported(_))` if a given extension is not supported
    pub fn require(&self, ext: &'static str) -> Result<()> {
        if self.contains(ext) {
            Ok(())
        } else {
            Err(Error::ExtensionUnsupported(ext))
        }
    }
}

impl fmt::Debug for EglExtensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_set();
        for ext in &self.0 {
            let ext = String::from_utf8_lossy(ext);
            debug.entry(&ext.as_ref());
        }
        debug.finish()
    }
}

/// A link between GBM and OpneGL.
#[derive(Debug)]
pub struct EglImage {
    egl_display: egl_ffi::EGLDisplay,
    egl_image: egl_ffi::EGLImage,
    egl_image_target_renderbuffer_starage_oes: egl_ffi::EglImageTargetRenderbufferStorageOesProc,
    egl_image_target_texture_2d_oes: egl_ffi::EglImageTargetTexture2dOesProc,
}

impl EglImage {
    /// Associate this buffer with a currently bound GL's renderbuffer object.
    ///
    /// This allows to render directly to this buffer.
    ///
    /// # Safety
    ///
    /// This function must be called from an OpenGL(-ES) context with support for [`GL_OES_EGL_image`][1]
    /// extension and a bound `GL_RENDERBUFFER`. Note that [`EglDisplay`](crate::EglDisplay) does not
    /// guarantee the presence of this extention.
    ///
    /// Rendering to a buffer that is currently in use by the compositor may cause visual glitches
    /// and may be considered UB.
    ///
    /// [1]: https://registry.khronos.org/OpenGL/extensions/OES/OES_EGL_image.txt
    pub unsafe fn set_as_gl_renderbuffer_storage(&self) {
        const GL_RENDERBUFFER: egl_ffi::EGLenum = 0x8D41;
        unsafe {
            (self.egl_image_target_renderbuffer_starage_oes)(GL_RENDERBUFFER, self.egl_image);
        }
    }

    /// Associate this buffer with a currently bound GL_TEXTURE_2D texture.
    ///
    /// This allows sample from this buffer.
    ///
    /// # Safety
    ///
    /// Analagous to [`set_as_gl_renderbuffer_storage`](Self::set_as_gl_renderbuffer_storage).
    pub unsafe fn set_as_gl_texture_2d(&self) {
        const GL_TEXTURE_2D: egl_ffi::EGLenum = 0x0DE1;
        unsafe {
            (self.egl_image_target_texture_2d_oes)(GL_TEXTURE_2D, self.egl_image);
        }
    }
}

impl Drop for EglImage {
    fn drop(&mut self) {
        // SAFETY: EGLImage will not be used to create any new targets. Destroying an image does not
        // affect its "siblings", in our case the renderbuffer object. We ignore the result, since
        // there is not much we can do in case of an error.
        unsafe { egl_ffi::eglDestroyImage(self.egl_display, self.egl_image) };
    }
}

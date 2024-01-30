use std::collections::HashMap;
use std::ffi::CStr;

use eglgbm::{egl_ffi::eglGetProcAddress, BufferExport, FormatTable, Fourcc};
use wayrs_protocols::linux_dmabuf_unstable_v1::zwp_linux_dmabuf_feedback_v1::TrancheFlags;
use wayrs_utils::dmabuf_feedback::DmabufFeedback;

use super::*;
use crate::protocol::*;
use crate::wl_shm;
use crate::Proxy;

const DRM_FORMAT_XRGB8888: Fourcc = Fourcc(u32::from_le_bytes(*b"XR24"));

pub struct RendererStateImp {
    shm_pools: HashMap<WlShmPool, ShmPool>,
    shm_buffers: HashMap<WlBuffer, ShmBufferSpec>,
    textures: HashMap<BufferId, Texture>,
    next_id: NonZeroU64,

    fourcc: Fourcc,
    mods: Vec<u64>,

    verts_buffer: u32,
    verts: Vec<Vert>,

    bound_textures: u32,
    texture_units: u32,

    gl: Box<gl46::GlFns>,
    _context: eglgbm::EglContext,
    egl: eglgbm::EglDisplay,
}

struct ShmPool {
    memmap: memmap2::Mmap,
    size: usize,
    refcnt: usize,
}

struct Texture {
    gl_name: u32,
    width: u32,
    height: u32,
    locks: u32,
}

impl RendererStateImp {
    pub fn new(render_node: &CStr, feedback: DmabufFeedback) -> Option<Self> {
        let egl = eglgbm::EglDisplay::new(render_node).unwrap();
        Self::with_egl(egl, Some(feedback), None)
    }

    pub fn with_drm_fd(fd: RawFd, supported_plane_formats: &FormatTable) -> Option<Self> {
        let egl = eglgbm::EglDisplay::with_drm_fd(fd).unwrap();
        Self::with_egl(egl, None, Some(supported_plane_formats))
    }

    fn with_egl(
        egl: eglgbm::EglDisplay,
        feedback: Option<DmabufFeedback>,
        format_table: Option<&FormatTable>,
    ) -> Option<Self> {
        eprintln!("EGL v{}.{}", egl.major_version(), egl.minor_version());

        let egl_context = eglgbm::EglContextBuilder::new(eglgbm::GraphicsApi::OpenGl)
            .version(4, 6)
            .debug(true)
            .build(&egl)
            .unwrap();
        egl_context.make_current().unwrap();

        let gl = unsafe {
            let gl = gl46::GlFns::load_from(&|name| eglGetProcAddress(name.cast())).unwrap();
            setup_gl_debug_cb(&gl);
            let mut gl_maj = 0;
            let mut gl_min = 0;
            gl.GetInteger64v(gl46::GL_MAJOR_VERSION, &mut gl_maj);
            gl.GetInteger64v(gl46::GL_MINOR_VERSION, &mut gl_min);
            eprintln!("OpenGL v{gl_maj}.{gl_min}");
            gl
        };

        let mut verts_buffer = 0;
        let mut vertex_array = 0;
        let shader;

        let texture_units = {
            let mut n = 0;
            unsafe { gl.GetIntegerv(gl46::GL_MAX_TEXTURE_IMAGE_UNITS, &mut n) };
            assert!(n >= 16);
            n as u32
        };

        eprintln!("gl46_renderer: {texture_units} texture units available");

        unsafe {
            gl.Enable(gl46::GL_BLEND);
            gl.BlendFunc(gl46::GL_ONE, gl46::GL_ONE_MINUS_SRC_ALPHA);

            gl.GenVertexArrays(1, &mut vertex_array);
            gl.CreateBuffers(1, &mut verts_buffer);

            gl.BindVertexArray(vertex_array);
            gl.BindVertexBuffer(0, verts_buffer, 0, std::mem::size_of::<Vert>() as i32);
            gl.EnableVertexAttribArray(0);
            gl.EnableVertexAttribArray(1);
            gl.VertexAttribBinding(0, 0);
            gl.VertexAttribBinding(1, 0);
            gl.VertexAttribFormat(0, 2, gl46::GL_FLOAT, 0, 0);
            gl.VertexAttribFormat(1, 4, gl46::GL_FLOAT, 0, 8);

            shader = create_shader(&gl, texture_units);
            gl.UseProgram(shader);

            let units: Vec<_> = (0..texture_units as i32).collect();
            gl.Uniform1iv(1, units.len() as i32, units.as_ptr());
        }

        let (fourcc, mods) = match feedback {
            Some(feedback) => select_format_from_feedback(&egl, feedback),
            None => select_format_from_format_table(&egl, format_table.unwrap()),
        };

        Some(Self {
            shm_pools: HashMap::new(),
            shm_buffers: HashMap::new(),
            textures: HashMap::new(),
            next_id: NonZeroU64::MIN,

            fourcc,
            mods,
            verts_buffer,
            verts: Vec::new(),

            texture_units,
            bound_textures: 0,

            gl: Box::new(gl),
            _context: egl_context,
            egl,
        })
    }

    pub fn allocate_framebuffer(
        &mut self,
        width: u32,
        height: u32,
        scan_out: bool,
    ) -> (Framebuffer, BufferExport) {
        let (egl_image, export) = self
            .egl
            .alloc_buffer(width, height, self.fourcc, &self.mods, scan_out)
            .unwrap();
        let fb = unsafe { Framebuffer::new(egl_image, &self.gl) };
        (fb, export)
    }

    pub fn gl(&self) -> &gl46::GlFns {
        &self.gl
    }

    pub fn frame<'a>(
        &'a mut self,
        width: u32,
        height: u32,
        fb: &Framebuffer,
    ) -> Box<dyn Frame + 'a> {
        unsafe {
            self.gl.BindFramebuffer(gl46::GL_FRAMEBUFFER, fb.fbo);
            self.gl.Viewport(0, 0, width as i32, height as i32);
            self.gl.Uniform2f(0, width as f32, height as f32);
        }

        Box::new(FrameImp {
            time: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u32,
            width,
            height,
            state: self,
        })
    }

    pub fn finish_frame(&mut self) {
        self.flush_quads();
        unsafe { self.gl.Finish() };
    }

    fn flush_quads(&mut self) {
        if !self.verts.is_empty() {
            unsafe {
                self.gl.NamedBufferData(
                    self.verts_buffer,
                    std::mem::size_of_val(self.verts.as_slice()) as isize,
                    self.verts.as_ptr().cast(),
                    gl46::GL_STREAM_DRAW,
                );
                self.gl
                    .DrawArrays(gl46::GL_TRIANGLES, 0, self.verts.len() as i32);
            }
            self.verts.clear();
            self.bound_textures = 0;
        }
    }

    fn drop_buffer(&mut self, buffer_id: BufferId) {
        let buffer = self.textures.remove(&buffer_id).unwrap();
        assert_eq!(buffer.locks, 0);
        unsafe { self.gl.DeleteTextures(1, &buffer.gl_name) };
    }
}

impl RendererState for RendererStateImp {
    fn create_shm_pool(&mut self, fd: OwnedFd, size: usize, resource: WlShmPool) {
        self.shm_pools.insert(
            resource,
            ShmPool {
                memmap: unsafe { memmap2::MmapOptions::new().len(size).map(&fd).unwrap() },
                size,
                refcnt: 0,
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
        let shm_pool = self.shm_pools.get_mut(&pool).unwrap();
        if shm_pool.refcnt == 0 {
            self.shm_pools.remove(&pool);
        }
    }

    fn create_shm_buffer(&mut self, spec: ShmBufferSpec, resource: WlBuffer) {
        self.shm_pools.get_mut(&spec.pool).unwrap().refcnt += 1;
        self.shm_buffers.insert(resource, spec);
    }

    fn buffer_commited(&mut self, buffer_resource: WlBuffer) -> BufferId {
        let spec = self.shm_buffers.get(&buffer_resource).unwrap();

        buffer_resource.release();
        let pool = &self.shm_pools[&spec.pool];
        let bytes =
            &pool.memmap[spec.offset as usize..][..spec.stride as usize * spec.height as usize];

        let mut tex = 0;
        unsafe {
            self.gl.CreateTextures(gl46::GL_TEXTURE_2D, 1, &mut tex);
            self.gl
                .TextureParameteri(tex, gl46::GL_TEXTURE_MIN_FILTER, gl46::GL_NEAREST.0 as i32);
            self.gl
                .TextureParameteri(tex, gl46::GL_TEXTURE_MAG_FILTER, gl46::GL_NEAREST.0 as i32);
            self.gl.TextureParameteri(
                tex,
                gl46::GL_TEXTURE_WRAP_S,
                gl46::GL_CLAMP_TO_EDGE.0 as i32,
            );
            self.gl.TextureParameteri(
                tex,
                gl46::GL_TEXTURE_WRAP_T,
                gl46::GL_CLAMP_TO_EDGE.0 as i32,
            );
            self.gl.TextureStorage2D(
                tex,
                1,
                if spec.wl_format == wl_shm::Format::Argb8888 as u32 {
                    gl46::GL_RGBA8
                } else if spec.wl_format == wl_shm::Format::Xrgb8888 as u32 {
                    gl46::GL_RGB8
                } else {
                    panic!("unsupported wl format")
                },
                spec.width as i32,
                spec.height as i32,
            );
            self.gl.TextureSubImage2D(
                tex,
                0,
                0,
                0,
                spec.width as i32,
                spec.height as i32,
                gl46::GL_BGRA,
                gl46::GL_UNSIGNED_BYTE,
                bytes.as_ptr().cast(),
            );
        }

        let new_id = BufferId(next_id(&mut self.next_id));
        self.textures.insert(
            new_id,
            Texture {
                gl_name: tex,
                width: spec.width,
                height: spec.height,
                locks: 1,
            },
        );

        new_id
    }

    fn get_buffer_size(&self, buffer_id: BufferId) -> (u32, u32) {
        let buf = &self.textures[&buffer_id];
        (buf.width, buf.height)
    }

    fn buffer_lock(&mut self, buffer_id: BufferId) {
        let buf = self.textures.get_mut(&buffer_id).unwrap();
        buf.locks += 1;
    }

    fn buffer_unlock(&mut self, buffer_id: BufferId) {
        let buf = self.textures.get_mut(&buffer_id).unwrap();
        buf.locks -= 1;
        if buf.locks == 0 {
            self.drop_buffer(buffer_id);
        }
    }

    fn buffer_resource_destroyed(&mut self, resource: WlBuffer) {
        let shm_spec = self.shm_buffers.remove(&resource).unwrap();
        let shm_pool = self.shm_pools.get_mut(&shm_spec.pool).unwrap();
        shm_pool.refcnt -= 1;
        if !shm_spec.pool.is_alive() && shm_pool.refcnt == 0 {
            self.shm_pools.remove(&shm_spec.pool);
        }
    }
}

pub struct FrameImp<'a> {
    time: u32,
    width: u32,
    height: u32,
    state: &'a mut RendererStateImp,
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
        unsafe {
            self.state.gl.ClearColor(r, g, b, 1.0);
            self.state.gl.Clear(gl46::GL_COLOR_BUFFER_BIT);
        }
    }

    fn render_buffer(
        &mut self,
        buf: BufferId,
        _opaque_region: Option<&pixman::Region32>,
        alpha: f32,
        x: i32,
        y: i32,
    ) {
        if self.state.bound_textures == self.state.texture_units {
            self.state.flush_quads();
        }

        let tex = &self.state.textures[&buf];
        unsafe {
            self.state
                .gl
                .BindTextureUnit(self.state.bound_textures, tex.gl_name);
        }
        let mut vert = Vert {
            x: x as f32,
            y: y as f32,
            r: 0.0,
            g: 0.0,
            b: self.state.bound_textures as f32,
            a: -1.0 - alpha,
        };
        self.state.bound_textures += 1;
        self.state.verts.push(vert);
        vert.x = (x + tex.width as i32) as f32;
        vert.r = 1.0;
        self.state.verts.push(vert);
        vert.y = (y + tex.height as i32) as f32;
        vert.g = 1.0;
        self.state.verts.push(vert);
        self.state.verts.push(vert);
        vert.x = x as f32;
        vert.r = 0.0;
        self.state.verts.push(vert);
        vert.y = y as f32;
        vert.g = 0.0;
        self.state.verts.push(vert);
    }

    fn render_rect(&mut self, r: f32, g: f32, b: f32, a: f32, rect: pixman::Rectangle32) {
        let mut vert = Vert {
            x: rect.x as f32,
            y: rect.y as f32,
            r,
            g,
            b,
            a,
        };
        self.state.verts.push(vert);
        vert.x = (rect.x + rect.width as i32) as f32;
        self.state.verts.push(vert);
        vert.y = (rect.y + rect.height as i32) as f32;
        self.state.verts.push(vert);
        self.state.verts.push(vert);
        vert.x = rect.x as f32;
        self.state.verts.push(vert);
        vert.y = rect.y as f32;
        self.state.verts.push(vert);
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct Vert {
    x: f32,
    y: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

pub struct Framebuffer {
    fbo: u32,
    rbo: u32,
}

impl Framebuffer {
    pub fn destroy(&self, gl: &gl46::GlFns) {
        unsafe { gl.DeleteFramebuffers(1, &self.fbo) };
        unsafe { gl.DeleteRenderbuffers(1, &self.rbo) };
    }

    unsafe fn new(egl_image: eglgbm::EglImage, gl: &gl46::GlFns) -> Self {
        let mut fbo = 0;
        let mut rbo = 0;

        gl.GenFramebuffers(1, &mut fbo);
        gl.GenRenderbuffers(1, &mut rbo);

        gl.BindFramebuffer(gl46::GL_FRAMEBUFFER, fbo);
        gl.BindRenderbuffer(gl46::GL_RENDERBUFFER, rbo);
        gl.DrawBuffers(1, &gl46::GL_COLOR_ATTACHMENT0);

        egl_image.set_as_gl_renderbuffer_storage();

        gl.FramebufferRenderbuffer(
            gl46::GL_FRAMEBUFFER,
            gl46::GL_COLOR_ATTACHMENT0,
            gl46::GL_RENDERBUFFER,
            rbo,
        );

        assert_eq!(
            gl.CheckNamedFramebufferStatus(fbo, gl46::GL_FRAMEBUFFER),
            gl46::GL_FRAMEBUFFER_COMPLETE
        );

        Self { fbo, rbo }
    }
}

fn select_format_from_feedback(
    egl: &eglgbm::EglDisplay,
    feedback: DmabufFeedback,
) -> (Fourcc, Vec<u64>) {
    let format_table = feedback.format_table();
    let mut formats = HashMap::<Fourcc, Vec<u64>>::new();

    for tranche in feedback.tranches() {
        if tranche.flags.contains(TrancheFlags::Scanout) {
            continue;
        }
        for &index in tranche.formats.as_ref().expect("tranche.formats") {
            let fmt = format_table[index as usize];
            if egl.is_format_supported(Fourcc(fmt.fourcc), fmt.modifier) {
                formats
                    .entry(Fourcc(fmt.fourcc))
                    .or_default()
                    .push(fmt.modifier);
            }
        }
    }

    (
        DRM_FORMAT_XRGB8888,
        formats
            .remove(&DRM_FORMAT_XRGB8888)
            .expect("xrgb8888 not supported"),
    )
}

fn select_format_from_format_table(
    egl: &eglgbm::EglDisplay,
    format_table: &FormatTable,
) -> (Fourcc, Vec<u64>) {
    let mut formats = HashMap::<Fourcc, Vec<u64>>::new();

    for (&format, modifiers) in format_table {
        for &modifier in modifiers {
            if egl.is_format_supported(format, modifier) {
                formats.entry(format).or_default().push(modifier);
            }
        }
    }

    (
        DRM_FORMAT_XRGB8888,
        formats
            .remove(&DRM_FORMAT_XRGB8888)
            .expect("xrgb8888 not supported"),
    )
}

unsafe fn create_shader(gl: &gl46::GlFns, texture_units: u32) -> u32 {
    let vertex_shader = b"
        #version 460 core
        layout(location = 0) in vec2 a_Pos;
        layout(location = 1) in vec4 a_Color;
        out vec4 v_Color;
        layout(location = 0) uniform vec2 u_ScreenSize;
        void main() {
            gl_Position = vec4(a_Pos * 2.0 / u_ScreenSize - vec2(1.0), 0.0, 1.0);
            v_Color = a_Color;
        }\0";

    let fragment_shader = format!(
        "#version 460 core
        in vec4 v_Color;
        out vec4 frag_color;
        layout(location = 1) uniform sampler2D u_Textures[{texture_units}];
        void main() {{
            if (v_Color.a < 0.0) {{
                int tex_i = int(v_Color.b);
                frag_color = texture(u_Textures[tex_i], v_Color.rg) * (-1.0 - v_Color.a);
            }} else {{
                frag_color = v_Color;
            }}
        }}\0"
    );

    let vs = gl.CreateShader(gl46::GL_VERTEX_SHADER);
    gl.ShaderSource(vs, 1, &(vertex_shader.as_ptr() as _), std::ptr::null());
    gl.CompileShader(vs);
    assert_shader_ok(gl, vs);

    let fs = gl.CreateShader(gl46::GL_FRAGMENT_SHADER);
    gl.ShaderSource(fs, 1, &(fragment_shader.as_ptr() as _), std::ptr::null());
    gl.CompileShader(fs);
    assert_shader_ok(gl, fs);

    let program = gl.CreateProgram();
    gl.AttachShader(program, fs);
    gl.AttachShader(program, vs);
    gl.LinkProgram(program);
    assert_shader_program_ok(gl, program);

    gl.DeleteShader(fs);
    gl.DeleteShader(vs);

    program
}

unsafe fn assert_shader_ok(gl: &gl46::GlFns, shader: u32) {
    let mut success = 0;
    gl.GetShaderiv(shader, gl46::GL_COMPILE_STATUS, &mut success);

    if success != 1 {
        let mut log = [0u8; 1024];
        let mut len = 0;
        gl.GetShaderInfoLog(shader, log.len() as _, &mut len, log.as_mut_ptr() as *mut _);
        let msg = std::str::from_utf8(&log[..len as usize]).unwrap();
        panic!("Shader error:\n{msg}");
    }
}

unsafe fn assert_shader_program_ok(gl: &gl46::GlFns, shader_program: u32) {
    let mut success = 0;
    gl.GetProgramiv(shader_program, gl46::GL_LINK_STATUS, &mut success);

    if success != 1 {
        let mut log = [0u8; 1024];
        let mut len = 0;
        gl.GetProgramInfoLog(
            shader_program,
            log.len() as _,
            &mut len,
            log.as_mut_ptr() as *mut _,
        );
        let msg = std::str::from_utf8(&log[..len as usize]).unwrap();
        panic!("Shader program error:\n{msg}");
    }
}

pub unsafe fn setup_gl_debug_cb(gl: &gl46::GlFns) {
    use std::ffi::c_void;

    unsafe extern "system" fn gl_debug_cb(
        _source: gl46::GLenum,
        _type: gl46::GLenum,
        _id: u32,
        severity: gl46::GLenum,
        length: i32,
        message: *const u8,
        _: *const c_void,
    ) {
        let msg = unsafe { std::slice::from_raw_parts(message, length as usize) };
        let msg = String::from_utf8_lossy(msg);

        let severity_str = match severity {
            gl46::GL_DEBUG_SEVERITY_HIGH => "high",
            gl46::GL_DEBUG_SEVERITY_LOW => "low",
            gl46::GL_DEBUG_SEVERITY_MEDIUM => "medium",
            gl46::GL_DEBUG_SEVERITY_NOTIFICATION => "notification",
            _ => "unknown",
        };

        eprintln!("[OpenGL] ({severity_str}): {msg}",);
    }

    unsafe { gl.DebugMessageCallback(Some(gl_debug_cb), std::ptr::null()) };
}

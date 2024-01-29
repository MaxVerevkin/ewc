use std::collections::HashMap;
use std::ffi::CStr;

use eglgbm::{egl_ffi::eglGetProcAddress, BufferExport, Fourcc};
use wayrs_protocols::linux_dmabuf_unstable_v1::zwp_linux_dmabuf_feedback_v1::TrancheFlags;
use wayrs_utils::dmabuf_feedback::DmabufFeedback;

use super::*;
use crate::wl_shm;

const DRM_FORMAT_XRGB8888: Fourcc = Fourcc(u32::from_le_bytes(*b"XR24"));

pub struct RendererStateImp {
    shm_pools: HashMap<ShmPoolId, ShmPool>,
    shm_buffers: HashMap<crate::protocol::WlBuffer, ShmBufferSpec>,
    textures: HashMap<BufferId, Texture>,
    next_id: NonZeroU64,

    fourcc: Fourcc,
    mods: Vec<u64>,

    framebuffer: u32,
    renderbuffer: u32,
    verts_buffer: u32,
    verts: Vec<Vert>,

    bound_textures: u32,

    gl: Box<gl46::GlFns>,
    _context: eglgbm::EglContext,
    egl: eglgbm::EglDisplay,
}

struct ShmPool {
    memmap: memmap2::Mmap,
    size: usize,
    resource_alive: bool,
    refcnt: usize,
}

struct Texture {
    gl_name: u32,
    width: u32,
    height: u32,
    locks: u32,
}

impl RendererStateImp {
    pub fn new(render_node: &CStr, feedback: Option<DmabufFeedback>) -> Option<Self> {
        let egl = eglgbm::EglDisplay::new(render_node).unwrap();
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

        let mut framebuffer = 0;
        let mut renderbuffer = 0;
        let mut verts_buffer = 0;
        let mut vertex_array = 0;
        let shader;

        unsafe {
            gl.Enable(gl46::GL_BLEND);
            gl.BlendFunc(gl46::GL_ONE, gl46::GL_ONE_MINUS_SRC_ALPHA);

            gl.GenFramebuffers(1, &mut framebuffer);
            gl.GenRenderbuffers(1, &mut renderbuffer);
            gl.GenVertexArrays(1, &mut vertex_array);
            gl.CreateBuffers(1, &mut verts_buffer);

            gl.BindFramebuffer(gl46::GL_FRAMEBUFFER, framebuffer);
            gl.BindRenderbuffer(gl46::GL_RENDERBUFFER, renderbuffer);
            gl.DrawBuffers(1, &gl46::GL_COLOR_ATTACHMENT0);

            gl.BindVertexArray(vertex_array);
            gl.BindVertexBuffer(0, verts_buffer, 0, std::mem::size_of::<Vert>() as i32);
            gl.EnableVertexAttribArray(0);
            gl.EnableVertexAttribArray(1);
            gl.VertexAttribBinding(0, 0);
            gl.VertexAttribBinding(1, 0);
            gl.VertexAttribFormat(0, 2, gl46::GL_FLOAT, 0, 0);
            gl.VertexAttribFormat(1, 4, gl46::GL_FLOAT, 0, 8);

            shader = create_shader(&gl);
            gl.UseProgram(shader);

            let units = std::array::from_fn::<i32, 32, _>(|i| i as i32);
            gl.Uniform1iv(1, 32, units.as_ptr());
        }

        let (fourcc, mods) = match feedback {
            Some(feedback) => select_format_from_feedback(&egl, feedback),
            None => (
                DRM_FORMAT_XRGB8888,
                egl.supported_formats()
                    .get(&DRM_FORMAT_XRGB8888)
                    .expect("xrgb8888 not supported")
                    .clone(),
            ),
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

            bound_textures: 0,

            framebuffer,
            renderbuffer,

            gl: Box::new(gl),
            _context: egl_context,
            egl,
        })
    }

    pub fn allocate_buffer(&mut self, width: u32, height: u32) -> (eglgbm::EglImage, BufferExport) {
        eprintln!("allocating buffer {width}x{height}");
        self.egl
            .alloc_buffer(width, height, self.fourcc, &self.mods)
            .unwrap()
    }

    pub fn frame<'a>(
        &'a mut self,
        width: u32,
        height: u32,
        image: &eglgbm::EglImage,
    ) -> Box<dyn Frame + 'a> {
        self.bound_textures = 0;

        unsafe {
            image.set_as_gl_renderbuffer_storage();
            self.gl.FramebufferRenderbuffer(
                gl46::GL_FRAMEBUFFER,
                gl46::GL_COLOR_ATTACHMENT0,
                gl46::GL_RENDERBUFFER,
                self.renderbuffer,
            );

            assert_eq!(
                self.gl
                    .CheckNamedFramebufferStatus(self.framebuffer, gl46::GL_FRAMEBUFFER),
                gl46::GL_FRAMEBUFFER_COMPLETE
            );

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
        unsafe {
            if !self.verts.is_empty() {
                self.gl.NamedBufferData(
                    self.verts_buffer,
                    std::mem::size_of_val(self.verts.as_slice()) as isize,
                    self.verts.as_ptr().cast(),
                    gl46::GL_STREAM_DRAW,
                );
                self.gl
                    .DrawArrays(gl46::GL_TRIANGLES, 0, self.verts.len() as i32);
                self.verts.clear();
            }

            self.gl.Finish();
        }
    }

    fn consider_dropping_shm_pool(&mut self, pool_id: ShmPoolId) {
        let shm_pool = self.shm_pools.get(&pool_id).unwrap();
        if !shm_pool.resource_alive && shm_pool.refcnt == 0 {
            self.shm_pools.remove(&pool_id);
        }
    }

    fn drop_buffer(&mut self, buffer_id: BufferId) {
        let buffer = self.textures.remove(&buffer_id).unwrap();
        assert_eq!(buffer.locks, 0);
        unsafe { self.gl.DeleteTextures(1, &buffer.gl_name) };
    }
}

impl RendererState for RendererStateImp {
    fn create_shm_pool(&mut self, fd: OwnedFd, size: usize) -> ShmPoolId {
        let id = ShmPoolId(next_id(&mut self.next_id));
        self.shm_pools.insert(
            id,
            ShmPool {
                memmap: unsafe { memmap2::MmapOptions::new().len(size).map(&fd).unwrap() },
                size,
                resource_alive: true,
                refcnt: 0,
            },
        );
        id
    }

    fn resize_shm_pool(&mut self, pool_id: ShmPoolId, new_size: usize) {
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

    fn shm_pool_resource_destroyed(&mut self, pool_id: ShmPoolId) {
        self.shm_pools.get_mut(&pool_id).unwrap().resource_alive = false;
        self.consider_dropping_shm_pool(pool_id);
    }

    fn create_shm_buffer(&mut self, spec: ShmBufferSpec, resource: crate::protocol::WlBuffer) {
        self.shm_pools.get_mut(&spec.pool_id).unwrap().refcnt += 1;
        self.shm_buffers.insert(resource, spec);
    }

    fn buffer_commited(&mut self, buffer_resource: crate::protocol::WlBuffer) -> BufferId {
        let spec = self.shm_buffers.get(&buffer_resource).unwrap();

        buffer_resource.release();
        let pool = &self.shm_pools[&spec.pool_id];
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

    fn buffer_resource_destroyed(&mut self, resource: crate::protocol::WlBuffer) {
        let shm_spec = self.shm_buffers.remove(&resource).unwrap();
        self.shm_pools.get_mut(&shm_spec.pool_id).unwrap().refcnt -= 1;
        self.consider_dropping_shm_pool(shm_spec.pool_id);
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
        assert!(self.state.bound_textures < 32);
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

unsafe fn create_shader(gl: &gl46::GlFns) -> u32 {
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

    let fragment_shader = b"
        #version 460 core
        in vec4 v_Color;
        out vec4 frag_color;
        layout(location = 1) uniform sampler2D u_Textures[32];
        void main() {
            if (v_Color.a < 0.0) {
                int tex_i = int(v_Color.b);
                frag_color = texture(u_Textures[tex_i], v_Color.rg) * (-1.0 - v_Color.a);
            } else {
                frag_color = v_Color;
            }
        }\0";

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

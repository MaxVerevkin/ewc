use std::collections::VecDeque;
use std::os::fd::AsRawFd;

use wayrs_client::proxy::Proxy as _;
use wayrs_client::{Connection, IoMode};
use wayrs_client::{EventCtx, protocol::*};
use wayrs_protocols::linux_dmabuf_v1::*;
use wayrs_protocols::xdg_shell::*;
use wayrs_utils::dmabuf_feedback::{DmabufFeedback, DmabufFeedbackHandler};
use wayrs_utils::seats::{SeatHandler, Seats};
use wayrs_utils::shm_alloc::{BufferSpec, ShmAlloc};

use super::*;

struct BackendImp {
    conn: Connection<State>,
    state: State,
}

pub fn new() -> Option<Box<dyn Backend>> {
    let mut conn = Connection::connect()
        .map_err(|_| eprintln!("backend/wayland: could not connect to a wayland compositor"))
        .ok()?;
    conn.blocking_roundtrip()
        .map_err(|_| eprintln!("backend/wayland: roundtrip failed"))
        .ok()?;

    let seats = Seats::bind(&mut conn);
    let wl_compositor: WlCompositor = conn
        .bind_singleton(5..=6)
        .map_err(|_| eprintln!("backend/wayland: wl_compositor v5-6 is required"))
        .ok()?;
    let xdg_wm_base: XdgWmBase = conn
        .bind_singleton(1)
        .map_err(|_| eprintln!("backend/wayland: xdg_wb_base is required"))
        .ok()?;
    let linux_dmabuf: Option<ZwpLinuxDmabufV1> = conn
        .bind_singleton(4)
        .map_err(|_| eprintln!("backend/wayland: linux-dmabuf is not supported"))
        .ok();

    let wl_surface = wl_compositor.create_surface(&mut conn);
    let xdg_surface = xdg_wm_base.get_xdg_surface_with_cb(&mut conn, wl_surface, xdg_surface_cb);
    let xdg_toplevel = xdg_surface.get_toplevel_with_cb(&mut conn, xdg_toplevel_cb);
    wl_surface.commit(&mut conn);

    let renderer_kind = match linux_dmabuf {
        Some(linux_dmabuf) if std::env::var_os("EWC_NO_GL").is_none() => RendererKind::OpenGl {
            linux_dmabuf,
            swapchain: None,
            feedback: DmabufFeedback::get_default(&mut conn, linux_dmabuf),
            state: None,
        },
        _ => RendererKind::Pixman {
            shm: ShmAlloc::bind(&mut conn).unwrap(),
            state: pixman_renderer::RendererStateImp::new(),
        },
    };

    let mut state = State {
        backend_events_queue: VecDeque::new(),
        renderer_kind,

        seats,

        next_input_id: NonZeroU64::MIN,
        keyboards: Vec::new(),
        pointers: Vec::new(),

        wl_surface,
        xdg_surface,
        xdg_toplevel,
        throttle_cb: None,
        mapped: false,
        width: 80,
        height: 60,
    };

    if linux_dmabuf.is_some() {
        loop {
            conn.dispatch_events(&mut state);
            conn.flush(IoMode::Blocking).unwrap();

            let RendererKind::OpenGl {
                state: gl_state, ..
            } = &mut state.renderer_kind
            else {
                unreachable!()
            };
            if gl_state.is_some() {
                break;
            }

            conn.recv_events(IoMode::Blocking).unwrap();
        }
    }

    conn.flush(IoMode::Blocking).unwrap();
    Some(Box::new(BackendImp { conn, state }))
}

impl Backend for BackendImp {
    fn register_fds_with(
        &self,
        reg: &'_ mut dyn FnMut(RawFd, u32) -> io::Result<()>,
    ) -> io::Result<()> {
        reg(self.conn.as_raw_fd(), 0)
    }

    fn poll(&mut self, _data: u32) -> io::Result<()> {
        match self.conn.recv_events(IoMode::NonBlocking) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) => return Err(e),
        }
        self.conn.dispatch_events(&mut self.state);
        self.conn.flush(IoMode::Blocking)
    }

    fn next_event(&mut self) -> Option<BackendEvent> {
        self.state.backend_events_queue.pop_front()
    }

    fn switch_vt(&mut self, _vt: u32) {}

    fn pointer_get_name(&self, _id: PointerId) -> Option<&str> {
        Some("wl_pointer")
    }

    fn pointer_configure(&mut self, _id: PointerId, _config: &PointerConfig) {}

    fn renderer_state(&mut self) -> &mut dyn RendererState {
        match &mut self.state.renderer_kind {
            RendererKind::Pixman { state, .. } => state,
            RendererKind::OpenGl { state, .. } => state.as_mut().unwrap().as_mut(),
        }
    }

    fn render_frame(&mut self, clear: Color, render_list: &[RenderNode], time: u32) {
        assert!(self.state.mapped);
        assert!(self.state.throttle_cb.is_none());

        self.state.throttle_cb = Some(self.state.wl_surface.frame_with_cb(&mut self.conn, |ctx| {
            assert_eq!(ctx.state.throttle_cb, Some(ctx.proxy));
            ctx.state.throttle_cb = None;
            ctx.state
                .backend_events_queue
                .push_back(BackendEvent::Frame);
        }));

        match &mut self.state.renderer_kind {
            RendererKind::Pixman { shm, state } => {
                let (buffer, canvas) = shm
                    .alloc_buffer(
                        &mut self.conn,
                        BufferSpec {
                            width: self.state.width,
                            height: self.state.height,
                            stride: self.state.width * 4,
                            format: wl_shm::Format::Argb8888,
                        },
                    )
                    .unwrap();
                let mut frame = state.frame(
                    canvas,
                    self.state.width,
                    self.state.height,
                    crate::protocol::wl_shm::Format::Argb8888,
                );
                frame.clear(clear.r, clear.g, clear.b);
                frame.render(render_list, time);
                self.state
                    .wl_surface
                    .attach(&mut self.conn, Some(buffer.into_wl_buffer()), 0, 0);
            }
            RendererKind::OpenGl {
                linux_dmabuf,
                swapchain,
                feedback: _,
                state,
            } => 'blk: {
                let state = state.as_mut().unwrap();
                if let Some(sw) = swapchain {
                    if sw.width != self.state.width || sw.height != self.state.height {
                        let sw = swapchain.take().unwrap();
                        for buf in sw.bufs {
                            buf.destroy(&mut self.conn, state.gl());
                        }
                    }
                }

                let sw = swapchain.get_or_insert_with(|| GlSwapchain {
                    width: self.state.width,
                    height: self.state.height,
                    bufs: Vec::new(),
                });

                let buf = if let Some(buf) = sw.bufs.iter_mut().find(|buf| !buf.in_use) {
                    buf
                } else if sw.bufs.len() < 2 {
                    let (fb, export) = state.allocate_framebuffer(sw.width, sw.height, false);
                    let params = linux_dmabuf.create_params(&mut self.conn);
                    for (i, plane) in export.planes.into_iter().enumerate() {
                        params.add(
                            &mut self.conn,
                            plane.dmabuf,
                            i as u32,
                            plane.offset,
                            plane.stride,
                            (export.modifier >> 32) as u32,
                            export.modifier as u32,
                        );
                    }
                    let wl = params.create_immed_with_cb(
                        &mut self.conn,
                        sw.width as i32,
                        sw.height as i32,
                        export.format.0,
                        zwp_linux_buffer_params_v1::Flags::empty(),
                        dmabuf_wl_buffer_cb,
                    );
                    params.destroy(&mut self.conn);
                    sw.bufs.push(GlBuf {
                        wl,
                        fb,
                        in_use: false,
                    });
                    sw.bufs.last_mut().unwrap()
                } else {
                    eprintln!("backend/wayland/gl46: skipping frame, not enough buffers");
                    break 'blk;
                };
                assert!(!buf.in_use);

                let mut frame = state.frame(sw.width, sw.height, &buf.fb);
                frame.clear(clear.r, clear.g, clear.b);
                frame.render(render_list, time);
                drop(frame);
                state.finish_frame();

                buf.in_use = true;
                self.state
                    .wl_surface
                    .attach(&mut self.conn, Some(buf.wl), 0, 0);
            }
        }

        self.state
            .wl_surface
            .damage(&mut self.conn, 0, 0, i32::MAX, i32::MAX);
        self.state.wl_surface.commit(&mut self.conn);
        self.conn.flush(IoMode::Blocking).unwrap();
    }
}

struct State {
    backend_events_queue: VecDeque<BackendEvent>,
    renderer_kind: RendererKind,

    seats: Seats,

    next_input_id: NonZeroU64,
    keyboards: Vec<Keyboard>,
    pointers: Vec<Pointer>,

    wl_surface: WlSurface,
    #[allow(dead_code)]
    xdg_surface: XdgSurface,
    #[allow(dead_code)]
    xdg_toplevel: XdgToplevel,
    throttle_cb: Option<WlCallback>,
    mapped: bool,
    width: u32,
    height: u32,
}

enum RendererKind {
    Pixman {
        shm: ShmAlloc,
        state: pixman_renderer::RendererStateImp,
    },
    OpenGl {
        linux_dmabuf: ZwpLinuxDmabufV1,
        swapchain: Option<GlSwapchain>,
        feedback: DmabufFeedback,
        // TODO: instead of wrapping this in an Option, make GL renderer capable of switching main device at runtime, and make it work initially without any main device.
        state: Option<Box<gl46_renderer::RendererStateImp>>,
    },
}

struct GlBuf {
    wl: WlBuffer,
    fb: gl46_renderer::Framebuffer,
    in_use: bool,
}

impl GlBuf {
    fn destroy(self, conn: &mut Connection<State>, gl: &gl46::GlFns) {
        self.wl.destroy(conn);
        self.fb.destroy(gl);
    }
}

struct GlSwapchain {
    width: u32,
    height: u32,
    bufs: Vec<GlBuf>,
}

struct Keyboard {
    id: KeyboardId,
    wl: WlKeyboard,
    seat: WlSeat,
    pressed_keys: Vec<u32>,
    entered_with_keys: Vec<u32>,
}

struct Pointer {
    id: PointerId,
    wl: WlPointer,
    seat: WlSeat,
}

impl SeatHandler for State {
    fn get_seats(&mut self) -> &mut Seats {
        &mut self.seats
    }

    fn keyboard_added(&mut self, conn: &mut Connection<Self>, seat: WlSeat) {
        let id = KeyboardId(next_id(&mut self.next_input_id));
        let wl = seat.get_keyboard_with_cb(conn, wl_keyboard_cb);
        self.keyboards.push(Keyboard {
            id,
            wl,
            seat,
            pressed_keys: Vec::new(),
            entered_with_keys: Vec::new(),
        });
        self.backend_events_queue
            .push_back(BackendEvent::NewKeyboard(id));
    }

    fn keyboard_removed(&mut self, conn: &mut Connection<Self>, seat: WlSeat) {
        let i = self.keyboards.iter().position(|k| k.seat == seat).unwrap();
        let kbd = self.keyboards.swap_remove(i);
        if kbd.wl.version() >= 3 {
            kbd.wl.release(conn);
        }
        self.backend_events_queue
            .push_back(BackendEvent::KeyboardRemoved(kbd.id));
    }

    fn pointer_added(&mut self, conn: &mut Connection<Self>, seat: WlSeat) {
        let id = PointerId(next_id(&mut self.next_input_id));
        let wl = seat.get_pointer_with_cb(conn, wl_pointer_cb);
        self.pointers.push(Pointer { id, wl, seat });
        self.backend_events_queue
            .push_back(BackendEvent::NewPointer(id));
    }

    fn pointer_removed(&mut self, conn: &mut Connection<Self>, seat: WlSeat) {
        let i = self.pointers.iter().position(|k| k.seat == seat).unwrap();
        let ptr = self.pointers.swap_remove(i);
        if ptr.wl.version() >= 3 {
            ptr.wl.release(conn);
        }
        self.backend_events_queue
            .push_back(BackendEvent::PointerRemoved(ptr.id));
    }
}

impl DmabufFeedbackHandler for State {
    fn get_dmabuf_feedback(&mut self, wl: ZwpLinuxDmabufFeedbackV1) -> &mut DmabufFeedback {
        let RendererKind::OpenGl { feedback, .. } = &mut self.renderer_kind else { unreachable!() };
        assert_eq!(feedback.wl(), wl);
        feedback
    }

    fn feedback_done(&mut self, _conn: &mut Connection<Self>, wl: ZwpLinuxDmabufFeedbackV1) {
        let RendererKind::OpenGl {
            feedback, state, ..
        } = &mut self.renderer_kind
        else {
            unreachable!()
        };
        assert_eq!(feedback.wl(), wl);
        let drm_device = eglgbm::DrmDevice::new_from_id(feedback.main_device().unwrap()).unwrap();
        let render_node_path = drm_device.render_node().unwrap();
        *state = Some(Box::new(
            gl46_renderer::RendererStateImp::new(render_node_path, feedback).unwrap(),
        ));
    }
}

fn wl_keyboard_cb(ctx: EventCtx<State, WlKeyboard>) {
    let kbd = ctx
        .state
        .keyboards
        .iter_mut()
        .find(|k| k.wl == ctx.proxy)
        .unwrap();

    use wl_keyboard::Event;
    match ctx.event {
        Event::Keymap(_) => (),
        Event::Enter(args) => {
            kbd.entered_with_keys = args
                .keys
                .chunks_exact(4)
                .map(|key| u32::from_ne_bytes(key.try_into().unwrap()))
                .collect();
        }
        Event::Leave(_) => {
            // for key in kbd.pressed_keys.drain(..) {
            //     ctx.state
            //         .backend_events_queue
            //         .push_back(BackendEvent::KeyReleased(kbd.id, key));
            // }
        }
        Event::Key(args) => {
            let timestamp = InputTimestamp(args.time);
            use wl_keyboard::KeyState;
            match args.state {
                KeyState::Released => {
                    if let Some(i) = kbd.entered_with_keys.iter().position(|k| *k == args.key) {
                        kbd.entered_with_keys.swap_remove(i);
                    } else {
                        kbd.pressed_keys.retain(|k| *k != args.key);
                        ctx.state
                            .backend_events_queue
                            .push_back(BackendEvent::KeyReleased(kbd.id, timestamp, args.key));
                    }
                }
                KeyState::Pressed => {
                    kbd.pressed_keys.push(args.key);
                    ctx.state
                        .backend_events_queue
                        .push_back(BackendEvent::KeyPressed(kbd.id, timestamp, args.key));
                }
                _ => unreachable!(),
            };
        }
        Event::Modifiers(_) => (),
        Event::RepeatInfo(_) => (),
        _ => (),
    }
}

fn wl_pointer_cb(ctx: EventCtx<State, WlPointer>) {
    let ptr = ctx
        .state
        .pointers
        .iter_mut()
        .find(|k| k.wl == ctx.proxy)
        .unwrap();

    use wl_pointer::Event;
    match ctx.event {
        Event::Enter(args) => {
            ptr.wl.set_cursor(ctx.conn, args.serial, None, 0, 0);
            // ctx.state
            //     .backend_events_queue
            //     .push_back(BackendEvent::PointerMotionAbsolute(
            //         ptr.id,
            //         args.surface_x.as_f32(),
            //         args.surface_y.as_f32(),
            //     ))
        }
        // Event::Leave(_) => todo!(),
        Event::Motion(args) => {
            ctx.state
                .backend_events_queue
                .push_back(BackendEvent::PointerMotionAbsolute(
                    ptr.id,
                    InputTimestamp(args.time),
                    args.surface_x.as_f32(),
                    args.surface_y.as_f32(),
                ))
        }
        Event::Button(args) => {
            let timestamp = InputTimestamp(args.time);
            ctx.state.backend_events_queue.push_back(match args.state {
                wl_pointer::ButtonState::Released => {
                    BackendEvent::PointerBtnRelease(ptr.id, timestamp, args.button)
                }
                wl_pointer::ButtonState::Pressed => {
                    BackendEvent::PointerBtnPress(ptr.id, timestamp, args.button)
                }
                _ => unreachable!(),
            });
        }
        Event::Axis(args) => {
            if args.axis == wl_pointer::Axis::VerticalScroll {
                ctx.state
                    .backend_events_queue
                    .push_back(BackendEvent::PointerAxisVertial(
                        ptr.id,
                        InputTimestamp(args.time),
                        args.value.as_f32(),
                    ));
            }
        }
        // Event::Frame => todo!(),
        // Event::AxisSource(_) => todo!(),
        // Event::AxisStop(_) => todo!(),
        // Event::AxisDiscrete(_) => todo!(),
        // Event::AxisValue120(_) => todo!(),
        // Event::AxisRelativeDirection(_) => todo!(),
        _ => (),
    }
}

fn xdg_surface_cb(ctx: EventCtx<State, XdgSurface>) {
    if let xdg_surface::Event::Configure(serial) = ctx.event {
        ctx.proxy.ack_configure(ctx.conn, serial);
        if !ctx.state.mapped {
            ctx.state.mapped = true;
            ctx.state
                .backend_events_queue
                .push_back(BackendEvent::Frame);
        }
    }
}

fn xdg_toplevel_cb(ctx: EventCtx<State, XdgToplevel>) {
    match ctx.event {
        xdg_toplevel::Event::Configure(args) => {
            if args.width != 0 {
                ctx.state.width = args.width.try_into().unwrap();
            }
            if args.height != 0 {
                ctx.state.height = args.height.try_into().unwrap();
            }
        }
        xdg_toplevel::Event::Close => {
            ctx.state
                .backend_events_queue
                .push_back(BackendEvent::ShutDown);
            ctx.conn.break_dispatch_loop();
        }
        _ => unreachable!(),
    }
}

fn dmabuf_wl_buffer_cb(ctx: EventCtx<State, WlBuffer>) {
    let wl_buffer::Event::Release = ctx.event;
    let RendererKind::OpenGl { swapchain, .. } = &mut ctx.state.renderer_kind else {
        unreachable!()
    };
    if let Some(sw) = swapchain {
        if let Some(buf) = sw.bufs.iter_mut().find(|buf| buf.wl == ctx.proxy) {
            buf.in_use = false;
        }
    }
}

use std::collections::VecDeque;
use std::io;
use std::num::NonZeroU64;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use wayrs_client::global::GlobalsExt;
use wayrs_client::proxy::Proxy as _;
use wayrs_client::Connection;
use wayrs_client::IoMode;
use wayrs_client::{protocol::*, EventCtx};
use wayrs_protocols::xdg_shell::*;
use wayrs_utils::seats::{SeatHandler, Seats};
use wayrs_utils::shm_alloc::{BufferSpec, ShmAlloc};

use super::pixman_renderer::*;
use super::*;

struct BackendImp {
    conn: Connection<State>,
    state: State,
    renderer_state: RendererState,
}

pub fn new() -> Option<Box<dyn Backend>> {
    let (mut conn, globals) = Connection::<State>::connect_and_collect_globals()
        .map_err(|_| eprintln!("backend/wayland: could not connect to a wayland compositor"))
        .ok()?;

    let shm = ShmAlloc::bind(&mut conn, &globals).unwrap();
    let seats = Seats::bind(&mut conn, &globals);
    let wl_compositor: WlCompositor = globals.bind(&mut conn, 6).unwrap();
    let xdg_wm_base: XdgWmBase = globals.bind(&mut conn, 1).unwrap();

    let wl_surface = wl_compositor.create_surface(&mut conn);
    let xdg_surface = xdg_wm_base.get_xdg_surface_with_cb(&mut conn, wl_surface, |ctx| {
        if let xdg_surface::Event::Configure(serial) = ctx.event {
            ctx.proxy.ack_configure(ctx.conn, serial);
            if !ctx.state.mapped {
                ctx.state.mapped = true;
                ctx.state
                    .backend_events_queue
                    .push_back(BackendEvent::Frame);
            }
        }
    });
    let xdg_toplevel = xdg_surface.get_toplevel_with_cb(&mut conn, |ctx| match ctx.event {
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
    });
    wl_surface.commit(&mut conn);

    let state = State {
        backend_events_queue: VecDeque::new(),

        shm,
        seats,

        next_input_id: NonZeroU64::MIN,
        keyboards: Vec::new(),
        pointers: Vec::new(),

        wl_surface,
        xdg_surface,
        xdg_toplevel,
        throttle_cb: None,
        mapped: false,
        width: 800,
        height: 600,
    };
    conn.flush(IoMode::Blocking).unwrap();
    Some(Box::new(BackendImp {
        conn,
        state,
        renderer_state: RendererState::new(),
    }))
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

    fn create_shm_pool(&mut self, fd: OwnedFd, size: usize) -> ShmPoolId {
        self.renderer_state.create_shm_pool(fd, size)
    }

    fn resize_shm_pool(&mut self, pool_id: ShmPoolId, new_size: usize) {
        self.renderer_state.resize_shm_pool(pool_id, new_size)
    }

    fn shm_pool_resource_destroyed(&mut self, pool_id: ShmPoolId) {
        self.renderer_state.shm_pool_resource_destroyed(pool_id)
    }

    fn create_shm_buffer(
        &mut self,
        pool_id: ShmPoolId,
        offset: usize,
        wl_format: u32,
        width: u32,
        height: u32,
        stride: u32,
        resource: crate::protocol::WlBuffer,
    ) -> BufferId {
        self.renderer_state
            .create_shm_buffer(pool_id, offset, wl_format, width, height, stride, resource)
    }

    fn get_buffer_size(&self, buffer_id: BufferId) -> (u32, u32) {
        self.renderer_state.get_buffer_size(buffer_id)
    }

    fn buffer_lock(&mut self, buffer_id: BufferId) {
        self.renderer_state.buffer_lock(buffer_id)
    }

    fn buffer_unlock(&mut self, buffer_id: BufferId) {
        self.renderer_state.buffer_unlock(buffer_id)
    }

    fn buffer_resource_destroyed(&mut self, buffer_id: BufferId) {
        self.renderer_state.buffer_resource_destroyed(buffer_id)
    }

    fn render_frame(&mut self, f: &mut dyn FnMut(&mut dyn Frame)) {
        const FORMAT: wl_shm::Format = wl_shm::Format::Argb8888;

        // eprintln!("start frame");
        assert!(self.state.mapped);
        assert!(self.state.throttle_cb.is_none());
        let (buffer, canvas) = self.state.shm.alloc_buffer(
            &mut self.conn,
            BufferSpec {
                width: self.state.width,
                height: self.state.height,
                stride: self.state.width * 4,
                format: FORMAT,
            },
        );
        self.state.throttle_cb = Some(self.state.wl_surface.frame_with_cb(&mut self.conn, |ctx| {
            assert_eq!(ctx.state.throttle_cb, Some(ctx.proxy));
            ctx.state.throttle_cb = None;
            ctx.state
                .backend_events_queue
                .push_back(BackendEvent::Frame);
        }));
        f(&mut FrameImp {
            time: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u32,
            width: self.state.width,
            height: self.state.height,
            renderer: pixman_renderer::Renderer::new(
                &self.renderer_state,
                canvas,
                self.state.width,
                self.state.height,
                FORMAT as u32,
            ),
        });
        self.state
            .wl_surface
            .attach(&mut self.conn, Some(buffer.into_wl_buffer()), 0, 0);
        self.state
            .wl_surface
            .damage(&mut self.conn, 0, 0, i32::MAX, i32::MAX);
        self.state.wl_surface.commit(&mut self.conn);
        self.conn.flush(IoMode::Blocking).unwrap();
        // eprintln!("end frame");
    }
}

struct FrameImp<'a> {
    time: u32,
    width: u32,
    height: u32,
    renderer: super::pixman_renderer::Renderer<'a>,
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
        self.renderer.clear(r, g, b);
    }

    fn render_buffer(
        &mut self,
        buf: BufferId,
        opaque_region: Option<&pixman::Region32>,
        alpha: f32,
        x: i32,
        y: i32,
    ) {
        self.renderer.render_buffer(buf, opaque_region, alpha, x, y);
    }

    fn render_rect(&mut self, r: f32, g: f32, b: f32, a: f32, x: i32, y: i32, w: u32, h: u32) {
        self.renderer.render_rect(r, g, b, a, x, y, w, h);
    }
}

struct State {
    backend_events_queue: VecDeque<BackendEvent>,

    shm: ShmAlloc,
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
            for key in kbd.pressed_keys.drain(..) {
                ctx.state
                    .backend_events_queue
                    .push_back(BackendEvent::KeyReleased(kbd.id, key));
            }
        }
        Event::Key(args) => {
            use wl_keyboard::KeyState;
            match args.state {
                KeyState::Released => {
                    if let Some(i) = kbd.entered_with_keys.iter().position(|k| *k == args.key) {
                        kbd.entered_with_keys.swap_remove(i);
                    } else {
                        kbd.pressed_keys.retain(|k| *k != args.key);
                        ctx.state
                            .backend_events_queue
                            .push_back(BackendEvent::KeyReleased(kbd.id, args.key));
                    }
                }
                KeyState::Pressed => {
                    kbd.pressed_keys.push(args.key);
                    ctx.state
                        .backend_events_queue
                        .push_back(BackendEvent::KeyPressed(kbd.id, args.key));
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
            ctx.state
                .backend_events_queue
                .push_back(BackendEvent::PointerMotionAbsolute(
                    ptr.id,
                    args.surface_x.as_f32(),
                    args.surface_y.as_f32(),
                ))
        }
        // Event::Leave(_) => todo!(),
        Event::Motion(args) => {
            ctx.state
                .backend_events_queue
                .push_back(BackendEvent::PointerMotionAbsolute(
                    ptr.id,
                    args.surface_x.as_f32(),
                    args.surface_y.as_f32(),
                ))
        }
        Event::Button(args) => {
            ctx.state.backend_events_queue.push_back(match args.state {
                wl_pointer::ButtonState::Released => {
                    BackendEvent::PointerBtnRelease(ptr.id, args.button)
                }
                wl_pointer::ButtonState::Pressed => {
                    BackendEvent::PointerBtnPress(ptr.id, args.button)
                }
                _ => unreachable!(),
            });
        }
        // Event::Axis(_) => todo!(),
        // Event::Frame => todo!(),
        // Event::AxisSource(_) => todo!(),
        // Event::AxisStop(_) => todo!(),
        // Event::AxisDiscrete(_) => todo!(),
        // Event::AxisValue120(_) => todo!(),
        // Event::AxisRelativeDirection(_) => todo!(),
        _ => (),
    }
}

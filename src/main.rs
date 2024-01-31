#![allow(unreachable_code, clippy::new_without_default, incomplete_features)]
#![feature(inline_const_pat, pointer_is_aligned)]

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::env;
use std::io;
use std::num::NonZeroU32;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use xkbcommon::xkb;

mod backend;
mod client;
mod cursor;
mod event_loop;
mod focus_stack;
mod globals;
mod protocol;
mod seat;
mod wayland_core;

use crate::backend::{Backend, BackendEvent, Color, Frame};
use crate::client::{Client, ClientId};
use crate::cursor::Cursor;
use crate::event_loop::EventLoop;
use crate::focus_stack::FocusStack;
use crate::globals::compositor::{Compositor, Surface};
use crate::globals::ewc_debug::Debugger;
use crate::globals::GlobalsManager;
use crate::protocol::xdg_toplevel::ResizeEdge;
use crate::protocol::*;
use crate::seat::pointer::{PtrState, BTN_LEFT, BTN_RIGHT};
use crate::seat::Seat;
use crate::wayland_core::*;

#[macro_export]
macro_rules! debug {
    ($debugger:expr, $($fmt:tt)*) => {
        if $debugger.accum_interest().contains($crate::protocol::ewc_debug_v1::Interest::Messages) {
            $debugger.message(&format!($($fmt)*));
        }
    };
}

pub struct Server {
    socket: UnixListener,
    socket_path: PathBuf,
    to_flush_set: Rc<ToFlushSet>,
    clients: HashMap<ClientId, Client>,
    next_client_id: ClientId,
    event_loop: EventLoop,
    state: State,
}

pub struct State {
    pub globals: GlobalsManager,
    pub backend: Box<dyn Backend>,
    pub seat: Seat,
    pub cursor: Cursor,
    pub focus_stack: FocusStack,
    pub debugger: Debugger,
}

#[derive(Default, Clone)]
pub struct ToFlushSet(RefCell<HashSet<ClientId>>);

impl ToFlushSet {
    pub fn add(&self, client_id: ClientId) {
        self.0.borrow_mut().insert(client_id);
    }
}

fn choose_backend() -> Box<dyn Backend> {
    if let Some(b) = backend::wayland::new() {
        eprintln!("using wayland backend");
        return b;
    }

    if let Some(b) = backend::drmkms::new() {
        eprintln!("using drmkms backend");
        return b;
    }

    panic!("No backend available")
}

impl Server {
    pub fn destroy_client(&mut self, client_id: ClientId) {
        eprintln!("destroying client");
        self.state.globals.remove_client(client_id);
        self.state.seat.remove_client(client_id);
        self.state.focus_stack.remove_client(client_id);
        self.state.debugger.remove_client(client_id);
        let client = self.clients.remove(&client_id).unwrap();
        client.shm.destroy(&mut self.state);
        self.event_loop.remove(client.conn.as_raw_fd()).unwrap();
    }

    pub fn new(socket_path: PathBuf) -> Self {
        let mut backend = choose_backend();
        let socket = UnixListener::bind(&socket_path).unwrap();
        socket.set_nonblocking(true).unwrap();
        let mut event_loop = EventLoop::new().unwrap();
        event_loop
            .add_fd(socket.as_raw_fd(), event_loop::Event::Socket)
            .unwrap();
        backend
            .register_fds_with(&mut |fd, data| {
                event_loop.add_fd(fd, event_loop::Event::Backend(data))
            })
            .unwrap();
        let cursor = Cursor::new(backend.as_mut());
        let mut globals = GlobalsManager::default();
        Compositor::register_globals(&mut globals);
        Seat::register_globals(&mut globals);
        globals.add_global::<WlShm>(1);
        globals.add_global::<XdgWmBase>(3);
        globals.add_global::<WlOutput>(2);
        globals.add_global::<EwcDebugV1>(1);
        Self {
            socket,
            socket_path,
            to_flush_set: Rc::new(ToFlushSet::default()),
            clients: HashMap::new(),
            next_client_id: ClientId::first(),
            event_loop,
            state: State {
                globals,
                backend,
                cursor,
                seat: Seat::new(),
                focus_stack: FocusStack::default(),
                debugger: Debugger::default(),
            },
        }
    }
}

fn render_surface(frame: &mut dyn Frame, surf: &Surface, alpha: f32, x: i32, y: i32) {
    for frame_cb in surf.cur.borrow_mut().frame_cbs.drain(..) {
        frame_cb.done(frame.time());
    }
    let Some((buf_id, _, _)) = surf.cur.borrow().buffer else { return };
    frame.render_buffer(
        buf_id,
        surf.cur.borrow().opaque_region.as_ref(),
        alpha,
        x,
        y,
    );
    for sub in &surf.cur.borrow().subsurfaces.clone() {
        let position = sub.position;
        render_surface(frame, &sub.surface, alpha, x + position.0, y + position.1);
    }
}

impl Server {
    fn pointer_moved(&mut self) {
        match &self.state.seat.pointer.state {
            PtrState::Moving {
                toplevel,
                ptr_start_x: px,
                ptr_start_y: py,
                toplevel_start_x: tx,
                toplevel_start_y: ty,
            } => {
                let toplevel = toplevel.upgrade().unwrap();
                toplevel
                    .x
                    .set(tx + (self.state.seat.pointer.x - px).round() as i32);
                toplevel
                    .y
                    .set(ty + (self.state.seat.pointer.y - py).round() as i32);
            }
            PtrState::Resizing {
                toplevel,
                edge,
                ptr_start_x: px,
                ptr_start_y: py,
                toplevel_start_width: sw,
                toplevel_start_height: sh,
            } => {
                let toplevel = toplevel.upgrade().unwrap();
                let mut dw = 0;
                let mut dh = 0;
                if *edge as u32 & ResizeEdge::Top as u32 != 0 {
                    dh = (*py - self.state.seat.pointer.y).round() as i32;
                }
                if *edge as u32 & ResizeEdge::Bottom as u32 != 0 {
                    dh = (self.state.seat.pointer.y - *py).round() as i32;
                }
                if *edge as u32 & ResizeEdge::Right as u32 != 0 {
                    dw = (self.state.seat.pointer.x - *px).round() as i32;
                }
                if *edge as u32 & ResizeEdge::Left as u32 != 0 {
                    dw = (*px - self.state.seat.pointer.x).round() as i32;
                }
                if dw != 0 || dh != 0 {
                    toplevel.request_size(
                        *edge,
                        NonZeroU32::new(sw.checked_add_signed(dw).unwrap_or(1))
                            .unwrap_or(NonZeroU32::MIN),
                        NonZeroU32::new(sh.checked_add_signed(dh).unwrap_or(1))
                            .unwrap_or(NonZeroU32::MIN),
                    );
                }
            }
            _ => {
                if let Some((_i, surf, sx, sy)) = self.state.focus_stack.surface_at(
                    self.state.seat.pointer.x.round() as i32,
                    self.state.seat.pointer.y.round() as i32,
                ) {
                    self.state
                        .seat
                        .pointer
                        .forward_pointer(Some((surf.wl.clone(), sx, sy)));
                } else {
                    self.state.seat.pointer.forward_pointer(None);
                    self.state.cursor.set_normal();
                }
            }
        }
    }

    fn poll_backend(&mut self, backend_data: u32) -> io::Result<()> {
        self.state.backend.poll(backend_data)?;
        while let Some(event) = self.state.backend.next_event() {
            match event {
                BackendEvent::ShutDown => return Err(io::Error::other("backend shutdown")),
                BackendEvent::Frame => {
                    debug!(self.state.debugger, "got a frame event!");
                    let t = std::time::Instant::now();
                    self.state.backend.render_frame(&mut |frame| {
                        frame.clear(0.2, 0.1, 0.2);
                        for (toplevel_i, toplevel) in
                            self.state.focus_stack.inner().iter().enumerate()
                        {
                            let toplevel = toplevel.upgrade().unwrap();
                            let xdg_surface = toplevel.xdg_surface.upgrade().unwrap();
                            let alpha = if toplevel_i == self.state.focus_stack.inner().len() - 1 {
                                1.0
                            } else {
                                0.8
                            };
                            if let Some(geom) = xdg_surface.get_window_geometry() {
                                let border_color =
                                    if toplevel_i == self.state.focus_stack.inner().len() - 1 {
                                        Color::from_rgba(1.0, 0.0, 0.0, 1.0)
                                    } else {
                                        Color::from_rgba(0.2, 0.2, 0.2, 1.0) * alpha
                                    };
                                frame.render_rect(
                                    border_color,
                                    pixman::Rectangle32 {
                                        x: toplevel.x.get() - 2,
                                        y: toplevel.y.get() - 2,
                                        width: 2,
                                        height: geom.height.get() + 4,
                                    },
                                );
                                frame.render_rect(
                                    border_color,
                                    pixman::Rectangle32 {
                                        x: toplevel.x.get() + geom.width.get() as i32,
                                        y: toplevel.y.get() - 2,
                                        width: 2,
                                        height: geom.height.get() + 4,
                                    },
                                );
                                frame.render_rect(
                                    border_color,
                                    pixman::Rectangle32 {
                                        x: toplevel.x.get(),
                                        y: toplevel.y.get() - 2,
                                        width: geom.width.get(),
                                        height: 2,
                                    },
                                );
                                frame.render_rect(
                                    border_color,
                                    pixman::Rectangle32 {
                                        x: toplevel.x.get(),
                                        y: toplevel.y.get() + geom.height.get() as i32,
                                        width: geom.width.get(),
                                        height: 2,
                                    },
                                );
                                render_surface(
                                    frame,
                                    &xdg_surface.wl_surface.upgrade().unwrap(),
                                    alpha,
                                    toplevel.x.get() - geom.x,
                                    toplevel.y.get() - geom.y,
                                );
                            }
                        }
                        if let Some((buf_id, hx, hy)) = self.state.cursor.get_buffer() {
                            frame.render_buffer(
                                buf_id,
                                None,
                                1.0,
                                self.state.seat.pointer.x.round() as i32 - hx,
                                self.state.seat.pointer.y.round() as i32 - hy,
                            );
                        }
                    });
                    self.state.debugger.frame(t.elapsed());
                }
                BackendEvent::NewKeyboard(_id) => (),
                BackendEvent::KeyboardRemoved(_id) => (),
                BackendEvent::KeyPressed(_id, key) => {
                    let keysym = self
                        .state
                        .seat
                        .keyboard
                        .xkb_state
                        .key_get_one_sym(xkb::Keycode::new(key + 8));
                    if self.state.seat.keyboard.get_mods().logo && keysym == xkb::Keysym::Escape {
                        return Err(io::Error::other("quit"));
                    } else if keysym >= xkb::Keysym::XF86_Switch_VT_1
                        && keysym <= xkb::Keysym::XF86_Switch_VT_12
                    {
                        self.state
                            .backend
                            .switch_vt(keysym.raw() - xkb::Keysym::XF86_Switch_VT_1.raw() + 1);
                    } else {
                        if let Some(toplevel) = self.state.focus_stack.top() {
                            self.state.seat.kbd_focus_surface(Some(
                                toplevel.wl_surface.upgrade().unwrap().wl.clone(),
                            ));
                        }
                        self.state.seat.keyboard.update_key(key, true);
                    }
                }
                BackendEvent::KeyReleased(_id, key) => {
                    if let Some(toplevel) = self.state.focus_stack.top() {
                        self.state.seat.kbd_focus_surface(Some(
                            toplevel.wl_surface.upgrade().unwrap().wl.clone(),
                        ));
                    }
                    self.state.seat.keyboard.update_key(key, false);
                }
                BackendEvent::NewPointer(_id) => (),
                BackendEvent::PointerMotionAbsolute(_id, x, y) => {
                    self.state.seat.pointer.x = x;
                    self.state.seat.pointer.y = y;
                    self.pointer_moved();
                }
                BackendEvent::PointerMotionRelative(_id, dx, dy) => {
                    self.state.seat.pointer.x += dx;
                    self.state.seat.pointer.y += dy;
                    self.pointer_moved();
                }
                BackendEvent::PointerBtnPress(_id, btn) => {
                    if self.state.seat.keyboard.get_mods().alt && btn == BTN_LEFT {
                        self.state
                            .seat
                            .pointer
                            .start_move(&mut self.state.focus_stack, None);
                    } else if self.state.seat.keyboard.get_mods().alt && btn == BTN_RIGHT {
                        self.state.seat.pointer.start_resize(
                            &mut self.state.focus_stack,
                            xdg_toplevel::ResizeEdge::BottomRight,
                            None,
                        );
                    } else {
                        self.state.seat.pointer.forward_btn(btn, true);
                    }
                }
                BackendEvent::PointerBtnRelease(_id, btn) => match &self.state.seat.pointer.state {
                    PtrState::Moving { .. } | PtrState::Resizing { .. } => {
                        self.state.seat.pointer.state = PtrState::None;
                    }
                    _ => {
                        self.state.seat.pointer.forward_btn(btn, false);
                    }
                },
                BackendEvent::PointerAxisVertial(_id, value) => {
                    self.state.seat.pointer.axis_vertical(value);
                }
                BackendEvent::PointerRemoved(_id) => (),
            }
        }
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0, 0];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    assert_ne!(fds[0], -1);
    assert_ne!(fds[1], -1);
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn select_socket_name(xdg_runtime: &Path) -> Option<(String, PathBuf)> {
    for num in 0..10 {
        let socket_name = format!("wayland-{num}");
        let path = xdg_runtime.join(&socket_name);
        if !path.exists() {
            return Some((socket_name, path));
        }
    }
    None
}

fn main() {
    let xdg_runtime: PathBuf = env::var_os("XDG_RUNTIME_DIR")
        .expect("no XDG_RUNTIME_DIR variable")
        .into();
    let (socket_name, socket_path) =
        select_socket_name(&xdg_runtime).expect("could not select socket");

    let (quit_read, quit_write) = pipe().unwrap();
    signal_hook::low_level::pipe::register(signal_hook::consts::SIGTERM, quit_write.as_raw_fd())
        .unwrap();
    signal_hook::low_level::pipe::register(signal_hook::consts::SIGINT, quit_write.as_raw_fd())
        .unwrap();

    let mut server = Server::new(socket_path);
    server
        .event_loop
        .add_fd(quit_read.as_raw_fd(), event_loop::Event::Quit)
        .unwrap();

    println!("Running on {socket_name}");
    std::env::set_var("WAYLAND_DISPLAY", socket_name);
    std::process::Command::new("foot").spawn().unwrap();

    loop {
        match server.event_loop.poll().unwrap() {
            event_loop::Event::Socket => match server.socket.accept() {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
                Err(e) => panic!("socket error: {e}"),
                Ok((stream, _)) => {
                    eprintln!("new client");
                    let id = server.next_client_id;
                    server.next_client_id = id.next();
                    let client = Client::new(stream, id, server.to_flush_set.clone());
                    server
                        .event_loop
                        .add_fd(client.conn.as_raw_fd(), event_loop::Event::Client(id))
                        .unwrap();
                    server.clients.insert(id, client);
                }
            },
            event_loop::Event::Backend(id) => server.poll_backend(id).unwrap(),
            event_loop::Event::Quit => break,
            event_loop::Event::Client(client_id) => {
                let client = server.clients.get_mut(&client_id).unwrap();
                // print_client_surface_tree(client);
                if let Err(e) = client.poll(&mut server.state) {
                    eprintln!("client error: {e}");
                    server.destroy_client(client_id);
                }
            }
            event_loop::Event::MayGoIdle => {
                for (i, toplevel) in server.state.focus_stack.inner().iter().enumerate() {
                    let toplevel = toplevel.upgrade().unwrap();
                    toplevel.set_activated(i == server.state.focus_stack.inner().len() - 1);
                    toplevel.apply_pending_configure();
                }

                for client_id in server.to_flush_set.clone().0.borrow_mut().drain() {
                    if let Some(client) = server.clients.get(&client_id) {
                        if let Err(e) = client.conn.flush() {
                            eprintln!("client error: {e}");
                            server.destroy_client(client_id);
                        }
                    }
                }
            }
        }
    }
}

#[allow(dead_code)]
fn print_client_surface_tree(client: &Client) {
    fn subtree(client: &Client, indent: usize, root: Option<&Surface>) {
        match root {
            Some(root) => {
                for sub in &root.cur.borrow().subsurfaces {
                    eprint!(
                        "{} {:?}/{:?}",
                        " ".repeat(indent),
                        sub.surface.wl,
                        sub.surface.get_subsurface().unwrap().wl,
                    );
                    match sub.surface.cur.borrow().buffer {
                        Some((_buffer, w, h)) => {
                            eprintln!(" {},{} {w}x{h}", sub.position.0, sub.position.1,);
                            subtree(client, indent + 4, Some(&sub.surface));
                        }
                        None => {
                            eprintln!(" {},{} <not mapped>", sub.position.0, sub.position.1);
                        }
                    }
                }
            }
            None => {
                for s in client.compositor.surfaces.values() {
                    let role = match &*s.role.borrow() {
                        globals::compositor::SurfaceRole::None => "no role",
                        globals::compositor::SurfaceRole::Cursor => "cursor",
                        globals::compositor::SurfaceRole::Subsurface(_) => continue,
                        globals::compositor::SurfaceRole::Xdg(_) => "xdg",
                    };
                    eprint!("{}{:?} ({role})", " ".repeat(indent), s.wl);
                    match s.cur.borrow().buffer {
                        Some((_buffer, w, h)) => {
                            eprintln!(" {w}x{h}");
                            subtree(client, indent + 4, Some(s));
                        }
                        None => eprintln!(" <not mapped>"),
                    }
                }
            }
        }
    }
    eprintln!("<-- client surface tree");
    subtree(client, 2, None);
    eprintln!("    client surface tree -->");
}

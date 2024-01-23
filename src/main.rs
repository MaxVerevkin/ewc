#![allow(unreachable_code, clippy::new_without_default, incomplete_features)]
#![feature(inline_const_pat, pointer_is_aligned)]

use std::cell::RefCell;
use std::env;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::rc::Rc;
use std::rc::Weak;

use globals::compositor::Surface;
use globals::seat::BTN_LEFT;
use globals::xdg_shell::XdgToplevelRole;
use hashbrown::HashMap;
use hashbrown::HashSet;
use xkbcommon::xkb;

mod backend;
mod client;
mod event_loop;
mod globals;
mod protocol;
mod wayland_core;

use crate::backend::{Backend, BackendEvent, Frame};
use crate::client::{Client, ClientId};
use crate::event_loop::EventLoop;
use crate::globals::compositor::Compositor;
use crate::globals::seat::Seat;
use crate::globals::Global;
use crate::protocol::*;
use crate::wayland_core::*;

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
    pub globals: Vec<Global>,
    pub backend: Box<dyn Backend>,
    pub seat: Seat,
    pub cursor: Option<(Rc<Surface>, i32, i32)>,
    pub focus_stack: Vec<Weak<XdgToplevelRole>>,
    pub moving_toplevel: Option<(Weak<XdgToplevelRole>, f32, f32, i32, i32)>,
}

#[derive(Default, Clone)]
pub struct ToFlushSet(RefCell<HashSet<ClientId>>);

impl ToFlushSet {
    pub fn add(&self, client_id: ClientId) {
        self.0.borrow_mut().insert(client_id);
    }
}

impl Server {
    pub fn destroy_client(&mut self, client_id: ClientId) {
        eprintln!("destroying client");
        self.state
            .focus_stack
            .retain(|s| s.upgrade().unwrap().wl.client_id() != client_id);
        let client = self.clients.remove(&client_id).unwrap();
        client.shm.destroy(&mut self.state);
        self.event_loop.remove(client.conn.as_raw_fd()).unwrap();
    }

    pub fn toplevel_at(&self, x: i32, y: i32) -> Option<usize> {
        for (toplevel_i, toplevel) in self.state.focus_stack.iter().enumerate().rev() {
            let tl = toplevel.upgrade().unwrap();
            let xdg = tl.xdg_surface.upgrade().unwrap();
            let Some(geom) = xdg.get_window_geometry() else { continue };
            let tlx = x - tl.x.get();
            let tly = y - tl.y.get();
            if tlx >= 0
                && tly >= 0
                && tlx < geom.width.get() as i32
                && tly < geom.height.get() as i32
            {
                return Some(toplevel_i);
            }
        }
        None
    }

    pub fn surface_at(&self, x: i32, y: i32) -> Option<(usize, Rc<Surface>, f32, f32)> {
        fn surface_at(surf: Rc<Surface>, x: i32, y: i32) -> Option<(Rc<Surface>, i32, i32)> {
            for subs in surf.cur.borrow().subsurfaces.iter().rev() {
                if let Some(res) = surface_at(
                    subs.surface.clone(),
                    x - subs.position.0,
                    y - subs.position.1,
                ) {
                    return Some(res);
                }
            }
            let (_, w, h) = surf.cur.borrow().buffer?;
            let ok = x >= 0
                && y >= 0
                && x < w as i32
                && y < h as i32
                && surf
                    .cur
                    .borrow()
                    .input_region
                    .as_ref()
                    .map_or(true, |reg| reg.contains_point(x, y).is_some());
            ok.then_some((surf, x, y))
        }
        for (toplevel_i, toplevel) in self.state.focus_stack.iter().enumerate().rev() {
            let tl = toplevel.upgrade().unwrap();
            let xdg = tl.xdg_surface.upgrade().unwrap();
            let Some(geom) = xdg.get_window_geometry() else { continue };
            let tlx = x - tl.x.get();
            let tly = y - tl.y.get();
            if !(tlx >= 0
                && tly >= 0
                && tlx < geom.width.get() as i32
                && tly < geom.height.get() as i32)
            {
                continue;
            }
            if let Some((surf, sx, sy)) =
                surface_at(tl.wl_surface.upgrade().unwrap(), tlx + geom.x, tly + geom.y)
            {
                return Some((toplevel_i, surf, sx as f32, sy as f32));
            }
        }
        None
    }

    pub fn new(socket_path: PathBuf) -> Self {
        let backend = backend::wayland::new();
        let socket = UnixListener::bind(&socket_path).unwrap();
        socket.set_nonblocking(true).unwrap();
        let mut event_loop = EventLoop::new().unwrap();
        event_loop
            .add_fd(socket.as_raw_fd(), event_loop::Event::Socket)
            .unwrap();
        event_loop
            .add_fd(backend.get_fd(), event_loop::Event::Backend)
            .unwrap();
        Self {
            socket,
            socket_path,
            to_flush_set: Rc::new(ToFlushSet::default()),
            clients: HashMap::new(),
            next_client_id: ClientId::first(),
            event_loop,
            state: State {
                globals: vec![
                    Compositor::global(1),
                    Global::new::<WlSubcompositor>(2, 1),
                    Global::new::<WlShm>(3, 1),
                    Global::new::<XdgWmBase>(4, 3),
                    Global::new::<WlDataDeviceManager>(5, 3),
                    Seat::global(6),
                    Global::new::<WlOutput>(7, 2),
                ],
                backend,
                seat: Seat::new(),
                cursor: None,
                focus_stack: Vec::new(),
                moving_toplevel: None,
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
    fn poll_backend(&mut self) -> io::Result<()> {
        self.state.backend.poll()?;
        while let Some(event) = self.state.backend.next_event() {
            match event {
                BackendEvent::ShutDown => return Err(io::Error::other("backend shutdown")),
                BackendEvent::Frame => {
                    let t = std::time::Instant::now();
                    self.state.backend.render_frame(&mut |frame| {
                        frame.clear(0.2, 0.1, 0.2);
                        for (toplevel_i, toplevel) in self.state.focus_stack.iter().enumerate() {
                            let toplevel = toplevel.upgrade().unwrap();
                            let xdg_surface = toplevel.xdg_surface.upgrade().unwrap();
                            let alpha = if toplevel_i == self.state.focus_stack.len() - 1 {
                                1.0
                            } else {
                                0.8
                            };
                            if let Some(geom) = xdg_surface.get_window_geometry() {
                                render_surface(
                                    frame,
                                    &xdg_surface.wl_surface.upgrade().unwrap(),
                                    alpha,
                                    toplevel.x.get() - geom.x,
                                    toplevel.y.get() - geom.y,
                                );
                            }
                        }
                        match &self.state.cursor {
                            Some((surf, hx, hy)) => {
                                if let Some((buf, _, _)) = surf.cur.borrow().buffer {
                                    frame.render_buffer(
                                        buf,
                                        surf.cur.borrow().opaque_region.as_ref(),
                                        1.0,
                                        self.state.seat.pointer_x.round() as i32 - hx,
                                        self.state.seat.pointer_y.round() as i32 - hy,
                                    );
                                }
                            }
                            None => {
                                frame.render_rect(
                                    0.5,
                                    0.5,
                                    0.5,
                                    1.0,
                                    self.state.seat.pointer_x.round() as i32,
                                    self.state.seat.pointer_y.round() as i32,
                                    10,
                                    10,
                                );
                            }
                        }
                    });
                    dbg!(t.elapsed());
                }
                BackendEvent::NewKeyboard(_id) => (),
                BackendEvent::KeyboardRemoved(_id) => (),
                BackendEvent::KeyPressed(_id, key) => {
                    if self.state.seat.get_mods().logo
                        && self
                            .state
                            .seat
                            .xkb_state
                            .key_get_one_sym(xkb::Keycode::new(key + 8))
                            == xkb::Keysym::Escape
                    {
                        return Err(io::Error::other("quit"));
                    } else {
                        if let Some(toplevel) = self.state.focus_stack.last() {
                            let toplevel = toplevel.upgrade().unwrap();
                            self.state.seat.kbd_focus_surface(Some(
                                toplevel.wl_surface.upgrade().unwrap().wl.clone(),
                            ));
                        }
                        self.state.seat.update_key(key, true);
                    }
                }
                BackendEvent::KeyReleased(_id, key) => {
                    if let Some(toplevel) = self.state.focus_stack.last() {
                        let toplevel = toplevel.upgrade().unwrap();
                        self.state.seat.kbd_focus_surface(Some(
                            toplevel.wl_surface.upgrade().unwrap().wl.clone(),
                        ));
                    }
                    self.state.seat.update_key(key, false);
                }
                BackendEvent::NewPointer(_id) => (),
                BackendEvent::PointerMotion(_id, x, y) => {
                    self.state.seat.pointer_x = x;
                    self.state.seat.pointer_y = y;
                    if let Some((toplevel, px, py, tx, ty)) = &self.state.moving_toplevel {
                        let toplevel = toplevel.upgrade().unwrap();
                        toplevel.x.set(tx + (x - px).round() as i32);
                        toplevel.y.set(ty + (y - py).round() as i32);
                    } else if let Some((_i, surf, sx, sy)) =
                        self.surface_at(x.round() as i32, y.round() as i32)
                    {
                        self.state
                            .seat
                            .ptr_forward_pointer(Some((surf.wl.clone(), sx, sy)));
                    } else {
                        self.state.seat.ptr_forward_pointer(None);
                        self.state.cursor = None;
                    }
                }
                BackendEvent::PointerBtnPress(_id, btn) => {
                    let x = self.state.seat.pointer_x.round() as i32;
                    let y = self.state.seat.pointer_y.round() as i32;
                    if self.state.seat.get_mods().alt && btn == BTN_LEFT {
                        if let Some(toplevel_i) = self.toplevel_at(x, y) {
                            let toplevel = self.state.focus_stack[toplevel_i].upgrade().unwrap();
                            self.state.moving_toplevel = Some((
                                Rc::downgrade(&toplevel),
                                self.state.seat.pointer_x,
                                self.state.seat.pointer_y,
                                toplevel.x.get(),
                                toplevel.y.get(),
                            ));
                            let tl = self.state.focus_stack.remove(toplevel_i);
                            self.state.focus_stack.push(tl);
                        }
                    } else {
                        self.state.seat.ptr_forward_btn(btn, true);
                    }
                }
                BackendEvent::PointerBtnRelease(_id, btn) => {
                    self.state.moving_toplevel = None;
                    self.state.seat.ptr_forward_btn(btn, false);
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

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0, 0];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    assert_ne!(fds[0], -1);
    assert_ne!(fds[1], -1);
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn main() {
    let socket_number = std::env::args()
        .nth(1)
        .map(|arg| dbg!(arg).parse::<u32>().unwrap())
        .unwrap_or(10);

    let (quit_read, quit_write) = pipe().unwrap();
    signal_hook::low_level::pipe::register(signal_hook::consts::SIGTERM, quit_write.as_raw_fd())
        .unwrap();
    signal_hook::low_level::pipe::register(signal_hook::consts::SIGINT, quit_write.as_raw_fd())
        .unwrap();

    let mut socket_path: PathBuf = env::var_os("XDG_RUNTIME_DIR").unwrap().into();
    socket_path.push(format!("wayland-{socket_number}"));
    println!("Running on {}", socket_path.display());

    let mut server = Server::new(socket_path);
    server
        .event_loop
        .add_fd(quit_read.as_raw_fd(), event_loop::Event::Quit)
        .unwrap();

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
            event_loop::Event::Backend => server.poll_backend().unwrap(),
            event_loop::Event::Quit => break,
            event_loop::Event::Client(client_id) => {
                let client = server.clients.get_mut(&client_id).unwrap();
                print_client_surface_tree(client);
                if let Err(e) = client.poll(&mut server.state) {
                    eprintln!("client error: {e}");
                    server.destroy_client(client_id);
                }
            }
            event_loop::Event::MayGoIdle => {
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
    fn subtree(client: &Client, indent: usize, root: Option<ObjectId>) {
        match root {
            Some(root) => {
                for sub in &client
                    .compositor
                    .surfaces
                    .get(&root)
                    .unwrap()
                    .cur
                    .borrow()
                    .subsurfaces
                {
                    eprint!(
                        "{} {:?}/{:?}",
                        " ".repeat(indent),
                        sub.surface.wl,
                        sub.surface.get_subsurface().unwrap().wl,
                    );
                    match sub.surface.cur.borrow().buffer {
                        Some((_buffer, w, h)) => {
                            eprintln!(" {},{} {w}x{h}", sub.position.0, sub.position.1,);
                            subtree(client, indent + 4, Some(sub.surface.wl.id()));
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
                            subtree(client, indent + 4, Some(s.wl.id()));
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

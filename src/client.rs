use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::num::NonZeroU64;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::rc::Rc;

use crate::globals::compositor::Compositor;
use crate::globals::linux_dmabuf::LinuxDmabuf;
use crate::globals::shm::Shm;
use crate::globals::single_pixel_buffer::SinglePixelBufferManager;
use crate::protocol::*;
use crate::seat::{ClientSeat, DataSource};
use crate::wayland_core::*;
use crate::{State, ToFlushSet};

use wayrs_core::MessageBuffersPool;
use wayrs_core::transport::{PeekHeaderError, RecvMessageError};

pub struct Connection {
    client_id: ClientId,
    to_flush_set: Rc<ToFlushSet>,
    socket: RefCell<BufferedSocket<UnixStream>>,
    msg_buf_pool: RefCell<MessageBuffersPool>,
    events_queue: RefCell<VecDeque<Message>>,
    resources: RefCell<ObjectStorage>,
    wl_display: WlDisplay,
    pub seat: ClientSeat,
}

impl AsRawFd for Connection {
    fn as_raw_fd(&self) -> RawFd {
        self.socket.borrow().as_raw_fd()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientId(NonZeroU64);

impl ClientId {
    pub fn first() -> Self {
        Self(NonZeroU64::MIN)
    }

    pub fn next(self) -> Self {
        Self(self.0.checked_add(1).unwrap())
    }
}

impl Connection {
    fn new(stream: UnixStream, client_id: ClientId, to_flush_set: Rc<ToFlushSet>) -> Rc<Self> {
        Rc::new_cyclic(|conn| {
            let (wl_display, resources) = ObjectStorage::new(conn.clone());
            Self {
                client_id,
                to_flush_set,
                socket: RefCell::new(BufferedSocket::from(stream)),
                msg_buf_pool: RefCell::new(MessageBuffersPool::default()),
                events_queue: RefCell::new(VecDeque::new()),
                resources: RefCell::new(resources),
                wl_display,
                seat: ClientSeat::default(),
            }
        })
    }

    pub fn client_id(&self) -> ClientId {
        self.client_id
    }

    pub fn flush(&self) -> io::Result<()> {
        let mut eq = self.events_queue.borrow_mut();
        let mut socket = self.socket.borrow_mut();
        let mut msg_buf_pool = self.msg_buf_pool.borrow_mut();
        while let Some(msg) = eq.pop_front() {
            if let Err(e) = socket.write_message(msg, &mut msg_buf_pool, IoMode::Blocking) {
                eq.push_front(e.msg);
                return Err(e.err);
            }
        }
        socket.flush(IoMode::Blocking)
    }

    pub fn send_event(&self, msg: Message) {
        self.events_queue.borrow_mut().push_back(msg);
        self.to_flush_set.add(self.client_id);
    }

    pub fn register_clients_object(&self, object: Object) -> io::Result<()> {
        self.resources.borrow_mut().register_clients(object)
    }

    pub fn create_servers_object<P: Proxy>(self: &Rc<Self>, version: u32) -> io::Result<P> {
        self.resources
            .borrow_mut()
            .create_servers(self, P::INTERFACE, version)
            .map(|x| x.try_into().unwrap())
    }

    pub fn reuse_id(&self, id: ObjectId) {
        if id.created_by_client() {
            self.wl_display.delete_id(id.as_u32());
        } else {
            self.resources.borrow_mut().reuse_servers_id(id);
        }
    }

    pub fn get_object(&self, id: ObjectId) -> Option<Object> {
        self.resources.borrow().get(id)
    }

    fn recv_request(self: &Rc<Self>) -> io::Result<(Message, Object)> {
        let mut socket = self.socket.borrow_mut();
        let mut msg_buf_pool = self.msg_buf_pool.borrow_mut();
        let header = socket
            .peek_message_header(IoMode::NonBlocking)
            .map_err(|err| match err {
                PeekHeaderError::Io(io) => io,
                other => io::Error::new(io::ErrorKind::InvalidData, other),
            })?;
        let object = self
            .get_object(header.object_id)
            .ok_or_else(|| io::Error::other("request for unknown object"))?;
        let signature = object
            .interface()
            .requests
            .get(header.opcode as usize)
            .ok_or_else(|| io::Error::other("invalid request opcode"))?
            .signature;
        let msg = socket
            .recv_message(header, signature, &mut msg_buf_pool, IoMode::NonBlocking)
            .map_err(|err| match err {
                RecvMessageError::Io(io) => io,
                other => io::Error::new(io::ErrorKind::InvalidData, other),
            })?;
        for (arg_i, arg) in msg.args.iter().enumerate() {
            if let &ArgValue::NewId(id) = arg {
                let ArgType::NewId(iface) = signature[arg_i] else { unreachable!() };
                self.register_clients_object(Object::new(self, id, iface, object.version()))?;
            }
        }
        Ok((msg, object))
    }
}

pub type ResourceCallback = Box<dyn Fn(&mut Client, &mut State, Object, Message) -> io::Result<()>>;

pub struct RequestCtx<'a, P: Proxy> {
    pub client: &'a mut Client,
    pub state: &'a mut State,
    pub proxy: P,
    pub request: P::Request,
}

pub struct Client {
    pub conn: Rc<Connection>,
    pub compositor: Compositor,
    pub shm: Shm,
    pub data_sources: HashMap<WlDataSource, DataSource>,
    pub linux_dambuf: LinuxDmabuf,
    pub single_pixel_buffer_manager: SinglePixelBufferManager,
}

impl Client {
    pub fn new(stream: UnixStream, id: ClientId, to_flush_set: Rc<ToFlushSet>) -> Self {
        let conn = Connection::new(stream, id, to_flush_set);
        conn.wl_display.set_callback(wl_display_cb);
        Self {
            conn,
            compositor: Compositor::default(),
            shm: Shm::default(),
            data_sources: HashMap::new(),
            linux_dambuf: LinuxDmabuf::default(),
            single_pixel_buffer_manager: SinglePixelBufferManager::default(),
        }
    }

    pub fn poll(&mut self, state: &mut State) -> io::Result<()> {
        loop {
            let (msg, object) = match self.conn.recv_request() {
                Ok((msg, object)) => (msg, object),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            };

            if object.state() == ObjectState::Dead {
                continue;
            }

            let is_destructor =
                object.interface().requests[msg.header.opcode as usize].is_destructor;

            object.exec_callback(self, state, msg)?;

            if is_destructor {
                object.destroy();
            }
        }
    }
}

fn wl_display_cb(ctx: RequestCtx<WlDisplay>) -> io::Result<()> {
    use wl_display::Request;
    match ctx.request {
        Request::Sync(cb) => cb.done(0), // WTF is this "event serial"?
        Request::GetRegistry(registry) => ctx.state.globals.add_registry(registry),
    }
    Ok(())
}

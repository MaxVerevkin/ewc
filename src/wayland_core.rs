use std::cell::Cell;
use std::cmp;
use std::ffi::{CStr, CString};
use std::fmt::{self, Debug, Formatter};
use std::hash::{Hash, Hasher};
use std::io::{self, IoSlice, IoSliceMut};
use std::num::NonZeroU32;
use std::os::fd::OwnedFd;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::rc::{Rc, Weak};

use nix::sys::socket::{self, ControlMessage, ControlMessageOwned};

use buf::{ArrayBuffer, RingBuffer};

use crate::client::{Client, ClientId, Connection, RequestCtx, ResourceCallback};
use crate::{protocol::*, State};

#[derive(Debug)]
pub struct BadMessage;

#[derive(Debug)]
pub struct WrongObject;

impl From<BadMessage> for io::Error {
    fn from(_: BadMessage) -> Self {
        io::Error::other("failed to parse message")
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MessageHeader {
    pub object_id: ObjectId,
    pub size: u16,
    pub opcode: u16,
}

impl MessageHeader {
    pub const fn size() -> u16 {
        8
    }
}

#[derive(Debug)]
pub struct Message {
    pub header: MessageHeader,
    pub args: Vec<ArgValue>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ArgType {
    Int,
    Uint,
    Fixed,

    Object,
    OptObject,
    NewId(&'static Interface),
    AnyNewId,

    String,
    OptString,
    Array,
    Fd,
}

#[derive(Debug)]
pub enum ArgValue {
    Int(i32),
    Uint(u32),
    Fixed(Fixed),

    Object(ObjectId),
    OptObject(Option<ObjectId>),
    NewId(ObjectId),
    AnyNewId(Object),

    String(CString),
    OptString(Option<CString>),
    Array(Vec<u8>),
    Fd(OwnedFd),
}

impl ArgValue {
    pub fn size(&self) -> u16 {
        fn len_with_padding(len: usize) -> u16 {
            let padding = (4 - (len % 4)) % 4;
            (4 + len + padding) as u16
        }

        match self {
            Self::Int(_)
            | Self::Uint(_)
            | Self::Fixed(_)
            | Self::Object(_)
            | Self::OptObject(_)
            | Self::NewId(_)
            | Self::OptString(None) => 4,
            Self::AnyNewId(object) => {
                len_with_padding(object.inner.interface.name.to_bytes_with_nul().len()) + 8
            }
            Self::String(string) | Self::OptString(Some(string)) => {
                len_with_padding(string.to_bytes_with_nul().len())
            }
            Self::Array(array) => len_with_padding(array.len()),
            Self::Fd(_) => 0,
        }
    }
}

/// Signed 24.8 decimal number
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Fixed(pub i32);

impl From<i32> for Fixed {
    fn from(value: i32) -> Self {
        Self(value * 256)
    }
}

impl From<u32> for Fixed {
    fn from(value: u32) -> Self {
        Self(value as i32 * 256)
    }
}

impl From<f32> for Fixed {
    fn from(value: f32) -> Self {
        Self((value * 256.0).round() as i32)
    }
}

impl Fixed {
    pub fn as_f64(self) -> f64 {
        self.0 as f64 / 256.0
    }

    pub fn as_f32(self) -> f32 {
        self.0 as f32 / 256.0
    }

    pub fn as_int(self) -> i32 {
        self.0 / 256
    }
}

impl Debug for Fixed {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.as_f64().fmt(f)
    }
}

pub trait Proxy: Clone + TryFrom<Object, Error = WrongObject> {
    type Request;

    const INTERFACE: &'static Interface;

    fn as_object(&self) -> &Object;

    fn parse_request(conn: &Rc<Connection>, msg: Message) -> Result<Self::Request, BadMessage>;

    fn id(&self) -> ObjectId {
        self.as_object().id()
    }

    fn client_id(&self) -> ClientId {
        self.as_object().client_id()
    }

    fn version(&self) -> u32 {
        self.as_object().version()
    }

    fn set_callback<F>(&self, cb: F)
    where
        F: Fn(RequestCtx<Self>) -> io::Result<()> + 'static,
    {
        self.as_object()
            .set_callback(Box::new(move |client, state, obj, msg| {
                let proxy: Self = obj.try_into().unwrap();
                let request = Self::parse_request(&client.conn, msg)?;
                cb(crate::client::RequestCtx {
                    client,
                    state,
                    proxy,
                    request,
                })
            }));
    }

    fn conn(&self) -> Rc<Connection> {
        self.as_object().conn()
    }

    fn is_alive(&self) -> bool {
        self.as_object().state() == ObjectState::Alive
    }
}

impl<P: Proxy> From<P> for ObjectId {
    fn from(value: P) -> Self {
        value.id()
    }
}

pub struct Interface {
    pub name: &'static CStr,
    pub version: u32,
    pub events: &'static [MessageDesc],
    pub requests: &'static [MessageDesc],
}

#[derive(Debug, Clone, Copy)]
pub struct MessageDesc {
    pub name: &'static str,
    pub is_destructor: bool,
    pub signature: &'static [ArgType],
}

impl PartialEq for &'static Interface {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for &'static Interface {}

impl Hash for &'static Interface {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl Debug for Interface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Interface").field(&self.name).finish()
    }
}

#[derive(Clone)]
pub struct Object {
    inner: Rc<ObjectInner>,
}

struct ObjectInner {
    id: ObjectId,
    interface: &'static Interface,
    version: u32,
    conn: Weak<Connection>,
    state: Cell<ObjectState>,
    callback: Cell<Option<ResourceCallback>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectState {
    Alive,
    Dead,
}

impl Object {
    fn make_display(conn: Weak<Connection>) -> Self {
        Self {
            inner: Rc::new(ObjectInner {
                id: ObjectId::DISPLAY,
                interface: WlDisplay::INTERFACE,
                version: 1,
                conn,
                state: Cell::new(ObjectState::Alive),
                callback: Cell::new(None),
            }),
        }
    }

    pub fn new(
        conn: &Rc<Connection>,
        id: ObjectId,
        interface: &'static Interface,
        version: u32,
    ) -> Self {
        Self {
            inner: Rc::new(ObjectInner {
                id,
                interface,
                version,
                conn: Rc::downgrade(conn),
                state: Cell::new(ObjectState::Alive),
                callback: Cell::new(None),
            }),
        }
    }

    pub fn id(&self) -> ObjectId {
        self.inner.id
    }

    pub fn client_id(&self) -> ClientId {
        self.inner.conn.upgrade().unwrap().client_id()
    }

    pub fn version(&self) -> u32 {
        self.inner.version
    }

    pub fn interface(&self) -> &'static Interface {
        self.inner.interface
    }

    pub fn conn(&self) -> Rc<Connection> {
        self.inner.conn.upgrade().unwrap()
    }

    pub fn state(&self) -> ObjectState {
        if self.inner.conn.strong_count() == 0 {
            ObjectState::Dead
        } else {
            self.inner.state.get()
        }
    }

    pub fn destroy(&self) {
        self.inner.state.set(ObjectState::Dead);
        self.conn().reuse_id(self.id());
    }

    pub fn set_callback(&self, callback: ResourceCallback) {
        assert_eq!(
            self.state(),
            ObjectState::Alive,
            "tried to set callback for dead object {self:?}"
        );
        let old = self.inner.callback.replace(Some(callback));
        assert!(
            old.is_none(),
            "object callback can only be set once (object = {self:?})"
        );
    }

    pub fn exec_callback(
        &self,
        client: &mut Client,
        state: &mut State,
        message: Message,
    ) -> io::Result<()> {
        let Some(callback) = self.inner.callback.take() else {
            panic!("unhandled request for {self:?}");
        };
        let result = (callback)(client, state, self.clone(), message);
        let replaced = self.inner.callback.replace(Some(callback));
        assert!(
            replaced.is_none(),
            "object callback can only bet set onec (object = {self:?})"
        );
        result
    }
}

impl PartialEq for Object {
    fn eq(&self, other: &Self) -> bool {
        Rc::as_ptr(&self.inner) == Rc::as_ptr(&other.inner)
    }
}

impl Eq for Object {}

impl PartialOrd for Object {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Object {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        Rc::as_ptr(&self.inner).cmp(&Rc::as_ptr(&other.inner))
    }
}

impl Hash for Object {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Rc::as_ptr(&self.inner).hash(state);
    }
}

impl Debug for Object {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}@{}v{}",
            self.inner.interface.name.to_string_lossy(),
            self.inner.id.as_u32(),
            self.inner.version
        )
    }
}

pub struct ObjectStorage {
    clients: Vec<Object>,
    servers: Vec<Object>,
    free_server_ids: Vec<ObjectId>,
    next_server_id: ObjectId,
}

impl ObjectStorage {
    pub fn new(conn: Weak<Connection>) -> (WlDisplay, Self) {
        let display = Object::make_display(conn);
        (
            display.clone().try_into().unwrap(),
            Self {
                clients: vec![display],
                servers: Vec::new(),
                free_server_ids: Vec::new(),
                next_server_id: ObjectId::MIN_SERVER,
            },
        )
    }

    pub fn get(&self, id: ObjectId) -> Option<Object> {
        if id.created_by_client() {
            self.clients.get(id.as_index()).cloned()
        } else {
            self.servers.get(id.as_index()).cloned()
        }
    }

    pub fn register_clients(&mut self, object: Object) -> io::Result<()> {
        if object.id().created_by_server() {
            return Err(io::Error::other(format!(
                "client used object id {} which is reserved for server-side objects",
                object.id().as_u32()
            )));
        }

        if object.id().as_index() > self.clients.len() {
            return Err(io::Error::other(format!(
                "client used object id {} but {} was never used",
                object.id().as_u32(),
                object.id().as_u32() - 1
            )));
        }

        if let Some(slot) = self.clients.get_mut(object.id().as_index()) {
            *slot = object;
        } else if object.id().as_index() == self.clients.len() {
            self.clients.push(object);
        }

        Ok(())
    }

    pub fn create_servers(
        &mut self,
        conn: &Rc<Connection>,
        interface: &'static Interface,
        version: u32,
    ) -> io::Result<Object> {
        let id = match self.free_server_ids.pop() {
            Some(id) => id,
            None => {
                let id = self.next_server_id;
                self.next_server_id.0 = self
                    .next_server_id
                    .0
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("run out of server-side object ids"))?;
                id
            }
        };

        if id.as_index() > self.servers.len() {
            unreachable!();
        }

        let object = Object::new(conn, id, interface, version);

        if let Some(slot) = self.servers.get_mut(id.as_index()) {
            *slot = object.clone();
        } else if object.id().as_index() == self.servers.len() {
            self.servers.push(object.clone());
        }

        Ok(object)
    }

    pub fn reuse_servers_id(&mut self, id: ObjectId) {
        assert!(id.created_by_server());
        self.free_server_ids.push(id);
    }
}

/// A Wayland object ID.
///
/// Uniquely identifies an object at each point of time. Note that an ID may have a limited
/// lifetime. Also an ID which once pointed to a certain object, may point to a different object in
/// the future, due to ID reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId(pub NonZeroU32);

impl ObjectId {
    pub const DISPLAY: Self = Self(unsafe { NonZeroU32::new_unchecked(1) });
    pub const MAX_CLIENT: Self = Self(unsafe { NonZeroU32::new_unchecked(0xFEFFFFFF) });
    pub const MIN_SERVER: Self = Self(unsafe { NonZeroU32::new_unchecked(0xFF000000) });

    /// Returns the numeric representation of the ID
    pub fn as_u32(self) -> u32 {
        self.0.get()
    }

    /// Calculate the index of this object in the array of client or server objects
    pub fn as_index(self) -> usize {
        if self.created_by_client() {
            (self.0.get() - 1) as usize
        } else {
            (self.0.get() - Self::MIN_SERVER.0.get()) as usize
        }
    }

    /// Whether the object with this ID was created by the server
    pub fn created_by_server(self) -> bool {
        self >= Self::MIN_SERVER
    }

    /// Whether the object with this ID was created by the client
    pub fn created_by_client(self) -> bool {
        self <= Self::MAX_CLIENT
    }
}

pub const BYTES_OUT_LEN: usize = 4096;
pub const BYTES_IN_LEN: usize = BYTES_OUT_LEN * 2;
pub const FDS_OUT_LEN: usize = 28;
pub const FDS_IN_LEN: usize = FDS_OUT_LEN * 2;

pub struct BufferedSocket {
    socket: UnixStream,
    bytes_in: RingBuffer<BYTES_IN_LEN>,
    bytes_out: RingBuffer<BYTES_OUT_LEN>,
    fds_in: ArrayBuffer<RawFd, FDS_IN_LEN>,
    fds_out: ArrayBuffer<RawFd, FDS_OUT_LEN>,
}

/// The "mode" of an IO operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoMode {
    /// Blocking.
    ///
    /// The function call may block, but it will never return [WouldBlock](io::ErrorKind::WouldBlock)
    /// error.
    Blocking,
    /// Non-blocking.
    ///
    /// The function call will not block on IO operations. [WouldBlock](io::ErrorKind::WouldBlock)
    /// error is returned if the operation cannot be completed immediately.
    NonBlocking,
}

impl AsRawFd for BufferedSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.socket.as_raw_fd()
    }
}

pub struct SendMessageError {
    pub msg: Message,
    pub err: io::Error,
}

impl From<UnixStream> for BufferedSocket {
    fn from(socket: UnixStream) -> Self {
        Self {
            socket,
            bytes_in: RingBuffer::new(),
            bytes_out: RingBuffer::new(),
            fds_in: ArrayBuffer::new(),
            fds_out: ArrayBuffer::new(),
        }
    }
}

impl BufferedSocket {
    /// Write a single Wayland message into the intevnal buffer.
    ///
    /// Flushes the buffer if neccessary. On failure, ownership of the message is returned.
    ///
    /// # Panics
    ///
    /// This function panics if the message size is larger than `BYTES_OUT_LEN` or it contains more
    /// than `FDS_OUT_LEN` file descriptors.
    pub fn write_message(&mut self, msg: Message, mode: IoMode) -> Result<(), SendMessageError> {
        // Calc size
        let size = MessageHeader::size() + msg.args.iter().map(ArgValue::size).sum::<u16>();
        let fds_cnt = msg
            .args
            .iter()
            .filter(|arg| matches!(arg, ArgValue::Fd(_)))
            .count();

        // Check size and flush if neccessary
        assert!(size as usize <= BYTES_OUT_LEN);
        assert!(fds_cnt <= FDS_OUT_LEN);
        if (size as usize) > self.bytes_out.writable_len()
            || fds_cnt > self.fds_out.get_writable().len()
        {
            if let Err(err) = self.flush(mode) {
                return Err(SendMessageError { msg, err });
            }
        }

        // Header
        self.bytes_out.write_uint(msg.header.object_id.0.get());
        self.bytes_out
            .write_uint((size as u32) << 16 | msg.header.opcode as u32);

        // Args
        for arg in msg.args.into_iter() {
            match arg {
                ArgValue::Uint(x) => self.bytes_out.write_uint(x),
                ArgValue::Int(x) | ArgValue::Fixed(Fixed(x)) => self.bytes_out.write_int(x),
                ArgValue::Object(ObjectId(x))
                | ArgValue::OptObject(Some(ObjectId(x)))
                | ArgValue::NewId(ObjectId(x)) => self.bytes_out.write_uint(x.get()),
                ArgValue::OptObject(None) | ArgValue::OptString(None) => {
                    self.bytes_out.write_uint(0)
                }
                ArgValue::AnyNewId(_) => unimplemented!(),
                ArgValue::String(string) | ArgValue::OptString(Some(string)) => {
                    self.send_array(string.to_bytes_with_nul())
                }
                ArgValue::Array(array) => self.send_array(&array),
                ArgValue::Fd(fd) => self.fds_out.write_one(fd.into_raw_fd()),
            }
        }

        Ok(())
    }

    pub fn peek_message_header(&mut self) -> io::Result<MessageHeader> {
        while self.bytes_in.readable_len() < MessageHeader::size() as usize {
            self.fill_incoming_buf()?;
        }

        let mut raw = [0; MessageHeader::size() as usize];
        self.bytes_in.peek_bytes(&mut raw);
        let object_id = u32::from_ne_bytes(raw[0..4].try_into().unwrap());
        let size_and_opcode = u32::from_ne_bytes(raw[4..8].try_into().unwrap());

        Ok(MessageHeader {
            object_id: ObjectId(NonZeroU32::new(object_id).expect("received event for null id")),
            size: ((size_and_opcode & 0xFFFF_0000) >> 16) as u16,
            opcode: (size_and_opcode & 0x0000_FFFF) as u16,
        })
    }

    pub fn recv_message(
        &mut self,
        header: MessageHeader,
        signature: &[ArgType],
    ) -> io::Result<Message> {
        // Check size and fill buffer if necessary
        let fds_cnt = signature
            .iter()
            .filter(|arg| matches!(arg, ArgType::Fd))
            .count();
        assert!(header.size as usize <= BYTES_IN_LEN);
        assert!(fds_cnt <= FDS_IN_LEN);
        while header.size as usize > self.bytes_in.readable_len()
            || fds_cnt > self.fds_in.get_readable().len()
        {
            self.fill_incoming_buf()?;
        }

        // Consume header
        self.bytes_in.move_tail(MessageHeader::size() as usize);

        let args = signature
            .iter()
            .map(|arg_type| match arg_type {
                ArgType::Int => ArgValue::Int(self.bytes_in.read_int()),
                ArgType::Uint => ArgValue::Uint(self.bytes_in.read_uint()),
                ArgType::Fixed => ArgValue::Fixed(Fixed(self.bytes_in.read_int())),
                ArgType::Object => {
                    ArgValue::Object(self.bytes_in.read_id().expect("unexpected null object id"))
                }
                ArgType::OptObject => ArgValue::OptObject(self.bytes_in.read_id()),
                ArgType::NewId(_) => {
                    ArgValue::NewId(self.bytes_in.read_id().expect("unexpected null new_id"))
                }
                ArgType::AnyNewId => unreachable!(),
                ArgType::String => ArgValue::String(self.recv_string()),
                ArgType::OptString => ArgValue::OptString(match self.bytes_in.read_uint() {
                    0 => None,
                    len => Some(self.recv_string_with_len(len)),
                }),
                ArgType::Array => ArgValue::Array(self.recv_array()),
                ArgType::Fd => {
                    let fd = self.fds_in.read_one();
                    assert_ne!(fd, -1);
                    ArgValue::Fd(unsafe { OwnedFd::from_raw_fd(fd) })
                }
            })
            .collect();

        Ok(Message { header, args })
    }

    pub fn flush(&mut self, mode: IoMode) -> io::Result<()> {
        if self.bytes_out.is_empty() && self.fds_out.get_readable().is_empty() {
            return Ok(());
        }

        let mut flags = socket::MsgFlags::MSG_NOSIGNAL;
        if mode == IoMode::NonBlocking {
            flags |= socket::MsgFlags::MSG_DONTWAIT;
        }

        let b;
        let cmsgs: &[ControlMessage] = match self.fds_out.get_readable() {
            [] => &[],
            fds => {
                b = [ControlMessage::ScmRights(fds)];
                &b
            }
        };

        let mut iov_buf = [IoSlice::new(&[]), IoSlice::new(&[])];
        let iov = self.bytes_out.get_readable_iov(&mut iov_buf);
        let sent = socket::sendmsg::<()>(self.socket.as_raw_fd(), iov, cmsgs, flags, None)?;

        for fd in self.fds_out.get_readable() {
            let _ = nix::unistd::close(*fd);
        }

        // Does this have to be true?
        assert_eq!(sent, self.bytes_out.readable_len());

        self.bytes_out.clear();
        self.fds_out.clear();

        Ok(())
    }
}

impl BufferedSocket {
    fn fill_incoming_buf(&mut self) -> io::Result<()> {
        self.fds_in.relocate();
        if self.bytes_in.is_full() && self.fds_in.get_writable().is_empty() {
            return Ok(());
        }

        let mut cmsg = nix::cmsg_space!([RawFd; FDS_OUT_LEN]);

        let flags = socket::MsgFlags::MSG_CMSG_CLOEXEC
            | socket::MsgFlags::MSG_NOSIGNAL
            | socket::MsgFlags::MSG_DONTWAIT;

        let mut iov_buf = [IoSliceMut::new(&mut []), IoSliceMut::new(&mut [])];
        let iov = self.bytes_in.get_writeable_iov(&mut iov_buf);
        let msg = socket::recvmsg::<()>(self.socket.as_raw_fd(), iov, Some(&mut cmsg), flags)?;

        for cmsg in msg.cmsgs() {
            if let ControlMessageOwned::ScmRights(fds) = cmsg {
                self.fds_in.extend(&fds);
            }
        }

        let read = msg.bytes;

        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "server disconnected",
            ));
        }

        self.bytes_in.move_head(read);

        Ok(())
    }

    fn send_array(&mut self, array: &[u8]) {
        let len = array.len() as u32;

        self.bytes_out.write_uint(len);
        self.bytes_out.write_bytes(array);

        let padding = ((4 - (len % 4)) % 4) as usize;
        self.bytes_out.write_bytes(&[0, 0, 0][..padding]);
    }

    fn recv_array(&mut self) -> Vec<u8> {
        let len = self.bytes_in.read_uint() as usize;

        let mut buf = vec![0; len];
        self.bytes_in.read_bytes(&mut buf);

        let padding = (4 - (len % 4)) % 4;
        self.bytes_in.move_tail(padding);

        buf
    }

    fn recv_string_with_len(&mut self, len: u32) -> CString {
        let mut buf = vec![0; len as usize];
        self.bytes_in.read_bytes(&mut buf);

        let padding = (4 - (len % 4)) % 4;
        self.bytes_in.move_tail(padding as usize);

        CString::from_vec_with_nul(buf).expect("received string with internal null bytes")
    }

    fn recv_string(&mut self) -> CString {
        let len = self.bytes_in.read_uint();
        self.recv_string_with_len(len)
    }
}

mod buf {
    use super::*;

    pub struct ArrayBuffer<T, const N: usize> {
        bytes: Box<[T; N]>,
        offset: usize,
        len: usize,
    }

    impl<T: Default + Copy, const N: usize> ArrayBuffer<T, N> {
        pub fn new() -> Self {
            Self {
                bytes: Box::new([T::default(); N]),
                offset: 0,
                len: 0,
            }
        }

        pub fn clear(&mut self) {
            self.offset = 0;
            self.len = 0;
        }

        pub fn get_writable(&mut self) -> &mut [T] {
            &mut self.bytes[(self.offset + self.len)..]
        }

        pub fn get_readable(&self) -> &[T] {
            &self.bytes[self.offset..][..self.len]
        }

        pub fn consume(&mut self, cnt: usize) {
            assert!(cnt <= self.len);
            self.offset += cnt;
            self.len -= cnt;
        }

        pub fn advance(&mut self, cnt: usize) {
            assert!(self.offset + self.len + cnt <= N);
            self.len += cnt;
        }

        pub fn relocate(&mut self) {
            if self.len > 0 && self.offset > 0 {
                self.bytes
                    .copy_within(self.offset..(self.offset + self.len), 0);
            }
            self.offset = 0;
        }

        pub fn write_one(&mut self, elem: T) {
            let writable = self.get_writable();
            assert!(!writable.is_empty());
            writable[0] = elem;
            self.advance(1);
        }

        pub fn read_one(&mut self) -> T {
            let readable = self.get_readable();
            assert!(!readable.is_empty());
            let elem = readable[0];
            self.consume(1);
            elem
        }

        pub fn extend(&mut self, src: &[T]) {
            let writable = &mut self.get_writable()[..src.len()];
            writable.copy_from_slice(src);
            self.advance(src.len());
        }
    }

    pub struct RingBuffer<const N: usize> {
        bytes: Box<[u8; N]>,
        offset: usize,
        len: usize,
    }

    impl<const N: usize> RingBuffer<N> {
        pub fn new() -> Self {
            Self {
                bytes: Box::new([0; N]),
                offset: 0,
                len: 0,
            }
        }

        pub fn clear(&mut self) {
            self.offset = 0;
            self.len = 0;
        }

        pub fn move_head(&mut self, n: usize) {
            self.len += n;
        }

        pub fn move_tail(&mut self, n: usize) {
            self.offset = (self.offset + n) % N;
            self.len = self.len.checked_sub(n).unwrap();
        }

        pub fn readable_len(&self) -> usize {
            self.len
        }

        pub fn writable_len(&self) -> usize {
            N - self.len
        }

        pub fn is_empty(&self) -> bool {
            self.len == 0
        }

        pub fn is_full(&self) -> bool {
            self.len == N
        }

        fn head(&self) -> usize {
            (self.offset + self.len) % N
        }

        pub fn write_bytes(&mut self, data: &[u8]) {
            assert!(self.writable_len() >= data.len());

            let head = self.head();
            if head + data.len() <= N {
                self.bytes[head..][..data.len()].copy_from_slice(data);
            } else {
                let size = N - head;
                let rest = data.len() - size;
                self.bytes[head..][..size].copy_from_slice(&data[..size]);
                self.bytes[..rest].copy_from_slice(&data[size..]);
            }

            self.move_head(data.len());
        }

        pub fn peek_bytes(&mut self, buf: &mut [u8]) {
            assert!(self.readable_len() >= buf.len());

            if self.offset + buf.len() <= N {
                buf.copy_from_slice(&self.bytes[self.offset..][..buf.len()]);
            } else {
                let size = N - self.offset;
                let rest = buf.len() - size;
                buf[..size].copy_from_slice(&self.bytes[self.offset..][..size]);
                buf[size..].copy_from_slice(&self.bytes[..rest]);
            }
        }

        pub fn read_bytes(&mut self, buf: &mut [u8]) {
            self.peek_bytes(buf);
            self.move_tail(buf.len());
        }

        pub fn get_writeable_iov<'b, 'a: 'b>(
            &'a mut self,
            iov_buf: &'b mut [IoSliceMut<'a>; 2],
        ) -> &'b mut [IoSliceMut<'a>] {
            let head = self.head();
            if self.len == 0 {
                self.offset = 0;
                iov_buf[0] = IoSliceMut::new(&mut *self.bytes);
                &mut iov_buf[0..1]
            } else if head < self.offset {
                iov_buf[0] = IoSliceMut::new(&mut self.bytes[head..self.offset]);
                &mut iov_buf[0..1]
            } else if self.offset == 0 {
                iov_buf[0] = IoSliceMut::new(&mut self.bytes[head..N]);
                &mut iov_buf[0..1]
            } else {
                let (left, right) = self.bytes.split_at_mut(head);
                iov_buf[0] = IoSliceMut::new(right);
                iov_buf[1] = IoSliceMut::new(&mut left[..self.offset]);
                &mut iov_buf[0..2]
            }
        }

        pub fn get_readable_iov<'b, 'a: 'b>(
            &'a self,
            iov_buf: &'b mut [IoSlice<'a>; 2],
        ) -> &'b [IoSlice<'a>] {
            let head = self.head();
            if self.offset < head {
                iov_buf[0] = IoSlice::new(&self.bytes[self.offset..head]);
                &iov_buf[0..1]
            } else if head == 0 {
                iov_buf[0] = IoSlice::new(&self.bytes[self.offset..]);
                &iov_buf[0..1]
            } else {
                let (left, right) = self.bytes.split_at(self.offset);
                iov_buf[0] = IoSlice::new(right);
                iov_buf[1] = IoSlice::new(&left[..head]);
                &iov_buf[0..2]
            }
        }

        pub fn write_int(&mut self, val: i32) {
            self.write_bytes(&val.to_ne_bytes());
        }

        pub fn write_uint(&mut self, val: u32) {
            self.write_bytes(&val.to_ne_bytes());
        }

        pub fn read_int(&mut self) -> i32 {
            let mut buf = [0; 4];
            self.read_bytes(&mut buf);
            i32::from_ne_bytes(buf)
        }

        pub fn read_uint(&mut self) -> u32 {
            let mut buf = [0; 4];
            self.read_bytes(&mut buf);
            u32::from_ne_bytes(buf)
        }

        pub fn read_id(&mut self) -> Option<ObjectId> {
            NonZeroU32::new(self.read_uint()).map(ObjectId)
        }
    }
}

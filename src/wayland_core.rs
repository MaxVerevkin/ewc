use std::cell::Cell;
use std::cmp;
use std::fmt::{self, Debug};
use std::hash::{Hash, Hasher};
use std::io;
use std::rc::{Rc, Weak};

use crate::State;
use crate::client::{Client, ClientId, Connection, RequestCtx, ResourceCallback};
use crate::protocol::*;

pub use wayrs_core::{
    ArgType, ArgValue, Fixed, Interface, IoMode, Message, MessageDesc, MessageHeader, ObjectId,
    transport::BufferedSocket,
};

#[derive(Debug, Clone, Copy)]
pub struct BadMessage;
#[derive(Debug, Clone, Copy)]
pub struct WrongObject;

impl From<BadMessage> for io::Error {
    fn from(_: BadMessage) -> Self {
        io::Error::other("failed to parse message")
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
        std::ptr::eq(Rc::as_ptr(&self.inner), Rc::as_ptr(&other.inner))
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
            self.clients.get(id_as_index(id)).cloned()
        } else {
            self.servers.get(id_as_index(id)).cloned()
        }
    }

    pub fn register_clients(&mut self, object: Object) -> io::Result<()> {
        if object.id().created_by_server() {
            return Err(io::Error::other(format!(
                "client used object id {} which is reserved for server-side objects",
                object.id().as_u32()
            )));
        }

        let idx = id_as_index(object.id());

        if idx > self.clients.len() {
            return Err(io::Error::other(format!(
                "client used object id {} but {} was never used",
                object.id().as_u32(),
                object.id().as_u32() - 1
            )));
        }

        if let Some(slot) = self.clients.get_mut(idx) {
            *slot = object;
        } else if idx == self.clients.len() {
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

        let idx = id_as_index(id);

        if idx > self.servers.len() {
            unreachable!();
        }

        let object = Object::new(conn, id, interface, version);

        if let Some(slot) = self.servers.get_mut(idx) {
            *slot = object.clone();
        } else if idx == self.servers.len() {
            self.servers.push(object.clone());
        }

        Ok(object)
    }

    pub fn reuse_servers_id(&mut self, id: ObjectId) {
        assert!(id.created_by_server());
        self.free_server_ids.push(id);
    }
}

/// Calculate the index of this object in the array of client or server objects
fn id_as_index(id: ObjectId) -> usize {
    if id.created_by_client() {
        (id.0.get() - 1) as usize
    } else {
        (id.0.get() - ObjectId::MIN_SERVER.0.get()) as usize
    }
}

use std::io;
use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::rc::Rc;

use crate::client::{ClientId, RequestCtx};
use crate::protocol::wl_registry::BindArgs;
use crate::protocol::*;
use crate::wayland_core::{Interface, Object, ObjectId, Proxy};
use crate::{Client, State};

pub mod compositor;
pub mod ewc_debug;
pub mod linux_dmabuf;
pub mod shm;
pub mod xdg_shell;

pub trait IsGlobal: Proxy + 'static {
    fn on_bind(&self, client: &mut Client, state: &mut State);
}

#[derive(Default)]
pub struct GlobalsManager {
    globals: Vec<Global>,
    registries: Vec<WlRegistry>,
    last_name: u32,
}

impl GlobalsManager {
    pub fn add_global<P: IsGlobal>(&mut self, version: u32) {
        assert!(version <= P::INTERFACE.version);
        assert_ne!(version, 0);
        let name = self.last_name.checked_add(1).unwrap();
        self.globals.push(Global::new::<P>(name, version));
        self.last_name = name;
    }

    pub fn add_registry(&mut self, registry: WlRegistry) {
        registry.set_callback(wl_registry_cb);
        for g in &self.globals {
            registry.global(g.name(), g.interface().name.to_owned(), g.version());
        }
        self.registries.push(registry);
    }

    pub fn remove_client(&mut self, client_id: ClientId) {
        self.registries.retain(|r| r.client_id() != client_id);
    }
}

fn wl_registry_cb(ctx: RequestCtx<WlRegistry>) -> io::Result<()> {
    let wl_registry::Request::Bind(args) = ctx.request;
    let global = ctx
        .state
        .globals
        .globals
        .iter()
        .find(|g| g.name() == args.name)
        .ok_or_else(|| io::Error::other("wl_registry::bind with invalid name"))?
        .clone();
    global.bind(ctx.client, ctx.state, args)
}

#[derive(Clone)]
struct Global {
    name: u32,
    version: u32,
    imp: Rc<dyn GlobalImp>,
}

trait GlobalImp {
    fn interface(&self) -> &'static Interface;
    fn bind(
        &self,
        client: &mut Client,
        state: &mut State,
        args: wl_registry::BindArgs,
    ) -> io::Result<()>;
}

impl Global {
    pub fn new<G: IsGlobal>(name: u32, version: u32) -> Self {
        struct Imp<G: IsGlobal>(PhantomData<G>);
        impl<G: IsGlobal> GlobalImp for Imp<G> {
            fn interface(&self) -> &'static Interface {
                G::INTERFACE
            }
            fn bind(
                &self,
                client: &mut Client,
                state: &mut State,
                args: wl_registry::BindArgs,
            ) -> io::Result<()> {
                let object_id = ObjectId(NonZeroU32::new(args.id_id).unwrap());
                let object = Object::new(&client.conn, object_id, G::INTERFACE, args.id_version);
                client.conn.register_clients_object(object.clone())?;
                G::try_from(object).unwrap().on_bind(client, state);
                Ok(())
            }
        }
        Self {
            name,
            version,
            imp: Rc::new(Imp::<G>(PhantomData)),
        }
    }

    pub fn name(&self) -> u32 {
        self.name
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn interface(&self) -> &'static Interface {
        self.imp.interface()
    }

    pub fn bind(&self, client: &mut Client, state: &mut State, args: BindArgs) -> io::Result<()> {
        if self.interface().name != args.id_interface.as_c_str() {
            return Err(io::Error::other("wl_registry::bind with invalid interface"));
        }
        if self.version() < args.id_version {
            return Err(io::Error::other("wl_registry::bind with invalid version"));
        }
        self.imp.bind(client, state, args)
    }
}

impl IsGlobal for WlOutput {
    fn on_bind(&self, _client: &mut Client, _state: &mut State) {
        // TODO
    }
}

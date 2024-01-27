use std::io;
use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::rc::Rc;

use crate::protocol::wl_registry::BindArgs;
use crate::protocol::*;
use crate::wayland_core::{Interface, Object, ObjectId, Proxy};
use crate::{Client, State};

pub mod compositor;
pub mod ewc_debug;
pub mod shm;
pub mod xdg_shell;

#[derive(Clone)]
pub struct Global {
    name: u32,
    version: u32,
    imp: Rc<dyn GlobalImp>,
}

pub trait IsGlobal: Proxy + 'static {
    fn on_bind(&self, _client: &mut Client, _state: &mut State) {}
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

impl IsGlobal for WlOutput {}

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::io;

use crate::client::{ClientId, RequestCtx};
use crate::config::Config;
use crate::globals::{GlobalsManager, IsGlobal};
use crate::protocol::*;
use crate::wayland_core::Proxy;
use crate::{Client, State};

mod keyboard;
pub mod pointer;

pub struct Seat {
    pub keyboard: keyboard::Keyboard,
    pub pointer: pointer::Pointer,
}

#[derive(Default)]
pub struct ClientSeat {
    pub keyboards: RefCell<Vec<WlKeyboard>>,
    pub pointers: RefCell<Vec<WlPointer>>,
    pub data_devices: RefCell<Vec<WlDataDevice>>,
    pub data_offers: RefCell<HashMap<WlDataOffer, WlDataSource>>,
}

#[derive(Debug)]
pub struct DataSource {
    pub wl: WlDataSource,
    pub mime: Vec<CString>,
}

impl Seat {
    pub fn register_globals(globals: &mut GlobalsManager) {
        globals.add_global::<WlSeat>(5);
        globals.add_global::<WlDataDeviceManager>(3);
    }

    pub fn new(config: &Config) -> Self {
        Self {
            keyboard: keyboard::Keyboard::new(config),
            pointer: pointer::Pointer::new(),
        }
    }

    pub fn remove_client(&mut self, client_id: ClientId) {
        if self
            .keyboard
            .get_selection()
            .is_some_and(|x| x.wl.client_id() == client_id)
        {
            self.keyboard.set_selection(None);
        }
    }

    pub fn surface_unmapped(&mut self, wl_surface: &WlSurface) {
        self.keyboard.surface_unmapped(wl_surface);
        self.pointer.surface_unmapped(wl_surface);
    }
}

impl DataSource {
    fn new_data_offer(&self, data_device: &WlDataDevice) -> io::Result<WlDataOffer> {
        let data_offer: WlDataOffer = data_device
            .conn()
            .create_servers_object(data_device.version())?;
        data_offer.set_callback(wl_data_offer_cb);
        data_device
            .conn()
            .seat
            .data_offers
            .borrow_mut()
            .insert(data_offer.clone(), self.wl.clone());
        data_device.data_offer(&data_offer);
        for mime in &self.mime {
            data_offer.offer(mime.clone());
        }
        Ok(data_offer)
    }
}

impl IsGlobal for WlSeat {
    fn on_bind(&self, _client: &mut Client, _state: &mut State) {
        self.capabilities(wl_seat::Capability::Keyboard | wl_seat::Capability::Pointer);
        self.set_callback(|ctx| {
            use wl_seat::Request;
            match ctx.request {
                Request::GetPointer(wl_pointer) => {
                    ctx.state.seat.pointer.init_new_resource(&wl_pointer);
                    ctx.client.conn.seat.pointers.borrow_mut().push(wl_pointer);
                }
                Request::GetKeyboard(wl_keyboard) => {
                    ctx.state.seat.keyboard.init_keyboard(&wl_keyboard)?;
                    ctx.client
                        .conn
                        .seat
                        .keyboards
                        .borrow_mut()
                        .push(wl_keyboard);
                }
                Request::GetTouch(_) => {
                    return Err(io::Error::other("touch input not supporetd"));
                }
                Request::Release => (),
            }
            Ok(())
        });
    }
}

impl IsGlobal for WlDataDeviceManager {
    fn on_bind(&self, _client: &mut Client, _state: &mut State) {
        self.set_callback(|ctx| {
            use wl_data_device_manager::Request;
            match ctx.request {
                Request::CreateDataSource(wl_data_source) => {
                    wl_data_source.set_callback(wl_data_source_cb);
                    ctx.client.data_sources.insert(
                        wl_data_source.clone(),
                        DataSource {
                            wl: wl_data_source,
                            mime: Vec::new(),
                        },
                    );
                }
                Request::GetDataDevice(args) => {
                    args.id.set_callback(wl_data_device_cb);
                    if ctx
                        .state
                        .seat
                        .keyboard
                        .focused_surface()
                        .is_some_and(|x| x.client_id() == args.id.client_id())
                    {
                        ctx.state.seat.keyboard.send_selection(&args.id);
                    }
                    ctx.client.conn.seat.data_devices.borrow_mut().push(args.id);
                }
            }
            Ok(())
        })
    }
}

fn wl_data_source_cb(ctx: RequestCtx<WlDataSource>) -> io::Result<()> {
    use wl_data_source::Request;
    match ctx.request {
        Request::Offer(mime) => {
            ctx.client
                .data_sources
                .get_mut(&ctx.proxy)
                .ok_or_else(|| io::Error::other("used data usource"))?
                .mime
                .push(mime);
        }
        Request::Destroy => {
            if ctx.state.seat.keyboard.get_selection().map(|x| &x.wl) == Some(&ctx.proxy) {
                ctx.state.seat.keyboard.set_selection(None);
            }
            ctx.client.data_sources.remove(&ctx.proxy);
        }
        Request::SetActions(_) => todo!(),
    }
    Ok(())
}

fn wl_data_device_cb(ctx: RequestCtx<WlDataDevice>) -> io::Result<()> {
    use wl_data_device::Request;
    match ctx.request {
        Request::StartDrag(_) => todo!(),
        Request::SetSelection(args) => {
            ctx.state.seat.keyboard.set_selection(match args.source {
                None => None,
                Some(source) => Some(
                    ctx.client
                        .data_sources
                        .remove(&source)
                        .ok_or_else(|| io::Error::other("used data usource"))?,
                ),
            });
        }
        Request::Release => {
            ctx.client
                .conn
                .seat
                .data_devices
                .borrow_mut()
                .retain(|x| *x != ctx.proxy);
        }
    }
    Ok(())
}

fn wl_data_offer_cb(ctx: RequestCtx<WlDataOffer>) -> io::Result<()> {
    use wl_data_offer::Request;
    match ctx.request {
        Request::Accept(_) => todo!(),
        Request::Receive(args) => {
            let data_source = ctx
                .client
                .conn
                .seat
                .data_offers
                .borrow()
                .get(&ctx.proxy)
                .unwrap()
                .clone();
            if data_source.is_alive() {
                data_source.send(args.mime_type, args.fd);
            }
        }
        Request::Destroy => {
            ctx.client
                .conn
                .seat
                .data_offers
                .borrow_mut()
                .remove(&ctx.proxy);
        }
        Request::Finish => todo!(),
        Request::SetActions(_) => todo!(),
    }
    Ok(())
}

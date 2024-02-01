use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::io;

use crate::client::{ClientId, RequestCtx};
use crate::globals::{GlobalsManager, IsGlobal};
use crate::protocol::*;
use crate::wayland_core::Proxy;
use crate::{Client, State};

mod keyboard;
pub mod pointer;

pub struct Seat {
    pub keyboard: keyboard::Keyboard,
    pub pointer: pointer::Pointer,
    pub selection: Option<DataSource>,
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

    pub fn new() -> Self {
        Self {
            keyboard: keyboard::Keyboard::new(),
            pointer: pointer::Pointer::new(),
            selection: None,
        }
    }

    pub fn remove_client(&mut self, client_id: ClientId) {
        if self
            .keyboard
            .focused_surface
            .as_ref()
            .is_some_and(|surf| surf.client_id() == client_id)
        {
            self.keyboard.focused_surface = None;
        }

        if let Some(surf) = self.pointer.get_focused_surface() {
            if surf.wl.client_id() == client_id {
                self.pointer.unfocus_surface(&surf.wl);
            }
        }

        if self
            .selection
            .as_ref()
            .is_some_and(|x| x.wl.client_id() == client_id)
        {
            self.selection = None;
            self.send_selection_to_focused();
        }
    }

    pub fn unfocus_surface(&mut self, wl_surface: &WlSurface) {
        self.keyboard.unfocus_surface(wl_surface);
        self.pointer.unfocus_surface(wl_surface);
    }

    pub fn kbd_focus_surface(&mut self, surface: Option<WlSurface>) {
        if self.keyboard.focused_surface == surface {
            return;
        }
        if let Some(surface) = &surface {
            if self
                .keyboard
                .focused_surface
                .as_ref()
                .map_or(true, |old| old.client_id() != surface.client_id())
            {
                for data_device in &*surface.conn().seat.data_devices.borrow() {
                    self.send_selection(data_device);
                }
            }
        }
        self.keyboard.focus_surface(surface);
    }

    pub fn send_selection_to_focused(&self) {
        if let Some(focused) = &self.keyboard.focused_surface {
            for data_device in &*focused.conn().seat.data_devices.borrow() {
                self.send_selection(data_device);
            }
        }
    }

    pub fn send_selection(&self, data_device: &WlDataDevice) {
        match &self.selection {
            None => data_device.selection(None),
            Some(selection) => {
                let data_offer: WlDataOffer = data_device
                    .conn()
                    .create_servers_object(data_device.version())
                    .unwrap();
                data_offer.set_callback(wl_data_offer_cb);
                data_device
                    .conn()
                    .seat
                    .data_offers
                    .borrow_mut()
                    .insert(data_offer.clone(), selection.wl.clone());
                data_device.data_offer(&data_offer);
                for mime in &selection.mime {
                    data_offer.offer(mime.clone());
                }
                data_device.selection(Some(&data_offer));
            }
        }
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
                        .focused_surface
                        .as_ref()
                        .is_some_and(|x| x.client_id() == args.id.client_id())
                    {
                        ctx.state.seat.send_selection(&args.id);
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
            if ctx.state.seat.selection.as_ref().map(|x| &x.wl) == Some(&ctx.proxy) {
                ctx.state.seat.selection = None;
                ctx.state.seat.send_selection_to_focused();
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
            if let Some(old) = ctx.state.seat.selection.take() {
                old.wl.cancelled();
            }

            ctx.state.seat.selection = match args.source {
                None => None,
                Some(source) => Some(
                    ctx.client
                        .data_sources
                        .remove(&source)
                        .ok_or_else(|| io::Error::other("used data usource"))?,
                ),
            };

            ctx.state.seat.send_selection_to_focused();
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

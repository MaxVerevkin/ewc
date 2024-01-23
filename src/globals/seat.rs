use std::cell::RefCell;
use std::ffi::CStr;
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::io::Write;
use std::os::fd::AsFd;
use std::os::fd::FromRawFd;

use xkbcommon::xkb;

use super::{Global, IsGlobal};
use crate::client::RequestCtx;
use crate::protocol::*;
use crate::wayland_core::{Fixed, Proxy};
use crate::Client;

// pub const BTN_MOUSE: u32 = 0x110;
pub const BTN_LEFT: u32 = 0x110;
// pub const BTN_RIGHT: u32 = 0x111;
// pub const BTN_MIDDLE: u32 = 0x112;
// pub const BTN_SIDE: u32 = 0x113;
// pub const BTN_EXTRA: u32 = 0x114;
// pub const BTN_FORWARD: u32 = 0x115;
// pub const BTN_BACK: u32 = 0x116;
// pub const BTN_TASK: u32 = 0x117;

pub struct Seat {
    keymap_file: File,
    keymap_file_size: u32,

    kbd_focused_surface: Option<WlSurface>,
    ptr_focused_surface: Option<(WlSurface, f32, f32)>,

    mods: ModsState,
    pub xkb_state: xkb::State,

    pub pointer_x: f32,
    pub pointer_y: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModsState {
    depressed: u32,
    latched: u32,
    locked: u32,
    group: u32,
}

impl ModsState {
    pub fn get(xkb_state: &xkb::State) -> Self {
        Self {
            depressed: xkb_state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
            latched: xkb_state.serialize_mods(xkb::STATE_MODS_LATCHED),
            locked: xkb_state.serialize_mods(xkb::STATE_MODS_LOCKED),
            group: xkb_state.serialize_mods(xkb::STATE_LAYOUT_EFFECTIVE),
        }
    }

    pub fn send(&self, serial: u32, wl_kbd: &WlKeyboard) {
        wl_kbd.modifiers(
            serial,
            self.depressed,
            self.latched,
            self.locked,
            self.group,
        );
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ModsMask {
    pub logo: bool,
    pub alt: bool,
}

pub struct ClientSeat {
    pub keyboards: RefCell<Vec<Keyboard>>,
    pub pointers: RefCell<Vec<Pointer>>,
}

pub struct Keyboard {
    pub wl: WlKeyboard,
}

pub struct Pointer {
    pub wl: WlPointer,
}

impl Seat {
    pub fn global(name: u32) -> Global {
        Global::new::<WlSeat>(name, 5)
    }

    pub fn new() -> Self {
        let xkb_context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let xkb_keymap =
            xkb::Keymap::new_from_names(&xkb_context, "", "", "us(dvp),ru", "", None, 0).unwrap();
        let keymap_string_ptr = unsafe {
            xkb::ffi::xkb_keymap_get_as_string(xkb_keymap.get_raw_ptr(), xkb::KEYMAP_FORMAT_TEXT_V1)
        };
        assert!(!keymap_string_ptr.is_null());
        let keymap_string = unsafe { CStr::from_ptr(keymap_string_ptr) };
        let keymap_bytes = keymap_string.to_bytes_with_nul();
        let mut keymap_file = unsafe {
            File::from_raw_fd(shmemfdrs::create_shmem(
                CString::new("/ewc-keymap-file").unwrap(),
                keymap_bytes.len(),
            ))
        };
        keymap_file.write_all(keymap_bytes).unwrap();
        let keymap_file_size = keymap_bytes.len() as u32;
        unsafe { libc::free(keymap_string_ptr.cast()) };
        let xkb_state = xkb::State::new(&xkb_keymap);
        Self {
            keymap_file,
            keymap_file_size,

            kbd_focused_surface: None,
            ptr_focused_surface: None,

            mods: ModsState::get(&xkb_state),
            xkb_state,

            pointer_x: 0.0,
            pointer_y: 0.0,
        }
    }

    pub fn kbd_focus_surface(&mut self, surface: Option<WlSurface>) {
        if self.kbd_focused_surface == surface {
            return;
        }

        if let Some(old_surf) = &self.kbd_focused_surface {
            if old_surf.is_alive() {
                for kbd in old_surf.conn().seat.keyboards.borrow().iter() {
                    kbd.wl.leave(1, old_surf);
                }
            }
        }

        self.kbd_focused_surface = surface;
        if let Some(new_surf) = &self.kbd_focused_surface {
            for kbd in new_surf.conn().seat.keyboards.borrow().iter() {
                self.kbd_enter(&kbd.wl);
            }
        }
    }

    pub fn ptr_forward_pointer(&mut self, surface: Option<(WlSurface, f32, f32)>) {
        if let Some((surface, x, y)) = &surface {
            if let Some((focused, _, _)) = &self.ptr_focused_surface {
                if surface == focused {
                    for ptr in surface.conn().seat.pointers.borrow().iter() {
                        ptr.wl.motion(0, Fixed::from(*x), Fixed::from(*y));
                    }
                    return;
                }
            }
        }

        if let Some((old_surf, _, _)) = &self.ptr_focused_surface {
            if old_surf.is_alive() {
                for ptr in old_surf.conn().seat.pointers.borrow().iter() {
                    ptr.wl.leave(1, old_surf);
                }
            }
        }

        self.ptr_focused_surface = surface;
        if let Some((new_surf, _x, _y)) = &self.ptr_focused_surface {
            for ptr in new_surf.conn().seat.pointers.borrow().iter() {
                self.ptr_enter(&ptr.wl);
            }
        }
    }

    pub fn ptr_forward_btn(&mut self, btn: u32, pressed: bool) {
        let state = if pressed {
            wl_pointer::ButtonState::Pressed
        } else {
            wl_pointer::ButtonState::Released
        };
        if let Some((new_surf, _x, _y)) = &self.ptr_focused_surface {
            for ptr in new_surf.conn().seat.pointers.borrow().iter() {
                ptr.wl.button(1, 0, btn, state);
            }
        }
    }

    fn kbd_enter(&self, wl_keyboard: &WlKeyboard) {
        if let Some(surf) = &self.kbd_focused_surface {
            wl_keyboard.enter(1, surf, Vec::new());
            self.mods.send(1, wl_keyboard);
        }
    }

    fn ptr_enter(&self, wl_pointer: &WlPointer) {
        if let Some((surf, x, y)) = &self.ptr_focused_surface {
            wl_pointer.enter(1, surf, Fixed::from(*x), Fixed::from(*y));
        }
    }

    pub fn unfocus_surface(&mut self, wl_surface: &WlSurface) {
        if self.kbd_focused_surface.as_ref() == Some(wl_surface) {
            self.kbd_focus_surface(None);
        }
        if self
            .ptr_focused_surface
            .as_ref()
            .is_some_and(|x| x.0 == *wl_surface)
        {
            self.ptr_forward_pointer(None);
        }
    }

    pub fn update_key(&mut self, key: u32, pressed: bool) {
        self.xkb_state.update_key(
            xkb::Keycode::new(key + 8),
            if pressed {
                xkbcommon::xkb::KeyDirection::Down
            } else {
                xkbcommon::xkb::KeyDirection::Up
            },
        );

        let mods = ModsState::get(&self.xkb_state);
        if self.mods != mods {
            self.mods = mods;
            if let Some(focused_surf) = &self.kbd_focused_surface {
                for kbd in focused_surf.conn().seat.keyboards.borrow().iter() {
                    mods.send(1, &kbd.wl);
                }
            }
        }

        let state = if pressed {
            wl_keyboard::KeyState::Pressed
        } else {
            wl_keyboard::KeyState::Released
        };

        if let Some(focused_surf) = &self.kbd_focused_surface {
            for kbd in focused_surf.conn().seat.keyboards.borrow().iter() {
                kbd.wl.key(1, 0, key, state);
            }
        }
    }

    pub fn get_mods(&self) -> ModsMask {
        let mask = self.mods.depressed | self.mods.latched;
        ModsMask {
            logo: mask
                & (1 << self
                    .xkb_state
                    .get_keymap()
                    .mod_get_index(xkb::MOD_NAME_LOGO))
                != 0,
            alt: mask & (1 << self.xkb_state.get_keymap().mod_get_index(xkb::MOD_NAME_ALT)) != 0,
        }
    }
}

impl ClientSeat {
    pub fn new() -> Self {
        Self {
            keyboards: RefCell::new(Vec::new()),
            pointers: RefCell::new(Vec::new()),
        }
    }
}

impl IsGlobal for WlSeat {
    fn on_bind(&self, _client: &mut Client) {
        self.capabilities(wl_seat::Capability::Keyboard | wl_seat::Capability::Pointer);
        self.set_callback(|ctx| {
            use wl_seat::Request;
            match ctx.request {
                Request::GetPointer(wl_pointer) => {
                    wl_pointer.set_callback(wl_pointer_cb);
                    if let Some((surf, _x, _y)) = &ctx.state.seat.ptr_focused_surface {
                        if surf.client_id() == ctx.client.conn.client_id() {
                            ctx.state.seat.ptr_enter(&wl_pointer);
                        }
                    }
                    ctx.client
                        .conn
                        .seat
                        .pointers
                        .borrow_mut()
                        .push(Pointer { wl: wl_pointer });
                }
                Request::GetKeyboard(wl_keyboard) => {
                    wl_keyboard.set_callback(wl_keyboard_cb);
                    wl_keyboard.keymap(
                        wl_keyboard::KeymapFormat::XkbV1,
                        ctx.state.seat.keymap_file.as_fd().try_clone_to_owned()?,
                        ctx.state.seat.keymap_file_size,
                    );
                    wl_keyboard.repeat_info(40, 300);
                    if let Some(surf) = &ctx.state.seat.kbd_focused_surface {
                        if surf.client_id() == ctx.client.conn.client_id() {
                            ctx.state.seat.kbd_enter(&wl_keyboard);
                        }
                    }
                    ctx.client
                        .conn
                        .seat
                        .keyboards
                        .borrow_mut()
                        .push(Keyboard { wl: wl_keyboard });
                }
                Request::GetTouch(_) => todo!(),
                Request::Release => (),
            }
            Ok(())
        });
    }
}

fn wl_keyboard_cb(ctx: RequestCtx<WlKeyboard>) -> io::Result<()> {
    let wl_keyboard::Request::Release = ctx.request;
    ctx.client
        .conn
        .seat
        .keyboards
        .borrow_mut()
        .retain(|k| k.wl != ctx.proxy);
    Ok(())
}

fn wl_pointer_cb(ctx: RequestCtx<WlPointer>) -> io::Result<()> {
    use wl_pointer::Request;
    match ctx.request {
        Request::SetCursor(args) => match args.surface {
            None => ctx.state.cursor = None,
            Some(surf) => {
                let surface = ctx.client.compositor.surfaces.get(&surf).unwrap();
                if surface.has_role() {
                    return Err(io::Error::other("surface already has a role"));
                }
                ctx.state.cursor = Some((surface.clone(), args.hotspot_x, args.hotspot_y));
            }
        },
        Request::Release => {
            ctx.client
                .conn
                .seat
                .pointers
                .borrow_mut()
                .retain(|p| p.wl != ctx.proxy);
        }
    }
    Ok(())
}

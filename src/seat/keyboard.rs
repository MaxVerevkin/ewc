use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsFd, FromRawFd};

use xkbcommon::xkb;

use crate::client::RequestCtx;
use crate::protocol::*;
use crate::wayland_core::Proxy;

pub struct Keyboard {
    keymap_file: File,
    keymap_file_size: u32,
    pub xkb_state: xkb::State,
    mods: ModsState,
    pub(super) focused_surface: Option<WlSurface>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModsState {
    depressed: u32,
    latched: u32,
    locked: u32,
    group: u32,
}

impl ModsState {
    fn get(xkb_state: &xkb::State) -> Self {
        Self {
            depressed: xkb_state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
            latched: xkb_state.serialize_mods(xkb::STATE_MODS_LATCHED),
            locked: xkb_state.serialize_mods(xkb::STATE_MODS_LOCKED),
            group: xkb_state.serialize_mods(xkb::STATE_LAYOUT_EFFECTIVE),
        }
    }

    fn send(&self, serial: u32, wl_kbd: &WlKeyboard) {
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

impl Keyboard {
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
            mods: ModsState::get(&xkb_state),
            xkb_state,
            focused_surface: None,
        }
    }

    pub fn init_keyboard(&self, wl_keyboard: &WlKeyboard) -> io::Result<()> {
        wl_keyboard.set_callback(wl_keyboard_cb);
        wl_keyboard.keymap(
            wl_keyboard::KeymapFormat::XkbV1,
            self.keymap_file.as_fd().try_clone_to_owned()?,
            self.keymap_file_size,
        );
        if wl_keyboard.version() >= 4 {
            wl_keyboard.repeat_info(40, 300);
        }
        if let Some(surf) = &self.focused_surface {
            if surf.client_id() == wl_keyboard.client_id() {
                self.enter(wl_keyboard);
            }
        }
        Ok(())
    }

    pub(super) fn focus_surface(&mut self, surface: Option<WlSurface>) {
        if self.focused_surface == surface {
            return;
        }

        if let Some(old_surf) = &self.focused_surface {
            if old_surf.is_alive() {
                for kbd in old_surf.conn().seat.keyboards.borrow().iter() {
                    kbd.leave(1, old_surf);
                }
            }
        }

        self.focused_surface = surface;
        if let Some(new_surf) = &self.focused_surface {
            for kbd in new_surf.conn().seat.keyboards.borrow().iter() {
                self.enter(kbd);
            }
        }
    }

    fn enter(&self, wl_keyboard: &WlKeyboard) {
        if let Some(surf) = &self.focused_surface {
            wl_keyboard.enter(1, surf, Vec::new());
            self.mods.send(1, wl_keyboard);
        }
    }

    pub fn unfocus_surface(&mut self, wl_surface: &WlSurface) {
        if self.focused_surface.as_ref() == Some(wl_surface) {
            self.focus_surface(None);
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
            if let Some(focused_surf) = &self.focused_surface {
                for kbd in focused_surf.conn().seat.keyboards.borrow().iter() {
                    mods.send(1, kbd);
                }
            }
        }

        let state = if pressed {
            wl_keyboard::KeyState::Pressed
        } else {
            wl_keyboard::KeyState::Released
        };

        if let Some(focused_surf) = &self.focused_surface {
            for kbd in focused_surf.conn().seat.keyboards.borrow().iter() {
                kbd.key(1, 0, key, state);
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

fn wl_keyboard_cb(ctx: RequestCtx<WlKeyboard>) -> io::Result<()> {
    let wl_keyboard::Request::Release = ctx.request;
    ctx.client
        .conn
        .seat
        .keyboards
        .borrow_mut()
        .retain(|k| *k != ctx.proxy);
    Ok(())
}

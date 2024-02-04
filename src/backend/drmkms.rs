use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::Path;
use std::rc::Rc;

use drm::buffer::{Buffer as _, DrmFourcc};
use drm::control::atomic::AtomicModeReq;
use drm::control::dumbbuffer::DumbBuffer;
use drm::control::{AtomicCommitFlags, Device, FbCmd2Flags};
use drm::Device as _;
use input::event::keyboard::KeyboardEventTrait;
use input::event::pointer::{PointerEventTrait, PointerScrollEvent};
use input::Libinput;

use super::*;
use crate::protocol::wl_shm;

pub fn new() -> Option<Box<dyn Backend>> {
    let seat = Rc::new(libseat::Seat::open().unwrap());
    let mut libinput = input::Libinput::new_with_udev(LibinputIface {
        seat: seat.clone(),
        devices: HashMap::new(),
    });
    libinput.udev_assign_seat(seat.name()).unwrap();

    let card = Card::open(&seat, "/dev/dri/card1").unwrap();

    card.set_client_capability(drm::ClientCapability::UniversalPlanes, true)
        .expect("Unable to request UniversalPlanes capability");
    card.set_client_capability(drm::ClientCapability::Atomic, true)
        .expect("Unable to request Atomic capability");

    card.reset_crtcs().expect("could not reset CRTCs");

    let res = card
        .resource_handles()
        .expect("Could not load normal resource ids.");
    let coninfo: Vec<drm::control::connector::Info> = res
        .connectors()
        .iter()
        .flat_map(|con| card.get_connector(*con, true))
        .collect();
    let crtcinfo: Vec<drm::control::crtc::Info> = res
        .crtcs()
        .iter()
        .flat_map(|crtc| card.get_crtc(*crtc))
        .collect();

    let con = coninfo
        .iter()
        .rev()
        .find(|i| i.state() == drm::control::connector::State::Connected)
        .expect("No connected connectors");
    let mode = *con.modes().first().expect("No modes found on connector");
    let (disp_width, disp_height) = mode.size();
    let disp_width = disp_width as u32;
    let disp_height = disp_height as u32;

    let crtc = crtcinfo.first().expect("No crtcs found");
    let planes = card.plane_handles().expect("Could not list planes");

    #[derive(Debug)]
    struct PlaneData {
        handle: drm::control::plane::Handle,
        is_primary: bool,
        formats: HashMap<eglgbm::Fourcc, Vec<u64>>,
    }

    let mut plane_data = Vec::<PlaneData>::new();
    for &plane in &planes {
        let Ok(plane_info) = card.get_plane(plane) else { continue };
        if !res
            .filter_crtcs(plane_info.possible_crtcs())
            .contains(&crtc.handle())
        {
            continue;
        }
        let Ok(props) = card.get_properties(plane) else { continue };
        let mut is_primary = None;
        let mut formats = None;
        for (&prop_id, &prop_value) in &props {
            let Ok(info) = card.get_property(prop_id) else { continue };
            match info.name().to_str() {
                Ok("type") => {
                    is_primary =
                        Some(prop_value == (drm::control::PlaneType::Primary as u32).into());
                }
                Ok("IN_FORMATS") => {
                    let blob = card.get_property_blob(prop_value).unwrap();
                    formats = Some(parse_drm_format_modifier_blob(&blob));
                }
                _ => (),
            }
        }
        if let Some((is_primary, formats)) = is_primary.zip(formats) {
            plane_data.push(PlaneData {
                handle: plane,
                is_primary,
                formats,
            });
        }
    }

    let (better_planes, compatible_planes): (Vec<PlaneData>, Vec<PlaneData>) =
        plane_data.into_iter().partition(|plane| plane.is_primary);
    let plane = better_planes.first().unwrap_or(&compatible_planes[0]);

    let (renderer_kind, fb_swapchain) = if std::env::var_os("EWC_NO_GL").is_none() {
        let mut state =
            gl46_renderer::RendererStateImp::with_drm_fd(card.as_fd().as_raw_fd(), &plane.formats)
                .unwrap();
        let (glfb, export) = state.allocate_framebuffer(disp_width, disp_height, true);
        let (glfb2, export2) = state.allocate_framebuffer(disp_width, disp_height, true);
        let buf = PlanarBufer {
            width: disp_width,
            height: disp_height,
            export,
        };
        let buf2 = PlanarBufer {
            width: disp_width,
            height: disp_height,
            export: export2,
        };
        let fb = card
            .add_planar_framebuffer(&buf, FbCmd2Flags::MODIFIERS)
            .unwrap();
        let fb2 = card
            .add_planar_framebuffer(&buf2, FbCmd2Flags::MODIFIERS)
            .unwrap();
        (
            RendererKind::OpenGl {
                width: disp_width,
                height: disp_height,
                swapchain: [glfb, glfb2],
                state,
            },
            [fb, fb2],
        )
    } else {
        let buf = card
            .create_dumb_buffer((disp_width, disp_height), DrmFourcc::Xrgb8888, 32)
            .expect("Could not create dumb buffer");
        let buf2 = card
            .create_dumb_buffer((disp_width, disp_height), DrmFourcc::Xrgb8888, 32)
            .expect("Could not create dumb buffer");
        let fb = card
            .add_framebuffer(&buf, 24, 32)
            .expect("Could not create FB");
        let fb2 = card
            .add_framebuffer(&buf2, 24, 32)
            .expect("Could not create FB");
        (
            RendererKind::Pixman {
                swapchain: [buf, buf2],
                state: pixman_renderer::RendererStateImp::new(),
                temp_buf: vec![0u8; disp_width as usize * disp_height as usize * 4],
            },
            [fb, fb2],
        )
    };

    let con_props = card
        .get_properties(con.handle())
        .expect("Could not get props of connector")
        .as_hashmap(&card)
        .expect("Could not get a prop from connector");
    let crtc_props = card
        .get_properties(crtc.handle())
        .expect("Could not get props of crtc")
        .as_hashmap(&card)
        .expect("Could not get a prop from crtc");
    let plane_props = card
        .get_properties(plane.handle)
        .expect("Could not get props of plane")
        .as_hashmap(&card)
        .expect("Could not get a prop from plane");

    let mut atomic_req = AtomicModeReq::new();
    atomic_req.add_property(
        con.handle(),
        con_props["CRTC_ID"].handle(),
        drm::control::property::Value::CRTC(Some(crtc.handle())),
    );
    let blob = card
        .create_property_blob(&mode)
        .expect("Failed to create blob");
    atomic_req.add_property(crtc.handle(), crtc_props["MODE_ID"].handle(), blob);
    atomic_req.add_property(
        crtc.handle(),
        crtc_props["ACTIVE"].handle(),
        drm::control::property::Value::Boolean(true),
    );
    atomic_req.add_property(
        plane.handle,
        plane_props["FB_ID"].handle(),
        drm::control::property::Value::Framebuffer(Some(fb_swapchain[0])),
    );
    atomic_req.add_property(
        plane.handle,
        plane_props["CRTC_ID"].handle(),
        drm::control::property::Value::CRTC(Some(crtc.handle())),
    );
    atomic_req.add_property(
        plane.handle,
        plane_props["SRC_X"].handle(),
        drm::control::property::Value::UnsignedRange(0),
    );
    atomic_req.add_property(
        plane.handle,
        plane_props["SRC_Y"].handle(),
        drm::control::property::Value::UnsignedRange(0),
    );
    atomic_req.add_property(
        plane.handle,
        plane_props["SRC_W"].handle(),
        drm::control::property::Value::UnsignedRange((mode.size().0 as u64) << 16),
    );
    atomic_req.add_property(
        plane.handle,
        plane_props["SRC_H"].handle(),
        drm::control::property::Value::UnsignedRange((mode.size().1 as u64) << 16),
    );
    atomic_req.add_property(
        plane.handle,
        plane_props["CRTC_X"].handle(),
        drm::control::property::Value::SignedRange(0),
    );
    atomic_req.add_property(
        plane.handle,
        plane_props["CRTC_Y"].handle(),
        drm::control::property::Value::SignedRange(0),
    );
    atomic_req.add_property(
        plane.handle,
        plane_props["CRTC_W"].handle(),
        drm::control::property::Value::UnsignedRange(mode.size().0 as u64),
    );
    atomic_req.add_property(
        plane.handle,
        plane_props["CRTC_H"].handle(),
        drm::control::property::Value::UnsignedRange(mode.size().1 as u64),
    );
    card.atomic_commit(
        AtomicCommitFlags::ALLOW_MODESET | AtomicCommitFlags::PAGE_FLIP_EVENT,
        atomic_req.clone(),
    )
    .expect("Failed to set mode");

    Some(Box::new(BackendImp {
        suspended: false,
        card,
        seat,
        libinput,
        atomic_req,
        plane: plane.handle,
        plane_props,
        backend_events_queue: VecDeque::new(),
        fb_swapchain,
        renderer_kind,
    }))
}

#[allow(clippy::large_enum_variant)]
enum RendererKind {
    Pixman {
        swapchain: [DumbBuffer; 2],
        state: pixman_renderer::RendererStateImp,
        temp_buf: Vec<u8>,
    },
    OpenGl {
        width: u32,
        height: u32,
        swapchain: [gl46_renderer::Framebuffer; 2],
        state: gl46_renderer::RendererStateImp,
    },
}

struct BackendImp {
    suspended: bool,
    card: Card,
    seat: Rc<libseat::Seat>,
    libinput: Libinput,
    atomic_req: AtomicModeReq,
    plane: drm::control::plane::Handle,
    plane_props: HashMap<String, drm::control::property::Info>,
    backend_events_queue: VecDeque<BackendEvent>,
    fb_swapchain: [drm::control::framebuffer::Handle; 2],
    renderer_kind: RendererKind,
}

struct LibinputIface {
    seat: Rc<libseat::Seat>,
    devices: HashMap<RawFd, libseat::DeviceId>,
}

impl input::LibinputInterface for LibinputIface {
    fn open_restricted(&mut self, path: &Path, _flags: i32) -> Result<OwnedFd, i32> {
        let (fd, id) = self
            .seat
            .open_device(path)
            .map_err(|e| e.raw_os_error().unwrap())?;
        self.devices.insert(fd.as_raw_fd(), id);
        Ok(fd)
    }

    fn close_restricted(&mut self, fd: OwnedFd) {
        let id = self.devices.remove(&fd.as_raw_fd()).unwrap();
        self.seat.close_device(id).unwrap();
    }
}

struct Card {
    fd: OwnedFd,
    id: Option<libseat::DeviceId>,
}

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl Card {
    fn open(seat: &libseat::Seat, path: &str) -> io::Result<Self> {
        let (fd, id) = seat.open_device(path)?;
        Ok(Self { fd, id: Some(id) })
    }

    fn reset_crtcs(&self) -> io::Result<()> {
        let resources = self.resource_handles()?;
        let mut atomic_req = AtomicModeReq::new();
        for &con in resources.connectors() {
            let props = self.get_properties(con)?.as_hashmap(self)?;
            atomic_req.add_property(
                con,
                props["CRTC_ID"].handle(),
                drm::control::property::Value::CRTC(None),
            );
        }
        for &plane in &self.plane_handles()? {
            let props = self.get_properties(plane)?.as_hashmap(self)?;
            atomic_req.add_property(
                plane,
                props["FB_ID"].handle(),
                drm::control::property::Value::Framebuffer(None),
            );
            atomic_req.add_property(
                plane,
                props["CRTC_ID"].handle(),
                drm::control::property::Value::CRTC(None),
            );
        }
        for &crtc in resources.crtcs() {
            let props = self.get_properties(crtc)?.as_hashmap(self)?;
            atomic_req.add_property(
                crtc,
                props["MODE_ID"].handle(),
                drm::control::property::Value::Blob(0),
            );
            atomic_req.add_property(
                crtc,
                props["ACTIVE"].handle(),
                drm::control::property::Value::Boolean(false),
            );
        }
        self.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, atomic_req.clone())?;
        Ok(())
    }
}

impl drm::Device for Card {}
impl drm::control::Device for Card {}

impl Drop for BackendImp {
    fn drop(&mut self) {
        self.card.destroy_framebuffer(self.fb_swapchain[0]).unwrap();
        self.card.destroy_framebuffer(self.fb_swapchain[1]).unwrap();
        match &self.renderer_kind {
            RendererKind::Pixman { swapchain, .. } => {
                self.card.destroy_dumb_buffer(swapchain[0]).unwrap();
                self.card.destroy_dumb_buffer(swapchain[1]).unwrap();
            }
            RendererKind::OpenGl {
                swapchain, state, ..
            } => {
                swapchain[0].destroy(state.gl());
                swapchain[1].destroy(state.gl());
            }
        }

        let id = self.card.id.take().unwrap();
        self.seat.close_device(id).unwrap();
    }
}

const DRM: u32 = 0;
const LIBSEAT: u32 = 1;
const LIBINPUT: u32 = 2;

impl Backend for BackendImp {
    fn register_fds_with(
        &self,
        reg: &'_ mut dyn FnMut(RawFd, u32) -> io::Result<()>,
    ) -> io::Result<()> {
        reg(self.card.fd.as_raw_fd(), DRM)?;
        reg(self.seat.get_fd().unwrap().as_raw_fd(), LIBSEAT)?;
        reg(self.libinput.as_raw_fd(), LIBINPUT)?;
        Ok(())
    }

    fn poll(&mut self, data: u32) -> io::Result<()> {
        match data {
            DRM => {
                for event in self.card.receive_events().unwrap() {
                    match event {
                        drm::control::Event::Vblank(_) => todo!("vblank"),
                        drm::control::Event::PageFlip(_event) => {
                            self.backend_events_queue.push_back(BackendEvent::Frame);
                        }
                        drm::control::Event::Unknown(_) => todo!("unknown"),
                    }
                }
            }
            LIBSEAT => {
                self.seat.dispatch(0).unwrap();
                while let Some(seat_event) = self.seat.next_event() {
                    match seat_event {
                        libseat::Event::Enable => {
                            eprintln!("seat enabled");
                            if self.suspended {
                                self.card.reset_crtcs().expect("could not reset CRTCs");
                                self.atomic_req.add_property(
                                    self.plane,
                                    self.plane_props["FB_ID"].handle(),
                                    drm::control::property::Value::Framebuffer(Some(
                                        self.fb_swapchain[0],
                                    )),
                                );
                                self.card
                                    .atomic_commit(
                                        AtomicCommitFlags::ALLOW_MODESET
                                            | AtomicCommitFlags::PAGE_FLIP_EVENT,
                                        self.atomic_req.clone(),
                                    )
                                    .expect("Failed to set mode");
                                self.libinput.resume().unwrap();
                                self.suspended = false;
                            }
                        }
                        libseat::Event::Disable => {
                            eprintln!("seat disabled");
                            self.seat.disable().unwrap();
                            self.libinput.suspend();
                            self.suspended = true;
                        }
                    }
                }
            }
            LIBINPUT => {
                self.libinput.dispatch().unwrap();
                for event in &mut self.libinput {
                    match event {
                        input::Event::Device(_) => (),
                        input::Event::Keyboard(e) => {
                            let input::event::KeyboardEvent::Key(e) = e else { continue };
                            let key = e.key();
                            let timestamp = InputTimestamp(e.time());
                            self.backend_events_queue.push_back(
                                if e.key_state() == input::event::keyboard::KeyState::Pressed {
                                    BackendEvent::KeyPressed(
                                        KeyboardId(NonZeroU64::MIN),
                                        timestamp,
                                        key,
                                    )
                                } else {
                                    BackendEvent::KeyReleased(
                                        KeyboardId(NonZeroU64::MIN),
                                        timestamp,
                                        key,
                                    )
                                },
                            );
                        }
                        input::Event::Pointer(e) => {
                            let timestamp = InputTimestamp(e.time());
                            match e {
                                input::event::PointerEvent::Motion(e) => {
                                    self.backend_events_queue.push_back(
                                        BackendEvent::PointerMotionRelative(
                                            PointerId(NonZeroU64::MIN),
                                            timestamp,
                                            e.dx() as f32,
                                            e.dy() as f32,
                                        ),
                                    );
                                }
                                // input::event::PointerEvent::MotionAbsolute(_) => todo!(),
                                input::event::PointerEvent::Button(e) => {
                                    let btn = e.button();
                                    self.backend_events_queue.push_back(
                                        if e.button_state()
                                            == input::event::pointer::ButtonState::Pressed
                                        {
                                            BackendEvent::PointerBtnPress(
                                                PointerId(NonZeroU64::MIN),
                                                timestamp,
                                                btn,
                                            )
                                        } else {
                                            BackendEvent::PointerBtnRelease(
                                                PointerId(NonZeroU64::MIN),
                                                timestamp,
                                                btn,
                                            )
                                        },
                                    );
                                }
                                // input::event::PointerEvent::Axis(_) => todo!(),
                                input::event::PointerEvent::ScrollWheel(scroll_wheel) => {
                                    assert!(!scroll_wheel
                                        .has_axis(input::event::pointer::Axis::Horizontal));
                                    let value = scroll_wheel
                                        .has_axis(input::event::pointer::Axis::Vertical)
                                        .then(|| {
                                            scroll_wheel
                                                .scroll_value(input::event::pointer::Axis::Vertical)
                                        })
                                        .unwrap_or(0.0);
                                    self.backend_events_queue.push_back(
                                        BackendEvent::PointerAxisVertial(
                                            PointerId(NonZeroU64::MIN),
                                            timestamp,
                                            value as f32,
                                        ),
                                    );
                                }
                                // input::event::PointerEvent::ScrollFinger(_) => todo!(),
                                // input::event::PointerEvent::ScrollContinuous(_) => todo!(),
                                _ => (),
                            }
                        }
                        input::Event::Touch(_) => (),
                        input::Event::Tablet(_) => (),
                        input::Event::TabletPad(_) => (),
                        input::Event::Gesture(_) => (),
                        input::Event::Switch(_) => (),
                        _ => (),
                    }
                }
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn next_event(&mut self) -> Option<BackendEvent> {
        self.backend_events_queue.pop_front()
    }

    fn switch_vt(&mut self, vt: u32) {
        self.seat.switch_session(vt as i32).unwrap();
    }

    fn renderer_state(&mut self) -> &mut dyn RendererState {
        match &mut self.renderer_kind {
            RendererKind::Pixman { state, .. } => state,
            RendererKind::OpenGl { state, .. } => state,
        }
    }

    fn render_frame(&mut self, clear: Color, render_list: &[RenderNode], time: u32) {
        if self.suspended {
            return;
        }

        match &mut self.renderer_kind {
            RendererKind::Pixman {
                swapchain,
                state,
                temp_buf,
            } => {
                self.fb_swapchain.swap(0, 1);
                swapchain.swap(0, 1);

                let (width, height) = swapchain[0].size();
                const FORMAT: wl_shm::Format = wl_shm::Format::Xrgb8888;

                let mut frame = state.frame(temp_buf, width, height, FORMAT);
                frame.clear(clear.r, clear.g, clear.a);
                frame.render(render_list, time);
                drop(frame);

                // Reading from mapped buffer is terribly slow, but required for blending.
                // When blending is involved, rendering to a CPU buffer and then copying is much faster.
                {
                    let mut map = self
                        .card
                        .map_dumb_buffer(&mut swapchain[1])
                        .expect("Could not map dumbbuffer");
                    map.copy_from_slice(temp_buf);
                }
            }
            RendererKind::OpenGl {
                width,
                height,
                swapchain,
                state,
            } => {
                self.fb_swapchain.swap(0, 1);
                swapchain.swap(0, 1);
                let mut frame = state.frame(*width, *height, &swapchain[1]);
                frame.clear(clear.r, clear.g, clear.b);
                frame.render(render_list, time);
                drop(frame);
                state.finish_frame();
            }
        }

        let mut atomic_req = AtomicModeReq::new();
        atomic_req.add_property(
            self.plane,
            self.plane_props["FB_ID"].handle(),
            drm::control::property::Value::Framebuffer(Some(self.fb_swapchain[1])),
        );
        if let Err(e) = self.card.atomic_commit(
            AtomicCommitFlags::PAGE_FLIP_EVENT | AtomicCommitFlags::NONBLOCK,
            atomic_req,
        ) {
            eprintln!("drmkms: atomic nonblock page flip failed: {e:?}");
        };
    }
}

struct PlanarBufer {
    width: u32,
    height: u32,
    export: eglgbm::BufferExport,
}

impl drm::buffer::PlanarBuffer for PlanarBufer {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn format(&self) -> DrmFourcc {
        self.export.format.0.try_into().unwrap()
    }

    fn modifier(&self) -> Option<drm::buffer::DrmModifier> {
        Some(self.export.modifier.into())
    }

    fn pitches(&self) -> [u32; 4] {
        [
            self.export.planes[0].stride,
            self.export.planes.get(1).map_or(0, |x| x.stride),
            self.export.planes.get(2).map_or(0, |x| x.stride),
            self.export.planes.get(3).map_or(0, |x| x.stride),
        ]
    }

    fn handles(&self) -> [Option<drm::buffer::Handle>; 4] {
        [
            bytemuck::cast(self.export.planes[0].handle),
            self.export
                .planes
                .get(1)
                .and_then(|x| bytemuck::cast(x.handle)),
            self.export
                .planes
                .get(2)
                .and_then(|x| bytemuck::cast(x.handle)),
            self.export
                .planes
                .get(3)
                .and_then(|x| bytemuck::cast(x.handle)),
        ]
    }

    fn offsets(&self) -> [u32; 4] {
        [
            self.export.planes[0].offset,
            self.export.planes.get(1).map_or(0, |x| x.offset),
            self.export.planes.get(2).map_or(0, |x| x.offset),
            self.export.planes.get(3).map_or(0, |x| x.offset),
        ]
    }
}

fn parse_drm_format_modifier_blob(blob: &[u8]) -> HashMap<eglgbm::Fourcc, Vec<u64>> {
    /*
    struct drm_format_modifier_blob {
        __u32 version;
        __u32 flags;
        __u32 count_formats;
        __u32 formats_offset;
        __u32 count_modifiers;
        __u32 modifiers_offset;
        // __u32 formats[]
        // struct drm_format_modifier modifiers[]
    };

    struct drm_format_modifier {
        // Bitmask of formats in get_plane format list this info applies to. The
        // offset allows a sliding window of which 64 formats (bits).
        //
        // Some examples:
        // In today's world with < 65 formats, and formats 0, and 2 are
        // supported
        // 0x0000000000000005
        //		  ^-offset = 0, formats = 5
        //
        // If the number formats grew to 128, and formats 98-102 are
        // supported with the modifier:
        //
        // 0x0000007c00000000 0000000000000000
        //		  ^
        //		  |__offset = 64, formats = 0x7c00000000
        //
        __u64 formats;
        __u32 offset;
        __u32 pad;
        __u64 modifier;
    };
    */

    let (_version, rest) = blob.split_first_chunk::<4>().unwrap();
    let (_flags, rest) = rest.split_first_chunk::<4>().unwrap();
    let (count_formats, rest) = rest.split_first_chunk::<4>().unwrap();
    let (formats_offset, rest) = rest.split_first_chunk::<4>().unwrap();
    let (count_modifiers, rest) = rest.split_first_chunk::<4>().unwrap();
    let (modifiers_offset, _rest) = rest.split_first_chunk::<4>().unwrap();

    let count_formats = u32::from_ne_bytes(*count_formats) as usize;
    let formats_offset = u32::from_ne_bytes(*formats_offset) as usize;
    let count_modifiers = u32::from_ne_bytes(*count_modifiers) as usize;
    let modifiers_offset = u32::from_ne_bytes(*modifiers_offset) as usize;

    let mut formats = Vec::new();
    for i in 0..count_formats {
        let format = &blob[formats_offset + i * 4..][..4];
        let format = u32::from_ne_bytes(format.try_into().unwrap());
        formats.push(format);
    }

    let mut map = HashMap::<_, Vec<u64>>::new();

    for i in 0..count_modifiers {
        let formats_mask = &blob[modifiers_offset + i * 24..][..8];
        let formats_mask = u64::from_ne_bytes(formats_mask.try_into().unwrap());

        let offset = &blob[modifiers_offset + 8 + i * 24..][..4];
        let offset = u32::from_ne_bytes(offset.try_into().unwrap());

        let modifier = &blob[modifiers_offset + 16 + i * 24..][..8];
        let modifier = u64::from_ne_bytes(modifier.try_into().unwrap());

        for i in 0..64 {
            if formats_mask & (1u64 << i) != 0 {
                let format = formats[(i + offset) as usize];
                map.entry(eglgbm::Fourcc(format))
                    .or_default()
                    .push(modifier);
            }
        }
    }

    map
}

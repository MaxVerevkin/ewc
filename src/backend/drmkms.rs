use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::Path;
use std::rc::Rc;

use drm::buffer::{Buffer as _, DrmFourcc};
use drm::control::atomic::AtomicModeReq;
use drm::control::dumbbuffer::DumbBuffer;
use drm::control::{AtomicCommitFlags, Device as _};
use drm::Device as _;
use input::event::keyboard::KeyboardEventTrait;
use input::event::pointer::PointerScrollEvent;
use input::Libinput;

use super::pixman_renderer::*;
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

    // Load the information.
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
        .find(|i| i.state() == drm::control::connector::State::Connected)
        .expect("No connected connectors");
    let mode = *con.modes().first().expect("No modes found on connector");
    let (disp_width, disp_height) = mode.size();
    dbg!(disp_width, disp_height);

    let crtc = crtcinfo.first().expect("No crtcs found");

    let buf = card
        .create_dumb_buffer(
            (disp_width as u32, disp_height as u32),
            DrmFourcc::Xrgb8888,
            32,
        )
        .expect("Could not create dumb buffer");
    let buf2 = card
        .create_dumb_buffer(
            (disp_width as u32, disp_height as u32),
            DrmFourcc::Xrgb8888,
            32,
        )
        .expect("Could not create dumb buffer");
    let fb = card
        .add_framebuffer(&buf, 24, 32)
        .expect("Could not create FB");
    let fb2 = card
        .add_framebuffer(&buf2, 24, 32)
        .expect("Could not create FB");

    let planes = card.plane_handles().expect("Could not list planes");
    let (better_planes, compatible_planes): (
        Vec<drm::control::plane::Handle>,
        Vec<drm::control::plane::Handle>,
    ) = planes
        .iter()
        .filter(|&&plane| {
            card.get_plane(plane).is_ok_and(|plane_info| {
                let compatible_crtcs = res.filter_crtcs(plane_info.possible_crtcs());
                compatible_crtcs.contains(&crtc.handle())
            })
        })
        .partition(|&&plane| {
            if let Ok(props) = card.get_properties(plane) {
                for (&id, &val) in props.iter() {
                    if let Ok(info) = card.get_property(id) {
                        if info.name().to_str().is_ok_and(|x| x == "type") {
                            return val == (drm::control::PlaneType::Primary as u32).into();
                        }
                    }
                }
            }
            false
        });
    let plane = *better_planes.first().unwrap_or(&compatible_planes[0]);

    // println!("{:#?}", mode);
    // println!("{:#?}", fb);
    // println!("{:#?}", buf);
    // println!("{:#?}", plane);

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
        .get_properties(plane)
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
        plane,
        plane_props["FB_ID"].handle(),
        drm::control::property::Value::Framebuffer(Some(fb)),
    );
    atomic_req.add_property(
        plane,
        plane_props["CRTC_ID"].handle(),
        drm::control::property::Value::CRTC(Some(crtc.handle())),
    );
    atomic_req.add_property(
        plane,
        plane_props["SRC_X"].handle(),
        drm::control::property::Value::UnsignedRange(0),
    );
    atomic_req.add_property(
        plane,
        plane_props["SRC_Y"].handle(),
        drm::control::property::Value::UnsignedRange(0),
    );
    atomic_req.add_property(
        plane,
        plane_props["SRC_W"].handle(),
        drm::control::property::Value::UnsignedRange((mode.size().0 as u64) << 16),
    );
    atomic_req.add_property(
        plane,
        plane_props["SRC_H"].handle(),
        drm::control::property::Value::UnsignedRange((mode.size().1 as u64) << 16),
    );
    atomic_req.add_property(
        plane,
        plane_props["CRTC_X"].handle(),
        drm::control::property::Value::SignedRange(0),
    );
    atomic_req.add_property(
        plane,
        plane_props["CRTC_Y"].handle(),
        drm::control::property::Value::SignedRange(0),
    );
    atomic_req.add_property(
        plane,
        plane_props["CRTC_W"].handle(),
        drm::control::property::Value::UnsignedRange(mode.size().0 as u64),
    );
    atomic_req.add_property(
        plane,
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
        buf_front: buf,
        buf_back: buf2,
        temp_buf: vec![0u8; disp_width as usize * disp_height as usize * 4],
        fb_front: fb,
        fb_back: fb2,
        atomic_req,
        plane,
        plane_props,
        backend_events_queue: VecDeque::new(),
        renderer_state: RendererStateImp::new(),
    }))
}

struct BackendImp {
    suspended: bool,
    card: Card,
    seat: Rc<libseat::Seat>,
    libinput: Libinput,
    buf_front: DumbBuffer,
    buf_back: DumbBuffer,
    temp_buf: Vec<u8>,
    fb_front: drm::control::framebuffer::Handle,
    fb_back: drm::control::framebuffer::Handle,
    atomic_req: AtomicModeReq,
    plane: drm::control::plane::Handle,
    plane_props: HashMap<String, drm::control::property::Info>,
    backend_events_queue: VecDeque<BackendEvent>,
    renderer_state: RendererStateImp,
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
}

impl drm::Device for Card {}
impl drm::control::Device for Card {}

impl Drop for BackendImp {
    fn drop(&mut self) {
        self.card.destroy_framebuffer(self.fb_back).unwrap();
        self.card.destroy_dumb_buffer(self.buf_back).unwrap();
        self.card.destroy_framebuffer(self.fb_front).unwrap();
        self.card.destroy_dumb_buffer(self.buf_front).unwrap();

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
                                self.atomic_req.add_property(
                                    self.plane,
                                    self.plane_props["FB_ID"].handle(),
                                    drm::control::property::Value::Framebuffer(Some(self.fb_front)),
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
                            self.backend_events_queue.push_back(
                                if e.key_state() == input::event::keyboard::KeyState::Pressed {
                                    BackendEvent::KeyPressed(KeyboardId(NonZeroU64::MIN), key)
                                } else {
                                    BackendEvent::KeyReleased(KeyboardId(NonZeroU64::MIN), key)
                                },
                            );
                        }
                        input::Event::Pointer(e) => match e {
                            input::event::PointerEvent::Motion(e) => {
                                self.backend_events_queue.push_back(
                                    BackendEvent::PointerMotionRelative(
                                        PointerId(NonZeroU64::MIN),
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
                                            btn,
                                        )
                                    } else {
                                        BackendEvent::PointerBtnRelease(
                                            PointerId(NonZeroU64::MIN),
                                            btn,
                                        )
                                    },
                                );
                            }
                            // input::event::PointerEvent::Axis(_) => todo!(),
                            input::event::PointerEvent::ScrollWheel(scroll_wheel) => {
                                assert!(
                                    !scroll_wheel.has_axis(input::event::pointer::Axis::Horizontal)
                                );
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
                                        value as f32,
                                    ),
                                );
                            }
                            // input::event::PointerEvent::ScrollFinger(_) => todo!(),
                            // input::event::PointerEvent::ScrollContinuous(_) => todo!(),
                            _ => (),
                        },
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
        &mut self.renderer_state
    }

    fn render_frame(&mut self, f: &mut dyn FnMut(&mut dyn Frame)) {
        if self.suspended {
            return;
        }

        std::mem::swap(&mut self.fb_front, &mut self.fb_back);
        std::mem::swap(&mut self.buf_front, &mut self.buf_back);

        let (width, height) = self.buf_front.size();
        const FORMAT: wl_shm::Format = wl_shm::Format::Xrgb8888;

        f(self
            .renderer_state
            .frame(&mut self.temp_buf, width, height, FORMAT as u32)
            .as_mut());

        // Reading from mapped buffer is terribly slow, but required for blending.
        // When blending is involved, rendering to a CPU buffer and then copying is much faster.
        {
            let mut map = self
                .card
                .map_dumb_buffer(&mut self.buf_back)
                .expect("Could not map dumbbuffer");
            map.copy_from_slice(&self.temp_buf);
        }

        let mut atomic_req = AtomicModeReq::new();
        atomic_req.add_property(
            self.plane,
            self.plane_props["FB_ID"].handle(),
            drm::control::property::Value::Framebuffer(Some(self.fb_back)),
        );
        if let Err(e) = self.card.atomic_commit(
            AtomicCommitFlags::PAGE_FLIP_EVENT | AtomicCommitFlags::NONBLOCK,
            atomic_req,
        ) {
            eprintln!("drmkms: atomic nonblock page flip failed: {e:?}");
        };
    }
}

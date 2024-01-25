use std::collections::{HashMap, VecDeque};
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::Path;
use std::rc::Rc;

use drm::buffer::{Buffer as _, DrmFourcc};
use drm::control::dumbbuffer::DumbBuffer;
use drm::control::{self, atomic, property, AtomicCommitFlags, Device as _};
use drm::control::{connector, crtc};
use drm::Device as _;
use input::event::keyboard::KeyboardEventTrait;
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
    let coninfo: Vec<connector::Info> = res
        .connectors()
        .iter()
        .flat_map(|con| card.get_connector(*con, true))
        .collect();
    let crtcinfo: Vec<crtc::Info> = res
        .crtcs()
        .iter()
        .flat_map(|crtc| card.get_crtc(*crtc))
        .collect();

    let con = coninfo
        .iter()
        .find(|i| i.state() == connector::State::Connected)
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
    let fb = card
        .add_framebuffer(&buf, 24, 32)
        .expect("Could not create FB");

    let planes = card.plane_handles().expect("Could not list planes");
    let (better_planes, compatible_planes): (
        Vec<control::plane::Handle>,
        Vec<control::plane::Handle>,
    ) = planes
        .iter()
        .filter(|&&plane| {
            card.get_plane(plane)
                .map(|plane_info| {
                    let compatible_crtcs = res.filter_crtcs(plane_info.possible_crtcs());
                    compatible_crtcs.contains(&crtc.handle())
                })
                .unwrap_or(false)
        })
        .partition(|&&plane| {
            if let Ok(props) = card.get_properties(plane) {
                for (&id, &val) in props.iter() {
                    if let Ok(info) = card.get_property(id) {
                        if info.name().to_str().map(|x| x == "type").unwrap_or(false) {
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

    let mut atomic_req = atomic::AtomicModeReq::new();
    atomic_req.add_property(
        con.handle(),
        con_props["CRTC_ID"].handle(),
        property::Value::CRTC(Some(crtc.handle())),
    );
    let blob = card
        .create_property_blob(&mode)
        .expect("Failed to create blob");
    atomic_req.add_property(crtc.handle(), crtc_props["MODE_ID"].handle(), blob);
    atomic_req.add_property(
        crtc.handle(),
        crtc_props["ACTIVE"].handle(),
        property::Value::Boolean(true),
    );
    atomic_req.add_property(
        plane,
        plane_props["FB_ID"].handle(),
        property::Value::Framebuffer(Some(fb)),
    );
    atomic_req.add_property(
        plane,
        plane_props["CRTC_ID"].handle(),
        property::Value::CRTC(Some(crtc.handle())),
    );
    atomic_req.add_property(
        plane,
        plane_props["SRC_X"].handle(),
        property::Value::UnsignedRange(0),
    );
    atomic_req.add_property(
        plane,
        plane_props["SRC_Y"].handle(),
        property::Value::UnsignedRange(0),
    );
    atomic_req.add_property(
        plane,
        plane_props["SRC_W"].handle(),
        property::Value::UnsignedRange((mode.size().0 as u64) << 16),
    );
    atomic_req.add_property(
        plane,
        plane_props["SRC_H"].handle(),
        property::Value::UnsignedRange((mode.size().1 as u64) << 16),
    );
    atomic_req.add_property(
        plane,
        plane_props["CRTC_X"].handle(),
        property::Value::SignedRange(0),
    );
    atomic_req.add_property(
        plane,
        plane_props["CRTC_Y"].handle(),
        property::Value::SignedRange(0),
    );
    atomic_req.add_property(
        plane,
        plane_props["CRTC_W"].handle(),
        property::Value::UnsignedRange(mode.size().0 as u64),
    );
    atomic_req.add_property(
        plane,
        plane_props["CRTC_H"].handle(),
        property::Value::UnsignedRange(mode.size().1 as u64),
    );

    // Set the crtc
    // On many setups, this requires root access.
    card.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, atomic_req.clone())
        .expect("Failed to set mode");

    // HACK: poor man's vsync
    let (frame_pipe, frame_notify) = crate::pipe().unwrap();
    std::thread::spawn(move || {
        let mut file = std::fs::File::from(frame_notify);
        loop {
            file.write_all(&[0]).unwrap();
            #[allow(deprecated)]
            std::thread::sleep_ms(30);
        }
    });

    Some(Box::new(BackendImp {
        card,
        seat,
        libinput,
        buf,
        buf_data: vec![0u8; disp_width as usize * disp_height as usize * 4],
        fb,
        frame_pipe,
        backend_events_queue: VecDeque::new(),
        renderer_state: RendererState::new(),
    }))
}

struct BackendImp {
    card: Card,
    seat: Rc<libseat::Seat>,
    libinput: Libinput,
    buf: DumbBuffer,
    buf_data: Vec<u8>,
    fb: drm::control::framebuffer::Handle,
    frame_pipe: OwnedFd,
    backend_events_queue: VecDeque<BackendEvent>,
    renderer_state: RendererState,
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
        self.card.destroy_framebuffer(self.fb).unwrap();
        self.card.destroy_dumb_buffer(self.buf).unwrap();

        let id = self.card.id.take().unwrap();
        self.seat.close_device(id).unwrap();
    }
}

const FRAME_PIPE: u32 = 0;
const LIBSEAT: u32 = 1;
const LIBINPUT: u32 = 2;

impl Backend for BackendImp {
    fn register_fds_with(
        &self,
        reg: &'_ mut dyn FnMut(RawFd, u32) -> io::Result<()>,
    ) -> io::Result<()> {
        reg(self.frame_pipe.as_raw_fd(), FRAME_PIPE)?;
        reg(self.seat.get_fd().unwrap().as_raw_fd(), LIBSEAT)?;
        reg(self.libinput.as_raw_fd(), LIBINPUT)?;
        Ok(())
    }

    fn poll(&mut self, data: u32) -> io::Result<()> {
        match data {
            FRAME_PIPE => {
                let mut buf = [0u8; 16];
                assert!(
                    unsafe {
                        libc::read(
                            self.frame_pipe.as_raw_fd(),
                            buf.as_mut_ptr().cast(),
                            buf.len(),
                        )
                    } > 0
                );
                self.backend_events_queue.push_back(BackendEvent::Frame);
            }
            LIBSEAT => {
                self.seat.dispatch(0).unwrap();
                while let Some(seat_event) = self.seat.next_event() {
                    match seat_event {
                        libseat::Event::Enable => {
                            eprintln!("seat enabled");
                        }
                        libseat::Event::Disable => {
                            self.seat.disable().unwrap();
                            todo!("seat disabled");
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
                            // input::event::PointerEvent::ScrollWheel(_) => todo!(),
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

    fn create_shm_pool(&mut self, fd: OwnedFd, size: usize) -> ShmPoolId {
        self.renderer_state.create_shm_pool(fd, size)
    }

    fn resize_shm_pool(&mut self, pool_id: ShmPoolId, new_size: usize) {
        self.renderer_state.resize_shm_pool(pool_id, new_size)
    }

    fn shm_pool_resource_destroyed(&mut self, pool_id: ShmPoolId) {
        self.renderer_state.shm_pool_resource_destroyed(pool_id)
    }

    fn create_shm_buffer(
        &mut self,
        pool_id: ShmPoolId,
        offset: usize,
        wl_format: u32,
        width: u32,
        height: u32,
        stride: u32,
        resource: crate::protocol::WlBuffer,
    ) -> BufferId {
        self.renderer_state
            .create_shm_buffer(pool_id, offset, wl_format, width, height, stride, resource)
    }

    fn get_buffer_size(&self, buffer_id: BufferId) -> (u32, u32) {
        self.renderer_state.get_buffer_size(buffer_id)
    }

    fn buffer_lock(&mut self, buffer_id: BufferId) {
        self.renderer_state.buffer_lock(buffer_id)
    }

    fn buffer_unlock(&mut self, buffer_id: BufferId) {
        self.renderer_state.buffer_unlock(buffer_id)
    }

    fn buffer_resource_destroyed(&mut self, buffer_id: BufferId) {
        self.renderer_state.buffer_resource_destroyed(buffer_id)
    }

    fn render_frame(&mut self, f: &mut dyn FnMut(&mut dyn Frame)) {
        let (width, height) = self.buf.size();
        const FORMAT: wl_shm::Format = wl_shm::Format::Argb8888;
        f(&mut FrameImp {
            time: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u32,
            width,
            height,
            renderer: Renderer::new(
                &self.renderer_state,
                &mut self.buf_data,
                width,
                height,
                FORMAT as u32,
            ),
        });

        let mut map = self
            .card
            .map_dumb_buffer(&mut self.buf)
            .expect("Could not map dumbbuffer");
        unsafe {
            libc::memcpy(
                map.as_mut_ptr().cast(),
                self.buf_data.as_mut_ptr().cast(),
                self.buf_data.len(),
            )
        };
    }
}

struct FrameImp<'a> {
    time: u32,
    width: u32,
    height: u32,
    renderer: super::pixman_renderer::Renderer<'a>,
}

impl Frame for FrameImp<'_> {
    fn time(&self) -> u32 {
        self.time
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn clear(&mut self, r: f32, g: f32, b: f32) {
        self.renderer.clear(r, g, b);
    }

    fn render_buffer(
        &mut self,
        buf: BufferId,
        opaque_region: Option<&pixman::Region32>,
        alpha: f32,
        x: i32,
        y: i32,
    ) {
        self.renderer.render_buffer(buf, opaque_region, alpha, x, y);
    }

    fn render_rect(&mut self, r: f32, g: f32, b: f32, a: f32, x: i32, y: i32, w: u32, h: u32) {
        self.renderer.render_rect(r, g, b, a, x, y, w, h);
    }
}

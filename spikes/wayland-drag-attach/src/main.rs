use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode,
    UinputAbsSetup,
};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::data_device_manager::data_device::{DataDevice, DataDeviceHandler};
use smithay_client_toolkit::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use smithay_client_toolkit::data_device_manager::data_source::{DataSourceHandler, DragSource};
use smithay_client_toolkit::data_device_manager::{DataDeviceManagerState, WritePipe};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::xdg::window::{Window, WindowConfigure, WindowDecorations, WindowHandler};
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_data_device, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm, delegate_xdg_shell, delegate_xdg_window,
    registry_handlers,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::wl_data_device_manager::DndAction;
use wayland_client::protocol::{wl_data_device, wl_data_source, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface};
use wayland_client::{Connection, QueueHandle};

const BTN_LEFT: u16 = 0x110;
const BTN_TASK: u16 = 0x117;
const ORIGIN_SIZE: u32 = 200;
const PHASE_MAPPED: u8 = 1;
const PHASE_DRAGGING: u8 = 2;
const PHASE_DONE: u8 = 3;

static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn log(msg: impl AsRef<str>) {
    let elapsed = START.get_or_init(Instant::now).elapsed();
    eprintln!("[{:7.3}] {}", elapsed.as_secs_f64(), msg.as_ref());
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Layer,
    Toplevel,
}

#[derive(Clone, Debug)]
struct Config {
    mode: Mode,
    origin: (i32, i32),
    target: (i32, i32),
    screen: (i32, i32),
    files: Vec<PathBuf>,
    keep_input: bool,
    shots: Option<PathBuf>,
    timeout: Duration,
    post_click: Option<(i32, i32)>,
}

fn parse_args() -> Result<Config> {
    let mut args = std::env::args().skip(1);
    let mut cfg = Config {
        mode: Mode::Layer,
        origin: (1500, 400),
        target: (1500, 1300),
        screen: (2560, 2160),
        files: Vec::new(),
        keep_input: false,
        shots: None,
        timeout: Duration::from_secs(20),
        post_click: None,
    };
    let pair = |args: &mut dyn Iterator<Item = String>| -> Result<(i32, i32)> {
        let a = args.next().context("missing x")?.parse()?;
        let b = args.next().context("missing y")?.parse()?;
        Ok((a, b))
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mode" => {
                cfg.mode = match args.next().as_deref() {
                    Some("layer") => Mode::Layer,
                    Some("toplevel") => Mode::Toplevel,
                    other => return Err(anyhow!("unknown mode {other:?}")),
                }
            }
            "--origin" => cfg.origin = pair(&mut args)?,
            "--target" => cfg.target = pair(&mut args)?,
            "--screen" => cfg.screen = pair(&mut args)?,
            "--keep-input" => cfg.keep_input = true,
            "--post-click" => cfg.post_click = Some(pair(&mut args)?),
            "--shots" => cfg.shots = Some(PathBuf::from(args.next().context("missing dir")?)),
            "--timeout" => cfg.timeout = Duration::from_secs(args.next().context("missing secs")?.parse()?),
            path => cfg.files.push(std::fs::canonicalize(path).with_context(|| format!("file {path}"))?),
        }
    }
    if cfg.files.is_empty() {
        return Err(anyhow!("pass at least one file to offer"));
    }
    Ok(cfg)
}

fn uri_list(files: &[PathBuf]) -> String {
    let mut out = String::new();
    for path in files {
        out.push_str("file://");
        for byte in path.to_string_lossy().bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => out.push(byte as char),
                _ => out.push_str(&format!("%{byte:02X}")),
            }
        }
        out.push_str("\r\n");
    }
    out
}

enum Origin {
    Layer(LayerSurface),
    Toplevel(Window),
}

impl Origin {
    fn surface(&self) -> &wl_surface::WlSurface {
        match self {
            Origin::Layer(layer) => layer.wl_surface(),
            Origin::Toplevel(window) => window.wl_surface(),
        }
    }
}

struct State {
    cfg: Config,
    phase: Arc<AtomicU8>,
    registry: RegistryState,
    outputs: OutputState,
    seats: SeatState,
    compositor: CompositorState,
    shm: Shm,
    pool: SlotPool,
    data_device_manager: DataDeviceManagerState,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    data_device: Option<DataDevice>,
    origin: Origin,
    size: (u32, u32),
    drag: Option<DragSource>,
    outcome: Option<Result<(), String>>,
    running: bool,
}

impl State {
    fn paint(&mut self) {
        let (w, h) = self.size;
        let stride = w as i32 * 4;
        let Ok((buffer, canvas)) = self.pool.create_buffer(w as i32, h as i32, stride, wl_shm::Format::Argb8888) else {
            log("cannot allocate buffer");
            return;
        };
        for px in canvas.chunks_exact_mut(4) {
            px.copy_from_slice(&[0x00, 0x00, 0x60, 0x60]);
        }
        let surface = self.origin.surface();
        if buffer.attach_to(surface).is_err() {
            log("cannot attach buffer");
            return;
        }
        surface.damage_buffer(0, 0, w as i32, h as i32);
        surface.commit();
        if self.phase.load(Ordering::Acquire) < PHASE_MAPPED {
            self.phase.store(PHASE_MAPPED, Ordering::Release);
            log(format!("origin surface mapped at {}x{}", w, h));
        }
    }

    fn start_drag(&mut self, qh: &QueueHandle<Self>, serial: u32) {
        let Some(device) = &self.data_device else {
            log("no data device yet");
            return;
        };
        let source = self.data_device_manager.create_drag_and_drop_source(qh, ["text/uri-list"], DndAction::Copy);
        source.start_drag(device, self.origin.surface(), None, serial);
        log(format!("start_drag sent with serial {serial}"));
        self.drag = Some(source);
        if !self.cfg.keep_input {
            match Region::new(&self.compositor) {
                Ok(region) => {
                    let surface = self.origin.surface();
                    surface.set_input_region(Some(region.wl_region()));
                    surface.commit();
                    log("origin input region emptied so the drag routes to windows beneath");
                }
                Err(err) => log(format!("cannot create region: {err}")),
            }
        }
        self.phase.store(PHASE_DRAGGING, Ordering::Release);
    }

    fn finish(&mut self, outcome: Result<(), String>) {
        if self.outcome.is_none() {
            self.outcome = Some(outcome);
        }
        self.phase.store(PHASE_DONE, Ordering::Release);
        self.running = false;
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for State {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        log("layer surface closed by compositor");
        self.finish(Err("layer surface closed".into()));
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface, configure: LayerSurfaceConfigure, _: u32) {
        let (w, h) = configure.new_size;
        if w > 0 && h > 0 {
            self.size = (w, h);
        }
        log(format!("layer configure {:?}", configure.new_size));
        self.paint();
    }
}

impl WindowHandler for State {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.finish(Err("toplevel close requested".into()));
    }
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window, configure: WindowConfigure, _: u32) {
        let w = configure.new_size.0.map(|v| v.get()).unwrap_or(self.cfg.screen.0 as u32);
        let h = configure.new_size.1.map(|v| v.get()).unwrap_or(self.cfg.screen.1 as u32);
        self.size = (w, h);
        log(format!("toplevel configure {:?} state {:?}", configure.new_size, configure.state));
        self.paint();
    }
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }
    fn new_seat(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if self.seat.is_none() {
            self.data_device = Some(self.data_device_manager.get_data_device(qh, &seat));
            self.seat = Some(seat);
            log("seat found, data device created");
        }
    }
    fn new_capability(&mut self, conn: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        if self.seat.is_none() {
            self.new_seat(conn, qh, seat.clone());
        }
        if self.seat.as_ref() != Some(&seat) {
            return;
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            match self.seats.get_pointer(qh, &seat) {
                Ok(pointer) => {
                    self.pointer = Some(pointer);
                    log("pointer capability acquired");
                }
                Err(err) => log(format!("cannot get pointer: {err}")),
            }
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Pointer {
            self.pointer = None;
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for State {
    fn pointer_frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for event in events {
            if &event.surface != self.origin.surface() {
                continue;
            }
            match &event.kind {
                PointerEventKind::Enter { serial } => log(format!("pointer entered origin at {:?} serial {serial}", event.position)),
                PointerEventKind::Leave { serial } => log(format!("pointer left origin serial {serial}")),
                PointerEventKind::Press { button, serial, .. } => {
                    log(format!("button {button:#x} pressed on origin serial {serial}"));
                    if *button == u32::from(BTN_LEFT) && self.drag.is_none() {
                        self.start_drag(qh, *serial);
                    }
                }
                PointerEventKind::Release { button, serial, .. } => log(format!("button {button:#x} released on origin serial {serial}")),
                PointerEventKind::Motion { .. } => {}
                PointerEventKind::Axis { .. } => {}
            }
        }
    }
}

impl DataDeviceHandler for State {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_device::WlDataDevice, x: f64, y: f64, _: &wl_surface::WlSurface) {
        log(format!("data device: drag entered our own surface at {x:.0},{y:.0}"));
    }
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_device::WlDataDevice) {
        log("data device: drag left our surface");
    }
    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_device::WlDataDevice, _: f64, _: f64) {}
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_device::WlDataDevice) {}
    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_device::WlDataDevice) {
        log("data device: drop performed on our own surface (unexpected)");
    }
}

impl DataOfferHandler for State {
    fn source_actions(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, actions: DndAction) {
        log(format!("offer source actions {actions:?}"));
    }
    fn selected_action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, actions: DndAction) {
        log(format!("offer selected action {actions:?}"));
    }
}

impl DataSourceHandler for State {
    fn accept_mime(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_source::WlDataSource, mime: Option<String>) {
        log(format!("destination accepts mime {mime:?}"));
    }
    fn send_request(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_source::WlDataSource, mime: String, fd: WritePipe) {
        let body = uri_list(&self.cfg.files);
        let mut file = std::fs::File::from(std::os::fd::OwnedFd::from(fd));
        let result = file.write_all(body.as_bytes()).and_then(|_| file.flush());
        log(format!("send_request for {mime}: wrote {} bytes ({result:?})", body.len()));
    }
    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_source::WlDataSource) {
        log("data source cancelled by compositor");
        self.finish(Err("drag cancelled".into()));
    }
    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_source::WlDataSource) {
        log("dnd_dropped: destination accepted the drop");
    }
    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_source::WlDataSource) {
        log("dnd_finished: destination finished reading");
        self.finish(Ok(()));
    }
    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_data_source::WlDataSource, action: DndAction) {
        log(format!("compositor selected action {action:?}"));
    }
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(State);
delegate_output!(State);
delegate_shm!(State);
delegate_seat!(State);
delegate_pointer!(State);
delegate_layer!(State);
delegate_xdg_shell!(State);
delegate_xdg_window!(State);
delegate_data_device!(State);
delegate_registry!(State);

fn abs_coords(screen: (i32, i32), p: (i32, i32)) -> (i32, i32) {
    let scale = |v: i32, extent: i32| ((f64::from(v) + 0.5) * 65536.0 / f64::from(extent)).floor().clamp(0.0, 65535.0) as i32;
    (scale(p.0, screen.0), scale(p.1, screen.1))
}

fn shot(dir: &Option<PathBuf>, name: &str) {
    if let Some(dir) = dir {
        let path = dir.join(format!("{name}.png"));
        let status = std::process::Command::new("spectacle")
            .args(["-b", "-n", "-o"])
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        log(format!("screenshot {name}: {status:?}"));
    }
}

fn injector(cfg: Config, phase: Arc<AtomicU8>) -> Result<()> {
    let mut buttons = AttributeSet::<KeyCode>::new();
    for code in BTN_LEFT..=BTN_TASK {
        buttons.insert(KeyCode::new(code));
    }
    let axis = AbsInfo::new(0, 0, 65535, 0, 0, 0);
    let mut device = VirtualDevice::builder()?
        .name("Splice Spike Pointer")
        .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x5350, 0x0099, 1))
        .with_keys(&buttons)?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_X, axis))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_Y, axis))?
        .build()?;
    log("virtual pointer created; waiting for udev and the compositor to pick it up");
    std::thread::sleep(Duration::from_millis(1500));
    let wait_for = |target: u8, limit: Duration| {
        let deadline = Instant::now() + limit;
        while phase.load(Ordering::Acquire) < target && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        phase.load(Ordering::Acquire) >= target
    };
    if !wait_for(PHASE_MAPPED, Duration::from_secs(5)) {
        return Err(anyhow!("origin surface never mapped"));
    }
    let move_to = |device: &mut VirtualDevice, p: (i32, i32)| -> Result<()> {
        let (x, y) = abs_coords(cfg.screen, p);
        device.emit(&[
            InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_X.0, x),
            InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Y.0, y),
        ])?;
        Ok(())
    };
    move_to(&mut device, (cfg.origin.0 - 3, cfg.origin.1 - 3))?;
    std::thread::sleep(Duration::from_millis(120));
    move_to(&mut device, cfg.origin)?;
    std::thread::sleep(Duration::from_millis(300));
    log(format!("injecting BTN_LEFT press at {:?}", cfg.origin));
    device.emit(&[InputEvent::new(EventType::KEY.0, BTN_LEFT, 1)])?;
    if !wait_for(PHASE_DRAGGING, Duration::from_secs(2)) {
        log("drag did not start within 2s of the press; continuing anyway");
    }
    std::thread::sleep(Duration::from_millis(250));
    shot(&cfg.shots, "1-after-start");
    let steps = 40;
    for i in 1..=steps {
        let t = f64::from(i) / f64::from(steps);
        let p = (
            cfg.origin.0 + ((cfg.target.0 - cfg.origin.0) as f64 * t) as i32,
            cfg.origin.1 + ((cfg.target.1 - cfg.origin.1) as f64 * t) as i32,
        );
        move_to(&mut device, p)?;
        std::thread::sleep(Duration::from_millis(15));
        if i == steps / 2 {
            shot(&cfg.shots, "2-mid-drag");
        }
    }
    std::thread::sleep(Duration::from_millis(400));
    shot(&cfg.shots, "3-over-target");
    log(format!("injecting BTN_LEFT release at {:?}", cfg.target));
    device.emit(&[InputEvent::new(EventType::KEY.0, BTN_LEFT, 0)])?;
    std::thread::sleep(Duration::from_millis(400));
    if let Some(click) = cfg.post_click {
        std::thread::sleep(Duration::from_millis(700));
        shot(&cfg.shots, "4-drop-menu");
        move_to(&mut device, (click.0 - 2, click.1 - 2))?;
        std::thread::sleep(Duration::from_millis(120));
        move_to(&mut device, click)?;
        std::thread::sleep(Duration::from_millis(250));
        log(format!("injecting follow-up click at {click:?}"));
        device.emit(&[InputEvent::new(EventType::KEY.0, BTN_LEFT, 1)])?;
        std::thread::sleep(Duration::from_millis(90));
        device.emit(&[InputEvent::new(EventType::KEY.0, BTN_LEFT, 0)])?;
        std::thread::sleep(Duration::from_millis(1500));
        shot(&cfg.shots, "5-after-click");
    }
    log("injector finished");
    Ok(())
}

fn main() -> Result<()> {
    START.get_or_init(Instant::now);
    let cfg = parse_args()?;
    log(format!("config {cfg:?}"));
    let conn = Connection::connect_to_env().context("connect to Wayland")?;
    let (globals, queue) = registry_queue_init::<State>(&conn).context("registry")?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).context("wl_compositor")?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm")?;
    let pool = SlotPool::new((cfg.screen.0 * cfg.screen.1 * 4) as usize, &shm).context("shm pool")?;
    let data_device_manager = DataDeviceManagerState::bind(&globals, &qh).context("wl_data_device_manager")?;
    let surface = compositor.create_surface(&qh);
    let origin = match cfg.mode {
        Mode::Layer => {
            let layer_shell = LayerShell::bind(&globals, &qh).context("zwlr_layer_shell_v1 (not on GNOME)")?;
            let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("splice-spike-origin"), None);
            layer.set_anchor(Anchor::TOP | Anchor::LEFT);
            layer.set_size(ORIGIN_SIZE, ORIGIN_SIZE);
            layer.set_exclusive_zone(-1);
            let half = ORIGIN_SIZE as i32 / 2;
            layer.set_margin(cfg.origin.1 - half, 0, 0, cfg.origin.0 - half);
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            layer.commit();
            Origin::Layer(layer)
        }
        Mode::Toplevel => {
            let xdg = XdgShell::bind(&globals, &qh).context("xdg_wm_base")?;
            let window = xdg.create_window(surface, WindowDecorations::RequestClient, &qh);
            window.set_title("Splice spike drag origin");
            window.set_app_id("dev.splice.spike.origin");
            window.set_fullscreen(None);
            window.commit();
            Origin::Toplevel(window)
        }
    };
    let phase = Arc::new(AtomicU8::new(0));
    let mut state = State {
        cfg: cfg.clone(),
        phase: phase.clone(),
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
        seats: SeatState::new(&globals, &qh),
        compositor,
        shm,
        pool,
        data_device_manager,
        seat: None,
        pointer: None,
        data_device: None,
        origin,
        size: (ORIGIN_SIZE, ORIGIN_SIZE),
        drag: None,
        outcome: None,
        running: true,
    };
    for seat in state.seats.seats().collect::<Vec<_>>() {
        state.new_seat(&conn, &qh, seat);
    }
    let mut event_loop: EventLoop<State> = EventLoop::try_new().context("event loop")?;
    WaylandSource::new(conn.clone(), queue).insert(event_loop.handle()).map_err(|e| anyhow!("wayland source: {e}"))?;
    let timeout = cfg.timeout;
    event_loop
        .handle()
        .insert_source(Timer::from_duration(timeout), move |_, _, state: &mut State| {
            log("timeout reached");
            state.finish(Err("timeout".into()));
            TimeoutAction::Drop
        })
        .map_err(|e| anyhow!("timer: {e}"))?;
    let injector_cfg = cfg.clone();
    let injector_phase = phase.clone();
    let injector = std::thread::spawn(move || injector(injector_cfg, injector_phase));
    while state.running || !injector.is_finished() {
        event_loop.dispatch(Some(Duration::from_millis(50)), &mut state).context("dispatch")?;
        if injector.is_finished() && state.drag.is_none() && state.phase.load(Ordering::Acquire) < PHASE_DRAGGING {
            state.finish(Err("injector finished before a drag started".into()));
        }
    }
    let _ = conn.flush();
    match injector.join() {
        Ok(Ok(())) => {}
        Ok(Err(err)) => log(format!("injector error: {err:#}")),
        Err(_) => log("injector thread panicked"),
    }
    match state.outcome.take().unwrap_or(Err("no outcome".into())) {
        Ok(()) => {
            log("RESULT: OK, the injected press started a native drag that another client accepted");
            Ok(())
        }
        Err(reason) => {
            log(format!("RESULT: FAILED ({reason})"));
            std::process::exit(2);
        }
    }
}

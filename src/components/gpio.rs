// GPIO Factory helper (port of moonraker/components/gpio.py)
//
// python-periphery is replaced by direct use of the GPIO character device
// v1 ABI (the same ABI periphery uses).

use crate::common::ServerError;
use crate::confighelper::{ConfigError, ConfigHelper};
use crate::server::{server, spawn, Component};
use async_trait::async_trait;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::os::fd::RawFd;
use std::rc::Rc;
use tokio::io::unix::AsyncFd;

const GPIOHANDLE_REQUEST_INPUT: u32 = 1;
const GPIOHANDLE_REQUEST_OUTPUT: u32 = 2;
const GPIOHANDLE_REQUEST_ACTIVE_LOW: u32 = 4;
const GPIOHANDLE_REQUEST_BIAS_PULL_UP: u32 = 32;
const GPIOHANDLE_REQUEST_BIAS_PULL_DOWN: u32 = 64;
const GPIOHANDLE_REQUEST_BIAS_DISABLE: u32 = 128;
const GPIOEVENT_REQUEST_BOTH_EDGES: u32 = 3;
const GPIOEVENT_EVENT_RISING_EDGE: u32 = 1;
const GPIOEVENT_EVENT_FALLING_EDGE: u32 = 2;
const MAX_ERRORS: u32 = 50;
const ERROR_RESET_TIME: f64 = 5.0;

#[repr(C)]
struct GpioHandleRequest {
    lineoffsets: [u32; 64],
    flags: u32,
    default_values: [u8; 64],
    consumer_label: [u8; 32],
    lines: u32,
    fd: i32,
}

#[repr(C)]
struct GpioHandleData {
    values: [u8; 64],
}

#[repr(C)]
struct GpioEventRequest {
    lineoffset: u32,
    handleflags: u32,
    eventflags: u32,
    consumer_label: [u8; 32],
    fd: i32,
}

#[repr(C)]
struct GpioEventData {
    timestamp: u64,
    id: u32,
}

const fn iowr(nr: u64, size: usize) -> u64 {
    (3 << 30) | ((size as u64) << 16) | (0xB4 << 8) | nr
}

const GPIO_GET_LINEHANDLE_IOCTL: u64 = iowr(0x03, std::mem::size_of::<GpioHandleRequest>());
const GPIO_GET_LINEEVENT_IOCTL: u64 = iowr(0x04, std::mem::size_of::<GpioEventRequest>());
const GPIOHANDLE_GET_LINE_VALUES_IOCTL: u64 = iowr(0x08, std::mem::size_of::<GpioHandleData>());
const GPIOHANDLE_SET_LINE_VALUES_IOCTL: u64 = iowr(0x09, std::mem::size_of::<GpioHandleData>());

fn label() -> [u8; 32] {
    let mut l = [0u8; 32];
    l[..9].copy_from_slice(b"moonraker");
    l
}

fn kernel_version() -> (i64, i64) {
    // SAFETY: a C struct of byte arrays: all zero is a valid value
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: uts is a valid, exclusively borrowed utsname for uname to fill
    if unsafe { libc::uname(&mut uts) } != 0 {
        return (0, 0);
    }
    // SAFETY: uname succeeded, so release is NUL-terminated within the array, which outlives the CStr
    let rel = unsafe { std::ffi::CStr::from_ptr(uts.release.as_ptr()) }.to_string_lossy().to_string();
    let parts: Vec<Option<i64>> = rel.split('.').take(2).map(|p| p.parse().ok()).collect();
    match (parts.first().copied().flatten(), parts.get(1).copied().flatten()) {
        (Some(a), Some(b)) => (a, b),
        _ => (0, 0),
    }
}

fn io_err(msg: &str) -> ServerError {
    ServerError::new(format!("{}: {}", msg, std::io::Error::last_os_error()), 500)
}

struct PinParams {
    inverted: bool,
    pin_id: u32,
    chip_id: String,
    full_name: String,
    initial_value: i32,
    bias: u32,
    output_high: bool,
}

/// A GPIO's names (GpioBase): the pin as configured, its full name, and
/// whether it is inverted
pub struct PinInfo {
    pub orig: String,
    pub name: String,
    pub inverted: bool,
}

/// closes a line fd once
fn close_fd(fd: &Cell<RawFd>) {
    let f = fd.replace(-1);
    if f >= 0 {
        // SAFETY: f was taken out of the cell (replaced by -1), so it is closed once
        unsafe { libc::close(f) };
    }
}

pub struct GpioOutputPin {
    pub info: PinInfo,
    fd: Cell<RawFd>,
    value: Cell<i32>,
}

impl GpioOutputPin {
    pub fn write(&self, value: i32) -> Result<(), ServerError> {
        let v = (value != 0) as i32;
        let mut data = GpioHandleData { values: [0; 64] };
        data.values[0] = v as u8;
        // SAFETY: the line handle fd is open (closing sets it to -1, which the kernel rejects); the request encodes size_of::<GpioHandleData>() and data is exclusively borrowed
        if unsafe { libc::ioctl(self.fd.get(), GPIOHANDLE_SET_LINE_VALUES_IOCTL as _, &mut data) } < 0 {
            return Err(io_err("Setting line value"));
        }
        self.value.set(v);
        Ok(())
    }
    pub fn get_value(&self) -> i32 {
        self.value.get()
    }
    pub fn close(&self) {
        close_fd(&self.fd);
    }
}

pub type GpioEventCallback = Rc<dyn Fn(f64, f64, i32)>;

pub struct GpioEvent {
    pub name: String,
    pub info: PinInfo,
    fd: RawFd,
    closed: Cell<bool>,
    value: Cell<i32>,
    callback: GpioEventCallback,
    on_error: RefCell<Option<Rc<dyn Fn(String)>>>,
    debounce_period: Cell<f64>,
    last_event_time: Cell<f64>,
    error_count: Cell<u32>,
    last_error_reset: Cell<f64>,
    reader: crate::server::TaskSlot,
    debounce_task: crate::server::TaskSlot,
}

impl GpioEvent {
    pub fn setup_debounce(&self, period: f64, err_cb: Option<Rc<dyn Fn(String)>>) {
        self.debounce_period.set(period.max(0.0));
        *self.on_error.borrow_mut() = err_cb;
    }

    pub fn get_value(&self) -> i32 {
        self.value.get()
    }

    pub fn fileno(&self) -> RawFd {
        self.fd
    }

    fn read_value(&self) -> i32 {
        let mut data = GpioHandleData { values: [0; 64] };
        // SAFETY: the event fd is open until close(); the request encodes size_of::<GpioHandleData>() and data is exclusively borrowed
        unsafe { libc::ioctl(self.fd, GPIOHANDLE_GET_LINE_VALUES_IOCTL as _, &mut data) };
        data.values[0] as i32
    }

    pub fn start(self: &Rc<Self>) {
        if self.reader.is_set() {
            return;
        }
        self.value.set(self.read_value());
        self.last_event_time.set(crate::utils::monotonic());
        let w = Rc::downgrade(self);
        let fd = self.fd;
        let h = spawn(async move {
            // SAFETY: the event fd is only closed by close() at shutdown, after
            // stop(); nothing registers a new fd after that
            let afd = match unsafe { AsyncFd::register(fd) } {
                Ok(a) => a,
                Err(e) => {
                    log::error!("GPIO: unable to watch event fd: {}", e);
                    return;
                }
            };
            loop {
                let mut guard = match afd.readable().await {
                    Ok(g) => g,
                    Err(_) => return,
                };
                let mut evt = GpioEventData { timestamp: 0, id: 0 };
                // SAFETY: evt is a #[repr(C)] gpioevent_data (16 bytes, the kernel's layout) and the length is its size; fd is open while the reader task runs
                let n = unsafe {
                    libc::read(fd, &mut evt as *mut _ as *mut libc::c_void, std::mem::size_of::<GpioEventData>())
                };
                if n < 0 {
                    guard.clear_ready();
                    continue;
                }
                match w.upgrade() {
                    Some(e) => e.on_event_trigger(evt.id),
                    None => return,
                }
            }
        });
        self.reader.set(h);
        log::debug!("GPIO {}: Listening for events, current state: {}", self.name, self.value.get());
    }

    pub fn stop(&self) {
        self.debounce_task.cancel();
        self.reader.cancel();
    }

    fn on_event_trigger(self: &Rc<Self>, id: u32) {
        let last_value = self.value.get();
        match id {
            GPIOEVENT_EVENT_RISING_EDGE => self.value.set(1),
            GPIOEVENT_EVENT_FALLING_EDGE => self.value.set(0),
            _ => return,
        }
        if self.debounce_period.get() > 0.0 {
            if !self.debounce_task.is_set() {
                let w = Rc::downgrade(self);
                let period = self.debounce_period.get();
                let h = spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs_f64(period)).await;
                    if let Some(e) = w.upgrade() {
                        e.debounce_task.clear();
                        if last_value != e.value.get() {
                            e.run_callback();
                        }
                    }
                });
                self.debounce_task.set(h);
            } else {
                self.increment_error();
            }
        } else if last_value != self.value.get() {
            self.run_callback();
        }
    }

    fn run_callback(&self) {
        let eventtime = crate::utils::monotonic();
        let duration = eventtime - self.last_event_time.get();
        self.last_event_time.set(eventtime);
        (self.callback)(eventtime, duration, self.value.get());
    }

    fn increment_error(&self) {
        let eventtime = crate::utils::monotonic();
        if eventtime - self.last_error_reset.get() > ERROR_RESET_TIME {
            self.error_count.set(0);
            self.last_error_reset.set(eventtime);
        }
        self.error_count.set(self.error_count.get() + 1);
        if self.error_count.get() >= MAX_ERRORS {
            self.stop();
            if let Some(cb) = self.on_error.borrow().clone() {
                cb(format!("Too Many Consecutive Errors, GPIO Event Disabled on {}", self.name));
            }
        }
    }

    pub fn close(&self) {
        self.stop();
        if !self.closed.replace(true) {
            // SAFETY: the closed flag makes this run once; the reader task was stopped first
            unsafe { libc::close(self.fd) };
        }
    }
}

enum Reserved {
    Out(Rc<GpioOutputPin>),
    Event(Rc<GpioEvent>),
}

pub struct GpioFactory {
    reserved: RefCell<HashMap<String, Reserved>>,
}

impl GpioFactory {
    pub fn new(_config: &ConfigHelper) -> Rc<Self> {
        Rc::new(GpioFactory { reserved: RefCell::new(HashMap::new()) })
    }

    fn parse_pin(&self, pin_desc: &str, initial_value: i32, is_event: bool) -> Result<PinParams, ServerError> {
        let re = regex::Regex::new(r"^(?P<bias>[~^])?(?P<inverted>!)?(?:(?P<chip_id>gpiochip[0-9]+)/)?(?P<pin_name>gpio(?P<pin_id>[0-9]+))")
            .unwrap();
        let c = re.captures(pin_desc).ok_or_else(|| {
            ServerError::bad(format!(
                "Invalid pin format {}. Refer to the configuration documentation for details on the pin format.",
                pin_desc
            ))
        })?;
        let inverted = c.name("inverted").is_some();
        let bias_flag = c.name("bias").map(|m| m.as_str().to_string());
        let mut bias = 0;
        let mut output_high = false;
        if is_event {
            bias = match bias_flag.as_deref() {
                Some("^") => GPIOHANDLE_REQUEST_BIAS_PULL_UP,
                Some("~") => GPIOHANDLE_REQUEST_BIAS_PULL_DOWN,
                _ if kernel_version() >= (5, 5) => GPIOHANDLE_REQUEST_BIAS_DISABLE,
                _ => 0,
            };
        } else {
            if let Some(b) = bias_flag {
                return Err(ServerError::bad(format!(
                    "Invalid pin format {}.  Bias flag {} not available for output pins.",
                    pin_desc, b
                )));
            }
            output_high = (initial_value != 0) ^ inverted;
        }
        let chip_id = c.name("chip_id").map(|m| m.as_str()).unwrap_or("gpiochip0").to_string();
        let pin_name = c["pin_name"].to_string();
        Ok(PinParams {
            inverted,
            pin_id: c["pin_id"].parse().unwrap_or(0),
            full_name: format!("{}:{}", chip_id, pin_name),
            chip_id,
            initial_value,
            bias,
            output_high,
        })
    }

    fn open_chip(&self, p: &PinParams) -> Result<RawFd, ServerError> {
        if self.reserved.borrow().contains_key(&p.full_name) {
            return Err(ServerError::bad(format!("GPIO {} already reserved", p.full_name)));
        }
        let chip_path = std::path::Path::new("/dev").join(&p.chip_id);
        if !chip_path.exists() {
            return Err(ServerError::bad(format!("Chip path {} does not exist", chip_path.display())));
        }
        let cpath = std::ffi::CString::new(chip_path.to_string_lossy().as_bytes()).unwrap();
        // SAFETY: cpath is a NUL-terminated CString alive for the call
        let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR) };
        if fd < 0 {
            return Err(io_err("Opening GPIO chip"));
        }
        Ok(fd)
    }

    fn init_failed(full_name: &str) {
        log::error!(
            "Unable to init {}.  Make sure the gpio is not in use by another program or exported by sysfs.",
            full_name
        );
    }

    pub fn setup_gpio_out(&self, pin_name: &str, initial_value: i32) -> Result<Rc<GpioOutputPin>, ServerError> {
        let initial_value = (initial_value != 0) as i32;
        let p = self.parse_pin(pin_name, initial_value, false)?;
        let chip = self.open_chip(&p)?;
        let mut req = GpioHandleRequest {
            lineoffsets: [0; 64],
            flags: GPIOHANDLE_REQUEST_OUTPUT | if p.inverted { GPIOHANDLE_REQUEST_ACTIVE_LOW } else { 0 },
            default_values: [0; 64],
            consumer_label: label(),
            lines: 1,
            fd: -1,
        };
        req.lineoffsets[0] = p.pin_id;
        req.default_values[0] = p.output_high as u8;
        // SAFETY: chip is the open chip fd; the request encodes size_of::<GpioHandleRequest>() (#[repr(C)], the kernel's layout) and req is exclusively borrowed
        let r = unsafe { libc::ioctl(chip, GPIO_GET_LINEHANDLE_IOCTL as _, &mut req) };
        let err = io_err("Opening output GPIO");
        // SAFETY: the chip fd was opened by open_chip and is closed once, after its last use
        unsafe { libc::close(chip) };
        if r < 0 {
            Self::init_failed(&p.full_name);
            return Err(err);
        }
        let pin = Rc::new(GpioOutputPin {
            info: PinInfo { orig: pin_name.to_string(), name: p.full_name.clone(), inverted: p.inverted },
            fd: Cell::new(req.fd),
            value: Cell::new(p.initial_value),
        });
        self.reserved.borrow_mut().insert(p.full_name, Reserved::Out(pin.clone()));
        Ok(pin)
    }

    pub fn register_gpio_event(&self, pin_name: &str, callback: GpioEventCallback) -> Result<Rc<GpioEvent>, ServerError> {
        let p = self.parse_pin(pin_name, 0, true)?;
        let chip = self.open_chip(&p)?;
        let mut req = GpioEventRequest {
            lineoffset: p.pin_id,
            handleflags: GPIOHANDLE_REQUEST_INPUT | p.bias | if p.inverted { GPIOHANDLE_REQUEST_ACTIVE_LOW } else { 0 },
            eventflags: GPIOEVENT_REQUEST_BOTH_EDGES,
            consumer_label: label(),
            fd: -1,
        };
        // SAFETY: chip is the open chip fd; the request encodes size_of::<GpioEventRequest>() (#[repr(C)], the kernel's layout) and req is exclusively borrowed
        let r = unsafe { libc::ioctl(chip, GPIO_GET_LINEEVENT_IOCTL as _, &mut req) };
        let err = io_err("Opening input GPIO");
        // SAFETY: the chip fd was opened by open_chip and is closed once, after its last use
        unsafe { libc::close(chip) };
        if r < 0 {
            Self::init_failed(&p.full_name);
            return Err(err);
        }
        // SAFETY: req.fd is the event fd the kernel just returned; fcntl takes no pointers
        unsafe {
            let flags = libc::fcntl(req.fd, libc::F_GETFL);
            libc::fcntl(req.fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let evt = Rc::new(GpioEvent {
            name: p.full_name.clone(),
            info: PinInfo { orig: pin_name.to_string(), name: p.full_name.clone(), inverted: p.inverted },
            fd: req.fd,
            closed: Cell::new(false),
            value: Cell::new(0),
            callback,
            on_error: RefCell::new(None),
            debounce_period: Cell::new(0.0),
            last_event_time: Cell::new(0.0),
            error_count: Cell::new(0),
            last_error_reset: Cell::new(0.0),
            reader: Default::default(),
            debounce_task: Default::default(),
        });
        self.reserved.borrow_mut().insert(p.full_name, Reserved::Event(evt.clone()));
        Ok(evt)
    }
}

#[async_trait(?Send)]
impl Component for GpioFactory {
    async fn close(self: Rc<Self>) {
        for (_, r) in self.reserved.borrow().iter() {
            match r {
                Reserved::Out(p) => p.close(),
                Reserved::Event(e) => e.close(),
            }
        }
    }
}

fn factory(config: &ConfigHelper, option: &str) -> Result<Rc<GpioFactory>, ConfigError> {
    let srv = server();
    let err = || {
        ConfigHelper::error(format!("Section [{}], option '{}', GPIO Component not available", config.get_name(), option))
    };
    srv.load_component(config, "gpio").map_err(|_| err())?;
    srv.get::<crate::components::gpio::GpioFactory>().ok_or_else(err)
}

fn parse_err(config: &ConfigHelper, option: &str, msg: &str) -> ConfigError {
    ConfigHelper::error(format!("[{}]: Option '{}' encountered the following error while parsing: {}", config.get_name(), option, msg))
}

/// ConfigHelper.getgpioout
pub fn getgpioout(config: &ConfigHelper, option: &str, initial_value: i32) -> Result<Rc<GpioOutputPin>, ConfigError> {
    let f = factory(config, option)?;
    let val = config.get(option)?;
    f.setup_gpio_out(&val, initial_value).map_err(|e| parse_err(config, option, &e.message))
}

/// ConfigHelper.getgpioevent
pub fn getgpioevent(config: &ConfigHelper, option: &str, cb: GpioEventCallback) -> Result<Rc<GpioEvent>, ConfigError> {
    let f = factory(config, option)?;
    let val = config.get(option)?;
    f.register_gpio_event(&val, cb).map_err(|e| parse_err(config, option, &e.message))
}

pub fn load_component(config: &ConfigHelper) -> Rc<GpioFactory> {
    GpioFactory::new(config)
}

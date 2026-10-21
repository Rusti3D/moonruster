// Support for interfacing with TD-1 devices (port of moonraker/components/td1.py)

use crate::async_serial::AsyncSerial;
use crate::common::*;
use crate::confighelper::{ConfigError, ConfigHelper};
use crate::server::{server, spawn, Component};
use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

const TD1_VID: u32 = 0xE4B2;
const TD1_PID: u32 = 0x0045;

fn utc_now_iso() -> String {
    // datetime.now(tz=utc).isoformat() + "Z"
    format!("{}Z", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.6f+00:00"))
}

/// serial.tools.list_ports.comports() filtered to TD1 devices
fn find_td1_devices() -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut names: Vec<String> = std::fs::read_dir("/sys/class/tty")
        .map(|r| r.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect())
        .unwrap_or_default();
    names.sort();
    let prefixes = ["ttyS", "ttyUSB", "ttyXRUSB", "ttyACM", "ttyAMA", "rfcomm", "ttyAP", "ttyGS"];
    for name in names {
        if !prefixes.iter().any(|p| name.starts_with(p)) {
            continue;
        }
        let dev = match std::fs::canonicalize(Path::new("/sys/class/tty").join(&name).join("device")) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let subsystem = std::fs::canonicalize(dev.join("subsystem"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_default();
        let iface: PathBuf = match subsystem.as_str() {
            "usb-serial" => match dev.parent() {
                Some(p) => p.to_path_buf(),
                None => continue,
            },
            "usb" => dev.clone(),
            _ => continue,
        };
        let usb_dev = match iface.parent() {
            Some(p) => p,
            None => continue,
        };
        let read = |f: &str| std::fs::read_to_string(usb_dev.join(f)).ok().map(|s| s.trim().to_string());
        let vid = read("idVendor").and_then(|v| u32::from_str_radix(&v, 16).ok());
        let pid = read("idProduct").and_then(|v| u32::from_str_radix(&v, 16).ok());
        if vid == Some(TD1_VID) && pid == Some(TD1_PID) {
            if let Some(sn) = read("serial").filter(|s| !s.is_empty()) {
                found.push((format!("/dev/{}", name), sn));
            }
        }
    }
    found
}

struct Td1Connection {
    serial_number: String,
    serial: Rc<AsyncSerial>,
    error_state: Cell<bool>,
    enabled: Cell<bool>,
    serial_task: RefCell<Option<tokio::task::JoinHandle<()>>>,
    td: Cell<Option<f64>>,
    color: RefCell<Option<String>>,
    scan_time: RefCell<String>,
    error: RefCell<Option<String>>,
    done_initializing: Cell<bool>,
}

impl Td1Connection {
    fn new(config: &ConfigHelper, serial_number: &str, port: &str) -> Result<Rc<Self>, ConfigError> {
        let baud = config.getint_or("baudrate", 115200)?;
        let name = format!("{}-{}", config.get_name(), serial_number);
        Ok(Rc::new(Td1Connection {
            serial_number: serial_number.to_string(),
            serial: AsyncSerial::new(&name, port, baud),
            error_state: Cell::new(false),
            enabled: Cell::new(true),
            serial_task: RefCell::new(None),
            td: Cell::new(None),
            color: RefCell::new(None),
            scan_time: RefCell::new(utc_now_iso()),
            error: RefCell::new(None),
            done_initializing: Cell::new(false),
        }))
    }

    async fn initialize(self: &Rc<Self>) {
        let c = self.clone();
        *self.serial_task.borrow_mut() = Some(spawn(async move { c.run_serial().await }));
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if self.serial.connected() {
                break;
            }
        }
    }

    async fn readline_with_timeout(&self, timeout: f64) -> Option<Vec<u8>> {
        tokio::time::timeout(Duration::from_secs_f64(timeout), self.serial.read_line()).await.ok().flatten()
    }

    async fn run_serial(self: Rc<Self>) {
        if let Err(e) = self.serial.open(true) {
            self.error_state.set(true);
            log::error!("Error trying to open TD1 serial: {}\n{}", self.serial_number, e);
            return;
        }
        if let Err(e) = self.check_error_file().await {
            log::error!("{}", e);
            return;
        }
        self.done_initializing.set(true);
        while self.enabled.get() {
            let line = match self.serial.read_line().await {
                None => break,
                Some(l) => l,
            };
            let line = String::from_utf8_lossy(&line).trim_end().to_string();
            self.parse_line(&line);
            log::debug!("{}: {}", self.serial_number, line);
        }
    }

    fn parse_line(&self, line: &str) {
        let parts: Vec<&str> = line.split(',').collect();
        if parts.len() >= 6 {
            match parts[4].trim().parse::<f64>() {
                Ok(td) => {
                    self.td.set(Some(td));
                    *self.color.borrow_mut() = Some(parts[5].trim().to_string());
                    *self.scan_time.borrow_mut() = utc_now_iso();
                }
                Err(_) => log::error!("Parse error from {}: could not convert string to float: '{}'", self.serial_number, parts[4]),
            }
        }
    }

    async fn check_error_file(&self) -> Result<(), String> {
        let file_name = "errors.txt";
        let mut ack: Option<String> = None;
        while ack.as_deref() != Some("ready") && ack.as_deref() != Some("No file named errors.txt") && self.enabled.get() {
            if ack.is_none() {
                log::debug!("{}: Sending retrieve file", self.serial_number);
                let _ = self.serial.send(b"retrieve file\n".to_vec()).await;
                let _ = self.serial.send(format!("{}\n", file_name).into_bytes()).await;
            }
            match self.readline_with_timeout(5.0).await {
                Some(line) => {
                    let a = String::from_utf8_lossy(&line).trim().to_string();
                    log::debug!("{}: {}", self.serial_number, a);
                    ack = Some(a);
                }
                None => {
                    log::debug!("{}: Timed out", self.serial_number);
                    ack = None;
                }
            }
        }
        log::debug!("{}: Done checking for error file", self.serial_number);
        if ack.as_deref() == Some("No file named errors.txt") || !self.enabled.get() {
            return Ok(());
        }
        let read_int = |d: Option<Vec<u8>>| -> Result<i64, String> {
            let s = String::from_utf8_lossy(&d.unwrap_or_default()).trim().to_string();
            s.parse::<i64>().map_err(|_| format!("invalid literal for int() with base 10: '{}'", s))
        };
        let file_size = read_int(self.serial.read_line().await)?;
        let blk_count = read_int(self.serial.read_line().await)?;
        log::debug!("Receiving file: {}, Size: {} bytes, Blocks: {}", file_name, file_size, blk_count);
        let mut content = String::new();
        for _ in 0..blk_count {
            let _ = self.serial.read_line().await;
            let data = self.serial.read_line().await.unwrap_or_default();
            content.push_str(&String::from_utf8_lossy(&data));
            let _ = self.serial.send(b"ready\n".to_vec()).await;
        }
        log::info!("{}: Error file content: {}", self.serial_number, content);
        log::info!("{}: File '{}' received successfully.", self.serial_number, file_name);
        *self.error.borrow_mut() = Some(content);
        Ok(())
    }

    async fn close(&self) {
        self.enabled.set(false);
        let _ = tokio::time::timeout(Duration::from_secs(1), self.serial.close()).await;
        let t = self.serial_task.borrow_mut().take();
        if let Some(t) = t {
            let ab = t.abort_handle();
            if tokio::time::timeout(Duration::from_secs(1), t).await.is_err() {
                ab.abort();
            }
        }
    }

    async fn reboot(&self) {
        log::info!("Rebooting {}", self.serial_number);
        self.enabled.set(false);
        // set_read_callback(None, force=True) ends the reader loop
        let t = self.serial_task.borrow_mut().take();
        if let Some(t) = t {
            t.abort();
        }
        let _ = self.serial.send(b"change settings\n".to_vec()).await;
        let ack = self.serial.read_line().await.unwrap_or_default();
        if String::from_utf8_lossy(&ack).trim() == "ready" {
            let _ = self.serial.send(b"RGB_Enabled = True\ndone\n".to_vec()).await;
        }
        let _ = self.serial.read_line().await;
    }

    fn get_data(&self) -> Map<String, Value> {
        let mut m = Map::new();
        if !self.done_initializing.get() {
            return m;
        }
        m.insert(
            self.serial_number.clone(),
            json!({
                "td": self.td.get(),
                "color": *self.color.borrow(),
                "scan_time": *self.scan_time.borrow(),
                "error": *self.error.borrow(),
            }),
        );
        m
    }
}

pub struct Td1 {
    config: ConfigHelper,
    enabled: Cell<bool>,
    watch_task: RefCell<Option<tokio::task::JoinHandle<()>>>,
    start_task: RefCell<Option<tokio::task::JoinHandle<()>>>,
    conns: RefCell<IndexMap<String, Rc<Td1Connection>>>,
}

impl Td1 {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, ConfigError> {
        let srv = server();
        let t = Rc::new(Td1 {
            config: config.clone(),
            enabled: Cell::new(true),
            watch_task: RefCell::new(None),
            start_task: RefCell::new(None),
            conns: RefCell::new(IndexMap::new()),
        });
        let tt = t.clone();
        let _ = srv.register_endpoint(
            "/machine/td1/data",
            RequestType::GET,
            callback(move |_req| {
                let t = tt.clone();
                async move {
                    let mut latest = Map::new();
                    for c in t.conns.borrow().values() {
                        latest.extend(c.get_data());
                    }
                    Ok(json!({"status": "ok", "devices": latest}))
                }
            }),
        );
        let tt = t.clone();
        let _ = srv.register_endpoint(
            "/machine/td1/reboot",
            RequestType::POST,
            callback(move |req| {
                let t = tt.clone();
                async move {
                    let sn = req.get_str("serial")?;
                    let conn = t.conns.borrow().get(&sn).cloned();
                    match conn {
                        None => Ok(json!({"status": "serial_error"})),
                        Some(c) => {
                            c.reboot().await;
                            c.close().await;
                            t.conns.borrow_mut().shift_remove(&sn);
                            Ok(json!({"status": "ok"}))
                        }
                    }
                }
            }),
        );
        Ok(t)
    }

    async fn start_serial_task(&self, port: &str, serial_number: &str) {
        let conn = match Td1Connection::new(&self.config, serial_number, port) {
            Ok(c) => c,
            Err(e) => {
                log::error!("{}", e.0);
                return;
            }
        };
        conn.initialize().await;
        if !conn.error_state.get() {
            self.conns.borrow_mut().insert(serial_number.to_string(), conn);
            log::info!("TD1 listening on {} (Serial: {})", port, serial_number);
        } else {
            log::error!("Failed to open port:{} for TD1 serial {}", port, serial_number);
        }
    }
}

#[async_trait(?Send)]
impl Component for Td1 {
    fn has_init(&self) -> bool {
        true
    }

    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        log::debug!("Initializing TD1");
        let t = self.clone();
        let start = spawn(async move {
            let devices = find_td1_devices();
            if devices.is_empty() {
                log::warn!("No TD1 devices found.");
                return;
            }
            for (port, sn) in devices {
                t.start_serial_task(&port, &sn).await;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
        *self.start_task.borrow_mut() = Some(start);
        let w = Rc::downgrade(&self);
        let watch = spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let t = match w.upgrade() {
                    Some(t) if t.enabled.get() => t,
                    _ => return,
                };
                let mut current: Vec<String> = Vec::new();
                for (port, sn) in find_td1_devices() {
                    current.push(sn.clone());
                    if !t.conns.borrow().contains_key(&sn) {
                        log::info!("Hot-plugged TD1 detected: {}", sn);
                        t.start_serial_task(&port, &sn).await;
                    }
                }
                let known: Vec<String> = t.conns.borrow().keys().cloned().collect();
                let disconnected: Vec<String> = known.iter().filter(|k| !current.contains(k)).cloned().collect();
                log::debug!(
                    "Disconnected: {}, All: {}",
                    crate::pyfmt::py_repr(&json!(disconnected)),
                    crate::pyfmt::py_repr(&json!(known))
                );
                for sn in disconnected {
                    let c = t.conns.borrow_mut().shift_remove(&sn);
                    if let Some(c) = c {
                        c.close().await;
                    }
                }
            }
        });
        *self.watch_task.borrow_mut() = Some(watch);
        Ok(())
    }

    async fn close(self: Rc<Self>) {
        self.enabled.set(false);
        let st = self.start_task.borrow_mut().take();
        if let Some(t) = st {
            let ab = t.abort_handle();
            if tokio::time::timeout(Duration::from_secs(1), t).await.is_err() {
                ab.abort();
            }
        }
        if let Some(t) = self.watch_task.borrow_mut().take() {
            t.abort();
        }
        let conns: Vec<Rc<Td1Connection>> = self.conns.borrow().values().cloned().collect();
        for c in conns {
            c.close().await;
        }
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<Td1>, ConfigError> {
    Td1::new(config)
}

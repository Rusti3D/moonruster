// PanelDue LCD display support (port of moonraker/components/paneldue.py)

use crate::async_serial::AsyncSerial;
use crate::common::*;
use crate::confighelper::{ConfigError, ConfigHelper};
use crate::pyfmt::py_round;
use crate::server::{server, spawn, Component};
use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

const MIN_EST_TIME: f64 = 10.0;
const INITIALIZE_TIMEOUT: f64 = 10.0;
const RESTART_GCODES: [&str; 2] = ["RESTART", "FIRMWARE_RESTART"];

/// ServerError raised by gcode processing vs any other python exception
enum PdErr {
    Server(String),
    Other(String),
}

fn other<T>(msg: &str) -> Result<T, PdErr> {
    Err(PdErr::Other(msg.to_string()))
}

#[derive(Default, Debug)]
struct DirectArgs {
    p: Option<String>,
    s: Option<i64>,
    r: Option<i64>,
}

/// python int() of a string
fn py_int_str(s: &str) -> Option<i64> {
    crate::pyfmt::py_int(s.trim())
}

fn get<'a>(v: &'a Value, k: &str) -> &'a Value {
    v.get(k).unwrap_or(&Value::Null)
}

fn f64_or(v: &Value, k: &str, d: f64) -> f64 {
    v.get(k).and_then(|x| x.as_f64()).unwrap_or(d)
}

pub struct PanelDue {
    machine_name: String,
    kinematics: RefCell<String>,
    firmware_name: RefCell<String>,
    last_message: RefCell<Option<String>>,
    last_gcode_response: RefCell<Option<String>>,
    current_file: RefCell<String>,
    file_metadata: RefCell<Value>,
    enable_checksum: bool,
    debug_queue: RefCell<VecDeque<String>>,
    enabled: Cell<bool>,
    extruder_count: Cell<i64>,
    heaters: RefCell<Vec<String>>,
    is_ready: Cell<bool>,
    is_shutdown: Cell<bool>,
    initialized: Cell<bool>,
    cq_busy: Cell<bool>,
    gq_busy: Cell<bool>,
    command_queue: RefCell<VecDeque<(String, DirectArgs)>>,
    gc_queue: RefCell<VecDeque<String>>,
    last_printer_state: RefCell<String>,
    last_update_time: Cell<f64>,
    confirmed_gcode: RefCell<String>,
    mbox_sequence: Cell<i64>,
    available_macros: IndexMap<String, String>,
    confirmed_macros: IndexMap<String, String>,
    non_trivial_keys: Vec<String>,
    ser_conn: Rc<AsyncSerial>,
    serial_task: crate::server::TaskSlot,
}

impl PanelDue {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, ConfigError> {
        let srv = server();
        let machine_name = config.get_or("machine_name", "Klipper")?;
        let enable_checksum = config.getboolean_or("enable_checksum", true)?;
        let mut available_macros: IndexMap<String, String> = IndexMap::new();
        let mut confirmed_macros: IndexMap<String, String> = IndexMap::new();
        confirmed_macros.insert("RESTART".into(), "RESTART".into());
        confirmed_macros.insert("FIRMWARE_RESTART".into(), "FIRMWARE_RESTART".into());
        let key = |m: &String| m.split_whitespace().next().unwrap_or("").to_string();
        if let Some(macros) = config.getlist_opt("macros", None, Some("\n"))? {
            available_macros = macros.iter().map(|m| (key(m), m.clone())).collect();
        }
        if let Some(cm) = config.getlist_opt("confirmed_macros", None, Some("\n"))? {
            confirmed_macros = cm.iter().map(|m| (key(m), m.clone())).collect();
        }
        for (k, v) in &confirmed_macros {
            available_macros.insert(k.clone(), v.clone());
        }
        let non_trivial_keys = config.getlist_opt("non_trivial_keys", Some(vec!["Klipper state".into()]), Some("\n"))?.unwrap_or_default();
        let ser_conn = AsyncSerial::from_config(config, 57600)?;
        log::info!("PanelDue Configured");
        let pd = Rc::new(PanelDue {
            machine_name,
            kinematics: RefCell::new("none".into()),
            firmware_name: RefCell::new("Repetier | Klipper".into()),
            last_message: RefCell::new(None),
            last_gcode_response: RefCell::new(None),
            current_file: RefCell::new(String::new()),
            file_metadata: RefCell::new(json!({})),
            enable_checksum,
            debug_queue: RefCell::new(VecDeque::new()),
            enabled: Cell::new(true),
            extruder_count: Cell::new(0),
            heaters: RefCell::new(vec![]),
            is_ready: Cell::new(false),
            is_shutdown: Cell::new(false),
            initialized: Cell::new(false),
            cq_busy: Cell::new(false),
            gq_busy: Cell::new(false),
            command_queue: RefCell::new(VecDeque::new()),
            gc_queue: RefCell::new(VecDeque::new()),
            last_printer_state: RefCell::new("O".into()),
            last_update_time: Cell::new(0.0),
            confirmed_gcode: RefCell::new(String::new()),
            mbox_sequence: Cell::new(0),
            available_macros,
            confirmed_macros,
            non_trivial_keys,
            ser_conn,
            serial_task: Default::default(),
        });
        let p = Rc::downgrade(&pd);
        srv.on_event_async("server:klippy_ready", move |_| {
            let p = p.clone();
            async move {
                if let Some(p) = p.upgrade() {
                    if let Err(e) = p.process_klippy_ready().await {
                        log::error!("{}", e.message);
                    }
                }
            }
        });
        let p = Rc::downgrade(&pd);
        srv.on_event("server:klippy_shutdown", move |_| {
            if let Some(p) = p.upgrade() {
                p.is_shutdown.set(true);
            }
        });
        let p = Rc::downgrade(&pd);
        srv.on_event("server:klippy_disconnect", move |_| {
            if let Some(p) = p.upgrade() {
                p.write_response(json!({"status": "O"}));
                *p.last_printer_state.borrow_mut() = "O".into();
                p.is_ready.set(false);
                p.is_shutdown.set(false);
            }
        });
        let p = Rc::downgrade(&pd);
        srv.on_event("server:gcode_response", move |args| {
            if let Some(p) = p.upgrade() {
                if let Some(r) = args.first().and_then(|v| v.as_str()) {
                    p.handle_gcode_response(r);
                }
            }
        });
        let p = Rc::downgrade(&pd);
        let _ = srv.register_remote_method(
            "paneldue_beep",
            crate::components::klippy_connection::remote_method(move |args| {
                if let Some(p) = p.upgrade() {
                    let freq = args.get("frequency").cloned().unwrap_or(Value::Null);
                    let duration = args.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    p.write_response(json!({"beep_freq": freq, "beep_length": (duration * 1000.0) as i64}));
                }
            }),
        );
        Ok(pd)
    }

    async fn run_serial(self: Rc<Self>) {
        let mut last_err = String::new();
        while self.enabled.get() {
            if let Err(e) = self.ser_conn.open(true) {
                if e != last_err {
                    log::error!("PanelDue Serial Open Error\n{}", e);
                    last_err = e;
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
            while let Some(line) = self.ser_conn.read_line().await {
                let decoded = String::from_utf8_lossy(&line).trim().to_string();
                match self.process_line(&decoded) {
                    Ok(()) => {}
                    Err(PdErr::Server(e)) => {
                        let msg = format!("GCode Processing Error: {}", decoded);
                        log::error!("{}\n{}", msg, e);
                        self.handle_gcode_response(&format!("!! {}", msg));
                    }
                    Err(PdErr::Other(e)) => log::error!("Error during gcode processing\n{}", e),
                }
            }
            if self.enabled.get() {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            last_err.clear();
            self.initialized.set(false);
        }
    }

    async fn process_klippy_ready(&self) -> Result<(), ServerError> {
        let kapis = server().klippy_apis();
        let mut retries = 10;
        let (printer_info, cfg_status) = loop {
            let r: Result<(Value, Map<String, Value>), ServerError> = async {
                let pi = kapis.get_klippy_info(false).await?;
                let mut q = Map::new();
                q.insert("configfile".into(), Value::Null);
                let cs = kapis.query_objects(&q).await?;
                Ok((pi, cs))
            }
            .await;
            match r {
                Ok(v) => break v,
                Err(e) => {
                    log::error!("PanelDue initialization request failed\n{}", e.message);
                    retries -= 1;
                    if retries == 0 {
                        return Err(e);
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        };
        *self.firmware_name.borrow_mut() =
            format!("Repetier | Klipper {}", printer_info.get("software_version").map(crate::pyfmt::py_str).unwrap_or_default());
        let config = cfg_status.get("configfile").and_then(|c| c.get("config")).cloned().unwrap_or(json!({}));
        let kin = config.get("printer").and_then(|p| p.get("kinematics")).map(crate::pyfmt::py_str).unwrap_or("none".into());
        *self.kinematics.borrow_mut() = kin.clone();
        log::info!(
            "PanelDue Config Received:\nFirmware Name: {}\nKinematics: {}\nPrinter Config: {}\n",
            self.firmware_name.borrow(),
            kin,
            crate::pyfmt::py_repr(&config)
        );
        let mut sub = Map::new();
        for k in ["motion_report", "gcode_move", "toolhead", "virtual_sdcard", "fan", "display_status", "print_stats", "idle_timeout", "gcode_macro PANELDUE_BEEP"] {
            sub.insert(k.into(), Value::Null);
        }
        let mut count = 0;
        let mut heaters = Vec::new();
        let mut extruders = Vec::new();
        if let Value::Object(cfg) = &config {
            for k in cfg.keys() {
                if k.starts_with("extruder") {
                    count += 1;
                    extruders.push(k.clone());
                    sub.insert(k.clone(), Value::Null);
                } else if k == "heater_bed" {
                    heaters.push(k.clone());
                    sub.insert(k.clone(), Value::Null);
                }
            }
        }
        extruders.sort();
        heaters.extend(extruders);
        self.extruder_count.set(count);
        *self.heaters.borrow_mut() = heaters;
        if let Err(e) = kapis.subscribe_objects(&sub, None).await {
            log::error!("Unable to complete subscription request\n{}", e.message);
        }
        self.is_shutdown.set(false);
        self.is_ready.set(true);
        Ok(())
    }

    fn process_line(self: &Rc<Self>, line: &str) -> Result<(), PdErr> {
        {
            let mut dq = self.debug_queue.borrow_mut();
            if dq.len() == 100 {
                dq.pop_front();
            }
            dq.push_back(line.to_string());
        }
        if line.to_uppercase().contains("M112") {
            spawn(async {
                let _ = server().klippy_apis().emergency_stop().await;
            });
            return Ok(());
        }
        let chars: Vec<char> = line.chars().collect();
        let n = chars.len() as i64;
        // python slice semantics on character indices
        let slice = |a: i64, b: i64| -> String {
            let norm = |i: i64| if i < 0 { (n + i).max(0) } else { i.min(n) };
            let (a, b) = (norm(a), norm(b));
            if a >= b {
                String::new()
            } else {
                chars[a as usize..b as usize].iter().collect()
            }
        };
        let script = if self.enable_checksum {
            let mut line_index = line.find(' ').map(|i| line[..i].chars().count() as i64).unwrap_or(-1);
            let line_no = match py_int_str(&slice(1, line_index)) {
                Some(v) => Some(v),
                None => {
                    line_index = -1;
                    None
                }
            };
            let cs_index = line.rfind('*').map(|i| line[..i].chars().count() as i64).unwrap_or(-1);
            let mut msg = "!! Invalid Checksum".to_string();
            if let Some(l) = line_no {
                msg.push_str(&format!(" Line Number: {}", l));
            }
            let checksum = match py_int_str(&slice(cs_index + 1, n)) {
                Some(c) => c,
                None => {
                    log::error!("PanelDue: {}", msg);
                    return Err(PdErr::Server(msg));
                }
            };
            let calc = slice(0, cs_index).chars().fold(0u32, |acc, c| acc ^ c as u32);
            if (calc & 0xFF) as i64 != checksum {
                log::info!("PanelDue: {}", msg);
                return Err(PdErr::Server(msg));
            }
            slice(line_index + 1, cs_index).trim().to_string()
        } else {
            line.to_string()
        };
        let mut parts: Vec<String> = script.split_whitespace().map(String::from).collect();
        let cmd = match parts.first() {
            Some(c) => c.trim().to_string(),
            None => return other("IndexError: list index out of range"),
        };
        if ["M23", "M30", "M32", "M36", "M37", "M98"].contains(&cmd.as_str()) {
            let arg = script[cmd.len()..].trim().to_string();
            if !arg.is_empty() {
                parts = vec![cmd.clone(), arg];
            }
        }
        if ["M20", "M30", "M36", "M408"].contains(&cmd.as_str()) {
            let mut args = DirectArgs::default();
            for p in &parts[1..] {
                let first = p.chars().next().unwrap_or(' ');
                if !"PSR".contains(first) {
                    args.p = Some(p.trim_matches(|c| " \"\t\n".contains(c)).to_string());
                    continue;
                }
                let rest = &p[first.len_utf8()..];
                match first {
                    'P' => args.p = Some(rest.trim_matches(|c| " \"\t\n".contains(c)).to_string()),
                    _ => match py_int_str(rest) {
                        Some(v) => {
                            if first == 'S' {
                                args.s = Some(v);
                            } else {
                                args.r = Some(v);
                            }
                        }
                        None => {
                            let msg = format!("paneldue: Error parsing direct gcode {}", script);
                            self.handle_gcode_response(&format!("!! {}", msg));
                            log::error!("{}", msg);
                            return Ok(());
                        }
                    },
                }
            }
            self.queue_command(cmd, args);
            return Ok(());
        }
        let args: Vec<String> = parts[1..].to_vec();
        let arg0 = || args.first().cloned().ok_or_else(|| PdErr::Other("IndexError: list index out of range".into()));
        let script = match cmd.as_str() {
            "M0" => "CANCEL_PRINT".to_string(),
            "M23" => format!("M23 {}", Self::clean_filename(&arg0()?)?),
            "M24" => "RESUME".into(),
            "M25" => "PAUSE".into(),
            "M32" => format!("SDCARD_PRINT_FILE FILENAME=\"{}\"", Self::clean_filename(&arg0()?)?.replace('"', "\\\"")),
            "M98" => self.prepare_m98(&arg0()?)?,
            "M120" => "SAVE_GCODE_STATE STATE=PANELDUE".into(),
            "M121" => "RESTORE_GCODE_STATE STATE=PANELDUE".into(),
            "M290" => {
                let a = arg0()?;
                format!("SET_GCODE_OFFSET Z_ADJUST={} MOVE=1", a.chars().skip(1).collect::<String>().trim())
            }
            "M292" => {
                let a = arg0()?;
                let c = a.chars().nth(1).ok_or_else(|| PdErr::Other("IndexError: string index out of range".into()))?;
                let p_val = py_int_str(&c.to_string()).ok_or_else(|| PdErr::Other(format!("invalid literal for int() with base 10: '{}'", c)))?;
                if p_val == 0 {
                    std::mem::take(&mut *self.confirmed_gcode.borrow_mut())
                } else {
                    String::new()
                }
            }
            "M999" => "FIRMWARE_RESTART".into(),
            _ => script,
        };
        if script.is_empty() {
            return Ok(());
        }
        self.queue_gcode(script);
        Ok(())
    }

    fn clean_filename(filename: &str) -> Result<String, PdErr> {
        // upstream discards the result of strip() here
        let mut f = filename.to_string();
        if let Some(s) = f.strip_prefix("0:/") {
            f = s.to_string();
        }
        if f.starts_with("gcodes/") {
            f = f[6..].to_string();
        } else if f.starts_with("/gcodes/") {
            f = f[7..].to_string();
        }
        match f.chars().next() {
            None => return other("IndexError: string index out of range"),
            Some('/') => {}
            Some(_) => f = format!("/{}", f),
        }
        Ok(f)
    }

    fn prepare_m98(&self, arg: &str) -> Result<String, PdErr> {
        let macro_: String = arg.chars().skip(1).collect::<String>().trim_matches(|c| " \"\t\n".contains(c)).to_string();
        let name = match macro_.rfind('/') {
            Some(i) => macro_[i + 1..].to_string(),
            None => macro_,
        };
        let cmd = self.available_macros.get(&name).cloned().ok_or_else(|| PdErr::Server(format!("Macro {} invalid", name)))?;
        if self.confirmed_macros.contains_key(&name) {
            self.create_confirmation(&name, &cmd);
            return Ok(String::new());
        }
        Ok(cmd)
    }

    fn create_confirmation(&self, name: &str, gcode: &str) {
        self.mbox_sequence.set(self.mbox_sequence.get() + 1);
        *self.confirmed_gcode.borrow_mut() = gcode.to_string();
        let mbox = json!({
            "msgBox.mode": 3,
            "msgBox.msg": format!("Please confirm your intent to run {}. Press OK to continue, or CANCEL to abort.", name),
            "msgBox.seq": self.mbox_sequence.get(),
            "msgBox.title": "Confirmation Dialog",
            "msgBox.controls": 0,
            "msgBox.timeout": 0,
        });
        log::debug!("Creating PanelDue Confirmation: {}", crate::pyfmt::py_repr(&mbox));
        self.write_response(mbox);
    }

    fn queue_gcode(self: &Rc<Self>, script: String) {
        self.gc_queue.borrow_mut().push_back(script);
        if !self.gq_busy.get() {
            self.gq_busy.set(true);
            let p = self.clone();
            spawn(async move {
                let kapis = server().klippy_apis();
                loop {
                    let script = match p.gc_queue.borrow_mut().pop_front() {
                        None => break,
                        Some(s) => s,
                    };
                    let r = if RESTART_GCODES.contains(&script.as_str()) {
                        kapis.do_restart(&script, false).await
                    } else {
                        kapis.run_gcode(&script).await
                    };
                    if let Err(e) = r {
                        let msg = format!("Error executing script {}", script);
                        p.handle_gcode_response(&format!("!! {}", msg));
                        log::error!("{}\n{}", msg, e.message);
                    }
                }
                p.gq_busy.set(false);
            });
        }
    }

    fn queue_command(self: &Rc<Self>, cmd: String, args: DirectArgs) {
        self.command_queue.borrow_mut().push_back((cmd, args));
        if !self.cq_busy.get() {
            self.cq_busy.set(true);
            let p = self.clone();
            spawn(async move {
                loop {
                    let item = p.command_queue.borrow_mut().pop_front();
                    let (cmd, args) = match item {
                        None => break,
                        Some(i) => i,
                    };
                    if let Err(e) = p.run_direct(&cmd, args).await {
                        log::error!("Error processing command\n{}", e);
                    }
                }
                p.cq_busy.set(false);
            });
        }
    }

    async fn run_direct(&self, cmd: &str, a: DirectArgs) -> Result<(), String> {
        let unexpected = |name: &str, k: &str| Err(format!("{}() got an unexpected keyword argument 'arg_{}'", name, k));
        match cmd {
            "M20" => {
                if a.r.is_some() {
                    return unexpected("_run_paneldue_M20", "r");
                }
                let p = a.p.ok_or("_run_paneldue_M20() missing 1 required positional argument: 'arg_p'")?;
                self.m20(&p, a.s.unwrap_or(0))
            }
            "M30" => {
                if a.s.is_some() {
                    return unexpected("_run_paneldue_M30", "s");
                }
                if a.r.is_some() {
                    return unexpected("_run_paneldue_M30", "r");
                }
                self.m30(&a.p.unwrap_or_default()).await
            }
            "M36" => {
                if a.s.is_some() {
                    return unexpected("_run_paneldue_M36", "s");
                }
                if a.r.is_some() {
                    return unexpected("_run_paneldue_M36", "r");
                }
                self.m36(a.p)
            }
            _ => {
                if a.p.is_some() {
                    return unexpected("_run_paneldue_M408", "p");
                }
                self.m408(a.r, a.s.unwrap_or(1))
            }
        }
    }

    fn handle_gcode_response(&self, response: &str) {
        if response.contains("Klipper state") || response.starts_with("!!") {
            *self.last_gcode_response.borrow_mut() = Some(response.to_string());
        } else {
            for key in &self.non_trivial_keys {
                if response.contains(key.as_str()) {
                    *self.last_gcode_response.borrow_mut() = Some(response.to_string());
                    return;
                }
            }
        }
    }

    fn write_response(&self, response: Value) {
        let mut data = response.to_string().into_bytes();
        data.extend_from_slice(b"\r\n");
        let _ = self.ser_conn.send(data);
    }

    fn get_printer_status(&self, p_state: &Map<String, Value>) -> String {
        if self.is_shutdown.get() {
            return "S".into();
        }
        let sd_state = p_state.get("print_stats").and_then(|p| p.get("state")).and_then(|s| s.as_str()).unwrap_or("standby");
        let last = self.last_printer_state.borrow().clone();
        if sd_state == "printing" {
            return if last == "A" { "R".into() } else { "P".into() };
        } else if sd_state == "paused" {
            let p_active = p_state.get("idle_timeout").and_then(|i| i.get("state")).and_then(|s| s.as_str()).unwrap_or("Idle") == "Printing";
            return if p_active && last != "A" { "D".into() } else { "A".into() };
        }
        "I".into()
    }

    fn m408(&self, sequence: Option<i64>, response_type: i64) -> Result<(), String> {
        let mut response = Map::new();
        let curtime = crate::utils::monotonic();
        if curtime - self.last_update_time.get() > INITIALIZE_TIMEOUT {
            self.initialized.set(false);
        }
        self.last_update_time.set(curtime);
        if !self.initialized.get() {
            response.insert("dir".into(), json!("/macros"));
            response.insert("files".into(), json!(self.available_macros.keys().collect::<Vec<_>>()));
            self.initialized.set(true);
        }
        if !self.is_ready.get() {
            *self.last_printer_state.borrow_mut() = "O".into();
            response.insert("status".into(), json!("O"));
            self.write_response(Value::Object(response));
            return Ok(());
        }
        if let Some(seq) = sequence {
            if let Some(r) = self.last_gcode_response.borrow_mut().take().filter(|r| !r.is_empty()) {
                response.insert("seq".into(), json!(seq + 1));
                response.insert("resp".into(), json!(r));
            }
        }
        if response_type == 1 {
            response.insert("myName".into(), json!(self.machine_name));
            response.insert("firmwareName".into(), json!(*self.firmware_name.borrow()));
            response.insert("numTools".into(), json!(self.extruder_count.get()));
            response.insert("geometry".into(), json!(*self.kinematics.borrow()));
            response.insert("axes".into(), json!(3));
        }
        let p_state = server().klippy().get_subscription_cache();
        let empty = json!({});
        let toolhead = p_state.get("toolhead").unwrap_or(&empty);
        let gcode_move = p_state.get("gcode_move").unwrap_or(&empty);
        let status = self.get_printer_status(&p_state);
        *self.last_printer_state.borrow_mut() = status.clone();
        response.insert("status".into(), json!(status));
        let homing_origin = gcode_move.get("homing_origin").and_then(|v| v.as_array()).cloned().unwrap_or(vec![json!(0.0); 4]);
        response.insert("babystep".into(), json!(py_round(homing_origin.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0), 3)));
        let pos: Vec<f64> = p_state
            .get("motion_report")
            .and_then(|m| m.get("live_position"))
            .and_then(|v| v.as_array())
            .map(|a| a.iter().map(|x| x.as_f64().unwrap_or(0.0)).collect())
            .unwrap_or(vec![0.0; 4]);
        response.insert("pos".into(), json!(pos.iter().take(3).map(|p| py_round(*p, 2)).collect::<Vec<_>>()));
        let homed = toolhead.get("homed_axes").and_then(|v| v.as_str()).unwrap_or("");
        response.insert("homed".into(), json!("xyz".chars().map(|a| homed.contains(a) as i64).collect::<Vec<_>>()));
        response.insert("sfactor".into(), json!(py_round(f64_or(gcode_move, "speed_factor", 1.0) * 100.0, 2)));
        let sd_status = p_state.get("virtual_sdcard").unwrap_or(&empty);
        let print_stats = p_state.get("print_stats").unwrap_or(&empty);
        let fname = print_stats.get("filename").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let sd_print_state = print_stats.get("state").and_then(|v| v.as_str());
        if matches!(sd_print_state, Some("printing") | Some("paused")) {
            if *self.current_file.borrow() != fname {
                *self.current_file.borrow_mut() = fname.clone();
                let fm = server().get::<crate::components::file_manager::FileManager>();
                *self.file_metadata.borrow_mut() = fm.map(|f| f.get_file_metadata(&fname)).unwrap_or(json!({}));
            }
            let progress = f64_or(sd_status, "progress", 0.0);
            if progress != 0.0 {
                response.insert("fraction_printed".into(), json!(py_round(progress, 3)));
                let md = self.file_metadata.borrow().clone();
                let est_time = f64_or(&md, "estimated_time", 0.0);
                let times_left: Vec<i64> = if est_time > MIN_EST_TIME {
                    let mut tl = vec![(est_time - est_time * progress) as i64];
                    let est_fil = f64_or(&md, "filament_total", 0.0);
                    if est_fil != 0.0 {
                        let fpct = (f64_or(print_stats, "filament_used", 0.0) / est_fil).min(1.0);
                        tl.push((est_time - est_time * fpct) as i64);
                    }
                    let obj_h = f64_or(&md, "object_height", 0.0);
                    if obj_h != 0.0 {
                        let cur_h = get(gcode_move, "gcode_position").get(2).and_then(|v| v.as_f64()).unwrap_or(0.0);
                        let hpct = (cur_h / obj_h).min(1.0);
                        tl.push((est_time - est_time * hpct) as i64);
                    }
                    tl
                } else {
                    let duration = f64_or(print_stats, "print_duration", 0.0);
                    vec![(duration / progress - duration) as i64]
                };
                response.insert("timesLeft".into(), json!(times_left));
            }
        } else {
            self.current_file.borrow_mut().clear();
            *self.file_metadata.borrow_mut() = json!({});
        }
        if let Some(fs) = p_state.get("fan").and_then(|f| f.get("speed")).and_then(|v| v.as_f64()) {
            response.insert("fanPercent".into(), json!([py_round(fs * 100.0, 1)]));
        }
        let mut extruder_name = String::new();
        if self.extruder_count.get() > 0 {
            extruder_name = toolhead.get("extruder").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if !extruder_name.is_empty() {
                let tool = if extruder_name != "extruder" {
                    extruder_name.chars().last().and_then(|c| c.to_digit(10)).ok_or_else(|| {
                        format!("invalid literal for int() with base 10: '{}'", extruder_name.chars().last().unwrap_or(' '))
                    })? as i64
                } else {
                    0
                };
                response.insert("tool".into(), json!(tool));
            }
        }
        let efactor = py_round(f64_or(gcode_move, "extrude_factor", 1.0) * 100.0, 2);
        let mut push = |k: &str, v: Value| {
            response.entry(k.to_string()).or_insert_with(|| json!([])).as_array_mut().unwrap().push(v);
        };
        for name in self.heaters.borrow().iter() {
            let h = p_state.get(name).unwrap_or(&empty);
            let temp = py_round(f64_or(h, "temperature", 0.0), 1);
            let target = py_round(f64_or(h, "target", 0.0), 1);
            push("heaters", json!(temp));
            push("active", json!(target));
            push("standby", json!(target));
            if name.starts_with("extruder") {
                let a_stat = if *name == extruder_name { 2 } else { 1 };
                push("hstat", json!(if target != 0.0 { a_stat } else { 0 }));
                push("efactor", json!(efactor));
                push("extr", json!(py_round(pos.get(3).copied().unwrap_or(0.0), 2)));
            } else {
                push("hstat", json!(if target != 0.0 { 2 } else { 0 }));
            }
        }
        let msg = p_state.get("display_status").and_then(|d| d.get("message")).and_then(|v| v.as_str()).unwrap_or("").to_string();
        if !msg.is_empty() && Some(&msg) != self.last_message.borrow().as_ref() {
            response.insert("message".into(), json!(msg));
        }
        *self.last_message.borrow_mut() = Some(msg);
        self.write_response(Value::Object(response));
        Ok(())
    }

    fn m20(&self, arg_p: &str, response_type: i64) -> Result<(), String> {
        if response_type != 2 {
            log::info!("Cannot process response type {} in M20", response_type);
            return Ok(());
        }
        let mut path = arg_p.trim_matches('"').to_string();
        if path.starts_with("0:/") {
            path = path[2..].to_string();
        }
        let mut response = Map::new();
        response.insert("dir".into(), json!(path));
        response.insert("files".into(), json!([]));
        if path == "/macros" {
            response.insert("files".into(), json!(self.available_macros.keys().collect::<Vec<_>>()));
        } else {
            if path == "/" {
                response.insert("dir".into(), json!("/gcodes"));
                path = "gcodes".into();
            } else if path.starts_with("/gcodes") {
                path = path[1..].to_string();
            }
            let fm = server().get::<crate::components::file_manager::FileManager>().ok_or("file_manager")?;
            let flist = fm.list_dir(&path, true).map_err(|e| e.message)?;
            if crate::pyfmt::py_truthy(&flist) {
                response.insert("files".into(), flist);
            }
        }
        self.write_response(Value::Object(response));
        Ok(())
    }

    async fn m30(&self, arg_p: &str) -> Result<(), String> {
        let mut path = arg_p.trim_matches('"').to_string();
        if let Some(s) = path.strip_prefix("0:/") {
            path = s.to_string();
        } else {
            match path.chars().next() {
                None => return Err("IndexError: string index out of range".into()),
                Some('/') => path = path[1..].to_string(),
                Some(_) => {}
            }
        }
        if !path.starts_with("gcodes/") {
            path = format!("gcodes/{}", path);
        }
        let fm = server().get::<crate::components::file_manager::FileManager>().ok_or("file_manager")?;
        fm.delete_file(&path).await.map_err(|e| e.message)?;
        Ok(())
    }

    fn m36(&self, arg_p: Option<String>) -> Result<(), String> {
        let mut response = Map::new();
        let p_state = server().klippy().get_subscription_cache();
        let empty = json!({});
        let sd_status = p_state.get("virtual_sdcard").unwrap_or(&empty);
        let print_stats = p_state.get("print_stats").unwrap_or(&empty);
        let mut filename = match arg_p {
            Some(f) => f,
            None => {
                let mut active = false;
                let mut fname = String::new();
                if crate::pyfmt::py_truthy(sd_status) && crate::pyfmt::py_truthy(print_stats) {
                    fname = print_stats.get("filename").map(crate::pyfmt::py_str).ok_or("KeyError: 'filename'")?;
                    active = sd_status.get("is_active").map(crate::pyfmt::py_truthy).ok_or("KeyError: 'is_active'")?;
                }
                if fname.is_empty() || !active {
                    response.insert("err".into(), json!(1));
                    self.write_response(Value::Object(response));
                    return Ok(());
                }
                response.insert("fileName".into(), json!(fname.rsplit('/').next().unwrap_or("")));
                fname
            }
        };
        match filename.chars().next() {
            None => return Err("IndexError: string index out of range".into()),
            Some('/') => filename = filename[1..].to_string(),
            Some(_) => {}
        }
        if !filename.starts_with("gcodes/") {
            filename = format!("gcodes/{}", filename);
        }
        let fm = server().get::<crate::components::file_manager::FileManager>().ok_or("file_manager")?;
        let metadata = fm.get_file_metadata(&filename);
        if crate::pyfmt::py_truthy(&metadata) {
            response.insert("err".into(), json!(0));
            response.insert("size".into(), metadata.get("size").cloned().ok_or("KeyError: 'size'")?);
            let modified = metadata.get("modified").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let ctime = chrono::DateTime::from_timestamp(modified as i64, 0)
                .map(|d| d.with_timezone(&chrono::Local).format("%a %b %e %H:%M:%S %Y").to_string())
                .unwrap_or_default();
            response.insert("lastModified".into(), json!(format!("T{}", ctime)));
            if let Some(s) = metadata.get("slicer").filter(|v| !v.is_null()) {
                response.insert("generatedBy".into(), s.clone());
            }
            if let Some(h) = metadata.get("object_height").and_then(|v| v.as_f64()) {
                response.insert("height".into(), json!(py_round(h, 2)));
            }
            if let Some(h) = metadata.get("layer_height").and_then(|v| v.as_f64()) {
                response.insert("layerHeight".into(), json!(py_round(h, 2)));
            }
            if let Some(f) = metadata.get("filament_total").and_then(|v| v.as_f64()) {
                response.insert("filament".into(), json!([py_round(f, 1)]));
            }
            if let Some(t) = metadata.get("estimated_time").and_then(|v| v.as_f64()) {
                response.insert("printTime".into(), json!((t + 0.5) as i64));
            }
        } else {
            response.insert("err".into(), json!(1));
        }
        self.write_response(Value::Object(response));
        Ok(())
    }
}

#[async_trait(?Send)]
impl Component for PanelDue {
    fn has_init(&self) -> bool {
        true
    }

    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        let h = spawn(self.clone().run_serial());
        self.serial_task.set(h);
        Ok(())
    }

    async fn close(self: Rc<Self>) {
        self.enabled.set(false);
        self.ser_conn.close().await;
        let t = self.serial_task.take();
        if let Some(t) = t {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(3), t).await;
        }
        let mut msg = "\nPanelDue GCode Dump:".to_string();
        for (i, gc) in self.debug_queue.borrow().iter().enumerate() {
            msg.push_str(&format!("\nSequence {}: {}", i, gc));
        }
        log::debug!("{}", msg);
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<PanelDue>, ConfigError> {
    PanelDue::new(config)
}

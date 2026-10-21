// SimplyPrint Connection Support (port of moonraker/components/simplyprint.py)

use crate::common::*;
use crate::components::database::NamespaceWrapper;
use crate::components::http_client::{Body, HttpClient, RequestOpts};
use crate::confighelper::{ConfigError, ConfigHelper};
use crate::pyfmt::py_repr;
use crate::server::{server, spawn, Component};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

const COMPONENT_VERSION: &str = "0.0.1";
const SP_VERSION: &str = "0.1";
const CONNECTION_ERROR_LOG_TIME: f64 = 60.0;
const PRE_SETUP_EVENTS: [&str; 6] = ["connection", "state_change", "shutdown", "machine_data", "firmware", "ping"];
const INITIAL_AMBIENT: i64 = 85;
const AMBIENT_CHECK_TIME: f64 = 5.0 * 60.0;
const TARGET_CHECK_TIME: f64 = 60.0 * 60.0;
const SAMPLE_CHECK_TIME: f64 = 20.0;
const AMB_CHECK_INTERVAL: f64 = 5.0;
const FALLBACK_URL: &str = "http://127.0.0.1:8080/?action=snapshot";
const SP_SNAPSHOT_URL: &str = "https://api.simplyprint.io/jobs/ReceiveSnapshot";

fn now() -> f64 {
    crate::utils::monotonic()
}

/// FlexTimer equivalent: the callback returns the delay until the next call
struct Timer {
    handle: RefCell<Option<tokio::task::JoinHandle<()>>>,
}

impl Timer {
    fn new() -> Self {
        Timer { handle: RefCell::new(None) }
    }
    fn start(&self, delay: f64, cb: Rc<dyn Fn() -> f64>) {
        self.stop();
        let h = spawn(async move {
            let mut d = delay;
            loop {
                if d > 0.0 {
                    tokio::time::sleep(Duration::from_secs_f64(d)).await;
                }
                d = cb();
            }
        });
        *self.handle.borrow_mut() = Some(h);
    }
    fn stop(&self) {
        if let Some(h) = self.handle.borrow_mut().take() {
            h.abort();
        }
    }
    fn is_running(&self) -> bool {
        self.handle.borrow().is_some()
    }
}

/// Protocol logger, active only with verbose logging
struct ProtoLogger {
    path: Option<PathBuf>,
}

impl ProtoLogger {
    fn new() -> Self {
        let srv = server();
        if !srv.is_verbose_enabled() {
            return ProtoLogger { path: None };
        }
        let root = srv
            .lookup_component::<crate::components::file_manager::FileManager>("file_manager")
            .map(|f| f.get_directory("logs"))
            .unwrap_or_default();
        let parent = if root.is_empty() { std::env::temp_dir() } else { PathBuf::from(root) };
        ProtoLogger { path: Some(parent.join("simplyprint.log")) }
    }
    fn write(&self, func: &str, msg: &str) {
        let path = match &self.path {
            Some(p) => p,
            None => return,
        };
        // TimedRotatingFileHandler(when='midnight', backupCount=2)
        if let Ok(md) = std::fs::metadata(path) {
            if let Ok(m) = md.modified() {
                let day = chrono::DateTime::<chrono::Local>::from(m).date_naive();
                if day != chrono::Local::now().date_naive() {
                    let dest = PathBuf::from(format!("{}.{}", path.display(), day.format("%Y-%m-%d")));
                    let _ = std::fs::rename(path, &dest);
                    if let Some(parent) = path.parent() {
                        let prefix = format!("{}.", path.file_name().unwrap().to_string_lossy());
                        let mut olds: Vec<PathBuf> = std::fs::read_dir(parent)
                            .map(|r| r.flatten().map(|e| e.path()).filter(|p| p.file_name().map(|n| n.to_string_lossy().starts_with(&prefix)).unwrap_or(false)).collect())
                            .unwrap_or_default();
                        olds.sort();
                        while olds.len() > 2 {
                            let _ = std::fs::remove_file(olds.remove(0));
                        }
                    }
                }
            }
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S,%3f");
            let _ = writeln!(f, "{} [{}()] - {}", ts, func, msg);
        }
    }
    fn info(&self, msg: &str) {
        self.write("info", msg);
    }
    fn debug(&self, _msg: &str) {}
}

#[derive(Default)]
struct ReportCache {
    state: String,
    temps: Map<String, Value>,
    metadata: Value,
    mesh: Value,
    job_info: Map<String, Value>,
    exclude_object_status: Map<String, Value>,
    active_extruder: String,
    firmware_info: Map<String, Value>,
    machine_info: Map<String, Value>,
    cpu_info: Map<String, Value>,
    throttled_state: Value,
    current_wsid: Option<u64>,
    filament_state: String,
}

impl ReportCache {
    fn reset_print_state(&mut self) {
        self.temps = Map::new();
        self.mesh = json!({});
        self.job_info = Map::new();
        self.exclude_object_status = Map::new();
    }
}

struct LayerDetect {
    layer: i64,
    layer_z: f64,
    active: bool,
    layer_height: f64,
    fl_height: f64,
    layer_count: i64,
    check_next: bool,
}

impl LayerDetect {
    fn new() -> Self {
        LayerDetect { layer: 0, layer_z: 0.0, active: false, layer_height: 0.0, fl_height: 0.0, layer_count: 99999999999, check_next: false }
    }
    fn update(&mut self, pos: &Value) {
        let z = pos.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0);
        if !self.active || self.layer_z == z || self.layer_height == 0.0 {
            self.check_next = false;
            return;
        }
        if !self.check_next {
            self.check_next = true;
            return;
        }
        self.check_next = false;
        let layer = 1 + ((z - self.fl_height) / self.layer_height + 0.5) as i64;
        self.layer = layer.min(self.layer_count);
        self.layer_z = z;
    }
    fn start(&mut self, md: &Value) {
        *self = LayerDetect::new();
        let lh = md.get("layer_height").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let flh = md.get("first_layer_height").and_then(|v| v.as_f64()).unwrap_or(lh);
        if lh > 0.000001 && flh > 0.000001 {
            self.active = true;
            self.layer_height = lh;
            self.fl_height = flh;
            if let Some(lc) = md.get("layer_count").and_then(|v| v.as_i64()) {
                self.layer_count = lc;
            } else if let Some(oh) = md.get("object_height").and_then(|v| v.as_f64()) {
                self.layer_count = ((oh - flh) / lh + 0.5) as i64;
            }
        }
    }
    fn resume(&mut self) {
        if self.layer_height > 0.0 {
            self.active = true;
        }
    }
    fn stop(&mut self) {
        self.active = false;
    }
}

struct AmbientDetect {
    initial_sample: Cell<f64>,
    ambient: Cell<i64>,
    last_sample_time: Cell<f64>,
    update_interval: Cell<f64>,
    timer: Timer,
    sensor_name: String,
}

pub struct SimplyPrint {
    uid: u64,
    logger: ProtoLogger,
    spdb: NamespaceWrapper,
    sp_info: RefCell<Map<String, Value>>,
    is_closing: Cell<bool>,
    ws_tx: RefCell<Option<tokio::sync::mpsc::UnboundedSender<Message>>>,
    cache: RefCell<ReportCache>,
    amb: AmbientDetect,
    layer_detect: RefCell<LayerDetect>,
    webcam_name: String,
    webcam_url: RefCell<String>,
    webcam_cam: RefCell<Option<crate::components::webcam::WebCam>>,
    webcam_connected: Cell<bool>,
    last_received_temps: RefCell<HashMap<String, f64>>,
    last_err_log_time: Cell<f64>,
    last_cpu_update_time: Cell<f64>,
    intervals: RefCell<HashMap<String, f64>>,
    printer_status: RefCell<Map<String, Value>>,
    heaters: RefCell<Vec<(String, String)>>,
    missed_job_events: RefCell<Vec<Map<String, Value>>>,
    announce_mutex: tokio::sync::Mutex<()>,
    connection_task: RefCell<Option<tokio::task::JoinHandle<()>>>,
    reconnect_delay: Cell<f64>,
    reconnect_token: RefCell<Option<String>>,
    last_sp_ping: Cell<f64>,
    ping_sp_timer: Timer,
    printer_info_timer: Timer,
    print_request_event: Rc<crate::utils::AsyncEvent>,
    next_temp_update_time: Cell<f64>,
    last_ping_received: Cell<f64>,
    gcode_terminal_enabled: Cell<bool>,
    current_job_id: Cell<Option<i64>>,
    connected: Cell<bool>,
    is_set_up: Cell<bool>,
    test: bool,
    connect_url: RefCell<String>,
    power_id: String,
    filament_sensor: String,
    // print handler
    download_task: RefCell<Option<tokio::task::JoinHandle<()>>>,
    print_ready_event: Rc<crate::utils::AsyncEvent>,
    download_progress: Cell<i64>,
    pending_file: RefCell<String>,
    last_started: RefCell<String>,
}

fn diff_obj(new: &Map<String, Value>, cached: &Map<String, Value>) -> Map<String, Value> {
    if cached.is_empty() {
        return new.clone();
    }
    new.iter().filter(|(k, v)| cached.get(*k) != Some(*v)).map(|(k, v)| (k.clone(), v.clone())).collect()
}

impl SimplyPrint {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, ConfigError> {
        let srv = server();
        let logger = ProtoLogger::new();
        let db = srv.database();
        db.register_local_namespace("simplyprint", true, false).map_err(|e| ConfigHelper::error(e.message))?;
        let spdb = db.wrap_namespace("simplyprint", true).map_err(|e| ConfigHelper::error(e.message))?;
        let sp_info = spdb.as_dict();
        let ambient = sp_info.get("ambient_temp").and_then(|v| v.as_i64()).unwrap_or(INITIAL_AMBIENT);
        let sensor_name = config.get_or("ambient_sensor", "")?;
        let webcam_name = config.get_or("webcam_name", "")?;
        let test = config.get_opt("use_test_endpoint")?.map(|s| !s.is_empty()).unwrap_or(false);
        let connect_url = config.get_opt("url")?;
        let mut power_id = String::new();
        match config.get_opt("power_device")? {
            Some(pid) => {
                power_id = pid.strip_prefix("power ").map(String::from).unwrap_or(pid.clone());
                if !config.has_section(&format!("power {}", power_id)) {
                    power_id.clear();
                    srv.add_warning(
                        &format!("Section [simplyprint], option 'power_device': Unable to locate configuration for power device {}", pid),
                        None,
                        true,
                    );
                }
            }
            None => {
                let pfx = config.get_prefix_sections("power ");
                if pfx.len() == 1 {
                    let name = pfx[0][6..].to_string();
                    if name.to_lowercase().contains("printer") {
                        power_id = name;
                    }
                }
            }
        }
        let mut filament_sensor = String::new();
        if let Some(fs) = config.get_opt("filament_sensor")? {
            let prefixes = ["filament_switch_sensor ", "filament_motion_sensor "];
            if prefixes.iter().any(|p| fs.starts_with(p)) {
                filament_sensor = fs;
            } else {
                srv.add_warning(
                    &format!(
                        "Section [simplyprint], option 'filament_sensor': Invalid sensor '{}', must start with one the following prefixes: {}",
                        fs,
                        py_repr(&json!(prefixes))
                    ),
                    None,
                    true,
                );
            }
        }
        let mut intervals = HashMap::new();
        for (k, v) in [("job", 1.0), ("temps", 1.0), ("temps_target", 0.25), ("cpu", 10.0), ("ai", 0.0), ("ping", 20.0)] {
            intervals.insert(k.to_string(), v);
        }
        let sp = Rc::new(SimplyPrint {
            uid: crate::components::websockets::next_uid(),
            logger,
            spdb,
            sp_info: RefCell::new(sp_info),
            is_closing: Cell::new(false),
            ws_tx: RefCell::new(None),
            cache: RefCell::new(ReportCache { state: "offline".into(), metadata: json!({}), mesh: json!({}), throttled_state: json!({}), ..Default::default() }),
            amb: AmbientDetect {
                initial_sample: Cell::new(-1000.0),
                ambient: Cell::new(ambient),
                last_sample_time: Cell::new(0.0),
                update_interval: Cell::new(AMBIENT_CHECK_TIME),
                timer: Timer::new(),
                sensor_name,
            },
            layer_detect: RefCell::new(LayerDetect::new()),
            webcam_name,
            webcam_url: RefCell::new(FALLBACK_URL.into()),
            webcam_cam: RefCell::new(None),
            webcam_connected: Cell::new(false),
            last_received_temps: RefCell::new(HashMap::new()),
            last_err_log_time: Cell::new(0.0),
            last_cpu_update_time: Cell::new(0.0),
            intervals: RefCell::new(intervals),
            printer_status: RefCell::new(Map::new()),
            heaters: RefCell::new(vec![]),
            missed_job_events: RefCell::new(vec![]),
            announce_mutex: tokio::sync::Mutex::new(()),
            connection_task: RefCell::new(None),
            reconnect_delay: Cell::new(1.0),
            reconnect_token: RefCell::new(None),
            last_sp_ping: Cell::new(0.0),
            ping_sp_timer: Timer::new(),
            printer_info_timer: Timer::new(),
            print_request_event: crate::utils::AsyncEvent::new(),
            next_temp_update_time: Cell::new(0.0),
            last_ping_received: Cell::new(0.0),
            gcode_terminal_enabled: Cell::new(false),
            current_job_id: Cell::new(None),
            connected: Cell::new(false),
            is_set_up: Cell::new(false),
            test,
            connect_url: RefCell::new(String::new()),
            power_id,
            filament_sensor,
            download_task: RefCell::new(None),
            print_ready_event: crate::utils::AsyncEvent::new(),
            download_progress: Cell::new(-1),
            pending_file: RefCell::new(String::new()),
            last_started: RefCell::new(String::new()),
        });
        match connect_url {
            Some(u) => {
                *sp.connect_url.borrow_mut() = u;
                sp.is_set_up.set(true);
            }
            None => sp.set_ws_url(),
        }
        Self::register_events(&sp);
        Ok(sp)
    }

    fn register_events(sp: &Rc<Self>) {
        let srv = server();
        macro_rules! on {
            ($evt:expr, |$s:ident, $args:ident| $body:expr) => {{
                let w = Rc::downgrade(sp);
                srv.on_event($evt, move |$args| {
                    if let Some($s) = w.upgrade() {
                        $body;
                    }
                });
            }};
        }
        on!("server:klippy_started", |s, args| {
            let st = args.first().and_then(|v| v.as_str()).and_then(|v| KlippyState::from_string(v).ok()).unwrap_or(KlippyState::STARTUP);
            s.on_klippy_startup(st)
        });
        {
            let w = Rc::downgrade(sp);
            srv.on_event_async("server:klippy_ready", move |_| {
                let w = w.clone();
                async move {
                    if let Some(s) = w.upgrade() {
                        s.on_klippy_ready().await;
                    }
                }
            });
        }
        on!("server:klippy_shutdown", |s, _a| {
            s.update_state("error");
            s.send_sp("printer_error", json!({"error": server().klippy().state_message()}));
        });
        on!("server:klippy_disconnect", |s, _a| s.on_klippy_disconnected());
        on!("job_state:state_changed", |s, args| s.on_job_state_changed(args));
        on!("klippy_apis:pause_requested", |s, _a| {
            s.print_request_event.set();
            if s.cache.borrow().state == "printing" {
                s.update_state("pausing");
            }
        });
        on!("klippy_apis:resume_requested", |s, _a| {
            s.print_request_event.set();
            if s.cache.borrow().state == "paused" {
                s.update_state("resuming");
            }
        });
        on!("klippy_apis:cancel_requested", |s, _a| {
            s.print_request_event.set();
            let st = s.cache.borrow().state.clone();
            if ["printing", "paused", "pausing"].contains(&st.as_str()) {
                s.update_state("cancelling");
            }
        });
        on!("proc_stats:proc_stat_update", |s, args| s.on_proc_update(args.first().cloned().unwrap_or(Value::Null)));
        on!("proc_stats:cpu_throttled", |s, args| s.cache.borrow_mut().throttled_state = args.first().cloned().unwrap_or(json!({})));
        on!("websockets:client_identified", |s, args| s.on_websocket_identified(args.first().and_then(|v| v.as_u64()).unwrap_or(0)));
        on!("websockets:client_removed", |s, args| s.on_websocket_removed(args.first().and_then(|v| v.as_u64()).unwrap_or(0)));
        on!("server:gcode_response", |s, args| {
            if s.gcode_terminal_enabled.get() {
                let r = args.first().and_then(|v| v.as_str()).unwrap_or("");
                let resp: Vec<&str> = r.trim().split('\n').map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
                s.send_sp("term_update", json!({"response": resp}));
            }
        });
        on!("klippy_connection:gcode_received", |s, args| {
            if s.gcode_terminal_enabled.get() {
                let r = args.first().and_then(|v| v.as_str()).unwrap_or("");
                let cmds: Vec<&str> = r.split_whitespace().collect();
                s.send_sp("term_update", json!({"command": cmds}));
            }
        });
        on!("power:power_changed", |s, args| {
            let d = args.first().cloned().unwrap_or(Value::Null);
            if !s.power_id.is_empty() && d.get("device").and_then(|v| v.as_str()) == Some(s.power_id.as_str()) {
                s.send_sp("power_controller", json!({"on": d.get("status").and_then(|v| v.as_str()) == Some("on")}));
            }
        });
    }

    fn interval(&self, k: &str) -> f64 {
        self.intervals.borrow().get(k).copied().unwrap_or(0.0)
    }

    // ***** Connection *****

    async fn connect(self: Rc<Self>) {
        let mut log_connect = true;
        while !self.is_closing.get() {
            let mut url = self.connect_url.borrow().clone();
            if let Some(t) = self.reconnect_token.borrow().as_ref() {
                url = format!("{}/{}", url, t);
            }
            if log_connect {
                log::info!("Connecting To SimplyPrint: {}", url);
                log_connect = false;
            }
            match tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(url.as_str())).await {
                Ok(Ok((ws, _))) => {
                    self.last_ping_received.set(now());
                    log::info!("Connected to SimplyPrint Cloud");
                    self.clone().read_messages(ws).await;
                    log_connect = true;
                }
                res => {
                    let t = now();
                    if t - self.last_err_log_time.get() > CONNECTION_ERROR_LOG_TIME {
                        self.last_err_log_time.set(t);
                        let e = match res {
                            Ok(Err(e)) => e.to_string(),
                            _ => "Timeout while connecting".into(),
                        };
                        log::error!("Failed to connect to SimplyPrint\n{}", e);
                    }
                }
            }
            if !self.is_closing.get() {
                tokio::time::sleep(Duration::from_secs_f64(self.reconnect_delay.get())).await;
            }
        }
    }

    async fn read_messages(
        self: Rc<Self>,
        ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    ) {
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
        *self.ws_tx.borrow_mut() = Some(tx);
        let (code, reason) = loop {
            tokio::select! {
                msg = stream.next() => match msg {
                    Some(Ok(Message::Text(t))) => self.process_message(t.as_str()),
                    Some(Ok(Message::Ping(_))) => self.last_ping_received.set(now()),
                    Some(Ok(Message::Close(f))) => break f.map(|f| (Some(u16::from(f.code)), Some(f.reason.to_string()))).unwrap_or((None, None)),
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break (None, None),
                },
                out = rx.recv() => {
                    if let Some(m) = out {
                        let _ = sink.send(m).await;
                    }
                }
            }
        };
        *self.ws_tx.borrow_mut() = None;
        self.ping_sp_timer.stop();
        let ping_time = now() - self.last_ping_received.get();
        log::info!(
            "SimplyPrint Disconnected - Code: {}, Reason: {}, Server Ping Time Elapsed: {}",
            code.map(|c| c.to_string()).unwrap_or("None".into()),
            reason.unwrap_or("None".into()),
            crate::pyfmt::py_float_repr(ping_time)
        );
        self.connected.set(false);
    }

    fn process_message(self: &Rc<Self>, msg: &str) {
        self.logger.info(&format!("received: {}", msg));
        let packet: Value = match serde_json::from_str(msg) {
            Ok(p) => p,
            Err(_) => {
                log::debug!("Invalid message, not JSON: {}", msg);
                return;
            }
        };
        let event = packet.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let data = packet.get("data").cloned().filter(|d| !d.is_null());
        match event.as_str() {
            "connected" => {
                log::info!("SimplyPrint Reports Connection Success");
                self.connected.set(true);
                *self.reconnect_token.borrow_mut() = None;
                if let Some(d) = &data {
                    if d.get("in_setup").and_then(|v| v.as_i64()).unwrap_or(0) == 1 {
                        self.is_set_up.set(false);
                        self.save_item("printer_id", Value::Null);
                        self.save_item("printer_name", Value::Null);
                        if let Some(sid) = d.get("short_id") {
                            let s = self.clone();
                            let sid = crate::pyfmt::py_str(sid);
                            spawn(async move { s.announce_setup(&sid).await });
                        }
                    }
                    if let Some(Value::Object(iv)) = d.get("interval") {
                        self.update_intervals(iv);
                    }
                    *self.reconnect_token.borrow_mut() = d.get("reconnect_token").and_then(|v| v.as_str()).map(String::from);
                    if let Some(name) = d.get("name").filter(|n| !n.is_null()) {
                        self.save_item("printer_name", name.clone());
                    }
                }
                self.reconnect_delay.set(1.0);
                self.push_initial_state();
                let w = Rc::downgrade(self);
                self.ping_sp_timer.start(
                    0.0,
                    Rc::new(move || match w.upgrade() {
                        Some(s) => {
                            s.last_sp_ping.set(now());
                            s.send_sp("ping", Value::Null);
                            s.interval("ping")
                        }
                        None => 3600.0,
                    }),
                );
            }
            "error" => {
                log::info!("SimplyPrint Connection Error: {}", py_repr(&data.clone().unwrap_or(Value::Null)));
                self.reconnect_delay.set(30.0);
                *self.reconnect_token.borrow_mut() = None;
            }
            "new_token" => {
                let d = match data {
                    None => {
                        log::debug!("Invalid message, no data");
                        return;
                    }
                    Some(d) => d,
                };
                if d.get("no_exist").and_then(|v| v.as_bool()) == Some(true) && self.is_set_up.get() {
                    self.is_set_up.set(false);
                    self.save_item("printer_id", Value::Null);
                }
                let token = match d.get("token") {
                    Some(Value::String(t)) => {
                        log::info!("SimplyPrint Token Received");
                        Value::String(t.clone())
                    }
                    other => {
                        log::debug!("Invalid token received: {}", py_repr(&other.cloned().unwrap_or(Value::Null)));
                        Value::Null
                    }
                };
                self.save_item("printer_token", token);
                self.set_ws_url();
                if let Some(sid) = d.get("short_id") {
                    match sid {
                        Value::String(s) => {
                            let sp = self.clone();
                            let s = s.clone();
                            spawn(async move { sp.announce_setup(&s).await });
                        }
                        other => self.logger.debug(&format!("Invalid short_id received: {}", py_repr(other))),
                    }
                }
            }
            "complete_setup" => {
                let d = match data {
                    None => {
                        log::debug!("Invalid message, no data");
                        return;
                    }
                    Some(d) => d,
                };
                let pid = d.get("printer_id").cloned().unwrap_or(Value::Null);
                if pid.is_null() {
                    log::debug!("Invalid printer id, received null (None) value");
                }
                self.save_item("printer_id", json!(crate::pyfmt::py_str(&pid)));
                self.set_ws_url();
                self.save_item("temp_short_setup_id", Value::Null);
                let s = self.clone();
                spawn(async move { s.remove_setup_announcement().await });
            }
            "demand" => {
                let mut d = match data {
                    Some(Value::Object(d)) => d,
                    _ => {
                        log::debug!("Invalid message, no data");
                        return;
                    }
                };
                let demand = d.shift_remove("demand").map(|v| crate::pyfmt::py_str(&v)).unwrap_or("unknown".into());
                self.process_demand(&demand, d);
            }
            "interval_change" => {
                if let Some(Value::Object(iv)) = &data {
                    self.update_intervals(iv);
                }
            }
            "pong" => {
                let diff = now() - self.last_sp_ping.get();
                self.send_sp("latency", json!({"ms": (diff * 1000.0 + 0.5) as i64}));
            }
            _ => log::debug!("Unknown event: {}", msg),
        }
    }

    fn process_demand(self: &Rc<Self>, demand: &str, args: Map<String, Value>) {
        let kconn = server().klippy();
        match demand {
            "pause" | "resume" | "cancel" => {
                if !kconn.is_connected() {
                    return;
                }
                let s = self.clone();
                let d = demand.to_string();
                spawn(async move { s.request_print_action(&d).await });
            }
            "terminal" => {
                if let Some(e) = args.get("enabled") {
                    self.gcode_terminal_enabled.set(crate::pyfmt::py_truthy(e));
                }
            }
            "gcode" => {
                if !kconn.is_connected() {
                    return;
                }
                let list: Vec<String> = args.get("list").and_then(|v| v.as_array()).map(|a| a.iter().map(crate::pyfmt::py_str).collect()).unwrap_or_default();
                let ident = args.get("identifier").filter(|v| !v.is_null()).map(crate::pyfmt::py_str);
                if !list.is_empty() {
                    let script = list.join("\n");
                    let s = self.clone();
                    spawn(async move {
                        let (success, msg) = match server().klippy_apis().run_gcode(&script).await {
                            Ok(_) => (true, Value::Null),
                            Err(e) => (false, json!(e.message)),
                        };
                        if let Some(id) = ident {
                            s.send_sp("gcode_executed", json!({"identifier": id, "success": success, "message": msg}));
                        }
                    });
                }
            }
            "webcam_snapshot" => {
                let s = self.clone();
                spawn(async move { s.post_image(args).await });
            }
            "file" => {
                let url = match args.get("url") {
                    Some(Value::String(u)) => u.clone(),
                    _ => {
                        log::debug!("Invalid url in message");
                        return;
                    }
                };
                let start = args.get("auto_start").map(crate::pyfmt::py_truthy).unwrap_or(false);
                let jid = args.get("job_id").and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0);
                self.current_job_id.set(if jid != 0 { Some(jid) } else { None });
                let s = self.clone();
                let h = spawn(async move { s.download_sp_file(&url, start).await });
                *self.download_task.borrow_mut() = Some(h);
            }
            "start_print" => {
                if kconn.is_connected() && self.cache.borrow().state == "operational" {
                    let s = self.clone();
                    spawn(async move { s.start_print().await });
                } else {
                    log::debug!("Failed to start print");
                }
            }
            "system_restart" => {
                spawn(async {
                    Self::call_internal_api("machine.reboot", Map::new()).await;
                });
            }
            "system_shutdown" => {
                spawn(async {
                    Self::call_internal_api("machine.shutdown", Map::new()).await;
                });
            }
            "api_restart" | "api_shutdown" => {
                let action = if demand == "api_restart" { "restart" } else { "shutdown" };
                spawn(async move {
                    if let Some(m) = server().lookup_component::<crate::components::machine::Machine>("machine") {
                        let _ = m.do_service_action(action, "moonraker").await;
                    }
                });
            }
            "psu_on" | "psu_off" => {
                if !self.power_id.is_empty() {
                    if let Some(p) = server().lookup_component::<crate::components::power::PrinterPower>("power") {
                        p.set_device_power(&self.power_id, if demand == "psu_on" { "on" } else { "off" }, false);
                    }
                }
            }
            "test_webcam" => {
                let s = self.clone();
                spawn(async move {
                    s.test_connection().await;
                    s.send_sp("webcam_status", json!({"connected": s.webcam_connected.get()}));
                });
            }
            _ => log::debug!("Unknown demand: {}", demand),
        }
    }

    fn save_item(&self, name: &str, data: Value) {
        if data.is_null() {
            self.sp_info.borrow_mut().shift_remove(name);
            let _ = self.spdb.pop(name, Some(Value::Null));
        } else {
            self.sp_info.borrow_mut().insert(name.into(), data.clone());
            let _ = self.spdb.insert(name, data);
        }
    }

    async fn call_internal_api(method: &str, args: Map<String, Value>) -> Option<Value> {
        let it = crate::components::application::InternalTransport;
        it.call_method(method, args.into_iter().collect()).await.ok()
    }

    fn set_ws_url(&self) {
        let info = self.sp_info.borrow();
        let token = info.get("printer_token").filter(|v| !v.is_null()).map(crate::pyfmt::py_str);
        let printer_id = info.get("printer_id").filter(|v| !v.is_null()).map(crate::pyfmt::py_str);
        let host = if self.test { "testws3.simplyprint.io" } else { "ws.simplyprint.io" };
        let ep = format!("wss://{}/{}/p", host, SP_VERSION);
        let mut url = format!("{}/0/0", ep);
        if let Some(t) = token {
            match printer_id {
                None => url = format!("{}/0/{}", ep, t),
                Some(p) => {
                    self.is_set_up.set(true);
                    url = format!("{}/{}/{}", ep, p, t);
                }
            }
        }
        *self.connect_url.borrow_mut() = url;
    }

    fn update_intervals(&self, iv: &Map<String, Value>) {
        let mut i = self.intervals.borrow_mut();
        for (k, v) in iv {
            i.insert(k.clone(), v.as_f64().unwrap_or(0.0) / 1000.0);
        }
        let dbg: Map<String, Value> = i.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        log::debug!("Intervals Updated: {}", py_repr(&Value::Object(dbg)));
    }

    async fn announce_setup(&self, short_id: &str) {
        let _g = self.announce_mutex.lock().await;
        let eid = self.sp_info.borrow().get("announcement_id").filter(|v| !v.is_null()).map(crate::pyfmt::py_str);
        if eid.is_some() && self.sp_info.borrow().get("temp_short_setup_id").and_then(|v| v.as_str()) == Some(short_id) {
            return;
        }
        let ann: Rc<crate::components::announcements::Announcements> = match server().lookup_component("announcements") {
            Some(a) => a,
            None => return,
        };
        if let Some(e) = &eid {
            let _ = ann.remove_announcement(e).await;
        }
        self.save_item("temp_short_setup_id", json!(short_id));
        let entry = ann.add_internal_announcement(
            "SimplyPrint Setup Request",
            &format!(
                "SimplyPrint is ready to complete setup for your printer. Please log in to your account and enter the following setup code:\n\n{}\n\n",
                short_id
            ),
            "https://simplyprint.io",
            "high",
            "simplyprint",
        );
        self.save_item("announcement_id", entry.get("entry_id").cloned().unwrap_or(Value::Null));
    }

    async fn remove_setup_announcement(&self) {
        let _g = self.announce_mutex.lock().await;
        let eid = match self.sp_info.borrow().get("announcement_id").filter(|v| !v.is_null()).map(crate::pyfmt::py_str) {
            None => return,
            Some(e) => e,
        };
        self.save_item("announcement_id", Value::Null);
        if let Some(ann) = server().lookup_component::<crate::components::announcements::Announcements>("announcements") {
            let _ = ann.remove_announcement(&eid).await;
        }
    }

    async fn request_print_action(&self, action: &str) {
        let cur_state = self.cache.borrow().state.clone();
        let kapis = server().klippy_apis();
        self.print_request_event.clear();
        let mut requested = false;
        let ret = match action {
            "pause" if cur_state == "printing" => {
                self.update_state("pausing");
                requested = true;
                kapis.pause_print().await
            }
            "resume" if cur_state == "paused" => {
                self.update_state("resuming");
                requested = true;
                kapis.resume_print().await
            }
            "cancel" if cur_state == "printing" || cur_state == "paused" => {
                self.update_state("cancelling");
                requested = true;
                kapis.cancel_print().await
            }
            _ => Ok(json!("")),
        };
        // python passes default=None so request errors resolve to None
        if requested && ret.is_err() {
            let _ = tokio::time::timeout(Duration::from_secs(1), self.print_request_event.wait()).await;
            self.update_state_from_klippy();
        }
    }

    // ***** Klippy events *****

    async fn on_klippy_ready(self: Rc<Self>) {
        let js: Option<Rc<crate::components::job_state::JobState>> = server().lookup_component("job_state");
        let last_stats = js.map(|j| j.get_last_stats()).unwrap_or_default();
        match last_stats.get("state").and_then(|v| v.as_str()) {
            None => {
                log::error!("KeyError: 'state'");
                return;
            }
            Some("printing") => self.on_print_started(&Value::Object(last_stats.clone()), false),
            Some(_) => self.update_state("operational"),
        }
        let kapis = server().klippy_apis();
        let mut q = Map::new();
        q.insert("heaters".into(), Value::Null);
        let query = kapis.query_objects(&q).await.ok();
        let mut sub_objs = Map::new();
        sub_objs.insert("display_status".into(), json!(["progress"]));
        sub_objs.insert("virtual_sdcard".into(), json!(["file_position", "progress"]));
        sub_objs.insert("bed_mesh".into(), json!(["mesh_matrix", "mesh_min", "mesh_max"]));
        sub_objs.insert("toolhead".into(), json!(["extruder"]));
        sub_objs.insert("gcode_move".into(), json!(["gcode_position"]));
        sub_objs.insert("exclude_object".into(), json!(["objects", "excluded_objects", "current_object"]));
        let mut has_amb = false;
        let cfg_amb = self.amb.sensor_name.clone();
        if let Some(q) = query {
            let heaters = q.get("heaters").cloned().unwrap_or(json!({}));
            let mut avail: Vec<String> = heaters.get("available_heaters").and_then(|v| v.as_array()).map(|a| a.iter().map(crate::pyfmt::py_str).collect()).unwrap_or_default();
            avail.sort();
            log::debug!("SimplyPrint: Heaters Detected: {}", py_repr(&json!(avail)));
            let mut htrs = self.heaters.borrow_mut();
            for h in avail {
                if h.starts_with("extruder") {
                    sub_objs.insert(h.clone(), json!(["temperature", "target"]));
                    let tool = if h == "extruder" { "tool0".to_string() } else { format!("tool{}", &h[8..]) };
                    htrs.retain(|(k, _)| *k != h);
                    htrs.push((h, tool));
                } else if h == "heater_bed" {
                    sub_objs.insert(h.clone(), json!(["temperature", "target"]));
                    htrs.retain(|(k, _)| *k != h);
                    htrs.push((h, "bed".into()));
                }
            }
            drop(htrs);
            let sensors: Vec<String> = heaters.get("available_sensors").and_then(|v| v.as_array()).map(|a| a.iter().map(crate::pyfmt::py_str).collect()).unwrap_or_default();
            if !cfg_amb.is_empty() {
                if sensors.contains(&cfg_amb) {
                    has_amb = true;
                    sub_objs.insert(cfg_amb.clone(), json!(["temperature"]));
                } else {
                    log::info!("SimplyPrint: Ambient sensor {} not configured in Klipper", cfg_amb);
                }
            }
        }
        if !self.filament_sensor.is_empty() {
            let objects = kapis.get_object_list().await.unwrap_or_default();
            if objects.iter().any(|o| o.as_str() == Some(self.filament_sensor.as_str())) {
                sub_objs.insert(self.filament_sensor.clone(), json!(["filament_detected"]));
            }
        }
        let transport: Rc<dyn ApiTransport> = self.clone();
        let status = kapis.subscribe_from_transport(&sub_objs, transport, false).await.unwrap_or(json!({}));
        if let Value::Object(status) = status {
            if !status.is_empty() {
                log::debug!("SimplyPrint: Got Initial Status: {}", py_repr(&Value::Object(status.clone())));
                *self.printer_status.borrow_mut() = status.clone();
                self.update_temps(1.0);
                self.next_temp_update_time.set(0.0);
                if status.contains_key("bed_mesh") {
                    self.send_mesh_data();
                }
                if let Some(e) = status.get("toolhead").and_then(|t| t.get("extruder")).and_then(|v| v.as_str()) {
                    self.send_active_extruder(e);
                }
                if let Some(gm) = status.get("gcode_move") {
                    self.layer_detect.borrow_mut().update(gm.get("gcode_position").unwrap_or(&Value::Null));
                }
                if let Some(eo) = status.get("exclude_object").and_then(|v| v.as_object()) {
                    self.handle_exclude_object(eo);
                }
                if !self.filament_sensor.is_empty() {
                    if let Some(fs) = status.get(&self.filament_sensor) {
                        self.send_filament_state(fs);
                    }
                }
                if has_amb {
                    if let Some(a) = status.get(&cfg_amb) {
                        self.update_ambient(a, SAMPLE_CHECK_TIME);
                    }
                }
            }
        }
        if !has_amb {
            self.amb_start();
        }
        let w = Rc::downgrade(&self);
        self.printer_info_timer.start(
            1.0,
            Rc::new(move || match w.upgrade() {
                Some(s) => {
                    if s.cache.borrow().state == "printing" {
                        s.update_job_progress();
                    }
                    s.interval("job")
                }
                None => 3600.0,
            }),
        );
    }

    fn send_filament_state(&self, fs: &Value) {
        let detected = fs.get("filament_detected").map(crate::pyfmt::py_truthy).unwrap_or(false);
        let state = if detected { "loaded" } else { "runout" };
        self.cache.borrow_mut().filament_state = state.into();
        self.send_sp("filament_sensor", json!({"state": state}));
    }

    fn on_websocket_identified(&self, uid: u64) {
        let conn = match server().wsm().get_client(uid) {
            Some(c) => c,
            None => return,
        };
        let cd = conn.client_data();
        if self.cache.borrow().current_wsid.is_none() && cd.get("type").map(|s| s.as_str()) == Some("web") {
            let ui = json!({"ui": cd.get("name"), "ui_version": cd.get("version")});
            {
                let mut c = self.cache.borrow_mut();
                for (k, v) in ui.as_object().unwrap() {
                    c.firmware_info.insert(k.clone(), v.clone());
                }
                c.current_wsid = Some(uid);
            }
            self.send_sp("machine_data", ui);
        }
    }

    fn on_websocket_removed(&self, uid: u64) {
        if self.cache.borrow().current_wsid != Some(uid) {
            return;
        }
        let ui = self.get_ui_info();
        let diff = diff_obj(&ui, &self.cache.borrow().firmware_info);
        if !diff.is_empty() {
            self.cache.borrow_mut().firmware_info.extend(ui.clone());
            self.send_sp("machine_data", Value::Object(ui));
        }
    }

    fn on_klippy_startup(&self, state: KlippyState) {
        if state != KlippyState::READY {
            self.update_state("error");
            self.send_sp("printer_error", json!({"error": server().klippy().state_message()}));
        }
        self.send_sp("connection", json!({"new": "connected"}));
        self.send_firmware_data();
    }

    fn on_klippy_disconnected(&self) {
        self.update_state("offline");
        self.send_sp("connection", json!({"new": "disconnected"}));
        self.amb_stop();
        self.printer_info_timer.stop();
        self.cache.borrow_mut().reset_print_state();
        self.printer_status.borrow_mut().clear();
    }

    fn on_job_state_changed(self: &Rc<Self>, args: Vec<Value>) {
        let evt = args.first().and_then(|v| v.as_str()).unwrap_or("").to_string();
        let prev = args.get(1).cloned().unwrap_or(json!({}));
        let new = args.get(2).cloned().unwrap_or(json!({}));
        match evt.as_str() {
            "started" => self.on_print_started(&new, true),
            "paused" => {
                self.send_sp("job_info", json!({"paused": true}));
                self.update_state("paused");
                self.layer_detect.borrow_mut().stop();
            }
            "resumed" => {
                self.update_state("printing");
                self.layer_detect.borrow_mut().resume();
            }
            "cancelled" | "complete" | "error" => {
                self.check_job_started(&new);
                self.reset_file();
                let payload = match evt.as_str() {
                    "cancelled" => json!({"cancelled": true}),
                    "complete" => json!({"finished": true}),
                    _ => json!({"failed": true, "error": new.get("message").cloned().unwrap_or(json!("Unknown Error"))}),
                };
                self.send_job_event(payload.as_object().cloned().unwrap());
                self.update_state_from_klippy();
                self.cache.borrow_mut().job_info.clear();
                self.layer_detect.borrow_mut().stop();
            }
            "standby" => {
                self.update_state_from_klippy();
                self.cache.borrow_mut().job_info.clear();
                self.layer_detect.borrow_mut().stop();
            }
            _ => log::info!("No defined callback for Job Event: {}", evt),
        }
        let _ = prev;
    }

    fn on_print_started(&self, new_stats: &Value, need_start_event: bool) {
        self.update_state("printing");
        let filename = new_stats.get("filename").map(crate::pyfmt::py_str).unwrap_or_default();
        let mut job_info = Map::new();
        job_info.insert("filename".into(), json!(filename));
        let fm: Option<Rc<crate::components::file_manager::FileManager>> = server().lookup_component("file_manager");
        let metadata = fm.map(|f| f.get_file_metadata(&filename)).unwrap_or(json!({}));
        if let Some(f) = metadata.get("filament_total").and_then(|v| v.as_f64()) {
            job_info.insert("filament".into(), json!(crate::pyfmt::py_round(f, 0) as i64));
        }
        if let Some(t) = metadata.get("estimated_time").filter(|v| !v.is_null()) {
            job_info.insert("time".into(), t.clone());
        }
        {
            let mut c = self.cache.borrow_mut();
            c.metadata = metadata.clone();
            c.job_info.extend(job_info.clone());
        }
        if need_start_event {
            job_info.insert("started".into(), json!(true));
        }
        self.layer_detect.borrow_mut().start(&metadata);
        self.send_job_event(job_info);
    }

    fn check_job_started(&self, new_stats: &Value) {
        if self.cache.borrow().job_info.is_empty() {
            let mut ji = Map::new();
            ji.insert("filename".into(), new_stats.get("filename").cloned().unwrap_or(json!("")));
            ji.insert("started".into(), json!(true));
            self.send_job_event(ji);
        }
    }

    fn reset_file(&self) {
        let cur = self.cache.borrow().job_info.get("filename").map(crate::pyfmt::py_str).unwrap_or_default();
        let last = self.last_started.borrow().clone();
        if !last.is_empty() && last == cur {
            spawn(async {
                let _ = server().klippy_apis().run_gcode("SDCARD_RESET_FILE").await;
            });
        }
        self.last_started.borrow_mut().clear();
    }

    fn on_proc_update(&self, stats: Value) {
        let cpu = match stats.get("system_cpu_usage") {
            Some(c) if crate::pyfmt::py_truthy(c) => c.clone(),
            _ => return,
        };
        let t = now();
        if t - self.last_cpu_update_time.get() < self.interval("cpu") {
            return;
        }
        self.last_cpu_update_time.set(t);
        let mut mem_pct = 0.0;
        if let Some(m) = stats.get("system_memory").filter(|m| crate::pyfmt::py_truthy(m)) {
            mem_pct = m["used"].as_f64().unwrap_or(0.0) / m["total"].as_f64().unwrap_or(1.0) * 100.0;
        }
        let mut cpu_data = Map::new();
        cpu_data.insert("usage".into(), json!((cpu["cpu"].as_f64().unwrap_or(0.0) + 0.5) as i64));
        cpu_data.insert("memory".into(), json!((mem_pct + 0.5) as i64));
        cpu_data.insert("flags".into(), self.cache.borrow().throttled_state.get("bits").cloned().unwrap_or(json!(0)));
        if let Some(temp) = stats.get("cpu_temp").and_then(|v| v.as_f64()) {
            cpu_data.insert("temp".into(), json!((temp + 0.5) as i64));
        }
        let diff = diff_obj(&cpu_data, &self.cache.borrow().cpu_info);
        if !diff.is_empty() {
            self.cache.borrow_mut().cpu_info.extend(cpu_data);
            self.send_sp("cpu", Value::Object(diff));
        }
    }

    fn handle_exclude_object(&self, eo: &Map<String, Value>) {
        let diff = diff_obj(eo, &self.cache.borrow().exclude_object_status);
        if diff.is_empty() {
            return;
        }
        self.cache.borrow_mut().exclude_object_status = eo.clone();
        if let Some(objs) = diff.get("objects").filter(|o| crate::pyfmt::py_truthy(o)) {
            self.send_sp("objects", objs.clone());
        }
        let mut job = Map::new();
        if let Some(co) = diff.get("current_object") {
            job.insert("object".into(), co.clone());
        }
        if let Some(sk) = diff.get("excluded_objects").filter(|o| crate::pyfmt::py_truthy(o)) {
            job.insert("skipped_objects".into(), sk.clone());
        }
        if !job.is_empty() {
            self.send_sp("job_info", Value::Object(job));
        }
    }

    fn update_job_progress(&self) {
        let mut job_info = Map::new();
        let md = self.cache.borrow().metadata.clone();
        let js: Option<Rc<crate::components::job_state::JobState>> = server().lookup_component("job_state");
        let last_stats = js.map(|j| j.get_last_stats()).unwrap_or_default();
        if let Some(est) = md.get("estimated_time").and_then(|v| v.as_f64()) {
            let duration = last_stats.get("print_duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let time_left = ((est - duration + 0.5) as i64).max(0);
            let last_time_left = self.cache.borrow().job_info.get("time").and_then(|v| v.as_f64()).unwrap_or(time_left as f64 + 60.0);
            let time_diff = last_time_left - time_left as f64;
            if (time_left < 60 || time_diff >= 30.0) && time_left as f64 != last_time_left {
                job_info.insert("time".into(), json!(time_left));
            }
        }
        let ps = self.printer_status.borrow().clone();
        let mut progress: Option<f64> = ps.get("display_status").and_then(|d| d.get("progress")).and_then(|v| v.as_f64()).filter(|p| *p != 0.0);
        if progress.is_none() {
            if let Some(vs) = ps.get("virtual_sdcard") {
                if let Some(fp) = vs.get("file_position").and_then(|v| v.as_f64()) {
                    let start = md.get("gcode_start_byte").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let end = md.get("gcode_end_byte").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let len = end - start;
                    if start != 0.0 && end != 0.0 && len > 0.0 {
                        progress = Some(((fp - start) / len).clamp(0.0, 1.0)).filter(|p| *p != 0.0);
                    }
                }
                if progress.is_none() {
                    progress = vs.get("progress").and_then(|v| v.as_f64()).filter(|p| *p != 0.0);
                }
            }
        }
        if let Some(p) = progress {
            let pct = (p * 100.0 + 0.5) as i64;
            if Some(pct) != Some(self.cache.borrow().job_info.get("progress").and_then(|v| v.as_i64()).unwrap_or(0)) {
                job_info.insert("progress".into(), json!(pct));
            }
        }
        let layer = last_stats
            .get("info")
            .and_then(|i| i.get("current_layer"))
            .and_then(|v| v.as_i64())
            .unwrap_or(self.layer_detect.borrow().layer);
        if Some(layer) != Some(self.cache.borrow().job_info.get("layer").and_then(|v| v.as_i64()).unwrap_or(-1)) {
            job_info.insert("layer".into(), json!(layer));
        }
        if !job_info.is_empty() {
            self.cache.borrow_mut().job_info.extend(job_info.clone());
            self.send_sp("job_info", Value::Object(job_info));
        }
    }

    fn update_temps(&self, eventtime: f64) {
        if eventtime < self.next_temp_update_time.get() {
            return;
        }
        let mut need_rapid = false;
        let mut temp_data = Map::new();
        let heaters = self.heaters.borrow().clone();
        let ps = self.printer_status.borrow().clone();
        for (obj, key) in heaters {
            let st = match ps.get(&obj) {
                Some(s) => s,
                None => continue,
            };
            let reported = st.get("temperature").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let target = st.get("target").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let ret = [(reported + 0.5) as i64, (target + 0.5) as i64];
            let last = self
                .cache
                .borrow()
                .temps
                .get(&key)
                .and_then(|v| v.as_array().map(|a| [a.first().and_then(|x| x.as_f64()).unwrap_or(-100.0), a.get(1).and_then(|x| x.as_f64()).unwrap_or(-100.0)]))
                .unwrap_or([-100.0, -100.0]);
            if ret[1] as f64 == last[1] {
                let seeking = if ret[1] != 0 { (ret[1] - ret[0]).abs() > 5 } else { ret[0] >= self.amb.ambient.get() + 25 };
                need_rapid |= seeking;
                let lr = self.last_received_temps.borrow().get(&key).copied();
                if let (Some(lr), false) = (lr, seeking) {
                    if (reported - lr).abs() < 0.75 {
                        self.last_received_temps.borrow_mut().remove(&key);
                        continue;
                    }
                }
                if ret[0] as f64 == last[0] {
                    self.last_received_temps.borrow_mut().insert(key.clone(), reported);
                    continue;
                }
                temp_data.insert(key.clone(), json!([ret[0]]));
            } else {
                temp_data.insert(key.clone(), json!(ret));
            }
            self.last_received_temps.borrow_mut().insert(key.clone(), reported);
            self.cache.borrow_mut().temps.insert(key, json!(ret));
        }
        if need_rapid {
            let tt = self.interval("temps_target");
            self.next_temp_update_time.set(if tt < 0.2501 { 0.0 } else { eventtime + tt });
        } else {
            self.next_temp_update_time.set(eventtime + self.interval("temps"));
        }
        if !temp_data.is_empty() {
            self.send_sp("temps", Value::Object(temp_data));
        }
    }

    fn update_state_from_klippy(&self) {
        let st = server().klippy().state();
        let sp = if st == KlippyState::READY {
            "operational"
        } else if st == KlippyState::ERROR || st == KlippyState::SHUTDOWN {
            "error"
        } else {
            "offline"
        };
        self.update_state(sp);
    }

    fn update_state(&self, new_state: &str) {
        if self.cache.borrow().state == new_state {
            return;
        }
        self.cache.borrow_mut().state = new_state.into();
        self.send_sp("state_change", json!({"new": new_state}));
        if new_state == "operational" {
            self.print_ready_event.set();
        }
    }

    fn send_mesh_data(&self) {
        let mesh = self.printer_status.borrow().get("bed_mesh").cloned().unwrap_or(json!({}));
        self.cache.borrow_mut().mesh = mesh.clone();
        self.send_sp("mesh_data", mesh);
    }

    fn send_job_event(&self, mut job_info: Map<String, Value>) {
        if self.connected.get() {
            self.send_sp("job_info", Value::Object(job_info));
        } else {
            job_info.extend(self.cache.borrow().job_info.clone());
            job_info.insert("delay".into(), json!(now()));
            let mut m = self.missed_job_events.borrow_mut();
            m.push(job_info);
            if m.len() > 10 {
                m.remove(0);
            }
        }
    }

    fn get_ui_info(&self) -> Map<String, Value> {
        let mut ui = Map::new();
        ui.insert("ui".into(), Value::Null);
        ui.insert("ui_version".into(), Value::Null);
        self.cache.borrow_mut().current_wsid = None;
        let conns = server().wsm().get_clients_by_type("web");
        if let Some(longest) = conns.first() {
            let cd = longest.client_data();
            ui.insert("ui".into(), json!(cd.get("name")));
            ui.insert("ui_version".into(), json!(cd.get("version")));
            self.cache.borrow_mut().current_wsid = Some(longest.uid());
        }
        ui
    }

    async fn send_machine_data(&self) {
        let srv = server();
        let mut data = self.get_ui_info();
        data.insert("api".into(), json!("Moonraker"));
        data.insert("api_version".into(), srv.get_app_args().get("software_version").cloned().unwrap_or(Value::Null));
        data.insert("sp_version".into(), json!(COMPONENT_VERSION));
        let machine: Rc<crate::components::machine::Machine> = match srv.lookup_component("machine") {
            Some(m) => m,
            None => return,
        };
        let sys_info = machine.get_system_info();
        let pyver: Vec<String> = sys_info
            .get("python")
            .and_then(|p| p.get("version"))
            .and_then(|v| v.as_array())
            .map(|a| a.iter().take(3).map(crate::pyfmt::py_str).collect())
            .unwrap_or_default();
        data.insert("python_version".into(), json!(pyver.join(".")));
        let cpu = sys_info.get("cpu_info").cloned().unwrap_or(json!({}));
        let mut model = cpu.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if model.is_empty() || model.chars().all(|c| c.is_ascii_digit()) {
            model = cpu.get("cpu_desc").and_then(|v| v.as_str()).unwrap_or("Unknown").to_string();
        }
        data.insert("machine".into(), json!(model));
        data.insert("os".into(), sys_info.get("distribution").and_then(|d| d.get("name")).cloned().unwrap_or(json!("Unknown")));
        let pub_intf = machine.get_public_network().await;
        data.insert("is_ethernet".into(), json!((!pub_intf["is_wifi"].as_bool().unwrap_or(false)) as i64));
        data.insert("ssid".into(), pub_intf.get("ssid").cloned().unwrap_or(json!("")));
        data.insert("local_ip".into(), pub_intf.get("address").cloned().unwrap_or(json!("Unknown")));
        data.insert("hostname".into(), pub_intf["hostname"].clone());
        data.insert("core_count".into(), json!(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)));
        if let Some(mem) = cpu.get("total_memory").and_then(|v| v.as_i64()) {
            data.insert("total_memory".into(), json!(mem * 1024));
        }
        self.cache.borrow_mut().machine_info = data.clone();
        self.send_sp("machine_data", Value::Object(data));
    }

    fn send_firmware_data(&self) {
        let kinfo = server().get_klippy_info();
        let version = match kinfo.get("software_version").and_then(|v| v.as_str()) {
            None => return,
            Some(v) => v.to_string(),
        };
        let mut firmware_date = String::new();
        let kpath = PathBuf::from(kinfo.get("klipper_path").and_then(|v| v.as_str()).unwrap_or("")).join("klippy");
        if kpath.is_dir() {
            if let Ok(m) = std::fs::metadata(&kpath).and_then(|m| m.modified()) {
                firmware_date = chrono::DateTime::<chrono::Utc>::from(m).format("%a %b %e %H:%M:%S %Y").to_string();
            }
        }
        let unsafe_ = version.ends_with("-dirty") || version == "?";
        let version = if unsafe_ { version.rsplit_once('-').map(|(a, _)| a.to_string()).unwrap_or(version) } else { version };
        let fw = json!({
            "firmware": "Klipper",
            "firmware_version": version,
            "firmware_date": firmware_date,
            "firmware_link": "https://github.com/Klipper3d/klipper",
        });
        let fw = fw.as_object().cloned().unwrap();
        let diff = diff_obj(&fw, &self.cache.borrow().firmware_info);
        if !diff.is_empty() {
            self.cache.borrow_mut().firmware_info = fw;
            self.send_sp("firmware", json!({"fw": diff, "raw": false, "unsafe": unsafe_}));
        }
    }

    fn send_active_extruder(&self, new_extruder: &str) {
        let tool = if new_extruder == "extruder" { "T0".to_string() } else { format!("T{}", new_extruder.get(8..).unwrap_or("")) };
        if tool == self.cache.borrow().active_extruder {
            return;
        }
        self.cache.borrow_mut().active_extruder = tool.clone();
        self.send_sp("tool", json!({"new": tool}));
    }

    fn push_initial_state(self: &Rc<Self>) {
        let st = self.cache.borrow().state.clone();
        self.send_sp("state_change", json!({"new": st}));
        let temps = self.cache.borrow().temps.clone();
        if !temps.is_empty() {
            self.send_sp("temps", Value::Object(temps));
        }
        let fw = self.cache.borrow().firmware_info.clone();
        if !fw.is_empty() {
            self.send_sp("firmware", json!({"fw": fw, "raw": false}));
        }
        let t = now();
        let missed = std::mem::take(&mut *self.missed_job_events.borrow_mut());
        for mut evt in missed {
            let d = evt.get("delay").and_then(|v| v.as_f64()).unwrap_or(t);
            evt.insert("delay".into(), json!(((t - d) + 0.5) as i64));
            self.send_sp("job_info", Value::Object(evt));
        }
        let ae = self.cache.borrow().active_extruder.clone();
        if !ae.is_empty() {
            self.send_sp("tool", json!({"new": ae}));
        }
        let cpu = self.cache.borrow().cpu_info.clone();
        if !cpu.is_empty() {
            self.send_sp("cpu_info", Value::Object(cpu));
        }
        self.send_sp("ambient", json!({"new": self.amb.ambient.get()}));
        if !self.power_id.is_empty() {
            let s = self.clone();
            spawn(async move {
                let mut a = Map::new();
                a.insert("device".into(), json!(s.power_id));
                if let Some(info) = Self::call_internal_api("machine.device_power.get_device", a).await {
                    let on = info.get(&s.power_id).and_then(|v| v.as_str()) == Some("on");
                    s.send_sp("power_controller", json!({"on": on}));
                }
            });
        }
        let fs = self.cache.borrow().filament_state.clone();
        if !fs.is_empty() {
            self.send_sp("filament_sensor", json!({"state": fs}));
        }
        self.send_sp("webcam_status", json!({"connected": self.webcam_connected.get()}));
        let s = self.clone();
        spawn(async move { s.send_machine_data().await });
        let s = self.clone();
        spawn(async move {
            let cfg = s.webcam_cam.borrow().as_ref().map(|c| c.as_dict()).unwrap_or(json!({}));
            s.send_sp(
                "webcam",
                json!({
                    "flipH": cfg.get("flip_horizontal").cloned().unwrap_or(json!(false)),
                    "flipV": cfg.get("flip_vertical").cloned().unwrap_or(json!(false)),
                    "rotate90": cfg.get("rotation").and_then(|v| v.as_i64()).unwrap_or(0) == 90,
                }),
            );
        });
    }

    fn send_sp(&self, evt_name: &str, data: Value) -> bool {
        let tx = self.ws_tx.borrow().clone();
        let tx = match tx {
            Some(t) if self.connected.get() && (self.is_set_up.get() || PRE_SETUP_EVENTS.contains(&evt_name)) => t,
            _ => return false,
        };
        let mut data = data;
        if (evt_name == "file_progress" || evt_name == "job_info") && data.is_object() {
            if let Some(j) = self.current_job_id.get() {
                data["job_id"] = json!(j);
            }
        }
        let packet = json!({"type": evt_name, "data": data});
        if tx.send(Message::Text(packet.to_string().into())).is_err() {
            return false;
        }
        if evt_name != "stream" {
            self.logger.info(&format!("sent: {}", py_repr(&packet)));
        } else {
            self.logger.info("sent: webcam stream");
        }
        true
    }

    // ***** Ambient detection *****

    fn update_ambient(&self, sensor: &Value, eventtime: f64) {
        let temp = match sensor.get("temperature").and_then(|v| v.as_f64()) {
            None => return,
            Some(t) => t,
        };
        if eventtime < self.amb.last_sample_time.get() + SAMPLE_CHECK_TIME {
            return;
        }
        self.amb.last_sample_time.set(eventtime);
        let new_amb = (temp + 0.5) as i64;
        if (new_amb - self.amb.ambient.get()).abs() < 2 {
            return;
        }
        self.amb.ambient.set(new_amb);
        self.on_ambient_changed(new_amb);
    }

    fn on_ambient_changed(&self, new: i64) {
        self.save_item("ambient_temp", json!(new));
        self.send_sp("ambient", json!({"new": new}));
    }

    fn amb_start(self: &Rc<Self>) {
        if self.amb.timer.is_running() {
            return;
        }
        let t0 = self.cache.borrow().temps.get("tool0").and_then(|v| v.get(0)).and_then(|v| v.as_i64());
        if let Some(cur) = t0 {
            if cur < self.amb.ambient.get() {
                self.amb.ambient.set(cur);
                self.on_ambient_changed(cur);
            }
        }
        let w = Rc::downgrade(self);
        self.amb.timer.start(
            0.0,
            Rc::new(move || match w.upgrade() {
                Some(s) => s.handle_detect_timer(now()),
                None => 3600.0,
            }),
        );
    }

    fn amb_stop(&self) {
        self.amb.timer.stop();
        self.amb.last_sample_time.set(0.0);
    }

    fn handle_detect_timer(&self, eventtime: f64) -> f64 {
        let a = &self.amb;
        let t0 = self.cache.borrow().temps.get("tool0").cloned();
        let (temp, target) = match t0.as_ref().and_then(|v| v.as_array()) {
            None => {
                a.initial_sample.set(-1000.0);
                return AMB_CHECK_INTERVAL;
            }
            Some(arr) => (arr.first().and_then(|v| v.as_f64()).unwrap_or(0.0), arr.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0)),
        };
        if target != 0.0 {
            a.initial_sample.set(-1000.0);
            a.last_sample_time.set(eventtime);
            a.update_interval.set(TARGET_CHECK_TIME);
            return AMB_CHECK_INTERVAL;
        }
        if eventtime - a.last_sample_time.get() < a.update_interval.get() {
            return AMB_CHECK_INTERVAL;
        }
        if a.initial_sample.get() == -1000.0 {
            a.initial_sample.set(temp);
            a.update_interval.set(SAMPLE_CHECK_TIME);
        } else {
            let diff = (temp - a.initial_sample.get()).abs();
            if diff <= 2.0 {
                let last = a.ambient.get();
                a.ambient.set(((temp + a.initial_sample.get()) / 2.0 + 0.5) as i64);
                a.initial_sample.set(-1000.0);
                a.last_sample_time.set(eventtime);
                a.update_interval.set(AMBIENT_CHECK_TIME);
                if last != a.ambient.get() {
                    log::debug!("SimplyPrint: New Ambient: {}", a.ambient.get());
                    self.on_ambient_changed(a.ambient.get());
                }
            } else {
                a.initial_sample.set(temp);
                a.update_interval.set(SAMPLE_CHECK_TIME);
            }
        }
        AMB_CHECK_INTERVAL
    }

    // ***** Webcam *****

    async fn initialize_url(&self) {
        let wcm: Option<Rc<crate::components::webcam::WebcamManager>> = server().lookup_component("webcam");
        let cams = match wcm {
            Some(w) => w.get_webcams(),
            None => return,
        };
        if cams.is_empty() {
            return;
        }
        let cam = match cams.get(&self.webcam_name).filter(|_| !self.webcam_name.is_empty()) {
            Some(c) => c.clone(),
            None => cams.values().next().unwrap().clone(),
        };
        match cam.get_snapshot_url(true).await {
            Err(e) => log::error!("Failed to retrieve webcam url\n{}", e.message),
            Ok(url) => {
                log::info!("SimplyPrint Webcam URL assigned: {}", url);
                *self.webcam_url.borrow_mut() = url;
                *self.webcam_cam.borrow_mut() = Some(cam);
            }
        }
    }

    fn http() -> Rc<HttpClient> {
        server().lookup_component("http_client").expect("http_client")
    }

    async fn test_connection(&self) {
        let url = self.webcam_url.borrow().clone();
        if !url.starts_with("http") {
            self.webcam_connected.set(false);
            return;
        }
        let resp = Self::http().get(&url, &[("Accept".into(), "image/jpeg".into())], false).await;
        self.webcam_connected.set(!resp.has_error());
    }

    async fn extract_image(&self) -> Result<String, ServerError> {
        use base64::Engine;
        let url = self.webcam_url.borrow().clone();
        let resp = Self::http().get(&url, &[("Accept".into(), "image/jpeg".into())], false).await;
        resp.raise_for_status(None)?;
        Ok(base64::engine::general_purpose::STANDARD.encode(&resp.content))
    }

    async fn post_image(&self, payload: Map<String, Value>) {
        let res: Result<(), ServerError> = async {
            if let Some(uid) = payload.get("id").filter(|v| !v.is_null()) {
                let url = payload.get("endpoint").map(crate::pyfmt::py_str).unwrap_or(SP_SNAPSHOT_URL.into());
                let img = self.extract_image().await?;
                let headers = vec![
                    ("User-Agent".to_string(), "Mozilla/5.0".to_string()),
                    ("Content-Type".to_string(), "application/x-www-form-urlencoded".to_string()),
                ];
                let esc = |s: &str| crate::utils::url_escape(s).replace("%20", "+");
                let body = format!("id={}&image={}", esc(&crate::pyfmt::py_str(uid)), esc(&img));
                let resp = Self::http().post(&url, Body::Text(body), &headers, RequestOpts::default()).await;
                resp.raise_for_status(None)?;
            } else if let Some(timer) = payload.get("timer").and_then(|v| v.as_f64()) {
                tokio::time::sleep(Duration::from_secs_f64((timer / 1000.0).max(0.0))).await;
                let img = self.extract_image().await?;
                self.send_sp("stream", json!({"base": img}));
            }
            Ok(())
        }
        .await;
        if let Err(e) = res {
            if server().is_verbose_enabled() {
                log::error!("SimplyPrint WebCam Stream Error\n{}", e.message);
            }
        }
    }

    // ***** Print handling *****

    async fn download_sp_file(self: Rc<Self>, url: &str, start: bool) {
        let srv = server();
        let fm: Rc<crate::components::file_manager::FileManager> = match srv.lookup_component("file_manager") {
            Some(f) => f,
            None => return,
        };
        let gc_path = PathBuf::from(fm.get_directory("gcodes"));
        if !gc_path.is_dir() {
            log::debug!("GCode Path Not Registered: {}", gc_path.display());
            self.send_sp("file_progress", json!({"state": "error", "message": "GCode Path not Registered"}));
            return;
        }
        self.on_download_progress(0);
        log::debug!("Downloading URL: {}", url);
        let w = Rc::downgrade(&self);
        let cb: crate::components::http_client::ProgressCallback = Rc::new(move |pct, _size, _recd| {
            if let Some(s) = w.upgrade() {
                s.on_download_progress(pct);
            }
        });
        let res = Self::http().download_file(url, "text/plain,applicaton/octet-stream", None, -1, Some(cb), 3600.0, 1, 1.0).await;
        self.download_progress.set(-1);
        let tmp_path = match res {
            Ok(p) => p,
            Err(e) => {
                log::error!("Failed to download file: {}\n{}", url, e.message);
                self.send_sp("file_progress", json!({"state": "error", "message": "Network Error"}));
                return;
            }
        };
        log::debug!("Simplyprint: Download Complete");
        let fname = tmp_path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut fpath = gc_path.join(&fname);
        if self.cache.borrow().job_info.get("filename").map(crate::pyfmt::py_str).unwrap_or_default() == fpath.display().to_string() {
            let p = std::path::Path::new(&fname);
            let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let suffix = p.extension().map(|s| format!(".{}", s.to_string_lossy())).unwrap_or_default();
            let mut count = 0;
            while fpath.exists() {
                fpath = gc_path.join(format!("{}_copy_{}.{}", stem, count, suffix));
                count += 1;
            }
        }
        let fpath_name = fpath.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut args = Map::new();
        args.insert("filename".into(), json!(fpath_name));
        args.insert("tmp_file_path".into(), json!(tmp_path.display().to_string()));
        let mut state = "pending";
        if self.cache.borrow().state == "operational" {
            args.insert("print".into(), json!(if start { "true" } else { "false" }));
        }
        let ret = match fm.finalize_upload(args, None).await {
            Ok(r) => r,
            Err(e) => {
                log::error!("GCode Finalization Failed\n{}", e.message);
                self.send_sp("file_progress", json!({"state": "error", "message": format!("GCode Finalization Failed: {}", e.message)}));
                return;
            }
        };
        *self.pending_file.borrow_mut() = fpath_name;
        if ret.get("print_started").and_then(|v| v.as_bool()).unwrap_or(false) {
            state = "started";
            *self.last_started.borrow_mut() = std::mem::take(&mut *self.pending_file.borrow_mut());
        } else if !start && self.check_can_print().await {
            state = "ready";
        }
        if state == "pending" {
            self.print_ready_event.clear();
            if tokio::time::timeout(Duration::from_secs(10), self.print_ready_event.wait()).await.is_err() {
                self.pending_file.borrow_mut().clear();
                self.send_sp("file_progress", json!({"state": "error", "message": "Pending print timed out"}));
                return;
            }
            if start {
                self.start_print().await;
                return;
            }
            state = "ready";
        }
        self.send_sp("file_progress", json!({"state": state}));
    }

    async fn start_print(&self) {
        let pending = std::mem::take(&mut *self.pending_file.borrow_mut());
        if pending.is_empty() {
            return;
        }
        let user = UserInfo::new("SimplyPrint", "");
        let mut data = json!({"state": "started"});
        match server().klippy_apis().start_print(&pending, false, Some(user)).await {
            Err(e) => {
                log::error!("Print Failed to start\n{}", e.message);
                data["state"] = json!("error");
                data["message"] = json!("Failed to start print");
            }
            Ok(_) => *self.last_started.borrow_mut() = pending,
        }
        self.send_sp("file_progress", data);
    }

    async fn check_can_print(&self) -> bool {
        if server().klippy().state() != KlippyState::READY {
            return false;
        }
        let mut q = Map::new();
        q.insert("print_stats".into(), Value::Null);
        match server().klippy_apis().query_objects(&q).await {
            Err(_) => false,
            Ok(r) => match r.get("print_stats").and_then(|p| p.get("state")).and_then(|v| v.as_str()) {
                None => false,
                Some(s) => s != "printing" && s != "paused",
            },
        }
    }

    fn on_download_progress(&self, percent: i64) {
        if percent == self.download_progress.get() {
            return;
        }
        self.download_progress.set(percent);
        self.send_sp("file_progress", json!({"state": "downloading", "percent": percent}));
    }
}

impl ApiTransport for SimplyPrint {
    fn send_status(&self, status: &Map<String, Value>, eventtime: f64) {
        {
            let mut ps = self.printer_status.borrow_mut();
            for (obj, vals) in status {
                let e = ps.entry(obj.clone()).or_insert_with(|| json!({}));
                if let (Some(e), Some(v)) = (e.as_object_mut(), vals.as_object()) {
                    e.extend(v.clone());
                }
            }
        }
        if let Some(a) = status.get(&self.amb.sensor_name) {
            self.update_ambient(a, eventtime);
        }
        self.update_temps(eventtime);
        if status.contains_key("bed_mesh") {
            self.send_mesh_data();
        }
        if let Some(e) = status.get("toolhead").and_then(|t| t.get("extruder")).and_then(|v| v.as_str()) {
            self.send_active_extruder(e);
        }
        if let Some(gm) = status.get("gcode_move") {
            self.layer_detect.borrow_mut().update(gm.get("gcode_position").unwrap_or(&Value::Null));
        }
        if let Some(eo) = status.get("exclude_object").and_then(|v| v.as_object()) {
            self.handle_exclude_object(eo);
        }
        if !self.filament_sensor.is_empty() {
            if let Some(fs) = status.get(&self.filament_sensor) {
                self.send_filament_state(fs);
            }
        }
    }

    fn transport_id(&self) -> u64 {
        self.uid
    }
}

#[async_trait(?Send)]
impl Component for SimplyPrint {
    fn has_init(&self) -> bool {
        true
    }

    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        self.initialize_url().await;
        self.test_connection().await;
        let h = spawn(self.clone().connect());
        *self.connection_task.borrow_mut() = Some(h);
        Ok(())
    }

    async fn close(self: Rc<Self>) {
        if let Some(t) = self.download_task.borrow_mut().take() {
            t.abort();
        }
        self.amb_stop();
        self.printer_info_timer.stop();
        self.ping_sp_timer.stop();
        self.send_sp("shutdown", Value::Null);
        self.is_closing.set(true);
        let tx = self.ws_tx.borrow().clone();
        if let Some(tx) = tx {
            let _ = tx.send(Message::Close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: 1001.into(),
                reason: "Client Shutdown".into(),
            })));
        }
        let t = self.connection_task.borrow_mut().take();
        if let Some(t) = t {
            let ab = t.abort_handle();
            if tokio::time::timeout(Duration::from_secs(2), t).await.is_err() {
                ab.abort();
            }
        }
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<SimplyPrint>, ConfigError> {
    SimplyPrint::new(config)
}

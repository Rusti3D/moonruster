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

mod ambient;
mod connection;
mod events;
mod helpers;
mod print;
mod webcam;

use helpers::*;

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
    webcam_cam: RefCell<Option<Rc<crate::components::webcam::WebCam>>>,
    webcam_connected: Cell<bool>,
    last_received_temps: RefCell<HashMap<String, f64>>,
    last_err_log_time: Cell<f64>,
    last_cpu_update_time: Cell<f64>,
    intervals: RefCell<HashMap<String, f64>>,
    printer_status: RefCell<Map<String, Value>>,
    heaters: RefCell<Vec<(String, String)>>,
    missed_job_events: RefCell<Vec<Map<String, Value>>>,
    announce_mutex: tokio::sync::Mutex<()>,
    connection_task: crate::server::TaskSlot,
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
    download_task: crate::server::TaskSlot,
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
        let test = crate::pyfmt::py_truthy(&config.get_value_or("use_test_endpoint", json!(false), false)?);
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
            connection_task: Default::default(),
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
            download_task: Default::default(),
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
        use crate::events::*;
        macro_rules! on {
            ($evt:ty, |$s:ident, $args:pat_param| $body:expr) => {{
                let w = Rc::downgrade(sp);
                srv.on::<$evt>(move |$args| {
                    if let Some($s) = w.upgrade() {
                        $body;
                    }
                });
            }};
        }
        on!(KlippyStarted, |s, (state, _)| s.on_klippy_startup(*state));
        {
            let w = Rc::downgrade(sp);
            srv.on_async::<crate::events::KlippyReady, _, _>(move |_| {
                let w = w.clone();
                async move {
                    if let Some(s) = w.upgrade() {
                        s.on_klippy_ready().await;
                    }
                }
            });
        }
        on!(KlippyShutdown, |s, _| {
            s.update_state("error");
            s.send_sp("printer_error", json!({"error": server().klippy().state_message()}));
        });
        on!(KlippyDisconnect, |s, _| s.on_klippy_disconnected());
        on!(JobStateChanged, |s, args| s.on_job_state_changed(args));
        on!(PauseRequested, |s, _| {
            s.print_request_event.set();
            if s.cache.borrow().state == "printing" {
                s.update_state("pausing");
            }
        });
        on!(ResumeRequested, |s, _| {
            s.print_request_event.set();
            if s.cache.borrow().state == "paused" {
                s.update_state("resuming");
            }
        });
        on!(CancelRequested, |s, _| {
            s.print_request_event.set();
            let st = s.cache.borrow().state.clone();
            if ["printing", "paused", "pausing"].contains(&st.as_str()) {
                s.update_state("cancelling");
            }
        });
        on!(ProcStatUpdate, |s, stats| s.on_proc_update(stats));
        on!(CpuThrottled, |s, state| s.cache.borrow_mut().throttled_state = state.clone());
        on!(ClientIdentified, |s, t| s.on_websocket_identified(t.transport_id()));
        on!(ClientRemoved, |s, t| s.on_websocket_removed(t.transport_id()));
        on!(GcodeResponse, |s, resp| {
            if s.gcode_terminal_enabled.get() {
                let r = resp.as_str().unwrap_or("");
                let resp: Vec<&str> = r.trim().split('\n').map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
                s.send_sp("term_update", json!({"response": resp}));
            }
        });
        on!(GcodeReceived, |s, script| {
            if s.gcode_terminal_enabled.get() {
                let r = script.as_str();
                let cmds: Vec<&str> = r.split_whitespace().collect();
                s.send_sp("term_update", json!({"command": cmds}));
            }
        });
        // power:power_changed has no Rust sender's type: its values
        let w = Rc::downgrade(sp);
        srv.on_values("power:power_changed", move |args| {
            let (Some(s), Some(d)) = (w.upgrade(), args.first()) else { return };
            if !s.power_id.is_empty() && d.get("device").and_then(|v| v.as_str()) == Some(s.power_id.as_str()) {
                s.send_sp("power_controller", json!({"on": d.get("status").and_then(|v| v.as_str()) == Some("on")}));
            }
        });
    }

    fn interval(&self, k: &str) -> f64 {
        self.intervals.borrow().get(k).copied().unwrap_or(0.0)
    }
}

impl ApiTransport for SimplyPrint {
    fn py_object(self: Rc<Self>) -> Option<(Rc<dyn std::any::Any>, &'static str, &'static str)> {
        Some((self, "moonraker.components.simplyprint", "SimplyPrint"))
    }
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
        self.connection_task.set(h);
        Ok(())
    }

    async fn close(self: Rc<Self>) {
        self.download_task.cancel();
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
        let t = self.connection_task.take();
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

// OctoPrint API compatibility (port of moonraker/components/octoprint_compat.py)

use crate::common::*;
use crate::confighelper::ConfigHelper;
use crate::server::{server, Component};
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::rc::Rc;

const OCTO_VERSION: &str = "1.5.0";

pub struct OctoPrintCompat {
    software_version: Value,
    enable_ufp: bool,
    webcam: Value,
    heaters: RefCell<IndexMap<String, Map<String, Value>>>,
    last_print_stats: RefCell<Map<String, Value>>,
}

impl Component for OctoPrintCompat {}

macro_rules! octo_endpoint {
    ($srv:expr, $o:expr, $path:expr, $rt:expr, $method:ident) => {{
        let oo = $o.clone();
        $srv.register_endpoint_ext(
            $path,
            $rt,
            callback(move |req: WebRequest| {
                let oo = oo.clone();
                async move { oo.$method(req).await }
            }),
            TransportType::HTTP,
            false,
            None,
            true,
        )?;
    }};
}

impl OctoPrintCompat {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, crate::confighelper::ConfigError> {
        let srv = server();
        let enable_ufp = config.getboolean_or("enable_ufp", true)?;
        let webcam = json!({
            "flipH": config.getboolean_or("flip_h", false)?,
            "flipV": config.getboolean_or("flip_v", false)?,
            "rotate90": config.getboolean_or("rotate_90", false)?,
            "streamUrl": config.get_or("stream_url", "/webcam/?action=stream")?,
            "webcamEnabled": config.getboolean_or("webcam_enabled", true)?,
        });
        let o = Rc::new(OctoPrintCompat {
            software_version: srv.get_app_arg("software_version").unwrap_or(Value::Null),
            enable_ufp,
            webcam,
            heaters: RefCell::new(IndexMap::new()),
            last_print_stats: RefCell::new(Map::new()),
        });
        let ow = Rc::downgrade(&o);
        srv.on_async::<crate::events::KlippyReady, _, _>(move |_| {
            let ow = ow.clone();
            async move {
                if let Some(o) = ow.upgrade() {
                    o.init().await;
                }
            }
        });
        let ow = Rc::downgrade(&o);
        srv.on::<crate::events::StatusUpdate>(move |status| {
            if let Some(o) = ow.upgrade() {
                o.handle_status_update(status);
            }
        });
        octo_endpoint!(srv, o, "/api/version", RequestType::GET, get_version);
        octo_endpoint!(srv, o, "/api/server", RequestType::GET, get_server);
        octo_endpoint!(srv, o, "/api/login", RequestType::POST, post_login_user);
        octo_endpoint!(srv, o, "/api/currentuser", RequestType::GET, post_login_user);
        octo_endpoint!(srv, o, "/api/settings", RequestType::GET, get_settings);
        octo_endpoint!(srv, o, "/api/job", RequestType::GET, get_job);
        octo_endpoint!(srv, o, "/api/printer", RequestType::GET, get_printer);
        octo_endpoint!(srv, o, "/api/printer/command", RequestType::POST, post_command);
        octo_endpoint!(srv, o, "/api/printerprofiles", RequestType::GET, get_printerprofiles);
        srv.app().register_upload_handler("/api/files/local", "api/files/moonraker");
        octo_endpoint!(srv, o, "/api/files/moonraker/(?P<relative_path>.+)", RequestType::POST, select_file);
        Ok(o)
    }

    async fn init(&self) {
        self.heaters.borrow_mut().clear();
        let apis = server().klippy_apis();
        let mut q = Map::new();
        q.insert("heaters".into(), Value::Null);
        let sensors: Vec<String> = match apis.query_objects(&q).await {
            Ok(r) => r
                .get("heaters")
                .and_then(|h| h.get("available_sensors"))
                .and_then(|a| a.as_array())
                .map(|a| a.iter().map(crate::pyfmt::py_str).collect())
                .unwrap_or_default(),
            Err(e) => {
                log::info!("Error Configuring heaters: {}", e);
                Vec::new()
            }
        };
        let mut sub: Map<String, Value> = sensors.iter().map(|s| (s.clone(), Value::Null)).collect();
        sub.insert("print_stats".into(), Value::Null);
        let result = match apis.subscribe_objects(&sub, None).await {
            Ok(r) => r,
            Err(e) => {
                log::error!("{}", e);
                return;
            }
        };
        *self.last_print_stats.borrow_mut() = result.get("print_stats").and_then(|v| v.as_object()).cloned().unwrap_or_default();
        if !sensors.is_empty() {
            let mut h = self.heaters.borrow_mut();
            for name in sensors {
                h.insert(name.clone(), result.get(&name).and_then(|v| v.as_object()).cloned().unwrap_or_default());
            }
        }
    }

    fn handle_status_update(&self, status: &Map<String, Value>) {
        if let Some(Value::Object(ps)) = status.get("print_stats") {
            let mut lps = self.last_print_stats.borrow_mut();
            for (k, v) in ps {
                lps.insert(k.clone(), v.clone());
            }
        }
        for (name, data) in self.heaters.borrow_mut().iter_mut() {
            if let Some(Value::Object(s)) = status.get(name) {
                for (k, v) in s {
                    data.insert(k.clone(), v.clone());
                }
            }
        }
    }

    pub fn printer_state(&self) -> String {
        let ks = server().klippy().state();
        if !ks.startup_complete() {
            return "Offline".into();
        } else if ks != KlippyState::READY {
            return "Error".into();
        }
        let st = self.last_print_stats.borrow().get("state").and_then(|v| v.as_str()).unwrap_or("standby").to_string();
        match st.as_str() {
            "standby" => "Operational",
            "printing" => "Printing",
            "paused" => "Paused",
            "complete" => "Operational",
            _ => "Error",
        }
        .into()
    }

    pub fn printer_temps(&self) -> Value {
        let mut temps = Map::new();
        for (heater, data) in self.heaters.borrow().iter() {
            let mut name = "bed".to_string();
            if let Some(rest) = heater.strip_prefix("extruder") {
                let tool_no = crate::pyfmt::py_int(rest).unwrap_or(0);
                name = format!("tool{}", tool_no);
            } else if heater != "heater_bed" {
                continue;
            }
            let actual = crate::pyfmt::round_value(data.get("temperature").unwrap_or(&json!(0.0)), 2);
            temps.insert(
                name,
                json!({"actual": actual, "offset": 0, "target": data.get("target").cloned().unwrap_or(json!(0.0))}),
            );
        }
        Value::Object(temps)
    }

    async fn get_version(self: Rc<Self>, _r: WebRequest) -> ApiResult {
        Ok(json!({
            "server": OCTO_VERSION,
            "api": "0.1",
            "text": format!("OctoPrint (Moonraker {})", crate::pyfmt::py_str(&self.software_version)),
        }))
    }

    async fn get_server(self: Rc<Self>, _r: WebRequest) -> ApiResult {
        let ready = server().klippy().state() == KlippyState::READY;
        Ok(json!({"server": OCTO_VERSION, "safemode": if ready { Value::Null } else { json!("settings") }}))
    }

    async fn post_login_user(self: Rc<Self>, _r: WebRequest) -> ApiResult {
        Ok(json!({
            "_is_external_client": false,
            "_login_mechanism": "apikey",
            "name": "_api",
            "active": true,
            "user": true,
            "admin": true,
            "apikey": null,
            "permissions": [],
            "groups": ["admins", "users"],
        }))
    }

    async fn get_settings(self: Rc<Self>, _r: WebRequest) -> ApiResult {
        let mut s = json!({
            "plugins": {},
            "feature": {"sdSupport": false, "temperatureGraph": false},
            "webcam": self.webcam,
        });
        if self.enable_ufp {
            s["plugins"] = json!({
                "UltimakerFormatPackage": {
                    "align_inline_thumbnail": false,
                    "inline_thumbnail": false,
                    "inline_thumbnail_align_value": "left",
                    "inline_thumbnail_scale_value": "50",
                    "installed": true,
                    "installed_version": "0.2.2",
                    "scale_inline_thumbnail": false,
                    "state_panel_thumbnail": true,
                }
            });
        }
        Ok(s)
    }

    async fn get_job(self: Rc<Self>, _r: WebRequest) -> ApiResult {
        Ok(json!({
            "job": {"file": {"name": null}, "estimatedPrintTime": null, "filament": {"length": null}, "user": null},
            "progress": {"completion": null, "filepos": null, "printTime": null, "printTimeLeft": null, "printTimeOrigin": null},
            "state": self.printer_state(),
        }))
    }

    async fn get_printer(self: Rc<Self>, _r: WebRequest) -> ApiResult {
        let state = self.printer_state();
        let s = state.as_str();
        Ok(json!({
            "temperature": self.printer_temps(),
            "state": {
                "text": state,
                "flags": {
                    "operational": !(s == "Error" || s == "Offline"),
                    "paused": s == "Paused",
                    "printing": s == "Printing",
                    "cancelling": s == "Cancelling",
                    "pausing": false,
                    "error": s == "Error",
                    "ready": s == "Operational",
                    "closedOrError": s == "Error" || s == "Offline",
                },
            },
        }))
    }

    async fn post_command(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let commands = req.get_opt("commands").and_then(|c| c.as_array().cloned()).unwrap_or_default();
        let apis = server().klippy_apis();
        for c in commands {
            let cmd = crate::pyfmt::py_str(&c);
            log::info!("Executing GCode: {}", cmd);
            if let Err(e) = apis.run_gcode(&cmd).await {
                log::error!("Error executing GCode {}\n{}", cmd, e);
            }
        }
        Ok(json!({}))
    }

    async fn get_printerprofiles(self: Rc<Self>, _r: WebRequest) -> ApiResult {
        let h = self.heaters.borrow();
        Ok(json!({
            "profiles": {
                "_default": {
                    "id": "_default",
                    "name": "Default",
                    "color": "default",
                    "model": "Default",
                    "default": true,
                    "current": true,
                    "heatedBed": h.contains_key("heater_bed"),
                    "heatedChamber": h.contains_key("chamber"),
                    "axes": {
                        "x": {"speed": 6000.0, "inverted": false},
                        "y": {"speed": 6000.0, "inverted": false},
                        "z": {"speed": 6000.0, "inverted": false},
                        "e": {"speed": 300.0, "inverted": false},
                    },
                }
            }
        }))
    }

    async fn select_file(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let command = req.get_str("command")?;
        let rel = req.get_str("relative_path")?;
        let trimmed = rel.trim_matches('/');
        let (root, filename) = trimmed
            .split_once('/')
            .ok_or_else(|| ServerError::exc("not enough values to unpack (expected 2, got 1)"))?;
        let srv = server();
        let fm = srv.get::<crate::components::file_manager::FileManager>().ok_or_else(|| ServerError::new("fm", 500))?;
        if command != "select" {
            return Err(ServerError::bad(format!("Unsupported Command: {}", command)));
        }
        if !req.get_boolean_or("print", false)? {
            return Ok(Value::Null);
        }
        if root != "gcodes" {
            return Err(ServerError::new("File must be located in the 'gcodes' root", 400));
        }
        if !fm.check_file_exists(root, filename, None) {
            return Err(ServerError::bad("File does not exist"));
        }
        let apis = srv.klippy_apis();
        let mut q = Map::new();
        q.insert("print_stats".into(), Value::Null);
        let pstate = match apis.query_objects(&q).await {
            Ok(r) => r.get("print_stats").and_then(|p| p.get("state")).and_then(|s| s.as_str()).unwrap_or("").to_string(),
            Err(_) => "not_avail".into(),
        };
        let user = req.get_current_user().cloned();
        let mut started = false;
        if !["printing", "paused", "not_avail"].contains(&pstate.as_str()) {
            if apis.start_print(filename, false, user.clone()).await.is_ok() {
                log::debug!("Job '{}' started via OctoPrint API", filename);
                started = true;
            }
        }
        if !started {
            if fm.upload_queue_enabled() {
                if let Some(jq) = srv.get::<crate::components::job_queue::JobQueue>() {
                    jq.queue_job(vec![filename.to_string()], false, false, user).await?;
                    log::debug!("Job '{}' queued via OctoPrint API", filename);
                }
            } else {
                return Err(ServerError::new("Conflict", 409));
            }
        }
        Ok(Value::Null)
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<OctoPrintCompat>, crate::confighelper::ConfigError> {
    OctoPrintCompat::new(config)
}

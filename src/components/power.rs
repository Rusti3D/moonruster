// Power Switch Control (port of moonraker/components/power.py)

use crate::common::*;
use crate::components::gpio::{getgpioout, GpioOutputPin};
use crate::components::http_client::{Body, HttpClient, HttpResponse, RequestOpts};
use crate::components::machine::Machine;
use crate::components::mqtt::{MqttCallback, MqttClient};
use crate::components::template::{AsyncFn, Caller, JinjaTemplate, ObjBuilder};
use crate::confighelper::{Bounds, ConfigError, ConfigHelper};
use crate::server::{server, spawn, Component};
use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn loop_time() -> f64 {
    crate::utils::monotonic()
}

fn quote(s: &str) -> String {
    crate::utils::url_quote(s)
}

// ***** Generic HTTP template objects *****

#[derive(Debug, Default)]
struct ReqState {
    method: String,
    url: String,
    body: Value,
    headers: Vec<(String, String)>,
    last_response: Option<Value>,
}

/// `http_request` object available to GenericHTTP templates
#[derive(Debug)]
struct HttpRequestObj {
    default_url: String,
    st: Mutex<ReqState>,
    caller: Mutex<Option<Caller>>,
}

impl HttpRequestObj {
    fn new(url: &str) -> Arc<Self> {
        Arc::new(HttpRequestObj {
            default_url: url.to_string(),
            st: Mutex::new(ReqState { method: "GET".into(), url: url.into(), ..Default::default() }),
            caller: Mutex::new(None),
        })
    }
    fn state_json(&self) -> Value {
        let st = self.st.lock().unwrap();
        let headers: Map<String, Value> = st.headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        json!({"method": st.method, "url": st.url, "body": st.body, "headers": headers})
    }
}

impl std::fmt::Debug for Caller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Caller")
    }
}

impl minijinja::value::Object for HttpRequestObj {
    fn call_method(
        self: &Arc<Self>,
        _state: &minijinja::State,
        method: &str,
        args: &[minijinja::Value],
    ) -> Result<minijinja::Value, minijinja::Error> {
        let to_json = |v: &minijinja::Value| serde_json::to_value(v).unwrap_or(Value::Null);
        let s = |i: usize| args.get(i).map(|a| to_json(a)).map(|v| crate::pyfmt::py_str(&v)).unwrap_or_default();
        let none = minijinja::Value::from(());
        match method {
            "send" => {
                let mut req = self.state_json();
                if let Some(kw) = args.last().and_then(|a| minijinja::value::Kwargs::try_from(a.clone()).ok()) {
                    req["kwargs"] = Value::Object(crate::components::template::kwargs_to_map(&kw));
                }
                let caller = self.caller.lock().unwrap().clone();
                let caller = caller.ok_or_else(|| minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, "no event loop"))?;
                let resp = caller.call("http_send", vec![req], Map::new())?;
                let rj = to_json(&resp);
                self.st.lock().unwrap().last_response = Some(rj.clone());
                Ok(minijinja::Value::from_object(ResponseObj(rj)))
            }
            "set_method" => {
                self.st.lock().unwrap().method = s(0);
                Ok(none)
            }
            "set_url" => {
                self.st.lock().unwrap().url = s(0);
                Ok(none)
            }
            "set_body" => {
                self.st.lock().unwrap().body = args.first().map(to_json).unwrap_or(Value::Null);
                Ok(none)
            }
            "add_header" => {
                let (k, v) = (s(0), s(1));
                let mut st = self.st.lock().unwrap();
                st.headers.retain(|(hk, _)| *hk != k);
                st.headers.push((k, v));
                Ok(none)
            }
            "set_headers" => {
                let h = args.first().map(to_json).unwrap_or(Value::Null);
                self.st.lock().unwrap().headers = h
                    .as_object()
                    .map(|m| m.iter().map(|(k, v)| (k.clone(), crate::pyfmt::py_str(v))).collect())
                    .unwrap_or_default();
                Ok(none)
            }
            "reset" => {
                let mut st = self.st.lock().unwrap();
                st.method = "GET".into();
                st.url = self.default_url.clone();
                st.body = Value::Null;
                st.headers.clear();
                Ok(none)
            }
            "last_response" => Ok(match self.st.lock().unwrap().last_response.clone() {
                Some(r) => minijinja::Value::from_object(ResponseObj(r)),
                None => none,
            }),
            _ => Err(minijinja::Error::new(minijinja::ErrorKind::UnknownMethod, format!("no method {}", method))),
        }
    }
}

/// HttpResponse as seen by templates
#[derive(Debug)]
struct ResponseObj(Value);

impl minijinja::value::Object for ResponseObj {
    fn get_value(self: &Arc<Self>, key: &minijinja::Value) -> Option<minijinja::Value> {
        let k = key.as_str()?;
        match k {
            "status_code" | "url" | "final_url" | "headers" | "content" => self.0.get(k).map(minijinja::Value::from_serialize),
            "text" => self.0.get("content").map(minijinja::Value::from_serialize),
            "encoding" => Some(minijinja::Value::from("utf-8")),
            _ => None,
        }
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &minijinja::State,
        method: &str,
        args: &[minijinja::Value],
    ) -> Result<minijinja::Value, minijinja::Error> {
        let err = |m: String| minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, m);
        match method {
            "json" => {
                let text = self.0.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let v: Value = serde_json::from_str(text).map_err(|e| err(e.to_string()))?;
                Ok(minijinja::Value::from_serialize(&v))
            }
            "has_error" => Ok(minijinja::Value::from(!self.0["error"].is_null())),
            "is_cachable" => Ok(minijinja::Value::from(false)),
            "raise_for_status" => {
                if self.0["error"].is_null() {
                    return Ok(minijinja::Value::from(()));
                }
                let msg = args.first().and_then(|a| a.as_str()).map(String::from).unwrap_or_else(|| crate::pyfmt::py_str(&self.0["error"]));
                Err(err(msg))
            }
            _ => Err(minijinja::Error::new(minijinja::ErrorKind::UnknownMethod, format!("no method {}", method))),
        }
    }
}

fn response_json(r: &HttpResponse) -> Value {
    let headers: Map<String, Value> = r.headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    json!({
        "url": r.url,
        "final_url": r.final_url,
        "status_code": r.status_code,
        "content": r.text(),
        "headers": headers,
        "error": if r.has_error() { json!(r.error_string()) } else { Value::Null },
    })
}

// ***** Device definitions *****

enum HttpKind {
    Tasmota { output_id: i64, timer: String },
    Shelly { output_id: i64, timer: String },
    SmartThings { device: String, token: String },
    HomeSeer { device: i64 },
    HomeAssistant { device: String, token: String, domain: String, status_delay: f64 },
    Loxone { output_id: String },
    Hue { device_id: String, device_type: String, state_key: String, on_state: String },
    Generic { urls: IndexMap<String, String>, request_template: Option<JinjaTemplate>, response_template: JinjaTemplate },
}

struct HttpDev {
    client: Rc<HttpClient>,
    user: String,
    password: String,
    has_basic_auth: bool,
    addr: String,
    port: i64,
    protocol: String,
    kind: HttpKind,
}

struct MqttDev {
    mqtt: Rc<MqttClient>,
    cmd_topic: String,
    cmd_payload: JinjaTemplate,
    retain_cmd_state: bool,
    query_topic: Option<String>,
    query_payload: Option<JinjaTemplate>,
    must_query: bool,
    last_request: RefCell<String>,
    response_count: Cell<i64>,
    state_timeout: f64,
    state_response: JinjaTemplate,
    qos: Option<u8>,
    query_response: RefCell<Option<tokio::sync::oneshot::Sender<Result<String, ServerError>>>>,
}

enum Kind {
    Gpio { timer: Option<f64>, gpio_out: Rc<GpioOutputPin>, rf: Option<(String, String)> },
    Klipper {
        is_shutdown: Cell<bool>,
        update_tx: RefCell<Option<tokio::sync::oneshot::Sender<String>>>,
        timer: Option<f64>,
        object_name: String,
        gc_cmd: String,
    },
    TpLink { timer: String, addr: String, output_id: Option<i64>, port: i64 },
    Http(HttpDev),
    Mqtt(MqttDev),
    UHub { location: String, port: Option<i64> },
}

pub struct PowerDevice {
    name: String,
    type_: String,
    state: RefCell<String>,
    request_lock: tokio::sync::Mutex<()>,
    init_task: RefCell<Option<tokio::task::JoinHandle<()>>>,
    locked_while_printing: bool,
    off_when_shutdown: bool,
    off_when_shutdown_delay: f64,
    shutdown_timer: RefCell<Option<tokio::task::JoinHandle<()>>>,
    restart_delay: f64,
    klipper_restart: bool,
    bound_services: RefCell<Vec<String>>,
    need_scheduled_restart: Cell<bool>,
    on_when_queued: bool,
    initial_state: Cell<Option<bool>>,
    restrict_actions: bool,
    poll_task: RefCell<Option<tokio::task::JoinHandle<()>>>,
    poll_interval: Cell<Option<f64>>,
    last_update_time: Cell<f64>,
    timer_handle: RefCell<Option<tokio::task::JoinHandle<()>>>,
    kind: Kind,
}

fn err(msg: impl Into<String>) -> ServerError {
    ServerError::bad(msg)
}

impl PowerDevice {
    fn new(config: &ConfigHelper) -> Result<Rc<Self>, ConfigError> {
        let srv = server();
        let name_parts: Vec<&str> = {
            let n = config.get_name();
            match n.split_once(char::is_whitespace) {
                Some((a, b)) if !b.trim().is_empty() => vec![a, b.trim_start()],
                _ => vec![n],
            }
        };
        if name_parts.len() != 2 {
            return Err(ConfigHelper::error(format!("Invalid Section Name: {}", config.get_name())));
        }
        let name = name_parts[1].to_string();
        let type_ = config.get("type")?;
        let can_poll = !matches!(type_.as_str(), "gpio" | "rf" | "klipper_device" | "mqtt");
        let locked_while_printing = config.getboolean_or("locked_while_printing", false)?;
        let off_when_shutdown = config.getboolean_or("off_when_shutdown", false)?;
        let mut off_when_shutdown_delay = 0.0;
        if off_when_shutdown {
            off_when_shutdown_delay = config.getfloat_b("off_when_shutdown_delay", Some(0.0), Bounds::min(0.0))?;
        }
        let mut restart_delay = 1.0;
        let klipper_restart = config.getboolean_or("restart_klipper_when_powered", false)?;
        if klipper_restart {
            restart_delay = config.getfloat_b("restart_delay", Some(1.0), Bounds::above(0.000001))?;
        }
        let mut bound: Vec<String> = config.getlist_opt("bound_services", Some(vec![]), Some("\n"))?.unwrap_or_default();
        if config.has_option("bound_service") {
            bound.push(config.get("bound_service")?);
        }
        let mut bound_services: Vec<String> = Vec::new();
        for svc in bound {
            let svc = match svc.strip_suffix(".service") {
                Some(s) => s.to_string(),
                None => svc,
            };
            if !bound_services.contains(&svc) {
                bound_services.push(svc);
            }
        }
        let mut on_when_queued = config.getboolean_or("on_when_job_queued", false)?;
        if config.has_option("on_when_upload_queued") {
            on_when_queued = config.getboolean_dep("on_when_upload_queued", false, true)?;
        }
        let initial_state = config.getboolean_opt("initial_state")?;
        let restrict_actions = config.getboolean_or("restrict_action_processing", true)?;
        let poll_interval = if can_poll { config.getfloat_opt("poll_interval", Bounds::min(1.0))? } else { None };
        let kind = Self::build_kind(config, &type_, &name, initial_state, off_when_shutdown, klipper_restart)?;
        let dev = Rc::new(PowerDevice {
            name,
            type_,
            state: RefCell::new("init".into()),
            request_lock: tokio::sync::Mutex::new(()),
            init_task: RefCell::new(None),
            locked_while_printing,
            off_when_shutdown,
            off_when_shutdown_delay,
            shutdown_timer: RefCell::new(None),
            restart_delay,
            klipper_restart,
            bound_services: RefCell::new(bound_services),
            need_scheduled_restart: Cell::new(false),
            on_when_queued,
            initial_state: Cell::new(initial_state),
            restrict_actions,
            poll_task: RefCell::new(None),
            poll_interval: Cell::new(poll_interval),
            last_update_time: Cell::new(0.0),
            timer_handle: RefCell::new(None),
            kind,
        });
        if klipper_restart {
            let w = Rc::downgrade(&dev);
            srv.on_event("server:klippy_started", move |args| {
                if let Some(d) = w.upgrade() {
                    let st = args.first().and_then(|v| v.as_str()).and_then(|s| KlippyState::from_string(s).ok());
                    d.schedule_firmware_restart(st.unwrap_or(KlippyState::STARTUP));
                }
            });
        }
        match &dev.kind {
            Kind::Klipper { .. } => {
                let w = Rc::downgrade(&dev);
                srv.on_event_async("server:klippy_ready", move |_| {
                    let w = w.clone();
                    async move {
                        if let Some(d) = w.upgrade() {
                            d.klipper_handle_ready().await;
                        }
                    }
                });
                let w = Rc::downgrade(&dev);
                srv.on_event("server:klippy_disconnect", move |_| {
                    if let Some(d) = w.upgrade() {
                        if let Kind::Klipper { is_shutdown, .. } = &d.kind {
                            is_shutdown.set(false);
                        }
                        d.klipper_set_state("init");
                        d.reset_timer();
                    }
                });
            }
            Kind::Mqtt(m) => {
                let w = Rc::downgrade(&dev);
                let cb: MqttCallback = Rc::new(move |payload| {
                    if let Some(d) = w.upgrade() {
                        d.mqtt_on_state_update(payload);
                    }
                });
                let state_topic = config.get("state_topic")?;
                m.mqtt.subscribe_topic(&state_topic, cb, m.qos).map_err(|e| ConfigHelper::error(e.message))?;
                let w = Rc::downgrade(&dev);
                srv.on_event_async("mqtt:connected", move |_| {
                    let w = w.clone();
                    async move {
                        if let Some(d) = w.upgrade() {
                            d.mqtt_on_connected().await;
                        }
                    }
                });
                let w = Rc::downgrade(&dev);
                srv.on_event_async("mqtt:disconnected", move |_| {
                    let w = w.clone();
                    async move {
                        if let Some(d) = w.upgrade() {
                            d.mqtt_on_disconnected().await;
                        }
                    }
                });
            }
            _ => {}
        }
        Ok(dev)
    }

    fn http_base(
        config: &ConfigHelper,
        default_port: i64,
        default_user: &str,
        default_password: &str,
        default_protocol: &str,
        is_generic: bool,
    ) -> Result<HttpDev, ConfigError> {
        let client: Rc<HttpClient> =
            server().lookup_component("http_client").ok_or_else(|| ConfigHelper::error("Component (http_client) not found"))?;
        let render = |t: JinjaTemplate| t.render(&json!({})).map_err(|e| ConfigHelper::error(e.message));
        let user = render(config.load_template("user", Some(default_user), false)?)?;
        let password = render(config.load_template("password", Some(default_password), false)?)?;
        let mut d = HttpDev {
            client,
            user,
            password,
            has_basic_auth: false,
            addr: String::new(),
            port: default_port,
            protocol: default_protocol.into(),
            kind: HttpKind::Loxone { output_id: String::new() },
        };
        if is_generic {
            return Ok(d);
        }
        let addr = config.get("address")?;
        d.addr = addr.trim_matches('/').split('/').map(quote).collect::<Vec<_>>().join("/");
        d.port = config.getint_or("port", default_port)?;
        d.protocol = config.get_or("protocol", default_protocol)?;
        if d.port == -1 {
            d.port = if d.protocol.to_lowercase() == "https" { 443 } else { 80 };
        }
        Ok(d)
    }

    fn build_kind(
        config: &ConfigHelper,
        type_: &str,
        name: &str,
        initial_state: Option<bool>,
        off_when_shutdown: bool,
        klipper_restart: bool,
    ) -> Result<Kind, ConfigError> {
        let basic_auth = |d: &mut HttpDev| {
            if !d.user.is_empty() && !d.password.is_empty() {
                d.has_basic_auth = true;
            }
        };
        Ok(match type_ {
            "gpio" | "rf" => {
                let timer = config.getfloat_opt("timer", Bounds::none())?;
                if let Some(t) = timer {
                    if t < 0.000001 {
                        return Err(ConfigHelper::error(format!(
                            "Option 'timer' in section [{}] must be above 0.0",
                            config.get_name()
                        )));
                    }
                }
                let initial_val = if type_ == "rf" { 0 } else { initial_state.unwrap_or(false) as i32 };
                let gpio_out = getgpioout(config, "pin", initial_val)?;
                let rf = if type_ == "rf" {
                    let zfill = |s: String| if s.len() < 24 { format!("{}{}", "0".repeat(24 - s.len()), s) } else { s };
                    Some((zfill(config.get("on_code")?), zfill(config.get("off_code")?)))
                } else {
                    None
                };
                Kind::Gpio { timer, gpio_out, rf }
            }
            "klipper_device" => {
                if off_when_shutdown {
                    return Err(ConfigHelper::error(format!(
                        "Option 'off_when_shutdown' in section [{}] is unsupported for 'klipper_device'",
                        config.get_name()
                    )));
                }
                if klipper_restart {
                    return Err(ConfigHelper::error(format!(
                        "Option 'restart_klipper_when_powered' in section [{}] is unsupported for 'klipper_device'",
                        config.get_name()
                    )));
                }
                let timer = config.getfloat_opt("timer", Bounds::above(0.000001))?;
                let object_name = config.get("object_name")?;
                let parts: Vec<&str> = object_name.split_whitespace().collect();
                let last = parts.last().copied().unwrap_or("");
                let gc_cmd = match parts.first().copied() {
                    Some("gcode_macro") => last.to_string(),
                    Some("output_pin") => format!("SET_PIN PIN={}", last),
                    _ => {
                        return Err(ConfigHelper::error(format!(
                            "Klipper object must be either 'output_pin' or 'gcode_macro' for option 'object_name' in section [{}]",
                            config.get_name()
                        )))
                    }
                };
                Kind::Klipper { is_shutdown: Cell::new(false), update_tx: RefCell::new(None), timer, object_name, gc_cmd }
            }
            "tplink_smartplug" => {
                let timer = config.get_or("timer", "")?;
                let a = config.get("address")?;
                let parts: Vec<&str> = a.split('/').collect();
                let output_id = if parts.len() > 1 {
                    server().add_warning(
                        &format!(
                            "Power Device {}: Including the output id in the address is deprecated, use the 'output_id' option",
                            name
                        ),
                        None,
                        true,
                    );
                    Some(parts[1].trim().parse::<i64>().map_err(|_| {
                        ConfigHelper::error(format!("invalid literal for int() with base 10: '{}'", parts[1]))
                    })?)
                } else {
                    config.getint_opt("output_id", Bounds::none())?
                };
                let port = config.getint_or("port", 9999)?;
                Kind::TpLink { timer, addr: parts[0].to_string(), output_id, port }
            }
            "tasmota" => {
                let mut d = Self::http_base(config, -1, "admin", "", "http", false)?;
                d.kind = HttpKind::Tasmota { output_id: config.getint_or("output_id", 1)?, timer: config.get_or("timer", "")? };
                Kind::Http(d)
            }
            "shelly" => {
                let mut d = Self::http_base(config, -1, "admin", "", "http", false)?;
                d.kind = HttpKind::Shelly { output_id: config.getint_or("output_id", 0)?, timer: config.get_or("timer", "")? };
                basic_auth(&mut d);
                Kind::Http(d)
            }
            "smartthings" => {
                let mut d = Self::http_base(config, 443, "", "", "https", false)?;
                let device = config.get_or("device", "")?;
                let token = config.gettemplate("token", false)?.render(&json!({})).map_err(|e| ConfigHelper::error(e.message))?;
                d.kind = HttpKind::SmartThings { device, token };
                Kind::Http(d)
            }
            "homeseer" => {
                let mut d = Self::http_base(config, -1, "admin", "", "http", false)?;
                d.kind = HttpKind::HomeSeer { device: config.getint("device")? };
                basic_auth(&mut d);
                Kind::Http(d)
            }
            "homeassistant" => {
                let mut d = Self::http_base(config, 8123, "", "", "http", false)?;
                let device = config.get("device")?;
                let token = config.gettemplate("token", false)?.render(&json!({})).map_err(|e| ConfigHelper::error(e.message))?;
                let domain = config.get_or("domain", "switch")?;
                let status_delay = config.getfloat_or("status_delay", 1.0)?;
                d.kind = HttpKind::HomeAssistant { device, token, domain, status_delay };
                Kind::Http(d)
            }
            "loxonev1" => {
                let mut d = Self::http_base(config, -1, "admin", "admin", "http", false)?;
                d.kind = HttpKind::Loxone { output_id: config.get_or("output_id", "")? };
                basic_auth(&mut d);
                Kind::Http(d)
            }
            "hue" => {
                let mut d = Self::http_base(config, 80, "", "", "http", false)?;
                let device_id = config.get("device_id")?;
                let device_type = config.get_or("device_type", "light")?;
                let (state_key, on_state) =
                    if device_type == "group" { ("action", "all_on") } else { ("state", "on") };
                d.kind = HttpKind::Hue { device_id, device_type, state_key: state_key.into(), on_state: on_state.into() };
                Kind::Http(d)
            }
            "http" => {
                let mut d = Self::http_base(config, -1, "", "", "http", true)?;
                let render = |o: &str| -> Result<String, ConfigError> {
                    config.gettemplate(o, false)?.render(&json!({})).map_err(|e| ConfigHelper::error(e.message))
                };
                let mut urls = IndexMap::new();
                urls.insert("on".to_string(), render("on_url")?);
                urls.insert("off".to_string(), render("off_url")?);
                urls.insert("status".to_string(), render("status_url")?);
                let request_template = config.gettemplate_opt("request_template", Some(None), true)?;
                let response_template = config.gettemplate("response_template", true)?;
                d.kind = HttpKind::Generic { urls, request_template, response_template };
                basic_auth(&mut d);
                Kind::Http(d)
            }
            "mqtt" => {
                let srv = server();
                srv.load_component(config, "mqtt")?;
                let mqtt: Rc<MqttClient> = srv.lookup_component("mqtt").ok_or_else(|| ConfigHelper::error("mqtt not loaded"))?;
                let cmd_topic = config.get("command_topic")?;
                let cmd_payload = config.gettemplate("command_payload", false)?;
                let retain_cmd_state = config.getboolean_or("retain_command_state", false)?;
                let query_topic = config.get_opt("query_topic")?;
                let query_payload = config.gettemplate_opt("query_payload", Some(None), false)?;
                let mut must_query = config.getboolean_or("query_after_command", false)?;
                if query_topic.is_none() {
                    must_query = false;
                }
                let _state_topic = config.get("state_topic")?;
                let state_timeout = config.getfloat_or("state_timeout", 2.0)?;
                let state_response = config.load_template("state_response_template", Some("{payload}"), false)?;
                let qos = config.getint_opt("qos", Bounds::range(0.0, 2.0))?.map(|q| q as u8);
                Kind::Mqtt(MqttDev {
                    mqtt,
                    cmd_topic,
                    cmd_payload,
                    retain_cmd_state,
                    query_topic,
                    query_payload,
                    must_query,
                    last_request: RefCell::new(String::new()),
                    response_count: Cell::new(0),
                    state_timeout,
                    state_response,
                    qos,
                    query_response: RefCell::new(None),
                })
            }
            "uhubctl" => {
                server().load_component(config, "shell_command")?;
                let location = config.get("location")?;
                let port = config.getint_opt("port", Bounds::none())?;
                if crate::components::machine::which("uhubctl").is_none() {
                    return Err(ConfigHelper::error(format!(
                        "[{}]: failed to locate 'uhubctl' binary.  Make sure uhubctl is correctly installed on the host machine.",
                        config.get_name()
                    )));
                }
                Kind::UHub { location, port }
            }
            _ => unreachable!(),
        })
    }

    // ***** Common device behaviour *****

    fn state(&self) -> String {
        self.state.borrow().clone()
    }

    fn set_state_val(&self, s: &str) {
        *self.state.borrow_mut() = s.to_string();
    }

    fn get_device_info(&self) -> Value {
        let mut v = json!({
            "device": self.name,
            "status": self.state(),
            "locked_while_printing": self.locked_while_printing,
            "type": self.type_,
        });
        if let Kind::Klipper { is_shutdown, .. } = &self.kind {
            v["is_shutdown"] = json!(is_shutdown.get());
        }
        v
    }

    fn notify_power_changed(&self) {
        let _ = server().send_event("power:power_changed", vec![self.get_device_info()]);
    }

    fn schedule_firmware_restart(&self, state: KlippyState) {
        if !self.need_scheduled_restart.get() {
            return;
        }
        self.need_scheduled_restart.set(false);
        if state == KlippyState::READY {
            log::info!("Power Device {}: Klipper reports 'ready', aborting FIRMWARE_RESTART", self.name);
            return;
        }
        log::info!("Power Device {}: Sending FIRMWARE_RESTART command to Klippy", self.name);
        let delay = self.restart_delay;
        spawn(async move {
            tokio::time::sleep(Duration::from_secs_f64(delay)).await;
            let _ = server().klippy_apis().do_restart("FIRMWARE_RESTART", true).await;
        });
    }

    async fn process_power_changed(&self) {
        self.notify_power_changed();
        if !self.bound_services.borrow().is_empty() {
            self.process_bound_services().await;
        }
        if self.state() == "on" && self.klipper_restart {
            self.need_scheduled_restart.set(true);
            let kstate = server().klippy().state();
            if !kstate.startup_complete() {
                return;
            }
            self.schedule_firmware_restart(kstate);
        }
    }

    async fn process_bound_services(&self) {
        let state = self.state();
        if self.bound_services.borrow().is_empty() || (state != "on" && state != "off") {
            return;
        }
        let machine: Rc<Machine> = match server().lookup_component("machine") {
            Some(m) => m,
            None => return,
        };
        let action = if state == "on" { "start" } else { "stop" };
        let services = self.bound_services.borrow().clone();
        for svc in services {
            log::info!("Power Device {}: Performing {} action on bound service {}", self.name, action, svc);
            if let Err(e) = machine.do_service_action(action, &svc).await {
                log::error!(
                    "Power Device {}: Error processing {} action for bound service {}\n{}",
                    self.name,
                    action,
                    svc,
                    e.message
                );
            }
        }
    }

    fn process_klippy_shutdown(self: &Rc<Self>) {
        if let Kind::Klipper { is_shutdown, .. } = &self.kind {
            is_shutdown.set(true);
            self.klipper_set_state("error");
            self.reset_timer();
            return;
        }
        if !self.off_when_shutdown {
            return;
        }
        if self.off_when_shutdown_delay == 0.0 {
            self.power_off_on_shutdown();
        } else {
            if let Some(h) = self.shutdown_timer.borrow_mut().take() {
                h.abort();
            }
            let w = Rc::downgrade(self);
            let delay = self.off_when_shutdown_delay;
            let h = spawn(async move {
                tokio::time::sleep(Duration::from_secs_f64(delay)).await;
                if let Some(d) = w.upgrade() {
                    d.power_off_on_shutdown();
                }
            });
            *self.shutdown_timer.borrow_mut() = Some(h);
        }
    }

    fn power_off_on_shutdown(&self) {
        if server().klippy().state() != KlippyState::SHUTDOWN {
            return;
        }
        log::info!("Powering off device '{}' due to klippy shutdown", self.name);
        if let Some(p) = server().lookup_component::<PrinterPower>("power") {
            p.set_device_power(&self.name, "off", false);
        }
    }

    fn setup_bound_services(&self) -> Result<(), ServerError> {
        if self.bound_services.borrow().is_empty() {
            return Ok(());
        }
        let machine: Rc<Machine> = server().lookup_component("machine").ok_or_else(|| err("machine"))?;
        let sys_info = machine.get_system_info();
        let avail: Vec<String> = sys_info
            .get("available_services")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().map(crate::pyfmt::py_str).collect())
            .unwrap_or_default();
        for svc in self.bound_services.borrow().iter() {
            if machine.unit_name() == *svc {
                return Err(err(format!("Power Device {}: Cannot bind to Moonraker service {}.", self.name, svc)));
            }
            if !avail.contains(svc) {
                return Err(err(format!("Bound Service {} is not available", svc)));
            }
        }
        log::info!("Power Device '{}' bound to services: {}", self.name, self.bound_services.borrow().join(", "));
        Ok(())
    }

    fn initialize(self: &Rc<Self>) -> Result<bool, ServerError> {
        self.setup_bound_services()?;
        let has_init = !matches!(self.kind, Kind::Klipper { .. } | Kind::Mqtt(_));
        if has_init {
            let d = self.clone();
            let h = spawn(async move { d.init_state().await });
            *self.init_task.borrow_mut() = Some(h);
        }
        Ok(self.state() != "error")
    }

    async fn set_initial_state(&self) {
        let state = self.state();
        if let Some(init) = self.initial_state.get() {
            if state == "on" || state == "off" {
                let new_state = if init { "on" } else { "off" };
                let res: Result<(), ServerError> = async {
                    if new_state != state {
                        log::info!("Power Device {}: setting initial state to {}", self.name, new_state);
                        self.set_power(new_state).await?;
                    }
                    if !self.bound_services.borrow().is_empty() {
                        self.process_bound_services().await;
                    }
                    Ok(())
                }
                .await;
                if res.is_err() {
                    log::info!("Power Device {}: Error setting initial state", self.name);
                }
            }
        }
    }

    async fn process_request(self: &Rc<Self>, req: &str, force: bool) -> Result<String, ServerError> {
        if self.state() == "init" && self.request_lock.try_lock().is_err() {
            return Ok(self.state());
        }
        let _g = self.request_lock.lock().await;
        let res = self.process_request_locked(req, force).await;
        self.last_update_time.set(loop_time());
        res
    }

    async fn process_request_locked(self: &Rc<Self>, req: &str, force: bool) -> Result<String, ServerError> {
        let base_state = self.state();
        self.refresh_status().await?;
        let mut cur_state = self.state();
        let mut req = req.to_string();
        if req == "toggle" {
            req = if cur_state == "off" { "on".into() } else { "off".into() };
        }
        if req == "on" || req == "off" {
            if req == cur_state {
                if base_state != cur_state {
                    if self.restrict_actions {
                        self.notify_power_changed();
                    } else {
                        self.process_power_changed().await;
                    }
                }
                return Ok(cur_state);
            }
            if !force && self.locked_while_printing && server().klippy().is_printing() {
                return Err(err(format!("Unable to change power for {} while printing", self.name)));
            }
            self.set_power(&req).await?;
            cur_state = self.state();
            self.process_power_changed().await;
        } else if req != "status" {
            return Err(err(format!("Unsupported power request: {}", req)));
        } else if base_state != cur_state {
            if self.restrict_actions {
                self.notify_power_changed();
            } else {
                self.process_power_changed().await;
            }
        }
        Ok(cur_state)
    }

    async fn notify_external_change(self: Rc<Self>) {
        let _g = self.request_lock.lock().await;
        if self.restrict_actions {
            self.notify_power_changed();
        } else {
            self.process_power_changed().await;
        }
        self.last_update_time.set(loop_time());
    }

    fn start_polling(self: &Rc<Self>) -> bool {
        if self.poll_interval.get().is_none() || self.poll_task.borrow().is_some() {
            return false;
        }
        let w = Rc::downgrade(self);
        let h = spawn(async move {
            loop {
                let d = match w.upgrade() {
                    Some(d) => d,
                    None => return,
                };
                let interval = match d.poll_interval.get() {
                    None => return,
                    Some(i) => i,
                };
                let mut time_diff = loop_time() - d.last_update_time.get();
                if time_diff >= interval {
                    POLLING.with(|p| p.set(true));
                    let _ = d.process_request("status", false).await;
                    POLLING.with(|p| p.set(false));
                    time_diff = d.poll_interval.get().unwrap_or(0.0);
                }
                drop(d);
                tokio::time::sleep(Duration::from_secs_f64(time_diff.max(0.0))).await;
            }
        });
        *self.poll_task.borrow_mut() = Some(h);
        true
    }

    fn should_log_error(&self) -> bool {
        server().is_verbose_enabled() || !POLLING.with(|p| p.get())
    }

    async fn close(&self) {
        if let Some(t) = self.init_task.borrow_mut().take() {
            t.abort();
        }
        self.reset_timer();
        let task = self.poll_task.borrow_mut().take();
        if let Some(t) = task {
            if self.request_lock.try_lock().is_err() {
                self.poll_interval.set(None);
                let ab = t.abort_handle();
                if tokio::time::timeout(Duration::from_secs(1), t).await.is_err() {
                    ab.abort();
                }
            } else {
                t.abort();
            }
        }
    }

    fn reset_timer(&self) {
        if let Some(h) = self.timer_handle.borrow_mut().take() {
            h.abort();
        }
    }

    fn check_timer(&self, timer: Option<f64>) {
        if self.state() == "on" {
            if let Some(t) = timer {
                let name = self.name.clone();
                let h = spawn(async move {
                    tokio::time::sleep(Duration::from_secs_f64(t)).await;
                    if let Some(p) = server().lookup_component::<PrinterPower>("power") {
                        p.set_device_power(&name, "off", false);
                    }
                });
                *self.timer_handle.borrow_mut() = Some(h);
            }
        }
    }

    // ***** Device specific dispatch *****

    async fn init_state(self: Rc<Self>) {
        match &self.kind {
            Kind::Gpio { .. } => {
                let _g = self.request_lock.lock().await;
                let res: Result<(), ServerError> = async {
                    match self.initial_state.get() {
                        None => self.set_power("off").await?,
                        Some(i) => {
                            self.set_power(if i { "on" } else { "off" }).await?;
                            if !self.bound_services.borrow().is_empty() {
                                self.process_bound_services().await;
                            }
                        }
                    }
                    Ok(())
                }
                .await;
                if let Err(e) = res {
                    log::error!("Power Device {}: Initialization failed\n{}", self.name, e.message);
                }
                self.last_update_time.set(loop_time());
            }
            Kind::UHub { .. } => {
                let _g = self.request_lock.lock().await;
                self.uhub_refresh(true).await;
                self.set_initial_state().await;
                self.last_update_time.set(loop_time());
                self.start_polling();
            }
            Kind::TpLink { .. } | Kind::Http(_) => {
                let _g = self.request_lock.lock().await;
                let mut last_err = String::new();
                loop {
                    let res = match &self.kind {
                        Kind::TpLink { .. } => self.tplink_info().await.map(|s| if s != 0 { "on".to_string() } else { "off".into() }),
                        _ => self.http_status_request().await,
                    };
                    match res {
                        Err(e) => {
                            if e.message != last_err {
                                log::error!("Device Init Error: {}\n{}", self.name, e.message);
                                last_err = e.message;
                            }
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                        Ok(state) => {
                            *self.init_task.borrow_mut() = None;
                            self.set_state_val(&state);
                            self.set_initial_state().await;
                            self.notify_power_changed();
                            self.last_update_time.set(loop_time());
                            self.start_polling();
                            return;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    async fn refresh_status(self: &Rc<Self>) -> Result<(), ServerError> {
        match &self.kind {
            Kind::Gpio { .. } => Ok(()),
            Kind::Klipper { is_shutdown, object_name, .. } => {
                let st = self.state();
                if is_shutdown.get() || ["on", "off", "init"].contains(&st.as_str()) {
                    return Ok(());
                }
                let mut req = Map::new();
                req.insert(object_name.clone(), Value::Null);
                let data = server().klippy_apis().query_objects(&req).await.ok();
                if !self.klipper_validate(data.as_ref()) {
                    self.set_state_val("error");
                } else {
                    self.klipper_set_state_from_data(data.as_ref().unwrap());
                }
                Ok(())
            }
            Kind::TpLink { .. } => {
                match self.tplink_info().await {
                    Err(e) => {
                        self.set_state_val("error");
                        if self.should_log_error() {
                            log::error!("Error Refreshing Device Status: {}\n{}", self.name, e.message);
                        }
                    }
                    Ok(s) => self.set_state_val(if s != 0 { "on" } else { "off" }),
                }
                Ok(())
            }
            Kind::Http(_) => {
                match self.http_status_request().await {
                    Err(e) => {
                        self.set_state_val("error");
                        if self.should_log_error() {
                            log::error!("Error Refreshing Device Status: {}\n{}", self.name, e.message);
                        }
                    }
                    Ok(s) => self.set_state_val(&s),
                }
                Ok(())
            }
            Kind::Mqtt(m) => self.mqtt_refresh(m).await,
            Kind::UHub { .. } => {
                self.uhub_refresh(false).await;
                Ok(())
            }
        }
    }

    async fn set_power(&self, state: &str) -> Result<(), ServerError> {
        match &self.kind {
            Kind::Gpio { timer, gpio_out, rf } => {
                self.reset_timer();
                let res = match rf {
                    None => gpio_out.write((state == "on") as i32),
                    Some((on, off)) => {
                        let code = if state == "on" { on.clone() } else { off.clone() };
                        let pin = gpio_out.clone();
                        Self::rf_transmit(&pin, &code)
                    }
                };
                if let Err(e) = res {
                    self.set_state_val("error");
                    let msg = format!("Error Toggling Device Power: {}", self.name);
                    log::error!("{}\n{}", msg, e.message);
                    return Err(err(msg));
                }
                self.set_state_val(state);
                self.check_timer(*timer);
                Ok(())
            }
            Kind::Klipper { is_shutdown, update_tx, timer, gc_cmd, .. } => {
                if is_shutdown.get() {
                    return Err(err(format!("Power Device {}: Cannot set power for device when Klipper is shutdown", self.name)));
                }
                self.reset_timer();
                let (tx, rx) = tokio::sync::oneshot::channel();
                *update_tx.borrow_mut() = Some(tx);
                let value = if state == "on" { "1" } else { "0" };
                let res: Result<(), ServerError> = async {
                    server().klippy_apis().run_gcode(&format!("{} VALUE={}", gc_cmd, value)).await?;
                    match tokio::time::timeout(Duration::from_secs(1), rx).await {
                        Err(_) => Err(ServerError::new("timeout", -1)),
                        Ok(_) => Ok(()),
                    }
                }
                .await;
                *update_tx.borrow_mut() = None;
                match res {
                    Err(e) if e.status_code == -1 => {
                        self.set_state_val("error");
                        return Err(err(format!("Power device {}: Timeout waiting for device state update", self.name)));
                    }
                    Err(e) => {
                        self.set_state_val("error");
                        let msg = format!("Error Toggling Device Power: {}", self.name);
                        log::error!("{}\n{}", msg, e.message);
                        return Err(err(msg));
                    }
                    Ok(()) => {}
                }
                self.check_timer(*timer);
                Ok(())
            }
            Kind::TpLink { timer, .. } => {
                let res: Result<i64, ServerError> = async {
                    if !timer.is_empty() && state == "off" {
                        self.tplink_command("clear_rules").await?;
                        let r = self.tplink_command("count_off").await?;
                        Ok(r["count_down"]["add_rule"]["err_code"].as_i64().ok_or_else(|| err("KeyError"))?)
                    } else {
                        let r = self.tplink_command(state).await?;
                        Ok(r["system"]["set_relay_state"]["err_code"].as_i64().ok_or_else(|| err("KeyError"))?)
                    }
                }
                .await;
                let e = match res {
                    Ok(e) => e,
                    Err(ex) => {
                        log::error!("Power Toggle Error: {}\n{}", self.name, ex.message);
                        1
                    }
                };
                if e != 0 {
                    self.set_state_val("error");
                    return Err(err(format!("Error Toggling Device Power: {}", self.name)));
                }
                self.set_state_val(state);
                Ok(())
            }
            Kind::Http(_) => match self.http_power_request(state).await {
                Err(e) => {
                    self.set_state_val("error");
                    let msg = format!("Error Setting Device Status: {} to {}", self.name, state);
                    log::error!("{}\n{}", msg, e.message);
                    Err(err(msg))
                }
                Ok(s) => {
                    self.set_state_val(&s);
                    Ok(())
                }
            },
            Kind::Mqtt(m) => self.mqtt_set_power(m, state).await,
            Kind::UHub { .. } => match self.run_uhubctl(state).await {
                Err(e) => {
                    self.set_state_val("error");
                    let msg = format!("Power Device {}: Error turning device {}", self.name, state);
                    log::info!("{}\n{}", msg, e);
                    Err(err(msg))
                }
                Ok(r) => {
                    log::debug!("Power Device {}: uhubctl device info: {}", self.name, crate::pyfmt::py_repr(&r));
                    self.set_state_val(r["state"].as_str().unwrap_or("error"));
                    Ok(())
                }
            },
        }
    }

    // ***** RF *****

    fn rf_transmit(pin: &GpioOutputPin, code: &str) -> Result<(), ServerError> {
        const PULSE_LEN: f64 = 0.00035;
        let digit = |w: [u32; 2]| -> Result<(), ServerError> {
            pin.write(1)?;
            std::thread::sleep(Duration::from_secs_f64(w[0] as f64 * PULSE_LEN));
            pin.write(0)?;
            std::thread::sleep(Duration::from_secs_f64(w[1] as f64 * PULSE_LEN));
            Ok(())
        };
        for _ in 0..10 {
            for c in code.chars() {
                match c {
                    '1' => digit([3, 1])?,
                    '0' => digit([1, 3])?,
                    _ => {}
                }
            }
            digit([1, 31])?;
        }
        Ok(())
    }

    // ***** Klipper device *****

    async fn klipper_handle_ready(self: Rc<Self>) {
        let (object_name, unit) = match &self.kind {
            Kind::Klipper { object_name, .. } => (object_name.clone(), server().klippy().unit_name()),
            _ => return,
        };
        if !unit.is_empty() && self.bound_services.borrow().contains(&unit) {
            server().add_warning(
                &format!(
                    "Power: Klipper Device {} contains invalid bound service {}.  Remove this service from the configuration and restart Moonraker.",
                    self.name, unit
                ),
                Some(&format!("kpd_{}", self.name)),
                true,
            );
            self.bound_services.borrow_mut().retain(|s| *s != unit);
        }
        let w = Rc::downgrade(&self);
        let cb: crate::components::klippy_apis::SubCallback = Rc::new(move |status, _| {
            if let Some(d) = w.upgrade() {
                d.klipper_set_state_from_data(&status);
            }
            None
        });
        let mut sub = Map::new();
        sub.insert(object_name, Value::Null);
        let data = server().klippy_apis().subscribe_objects(&sub, Some(cb)).await.ok();
        if !self.klipper_validate(data.as_ref()) {
            self.set_state_val("error");
        } else {
            let _g = self.request_lock.lock().await;
            self.klipper_set_state_from_data(data.as_ref().unwrap());
            self.set_initial_state().await;
            self.notify_power_changed();
        }
    }

    fn klipper_validate(&self, data: Option<&Map<String, Value>>) -> bool {
        let object_name = match &self.kind {
            Kind::Klipper { object_name, .. } => object_name,
            _ => return false,
        };
        match data {
            None => log::info!("Error querying klipper object: {}", object_name),
            Some(d) => match d.get(object_name) {
                None => log::info!("[power]: Invalid Klipper Device {}, no response returned from subscription.", object_name),
                Some(o) if o.get("value").is_none() => log::info!(
                    "[power]: Invalid Klipper Device {}, response does not contain a 'value' parameter",
                    object_name
                ),
                Some(_) => return true,
            },
        }
        false
    }

    fn klipper_set_state_from_data(self: &Rc<Self>, data: &Map<String, Value>) {
        let (object_name, update_tx) = match &self.kind {
            Kind::Klipper { object_name, update_tx, .. } => (object_name, update_tx),
            _ => return,
        };
        let value = match data.get(object_name).and_then(|o| o.get("value")) {
            None | Some(Value::Null) => return,
            Some(v) => v.clone(),
        };
        let state = if crate::pyfmt::py_truthy(&value) { "on" } else { "off" };
        self.klipper_set_state(state);
        if let Some(tx) = update_tx.borrow_mut().take() {
            let _ = tx.send(state.to_string());
        }
    }

    fn klipper_set_state(self: &Rc<Self>, state: &str) {
        let in_request = self.request_lock.try_lock().is_err();
        let last_state = self.state();
        self.set_state_val(state);
        self.last_update_time.set(loop_time());
        if last_state != state && last_state != "init" && !in_request {
            spawn(self.clone().notify_external_change());
        }
    }

    // ***** TPLink *****

    fn tplink_encrypt(data: &[u8]) -> Vec<u8> {
        let mut key: u8 = 0xAB;
        let mut res = (data.len() as u32).to_be_bytes().to_vec();
        for c in data {
            let v = key ^ c;
            key = v;
            res.push(v);
        }
        res
    }

    fn tplink_decrypt(data: &[u8]) -> String {
        let mut key: u8 = 0xAB;
        let mut res = String::new();
        for c in data {
            res.push((key ^ c) as char);
            key = *c;
        }
        res
    }

    fn tplink_command<'a>(&'a self, command: &'a str) -> futures::future::LocalBoxFuture<'a, Result<Value, ServerError>> {
        Box::pin(async move {
            let (timer, addr, output_id, port) = match &self.kind {
                Kind::TpLink { timer, addr, output_id, port } => (timer.clone(), addr.clone(), *output_id, *port),
                _ => return Err(err("not tplink")),
            };
            let out_cmd = match command {
                "on" | "off" => {
                    let mut c = json!({"system": {"set_relay_state": {"state": (command == "on") as i64}}});
                    if let Some(oid) = output_id {
                        let sysinfo = self.tplink_command("info").await?;
                        let child_id = sysinfo["system"]["get_sysinfo"]["children"]
                            .get(oid as usize)
                            .and_then(|c| c.get("id"))
                            .map(crate::pyfmt::py_str)
                            .ok_or_else(|| err("IndexError: list index out of range"))?;
                        c["context"] = json!({"child_ids": [child_id]});
                    }
                    c
                }
                "info" => json!({"system": {"get_sysinfo": {}}}),
                "clear_rules" => json!({"count_down": {"delete_all_rules": null}}),
                "count_off" => {
                    let delay: i64 = timer.trim().parse().map_err(|_| err(format!("invalid literal for int() with base 10: '{}'", timer)))?;
                    json!({"count_down": {"add_rule": {"enable": 1, "delay": delay, "act": 0, "name": "turn off"}}})
                }
                _ => return Err(err(format!("Invalid tplink command: {}", command))),
            };
            let res: Result<Vec<u8>, String> = async {
                let mut stream = tokio::net::TcpStream::connect(format!("{}:{}", addr, port)).await.map_err(|e| e.to_string())?;
                stream.write_all(&Self::tplink_encrypt(out_cmd.to_string().as_bytes())).await.map_err(|e| e.to_string())?;
                let mut buf = vec![0u8; 2048];
                let n = stream.read(&mut buf).await.map_err(|e| e.to_string())?;
                if n < 4 {
                    return Err("unpack requires a buffer of 4 bytes".into());
                }
                let length = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                let mut data = buf[4..n].to_vec();
                let mut retries = 5;
                let mut remaining = length.saturating_sub(data.len());
                while remaining > 0 && retries > 0 {
                    let mut b = vec![0u8; remaining];
                    let n = stream.read(&mut b).await.map_err(|e| e.to_string())?;
                    data.extend_from_slice(&b[..n]);
                    remaining = length.saturating_sub(data.len());
                    retries -= 1;
                }
                if retries == 0 {
                    return Err("Unable to read tplink packet".into());
                }
                Ok(data)
            }
            .await;
            let data = match res {
                Ok(d) => d,
                Err(e) => {
                    let msg = format!("Error sending tplink command: {}", command);
                    log::error!("{}\n{}", msg, e);
                    return Err(err(msg));
                }
            };
            serde_json::from_str(&Self::tplink_decrypt(&data)).map_err(|e| ServerError::new(e.to_string(), 500))
        })
    }

    async fn tplink_info(&self) -> Result<i64, ServerError> {
        let res = self.tplink_command("info").await?;
        let output_id = match &self.kind {
            Kind::TpLink { output_id, .. } => *output_id,
            _ => None,
        };
        let v = match output_id {
            Some(oid) => res["system"]["get_sysinfo"]["children"].get(oid as usize).and_then(|c| c.get("state")).cloned(),
            None => res["system"]["get_sysinfo"].get("relay_state").cloned(),
        };
        v.and_then(|v| v.as_i64()).ok_or_else(|| err("KeyError"))
    }

    // ***** HTTP devices *****

    async fn send_http_command(&self, d: &HttpDev, url: &str, command: &str, retries: usize) -> Result<Value, ServerError> {
        let opts = RequestOpts {
            request_timeout: 20.0,
            attempts: retries,
            retry_pause_time: 1.0,
            enable_cache: false,
            basic_auth: if d.has_basic_auth { Some((d.user.clone(), d.password.clone())) } else { None },
            ..Default::default()
        };
        let resp = d.client.get_opts(url, &[], opts).await;
        resp.raise_for_status(Some(&format!("Error sending '{}' command: {}", self.type_, command)))?;
        resp.json()
    }

    async fn http_command(&self, d: &HttpDev, command: &str) -> Result<Value, ServerError> {
        match &d.kind {
            HttpKind::Tasmota { output_id, timer } => {
                let out_cmd = match command {
                    "on" | "off" => {
                        let mut c = format!("Power{} {}", output_id, command);
                        if !timer.is_empty() && command == "off" {
                            c = format!("Backlog Delay {}0; {}", timer, c);
                        }
                        c
                    }
                    "info" => format!("Power{}", output_id),
                    _ => return Err(err(format!("Invalid tasmota command: {}", command))),
                };
                let query = crate::utils::urlencode(&[("user", d.user.clone()), ("password", d.password.clone()), ("cmnd", out_cmd)]);
                let url = format!("{}://{}/cm?{}", d.protocol, d.addr, query);
                self.send_http_command(d, &url, command, 3).await
            }
            HttpKind::Shelly { output_id, timer } => {
                let mut q: Vec<(&str, String)> = Vec::new();
                match command {
                    "on" | "off" => {
                        q.push(("turn", command.into()));
                        if command == "off" && !timer.is_empty() {
                            q[0].1 = "on".into();
                            q.push(("timer", timer.clone()));
                        }
                    }
                    "info" => {}
                    _ => return Err(err(format!("Invalid shelly command: {}", command))),
                }
                let url = format!("{}://{}/relay/{}?{}", d.protocol, d.addr, output_id, crate::utils::urlencode(&q));
                self.send_http_command(d, &url, command, 3).await
            }
            HttpKind::HomeSeer { device } => {
                let mut q = vec![("user", d.user.clone()), ("pass", d.password.clone()), ("request", command.to_string()), ("ref", device.to_string())];
                if let Some(label) = HOMESEER_LABEL.with(|l| l.borrow_mut().take()) {
                    q.push(("label", label));
                }
                let url = format!("{}://{}:{}/JSON?{}", d.protocol, d.addr, d.port, crate::utils::urlencode(&q));
                self.send_http_command(d, &url, command, 3).await
            }
            HttpKind::Loxone { output_id } => {
                let out_cmd = match command {
                    "on" | "off" => format!("jdev/sps/io/{}/{}", quote(output_id), command),
                    "info" => format!("jdev/sps/io/{}", quote(output_id)),
                    _ => return Err(err(format!("Invalid loxonev1 command: {}", command))),
                };
                let url = format!("http://{}/{}", d.addr, out_cmd);
                self.send_http_command(d, &url, command, 3).await
            }
            HttpKind::SmartThings { device, token } => {
                let (method, url, body) = match command {
                    "on" | "off" => (
                        "POST",
                        format!("{}://{}/v1/devices/{}/commands", d.protocol, d.addr, quote(device)),
                        Body::Json(json!([{"component": "main", "capability": "switch", "command": command}])),
                    ),
                    "info" => (
                        "GET",
                        format!("{}://{}/v1/devices/{}/components/main/capabilities/switch/status", d.protocol, d.addr, quote(device)),
                        Body::None,
                    ),
                    _ => return Err(err(format!("Invalid SmartThings command: {}", command))),
                };
                let headers = vec![("Authorization".to_string(), format!("Bearer {}", token))];
                let resp = d.client.request(method, &url, body, &headers, RequestOpts { attempts: 3, ..Default::default() }).await;
                resp.raise_for_status(Some(&format!("Error sending SmartThings command: {}", command)))?;
                resp.json()
            }
            HttpKind::HomeAssistant { device, token, domain, .. } => {
                let (method, out_cmd, body) = match command {
                    "on" | "off" => ("POST", format!("api/services/{}/turn_{}", quote(domain), command), Body::Json(json!({"entity_id": device}))),
                    "info" => ("GET", format!("api/states/{}", quote(device)), Body::None),
                    _ => return Err(err(format!("Invalid homeassistant command: {}", command))),
                };
                let url = format!("{}://{}:{}/{}", d.protocol, d.addr, d.port, out_cmd);
                let headers = vec![("Authorization".to_string(), format!("Bearer {}", token))];
                let resp = d.client.request(method, &url, body, &headers, RequestOpts { attempts: 3, ..Default::default() }).await;
                resp.raise_for_status(Some(&format!("Error sending homeassistant command: {}", command)))?;
                if method == "GET" {
                    return resp.json();
                }
                Ok(json!({}))
            }
            _ => Err(err("unsupported")),
        }
    }

    fn key_err(k: &str) -> ServerError {
        ServerError::new(format!("KeyError: '{}'", k), 500)
    }

    async fn http_status_request(&self) -> Result<String, ServerError> {
        let d = match &self.kind {
            Kind::Http(d) => d,
            _ => return Err(err("not http")),
        };
        match &d.kind {
            HttpKind::Tasmota { output_id, .. } => {
                let res = self.http_command(d, "info").await?;
                tasmota_state(&res, *output_id)
            }
            HttpKind::Shelly { timer, .. } => {
                let res = self.http_command(d, "info").await?;
                Ok(shelly_state(&res, timer))
            }
            HttpKind::SmartThings { .. } => {
                let res = self.http_command(d, "info").await?;
                res["switch"]["value"].as_str().map(|s| s.to_lowercase()).ok_or_else(|| Self::key_err("switch"))
            }
            HttpKind::HomeSeer { .. } => {
                let res = self.http_command(d, "getstatus").await?;
                res["Devices"][0]["status"].as_str().map(|s| s.to_lowercase()).ok_or_else(|| Self::key_err("Devices"))
            }
            HttpKind::HomeAssistant { .. } => {
                let res = self.http_command(d, "info").await?;
                res.get("state").map(crate::pyfmt::py_str).ok_or_else(|| Self::key_err("state"))
            }
            HttpKind::Loxone { .. } => {
                let res = self.http_command(d, "info").await?;
                loxone_state(&res)
            }
            HttpKind::Hue { device_id, device_type, on_state, .. } => {
                let url = format!("{}://{}:{}/api/{}/{}s/{}", d.protocol, d.addr, d.port, quote(&d.user), device_type, quote(device_id));
                let ret = d.client.request("GET", &url, Body::None, &[], RequestOpts::default()).await;
                let resp = ret.json()?;
                Ok(if crate::pyfmt::py_truthy(&resp["state"][on_state.as_str()]) { "on".into() } else { "off".into() })
            }
            HttpKind::Generic { .. } => self.generic_request(d, "status").await,
        }
    }

    async fn http_power_request(&self, state: &str) -> Result<String, ServerError> {
        let d = match &self.kind {
            Kind::Http(d) => d,
            _ => return Err(err("not http")),
        };
        match &d.kind {
            HttpKind::Tasmota { output_id, timer } => {
                let res = self.http_command(d, state).await?;
                if timer.is_empty() || state != "off" {
                    return tasmota_state(&res, *output_id);
                }
                Ok(state.to_string())
            }
            HttpKind::Shelly { timer, .. } => {
                let res = self.http_command(d, state).await?;
                Ok(shelly_state(&res, timer))
            }
            HttpKind::SmartThings { .. } => {
                let res = self.http_command(d, state).await?;
                let ack = res["results"][0]["status"].as_str().map(|s| s.to_lowercase()).ok_or_else(|| Self::key_err("results"))?;
                Ok(if ack == "accepted" { state.to_string() } else { "error".into() })
            }
            HttpKind::HomeSeer { .. } => {
                let mut label = state.to_string();
                if let Some(f) = label.get_mut(0..1) {
                    f.make_ascii_uppercase();
                }
                HOMESEER_LABEL.with(|l| *l.borrow_mut() = Some(label));
                self.http_command(d, "controldevicebylabel").await?;
                Ok(state.to_string())
            }
            HttpKind::HomeAssistant { status_delay, .. } => {
                self.http_command(d, state).await?;
                tokio::time::sleep(Duration::from_secs_f64(*status_delay)).await;
                self.http_status_request().await
            }
            HttpKind::Loxone { .. } => {
                let res = self.http_command(d, state).await?;
                loxone_state(&res)
            }
            HttpKind::Hue { device_id, device_type, state_key, .. } => {
                let url = format!(
                    "{}://{}:{}/api/{}/{}s/{}/{}",
                    d.protocol,
                    d.addr,
                    d.port,
                    quote(&d.user),
                    device_type,
                    quote(device_id),
                    quote(state_key)
                );
                let ret = d.client.request("PUT", &url, Body::Json(json!({"on": state == "on"})), &[], RequestOpts::default()).await;
                let resp = ret.json()?;
                let state_url = format!("/{}s/{}/{}/on", device_type, device_id, state_key);
                let v = resp[0]["success"].get(&state_url).cloned().ok_or_else(|| Self::key_err(&state_url))?;
                Ok(if crate::pyfmt::py_truthy(&v) { "on".into() } else { "off".into() })
            }
            HttpKind::Generic { .. } => self.generic_request(d, state).await,
        }
    }

    async fn generic_request(&self, d: &HttpDev, command: &str) -> Result<String, ServerError> {
        let (urls, request_template, response_template) = match &d.kind {
            HttpKind::Generic { urls, request_template, response_template } => (urls, request_template, response_template),
            _ => return Err(err("not generic")),
        };
        let url = urls.get(command).cloned().ok_or_else(|| Self::key_err(command))?;
        let req_obj = HttpRequestObj::new(&url);
        let client = d.client.clone();
        let basic = if d.has_basic_auth { Some((d.user.clone(), d.password.clone())) } else { None };
        let send = {
            let client = client.clone();
            let basic = basic.clone();
            move |req: Value| {
                let client = client.clone();
                let basic = basic.clone();
                async move {
                    let kw = req.get("kwargs").cloned().unwrap_or(json!({}));
                    let f = |k: &str, d: f64| kw.get(k).and_then(|v| v.as_f64()).unwrap_or(d);
                    let opts = RequestOpts {
                        request_timeout: f("request_timeout", 20.0),
                        connect_timeout: f("connect_timeout", 5.0),
                        attempts: f("attempts", 3.0) as usize,
                        retry_pause_time: f("retry_pause_time", 1.0),
                        enable_cache: false,
                        basic_auth: basic,
                        ..Default::default()
                    };
                    let body = match &req["body"] {
                        Value::Null => Body::None,
                        Value::String(s) => Body::Text(s.clone()),
                        v => Body::Json(v.clone()),
                    };
                    let headers: Vec<(String, String)> = req["headers"]
                        .as_object()
                        .map(|m| m.iter().map(|(k, v)| (k.clone(), crate::pyfmt::py_str(v))).collect())
                        .unwrap_or_default();
                    let method = req["method"].as_str().unwrap_or("GET").to_string();
                    let url = req["url"].as_str().unwrap_or("").to_string();
                    response_json(&client.request(&method, &url, body, &headers, opts).await)
                }
            }
        };
        let send_rc = Rc::new(send);
        let s2 = send_rc.clone();
        let http_send: AsyncFn = Rc::new(move |args: Vec<Value>, _kw| {
            let s = s2.clone();
            Box::pin(async move { Ok((s)(args.into_iter().next().unwrap_or(Value::Null)).await) })
        });
        let async_sleep: AsyncFn = Rc::new(|args: Vec<Value>, _kw| {
            Box::pin(async move {
                let t = args.first().and_then(|v| v.as_f64()).unwrap_or(0.0);
                tokio::time::sleep(Duration::from_secs_f64(t.max(0.0))).await;
                Ok(Value::Null)
            })
        });
        let ctx = json!({"command": command, "urls": urls});
        let builder = |o: Arc<HttpRequestObj>| -> ObjBuilder {
            Box::new(move |caller: Caller| {
                *o.caller.lock().unwrap() = Some(caller);
                minijinja::Value::from_dyn_object(o)
            })
        };
        let response = match request_template {
            Some(t) => {
                t.render_async_ext(
                    &ctx,
                    vec![("http_send", http_send.clone()), ("async_sleep", async_sleep.clone())],
                    vec![("http_request", builder(req_obj.clone()))],
                )
                .await?;
                req_obj.st.lock().unwrap().last_response.clone().ok_or_else(|| err("Failed to receive a response"))?
            }
            None => {
                let r = (send_rc)(req_obj.state_json()).await;
                req_obj.st.lock().unwrap().last_response = Some(r.clone());
                r
            }
        };
        if !response["error"].is_null() {
            let code = response["status_code"].as_i64().unwrap_or(500);
            return Err(ServerError::new(crate::pyfmt::py_str(&response["error"]), code));
        }
        let result = response_template
            .render_async_ext(&ctx, vec![("http_send", http_send), ("async_sleep", async_sleep)], vec![("http_request", builder(req_obj))])
            .await?
            .to_lowercase();
        if result != "on" && result != "off" {
            return Err(err(format!("Invalid result: {}", result)));
        }
        Ok(result)
    }

    // ***** MQTT device *****

    fn mqtt_on_state_update(self: &Rc<Self>, payload: Vec<u8>) {
        let m = match &self.kind {
            Kind::Mqtt(m) => m,
            _ => return,
        };
        let last_state = self.state();
        let in_request = self.request_lock.try_lock().is_err();
        m.response_count.set(m.response_count.get() + 1);
        let text = String::from_utf8_lossy(&payload).to_string();
        let ctx = json!({"payload": text, "last_request": *m.last_request.borrow(), "response_count": m.response_count.get()});
        let mut error: Option<ServerError> = None;
        let mut response = String::new();
        match m.state_response.render(&ctx) {
            Err(e) => {
                error = Some(e);
                self.set_state_val("error");
            }
            Ok(r) => {
                response = r.to_lowercase();
                if response == "discard" {
                    return;
                } else if response != "on" && response != "off" {
                    let err_msg = format!("Invalid State Received. Raw Payload: '{}', Rendered: '{}", text, response);
                    log::info!("MQTT Power Device {}: {}", self.name, err_msg);
                    error = Some(ServerError::new(err_msg, 500));
                    self.set_state_val("error");
                } else {
                    self.set_state_val(&response);
                }
            }
        }
        self.last_update_time.set(loop_time());
        if !in_request && last_state != self.state() {
            log::info!("MQTT Power Device {}: External Power event detected, new state: {}", self.name, self.state());
            spawn(self.clone().notify_external_change());
        }
        if let Some(tx) = m.query_response.borrow_mut().take() {
            let _ = tx.send(match error {
                Some(e) => Err(e),
                None => Ok(response),
            });
        }
    }

    async fn mqtt_wait_for_update(
        &self,
        m: &MqttDev,
        rx: tokio::sync::oneshot::Receiver<Result<String, ServerError>>,
        do_query: bool,
    ) -> Result<String, ServerError> {
        if let (Some(topic), true) = (&m.query_topic, do_query) {
            let payload = match &m.query_payload {
                Some(t) => Value::String(t.render(&json!({}))?),
                None => Value::Null,
            };
            m.mqtt.publish_topic(topic, payload, m.qos, false).await?;
        }
        match tokio::time::timeout(Duration::from_secs_f64(m.state_timeout), rx).await {
            Err(_) => Err(ServerError::new("TimeoutError", -2)),
            Ok(Err(_)) => Err(ServerError::new("cancelled", 500)),
            Ok(Ok(r)) => r,
        }
    }

    async fn mqtt_on_connected(self: Rc<Self>) {
        let m = match &self.kind {
            Kind::Mqtt(m) => m,
            _ => return,
        };
        let _g = self.request_lock.lock().await;
        let st = self.state();
        if st == "on" || st == "off" {
            return;
        }
        self.set_state_val("init");
        let mut success = false;
        while m.mqtt.is_connected() {
            let (tx, rx) = tokio::sync::oneshot::channel();
            *m.query_response.borrow_mut() = Some(tx);
            match self.mqtt_wait_for_update(m, rx, true).await {
                Err(e) if e.status_code == -2 => {
                    if m.query_topic.is_none() {
                        log::info!("MQTT Power Device {}: Initialization Timed Out", self.name);
                        break;
                    }
                }
                Err(e) => {
                    log::error!("MQTT Power Device {}: Init Failed\n{}", self.name, e.message);
                    break;
                }
                Ok(_) => {
                    success = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        *m.query_response.borrow_mut() = None;
        if !success {
            self.set_state_val("error");
        } else {
            log::info!("MQTT Power Device {} initialized", self.name);
        }
        self.set_initial_state().await;
        self.initial_state.set(None);
        self.notify_power_changed();
    }

    async fn mqtt_on_disconnected(self: Rc<Self>) {
        let m = match &self.kind {
            Kind::Mqtt(m) => m,
            _ => return,
        };
        if let Some(tx) = m.query_response.borrow_mut().take() {
            let _ = tx.send(Err(ServerError::new("MQTT Disconnected", 503)));
        }
        let _g = self.request_lock.lock().await;
        self.set_state_val("error");
        self.notify_power_changed();
    }

    async fn mqtt_refresh(&self, m: &MqttDev) -> Result<(), ServerError> {
        let st = self.state();
        if m.query_topic.is_some() && (m.must_query || (st != "on" && st != "off")) {
            if !m.mqtt.is_connected() {
                return Err(ServerError::new(format!("MQTT Power Device {}: MQTT Not Connected", self.name), 503));
            }
            *m.last_request.borrow_mut() = "refresh".into();
            m.response_count.set(0);
            let (tx, rx) = tokio::sync::oneshot::channel();
            *m.query_response.borrow_mut() = Some(tx);
            if let Err(e) = self.mqtt_wait_for_update(m, rx, true).await {
                log::error!("MQTT Power Device {}: Failed to refresh state\n{}", self.name, e.message);
                self.set_state_val("error");
            }
            *m.query_response.borrow_mut() = None;
            m.last_request.borrow_mut().clear();
            m.response_count.set(0);
        }
        Ok(())
    }

    async fn mqtt_set_power(&self, m: &MqttDev, state: &str) -> Result<(), ServerError> {
        if !m.mqtt.is_connected() {
            return Err(ServerError::new(format!("MQTT Power Device {}: MQTT Not Connected", self.name), 503));
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        *m.query_response.borrow_mut() = Some(tx);
        *m.last_request.borrow_mut() = format!("power_{}", state);
        m.response_count.set(0);
        let res: Result<String, ServerError> = async {
            let payload = m.cmd_payload.render(&json!({"command": state}))?;
            m.mqtt.publish_topic(&m.cmd_topic, Value::String(payload), m.qos, m.retain_cmd_state).await?;
            self.mqtt_wait_for_update(m, rx, m.must_query).await
        }
        .await;
        let new_state = match res {
            Ok(s) => s,
            Err(e) => {
                log::error!("MQTT Power Device {}: Failed to set state\n{}", self.name, e.message);
                "error".into()
            }
        };
        *m.query_response.borrow_mut() = None;
        m.last_request.borrow_mut().clear();
        m.response_count.set(0);
        self.set_state_val(&new_state);
        if new_state == "error" {
            return Err(ServerError::new(format!("MQTT Power Device {}: Failed to set device to state '{}'", self.name, state), 500));
        }
        Ok(())
    }

    // ***** uhubctl *****

    async fn uhub_refresh(&self, is_init: bool) {
        match self.run_uhubctl("info").await {
            Err(e) => {
                self.set_state_val("error");
                if !self.should_log_error() {
                    return;
                }
                let ename = if is_init { "Refresh" } else { "Initialization" };
                log::info!("Power Device {}: {} Error\n{}", self.name, ename, e);
            }
            Ok(r) => {
                if !POLLING.with(|p| p.get()) {
                    log::debug!("Power Device {}: uhubctl device info: {}", self.name, crate::pyfmt::py_repr(&r));
                }
                self.set_state_val(r["state"].as_str().unwrap_or("error"));
            }
        }
    }

    /// Returns the parsed port info, the error string includes uhubctl output
    async fn run_uhubctl(&self, action: &str) -> Result<Value, String> {
        let (location, port) = match &self.kind {
            Kind::UHub { location, port } => (location.clone(), *port),
            _ => return Err("not uhubctl".into()),
        };
        let mut cmd = format!("uhubctl -l {}", location);
        if let Some(p) = port {
            cmd.push_str(&format!(" -p {}", p));
        }
        let mut search_prefix = format!("Current status for hub {}", location);
        if action == "on" || action == "off" {
            cmd.push_str(&format!(" -a {}", action));
            search_prefix = format!("New status for hub {}", location);
        }
        let shell: Rc<crate::components::shell_command::ShellCommandFactory> =
            server().lookup_component("shell_command").ok_or("shell_command")?;
        let resp = shell
            .exec_cmd(&cmd, crate::components::shell_command::ExecOpts { log_complete: false, ..Default::default() })
            .await
            .map_err(|e| format!("{}\nuhubctrl output: {}", e.message, String::from_utf8_lossy(&e.stderr)))?;
        let re = regex::Regex::new(
            r"(?xi)^\s*
            (?:Port\s(?P<port>[0-9]+):)
            (?:\s(?P<bits>[0-9a-f]{4}))
            (?:\s(?P<pstate>power|off))
            (?P<flags>(?:\s[0-9a-z.]+)+)?
            (?:\s\[(?P<desc>.+)\])?
            \s*$",
        )
        .unwrap();
        for line in resp.lines() {
            if !search_prefix.is_empty() {
                if line.starts_with(&search_prefix) {
                    search_prefix.clear();
                }
                continue;
            }
            let c = match re.captures(line.trim()) {
                None => continue,
                Some(c) => c,
            };
            let pnum: i64 = match c["port"].parse() {
                Ok(p) => p,
                Err(_) => continue,
            };
            let bits = match i64::from_str_radix(&c["bits"], 16) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if port.map(|p| p != pnum).unwrap_or(false) {
                continue;
            }
            let state = if &c["pstate"] == "power" { "on" } else { "off" };
            let flags: Vec<String> = c.name("flags").map(|f| f.as_str().split_whitespace().map(String::from).collect()).unwrap_or_default();
            return Ok(json!({
                "port": pnum,
                "status_bits": bits,
                "state": state,
                "flags": flags,
                "desc": c.name("desc").map(|d| d.as_str()),
            }));
        }
        Err(format!(
            "Failed to receive response for device at location {}, port {}, ",
            location,
            port.map(|p| p.to_string()).unwrap_or("None".into())
        ))
    }
}

thread_local! {
    // set while the poll task runs a status request (asyncio.current_task() is poll_task)
    static POLLING: Cell<bool> = const { Cell::new(false) };
    static HOMESEER_LABEL: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn tasmota_state(res: &Value, output_id: i64) -> Result<String, ServerError> {
    match res.get(format!("POWER{}", output_id)) {
        Some(v) => Ok(crate::pyfmt::py_str(v).to_lowercase()),
        None if output_id == 1 => res.get("POWER").map(|v| crate::pyfmt::py_str(v).to_lowercase()).ok_or_else(|| PowerDevice::key_err("POWER")),
        None => Err(PowerDevice::key_err(&format!("POWER{}", output_id))),
    }
}

fn shelly_state(res: &Value, timer: &str) -> String {
    let ison = crate::pyfmt::py_truthy(&res["ison"]);
    let remaining = if timer.is_empty() { 0.0 } else { res["timer_remaining"].as_f64().unwrap_or(0.0) };
    if ison && remaining == 0.0 {
        "on".into()
    } else {
        "off".into()
    }
}

fn loxone_state(res: &Value) -> Result<String, ServerError> {
    let v = &res["LL"]["value"];
    let n: i64 = match v {
        Value::String(s) => s.trim().parse().map_err(|_| ServerError::exc(format!("invalid literal for int() with base 10: '{}'", s)))?,
        Value::Number(n) => n.as_f64().unwrap_or(0.0) as i64,
        _ => return Err(PowerDevice::key_err("LL")),
    };
    Ok(if n == 1 { "on".into() } else { "off".into() })
}

// ***** Printer Power *****

pub struct PrinterPower {
    devices: RefCell<IndexMap<String, Rc<PowerDevice>>>,
}

const DEV_TYPES: [&str; 14] = [
    "gpio",
    "klipper_device",
    "tplink_smartplug",
    "tasmota",
    "shelly",
    "homeseer",
    "homeassistant",
    "loxonev1",
    "rf",
    "mqtt",
    "smartthings",
    "hue",
    "http",
    "uhubctl",
];

impl PrinterPower {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, ConfigError> {
        let srv = server();
        let sections = config.get_prefix_sections("power");
        log::info!("Power component loading devices: {}", crate::pyfmt::py_repr(&json!(sections)));
        let mut devices = IndexMap::new();
        for section in sections {
            let cfg = config.getsection(&section);
            let dev_type = cfg.get("type")?;
            if !DEV_TYPES.contains(&dev_type.as_str()) {
                return Err(ConfigHelper::error(format!("Unsupported Device Type: {}", dev_type)));
            }
            match PowerDevice::new(&cfg) {
                Ok(d) => {
                    devices.insert(d.name.clone(), d);
                }
                Err(e) => {
                    srv.add_warning(&format!("Failed to load power device [{}]\n{}", cfg.get_name(), e.0), None, true);
                }
            }
        }
        let pp = Rc::new(PrinterPower { devices: RefCell::new(devices) });
        let p = pp.clone();
        let _ = srv.register_endpoint(
            "/machine/device_power/devices",
            RequestType::GET,
            callback(move |_req| {
                let p = p.clone();
                async move {
                    let list: Vec<Value> = p.devices.borrow().values().map(|d| d.get_device_info()).collect();
                    Ok(json!({"devices": list}))
                }
            }),
        );
        for ep in ["status", "on", "off"] {
            let p = pp.clone();
            let rt = if ep == "status" { RequestType::GET } else { RequestType::POST };
            let _ = srv.register_endpoint(
                &format!("/machine/device_power/{}", ep),
                rt,
                callback(move |req| {
                    let p = p.clone();
                    async move { p.handle_batch_power_request(req).await }
                }),
            );
        }
        let p = pp.clone();
        let _ = srv.register_endpoint(
            "/machine/device_power/device",
            RequestType::GET | RequestType::POST,
            callback(move |req| {
                let p = p.clone();
                async move { p.handle_single_power_request(req).await }
            }),
        );
        let p = Rc::downgrade(&pp);
        let _ = srv.register_remote_method(
            "set_device_power",
            crate::components::klippy_connection::remote_method(move |args| {
                if let Some(p) = p.upgrade() {
                    let device = match args.get("device") {
                        Some(d) => crate::pyfmt::py_str(d),
                        None => {
                            log::error!("set_device_power() missing 1 required positional argument: 'device'");
                            return;
                        }
                    };
                    let state = args.get("state").cloned().unwrap_or(Value::Null);
                    let force = args.get("force").map(crate::pyfmt::py_truthy).unwrap_or(false);
                    let req = match &state {
                        Value::Bool(b) => if *b { "on".to_string() } else { "off".to_string() },
                        Value::String(s) => match s.to_lowercase().as_str() {
                            "true" => "on".into(),
                            "false" => "off".into(),
                            other => other.to_string(),
                        },
                        _ => String::new(),
                    };
                    if !["on", "off", "toggle"].contains(&req.as_str()) {
                        log::info!("Invalid state received: {}", crate::pyfmt::py_str(&state));
                        return;
                    }
                    p.set_device_power(&device, &req, force);
                }
            }),
        );
        let p = Rc::downgrade(&pp);
        srv.on_event("server:klippy_shutdown", move |_| {
            if let Some(p) = p.upgrade() {
                let devs: Vec<Rc<PowerDevice>> = p.devices.borrow().values().cloned().collect();
                for d in devs {
                    d.process_klippy_shutdown();
                }
            }
        });
        let p = Rc::downgrade(&pp);
        srv.on_event_async("job_queue:job_queue_changed", move |args| {
            let p = p.clone();
            async move {
                if let Some(p) = p.upgrade() {
                    p.handle_job_queued(args.into_iter().next().unwrap_or(Value::Null)).await;
                }
            }
        });
        srv.register_notification("power:power_changed", None);
        Ok(pp)
    }

    async fn handle_job_queued(&self, queue_info: Value) {
        if queue_info.get("action").and_then(|v| v.as_str()) != Some("jobs_added") {
            return;
        }
        let devs: Vec<(String, Rc<PowerDevice>)> = self.devices.borrow().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (name, dev) in devs {
            if dev.on_when_queued {
                let fname = queue_info
                    .get("updated_queue")
                    .and_then(|q| q.get(0))
                    .and_then(|j| j.get("filename"))
                    .map(crate::pyfmt::py_str)
                    .unwrap_or("unknown".into());
                log::info!("Power Device {}: Job '{}' queued, powering on", name, fname);
                let _ = dev.process_request("on", false).await;
            }
        }
    }

    async fn handle_single_power_request(&self, req: WebRequest) -> ApiResult {
        let dev_name = req.get_str("device")?;
        let dev = self.devices.borrow().get(&dev_name).cloned().ok_or_else(|| err(format!("No valid device named {}", dev_name)))?;
        let action = if req.get_request_type() == RequestType::GET {
            "status".to_string()
        } else {
            let a = req.get_str("action")?.to_lowercase();
            if !["on", "off", "toggle"].contains(&a.as_str()) {
                return Err(err(format!("Invalid requested action '{}'", a)));
            }
            a
        };
        let result = dev.process_request(&action, false).await?;
        Ok(json!({dev_name: result}))
    }

    async fn handle_batch_power_request(&self, req: WebRequest) -> ApiResult {
        let args = req.get_args().clone();
        if args.is_empty() {
            return Err(err("No arguments provided"));
        }
        let action = req.get_endpoint().rsplit('/').next().unwrap_or("").to_string();
        let mut result = Map::new();
        for name in args.keys() {
            let dev = self.devices.borrow().get(name).cloned();
            match dev {
                Some(d) => {
                    let r = d.process_request(&action, false).await?;
                    result.insert(name.clone(), json!(r));
                }
                None => {
                    result.insert(name.clone(), json!("device_not_found"));
                }
            }
        }
        Ok(Value::Object(result))
    }

    pub fn set_device_power(&self, device: &str, request: &str, force: bool) {
        let dev = match self.devices.borrow().get(device).cloned() {
            None => {
                log::info!("No device found: {}", device);
                return;
            }
            Some(d) => d,
        };
        let req = request.to_string();
        spawn(async move {
            let _ = dev.process_request(&req, force).await;
        });
    }
}

#[async_trait(?Send)]
impl Component for PrinterPower {
    fn has_init(&self) -> bool {
        true
    }

    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        let devs: Vec<Rc<PowerDevice>> = self.devices.borrow().values().cloned().collect();
        for dev in devs {
            if !dev.initialize()? {
                server().add_warning(&format!("Power device '{}' failed to initialize", dev.name), None, true);
            }
        }
        Ok(())
    }

    async fn close(self: Rc<Self>) {
        let devs: Vec<Rc<PowerDevice>> = self.devices.borrow().values().cloned().collect();
        futures::future::join_all(devs.iter().map(|d| d.close())).await;
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<PrinterPower>, ConfigError> {
    PrinterPower::new(config)
}

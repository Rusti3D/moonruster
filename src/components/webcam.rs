// Centralized webcam configuration (port of moonraker/components/webcam.py)

use crate::common::*;
use crate::confighelper::ConfigHelper;
use crate::pyfmt::py_str;
use crate::server::{server, Component};
use async_trait::async_trait;
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::rc::Rc;

/// Moonraker field name -> database field name
const CAM_FIELDS: [(&str, &str); 11] = [
    ("name", "name"),
    ("service", "service"),
    ("target_fps", "targetFps"),
    ("stream_url", "urlStream"),
    ("snapshot_url", "urlSnapshot"),
    ("flip_horizontal", "flipX"),
    ("flip_vertical", "flipY"),
    ("enabled", "enabled"),
    ("target_fps_idle", "targetFpsIdle"),
    ("aspect_ratio", "aspectRatio"),
    ("icon", "icon"),
];

thread_local! {
    static DEFAULT_HOST: RefCell<String> = RefCell::new("http://127.0.0.1".into());
}

/// A webcam, fields kept in python attribute order. The manager holds each
/// one by Rc and updates it in place (WebCam.update), so every holder sees
/// the same camera, as in Moonraker
#[derive(Clone)]
pub struct WebCam {
    pub fields: RefCell<Map<String, Value>>,
}

fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

impl WebCam {
    #[allow(clippy::too_many_arguments)]
    fn build(values: Vec<(&str, Value)>) -> Result<Self, ServerError> {
        let mut fields = Map::new();
        for (k, v) in values {
            fields.insert(k.to_string(), v);
        }
        let rotation = fields.get("rotation").and_then(|v| v.as_i64()).unwrap_or(-1);
        if ![0, 90, 180, 270].contains(&rotation) {
            return Err(ServerError::bad(format!("Invalid value for 'rotation': {}", py_str(&fields["rotation"]))));
        }
        let ar = fields.get("aspect_ratio").map(py_str).unwrap_or_default();
        let (pre, post) = ar.split_once(':').unwrap_or_default();
        let isdigit = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
        if !(isdigit(pre) && ar.contains(':') && isdigit(post)) {
            return Err(ServerError::bad(format!("Invalid value for 'aspect_ratio': {}", ar)));
        }
        Ok(WebCam { fields: RefCell::new(fields) })
    }

    pub fn name(&self) -> String {
        self.fields.borrow().get("name").map(py_str).unwrap_or_default()
    }
    pub fn uid(&self) -> String {
        self.fields.borrow().get("uid").map(py_str).unwrap_or_default()
    }
    pub fn source(&self) -> String {
        self.fields.borrow().get("source").map(py_str).unwrap_or_default()
    }
    pub fn get(&self, f: &str) -> Value {
        self.fields.borrow().get(f).cloned().unwrap_or(Value::Null)
    }

    pub fn as_dict(&self) -> Value {
        let mut data = self.fields.borrow().clone();
        if crate::utils::is_cosmos() {
            for key in ["stream_url", "snapshot_url"] {
                if let Some(Value::String(url)) = data.get(key) {
                    let expanded = Self::expand_url(url);
                    data.insert(key.into(), json!(expanded));
                }
            }
        }
        Value::Object(data)
    }

    /// ":8080/stream" -> default host on port 8080 (COSMOS patch to moonraker;
    /// applied on COSMOS only, upstream Moonraker keeps the URL as given)
    fn expand_url(url: &str) -> String {
        static RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| crate::utils::regex(r"^:(\d+)(/.*)?$"));
        match RE.captures(url) {
            Some(c) => {
                let host = DEFAULT_HOST.with(|h| h.borrow().clone());
                format!("{}:{}{}", host, &c[1], c.get(2).map(|m| m.as_str()).unwrap_or(""))
            }
            None => url.to_string(),
        }
    }

    pub async fn get_stream_url(&self, convert_local: bool) -> Result<String, ServerError> {
        Self::get_url(&py_str(&self.get("stream_url")), convert_local).await
    }

    pub async fn get_snapshot_url(&self, convert_local: bool) -> Result<String, ServerError> {
        Self::get_url(&py_str(&self.get("snapshot_url")), convert_local).await
    }

    async fn get_url(url: &str, convert_local: bool) -> Result<String, ServerError> {
        if url.is_empty() {
            return Err(ServerError::bad("Empty URL Provided"));
        }
        let mut url = if crate::utils::is_cosmos() { Self::expand_url(url) } else { url.to_string() };
        if !crate::utils::regex(r"^\w+://[^/]+").is_match(&url) {
            let host = DEFAULT_HOST.with(|h| h.borrow().clone());
            url = format!("{}/{}", host, url.trim_start_matches('/'));
        }
        if !convert_local {
            return Ok(url);
        }
        Self::convert_local(&url).await
    }

    fn get_local_ips() -> Vec<String> {
        let mut out = Vec::new();
        if let Some(m) = server().get::<crate::components::machine::Machine>() {
            if let Some(net) = m.get_system_info().get("network").and_then(|v| v.as_object()) {
                for iface in net.values() {
                    for a in iface["ip_addresses"].as_array().cloned().unwrap_or_default() {
                        out.push(py_str(&a["address"]));
                    }
                }
            }
        }
        out
    }

    pub async fn convert_local(url: &str) -> Result<String, ServerError> {
        let re = crate::utils::regex(r"^(\w+)://([^/]+)(/.*)?");
        let c = match re.captures(url) {
            Some(c) => c,
            None => return Ok(url.to_string()),
        };
        let scheme = c[1].to_string();
        let addr_full = c[2].to_string();
        let fragment = c.get(3).map(|m| m.as_str().to_string()).unwrap_or_default();
        let am = if addr_full.starts_with('[') {
            crate::utils::regex(r"^\[(.+)\](:\d+)?").captures(&addr_full).map(|c| (c[1].to_string(), c.get(2).map(|m| m.as_str().to_string())))
        } else {
            crate::utils::regex(r"^([^:]+)(:\d+)?").captures(&addr_full).map(|c| (c[1].to_string(), c.get(2).map(|m| m.as_str().to_string())))
        };
        let (mut addr, port) = match am {
            Some(x) => x,
            None => return Ok(url.to_string()),
        };
        let port = match port {
            Some(p) => p.trim_start_matches(':').to_string(),
            None => match scheme.as_str() {
                "http" => "80".into(),
                "https" => "443".into(),
                "rtsp" => "554".into(),
                _ => return Ok(url.to_string()),
            },
        };
        // socket.getaddrinfo(addr, int(port))
        if let Ok(pnum) = port.parse::<u16>() {
            let host = addr.clone();
            if let Ok(Ok(mut addrs)) =
                tokio::task::spawn_blocking(move || std::net::ToSocketAddrs::to_socket_addrs(&(host.as_str(), pnum))).await
            {
                if let Some(a) = addrs.next() {
                    addr = a.ip().to_string();
                }
            }
        }
        let ip: std::net::IpAddr = match addr.parse() {
            Ok(i) => i,
            Err(_) => return Ok(url.to_string()),
        };
        if ip.is_loopback() || !Self::get_local_ips().contains(&addr) {
            return Ok(url.to_string());
        }
        let shell = server().get::<crate::components::shell_command::ShellCommandFactory>();
        if let Some(sh) = shell {
            if let Ok(resp) = sh.exec("ss -ltn", 2.0).await {
                for line in resp.split('\n').skip(1) {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() < 5 {
                        continue;
                    }
                    // `laddr, lport = parts[3].split(":")` raises on ipv6 entries
                    let (laddr, lport) = match parts[3].split(':').collect::<Vec<_>>()[..] {
                        [a, p] => (a, p),
                        [_] => return Err(ServerError::exc("not enough values to unpack (expected 2, got 1)")),
                        _ => return Err(ServerError::exc("too many values to unpack (expected 2)")),
                    };
                    if lport == port {
                        if laddr == "[::]" {
                            return Ok(format!("{}://[::1]:{}{}", scheme, port, fragment));
                        } else if laddr == "0.0.0.0" {
                            return Ok(format!("{}://127.0.0.1:{}{}", scheme, port, fragment));
                        }
                    }
                }
            }
        }
        Ok(url.to_string())
    }

    pub fn update(&self, req: &WebRequest) -> Result<(), ServerError> {
        let valid: Vec<String> = self.fields.borrow().keys().filter(|k| *k != "source" && *k != "uid").cloned().collect();
        for field in req.get_args().keys() {
            if !valid.contains(field) {
                continue;
            }
            let cur = self.get(field);
            let val = match cur {
                Value::Bool(_) => Value::Bool(req.get_boolean(field)?),
                Value::Number(n) if n.is_f64() => crate::utils::float_value(req.get_float(field)?),
                Value::Number(_) => json!(req.get_int(field)?),
                Value::String(_) => json!(req.get_str(field)?),
                _ => req.get(field)?,
            };
            self.fields.borrow_mut().insert(field.clone(), val);
        }
        Ok(())
    }

    pub fn set_default_host(host: &str) {
        DEFAULT_HOST.with(|h| *h.borrow_mut() = host.to_string());
    }

    pub fn from_config(config: &ConfigHelper) -> Result<Self, crate::confighelper::ConfigError> {
        let name = config.get_name().splitn(2, char::is_whitespace).last().unwrap_or("").trim().to_string();
        let ns = uuid::Uuid::parse_str(&server().app_arg_str("instance_uuid")).unwrap_or(uuid::Uuid::nil());
        let uid = uuid::Uuid::new_v5(&ns, format!("moonraker.webcam.{}", name).as_bytes()).to_string();
        Self::build(vec![
            ("name", json!(name)),
            ("enabled", json!(config.getboolean_or("enabled", true)?)),
            ("icon", json!(config.get_or("icon", "mdiWebcam")?)),
            ("aspect_ratio", json!(config.get_or("aspect_ratio", "4:3")?)),
            ("target_fps", json!(config.getint_or("target_fps", 15)?)),
            ("target_fps_idle", json!(config.getint_or("target_fps_idle", 5)?)),
            ("location", json!(config.get_or("location", "printer")?)),
            ("service", json!(config.get_or("service", "mjpegstreamer")?)),
            ("stream_url", json!(config.get("stream_url")?)),
            ("snapshot_url", json!(config.get_or("snapshot_url", "")?)),
            ("flip_horizontal", json!(config.getboolean_or("flip_horizontal", false)?)),
            ("flip_vertical", json!(config.getboolean_or("flip_vertical", false)?)),
            ("rotation", json!(config.getint_or("rotation", 0)?)),
            ("source", json!("config")),
            ("extra_data", json!({})),
            ("uid", json!(uid)),
        ])
        .map_err(|e| ConfigHelper::error(e.message))
    }

    pub fn from_web_request(req: &WebRequest, uid: &str) -> Result<Self, ServerError> {
        Self::build(vec![
            ("name", json!(req.get_str("name")?)),
            ("enabled", json!(req.get_boolean_or("enabled", true)?)),
            ("icon", json!(req.get_str_or("icon", "mdiWebcam"))),
            ("aspect_ratio", json!(req.get_str_or("aspect_ratio", "4:3"))),
            ("target_fps", json!(req.get_int_or("target_fps", 15)?)),
            ("target_fps_idle", json!(req.get_int_or("target_fps_idle", 5)?)),
            ("location", json!(req.get_str_or("location", "printer"))),
            ("service", json!(req.get_str_or("service", "mjpegstreamer"))),
            ("stream_url", json!(req.get_str("stream_url")?)),
            ("snapshot_url", json!(req.get_str_or("snapshot_url", ""))),
            ("flip_horizontal", json!(req.get_boolean_or("flip_horizontal", false)?)),
            ("flip_vertical", json!(req.get_boolean_or("flip_vertical", false)?)),
            ("rotation", json!(req.get_int_or("rotation", 0)?)),
            ("source", json!("database")),
            ("extra_data", req.get_opt("extra_data").unwrap_or(json!({}))),
            ("uid", json!(uid)),
        ])
    }

    pub fn from_database(cam: &Map<String, Value>) -> Result<Self, ServerError> {
        let s = |k: &str, d: &str| cam.get(k).map(py_str).unwrap_or_else(|| d.to_string());
        let b = |k: &str, d: bool| cam.get(k).map(py_truthy).unwrap_or(d);
        let i = |k: &str, d: i64| -> Result<i64, ServerError> {
            match cam.get(k) {
                None => Ok(d),
                Some(v) => value_to_int(v).ok_or_else(|| ServerError::bad(format!("invalid literal for int() with base 10: {}", py_str(v)))),
            }
        };
        let rotation = match cam.get("rotation") {
            Some(_) => i("rotation", 0)?,
            None => i("rotate", 0)?,
        };
        Self::build(vec![
            ("name", json!(py_str(cam.get("name").ok_or_else(|| ServerError::bad("'name'"))?))),
            ("enabled", json!(b("enabled", true))),
            ("icon", json!(s("icon", "mdiWebcam"))),
            ("aspect_ratio", json!(s("aspectRatio", "4:3"))),
            ("target_fps", json!(i("targetFps", 15)?)),
            ("target_fps_idle", json!(i("targetFpsIdle", 5)?)),
            ("location", json!(s("location", "printer"))),
            ("service", json!(s("service", "mjpegstreamer"))),
            ("stream_url", json!(s("urlStream", ""))),
            ("snapshot_url", json!(s("urlSnapshot", ""))),
            ("flip_horizontal", json!(b("flipX", false))),
            ("flip_vertical", json!(b("flipY", false))),
            ("rotation", json!(rotation)),
            ("source", json!("database")),
            ("extra_data", cam.get("extra_data").cloned().unwrap_or(json!({}))),
            ("uid", cam.get("uid").cloned().unwrap_or(Value::Null)),
        ])
    }
}

pub struct WebcamManager {
    webcams: RefCell<IndexMap<String, Rc<WebCam>>>,
}

#[async_trait(?Send)]
impl Component for WebcamManager {
    fn has_init(&self) -> bool {
        true
    }
    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        let srv = server();
        if let Some(m) = srv.get::<crate::components::machine::Machine>() {
            if let Some(ip) = m.public_ip() {
                Self::set_default_host_ip(&ip);
            }
        }
        let mut all_uids: Vec<String> = self.webcams.borrow().values().map(|w| w.uid()).collect();
        let db = srv.database();
        let db_cams = db.get_item_sync("webcams", None, Some(json!({}))).unwrap_or(json!({}));
        let mut ro_info = Vec::new();
        if let Some(cams) = db_cams.as_object() {
            for (uid, data) in cams {
                let mut d = match data.as_object() {
                    Some(d) => d.clone(),
                    None => {
                        log::error!("Failed to process webcam from db");
                        continue;
                    }
                };
                d.insert("uid".into(), json!(uid));
                let webcam = match WebCam::from_database(&d) {
                    Ok(w) => Rc::new(w),
                    Err(e) => {
                        log::error!("Failed to process webcam from db\n{}", e);
                        continue;
                    }
                };
                if all_uids.contains(uid) {
                    let _ = db.delete_item_sync("webcams", &json!(uid));
                    webcam.fields.borrow_mut().insert("uid".into(), json!(self.get_guaranteed_uuid()));
                    self.save_cam(&webcam, false);
                    ro_info.push(format!("Detected webcam UID collision: {}", uid));
                }
                all_uids.push(webcam.uid());
                if self.webcams.borrow().contains_key(&webcam.name()) {
                    ro_info.push(format!(
                        "Detected webcam name collision: {}, uuid: {}.  This camera will be ignored.",
                        webcam.name(),
                        uid
                    ));
                    continue;
                }
                self.webcams.borrow_mut().insert(webcam.name(), webcam);
            }
        }
        if !ro_info.is_empty() {
            srv.add_log_rollover_item("webcam", &ro_info.join("\n"), true);
        }
        Ok(())
    }
}

impl WebcamManager {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, crate::confighelper::ConfigError> {
        let srv = server();
        let mut webcams = IndexMap::new();
        for section in config.get_prefix_sections("webcam ") {
            let cam = WebCam::from_config(&config.getsection(&section))?;
            webcams.insert(cam.name(), Rc::new(cam));
        }
        let wm = Rc::new(WebcamManager { webcams: RefCell::new(webcams) });
        let w = wm.clone();
        srv.register_endpoint("/server/webcams/list", RequestType::GET, callback(move |_r| {
            let w = w.clone();
            async move { Ok(json!({"webcams": w.list_webcams()})) }
        }))?;
        let w = wm.clone();
        srv.register_endpoint("/server/webcams/item", RequestType::all(), callback(move |r| {
            let w = w.clone();
            async move { w.handle_webcam_request(r).await }
        }))?;
        let w = wm.clone();
        srv.register_endpoint("/server/webcams/test", RequestType::POST, callback(move |r| {
            let w = w.clone();
            async move { w.handle_webcam_test(r).await }
        }))?;
        srv.register_notification("webcam:webcams_changed", None);
        srv.on::<crate::events::PublicIpChanged>(|ip| {
            Self::set_default_host_ip(ip);
        });
        Ok(wm)
    }

    fn set_default_host_ip(ip: &str) {
        let mut host = "http://127.0.0.1".to_string();
        if !ip.is_empty() {
            match ip.parse::<std::net::IpAddr>() {
                Ok(std::net::IpAddr::V6(a)) => host = format!("http://[{}]", a),
                Ok(std::net::IpAddr::V4(a)) => host = format!("http://{}", a),
                Err(_) => log::debug!("Invalid IP Recd: {}", ip),
            }
        }
        WebCam::set_default_host(&host);
        log::info!("Default public webcam address set: {}", host);
    }

    pub fn get_webcams(&self) -> IndexMap<String, Rc<WebCam>> {
        self.webcams.borrow().clone()
    }

    fn list_webcams(&self) -> Vec<Value> {
        self.webcams.borrow().values().map(|w| w.as_dict()).collect()
    }

    fn save_cam(&self, cam: &Rc<WebCam>, save_local: bool) {
        if save_local {
            self.webcams.borrow_mut().insert(cam.name(), cam.clone());
        }
        let mut data = Map::new();
        for (mf, df) in CAM_FIELDS {
            data.insert(df.to_string(), cam.get(mf));
        }
        data.insert("location".into(), cam.get("location"));
        data.insert("rotation".into(), cam.get("rotation"));
        data.insert("extra_data".into(), cam.get("extra_data"));
        let _ = server().database().insert_item_sync("webcams", &json!(cam.uid()), Value::Object(data));
    }

    fn get_guaranteed_uuid(&self) -> String {
        let cur: Vec<String> = self.webcams.borrow().values().map(|w| w.uid()).collect();
        loop {
            let u = uuid::Uuid::new_v4().to_string();
            if !cur.contains(&u) {
                return u;
            }
        }
    }

    pub fn get_cam_by_uid(&self, uid: &str) -> Result<Rc<WebCam>, ServerError> {
        self.webcams
            .borrow()
            .values()
            .find(|c| c.uid() == uid)
            .cloned()
            .ok_or_else(|| ServerError::new(format!("Webcam with UID {} not found", uid), 404))
    }

    fn lookup_camera(&self, req: &WebRequest, required: bool) -> Result<Option<Rc<WebCam>>, ServerError> {
        if req.has("uid") {
            return self.get_cam_by_uid(&req.get_str("uid")?).map(Some);
        }
        let name = req.get_str("name")?;
        let cam = self.webcams.borrow().get(&name).cloned();
        if required && cam.is_none() {
            return Err(ServerError::new(format!("Webcam {} not found", name), 404));
        }
        Ok(cam)
    }

    async fn handle_webcam_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let rt = req.get_request_type();
        let webcam = self.lookup_camera(&req, rt != RequestType::POST)?;
        let data;
        if rt == RequestType::GET {
            data = webcam.unwrap().as_dict();
        } else if rt == RequestType::POST {
            let cam = match webcam {
                Some(cam) => {
                    if cam.source() == "config" {
                        return Err(ServerError::bad(format!(
                            "Cannot overwrite webcam '{}' sourced from Moonraker configuration",
                            cam.name()
                        )));
                    }
                    if let Some(new_name) = req.get_str_opt("name") {
                        if cam.name() != new_name {
                            if self.webcams.borrow().contains_key(&new_name) {
                                return Err(ServerError::bad(format!(
                                    "Cannot rename webcam from '{}' to '{}'.  Webcam with requested name '{}' already exists.",
                                    cam.name(),
                                    new_name,
                                    new_name
                                )));
                            }
                            self.webcams.borrow_mut().shift_remove(&cam.name());
                        }
                    }
                    cam.update(&req)?;
                    cam
                }
                None => {
                    let uid = self.get_guaranteed_uuid();
                    Rc::new(WebCam::from_web_request(&req, &uid)?)
                }
            };
            self.save_cam(&cam, true);
            data = cam.as_dict();
        } else {
            let cam = webcam.unwrap();
            if cam.source() == "config" {
                return Err(ServerError::bad(format!("Cannot delete webcam '{}' sourced from Moonraker configuration", cam.name())));
            }
            data = cam.as_dict();
            self.webcams.borrow_mut().shift_remove(&cam.name());
            let _ = server().database().delete_item_sync("webcams", &json!(cam.uid()));
        }
        if rt != RequestType::GET {
            server().send_event("webcam:webcams_changed", vec![json!({"webcams": self.list_webcams()})]);
        }
        Ok(json!({"webcam": data}))
    }

    async fn handle_webcam_test(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let cam = self.lookup_camera(&req, true)?.unwrap();
        let mut result = json!({"name": cam.name(), "snapshot_reachable": false});
        match cam.get_snapshot_url(true).await {
            Ok(u) => result["snapshot_url"] = json!(u),
            Err(e) => {
                log::error!("Error Processing snapshot url\n{}", e);
                result["snapshot_url"] = json!("");
            }
        }
        match cam.get_stream_url(true).await {
            Ok(u) => result["stream_url"] = json!(u),
            Err(e) => {
                log::error!("Error Processing stream url\n{}", e);
                result["stream_url"] = json!("");
            }
        }
        let url = py_str(&result["snapshot_url"]);
        if url.starts_with("http") {
            if let Some(client) = server().get::<crate::components::http_client::HttpClient>() {
                let ret = client
                    .get_opts(
                        &url,
                        &[],
                        crate::components::http_client::RequestOpts {
                            connect_timeout: 1.0,
                            request_timeout: 1.0,
                            enable_cache: true,
                            ..Default::default()
                        },
                    )
                    .await;
                result["snapshot_reachable"] = json!(!ret.has_error());
            }
        }
        Ok(result)
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<WebcamManager>, crate::confighelper::ConfigError> {
    WebcamManager::new(config)
}

#[cfg(test)]
mod tests {
    #[test]
    fn expand_url() {
        let e = super::WebCam::expand_url;
        assert_eq!(e(":8080/stream"), "http://127.0.0.1:8080/stream");
        assert_eq!(e(":8080"), "http://127.0.0.1:8080");
        assert_eq!(e("/webcam/?action=stream"), "/webcam/?action=stream");
        assert_eq!(e("http://cam:8080/stream"), "http://cam:8080/stream");
    }
}

// Integration with Spoolman (port of moonraker/components/spoolman.py)

use crate::common::*;
use crate::components::history::{History, HistoryFieldData};
use crate::components::http_client::{Body, HttpClient, HttpResponse, RequestOpts};
use crate::confighelper::{Bounds, ConfigError, ConfigHelper};
use crate::server::{server, spawn, Component};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

const DB_NAMESPACE: &str = "moonraker";
const ACTIVE_SPOOL_KEY: &str = "spoolman.spool_id";

fn ws_error_string(e: &tokio_tungstenite::tungstenite::Error) -> String {
    use tokio_tungstenite::tungstenite::Error as E;
    match e {
        E::Io(io) => match io.raw_os_error() {
            Some(code) => {
                let msg = std::io::Error::from_raw_os_error(code).to_string();
                format!("[Errno {}] {}", code, msg.split(" (os error").next().unwrap_or(&msg))
            }
            None => io.to_string(),
        },
        E::Http(resp) => format!("HTTP {}: {}", resp.status().as_u16(), resp.status().canonical_reason().unwrap_or("Unknown")),
        other => other.to_string(),
    }
}

pub struct SpoolManager {
    pub spoolman_url: String,
    ws_url: String,
    sync_rate_seconds: i64,
    pending_reports: RefCell<IndexMap<i64, f64>>,
    ws_sender: RefCell<Option<tokio::sync::mpsc::UnboundedSender<Message>>>,
    connection_task: crate::server::TaskSlot,
    spool_check_task: crate::server::TaskSlot,
    ws_connected: Cell<bool>,
    is_closing: Cell<bool>,
    spool_id: Rc<Cell<Option<i64>>>,
    error_logged: Cell<bool>,
    highest_epos: Cell<f64>,
    last_epos: Cell<f64>,
    current_extruder: RefCell<String>,
    spool_history: Rc<HistoryFieldData>,
    timer_gen: Cell<u64>,
    http: Rc<HttpClient>,
}

impl SpoolManager {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, ConfigError> {
        let srv = server();
        let orig_url = config.get("server")?;
        let re = regex::Regex::new(r"^(?i:(?P<scheme>https?)://)?(?P<host>.+)").unwrap();
        let caps = re
            .captures(&orig_url)
            .ok_or_else(|| ConfigHelper::error(format!("Section [spoolman], Option server: {}: Invalid URL format", orig_url)))?;
        let scheme = caps.name("scheme").map(|m| m.as_str().to_string()).unwrap_or("http".into());
        let host = caps["host"].trim_end_matches('/').to_string();
        let ws_scheme = if scheme == "https" { "wss" } else { "ws" };
        let sync_rate_seconds = config.getint_b("sync_rate", Some(5), Bounds::min(1.0))?;
        let spool_id: Rc<Cell<Option<i64>>> = Rc::new(Cell::new(None));
        let sid = spool_id.clone();
        let spool_history = HistoryFieldData::new(
            "spool_ids",
            "spoolman",
            "Spool IDs used",
            "collect",
            None,
            Some(Rc::new(move || match sid.get() {
                None => json!([]),
                Some(id) => json!([id]),
            })),
            false,
            false,
            false,
            None,
        )
        .map_err(|e| ConfigHelper::error(e.message))?;
        let history: Rc<History> =
            srv.get::<crate::components::history::History>().ok_or_else(|| ConfigHelper::error("Component (history) not found"))?;
        history.register_auxiliary_field(spool_history.clone()).map_err(|e| ConfigHelper::error(e.message))?;
        let http: Rc<HttpClient> =
            srv.get::<crate::components::http_client::HttpClient>().ok_or_else(|| ConfigHelper::error("Component (http_client) not found"))?;
        let ann: Rc<crate::components::announcements::Announcements> = srv
            .get::<crate::components::announcements::Announcements>()
            .ok_or_else(|| ConfigHelper::error("Component (announcements) not found"))?;
        ann.register_feed("spoolman");
        let sm = Rc::new(SpoolManager {
            spoolman_url: format!("{}://{}/api", scheme, host),
            ws_url: format!("{}://{}/api/v1/spool", ws_scheme, host),
            sync_rate_seconds,
            pending_reports: RefCell::new(IndexMap::new()),
            ws_sender: RefCell::new(None),
            connection_task: Default::default(),
            spool_check_task: Default::default(),
            ws_connected: Cell::new(false),
            is_closing: Cell::new(false),
            spool_id,
            error_logged: Cell::new(false),
            highest_epos: Cell::new(0.0),
            last_epos: Cell::new(0.0),
            current_extruder: RefCell::new("extruder".into()),
            spool_history,
            timer_gen: Cell::new(0),
            http,
        });
        srv.register_notification("spoolman:active_spool_set", None);
        srv.register_notification("spoolman:spoolman_status_changed", None);
        let s = sm.clone();
        srv.on_async::<crate::events::KlippyReady, _, _>(move |_| {
            let s = s.clone();
            async move {
                if let Err(e) = s.handle_klippy_ready().await {
                    log::error!("{}", e.message);
                }
            }
        });
        let s = sm.clone();
        let _ = srv.register_endpoint(
            "/server/spoolman/spool_id",
            RequestType::GET | RequestType::POST,
            callback(move |req| {
                let s = s.clone();
                async move { s.handle_spool_id_request(req) }
            }),
        );
        let s = sm.clone();
        let _ = srv.register_endpoint(
            "/server/spoolman/proxy",
            RequestType::POST,
            callback(move |req| {
                let s = s.clone();
                async move { s.proxy_spoolman_request(req).await }
            }),
        );
        let s = sm.clone();
        let _ = srv.register_endpoint(
            "/server/spoolman/status",
            RequestType::GET,
            callback(move |_req| {
                let s = s.clone();
                async move { Ok(s.status()) }
            }),
        );
        let s = Rc::downgrade(&sm);
        let _ = srv.register_remote_method(
            "spoolman_set_active_spool",
            crate::components::klippy_connection::remote_method(move |args| {
                if let Some(s) = s.upgrade() {
                    match args.get("spool_id") {
                        None | Some(Value::Null) => s.set_active_spool(None),
                        Some(v) if v.is_i64() => s.set_active_spool(v.as_i64()),
                        Some(_) => log::error!("AssertionError"),
                    }
                }
            }),
        );
        Ok(sm)
    }

    async fn connect_websocket(self: Rc<Self>) {
        let mut log_connect = true;
        let mut err_list: Vec<String> = Vec::new();
        let srv = server();
        while !self.is_closing.get() {
            if log_connect {
                log::info!("Connecting To Spoolman: {}", self.ws_url);
                log_connect = false;
            }
            let res = tokio::time::timeout(Duration::from_secs(5), tokio_tungstenite::connect_async(self.ws_url.as_str())).await;
            let res = match res {
                Err(_) => Err("Timeout while connecting".to_string()),
                Ok(Err(e)) => Err(ws_error_string(&e)),
                Ok(Ok((ws, _))) => Ok(ws),
            };
            match res {
                Err(e) => {
                    if err_list.len() < 10 && !err_list.contains(&e) {
                        err_list.push(e.clone());
                        let verbose = srv.is_verbose_enabled();
                        if verbose {
                            log::error!("Failed to connect to Spoolman\n{}", e);
                        }
                        srv.add_log_rollover_item("spoolman_connect", &format!("Failed to Connect to spoolman: {}", e), !verbose);
                    }
                }
                Ok(ws) => {
                    err_list.clear();
                    self.ws_connected.set(true);
                    self.error_logged.set(false);
                    self.clone().start_report_timer();
                    srv.add_log_rollover_item("spoolman_connect", "Connected to Spoolman Spool Manager", true);
                    if self.spool_id.get().is_some() {
                        self.cancel_spool_check_task();
                        let h = spawn(self.clone().check_spool_deleted());
                        self.spool_check_task.set(h);
                    }
                    self.send_status_notification();
                    self.clone().read_messages(ws).await;
                    log_connect = true;
                }
            }
            if !self.is_closing.get() {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }

    async fn read_messages(
        self: Rc<Self>,
        ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    ) {
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
        *self.ws_sender.borrow_mut() = Some(tx);
        let mut last_ping = tokio::time::Instant::now();
        let mut ping_iv = tokio::time::interval(Duration::from_secs(20));
        ping_iv.tick().await;
        let (code, reason) = loop {
            tokio::select! {
                msg = stream.next() => match msg {
                    Some(Ok(Message::Text(t))) => self.decode_message(t.as_str()),
                    Some(Ok(Message::Ping(_))) => last_ping = tokio::time::Instant::now(),
                    Some(Ok(Message::Close(f))) => {
                        break match f {
                            Some(f) => (Some(u16::from(f.code)), Some(f.reason.to_string())),
                            None => (None, None),
                        };
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break (None, None),
                },
                out = rx.recv() => {
                    if let Some(m) = out {
                        let _ = sink.send(m).await;
                    }
                }
                _ = ping_iv.tick() => {
                    let _ = sink.send(Message::Ping(Default::default())).await;
                }
            }
        };
        *self.ws_sender.borrow_mut() = None;
        self.timer_gen.set(self.timer_gen.get() + 1);
        self.ws_connected.set(false);
        let ping_time = last_ping.elapsed().as_secs_f64();
        let fmt = |v: Option<String>| v.unwrap_or("None".into());
        log::info!(
            "Spoolman Disconnected - Code: {}, Reason: {}, Server Ping Time Elapsed: {}",
            fmt(code.map(|c| c.to_string())),
            fmt(reason),
            crate::pyfmt::py_float_repr(ping_time)
        );
        if !self.is_closing.get() {
            self.send_status_notification();
        }
    }

    fn decode_message(&self, message: &str) {
        let event: Value = match serde_json::from_str(message) {
            Ok(v) => v,
            Err(e) => {
                log::error!("{}", e);
                return;
            }
        };
        if event.get("resource").and_then(|v| v.as_str()) != Some("spool") {
            return;
        }
        if let Some(sid) = self.spool_id.get() {
            if event.get("type").and_then(|v| v.as_str()) == Some("deleted")
                && event.get("payload").and_then(|p| p.get("id")).and_then(|v| v.as_i64()) == Some(sid)
            {
                self.pending_reports.borrow_mut().shift_remove(&sid);
                self.set_active_spool(None);
            }
        }
    }

    fn cancel_spool_check_task(&self) {
        self.spool_check_task.cancel();
    }

    async fn check_spool_deleted(self: Rc<Self>) {
        if let Some(sid) = self.spool_id.get() {
            let opts = RequestOpts { connect_timeout: 1.0, request_timeout: 2.0, enable_cache: true, ..Default::default() };
            let response = self.http.get_opts(&format!("{}/v1/spool/{}", self.spoolman_url, sid), &[], opts).await;
            if response.status_code == 404 {
                log::info!("Spool ID {} not found, setting to None", sid);
                self.pending_reports.borrow_mut().shift_remove(&sid);
                self.set_active_spool(None);
            } else if response.has_error() {
                log::info!("Attempt to check spool status failed: {}", Self::get_response_error(&response));
            } else {
                log::info!("Found Spool ID {} on spoolman instance", sid);
            }
        }
        self.spool_check_task.clear();
    }

    async fn handle_klippy_ready(self: Rc<Self>) -> Result<(), ServerError> {
        let s = Rc::downgrade(&self);
        let cb: crate::components::klippy_apis::SubCallback = Rc::new(move |status, _| {
            if let Some(s) = s.upgrade() {
                s.handle_status_update(status);
            }
            None
        });
        let mut objs = Map::new();
        objs.insert("toolhead".into(), json!(["position", "extruder"]));
        let result = server().klippy_apis().subscribe_objects(&objs, Some(cb)).await.unwrap_or_default();
        let toolhead = result.get("toolhead").cloned().unwrap_or(json!({}));
        *self.current_extruder.borrow_mut() =
            toolhead.get("extruder").and_then(|v| v.as_str()).unwrap_or("extruder").to_string();
        let initial = toolhead.get("position").and_then(|p| p.get(3)).and_then(|v| v.as_f64());
        log::debug!("Initial epos: {}", initial.map(crate::pyfmt::py_float_repr).unwrap_or("None".into()));
        match initial {
            Some(e) => {
                self.highest_epos.set(e);
                Ok(())
            }
            None => {
                log::error!("Spoolman integration unable to subscribe to epos");
                Err(ServerError::bad("Unable to subscribe to e position"))
            }
        }
    }

    fn get_response_error(response: &HttpResponse) -> String {
        let mut err_msg = format!("HTTP error: {} {}", response.status_code, response.error_string());
        if let Ok(Value::Object(m)) = response.json() {
            if let Some(msg) = m.get("message") {
                err_msg.push_str(&format!(", Spoolman message: {}", crate::pyfmt::py_str(msg)));
            }
        }
        err_msg
    }

    fn handle_status_update(&self, status: &Map<String, Value>) {
        let toolhead = match status.get("toolhead") {
            Some(t) => t,
            None => return,
        };
        let epos = toolhead.get("position").and_then(|p| p.get(3)).and_then(|v| v.as_f64()).unwrap_or(self.highest_epos.get());
        self.last_epos.set(epos);
        let cur = self.current_extruder.borrow().clone();
        let extr = toolhead.get("extruder").and_then(|v| v.as_str()).map(String::from).unwrap_or(cur.clone());
        if extr != cur {
            self.highest_epos.set(epos);
            *self.current_extruder.borrow_mut() = extr;
        } else if epos > self.highest_epos.get() {
            if let Some(sid) = self.spool_id.get() {
                self.add_extrusion(sid, epos - self.highest_epos.get());
            }
            self.highest_epos.set(epos);
        }
    }

    fn add_extrusion(&self, spool_id: i64, used_length: f64) {
        *self.pending_reports.borrow_mut().entry(spool_id).or_insert(0.0) += used_length;
    }

    pub fn connected(&self) -> bool {
        self.ws_connected.get()
    }

    pub fn set_active_spool(&self, spool_id: Option<i64>) {
        if self.spool_id.get() == spool_id {
            log::info!("Spool ID already set to: {}", spool_id.map(|i| i.to_string()).unwrap_or("None".into()));
            return;
        }
        self.spool_history.update(json!(spool_id));
        self.spool_id.set(spool_id);
        let _ = server().database().insert_item_sync(DB_NAMESPACE, &json!(ACTIVE_SPOOL_KEY), json!(spool_id));
        self.highest_epos.set(self.last_epos.get());
        server().send_event("spoolman:active_spool_set", vec![json!({"spool_id": spool_id})]);
        log::info!("Setting active spool to: {}", spool_id.map(|i| i.to_string()).unwrap_or("None".into()));
    }

    fn start_report_timer(self: Rc<Self>) {
        let gen = self.timer_gen.get() + 1;
        self.timer_gen.set(gen);
        let s = Rc::downgrade(&self);
        spawn(async move {
            loop {
                let s = match s.upgrade() {
                    Some(s) if s.timer_gen.get() == gen => s,
                    _ => break,
                };
                s.report_extrusion().await;
                let rate = s.sync_rate_seconds as f64;
                drop(s);
                tokio::time::sleep(Duration::from_secs_f64(rate)).await;
            }
        });
    }

    pub fn sync_rate_seconds(&self) -> i64 {
        self.sync_rate_seconds
    }

    pub async fn report_extrusion(&self) {
        if !self.ws_connected.get() {
            return;
        }
        let pending = std::mem::take(&mut *self.pending_reports.borrow_mut());
        for (spool_id, used_length) in pending {
            if !self.ws_connected.get() {
                self.add_extrusion(spool_id, used_length);
                continue;
            }
            log::debug!("Sending spool usage: ID: {}, Length: {:.3}mm", spool_id, used_length);
            let response = self
                .http
                .request(
                    "PUT",
                    &format!("{}/v1/spool/{}/use", self.spoolman_url, spool_id),
                    Body::Json(json!({"use_length": used_length})),
                    &[],
                    RequestOpts::default(),
                )
                .await;
            if response.has_error() {
                if response.status_code == 404 {
                    self.pending_reports.borrow_mut().shift_remove(&spool_id);
                    if Some(spool_id) == self.spool_id.get() {
                        log::info!("Spool ID {} not found, setting to None", spool_id);
                        self.set_active_spool(None);
                    }
                } else {
                    if !self.error_logged.get() {
                        self.error_logged.set(true);
                        log::info!(
                            "Failed to update extrusion for spool id {}, received {}",
                            spool_id,
                            Self::get_response_error(&response)
                        );
                    }
                    self.add_extrusion(spool_id, used_length);
                    continue;
                }
            }
            self.error_logged.set(false);
        }
    }

    fn handle_spool_id_request(&self, req: WebRequest) -> ApiResult {
        if req.get_request_type() == RequestType::POST {
            let spool_id = req.get_int_opt("spool_id")?;
            self.set_active_spool(spool_id);
        }
        Ok(json!({"spool_id": self.spool_id.get()}))
    }

    async fn proxy_spoolman_request(&self, req: WebRequest) -> ApiResult {
        let method = req.get_str("request_method")?;
        let path = req.get_str("path")?;
        let query = req.get_str_opt("query");
        let body = req.get_opt("body").unwrap_or(Value::Null);
        let use_v2_response = req.get_boolean_or("use_v2_response", false)?;
        if !["GET", "POST", "PUT", "PATCH", "DELETE"].contains(&method.as_str()) {
            return Err(ServerError::bad(format!("Invalid HTTP method: {}", method)));
        }
        if !body.is_null() && method == "GET" {
            return Err(ServerError::bad("GET requests cannot have a body"));
        }
        if path.len() < 4 || &path[..4] != "/v1/" {
            return Err(ServerError::bad("Invalid path, must start with the API version, e.g. /v1"));
        }
        let query = query.map(|q| format!("?{}", q)).unwrap_or_default();
        let full_url = format!("{}{}{}", self.spoolman_url, path, query);
        if !self.ws_connected.get() {
            if !use_v2_response {
                return Err(ServerError::new("Spoolman server not available", 503));
            }
            return Ok(json!({
                "response": null,
                "error": {"status_code": 503, "message": "Spoolman server not available"}
            }));
        }
        log::debug!("Proxying {} request to {}", method, full_url);
        let body = match body {
            Value::Null => Body::None,
            Value::String(s) => Body::Text(s),
            v => Body::Json(v),
        };
        let response = self.http.request(&method, &full_url, body, &[], RequestOpts::default()).await;
        if !use_v2_response {
            response.raise_for_status(None)?;
            return response.json();
        }
        if response.has_error() {
            let mut msg = response.error_string();
            if let Ok(Value::Object(m)) = response.json() {
                if let Some(v) = m.get("message") {
                    msg = crate::pyfmt::py_str(v);
                }
            }
            return Ok(json!({"response": null, "error": {"status_code": response.status_code, "message": msg}}));
        }
        let headers: Map<String, Value> = response.headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        Ok(json!({"response": response.json()?, "response_headers": headers, "error": null}))
    }

    fn status(&self) -> Value {
        let pending: Vec<Value> =
            self.pending_reports.borrow().iter().map(|(sid, used)| json!({"spool_id": sid, "filament_used": used})).collect();
        json!({
            "spoolman_connected": self.ws_connected.get(),
            "pending_reports": pending,
            "spool_id": self.spool_id.get(),
        })
    }

    fn send_status_notification(&self) {
        server().send_event("spoolman:spoolman_status_changed", vec![json!({"spoolman_connected": self.ws_connected.get()})]);
    }
}

#[async_trait(?Send)]
impl Component for SpoolManager {
    fn has_init(&self) -> bool {
        true
    }

    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        let sid = server().database().get_item_sync(DB_NAMESPACE, Some(&json!(ACTIVE_SPOOL_KEY)), Some(Value::Null))?;
        self.spool_id.set(sid.as_i64());
        let h = spawn(self.clone().connect_websocket());
        self.connection_task.set(h);
        Ok(())
    }

    async fn close(self: Rc<Self>) {
        self.is_closing.set(true);
        self.timer_gen.set(self.timer_gen.get() + 1);
        let sender = self.ws_sender.borrow().clone();
        if let Some(tx) = sender {
            let _ = tx.send(Message::Close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: 1001.into(),
                reason: "Moonraker Shutdown".into(),
            })));
        }
        self.cancel_spool_check_task();
        let task = self.connection_task.take();
        if let Some(t) = task {
            if !t.is_finished() {
                let abort = t.abort_handle();
                if tokio::time::timeout(Duration::from_secs(2), t).await.is_err() {
                    abort.abort();
                }
            }
        }
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<SpoolManager>, ConfigError> {
    SpoolManager::new(config)
}

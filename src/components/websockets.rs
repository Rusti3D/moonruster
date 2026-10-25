// Websocket Request/Response Handler (port of moonraker/components/websockets.py)

use crate::common::*;
use crate::components::application::{error_response, full_body, Body, HttpError, HttpRequest, HttpResponse};
use crate::confighelper::ConfigHelper;
use crate::server::{server, spawn, Component};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::{Request, Response};
use hyper_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use hyper_tungstenite::tungstenite::protocol::CloseFrame;
use hyper_tungstenite::tungstenite::{Message, Utf8Bytes};
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::net::IpAddr;
use std::rc::Rc;
use tokio::sync::{mpsc, oneshot};

pub const CLIENT_TYPES: [&str; 7] = ["web", "mobile", "desktop", "display", "bot", "agent", "other"];

thread_local! {
    static CONNECTION_COUNT: Cell<i64> = const { Cell::new(0) };
    static NEXT_UID: Cell<u64> = const { Cell::new(0) };
}

/// Generates unique connection ids.  Python uses id(obj) which yields
/// large integers, we emulate the magnitude to keep client expectations.
pub fn next_uid() -> u64 {
    NEXT_UID.with(|n| {
        let v = if n.get() == 0 {
            // pseudo random start in the range of typical python ids
            140_000_000_000_000u64 + (rand::random::<u32>() as u64) * 16
        } else {
            n.get() + 16
        };
        n.set(v);
        v
    })
}

pub enum OutMsg {
    /// shared, not copied, when one message goes to many clients
    Text(Utf8Bytes),
    Close(u16, String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConnKind {
    WebSocket,
    UnixSocket,
    Bridge,
}

/// BaseRemoteConnection: shared by websockets and unix socket connections
pub struct RemoteConnection {
    uid: u64,
    pub kind: ConnKind,
    sender: mpsc::UnboundedSender<OutMsg>,
    pub is_closed: Cell<bool>,
    client_data: RefCell<IndexMap<String, String>>,
    identified: Cell<bool>,
    user_info: RefCell<Option<UserInfo>>,
    need_auth: Cell<bool>,
    pending_responses: RefCell<HashMap<i64, oneshot::Sender<Result<Value, ServerError>>>>,
    next_req_id: Cell<i64>,
    ip: Option<IpAddr>,
    hostname: String,
    connected_time: Cell<f64>,
    pub last_pong_time: Cell<f64>,
    pub cors_allowed: Cell<bool>,
    pub close_code: RefCell<Option<u16>>,
    pub close_reason: RefCell<Option<String>>,
}

impl RemoteConnection {
    pub fn new(kind: ConnKind, sender: mpsc::UnboundedSender<OutMsg>, ip: Option<IpAddr>, hostname: String) -> Rc<Self> {
        let mut cd = IndexMap::new();
        cd.insert("name".into(), "unknown".into());
        cd.insert("version".into(), String::new());
        cd.insert("type".into(), String::new());
        cd.insert("url".into(), String::new());
        Rc::new(RemoteConnection {
            uid: next_uid(),
            kind,
            sender,
            is_closed: Cell::new(false),
            client_data: RefCell::new(cd),
            identified: Cell::new(false),
            user_info: RefCell::new(None),
            need_auth: Cell::new(false),
            pending_responses: RefCell::new(HashMap::new()),
            next_req_id: Cell::new(next_uid() as i64),
            ip,
            hostname,
            connected_time: Cell::new(0.0),
            last_pong_time: Cell::new(crate::utils::monotonic()),
            cors_allowed: Cell::new(false),
            close_code: RefCell::new(None),
            close_reason: RefCell::new(None),
        })
    }

    pub fn connected_time(&self) -> f64 {
        self.connected_time.get()
    }

    pub fn ip(&self) -> Option<IpAddr> {
        self.ip
    }

    pub fn uid(&self) -> u64 {
        self.uid
    }
    pub fn hostname(&self) -> String {
        self.hostname.clone()
    }
    pub fn set_connected_time(&self, t: f64) {
        self.connected_time.set(t);
    }
    pub fn identified(&self) -> bool {
        self.identified.get()
    }
    pub fn client_data(&self) -> IndexMap<String, String> {
        self.client_data.borrow().clone()
    }
    pub fn client_data_value(&self) -> Value {
        Value::Object(self.client_data.borrow().iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect())
    }
    pub fn set_client_data(&self, data: IndexMap<String, String>) {
        *self.client_data.borrow_mut() = data;
        self.identified.set(true);
    }
    pub fn set_client_type(&self, t: &str) {
        self.client_data.borrow_mut().insert("type".into(), t.into());
    }
    pub fn need_auth(&self) -> bool {
        self.need_auth.get()
    }
    pub fn set_need_auth(&self, v: bool) {
        self.need_auth.set(v);
    }
    pub fn get_user_info(&self) -> Option<UserInfo> {
        self.user_info.borrow().clone()
    }
    pub fn set_user_info(&self, u: UserInfo) {
        *self.user_info.borrow_mut() = Some(u);
        self.need_auth.set(false);
    }

    pub fn queue_message_str(&self, msg: impl Into<Utf8Bytes>) {
        if self.is_closed.get() {
            return;
        }
        let _ = self.sender.send(OutMsg::Text(msg.into()));
    }

    pub fn queue_message(&self, msg: &Value) {
        self.queue_message_str(msg.to_string());
    }

    pub fn authenticate(&self, token: Option<String>, api_key: Option<String>) -> Result<(), ServerError> {
        let auth = server().get::<crate::components::authorization::Authorization>();
        let mut uinfo = None;
        if let Some(a) = auth {
            if let Some(t) = token {
                uinfo = Some(a.validate_jwt(&t)?);
            } else if let Some(k) = api_key {
                uinfo = a.validate_api_key(&k)?;
            }
        }
        match uinfo {
            Some(u) => {
                self.set_user_info(u);
                Ok(())
            }
            None if self.need_auth.get() => Err(ServerError::new("Unauthorized", 401)),
            None => Ok(()),
        }
    }

    pub fn check_authenticated(&self, api_def: &ApiDefinition) -> Result<(), ServerError> {
        if !self.need_auth.get() {
            return Ok(());
        }
        if !server().has_component("authorization") {
            return Ok(());
        }
        if api_def.auth_required {
            return Err(ServerError::new("Unauthorized", 401));
        }
        Ok(())
    }

    pub fn on_user_logout(&self, user: &str) -> bool {
        let matched = match &*self.user_info.borrow() {
            None => return false,
            Some(u) => u.username == user,
        };
        if matched {
            *self.user_info.borrow_mut() = None;
            if self.kind == ConnKind::WebSocket {
                self.need_auth.set(true);
            }
            return true;
        }
        false
    }

    pub fn send_status_update(&self, status: &serde_json::Map<String, Value>, eventtime: f64) {
        if status.is_empty() {
            return;
        }
        // serialized directly, the hottest message during a print
        let msg = format!(
            r#"{{"jsonrpc":"2.0","method":"notify_status_update","params":[{},{}]}}"#,
            serde_json::to_string(status).unwrap_or_default(),
            serde_json::to_string(&eventtime).unwrap_or_default()
        );
        self.queue_message_str(msg);
    }

    pub async fn call_method_with_response(&self, method: &str, params: Option<Value>) -> Result<Value, ServerError> {
        let id = self.next_req_id.get();
        self.next_req_id.set(id + 1);
        let (tx, rx) = oneshot::channel();
        let mut msg = json!({"jsonrpc": "2.0", "method": method, "id": id});
        if let Some(p) = params {
            if !is_empty_params(&p) {
                msg["params"] = p;
            }
        }
        self.pending_responses.borrow_mut().insert(id, tx);
        self.queue_message(&msg);
        match rx.await {
            Ok(r) => r,
            Err(_) => Err(ServerError::new("Client Socket Disconnected", 500)),
        }
    }

    pub fn call_method(&self, method: &str, params: Option<Value>) {
        let mut msg = json!({"jsonrpc": "2.0", "method": method});
        if let Some(p) = params {
            if !is_empty_params(&p) {
                msg["params"] = p;
            }
        }
        self.queue_message(&msg);
    }

    pub fn send_notification(&self, name: &str, data: Vec<Value>) {
        server().wsm().notify_clients(name, &data, &[self.uid]);
    }

    pub fn resolve_pending_response(&self, response_id: &Value, result: Result<Value, ServerError>) -> bool {
        let id = match response_id.as_i64() {
            Some(i) => i,
            None => return false,
        };
        match self.pending_responses.borrow_mut().remove(&id) {
            None => false,
            Some(tx) => {
                let _ = tx.send(result);
                true
            }
        }
    }

    pub fn fail_pending(&self) {
        for (_, tx) in self.pending_responses.borrow_mut().drain() {
            let _ = tx.send(Err(ServerError::new("Client Socket Disconnected", 500)));
        }
    }

    pub fn close_socket(&self, code: u16, reason: &str) {
        let _ = self.sender.send(OutMsg::Close(code, reason.to_string()));
    }

    pub async fn process_message(self: Rc<Self>, message: Vec<u8>) {
        let rpc = server().app().json_rpc.clone();
        let transport: Rc<dyn ApiTransport> = self.clone();
        if let Some(resp) = rpc.dispatch(&message, transport).await {
            self.queue_message_str(resp);
        }
    }
}

/// The JSON-RPC notification notify_<name> with data as its params, as
/// json!({"jsonrpc", "method", "params"}) encodes it, without building it
fn notification(name: &str, data: &[Value]) -> String {
    let method = serde_json::to_string(&format!("notify_{}", name)).unwrap_or_default();
    if data.is_empty() {
        return format!(r#"{{"jsonrpc":"2.0","method":{}}}"#, method);
    }
    let params = serde_json::to_string(data).unwrap_or_default();
    format!(r#"{{"jsonrpc":"2.0","method":{},"params":{}}}"#, method, params)
}

fn is_empty_params(p: &Value) -> bool {
    match p {
        Value::Null => true,
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

impl ApiTransport for RemoteConnection {
    fn transport_type(&self) -> TransportType {
        TransportType::WEBSOCKET
    }
    fn py_object(self: Rc<Self>) -> Option<(Rc<dyn std::any::Any>, &'static str, &'static str)> {
        match self.kind {
            ConnKind::WebSocket => Some((self, "moonraker.components.websockets", "WebSocket")),
            ConnKind::UnixSocket => Some((self, "moonraker.components.extensions", "UnixSocketClient")),
            // Moonraker's BridgeSocket is not an APITransport
            ConnKind::Bridge => None,
        }
    }
    fn user_info(&self) -> Option<UserInfo> {
        self.get_user_info()
    }
    fn ip_addr(&self) -> Option<IpAddr> {
        self.ip
    }
    fn screen_rpc_request(&self, api_def: &ApiDefinition, _rt: RequestType, _args: &mut Args) -> Result<(), ServerError> {
        self.check_authenticated(api_def)
    }
    fn send_status(&self, status: &serde_json::Map<String, Value>, eventtime: f64) {
        self.send_status_update(status, eventtime);
    }
    fn transport_id(&self) -> u64 {
        self.uid
    }
    fn as_remote(self: Rc<Self>) -> Option<Rc<RemoteConnection>> {
        Some(self)
    }
}

// ***** Websocket Manager *****

pub struct WebsocketManager {
    clients: RefCell<IndexMap<u64, Rc<RemoteConnection>>>,
    bridge_connections: RefCell<IndexMap<u64, Rc<RemoteConnection>>>,
    closed_notify: RefCell<Option<Rc<tokio::sync::Notify>>>,
}

#[async_trait(?Send)]
impl Component for WebsocketManager {}

impl WebsocketManager {
    pub fn new(_config: &ConfigHelper) -> Rc<Self> {
        let wsm = Rc::new(WebsocketManager {
            clients: RefCell::new(IndexMap::new()),
            bridge_connections: RefCell::new(IndexMap::new()),
            closed_notify: RefCell::new(None),
        });
        let srv = server();
        let app = srv.app();
        app.register_websocket_handler("/websocket", false);
        app.register_websocket_handler("/klippysocket", true);
        let _ = app.register_endpoint(
            "/server/websocket/id",
            RequestType::GET,
            callback(|req: WebRequest| async move {
                let sc = req.get_client_connection().ok_or_else(|| ServerError::new("No connection", 500))?;
                Ok(json!({"websocket_id": sc.uid()}))
            }),
            TransportType::WEBSOCKET,
            true,
            None,
            true,
            false,
        );
        let _ = app.register_endpoint(
            "/server/connection/identify",
            RequestType::POST,
            callback(|req: WebRequest| async move { handle_identify(req).await }),
            TransportType::WEBSOCKET,
            true,
            None,
            false,
            false,
        );
        wsm
    }

    pub fn register_notification(self: &Rc<Self>, event_name: &str, notify_name: Option<&str>, event_type: Option<&str>) {
        let notify_name = notify_name
            .map(String::from)
            .unwrap_or_else(|| event_name.rsplit(':').next().unwrap_or(event_name).to_string());
        let is_logout = event_type == Some("logout");
        let wsm = Rc::downgrade(self);
        server().on_values(event_name, move |args| {
            if let Some(w) = wsm.upgrade() {
                w.notify_clients(&notify_name, args, &[]);
                if is_logout {
                    if let Some(user) = args.first() {
                        w.process_logout(user);
                    }
                }
            }
        });
    }

    fn process_logout(&self, user: &Value) {
        let name = match user.get("username").and_then(|v| v.as_str()) {
            Some(n) => n.to_string(),
            None => return,
        };
        for sc in self.clients.borrow().values() {
            sc.on_user_logout(&name);
        }
    }

    pub fn get_client(&self, uid: u64) -> Option<Rc<RemoteConnection>> {
        self.clients.borrow().get(&uid).cloned()
    }

    pub fn has_socket(&self, ws_id: u64) -> bool {
        self.clients.borrow().contains_key(&ws_id)
    }

    pub fn get_clients_by_name(&self, name: &str) -> Vec<Rc<RemoteConnection>> {
        if name.is_empty() {
            return Vec::new();
        }
        let name = name.to_lowercase();
        self.clients
            .borrow()
            .values()
            .filter(|sc| sc.client_data.borrow().get("name").map(|n| n.to_lowercase() == name).unwrap_or(false))
            .cloned()
            .collect()
    }

    pub fn get_unidentified_clients(&self) -> Vec<Rc<RemoteConnection>> {
        self.clients.borrow().values().filter(|sc| sc.client_data.borrow().is_empty()).cloned().collect()
    }

    pub fn get_client_ws(&self, ws_id: u64) -> Option<Rc<RemoteConnection>> {
        self.clients.borrow().get(&ws_id).filter(|c| c.kind == ConnKind::WebSocket).cloned()
    }

    pub fn get_clients_by_type(&self, client_type: &str) -> Vec<Rc<RemoteConnection>> {
        if client_type.is_empty() {
            return Vec::new();
        }
        let ct = client_type.to_lowercase();
        self.clients
            .borrow()
            .values()
            .filter(|sc| sc.client_data.borrow().get("type").map(|t| *t == ct).unwrap_or(false))
            .cloned()
            .collect()
    }

    pub fn add_client(&self, sc: Rc<RemoteConnection>) {
        let uid = sc.uid();
        self.clients.borrow_mut().insert(uid, sc.clone());
        server().emit::<crate::events::ClientAdded>(sc);
        log::debug!("New Websocket Added: {}", uid);
    }

    pub fn remove_client(&self, sc: &RemoteConnection) {
        let old = self.clients.borrow_mut().shift_remove(&sc.uid());
        if let Some(old) = old {
            server().emit::<crate::events::ClientRemoved>(old);
            log::debug!("Websocket Removed: {}", sc.uid());
        }
        self.check_closed_event();
    }

    pub fn add_bridge_connection(&self, bc: Rc<RemoteConnection>) {
        let uid = bc.uid();
        self.bridge_connections.borrow_mut().insert(uid, bc);
        log::debug!("New Bridge Connection Added: {}", uid);
    }

    pub fn remove_bridge_connection(&self, bc: &RemoteConnection) {
        if self.bridge_connections.borrow_mut().shift_remove(&bc.uid()).is_some() {
            log::debug!("Bridge Connection Removed: {}", bc.uid());
        }
        self.check_closed_event();
    }

    fn check_closed_event(&self) {
        if let Some(n) = self.closed_notify.borrow().as_ref() {
            if self.clients.borrow().is_empty() && self.bridge_connections.borrow().is_empty() {
                n.notify_waiters();
                n.notify_one();
            }
        }
    }

    pub fn notify_clients(&self, name: &str, data: &[Value], mask: &[u64]) {
        // encoded once, shared by every client's queue
        let encoded = Utf8Bytes::from(notification(name, data));
        let clients: Vec<Rc<RemoteConnection>> = self.clients.borrow().values().cloned().collect();
        for sc in clients {
            if mask.contains(&sc.uid()) || sc.need_auth() {
                continue;
            }
            sc.queue_message_str(encoded.clone());
        }
    }

    pub fn get_count(&self) -> usize {
        self.clients.borrow().len()
    }

    pub async fn close(&self) {
        if self.clients.borrow().is_empty() {
            return;
        }
        let notify = Rc::new(tokio::sync::Notify::new());
        *self.closed_notify.borrow_mut() = Some(notify.clone());
        for bc in self.bridge_connections.borrow().values() {
            bc.close_socket(1001, "Server Shutdown");
        }
        for sc in self.clients.borrow().values() {
            sc.close_socket(1001, "Server Shutdown");
        }
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), notify.notified()).await;
        *self.closed_notify.borrow_mut() = None;
    }
}

async fn handle_identify(req: WebRequest) -> ApiResult {
    let sc = req.get_client_connection().ok_or_else(|| ServerError::new("No connection", 500))?;
    if sc.identified() {
        return Err(ServerError::bad(format!(
            "Connection already identified: {}",
            crate::pyfmt::py_repr(&sc.client_data_value())
        )));
    }
    let name = req.get_str("client_name")?;
    let version = req.get_str("version")?;
    let client_type = req.get_str("type")?.to_lowercase();
    let url = req.get_str("url")?;
    sc.authenticate(req.get_str_opt("access_token"), req.get_str_opt("api_key"))?;
    if !CLIENT_TYPES.contains(&client_type.as_str()) {
        return Err(ServerError::bad(format!("Invalid Client Type: {}", client_type)));
    }
    let mut cd = IndexMap::new();
    cd.insert("name".to_string(), name.clone());
    cd.insert("version".to_string(), version.clone());
    cd.insert("type".to_string(), client_type.clone());
    cd.insert("url".to_string(), url);
    sc.set_client_data(cd);
    if client_type == "agent" {
        let ext: Rc<crate::components::extensions::ExtensionManager> =
            server().get::<crate::components::extensions::ExtensionManager>().ok_or_else(|| ServerError::new("Extensions not loaded", 500))?;
        if let Err(e) = ext.register_agent(&sc) {
            sc.set_client_type("");
            return Err(e);
        }
    }
    log::info!(
        "Websocket {} Client Identified - Name: {}, Version: {}, Type: {}",
        sc.uid(),
        name,
        version,
        client_type
    );
    let uid = sc.uid();
    server().emit::<crate::events::ClientIdentified>(sc);
    Ok(json!({"connection_id": uid}))
}

// ***** Websocket HTTP handling *****

fn text_response(status: u16, body: &str) -> HttpResponse {
    let mut r = HttpResponse::new(status);
    r.set_body(body.to_string());
    r
}

/// tornado's default WebSocketHandler.check_origin
fn same_origin(origin: &str, host: Option<&str>) -> bool {
    let netloc = match url::Url::parse(origin) {
        Ok(u) => {
            let mut n = u.host_str().unwrap_or("").to_string();
            if let Some(p) = u.port() {
                n = format!("{}:{}", n, p);
            }
            n.to_lowercase()
        }
        Err(_) => origin.to_lowercase(),
    };
    Some(netloc.as_str()) == host
}

/// Handles both /websocket and /klippysocket upgrade requests
pub async fn serve_websocket(
    mut req: Request<Incoming>,
    info: &HttpRequest,
    bridge: bool,
) -> (Result<Response<Body>, HttpResponse>, Option<UserInfo>) {
    let srv = server();
    let app = srv.app();
    // prepare()
    let max_conns = app.max_ws_conns;
    if CONNECTION_COUNT.with(|c| c.get()) >= max_conns {
        // ServerError raised in prepare -> 500
        return (Err(error_response(&HttpError::internal(), None)), None);
    }
    let auth = srv.get::<crate::components::authorization::Authorization>();
    let mut user = None;
    let mut need_auth = false;
    let mut cors_allowed = false;
    if let Some(a) = &auth {
        match a.authenticate_request(info, true).await {
            Ok(u) => user = u,
            Err(e) => {
                if bridge {
                    return (Err(error_response(&e, None)), None);
                }
                log::info!("Websocket Failed Authentication: {}", e.reason_phrase());
                need_auth = true;
            }
        }
        let origin = info.header("Origin").or_else(|| info.header("Sec-Websocket-Origin"));
        cors_allowed = a.check_cors(origin.as_deref()).await;
    }
    let mut klippy_stream = None;
    if bridge {
        let kconn = srv.klippy();
        match kconn.open_klippy_connection(false).await {
            Ok(s) => klippy_stream = Some(s),
            Err(e) => {
                let err = if e.status_code == 503 && e.message != "Klippy Unix Connection Not Available" {
                    HttpError::new(503)
                } else {
                    HttpError::std_from(&e)
                };
                return (Err(error_response(&err, None)), user);
            }
        }
    }
    // get()
    if info.header("Upgrade").map(|u| u.to_lowercase()).as_deref() != Some("websocket") {
        return (Err(text_response(400, "Can \"Upgrade\" only to \"WebSocket\".")), user);
    }
    let conn_hdr = info.header("Connection").unwrap_or_default();
    if !conn_hdr.split(',').any(|s| s.trim().to_lowercase() == "upgrade") {
        return (Err(text_response(400, "\"Connection\" must be \"Upgrade\".")), user);
    }
    let origin = info.header("Origin").or_else(|| info.header("Sec-Websocket-Origin"));
    if let Some(o) = &origin {
        let host = info.header("Host");
        if !same_origin(o, host.as_deref()) && !cors_allowed {
            return (Err(text_response(403, "Cross origin websockets not allowed")), user);
        }
    }
    let ver = info.header("Sec-WebSocket-Version").unwrap_or_default();
    if !["7", "8", "13"].contains(&ver.as_str()) {
        let mut r = HttpResponse::new(426);
        r.set_header("Sec-WebSocket-Version", "7, 8, 13");
        return (Err(r), user);
    }
    let cfg = hyper_tungstenite::tungstenite::protocol::WebSocketConfig::default();
    let (response, websocket) = match hyper_tungstenite::upgrade(&mut req, Some(cfg)) {
        Ok(r) => r,
        Err(_) => return (Err(text_response(400, "Missing/Invalid WebSocket Key")), user),
    };
    let (tx, rx) = mpsc::unbounded_channel();
    let ip = crate::utils::parse_ip_address(&info.remote_ip);
    let kind = if bridge { ConnKind::Bridge } else { ConnKind::WebSocket };
    let conn = RemoteConnection::new(kind, tx, ip, info.host_name());
    conn.cors_allowed.set(cors_allowed);
    if let Some(u) = user.clone() {
        *conn.user_info.borrow_mut() = Some(u);
    }
    conn.set_need_auth(need_auth);
    let agent = info.header("User-Agent").unwrap_or_default();
    let is_proxy = info.is_proxied();
    spawn(async move {
        let ws = match websocket.await {
            Ok(ws) => ws,
            Err(e) => {
                log::debug!("Websocket upgrade failed: {}", e);
                return;
            }
        };
        if bridge {
            run_bridge(conn, ws, rx, klippy_stream.unwrap(), agent, is_proxy).await;
        } else {
            run_websocket(conn, ws, rx, agent, is_proxy).await;
        }
    });
    let (parts, body) = response.into_parts();
    let body = full_body(body.collect().await.map(|c| c.to_bytes()).unwrap_or_default());
    (Ok(Response::from_parts(parts, body)), user)
}

type WsStream = hyper_tungstenite::WebSocketStream<hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>>;

const PING_INTERVAL: f64 = 10.0;
const PING_TIMEOUT: f64 = 30.0;

async fn writer_loop(
    mut sink: futures::stream::SplitSink<WsStream, Message>,
    mut rx: mpsc::UnboundedReceiver<OutMsg>,
    conn: Rc<RemoteConnection>,
) {
    let mut ping = tokio::time::interval(std::time::Duration::from_secs_f64(PING_INTERVAL));
    ping.tick().await;
    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    None => break,
                    Some(OutMsg::Text(t)) => {
                        if sink.send(Message::Text(t)).await.is_err() {
                            conn.is_closed.set(true);
                            log::info!("Websocket closed while writing: {}", conn.uid());
                            break;
                        }
                    }
                    Some(OutMsg::Close(code, reason)) => {
                        let frame = CloseFrame { code: CloseCode::from(code), reason: reason.into() };
                        let _ = sink.send(Message::Close(Some(frame))).await;
                        let _ = sink.close().await;
                        break;
                    }
                }
            }
            _ = ping.tick() => {
                let now = crate::utils::monotonic();
                if now - conn.last_pong_time.get() > PING_TIMEOUT + PING_INTERVAL {
                    let _ = sink.close().await;
                    break;
                }
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }
}

async fn run_websocket(conn: Rc<RemoteConnection>, ws: WsStream, rx: mpsc::UnboundedReceiver<OutMsg>, agent: String, is_proxy: bool) {
    CONNECTION_COUNT.with(|c| c.set(c.get() + 1));
    let srv = server();
    let wsm = srv.wsm();
    conn.set_connected_time(crate::utils::monotonic());
    log::info!(
        "Websocket Opened: ID: {}, Proxied: {}, User Agent: {}, Host Name: {}",
        conn.uid(),
        crate::pyfmt::py_bool(is_proxy),
        agent,
        conn.hostname()
    );
    wsm.add_client(conn.clone());
    let (sink, mut stream) = ws.split();
    let writer = spawn(writer_loop(sink, rx, conn.clone()));
    while let Some(msg) = stream.next().await {
        match msg {
            Ok(Message::Text(t)) => {
                let c = conn.clone();
                spawn(c.process_message(t.as_bytes().to_vec()));
            }
            Ok(Message::Binary(b)) => {
                let c = conn.clone();
                spawn(c.process_message(b.to_vec()));
            }
            Ok(Message::Pong(_)) => conn.last_pong_time.set(crate::utils::monotonic()),
            Ok(Message::Close(frame)) => {
                if let Some(f) = frame {
                    *conn.close_code.borrow_mut() = Some(f.code.into());
                    *conn.close_reason.borrow_mut() = Some(f.reason.to_string());
                }
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    // on_close
    conn.is_closed.set(true);
    CONNECTION_COUNT.with(|c| c.set(c.get() - 1));
    srv.klippy().remove_subscription(conn.uid());
    conn.fail_pending();
    let pong_elapsed = crate::utils::monotonic() - conn.last_pong_time.get();
    let fmt_opt = |v: Option<String>| v.unwrap_or_else(|| "None".into());
    log::info!(
        "Websocket Closed: ID: {} Close Code: {}, Close Reason: {}, Pong Time Elapsed: {:.2}",
        conn.uid(),
        fmt_opt(conn.close_code.borrow().map(|c| c.to_string())),
        fmt_opt(conn.close_reason.borrow().clone()),
        pong_elapsed
    );
    if conn.client_data().get("type").map(|t| t == "agent").unwrap_or(false) {
        if let Some(ext) = srv.get::<crate::components::extensions::ExtensionManager>() {
            ext.remove_agent(&conn);
        }
    }
    wsm.remove_client(&conn);
    let _ = conn.sender.send(OutMsg::Close(1000, String::new()));
    let _ = writer.await;
}

async fn run_bridge(
    conn: Rc<RemoteConnection>,
    ws: WsStream,
    rx: mpsc::UnboundedReceiver<OutMsg>,
    klippy: tokio::net::UnixStream,
    agent: String,
    is_proxy: bool,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    CONNECTION_COUNT.with(|c| c.set(c.get() + 1));
    let srv = server();
    let wsm = srv.wsm();
    conn.set_connected_time(crate::utils::monotonic());
    log::info!(
        "Bridge Socket Opened: ID: {}, Proxied: {}, User Agent: {}, Host Name: {}",
        conn.uid(),
        crate::pyfmt::py_bool(is_proxy),
        agent,
        conn.hostname()
    );
    wsm.add_bridge_connection(conn.clone());
    let (kread, mut kwrite) = klippy.into_split();
    let (sink, mut stream) = ws.split();
    let writer = spawn(writer_loop(sink, rx, conn.clone()));
    // klippy -> websocket
    let c2 = conn.clone();
    let reader = spawn(async move {
        let mut reader = tokio::io::BufReader::with_capacity(64 * 1024, kread);
        loop {
            let mut buf = Vec::new();
            match reader.read_until(0x03, &mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if buf.last() != Some(&0x03) {
                        break;
                    }
                    buf.pop();
                    if c2.is_closed.get() {
                        break;
                    }
                    c2.queue_message_str(String::from_utf8_lossy(&buf).to_string());
                }
            }
        }
        if !c2.is_closed.get() {
            log::debug!("Bridge Disconnection From _read_unix_stream()");
            c2.close_socket(1001, "Klippy Disconnected");
        }
    });
    while let Some(msg) = stream.next().await {
        let data: Vec<u8> = match msg {
            Ok(Message::Text(t)) => t.as_bytes().to_vec(),
            Ok(Message::Binary(b)) => b.to_vec(),
            Ok(Message::Pong(_)) => {
                conn.last_pong_time.set(crate::utils::monotonic());
                continue;
            }
            Ok(Message::Close(frame)) => {
                if let Some(f) = frame {
                    *conn.close_code.borrow_mut() = Some(f.code.into());
                    *conn.close_reason.borrow_mut() = Some(f.reason.to_string());
                }
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        };
        let mut out = data;
        out.push(0x03);
        if kwrite.write_all(&out).await.is_err() {
            if !conn.is_closed.get() {
                log::debug!("Klippy Disconnection From _write_request()");
                conn.close_socket(1001, "Klippy Disconnected");
            }
            break;
        }
    }
    CONNECTION_COUNT.with(|c| c.set(c.get() - 1));
    conn.is_closed.set(true);
    reader.abort();
    let pong_elapsed = crate::utils::monotonic() - conn.last_pong_time.get();
    let fmt_opt = |v: Option<String>| v.unwrap_or_else(|| "None".into());
    log::info!(
        "Bridge Socket Closed: ID: {} Close Code: {}, Close Reason: {}, Pong Time Elapsed: {:.2}",
        conn.uid(),
        fmt_opt(conn.close_code.borrow().map(|c| c.to_string())),
        fmt_opt(conn.close_reason.borrow().clone()),
        pong_elapsed
    );
    wsm.remove_bridge_connection(&conn);
    let _ = conn.sender.send(OutMsg::Close(1000, String::new()));
    let _ = writer.await;
}

pub fn load_component(config: &ConfigHelper) -> Rc<WebsocketManager> {
    WebsocketManager::new(config)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    // notification() makes the bytes json!() made: key order, escaping,
    // float and unicode forms
    #[test]
    fn notification_as_json() {
        let cases: Vec<(&str, Vec<serde_json::Value>)> = vec![
            ("klippy_ready", vec![]),
            ("status_update", vec![json!({"a": 1.5, "b": [1e300, -0.0, "\u{e9}\"\n"], "c": null}), json!(1234.25)]),
            ("gcode_response", vec![json!("ok \u{1F600} \u{1}")]),
        ];
        for (name, data) in cases {
            let mut msg = json!({"jsonrpc": "2.0", "method": format!("notify_{}", name)});
            if !data.is_empty() {
                msg["params"] = serde_json::Value::Array(data.clone());
            }
            assert_eq!(super::notification(name, &data), msg.to_string(), "{name}");
        }
    }

    #[test]
    fn status_update_format() {
        let status = json!({"toolhead": {"position": [1.5, 2.0], "homed_axes": "xyz"}});
        let et = 1234.5678f64;
        let expected = json!({"jsonrpc": "2.0", "method": "notify_status_update", "params": [status, et]}).to_string();
        let got = format!(
            r#"{{"jsonrpc":"2.0","method":"notify_status_update","params":[{},{}]}}"#,
            serde_json::to_string(status.as_object().unwrap()).unwrap(),
            serde_json::to_string(&et).unwrap()
        );
        assert_eq!(got, expected);
    }
}

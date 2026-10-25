// Moonraker extension management (port of moonraker/components/extensions.py)

use crate::common::*;
use crate::components::websockets::{ConnKind, OutMsg, RemoteConnection};
use crate::confighelper::ConfigHelper;
use crate::server::{server, spawn, Component};
use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

pub struct ExtensionManager {
    agents: RefCell<IndexMap<String, Rc<RemoteConnection>>>,
    agent_methods: RefCell<HashMap<u64, Vec<String>>>,
    uds_task: RefCell<Option<tokio::task::JoinHandle<()>>>,
}

#[async_trait(?Send)]
impl Component for ExtensionManager {
    async fn close(self: Rc<Self>) {
        if let Some(t) = self.uds_task.borrow_mut().take() {
            t.abort();
        }
    }
}

macro_rules! ext_endpoint {
    ($srv:expr, $ext:expr, $path:expr, $rt:expr, $method:ident, $tp:expr) => {{
        let e = $ext.clone();
        $srv.register_endpoint_ext(
            $path,
            $rt,
            callback(move |req: WebRequest| {
                let e = e.clone();
                async move { e.$method(req).await }
            }),
            $tp,
            true,
            None,
            true,
        )?;
    }};
}

impl ExtensionManager {
    pub fn new(_config: &ConfigHelper) -> Result<Rc<Self>, crate::confighelper::ConfigError> {
        let srv = server();
        let ext = Rc::new(ExtensionManager {
            agents: RefCell::new(IndexMap::new()),
            agent_methods: RefCell::new(HashMap::new()),
            uds_task: RefCell::new(None),
        });
        ext_endpoint!(srv, ext, "/connection/register_remote_method", RequestType::POST, register_agent_method, TransportType::WEBSOCKET);
        ext_endpoint!(srv, ext, "/connection/send_event", RequestType::POST, handle_agent_event, TransportType::WEBSOCKET);
        ext_endpoint!(srv, ext, "/server/extensions/list", RequestType::GET, handle_list_extensions, TransportType::all());
        ext_endpoint!(srv, ext, "/server/extensions/request", RequestType::POST, handle_call_agent, TransportType::all());
        Ok(ext)
    }

    pub fn register_agent(&self, conn: &Rc<RemoteConnection>) -> Result<(), ServerError> {
        let data = conn.client_data();
        let name = data.get("name").cloned().unwrap_or_default();
        let ctype = data.get("type").cloned().unwrap_or_default();
        if ctype != "agent" {
            return Err(ServerError::bad(format!("Cannot register client type '{}' as an agent", ctype)));
        }
        if self.agents.borrow().contains_key(&name) {
            return Err(ServerError::bad(format!("Agent '{}' already registered and connected'", name)));
        }
        self.agents.borrow_mut().insert(name.clone(), conn.clone());
        let evt = json!({"agent": name, "event": "connected", "data": conn.client_data_value()});
        conn.send_notification("agent_event", vec![evt]);
        Ok(())
    }

    pub fn remove_agent(&self, conn: &Rc<RemoteConnection>) {
        let name = conn.client_data().get("name").cloned().unwrap_or_default();
        if self.agents.borrow().contains_key(&name) {
            let kc = server().klippy();
            let methods = self.agent_methods.borrow_mut().remove(&conn.uid()).unwrap_or_default();
            for m in methods {
                kc.unregister_method(&m);
            }
            self.agents.borrow_mut().shift_remove(&name);
            conn.send_notification("agent_event", vec![json!({"agent": name, "event": "disconnected"})]);
        }
    }

    async fn handle_agent_event(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let conn = req.get_client_connection().ok_or_else(|| ServerError::bad("No connection detected"))?;
        let data = conn.client_data();
        if data.get("type").map(|s| s.as_str()) != Some("agent") {
            return Err(ServerError::bad("Only connections of the 'agent' type can send events"));
        }
        let name = data.get("name").cloned().unwrap_or_default();
        let evt_name = req.get_str("event")?;
        if evt_name == "connected" || evt_name == "disconnected" {
            return Err(ServerError::bad(format!("Event '{}' is reserved", evt_name)));
        }
        let mut evt = json!({"agent": name, "event": evt_name});
        if let Some(d) = req.get_opt("data").filter(|d| !d.is_null()) {
            evt["data"] = d;
        }
        conn.send_notification("agent_event", vec![evt]);
        Ok(json!("ok"))
    }

    async fn register_agent_method(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let conn = req.get_client_connection().ok_or_else(|| ServerError::bad("No connection detected"))?;
        let method_name = req.get_str("method_name")?;
        server().klippy().register_method_from_agent(&conn, &method_name)?;
        self.agent_methods.borrow_mut().entry(conn.uid()).or_default().push(method_name);
        Ok(json!("ok"))
    }

    async fn handle_list_extensions(self: Rc<Self>, _req: WebRequest) -> ApiResult {
        let agents: Vec<Value> = self.agents.borrow().values().map(|a| a.client_data_value()).collect();
        Ok(json!({"agents": agents}))
    }

    async fn handle_call_agent(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let agent = req.get_str("agent")?;
        let method = req.get_str("method")?;
        let args = req.get_opt("arguments").filter(|a| !a.is_null());
        if let Some(a) = &args {
            if !(a.is_array() || a.is_object()) {
                return Err(ServerError::bad("The 'arguments' field must contain an object or a list"));
            }
        }
        let conn = self.agents.borrow().get(&agent).cloned().ok_or_else(|| ServerError::bad(format!("Agent {} not connected", agent)))?;
        conn.call_method_with_response(&method, args).await
    }

    pub async fn start_unix_server(self: &Rc<Self>) {
        let sockfile = server().app_arg_str("unix_socket_path");
        let sock_path = crate::utils::resolve_path(&crate::utils::expand_user(&sockfile));
        log::info!("Creating Unix Domain Socket at '{}'", sock_path.display());
        // asyncio.start_unix_server only replaces an existing socket file
        if let Ok(m) = std::fs::metadata(&sock_path) {
            if std::os::unix::fs::FileTypeExt::is_socket(&m.file_type()) {
                let _ = std::fs::remove_file(&sock_path);
            }
        }
        let listener = match tokio::net::UnixListener::bind(&sock_path) {
            Ok(l) => l,
            Err(e) => {
                log::error!("Failed to create Unix Domain Socket: {}\n{}", sock_path.display(), e);
                return;
            }
        };
        *self.uds_task.borrow_mut() = Some(spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        spawn(unix_client(stream));
                    }
                    Err(e) => log::debug!("Unix accept error: {}", e),
                }
            }
        }));
    }
}

async fn unix_client(stream: tokio::net::UnixStream) {
    let fd = std::os::fd::AsRawFd::as_raw_fd(&stream);
    let cred = crate::utils::get_unix_peer_credentials(fd, "Unix Client Connection");
    let (read_half, mut write_half) = stream.into_split();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<OutMsg>();
    let conn = RemoteConnection::new(ConnKind::UnixSocket, tx, None, String::new());
    conn.set_connected_time(crate::utils::monotonic());
    let srv = server();
    let wsm = srv.wsm();
    wsm.add_client(conn.clone());
    let (pid, uid, gid) = match cred {
        Some((p, u, g)) => (p.to_string(), u.to_string(), g.to_string()),
        None => ("None".into(), "None".into(), "None".into()),
    };
    log::info!("Unix Socket Opened - Client ID: {}, Process ID: {}, User ID: {},  Group ID: {}", conn.uid(), pid, uid, gid);
    let close_reason: Rc<RefCell<(Option<u16>, String)>> = Rc::new(RefCell::new((None, "Read Exit".into())));
    let c2 = conn.clone();
    let cr = close_reason.clone();
    let writer = spawn(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                OutMsg::Text(t) => {
                    let mut data = Vec::with_capacity(t.len() + 1);
                    data.extend_from_slice(t.as_bytes());
                    data.push(0x03);
                    if write_half.write_all(&data).await.is_err() {
                        log::debug!("Unix Socket Disconnection From write_to_socket()");
                        *cr.borrow_mut() = (None, "Write Exception".into());
                        c2.is_closed.set(true);
                        break;
                    }
                }
                OutMsg::Close(code, reason) => {
                    *cr.borrow_mut() = (Some(code), reason);
                    break;
                }
            }
        }
        let _ = write_half.shutdown().await;
    });
    let mut reader = tokio::io::BufReader::with_capacity(64 * 1024, read_half);
    let writer_abort = writer.abort_handle();
    let read_fut = async {
        loop {
            let mut buf = Vec::new();
            match reader.read_until(0x03, &mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if buf.last() != Some(&0x03) {
                        break;
                    }
                    buf.pop();
                    let c = conn.clone();
                    spawn(c.process_message(buf));
                }
            }
        }
        log::debug!("Unix Socket Disconnection From _read_messages()");
    };
    // finish when either the reader exits or the writer closes
    let mut writer = writer;
    tokio::select! {
        _ = read_fut => {}
        _ = &mut writer => {}
    }
    writer_abort.abort();
    conn.is_closed.set(true);
    srv.klippy().remove_subscription(conn.uid());
    conn.fail_pending();
    let (code, reason) = close_reason.borrow().clone();
    log::info!(
        "Unix Socket Closed: ID: {}, Close Code: {}, Close Reason: {}",
        conn.uid(),
        code.map(|c| c.to_string()).unwrap_or_else(|| "None".into()),
        reason
    );
    if conn.client_data().get("type").map(|t| t == "agent").unwrap_or(false) {
        if let Some(ext) = srv.lookup_component::<ExtensionManager>("extensions") {
            ext.remove_agent(&conn);
        }
    }
    wsm.remove_client(&conn);
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<ExtensionManager>, crate::confighelper::ConfigError> {
    ExtensionManager::new(config)
}

// KlippyConnection - manage unix socket connection to Klipper
// (port of moonraker/components/klippy_connection.py)

use crate::common::*;
use crate::confighelper::ConfigHelper;
use crate::server::{server, spawn, Component, EventObject};
use async_trait::async_trait;
use futures::future::LocalBoxFuture;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

pub const RESERVED_ENDPOINTS: [&str; 3] = ["list_endpoints", "gcode/subscribe_output", "register_remote_method"];
const INIT_TIME: f64 = 0.25;
const LOG_ATTEMPT_INTERVAL: u64 = 8; // int(2. / INIT_TIME + .5)
const MAX_LOG_ATTEMPTS: u64 = 10 * LOG_ATTEMPT_INTERVAL;
const UNIX_BUFFER_LIMIT: usize = 20 * 1024 * 1024;
const SVC_INFO_KEY: &str = "klippy_connection.service_info";
const SRC_PATH_KEY: &str = "klippy_connection.path";
const PY_EXEC_KEY: &str = "klippy_connection.executable";

pub type RemoteMethod = Rc<dyn Fn(Map<String, Value>) -> Option<LocalBoxFuture<'static, ()>>>;
pub type Subscription = IndexMap<String, Option<Vec<String>>>;

pub fn remote_method<F: Fn(Map<String, Value>) + 'static>(f: F) -> RemoteMethod {
    Rc::new(move |p| {
        f(p);
        None
    })
}

struct SubEntry {
    transport: Rc<dyn ApiTransport>,
    sub: Subscription,
}

enum WriteCmd {
    Request(i64, Vec<u8>),
}

pub struct KlippyConnection {
    uds_address: PathBuf,
    writer_tx: RefCell<Option<mpsc::UnboundedSender<WriteCmd>>>,
    connected: Cell<bool>,
    connection_mutex: tokio::sync::Mutex<()>,
    log_no_access: Cell<bool>,
    connection_task: crate::server::TaskSlot,
    connection_done: RefCell<Option<Rc<tokio::sync::Notify>>>,
    connecting: Cell<bool>,
    closing: Cell<bool>,
    subscription_lock: tokio::sync::Mutex<()>,
    klippy_info: RefCell<Map<String, Value>>,
    klippy_identified: Cell<bool>,
    klippy_initializing: Cell<bool>,
    klippy_started: Cell<bool>,
    methods_registered: Cell<bool>,
    klipper_version: RefCell<String>,
    missing_reqs: RefCell<Vec<String>>,
    peer_cred: RefCell<Map<String, Value>>,
    service_info: RefCell<Value>,
    path: RefCell<PathBuf>,
    executable: RefCell<PathBuf>,
    init_attempts: Cell<u64>,
    state: Cell<KlippyState>,
    state_messages: RefCell<[String; 5]>,
    subscriptions: RefCell<IndexMap<u64, SubEntry>>,
    subscription_cache: RefCell<Map<String, Value>>,
    pending_requests: RefCell<HashMap<i64, (String, oneshot::Sender<Result<Value, ServerError>>)>>,
    remote_methods: RefCell<HashMap<String, RemoteMethod>>,
    klippy_reg_methods: RefCell<Vec<String>>,
    next_request_id: Cell<i64>,
    read_task: crate::server::TaskSlot,
    writer_task: crate::server::TaskSlot,
}

#[async_trait(?Send)]
impl Component for KlippyConnection {
    fn has_init(&self) -> bool {
        true
    }
    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        if let Some(machine) = server().get::<crate::components::machine::Machine>() {
            let si = self.service_info.borrow().clone();
            if si.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
                machine.log_service_info(&si);
            }
        }
        Ok(())
    }
}

impl KlippyConnection {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, crate::confighelper::ConfigError> {
        let uds_address = config.getpath_or("klippy_uds_address", std::path::Path::new("/tmp/klippy_uds"))?;
        let home = crate::utils::home_dir();
        let kc = Rc::new(KlippyConnection {
            uds_address,
            writer_tx: RefCell::new(None),
            connected: Cell::new(false),
            connection_mutex: tokio::sync::Mutex::new(()),
            log_no_access: Cell::new(true),
            connection_task: Default::default(),
            connection_done: RefCell::new(None),
            connecting: Cell::new(false),
            closing: Cell::new(false),
            subscription_lock: tokio::sync::Mutex::new(()),
            klippy_info: RefCell::new(Map::new()),
            klippy_identified: Cell::new(false),
            klippy_initializing: Cell::new(false),
            klippy_started: Cell::new(false),
            methods_registered: Cell::new(false),
            klipper_version: RefCell::new(String::new()),
            missing_reqs: RefCell::new(Vec::new()),
            peer_cred: RefCell::new(Map::new()),
            service_info: RefCell::new(json!({})),
            path: RefCell::new(home.join("klipper")),
            executable: RefCell::new(home.join("klippy-env/bin/python")),
            init_attempts: Cell::new(0),
            state: Cell::new(KlippyState::DISCONNECTED),
            state_messages: RefCell::new(Default::default()),
            subscriptions: RefCell::new(IndexMap::new()),
            subscription_cache: RefCell::new(Map::new()),
            pending_requests: RefCell::new(HashMap::new()),
            remote_methods: RefCell::new(HashMap::new()),
            klippy_reg_methods: RefCell::new(Vec::new()),
            next_request_id: Cell::new(crate::components::websockets::next_uid() as i64),
            read_task: Default::default(),
            writer_task: Default::default(),
        });
        kc.set_state_message(KlippyState::DISCONNECTED, "Klippy Disconnected");
        let k = Rc::downgrade(&kc);
        kc.remote_methods.borrow_mut().insert(
            "process_gcode_response".into(),
            remote_method(move |p| {
                if k.upgrade().is_some() {
                    let resp = p.get("response").cloned().unwrap_or(Value::Null);
                    server().send_event("server:gcode_response", vec![resp]);
                }
            }),
        );
        let k = Rc::downgrade(&kc);
        kc.remote_methods.borrow_mut().insert(
            "process_status_update".into(),
            remote_method(move |mut p| {
                if let Some(kc) = k.upgrade() {
                    let eventtime = p.get("eventtime").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let status = match p.remove("status") {
                        Some(Value::Object(s)) => s,
                        _ => Map::new(),
                    };
                    kc.process_status_update(eventtime, &status);
                }
            }),
        );
        Ok(kc)
    }

    fn set_state_message(&self, state: KlippyState, msg: &str) {
        self.state_messages.borrow_mut()[state.index()] = msg.to_string();
    }

    pub fn state_message_for(&self, state: KlippyState) -> String {
        self.state_messages.borrow()[state.index()].clone()
    }

    pub fn state(&self) -> KlippyState {
        if self.is_connected() && !self.klippy_started.get() {
            return KlippyState::STARTUP;
        }
        self.state.get()
    }

    pub fn state_message(&self) -> String {
        self.state_message_for(self.state.get())
    }

    pub fn klippy_info(&self) -> Value {
        Value::Object(self.klippy_info.borrow().clone())
    }

    pub fn peer_credentials(&self) -> Map<String, Value> {
        self.peer_cred.borrow().clone()
    }

    pub fn service_info(&self) -> Value {
        self.service_info.borrow().clone()
    }

    pub fn missing_requirements(&self) -> Vec<String> {
        self.missing_reqs.borrow().clone()
    }

    pub fn unit_name(&self) -> String {
        let si = self.service_info.borrow();
        let un = si.get("unit_name").and_then(|v| v.as_str()).unwrap_or("klipper.service");
        un.split('.').next().unwrap_or(un).to_string()
    }

    pub fn path(&self) -> PathBuf {
        self.path.borrow().clone()
    }

    pub fn executable(&self) -> PathBuf {
        self.executable.borrow().clone()
    }

    pub fn load_saved_state(&self) {
        let db = server().database();
        let kc_info = db.get_item_sync("moonraker", Some(&Value::String("klippy_connection".into())), Some(json!({})));
        let kc_info = kc_info.unwrap_or(json!({}));
        if let Some(p) = kc_info.get("path").and_then(|v| v.as_str()) {
            *self.path.borrow_mut() = PathBuf::from(p);
        }
        if let Some(p) = kc_info.get("executable").and_then(|v| v.as_str()) {
            *self.executable.borrow_mut() = PathBuf::from(p);
        }
        *self.service_info.borrow_mut() = kc_info.get("service_info").cloned().unwrap_or(json!({}));
    }

    /// Moonraker's wait_connected: the connection attempt in progress, if
    /// any, finished; whether Klippy is connected
    pub async fn wait_connected(self: &Rc<Self>) -> bool {
        let n = self.connection_done.borrow().clone();
        if let Some(n) = n {
            if self.connecting.get() {
                n.notified().await;
            }
        }
        self.is_connected()
    }

    pub async fn wait_started(self: &Rc<Self>, timeout: f64) -> bool {
        if !self.is_connected() {
            return false;
        }
        let n = self.connection_done.borrow().clone();
        if let Some(n) = n {
            if self.connecting.get() {
                let _ = tokio::time::timeout(std::time::Duration::from_secs_f64(timeout), n.notified()).await;
            }
        }
        self.is_connected()
    }

    pub fn register_remote_method(&self, method_name: &str, cb: RemoteMethod, need_klippy_reg: bool) -> Result<(), ServerError> {
        if self.remote_methods.borrow().contains_key(method_name) {
            return Err(ServerError::bad(format!("Remote method ({}) already registered", method_name)));
        }
        if server().is_running() {
            return Err(ServerError::bad(format!(
                "Failed to register remote method {}, methods must be registered during initialization",
                method_name
            )));
        }
        self.remote_methods.borrow_mut().insert(method_name.to_string(), cb);
        if need_klippy_reg {
            self.klippy_reg_methods.borrow_mut().push(method_name.to_string());
        }
        Ok(())
    }

    pub fn register_method_from_agent(
        self: &Rc<Self>,
        connection: &Rc<crate::components::websockets::RemoteConnection>,
        method_name: &str,
    ) -> Result<Option<tokio::task::JoinHandle<()>>, ServerError> {
        if connection.client_data().get("type").map(|t| t.as_str()) != Some("agent") {
            return Err(ServerError::bad("Only connections of the 'agent' type can register methods"));
        }
        if self.remote_methods.borrow().contains_key(method_name) {
            return Err(ServerError::bad(format!("Remote method ({}) already registered", method_name)));
        }
        let conn = Rc::downgrade(connection);
        let mname = method_name.to_string();
        self.remote_methods.borrow_mut().insert(
            method_name.to_string(),
            remote_method(move |p| {
                if let Some(c) = conn.upgrade() {
                    c.call_method(&mname, Some(Value::Object(p)));
                }
            }),
        );
        self.klippy_reg_methods.borrow_mut().push(method_name.to_string());
        if self.methods_registered.get() && self.state.get() != KlippyState::DISCONNECTED {
            let m = method_name.to_string();
            return Ok(Some(spawn(async move {
                let _ = server().klippy_apis().register_method(&m).await;
            })));
        }
        Ok(None)
    }

    pub fn unregister_method(&self, method_name: &str) {
        self.remote_methods.borrow_mut().remove(method_name);
        self.klippy_reg_methods.borrow_mut().retain(|m| m != method_name);
    }

    /// Starts a connection attempt if one is not in progress
    pub fn connect(self: &Rc<Self>) {
        if self.is_connected() || !server().is_running() || self.connecting.get() {
            return;
        }
        self.start_connect_task();
    }

    fn start_connect_task(self: &Rc<Self>) {
        let notify = Rc::new(tokio::sync::Notify::new());
        *self.connection_done.borrow_mut() = Some(notify.clone());
        self.connecting.set(true);
        let kc = self.clone();
        let handle = spawn(async move {
            kc.do_connect().await;
            kc.connecting.set(false);
            notify.notify_waiters();
        });
        self.connection_task.set(handle);
    }

    async fn do_connect(self: &Rc<Self>) -> bool {
        let _guard = self.connection_mutex.lock().await;
        while self.writer_tx.borrow().is_none() {
            tokio::time::sleep(std::time::Duration::from_secs_f64(INIT_TIME)).await;
            if self.closing.get() || !server().is_running() {
                return false;
            }
            if !self.uds_address.exists() {
                continue;
            }
            if !crate::utils::access_rw(&self.uds_address) {
                if self.log_no_access.get() {
                    log::info!(
                        "Cannot connect to Klippy, Linux user '{}' lacks permission to open Unix Domain Socket: {}",
                        crate::utils::get_user(),
                        self.uds_address.display()
                    );
                    self.log_no_access.set(false);
                }
                continue;
            }
            self.log_no_access.set(true);
            let stream = match tokio::net::UnixStream::connect(&self.uds_address).await {
                Ok(s) => s,
                Err(_) => continue,
            };
            log::info!("Klippy Connection Established");
            let fd = std::os::fd::AsRawFd::as_raw_fd(&stream);
            let have_creds = self.get_peer_credentials(fd);
            let (read_half, write_half) = stream.into_split();
            let (tx, rx) = mpsc::unbounded_channel();
            *self.writer_tx.borrow_mut() = Some(tx);
            self.connected.set(true);
            let kc = self.clone();
            self.writer_task.set(spawn(async move { kc.writer_loop(write_half, rx).await }));
            if have_creds {
                let pid = self.peer_cred.borrow().get("process_id").and_then(|v| v.as_i64()).unwrap_or(0);
                self.get_service_info(pid).await;
            }
            let kc = self.clone();
            self.read_task.set(spawn(async move { kc.read_stream(read_half).await }));
        }
        drop(_guard);
        self.init_klippy_connection().await
    }

    pub async fn open_klippy_connection(&self, primary: bool) -> Result<tokio::net::UnixStream, ServerError> {
        if !primary && !self.is_connected() {
            return Err(ServerError::new("Klippy Unix Connection Not Available", 503));
        }
        tokio::net::UnixStream::connect(&self.uds_address)
            .await
            .map_err(|e| ServerError::new(format!("Failed to open connection to Klippy: {}", e), 503))
    }

    fn get_peer_credentials(&self, fd: i32) -> bool {
        let (pid, uid, gid) = match crate::utils::get_unix_peer_credentials(fd, "Klippy") {
            Some(c) => c,
            None => return false,
        };
        if pid == 1 {
            log::debug!("Klipper Unix Socket created via Systemd Socket Activation");
            return false;
        }
        let mut pc = Map::new();
        pc.insert("process_id".into(), json!(pid));
        pc.insert("user_id".into(), json!(uid));
        pc.insert("group_id".into(), json!(gid));
        log::debug!("Klippy Connection: Received Peer Credentials: {}", crate::pyfmt::py_repr(&Value::Object(pc.clone())));
        *self.peer_cred.borrow_mut() = pc;
        true
    }

    async fn get_service_info(&self, process_id: i64) {
        let srv = server();
        let machine = match srv.get::<crate::components::machine::Machine>() {
            Some(m) => m,
            None => return,
        };
        let svc_info = machine.extract_service_info("klipper", process_id).await;
        if svc_info != *self.service_info.borrow() {
            let db = srv.database();
            let _ = db.insert_item("moonraker", &Value::String(SVC_INFO_KEY.into()), svc_info.clone()).await;
            *self.service_info.borrow_mut() = svc_info.clone();
            machine.log_service_info(&svc_info);
        }
    }

    async fn save_path_info(&self) {
        let info = self.klippy_info.borrow().clone();
        let kpath = PathBuf::from(info.get("klipper_path").and_then(|v| v.as_str()).unwrap_or(""));
        let kexec = PathBuf::from(info.get("python_path").and_then(|v| v.as_str()).unwrap_or(""));
        let db = server().database();
        if kpath != self.path() {
            *self.path.borrow_mut() = kpath.clone();
            let _ = db.insert_item("moonraker", &Value::String(SRC_PATH_KEY.into()), json!(kpath.to_string_lossy())).await;
            log::info!("Updated Stored Klipper Source Path: {}", kpath.display());
        }
        if kexec != self.executable() {
            *self.executable.borrow_mut() = kexec.clone();
            let _ = db.insert_item("moonraker", &Value::String(PY_EXEC_KEY.into()), json!(kexec.to_string_lossy())).await;
            log::info!("Updated Stored Klipper Python Path: {}", kexec.display());
        }
    }

    async fn writer_loop(self: Rc<Self>, mut writer: tokio::net::unix::OwnedWriteHalf, mut rx: mpsc::UnboundedReceiver<WriteCmd>) {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                WriteCmd::Request(id, data) => {
                    if writer.write_all(&data).await.is_err() {
                        if let Some((_, tx)) = self.pending_requests.borrow_mut().remove(&id) {
                            let _ = tx.send(Err(ServerError::new("Klippy Write Request Error", 503)));
                        }
                        if !self.closing.get() {
                            log::debug!("Klippy Disconnection From _write_request()");
                            let kc = self.clone();
                            spawn(async move { kc.close_conn(false).await });
                        }
                        break;
                    }
                }
            }
        }
    }

    async fn read_stream(self: Rc<Self>, read_half: tokio::net::unix::OwnedReadHalf) {
        let mut reader = tokio::io::BufReader::with_capacity(64 * 1024, read_half);
        let mut buf = Vec::new();
        loop {
            // reused, but a large response (configfile) doesn't stay allocated
            if buf.capacity() > 256 * 1024 {
                buf = Vec::new();
            } else {
                buf.clear();
            }
            match reader.read_until(0x03, &mut buf).await {
                Ok(0) => break,
                Ok(_) => {
                    if buf.last() != Some(&0x03) {
                        // EOF without terminator (IncompleteReadError)
                        break;
                    }
                    if buf.len() > UNIX_BUFFER_LIMIT {
                        log::error!("Klippy Stream Read Error: message exceeds buffer limit");
                        continue;
                    }
                    buf.pop();
                    match serde_json::from_slice::<Value>(&buf) {
                        Ok(cmd) => self.process_command(cmd),
                        Err(e) => log::error!(
                            "Error processing Klippy Host Response: {}\n{}",
                            String::from_utf8_lossy(&buf),
                            e
                        ),
                    }
                }
                Err(_) => break,
            }
        }
        if !self.closing.get() {
            log::debug!("Klippy Disconnection From _read_stream()");
            // close() aborts the read task, so run it in its own task
            let kc = self.clone();
            spawn(async move { kc.close_conn(false).await });
        }
    }

    async fn init_klippy_connection(self: &Rc<Self>) -> bool {
        self.klippy_identified.set(false);
        self.klippy_started.set(false);
        self.klippy_initializing.set(true);
        self.methods_registered.set(false);
        self.missing_reqs.borrow_mut().clear();
        self.init_attempts.set(0);
        self.state.set(KlippyState::STARTUP);
        while server().is_running() {
            tokio::time::sleep(std::time::Duration::from_secs_f64(INIT_TIME)).await;
            self.check_ready().await;
            if !self.klippy_initializing.get() {
                log::debug!("Klippy Connection Initialized");
                return true;
            }
            if !self.is_connected() {
                self.klippy_initializing.set(false);
                break;
            } else {
                self.init_attempts.set(self.init_attempts.get() + 1);
            }
        }
        log::debug!("Klippy Connection Failed to Init");
        false
    }

    async fn request_endpoints(self: &Rc<Self>) {
        let result = match server().klippy_apis().list_endpoints().await {
            Ok(r) => r,
            Err(_) => return,
        };
        let endpoints = result.get("endpoints").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let app = server().app();
        for ep in endpoints {
            let ep = match ep.as_str() {
                Some(e) => e.to_string(),
                None => continue,
            };
            if RESERVED_ENDPOINTS.contains(&ep.as_str()) {
                continue;
            }
            let kc = self.clone();
            let cb = callback(move |req: WebRequest| {
                let kc = kc.clone();
                async move { kc.request(req).await }
            });
            if let Err(e) = app.register_remote_endpoint(&ep, cb) {
                log::info!("Failed to register remote endpoint {}: {}", ep, e);
            }
        }
    }

    async fn request_initial_subscriptions(&self) {
        let apis = server().klippy_apis();
        let mut objs = Map::new();
        objs.insert("webhooks".into(), Value::Null);
        match apis.subscribe_objects(&objs, None).await {
            Err(e) => log::error!("Unable to subscribe to webhooks object\n{}", e),
            Ok(_) => log::info!("Webhooks Subscribed"),
        }
        match apis.subscribe_gcode_output().await {
            Err(e) => log::error!("Unable to register gcode output subscription\n{}", e),
            Ok(_) => log::info!("GCode Output Subscribed"),
        }
    }

    async fn check_ready(self: &Rc<Self>) {
        let srv = server();
        let send_id = !self.klippy_identified.get();
        let result = match srv.klippy_apis().get_klippy_info(send_id).await {
            Ok(r) => r,
            Err(e) => {
                let ia = self.init_attempts.get();
                if ia % LOG_ATTEMPT_INTERVAL == 0 && ia <= MAX_LOG_ATTEMPTS {
                    log::info!(
                        "{}\nKlippy info request error.  This indicates that\nKlippy may have experienced an error during startup.\nPlease check klippy.log for more information",
                        e
                    );
                }
                return;
            }
        };
        let result = match result {
            Value::Object(o) => o,
            _ => return,
        };
        let version = result.get("software_version").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if version != *self.klipper_version.borrow() {
            *self.klipper_version.borrow_mut() = version.clone();
            srv.add_log_rollover_item("klipper_version", &format!("Klipper Version: {}", version), true);
        }
        if let Some(klipper_pid) = result.get("process_id").and_then(|v| v.as_i64()) {
            let cur_pid = self.peer_cred.borrow().get("process_id").and_then(|v| v.as_i64());
            if cur_pid != Some(klipper_pid) {
                let mut pc = Map::new();
                pc.insert("process_id".into(), json!(klipper_pid));
                pc.insert("group_id".into(), result.get("group_id").cloned().unwrap_or(json!(-1)));
                pc.insert("user_id".into(), result.get("user_id").cloned().unwrap_or(json!(-1)));
                *self.peer_cred.borrow_mut() = pc;
                self.get_service_info(klipper_pid).await;
            }
        }
        *self.klippy_info.borrow_mut() = result.clone();
        let cur_state = self.state.get();
        let mut state_message = self.state_message_for(cur_state);
        if let Some(sm) = result.get("state_message").and_then(|v| v.as_str()) {
            state_message = sm.to_string();
            self.set_state_message(cur_state, &state_message);
        }
        let state_str = match result.get("state").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return,
        };
        if send_id {
            self.klippy_identified.set(true);
            self.save_path_info().await;
            srv.send_event("server:klippy_identified", vec![]).await;
            self.request_endpoints().await;
        }
        let new_state = KlippyState::from_string(&state_str).unwrap_or(KlippyState::ERROR);
        if !state_message.is_empty() {
            self.set_state_message(new_state, &state_message);
        }
        self.state.set(new_state);
        if new_state != KlippyState::STARTUP {
            self.request_initial_subscriptions().await;
            self.request_endpoints().await;
            let startup_state = self.state.get();
            let state = EventObject::KlippyState(startup_state.as_str().to_string(), self.state_message_for(startup_state));
            srv.send_event_object("server:klippy_started", vec![json!(startup_state.as_str())], state).await;
            self.klippy_started.set(true);
            if self.state.get() != KlippyState::READY {
                log::info!("\n{}", self.state_message());
                if self.state.get() == KlippyState::SHUTDOWN && startup_state != KlippyState::SHUTDOWN {
                    srv.send_event("server:klippy_shutdown", vec![]);
                }
            } else {
                self.verify_klippy_requirements().await;
                let methods = self.klippy_reg_methods.borrow().clone();
                for method in methods {
                    if let Err(e) = srv.klippy_apis().register_method(&method).await {
                        log::error!("Unable to register method '{}'\n{}", method, e);
                    }
                }
                self.methods_registered.set(true);
                if self.state.get() == KlippyState::READY {
                    log::info!("Klippy ready");
                    srv.send_event("server:klippy_ready", vec![]).await;
                    if self.state.get() == KlippyState::SHUTDOWN {
                        srv.send_event("server:klippy_shutdown", vec![]);
                    }
                } else {
                    log::info!("Klippy state transition from ready during init, new state: {}", self.state.get());
                }
            }
            self.klippy_initializing.set(false);
        }
    }

    async fn verify_klippy_requirements(&self) {
        let srv = server();
        let apis = srv.klippy_apis();
        let result = match apis.get_object_list().await {
            Ok(r) => r,
            Err(_) => {
                log::info!("Unable to retrieve Klipper Object List");
                return;
            }
        };
        let present: HashSet<String> = result.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        let mut missing: Vec<String> = ["virtual_sdcard", "display_status", "pause_resume"]
            .iter()
            .filter(|r| !present.contains(**r))
            .map(|s| s.to_string())
            .collect();
        missing.sort();
        *self.missing_reqs.borrow_mut() = missing.clone();
        if !missing.is_empty() {
            let err_str: Vec<String> = missing.iter().map(|o| format!("[{}]", o)).collect();
            log::info!(
                "\nWarning, unable to detect the following printer objects:\n{}\nPlease add the the above sections to printer.cfg for full Moonraker functionality.",
                err_str.join(", ")
            );
        }
        if !missing.contains(&"virtual_sdcard".to_string()) {
            let mut objs = Map::new();
            objs.insert("configfile".into(), Value::Null);
            match apis.query_objects(&objs).await {
                Err(_) => log::info!("Unable to set SD Card path"),
                Ok(q) => {
                    let vsd_path = q
                        .get("configfile")
                        .and_then(|c| c.get("config"))
                        .and_then(|c| c.get("virtual_sdcard"))
                        .and_then(|c| c.get("path"))
                        .and_then(|p| p.as_str())
                        .map(String::from);
                    match vsd_path {
                        Some(p) => {
                            if let Some(fm) = srv.get::<crate::components::file_manager::FileManager>() {
                                fm.validate_gcode_path(&p);
                            }
                        }
                        None => log::info!("Configuration for [virtual_sdcard] not found, unable to set SD Card path"),
                    }
                }
            }
        }
    }

    fn process_command(self: &Rc<Self>, mut cmd: Value) {
        if let Some(method) = cmd.get("method").and_then(|m| m.as_str()) {
            let cb = self.remote_methods.borrow().get(method).cloned();
            match cb {
                Some(cb) => {
                    // the message is owned, so params move instead of being copied
                    let params = match cmd.get_mut("params").map(Value::take) {
                        Some(Value::Object(p)) => p,
                        _ => Map::new(),
                    };
                    spawn(async move {
                        if let Some(f) = cb(params) {
                            f.await;
                        }
                    });
                }
                None => log::info!("Unknown method received: {}", method),
            }
            return;
        }
        if cmd.get("method").is_some() {
            return;
        }
        let req_id = cmd.get("id").and_then(|v| v.as_i64());
        let pending = req_id.and_then(|id| self.pending_requests.borrow_mut().remove(&id));
        let (_, tx) = match pending {
            Some(p) => p,
            None => {
                log::info!(
                    "No request matching request ID: {}, response: {}",
                    cmd.get("id").map(crate::pyfmt::py_repr).unwrap_or_else(|| "None".into()),
                    crate::pyfmt::py_repr(&cmd)
                );
                return;
            }
        };
        if let Some(result) = cmd.get("result") {
            let truthy = match result {
                Value::Null => false,
                Value::Bool(b) => *b,
                Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
                Value::String(s) => !s.is_empty(),
                Value::Array(a) => !a.is_empty(),
                Value::Object(o) => !o.is_empty(),
            };
            let _ = tx.send(Ok(if truthy { result.clone() } else { json!("ok") }));
        } else {
            let err = match cmd.get("error") {
                Some(Value::Object(o)) => o
                    .get("message")
                    .map(crate::pyfmt::py_str)
                    .unwrap_or_else(|| "Malformed Klippy Response".into()),
                Some(Value::String(s)) => s.clone(),
                Some(other) => crate::pyfmt::py_str(other),
                None => "Malformed Klippy Response".into(),
            };
            let _ = tx.send(Err(ServerError::new(err, 400)));
        }
    }

    pub fn process_status_update(&self, eventtime: f64, status: &Map<String, Value>) {
        {
            let mut cache = self.subscription_cache.borrow_mut();
            for (field, item) in status {
                if !cache.contains_key(field) {
                    cache.insert(field.clone(), json!({}));
                }
                if let (Some(e), Some(i)) = (cache.get_mut(field).and_then(|e| e.as_object_mut()), item.as_object()) {
                    for (k, v) in i {
                        // existing keys are updated in place, no key allocation
                        match e.get_mut(k) {
                            Some(slot) => *slot = v.clone(),
                            None => {
                                e.insert(k.clone(), v.clone());
                            }
                        }
                    }
                }
            }
        }
        if let Some(wh) = status.get("webhooks").and_then(|v| v.as_object()) {
            let cur = self.state.get();
            let mut state_message = self.state_message_for(cur);
            if let Some(sm) = wh.get("state_message").and_then(|v| v.as_str()) {
                state_message = sm.to_string();
                self.set_state_message(cur, &state_message);
            }
            if let Some(st) = wh.get("state").and_then(|v| v.as_str()) {
                let new_state = KlippyState::from_string(st).unwrap_or(KlippyState::ERROR);
                self.set_state_message(new_state, &state_message);
                if new_state == KlippyState::SHUTDOWN && !self.klippy_initializing.get() && self.state.get() != KlippyState::SHUTDOWN {
                    log::info!("Klippy has shutdown");
                    server().send_event("server:klippy_shutdown", vec![]);
                }
                self.state.set(new_state);
            }
        }
        let subs: Vec<(Rc<dyn ApiTransport>, Subscription)> =
            self.subscriptions.borrow().values().map(|e| (e.transport.clone(), e.sub.clone())).collect();
        for (conn, sub) in subs {
            let mut conn_status = Map::new();
            for (name, fields) in &sub {
                if let Some(Value::Object(val)) = status.get(name) {
                    let filtered: Map<String, Value> = match fields {
                        None => val.clone(),
                        Some(f) => val.iter().filter(|(k, _)| f.contains(k)).map(|(k, v)| (k.clone(), v.clone())).collect(),
                    };
                    if !filtered.is_empty() {
                        conn_status.insert(name.clone(), Value::Object(filtered));
                    }
                }
            }
            conn.send_status(&conn_status, eventtime);
        }
    }

    pub async fn request(self: &Rc<Self>, web_request: WebRequest) -> ApiResult {
        if !self.is_connected() {
            return Err(ServerError::new("Klippy Host not connected", 503));
        }
        let rpc_method = web_request.get_endpoint().to_string();
        if rpc_method == "objects/subscribe" {
            return self.request_subscription(web_request).await;
        }
        if rpc_method == "gcode/script" {
            let script = web_request.get_str_or("script", "");
            if !script.is_empty() {
                server().send_event("klippy_connection:gcode_received", vec![json!(script)]);
            }
        }
        self.request_standard(&rpc_method, Value::Object(web_request.args), None).await
    }

    fn parse_subscription(objects: Option<&Value>) -> Subscription {
        let mut sub = Subscription::new();
        if let Some(Value::Object(o)) = objects {
            for (k, v) in o {
                let fields = match v {
                    Value::Array(a) => Some(a.iter().map(crate::pyfmt::py_str).collect()),
                    _ => None,
                };
                sub.insert(k.clone(), fields);
            }
        }
        sub
    }

    fn sub_to_value(sub: &Subscription) -> Value {
        Value::Object(
            sub.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        match v {
                            None => Value::Null,
                            Some(f) => Value::Array(f.iter().map(|s| Value::String(s.clone())).collect()),
                        },
                    )
                })
                .collect(),
        )
    }

    async fn request_subscription(self: &Rc<Self>, web_request: WebRequest) -> ApiResult {
        let _lock = self.subscription_lock.lock().await;
        let mut args = web_request.args.clone();
        let conn = web_request
            .get_subscribable()
            .ok_or_else(|| ServerError::bad("No connection associated with subscription request"))?;
        let conn_id = conn.transport_id();
        self.subscriptions.borrow_mut().shift_remove(&conn_id);
        let requested_sub = Self::parse_subscription(args.get("objects"));
        let mut all_subs = requested_sub.clone();
        for entry in self.subscriptions.borrow().values() {
            for (obj, items) in &entry.sub {
                if let Some(prev) = all_subs.get(obj).cloned() {
                    match (items, prev) {
                        (None, _) | (_, None) => {
                            all_subs.insert(obj.clone(), None);
                        }
                        (Some(i), Some(p)) => {
                            let mut u: Vec<String> = p.clone();
                            for x in i {
                                if !u.contains(x) {
                                    u.push(x.clone());
                                }
                            }
                            all_subs.insert(obj.clone(), Some(u));
                        }
                    }
                } else {
                    all_subs.insert(obj.clone(), items.clone());
                }
            }
        }
        args.insert("objects".into(), Self::sub_to_value(&all_subs));
        args.insert("response_template".into(), json!({"method": "process_status_update"}));
        let mut result = self.request_standard("objects/subscribe", Value::Object(args), Some(20.0)).await?;
        let all_status = result.get("status").and_then(|s| s.as_object()).cloned().unwrap_or_default();
        let mut pruned = Map::new();
        let mut status_diff = Map::new();
        {
            let mut cache = self.subscription_cache.borrow_mut();
            for (obj, fields) in &all_status {
                let fields_map = fields.as_object().cloned().unwrap_or_default();
                if let Some(Value::Object(cached)) = cache.get(obj) {
                    for (fname, value) in &fields_map {
                        if let Some(cv) = cached.get(fname) {
                            if cv != value {
                                status_diff
                                    .entry(obj.clone())
                                    .or_insert_with(|| json!({}))
                                    .as_object_mut()
                                    .unwrap()
                                    .insert(fname.clone(), value.clone());
                            }
                        }
                    }
                }
                if obj == "configfile" {
                    let mut to_cache = fields_map.clone();
                    let mut removed = Vec::new();
                    for ex in ["config", "settings"] {
                        if to_cache.shift_remove(ex).is_some() {
                            removed.push(Value::String(ex.into()));
                        }
                    }
                    if !removed.is_empty() {
                        log::debug!(
                            "Removed excluded fields from subscription cache: {}: {}",
                            obj,
                            crate::pyfmt::py_repr(&Value::Array(removed))
                        );
                    }
                    cache.insert(obj.clone(), Value::Object(to_cache));
                } else {
                    cache.insert(obj.clone(), fields.clone());
                }
                if let Some(valid) = requested_sub.get(obj) {
                    match valid {
                        None => {
                            pruned.insert(obj.clone(), fields.clone());
                        }
                        Some(v) => {
                            let f: Map<String, Value> =
                                fields_map.iter().filter(|(k, _)| v.contains(k)).map(|(k, v)| (k.clone(), v.clone())).collect();
                            pruned.insert(obj.clone(), Value::Object(f));
                        }
                    }
                }
            }
            let keys: Vec<String> = cache.keys().cloned().collect();
            for k in keys {
                if !all_status.contains_key(&k) {
                    cache.remove(&k);
                }
            }
        }
        if !status_diff.is_empty() {
            log::debug!("Detected status difference during subscription: {}", crate::pyfmt::py_repr(&Value::Object(status_diff.clone())));
            let et = result.get("eventtime").and_then(|v| v.as_f64()).unwrap_or(0.0);
            self.process_status_update(et, &status_diff);
        }
        result["status"] = Value::Object(pruned);
        if !requested_sub.is_empty() {
            self.subscriptions.borrow_mut().insert(conn_id, SubEntry { transport: conn, sub: requested_sub });
        }
        Ok(result)
    }

    pub async fn request_standard(&self, rpc_method: &str, params: Value, timeout: Option<f64>) -> ApiResult {
        let id = self.next_request_id.get();
        self.next_request_id.set(id + 16);
        let (tx, mut rx) = oneshot::channel();
        self.pending_requests.borrow_mut().insert(id, (rpc_method.to_string(), tx));
        let msg = json!({"id": id, "method": rpc_method, "params": params});
        let mut data = msg.to_string().into_bytes();
        data.push(0x03);
        let sent = match (&*self.writer_tx.borrow(), self.closing.get()) {
            (Some(w), false) => w.send(WriteCmd::Request(id, data)).is_ok(),
            _ => false,
        };
        if !sent {
            self.pending_requests.borrow_mut().remove(&id);
            return Err(ServerError::new("Klippy Host not connected", 503));
        }
        let start = crate::utils::time_now();
        let to = timeout.unwrap_or(60.0);
        let result = loop {
            match tokio::time::timeout(std::time::Duration::from_secs_f64(to), &mut rx).await {
                Ok(Ok(r)) => break r,
                Ok(Err(_)) => break Err(ServerError::new("Klippy Disconnected", 503)),
                Err(_) => {
                    if timeout.is_some() {
                        break Err(ServerError::new("Klippy request timed out", 500));
                    }
                    log::info!("Request '{}' pending: {:.2} seconds", rpc_method, crate::utils::time_now() - start);
                }
            }
        };
        self.pending_requests.borrow_mut().remove(&id);
        result
    }

    pub fn remove_subscription(&self, conn_id: u64) {
        self.subscriptions.borrow_mut().shift_remove(&conn_id);
    }

    pub fn is_connected(&self) -> bool {
        self.connected.get() && !self.closing.get()
    }

    pub fn is_ready(&self) -> bool {
        self.state.get() == KlippyState::READY
    }

    pub fn is_printing(&self) -> bool {
        if !self.is_ready() {
            return false;
        }
        match server().get::<crate::components::job_state::JobState>() {
            Some(js) => js.get_last_stats().get("state").and_then(|v| v.as_str()) == Some("printing"),
            None => false,
        }
    }

    pub fn get_subscription_cache(&self) -> Map<String, Value> {
        self.subscription_cache.borrow().clone()
    }

    pub async fn rollover_log(&self) -> Result<(), ServerError> {
        if self.service_info.borrow().get("unit_name").is_none() {
            return Err(ServerError::bad("Unable to detect Klipper Service, cannot perform manual rollover"));
        }
        let logfile = self
            .klippy_info
            .borrow()
            .get("log_file")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| ServerError::bad("Unable to detect path to Klipper log file"))?;
        if self.is_printing() {
            return Err(ServerError::bad("Cannot rollover log while printing"));
        }
        let logpath = crate::utils::resolve_path(&crate::utils::expand_user(&logfile));
        if !logpath.is_file() {
            return Err(ServerError::bad(format!("No file at {} exists, cannot perform rollover", logpath.display())));
        }
        let machine: Rc<crate::components::machine::Machine> =
            server().get::<crate::components::machine::Machine>().ok_or_else(|| ServerError::new("Machine not loaded", 500))?;
        let unit = self.unit_name();
        machine.do_service_action("stop", &unit).await?;
        let suffix = crate::utils::local_strftime("%Y-%m-%d_%H-%M-%S");
        let new_path = PathBuf::from(format!("{}.{}", logpath.display(), suffix));
        if new_path.exists() {
            let _ = std::fs::remove_file(&new_path);
        }
        std::fs::rename(&logpath, &new_path)?;
        machine.do_service_action("start", &unit).await?;
        Ok(())
    }

    async fn on_connection_closed(self: &Rc<Self>) {
        self.klippy_identified.set(false);
        self.klippy_initializing.set(false);
        self.klippy_started.set(false);
        self.methods_registered.set(false);
        self.state.set(KlippyState::DISCONNECTED);
        self.set_state_message(KlippyState::DISCONNECTED, "Klippy Disconnected");
        let pending: Vec<_> = self.pending_requests.borrow_mut().drain().collect();
        for (_, (_, tx)) in pending {
            let _ = tx.send(Err(ServerError::new("Klippy Disconnected", 503)));
        }
        self.subscriptions.borrow_mut().clear();
        self.subscription_cache.borrow_mut().clear();
        self.peer_cred.borrow_mut().clear();
        self.missing_reqs.borrow_mut().clear();
        log::info!("Klippy Connection Removed");
        server().send_event("server:klippy_disconnect", vec![]).await;
        if server().is_running() {
            self.start_connect_task();
        }
    }

    pub async fn close_conn(self: &Rc<Self>, wait_closed: bool) {
        if self.closing.get() {
            if wait_closed {
                let _g = self.connection_mutex.lock().await;
            }
            return;
        }
        self.closing.set(true);
        if self.connecting.get() {
            self.connection_task.cancel();
            self.connecting.set(false);
            if let Some(n) = self.connection_done.borrow().as_ref() {
                n.notify_waiters();
            }
        }
        let _g = self.connection_mutex.lock().await;
        if self.writer_tx.borrow().is_some() {
            *self.writer_tx.borrow_mut() = None;
            self.connected.set(false);
            self.writer_task.cancel();
            self.read_task.cancel();
            drop(_g);
            self.closing.set(false);
            self.on_connection_closed().await;
            return;
        }
        self.closing.set(false);
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<KlippyConnection>, crate::confighelper::ConfigError> {
    KlippyConnection::new(config)
}

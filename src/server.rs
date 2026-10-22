// Moonruster - HTTP/Websocket API Server for Klipper (port of moonraker/server.py)

use crate::common::*;
use crate::confighelper::{ConfigError, ConfigHelper};
use async_trait::async_trait;
use futures::future::LocalBoxFuture;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

pub const API_VERSION: (i64, i64, i64) = (1, 5, 0);
pub const SERVER_COMPONENTS: [&str; 3] = ["application", "websockets", "klippy_connection"];
pub const CORE_COMPONENTS: [&str; 16] = [
    "dbus_manager",
    "database",
    "file_manager",
    "authorization",
    "klippy_apis",
    "machine",
    "data_store",
    "shell_command",
    "proc_stats",
    "job_state",
    "job_queue",
    "history",
    "http_client",
    "announcements",
    "webcam",
    "extensions",
];

pub type EventCallback = Rc<dyn Fn(Vec<Value>) -> Option<LocalBoxFuture<'static, ()>>>;

/// Lifecycle hooks implemented by components
#[async_trait(?Send)]
pub trait Component: Any {
    /// Return true if the component implements component_init
    fn has_init(&self) -> bool {
        false
    }
    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        Ok(())
    }
    async fn on_exit(self: Rc<Self>) {}
    async fn close(self: Rc<Self>) {}
}

/// Marker for registered objects that have no lifecycle (jsonrpc, etc)
pub struct NoLifecycle;
impl Component for NoLifecycle {}

thread_local! {
    static SERVER: RefCell<Option<Rc<Server>>> = const { RefCell::new(None) };
}

pub fn server() -> Rc<Server> {
    SERVER.with(|s| s.borrow().clone()).expect("Server not initialized")
}

pub fn try_server() -> Option<Rc<Server>> {
    SERVER.with(|s| s.borrow().clone())
}

/// Spawn a task on the local executor (python's register_callback/create_task)
pub fn spawn<F: std::future::Future<Output = ()> + 'static>(fut: F) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_local(fut)
}

pub struct Server {
    pub app_args: RefCell<Map<String, Value>>,
    pub config: RefCell<Option<ConfigHelper>>,
    pub host: RefCell<String>,
    pub port: Cell<i64>,
    pub ssl_port: Cell<i64>,
    events: RefCell<HashMap<String, Vec<EventCallback>>>,
    components: RefCell<IndexMap<String, Rc<dyn Any>>>,
    lifecycle: RefCell<IndexMap<String, Rc<dyn LifecycleDyn>>>,
    failed_components: RefCell<Vec<String>>,
    warnings: RefCell<IndexMap<String, String>>,
    warn_counter: Cell<u64>,
    is_configured: Cell<bool>,
    server_running: Cell<bool>,
    pub exit_reason: RefCell<String>,
    exit_notify: tokio::sync::Notify,
    exited: Cell<bool>,
    debug: bool,
    verbose: bool,
}

/// Object safe wrapper around Component
pub trait LifecycleDyn {
    fn has_init(&self) -> bool;
    fn init(self: Rc<Self>) -> LocalBoxFuture<'static, Result<(), ServerError>>;
    fn exit(self: Rc<Self>) -> LocalBoxFuture<'static, ()>;
    fn close_comp(self: Rc<Self>) -> LocalBoxFuture<'static, ()>;
}

impl<T: Component + 'static> LifecycleDyn for T {
    fn has_init(&self) -> bool {
        Component::has_init(self)
    }
    fn init(self: Rc<Self>) -> LocalBoxFuture<'static, Result<(), ServerError>> {
        self.component_init()
    }
    fn exit(self: Rc<Self>) -> LocalBoxFuture<'static, ()> {
        self.on_exit()
    }
    fn close_comp(self: Rc<Self>) -> LocalBoxFuture<'static, ()> {
        self.close()
    }
}

impl Server {
    pub fn new(app_args: Map<String, Value>) -> Rc<Server> {
        let debug = app_args.get("debug").and_then(|v| v.as_bool()).unwrap_or(false);
        let verbose = app_args.get("verbose").and_then(|v| v.as_bool()).unwrap_or(false);
        let srv = Rc::new(Server {
            app_args: RefCell::new(app_args),
            config: RefCell::new(None),
            host: RefCell::new("0.0.0.0".into()),
            port: Cell::new(7125),
            ssl_port: Cell::new(7130),
            events: RefCell::new(HashMap::new()),
            components: RefCell::new(IndexMap::new()),
            lifecycle: RefCell::new(IndexMap::new()),
            failed_components: RefCell::new(Vec::new()),
            warnings: RefCell::new(IndexMap::new()),
            warn_counter: Cell::new(1),
            is_configured: Cell::new(false),
            server_running: Cell::new(false),
            exit_reason: RefCell::new(String::new()),
            exit_notify: tokio::sync::Notify::new(),
            exited: Cell::new(false),
            debug,
            verbose,
        });
        SERVER.with(|s| *s.borrow_mut() = Some(srv.clone()));
        srv
    }

    /// Second stage of python's Server.__init__
    pub fn init(self: &Rc<Self>) -> Result<(), ConfigError> {
        let config = self.parse_config()?;
        *self.config.borrow_mut() = Some(config.clone());
        *self.host.borrow_mut() = config.get_or("host", "0.0.0.0")?;
        self.port.set(config.getint_or("port", 7125)?);
        self.ssl_port.set(config.getint_or("ssl_port", 7130)?);
        config.getboolean_dep("enable_debug_logging", false, true)?;
        log::set_max_level(if self.verbose { log::LevelFilter::Debug } else { log::LevelFilter::Info });
        self.load_component(&config, "klippy_connection")?;
        self.load_component(&config, "application")?;
        crate::loghelper::set_server(self);
        self.load_component(&config, "websockets")?;
        let warnings: Vec<String> = self
            .get_app_arg("startup_warnings")
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|w| w.as_str().map(String::from))
            .collect();
        for w in warnings {
            self.add_warning(&w, None, true);
        }
        let s = self.clone();
        self.register_endpoint("/server/info", RequestType::GET, callback(move |r| {
            let s = s.clone();
            async move { s.handle_info_request(r).await }
        }))
        .map_err(ConfigError::from)?;
        let s = self.clone();
        self.register_endpoint("/server/config", RequestType::GET, callback(move |r| {
            let s = s.clone();
            async move { s.handle_config_request(r).await }
        }))
        .map_err(ConfigError::from)?;
        let s = self.clone();
        self.register_endpoint("/server/restart", RequestType::POST, callback(move |_r| {
            let s = s.clone();
            async move {
                let s2 = s.clone();
                spawn(async move { s2.stop_server("restart").await });
                Ok(json!("ok"))
            }
        }))
        .map_err(ConfigError::from)?;
        self.register_notification("server:klippy_ready", None);
        self.register_notification("server:klippy_shutdown", None);
        self.register_notification("server:klippy_disconnect", Some("klippy_disconnected"));
        self.register_notification("server:gcode_response", None);
        Ok(())
    }

    pub fn config(&self) -> ConfigHelper {
        self.config.borrow().clone().expect("config not loaded")
    }

    pub fn get_app_args(&self) -> Map<String, Value> {
        self.app_args.borrow().clone()
    }

    pub fn get_app_arg(&self, key: &str) -> Option<Value> {
        self.app_args.borrow().get(key).cloned()
    }

    pub fn app_arg_str(&self, key: &str) -> String {
        self.get_app_arg(key).and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
    }

    pub fn data_path(&self) -> String {
        self.app_arg_str("data_path")
    }

    pub fn get_api_version(&self) -> (i64, i64, i64) {
        API_VERSION
    }

    pub fn get_warnings(&self) -> Vec<String> {
        self.warnings.borrow().values().cloned().collect()
    }

    pub fn is_running(&self) -> bool {
        self.server_running.get()
    }

    pub fn is_configured(&self) -> bool {
        self.is_configured.get()
    }

    pub fn is_debug_enabled(&self) -> bool {
        self.debug
    }

    pub fn is_verbose_enabled(&self) -> bool {
        self.verbose
    }

    fn parse_config(&self) -> Result<ConfigHelper, ConfigError> {
        let args = self.get_app_args();
        let cfg_file = args.get("config_file").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let backup = args.get("backup_config").and_then(|v| v.as_str()).map(String::from);
        let is_backup = args.get("is_backup_config").and_then(|v| v.as_bool()).unwrap_or(false);
        let config = crate::confighelper::get_configuration(&cfg_file, backup.as_deref(), is_backup)?;
        let cfg_files = config.get_config_files().join("\n");
        let mut cfg_item = format!("\n{} Moonraker Configuration {}\n\n", "#".repeat(20), "#".repeat(20));
        cfg_item.push_str(&config.write_config());
        cfg_item.push_str(&"#".repeat(65));
        cfg_item.push_str(&format!("\nAll Configuration Files:\n{}\n", cfg_files));
        cfg_item.push_str(&"#".repeat(65));
        self.add_log_rollover_item("config", &cfg_item, true);
        Ok(config)
    }

    pub async fn server_init(self: &Rc<Self>, start_server: bool) -> Result<(), ServerError> {
        {
            let s = self.clone();
            spawn(async move {
                let mut sig = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(s) => s,
                    Err(_) => return,
                };
                if sig.recv().await.is_some() {
                    log::info!("Exiting with signal SIGTERM");
                    let s2 = s.clone();
                    spawn(async move { s2.stop_server("terminate").await });
                }
            });
        }
        let comps: Vec<(String, Rc<dyn LifecycleDyn>)> =
            self.lifecycle.borrow().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut optional = Vec::new();
        for (name, comp) in comps {
            if !comp.has_init() {
                continue;
            }
            if CORE_COMPONENTS.contains(&name.as_str()) {
                self.initialize_component(name, comp).await;
            } else {
                optional.push(self.initialize_component(name, comp));
            }
        }
        futures::future::join_all(optional).await;
        if let Some(fm) = self.lookup_component::<crate::components::file_manager::FileManager>("file_manager") {
            fm.start_file_observer();
        }
        if self.warnings.borrow().is_empty() {
            let cfg_file = self.app_arg_str("config_file");
            self.config().create_backup(&cfg_file);
        }
        let mut restarting = false;
        if let Some(machine) = self.lookup_component::<crate::components::machine::Machine>("machine") {
            restarting = machine.validate_installation().await;
        }
        if !restarting && start_server {
            self.start_server(true).await;
        }
        Ok(())
    }

    pub async fn start_server(self: &Rc<Self>, connect_to_klippy: bool) {
        if let Some(ext) = self.lookup_component::<crate::components::extensions::ExtensionManager>("extensions") {
            ext.start_unix_server().await;
        }
        log::info!(
            "Starting Moonraker on ({}, {}), Hostname: {}",
            self.host.borrow(),
            self.port.get(),
            crate::utils::hostname()
        );
        let app = self.app();
        app.listen(&self.host.borrow(), self.port.get(), self.ssl_port.get()).await;
        self.server_running.set(true);
        if connect_to_klippy {
            self.klippy().connect();
        }
    }

    pub async fn run_until_exit(&self) {
        if self.exited.get() {
            return;
        }
        self.exit_notify.notified().await;
    }

    pub fn add_log_rollover_item(&self, name: &str, item: &str, log_it: bool) {
        crate::loghelper::set_rollover_info(name, item);
        if log_it {
            log::info!("{}", item);
        }
    }

    pub fn add_warning(&self, warning: &str, warn_id: Option<&str>, log_it: bool) -> String {
        let id = match warn_id {
            Some(i) => i.to_string(),
            None => {
                let c = self.warn_counter.get();
                self.warn_counter.set(c + 1);
                format!("__warn_{}", c)
            }
        };
        self.warnings.borrow_mut().insert(id.clone(), warning.to_string());
        if log_it {
            log::warn!("{}", warning);
        }
        id
    }

    pub fn remove_warning(&self, warn_id: &str) {
        self.warnings.borrow_mut().shift_remove(warn_id);
    }

    // ***** Component Management *****

    async fn initialize_component(self: &Rc<Self>, name: String, comp: Rc<dyn LifecycleDyn>) {
        log::info!("Performing Component Post Init: [{}]", name);
        if let Err(e) = comp.init().await {
            self.add_warning(&format!("Component '{}' failed to load with error: {}", name, e), None, true);
            self.set_failed_component(&name);
        }
    }

    pub fn load_components(self: &Rc<Self>) -> Result<(), ConfigError> {
        let config = self.config();
        let mut cfg_sections: IndexMap<String, ()> = IndexMap::new();
        for s in config.sections() {
            let first = s.split_whitespace().next().unwrap_or("").to_string();
            cfg_sections.insert(first, ());
        }
        if cfg_sections.shift_remove("server").is_none() {
            return Err(ConfigError("No section [server] in config".into()));
        }
        self.load_component(&config, "database")?;
        self.klippy().load_saved_state();
        for comp in CORE_COMPONENTS {
            self.load_component(&config, comp)?;
            cfg_sections.shift_remove(comp);
        }
        for (section, _) in cfg_sections {
            self.load_component_optional(&config, &section);
        }
        config.validate_config(self);
        self.is_configured.set(true);
        Ok(())
    }

    /// Load a component, returning an error on failure (python default=MISSING)
    pub fn load_component(self: &Rc<Self>, config: &ConfigHelper, name: &str) -> Result<(), ConfigError> {
        if self.components.borrow().contains_key(name) {
            return Ok(());
        }
        if self.is_configured() {
            return Err(ConfigError("Cannot load components after configuration".into()));
        }
        if self.failed_components.borrow().iter().any(|c| c == name) {
            return Err(ConfigError(format!("Component {} previously failed to load", name)));
        }
        // python imports the module before calling getsection(), an unknown
        // component never adds its section to the parsed config
        if !crate::components::COMPONENT_NAMES.contains(&name) {
            log::error!("Unable to load component: ({})
ModuleNotFoundError: No module named 'moonraker.components.{}'", name, name);
            self.set_failed_component(name);
            return Err(ConfigError(format!("No module named 'moonraker.components.{}'", name)));
        }
        let comp_cfg = if SERVER_COMPONENTS.contains(&name) {
            config.clone()
        } else {
            let fallback = if CORE_COMPONENTS.contains(&name) { Some("server") } else { None };
            config.getsection_fallback(name, fallback)
        };
        match crate::components::load_component(self, &comp_cfg, name) {
            Ok(Some((any, lc))) => {
                self.components.borrow_mut().insert(name.to_string(), any);
                self.lifecycle.borrow_mut().insert(name.to_string(), lc);
                log::info!("Component ({}) loaded", name);
                Ok(())
            }
            Ok(None) => {
                log::error!("Unable to load component: ({})\nModuleNotFoundError: No module named 'moonraker.components.{}'", name, name);
                self.set_failed_component(name);
                Err(ConfigError(format!("No module named 'moonraker.components.{}'", name)))
            }
            Err(e) => {
                log::error!("Unable to load component: ({})\n{}", name, e);
                self.set_failed_component(name);
                Err(e)
            }
        }
    }

    pub fn load_component_optional(self: &Rc<Self>, config: &ConfigHelper, name: &str) -> bool {
        self.load_component(config, name).is_ok()
    }

    pub fn lookup_component<T: 'static>(&self, name: &str) -> Option<Rc<T>> {
        let any = self.components.borrow().get(name).cloned()?;
        any.downcast::<T>().ok()
    }

    pub fn has_component(&self, name: &str) -> bool {
        self.components.borrow().contains_key(name)
    }

    pub fn component_names(&self) -> Vec<String> {
        self.components.borrow().keys().cloned().collect()
    }

    pub fn failed_components(&self) -> Vec<String> {
        self.failed_components.borrow().clone()
    }

    pub fn set_failed_component(&self, name: &str) {
        let mut fc = self.failed_components.borrow_mut();
        if !fc.iter().any(|c| c == name) {
            fc.push(name.to_string());
        }
    }

    pub fn register_component<T: Component + 'static>(&self, name: &str, comp: Rc<T>) -> Result<(), ServerError> {
        if self.components.borrow().contains_key(name) {
            return Err(ServerError::bad(format!("Component '{}' already registered", name)));
        }
        self.components.borrow_mut().insert(name.to_string(), comp.clone() as Rc<dyn Any>);
        self.lifecycle.borrow_mut().insert(name.to_string(), comp as Rc<dyn LifecycleDyn>);
        Ok(())
    }

    // ***** Shortcuts to core components *****

    pub fn app(&self) -> Rc<crate::components::application::MoonrakerApp> {
        self.lookup_component("application").expect("application not loaded")
    }

    pub fn klippy(&self) -> Rc<crate::components::klippy_connection::KlippyConnection> {
        self.lookup_component("klippy_connection").expect("klippy_connection not loaded")
    }

    pub fn wsm(&self) -> Rc<crate::components::websockets::WebsocketManager> {
        self.lookup_component("websockets").expect("websockets not loaded")
    }

    pub fn klippy_apis(&self) -> Rc<crate::components::klippy_apis::KlippyApi> {
        self.lookup_component("klippy_apis").expect("klippy_apis not loaded")
    }

    pub fn database(&self) -> Rc<crate::components::database::MoonrakerDatabase> {
        self.lookup_component("database").expect("database not loaded")
    }

    // ***** Endpoints and events *****

    pub fn register_endpoint(&self, endpoint: &str, request_types: RequestType, cb: ApiCallback) -> Result<(), ServerError> {
        self.app().register_endpoint(endpoint, request_types, cb, TransportType::all(), true, None, true, false)
    }

    pub fn register_endpoint_ext(
        &self,
        endpoint: &str,
        request_types: RequestType,
        cb: ApiCallback,
        transports: TransportType,
        wrap_result: bool,
        content_type: Option<&str>,
        auth_required: bool,
    ) -> Result<(), ServerError> {
        self.app().register_endpoint(endpoint, request_types, cb, transports, wrap_result, content_type, auth_required, false)
    }

    pub fn register_debug_endpoint(&self, endpoint: &str, request_types: RequestType, cb: ApiCallback) -> Result<(), ServerError> {
        self.app().register_debug_endpoint(endpoint, request_types, cb, TransportType::all(), true)
    }

    pub fn register_notification(&self, event_name: &str, notify_name: Option<&str>) {
        self.wsm().register_notification(event_name, notify_name, None);
    }

    pub fn register_event_handler(&self, event: &str, cb: EventCallback) {
        self.events.borrow_mut().entry(event.to_string()).or_default().push(cb);
    }

    /// Convenience for synchronous event handlers
    pub fn on_event<F: Fn(Vec<Value>) + 'static>(&self, event: &str, f: F) {
        self.register_event_handler(event, Rc::new(move |args| {
            f(args);
            None
        }));
    }

    /// Convenience for async event handlers
    pub fn on_event_async<F, Fut>(&self, event: &str, f: F)
    where
        F: Fn(Vec<Value>) -> Fut + 'static,
        Fut: std::future::Future<Output = ()> + 'static,
    {
        self.register_event_handler(event, Rc::new(move |args| Some(Box::pin(f(args)) as LocalBoxFuture<'static, ()>)));
    }

    /// Schedules the event and returns a future that resolves when all
    /// handlers complete.  The future may be dropped without awaiting.
    pub fn send_event(&self, event: &str, args: Vec<Value>) -> impl std::future::Future<Output = ()> {
        let handlers: Vec<EventCallback> = self.events.borrow().get(event).cloned().unwrap_or_default();
        // nothing to schedule for an event without handlers
        let handle = (!handlers.is_empty()).then(|| {
            spawn(async move {
                let mut futs = Vec::new();
                for h in handlers {
                    if let Some(f) = h(args.clone()) {
                        futs.push(f);
                    }
                }
                if !futs.is_empty() {
                    futures::future::join_all(futs).await;
                }
            })
        });
        async move {
            if let Some(h) = handle {
                let _ = h.await;
            }
        }
    }

    pub fn register_remote_method(&self, method_name: &str, cb: crate::components::klippy_connection::RemoteMethod) -> Result<(), ServerError> {
        self.klippy().register_remote_method(method_name, cb, true)
    }

    pub fn get_host_info(&self) -> Value {
        json!({
            "hostname": crate::utils::hostname(),
            "address": self.host.borrow().clone(),
            "port": self.port.get(),
            "ssl_port": self.ssl_port.get(),
        })
    }

    pub fn get_klippy_info(&self) -> Value {
        self.klippy().klippy_info()
    }

    pub fn restart(self: &Rc<Self>, delay: Option<f64>) {
        let s = self.clone();
        spawn(async move {
            if let Some(d) = delay {
                tokio::time::sleep(std::time::Duration::from_secs_f64(d)).await;
            }
            s.stop_server("restart").await;
        });
    }

    pub async fn stop_server(self: &Rc<Self>, exit_reason: &str) {
        if !self.server_running.get() && !self.exit_reason.borrow().is_empty() {
            return;
        }
        self.server_running.set(false);
        let comps: Vec<(String, Rc<dyn LifecycleDyn>)> =
            self.lifecycle.borrow().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (_, comp) in comps.iter() {
            comp.clone().exit().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        self.app().close().await;
        self.wsm().close().await;
        let kc = self.klippy();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), kc.close_conn(true)).await;
        for (name, comp) in comps {
            if SERVER_COMPONENTS.contains(&name.as_str()) {
                continue;
            }
            comp.close_comp().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        *self.exit_reason.borrow_mut() = exit_reason.to_string();
        self.exited.set(true);
        self.exit_notify.notify_waiters();
    }

    async fn handle_info_request(&self, req: WebRequest) -> ApiResult {
        let raw = req.get_boolean_or("raw", false)?;
        let reg_dirs = match self.lookup_component::<crate::components::file_manager::FileManager>("file_manager") {
            Some(fm) => fm.get_registered_dirs(),
            None => Vec::new(),
        };
        let kc = self.klippy();
        let warnings: Vec<String> = if raw {
            self.get_warnings()
        } else {
            self.get_warnings().iter().map(|w| w.replace('\n', "<br/>")).collect()
        };
        Ok(json!({
            "klippy_connected": kc.is_connected(),
            "klippy_state": kc.state().as_str(),
            "components": self.component_names(),
            "failed_components": self.failed_components(),
            "registered_directories": reg_dirs,
            "warnings": warnings,
            "websocket_count": self.wsm().get_count(),
            "moonraker_version": self.get_app_arg("software_version").unwrap_or(Value::Null),
            "missing_klippy_requirements": kc.missing_requirements(),
            "api_version": [API_VERSION.0, API_VERSION.1, API_VERSION.2],
            "api_version_string": format!("{}.{}.{}", API_VERSION.0, API_VERSION.1, API_VERSION.2),
        }))
    }

    async fn handle_config_request(&self, _req: WebRequest) -> ApiResult {
        let config = self.config();
        let cfg_parent = crate::utils::resolve_path(&crate::utils::expand_user(&self.app_arg_str("config_file")))
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("/"));
        let mut files = Vec::new();
        for (fname, sections) in config.get_file_sections() {
            let path = PathBuf::from(&fname);
            let rel = match path.strip_prefix(&cfg_parent) {
                Ok(r) => r.to_string_lossy().to_string(),
                Err(_) => fname.clone(),
            };
            files.push(json!({"filename": rel, "sections": sections}));
        }
        Ok(json!({
            "config": config.get_parsed_config(),
            "orig": config.get_orig_config(),
            "files": files,
        }))
    }
}

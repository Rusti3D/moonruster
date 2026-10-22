// Provides updates for Klipper and Moonraker
// (port of moonraker/components/update_manager/{update_manager,base_deploy,app_deploy,common}.py)

pub mod common;
pub mod git_deploy;
pub mod net_deploy;
mod pk_enums;
pub mod python_deploy;
pub mod swu_deploy;
pub mod system_deploy;

use crate::common::*;
use crate::components::database::NamespaceWrapper;
use crate::components::http_client::HttpClient;
use crate::components::machine::Machine;
use crate::components::shell_command::{OutputCallback, ShellCommandFactory};
use crate::confighelper::{ConfigError, ConfigHelper};
use crate::pyfmt::py_repr;
use crate::server::{server, spawn, Component};
use async_trait::async_trait;
use common::*;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

const UPDATE_REFRESH_INTERVAL: f64 = 3600.0;
const PIPVER_CHECK_DAYS: i64 = 30;
const OPTION_OVERRIDES: [&str; 4] = ["channel", "pinned_commit", "refresh_interval", "report_anomalies"];

fn str_list(v: &[String]) -> String {
    py_repr(&json!(v))
}

// ***** Command Helper *****

pub struct CommandHelper {
    refresh_interval: f64,
    umdb: NamespaceWrapper,
    http: Rc<HttpClient>,
    cur_update_app: RefCell<Option<String>>,
    cur_update_id: Cell<Option<u64>>,
    full_update: Cell<bool>,
    full_complete: Cell<bool>,
    pending_service_restarts: RefCell<Vec<String>>,
    pkg_updater: RefCell<Option<Rc<system_deploy::PackageDeploy>>>,
    updaters: RefCell<IndexMap<String, Rc<dyn Deployer>>>,
}

impl CommandHelper {
    fn new(config: &ConfigHelper) -> Result<Rc<Self>, ConfigError> {
        let srv = server();
        let http: Rc<HttpClient> =
            srv.lookup_component("http_client").ok_or_else(|| ConfigHelper::error("Component (http_client) not found"))?;
        config.getboolean_dep("enable_repo_debug", false, true)?;
        if srv.is_debug_enabled() {
            log::warn!("UPDATE MANAGER: REPO DEBUG ENABLED");
        }
        let db = srv.database();
        db.register_local_namespace("update_manager", false, false).map_err(|e| ConfigHelper::error(e.message))?;
        let umdb = db.wrap_namespace("update_manager", true).map_err(|e| ConfigHelper::error(e.message))?;
        let refresh_interval = config.getint_or("refresh_interval", 168)? as f64 * 60.0 * 60.0;
        Ok(Rc::new(CommandHelper {
            refresh_interval,
            umdb,
            http,
            cur_update_app: RefCell::new(None),
            cur_update_id: Cell::new(None),
            full_update: Cell::new(false),
            full_complete: Cell::new(false),
            pending_service_restarts: RefCell::new(Vec::new()),
            pkg_updater: RefCell::new(None),
            updaters: RefCell::new(IndexMap::new()),
        }))
    }

    pub fn shell(&self) -> Rc<ShellCommandFactory> {
        server().lookup_component("shell_command").expect("shell_command")
    }

    pub fn http(&self) -> Rc<HttpClient> {
        self.http.clone()
    }

    pub fn umdb(&self) -> &NamespaceWrapper {
        &self.umdb
    }

    fn set_update_info(&self, app: &str, uid: u64, full: bool) {
        *self.cur_update_app.borrow_mut() = Some(app.to_string());
        self.cur_update_id.set(Some(uid));
        self.full_update.set(full);
        self.full_complete.set(!full);
        self.pending_service_restarts.borrow_mut().clear();
    }

    pub fn is_full_update(&self) -> bool {
        self.full_update.get()
    }

    pub fn add_pending_restart(&self, svc: &str) {
        let mut p = self.pending_service_restarts.borrow_mut();
        if !p.iter().any(|s| s == svc) {
            p.push(svc.to_string());
        }
    }

    pub fn remove_pending_restart(&self, svc: &str) {
        self.pending_service_restarts.borrow_mut().retain(|s| s != svc);
    }

    fn set_full_complete(&self, complete: bool) {
        self.full_complete.set(complete);
    }

    fn clear_update_info(&self) {
        *self.cur_update_app.borrow_mut() = None;
        self.cur_update_id.set(None);
        self.full_update.set(false);
        self.full_complete.set(false);
        self.pending_service_restarts.borrow_mut().clear();
    }

    fn needs_service_restart(&self, svc: &str) -> bool {
        self.pending_service_restarts.borrow().iter().any(|s| s == svc)
    }

    fn is_app_updating(&self, app: &str) -> bool {
        self.cur_update_app.borrow().as_deref() == Some(app) || self.full_update.get()
    }

    pub fn is_update_busy(&self) -> bool {
        self.cur_update_app.borrow().is_some()
    }

    /// Callback that forwards command output as update responses
    pub fn notify_cb(self: &Rc<Self>) -> OutputCallback {
        let c = Rc::downgrade(self);
        Rc::new(move |data: Vec<u8>| {
            if let Some(c) = c.upgrade() {
                c.notify_update_response(&String::from_utf8_lossy(&data), false);
            }
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn run_cmd(
        self: &Rc<Self>,
        cmd: &str,
        timeout: f64,
        notify: bool,
        attempts: usize,
        env: Option<HashMap<String, String>>,
        cwd: Option<String>,
        sig_idx: usize,
        log_stderr: bool,
    ) -> Result<(), ServerError> {
        let cb = if notify { Some(self.notify_cb()) } else { None };
        let log_stderr = log_stderr || server().is_verbose_enabled();
        self.shell()
            .run_cmd_async(cmd, cb, None, timeout, attempts, false, sig_idx, None, true, log_stderr, env, cwd, None)
            .await
            .map_err(|e| ServerError::new(e.message, 500))
    }

    pub async fn run_cmd_simple(self: &Rc<Self>, cmd: &str, timeout: f64, notify: bool, attempts: usize) -> Result<(), ServerError> {
        self.run_cmd(cmd, timeout, notify, attempts, None, None, 1, false).await
    }

    fn status_info(&self) -> Value {
        let mut vinfo = Map::new();
        let updaters: Vec<(String, Rc<dyn Deployer>)> =
            self.updaters.borrow().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (name, u) in updaters {
            vinfo.insert(name, u.get_update_status());
        }
        let mut uinfo = self.get_rate_limit_stats();
        uinfo.insert("version_info".into(), Value::Object(vinfo));
        uinfo.insert("busy".into(), json!(self.is_update_busy()));
        Value::Object(uinfo)
    }

    pub fn notify_update_refreshed(&self) {
        let _ = server().send_event("update_manager:update_refreshed", vec![self.status_info()]);
    }

    pub fn notify_update_refreshed_delayed(self: &Rc<Self>) {
        let c = self.clone();
        spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            c.notify_update_refreshed();
        });
    }

    pub fn notify_update_response(&self, resp: &str, is_complete: bool) {
        let app = match self.cur_update_app.borrow().clone() {
            None => return,
            Some(a) => a,
        };
        let mut done = is_complete;
        if self.full_update.get() {
            done &= self.full_complete.get();
        }
        let notification = json!({
            "message": resp.trim(),
            "application": app,
            "proc_id": self.cur_update_id.get(),
            "complete": done,
        });
        let _ = server().send_event("update_manager:update_response", vec![notification]);
    }

    pub async fn install_packages(&self, packages: &[String], timeout: f64, notify: bool) -> Result<(), ServerError> {
        let pu = self.pkg_updater.borrow().clone();
        match pu {
            None => Ok(()),
            Some(p) => p.install_packages(packages, timeout, 3, notify).await,
        }
    }

    pub fn get_rate_limit_stats(&self) -> Map<String, Value> {
        self.http.github_api_stats().as_object().cloned().unwrap_or_default()
    }

    pub fn on_download_progress(self: &Rc<Self>) -> crate::components::http_client::ProgressCallback {
        let c = Rc::downgrade(self);
        Rc::new(move |progress: i64, download_size: i64, downloaded: i64| {
            if let Some(c) = c.upgrade() {
                let app = c.cur_update_app.borrow().clone().unwrap_or_else(|| "None".into());
                c.notify_update_response(
                    &format!(
                        "Downloading {}: {} KiB / {} KiB [{}%]",
                        app,
                        downloaded.div_euclid(1024),
                        download_size.div_euclid(1024),
                        progress
                    ),
                    false,
                );
            }
        })
    }

    pub fn create_tempdir(&self, suffix: &str, prefix: &str) -> Result<PathBuf, ServerError> {
        let base = std::env::temp_dir();
        for _ in 0..100 {
            let name: String = (0..8).map(|_| (b'a' + rand::random::<u8>() % 26) as char).collect();
            let p = base.join(format!("{}{}{}", prefix, name, suffix));
            if std::fs::create_dir(&p).is_ok() {
                return Ok(p);
            }
        }
        Err(ServerError::new("Failed to create temporary directory", 500))
    }
}

// ***** Base Deploy *****

pub struct BaseDeploy {
    pub name: String,
    pub prefix: RefCell<String>,
    pub refresh_interval: f64,
    pub cfg_hash: String,
    pub last_refresh_time: Cell<f64>,
    pub last_cfg_hash: RefCell<String>,
    pub cmd_helper: Rc<CommandHelper>,
}

impl BaseDeploy {
    pub fn new(
        config: &ConfigHelper,
        cmd_helper: &Rc<CommandHelper>,
        name: Option<&str>,
        prefix: &str,
        cfg_hash: Option<&str>,
    ) -> Result<BaseDeploy, ConfigError> {
        let name = name.map(String::from).unwrap_or_else(|| Self::parse_name(config));
        let prefix = if prefix.is_empty() { String::new() } else { format!("{} {}: ", prefix, name) };
        let mut refresh_interval = cmd_helper.refresh_interval;
        if let Some(r) = config.getint_opt("refresh_interval", crate::confighelper::Bounds::none())? {
            refresh_interval = r as f64 * 60.0 * 60.0;
        }
        let cfg_hash = match cfg_hash {
            Some(h) => h.to_string(),
            None => {
                use sha2::Digest;
                hex::encode(config.get_hash().finalize())
            }
        };
        Ok(BaseDeploy {
            name,
            prefix: RefCell::new(prefix),
            refresh_interval,
            cfg_hash,
            last_refresh_time: Cell::new(0.0),
            last_cfg_hash: RefCell::new(String::new()),
            cmd_helper: cmd_helper.clone(),
        })
    }

    pub fn parse_name(config: &ConfigHelper) -> String {
        let n = config.get_name();
        let name = match n.split_once(char::is_whitespace) {
            Some((_, rest)) => rest.trim_start().to_string(),
            None => n.to_string(),
        };
        match name.strip_prefix("client ") {
            Some(n) => n.to_string(),
            None => name,
        }
    }

    pub fn initialize(&self) -> Map<String, Value> {
        let storage = self.cmd_helper.umdb.get(&self.name, json!({})).as_object().cloned().unwrap_or_default();
        self.last_refresh_time.set(storage.get("last_refresh_time").and_then(|v| v.as_f64()).unwrap_or(0.0));
        *self.last_cfg_hash.borrow_mut() =
            storage.get("last_config_hash").and_then(|v| v.as_str()).unwrap_or("").to_string();
        storage
    }

    pub fn needs_refresh(&self, log_remaining_time: bool) -> bool {
        let next = self.last_refresh_time.get() + self.refresh_interval;
        let remaining = (next - crate::utils::time_now() + 0.5) as i64;
        if self.cfg_hash != *self.last_cfg_hash.borrow() || remaining <= 0 {
            return true;
        }
        if log_remaining_time {
            self.log_info(&format!("Next refresh in: {}", pretty_print_time(remaining)));
        }
        false
    }

    pub fn persistent_data(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("last_config_hash".into(), json!(self.cfg_hash));
        m.insert("last_refresh_time".into(), json!(self.last_refresh_time.get()));
        m
    }

    pub fn log_exc(&self, msg: &str, traceback: bool) -> ServerError {
        let log_msg = format!("{}{}", self.prefix.borrow(), msg);
        if traceback {
            log::error!("{}", log_msg);
        } else {
            log::info!("{}", log_msg);
        }
        ServerError::bad(msg)
    }

    pub fn log_info(&self, msg: &str) {
        log::info!("{}{}", self.prefix.borrow(), msg);
    }

    pub fn log_debug(&self, msg: &str) {
        log::debug!("{}{}", self.prefix.borrow(), msg);
    }

    pub fn notify_status(&self, msg: &str, is_complete: bool) {
        let log_msg = format!("{}{}", self.prefix.borrow(), msg);
        log::debug!("{}", log_msg);
        self.cmd_helper.notify_update_response(&log_msg, is_complete);
    }
}

/// Saves the deployer's persistent state (BaseDeploy._save_state)
pub fn save_state(d: &dyn Deployer) {
    let b = d.base();
    b.last_refresh_time.set(crate::utils::time_now());
    *b.last_cfg_hash.borrow_mut() = b.cfg_hash.clone();
    let _ = b.cmd_helper.umdb.insert(&b.name, Value::Object(d.get_persistent_data()));
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DeployKind {
    Base,
    Git,
    Net,
    Python,
    System,
}

#[async_trait(?Send)]
pub trait Deployer {
    fn base(&self) -> &BaseDeploy;
    fn kind(&self) -> DeployKind;
    fn app(&self) -> Option<&AppDeploy> {
        None
    }
    async fn initialize(&self) -> Map<String, Value> {
        self.base().initialize()
    }
    async fn refresh(&self) -> Result<(), ServerError> {
        Ok(())
    }
    async fn update(&self) -> Result<bool, ServerError> {
        Ok(false)
    }
    async fn rollback(&self) -> Result<bool, ServerError> {
        Err(ServerError::bad(format!("Rollback not available for {}", self.base().name)))
    }
    async fn recover(&self, _hard: bool, _force_dep_update: bool) -> Result<(), ServerError> {
        Err(ServerError::exc("NotImplementedError"))
    }
    fn get_update_status(&self) -> Value {
        json!({})
    }
    fn get_persistent_data(&self) -> Map<String, Value> {
        self.base().persistent_data()
    }
    async fn close(&self) {}
}

pub struct PlainDeploy {
    base: BaseDeploy,
}

impl Deployer for PlainDeploy {
    fn base(&self) -> &BaseDeploy {
        &self.base
    }
    fn kind(&self) -> DeployKind {
        DeployKind::Base
    }
}

// ***** App Deploy *****

pub struct AppDeploy {
    pub base: BaseDeploy,
    pub config: ConfigHelper,
    pub type_: AppType,
    pub channel: Channel,
    pub channel_invalid: bool,
    pub report_anomalies: bool,
    pub is_valid: Cell<bool>,
    pub path: PathBuf,
    pub virtualenv: Option<PathBuf>,
    pub py_exec: Option<PathBuf>,
    pub pip_cmd: Option<String>,
    pub pip_version_info: RefCell<Option<PipVersionInfo>>,
    pub pip_ver_date: Cell<f64>,
    pub venv_args: Option<String>,
    pub pip_env_vars: Option<HashMap<String, String>>,
    pub npm_pkg_json: Option<PathBuf>,
    pub python_reqs: Option<PathBuf>,
    pub install_script: Option<PathBuf>,
    pub system_deps_json: Option<PathBuf>,
    pub info_tags: Vec<String>,
    pub managed_services: Vec<String>,
}

fn resolve(p: &Path) -> PathBuf {
    crate::utils::resolve_path(p)
}

impl AppDeploy {
    pub fn new(config: &ConfigHelper, cmd_helper: &Rc<CommandHelper>, prefix: &str) -> Result<AppDeploy, ConfigError> {
        let base = BaseDeploy::new(config, cmd_helper, None, prefix, None)?;
        let type_names: Vec<&str> = AppType::valid_types().iter().map(|t| t.name()).collect();
        let type_ = AppType::from_string(&config.getchoice("type", &type_names, None, true)?).unwrap();
        let chan_names = ["stable", "beta", "dev"];
        let mut channel =
            Channel::from_string(&config.getchoice("channel", &chan_names, Some(type_.default_channel().name()), true)?).unwrap();
        let mut channel_invalid = false;
        if !type_.supported_channels().contains(&channel) {
            let str_channels: Vec<String> = type_.supported_channels().iter().map(|c| c.name().to_string()).collect();
            channel_invalid = true;
            let invalid = channel;
            channel = type_.default_channel();
            server().add_warning(
                &format!(
                    "[{}]: Invalid value '{}' for option 'channel'. Type '{}' supports the following channels: {}.  Falling back to channel '{}'",
                    config.get_name(),
                    invalid.name(),
                    type_.name(),
                    str_list(&str_channels),
                    channel.name()
                ),
                None,
                true,
            );
        }
        let report_anomalies = config.getboolean_or("report_anomalies", true)?;
        let info_tags = config.getlist_opt("info_tags", Some(vec![]), Some("\n"))?.unwrap_or_default();
        Ok(AppDeploy {
            base,
            config: config.clone(),
            type_,
            channel,
            channel_invalid,
            report_anomalies,
            is_valid: Cell::new(false),
            path: PathBuf::new(),
            virtualenv: None,
            py_exec: None,
            pip_cmd: None,
            pip_version_info: RefCell::new(None),
            pip_ver_date: Cell::new(0.0),
            venv_args: None,
            pip_env_vars: None,
            npm_pkg_json: None,
            python_reqs: None,
            install_script: None,
            system_deps_json: None,
            info_tags,
            managed_services: Vec::new(),
        })
    }

    pub fn name(&self) -> &str {
        &self.base.name
    }

    pub fn configure_path(&mut self, config: &ConfigHelper, reserve: bool) -> Result<(), ConfigError> {
        self.path = resolve(&crate::utils::expand_user(&config.get("path")?));
        Self::verify_path(config, "path", &self.path, false, false)?;
        if reserve && !["moonraker", "klipper"].contains(&self.name()) && !self.path.join(".writeable").is_file() {
            if let Some(fm) = server().lookup_component::<crate::components::file_manager::FileManager>("file_manager") {
                fm.add_reserved_path(&format!("update_manager {}", self.name()), &self.path, true);
            }
        }
        Ok(())
    }

    pub fn configure_virtualenv(&mut self, config: &ConfigHelper) -> Result<(), ConfigError> {
        let mut venv_path: Option<PathBuf> = None;
        if config.has_option("virtualenv") {
            let mut vp = crate::utils::expand_user(&config.get("virtualenv")?);
            if !vp.is_absolute() {
                vp = self.path.join(vp);
            }
            Self::verify_path(config, "virtualenv", &vp, false, false)?;
            venv_path = Some(vp);
        } else if config.has_option("env") {
            if self.name() != "klipper" {
                self.base.log_info("Option 'env' is deprecated, use 'virtualenv' instead.");
            }
            let py_exec = crate::utils::expand_user(&config.get("env")?);
            Self::verify_path(config, "env", &py_exec, true, true)?;
            venv_path = py_exec.parent().and_then(|p| p.parent()).map(resolve);
        }
        if let Some(vp) = venv_path {
            if !vp.join("bin/activate").is_file() {
                return Err(ConfigHelper::error(format!(
                    "[{}]: Invalid virtualenv at path {}. Verify that the 'virtualenv' option is set to a valid virtualenv path.",
                    config.get_name(),
                    vp.display()
                )));
            }
            let py_exec = vp.join("bin/python");
            if !(py_exec.is_file() && crate::utils::access_exec(&py_exec)) {
                return Err(ConfigHelper::error(format!(
                    "[{}]: Invalid python executable at {}. Verify that the 'virtualenv' option is set to a valid virtualenv path.",
                    config.get_name(),
                    py_exec.display()
                )));
            }
            self.base.log_info(&format!("Detected virtualenv: {}", vp.display()));
            if vp.join("bin/pip").is_file() {
                self.pip_cmd = Some(format!("{} -m pip", py_exec.display()));
            } else {
                self.base.log_info("Unable to locate pip executable");
            }
            self.py_exec = Some(py_exec);
            self.virtualenv = Some(vp);
        }
        self.venv_args = config.get_opt("venv_args")?;
        let env = config.getdict("pip_environment_variables", Some(Value::Null), (Some("\n"), "="), false)?;
        self.pip_env_vars = env.as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), crate::pyfmt::py_str(v))).collect());
        Ok(())
    }

    pub fn configure_dependencies(&mut self, config: &ConfigHelper, node_only: bool) -> Result<(), ConfigError> {
        if config.getboolean_or("enable_node_updates", false)? {
            let p = self.path.join("package-lock.json");
            Self::verify_path(config, "enable_node_updates", &p, true, false)?;
            self.npm_pkg_json = Some(p);
        }
        if node_only {
            return Ok(());
        }
        if self.py_exec.is_some() {
            let p = self.path.join(config.get("requirements")?);
            Self::verify_path(config, "requirements", &p, true, false)?;
            self.python_reqs = Some(p);
        }
        if !self.configure_sysdeps(config)? {
            if let Some(script) = config.get_opt("install_script")? {
                let p = resolve(&self.path.join(script));
                Self::verify_path(config, "install_script", &p, true, false)?;
                self.install_script = Some(p);
            }
        }
        Ok(())
    }

    pub fn configure_sysdeps(&mut self, config: &ConfigHelper) -> Result<bool, ConfigError> {
        match config.get_opt("system_dependencies")? {
            Some(deps) => {
                let p = resolve(&self.path.join(deps));
                Self::verify_path(config, "system_dependencies", &p, true, false)?;
                self.system_deps_json = Some(p);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub fn configure_managed_services(&mut self, config: &ConfigHelper) -> Result<(), ConfigError> {
        let mut svc_default = vec![];
        if config.getboolean_or("is_system_service", true)? {
            svc_default.push(self.name().to_string());
        }
        let svc_choices = vec![self.name().to_string(), "klipper".to_string(), "moonraker".to_string()];
        let mut services = config.getlist_opt("managed_services", Some(svc_default), None)?.unwrap_or_default();
        let srv = server();
        if services.iter().any(|s| s == self.name()) {
            let machine: Option<Rc<Machine>> = srv.lookup_component("machine");
            let asvc = PathBuf::from(srv.data_path()).join("moonraker.asvc");
            if !machine.map(|m| m.is_service_allowed(self.name())).unwrap_or(false) {
                srv.add_warning(
                    &format!(
                        "[{}]: Moonraker is not permitted to restart service '{}'.  To enable management of this service add {} to the bottom of the file {}.  To disable management for this service set 'is_system_service: False' in the configuration for this section.",
                        config.get_name(),
                        self.name(),
                        self.name(),
                        asvc.display()
                    ),
                    None,
                    true,
                );
                services.clear();
            }
        }
        for svc in &services {
            if !svc_choices.contains(svc) {
                srv.add_warning(
                    &format!(
                        "[{}]: Option 'managed_services: {}' contains an invalid value '{}'.  All values must be one of the following choices: {}",
                        config.get_name(),
                        services.join(" "),
                        svc,
                        str_list(&svc_choices)
                    ),
                    None,
                    true,
                );
                break;
            }
        }
        for svc in &svc_choices {
            if services.contains(svc) && !self.managed_services.contains(svc) {
                self.managed_services.push(svc.clone());
            }
        }
        self.base.log_debug(&format!("Managed services: {}", str_list(&self.managed_services)));
        Ok(())
    }

    pub fn verify_path(config: &ConfigHelper, option: &str, path: &Path, check_file: bool, check_exe: bool) -> Result<(), ConfigError> {
        let base_msg = format!("Invalid path for option `{}` in section [{}]: Path `{}`", option, config.get_name(), path.display());
        if !path.exists() {
            return Err(ConfigHelper::error(format!("{} does not exist", base_msg)));
        }
        if check_file && !path.is_file() {
            return Err(ConfigHelper::error(format!("{} is not a file", base_msg)));
        }
        if check_exe && !crate::utils::access_exec(path) {
            return Err(ConfigHelper::error(format!("{} is not executable", base_msg)));
        }
        Ok(())
    }

    pub fn initialize(&self) -> Map<String, Value> {
        let storage = self.base.initialize();
        if let Some(pv) = storage.get("pip_version_info").and_then(|v| v.as_array()) {
            if pv.len() >= 3 {
                let info = PipVersionInfo {
                    pip_version_string: crate::pyfmt::py_str(&pv[0]),
                    python_version_string: crate::pyfmt::py_str(&pv[1]),
                };
                let ts = pv[2].as_f64().unwrap_or(0.0);
                self.pip_ver_date.set(ts);
                self.base.log_info(&format!(
                    "Pip Version: {}, Python Version: {}\nLast checked on {}",
                    info.pip_version_string,
                    info.python_version_string,
                    py_utc_datetime(ts)
                ));
                *self.pip_version_info.borrow_mut() = Some(info);
            }
        }
        storage
    }

    pub fn check_same_paths(&self, app_path: &Path, executable: &Path) -> bool {
        let py_exec = match &self.py_exec {
            None => return false,
            Some(p) => p,
        };
        let same = |a: &Path, b: &Path| match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        };
        same(&self.path, &crate::utils::expand_user(&app_path.to_string_lossy()))
            && same(py_exec, &crate::utils::expand_user(&executable.to_string_lossy()))
    }

    pub async fn restart_service(&self) -> Result<(), ServerError> {
        if self.managed_services.is_empty() {
            return Ok(());
        }
        let srv = server();
        let machine: Rc<Machine> = srv.lookup_component("machine").ok_or_else(|| ServerError::new("machine", 500))?;
        let is_full = self.base.cmd_helper.is_full_update();
        for svc in &self.managed_services {
            if is_full && svc != self.name() {
                self.base.notify_status(&format!("Service {} restart postponed...", svc), false);
                self.base.cmd_helper.add_pending_restart(svc);
                continue;
            }
            self.base.cmd_helper.remove_pending_restart(svc);
            self.base.notify_status(&format!("Restarting service {}...", svc), false);
            if svc == "moonraker" {
                machine.restart_moonraker_service();
            } else {
                let mut name = svc.clone();
                if svc == "klipper" {
                    name = srv.klippy().unit_name();
                }
                machine.do_service_action("restart", &name).await?;
            }
        }
        Ok(())
    }

    pub fn read_system_dependencies(&self) -> Vec<String> {
        if let Some(deps_json) = &self.system_deps_json {
            let parsed = std::fs::read(deps_json)
                .map_err(|e| e.to_string())
                .and_then(|d| serde_json::from_slice::<Value>(&d).map_err(|e| e.to_string()));
            return match parsed {
                Ok(info) => SysDepsParser::new().parse_dependencies(&info),
                Err(e) => {
                    log::error!("Error reading system deps: {}\n{}", deps_json.display(), e);
                    vec![]
                }
            };
        }
        let inst_path = match &self.install_script {
            None => return vec![],
            Some(p) => p,
        };
        if !inst_path.is_file() {
            self.base.log_info(&format!("Failed to open install script: {}", inst_path.display()));
            return vec![];
        }
        let data = match std::fs::read_to_string(inst_path) {
            Ok(d) => d,
            Err(e) => {
                log::error!("Error reading install script: {}\n{}", inst_path.display(), e);
                return vec![];
            }
        };
        let re = regex::Regex::new(r#"PKGLIST="(.*)""#).unwrap();
        let strip_chars: Vec<char> = "${PKGLIST}".chars().collect();
        let mut packages = Vec::new();
        for c in re.captures_iter(&data) {
            let line = c[1].trim_start_matches(|ch| strip_chars.contains(&ch)).trim();
            packages.extend(line.split_whitespace().map(String::from));
        }
        if packages.is_empty() {
            self.base.log_info(&format!("No packages found in script: {}", inst_path.display()));
        }
        packages
    }

    pub fn read_python_reqs(&self) -> Vec<String> {
        let reqs = match &self.python_reqs {
            None => return vec![],
            Some(p) => p,
        };
        if !reqs.is_file() {
            self.base.log_info(&format!("Failed to open python requirements file: {}", reqs.display()));
            return vec![];
        }
        read_requirements_file(reqs).unwrap_or_default()
    }

    pub fn get_update_status(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("channel".into(), json!(self.channel.name()));
        m.insert("debug_enabled".into(), json!(server().is_debug_enabled()));
        m.insert("channel_invalid".into(), json!(self.channel_invalid));
        m.insert("is_valid".into(), json!(self.is_valid.get()));
        m.insert("configured_type".into(), json!(self.type_.name()));
        m.insert("info_tags".into(), json!(self.info_tags));
        m
    }

    pub fn get_persistent_data(&self) -> Map<String, Value> {
        let mut storage = self.base.persistent_data();
        storage.insert("is_valid".into(), json!(self.is_valid.get()));
        let pv = self.pip_version_info.borrow().as_ref().map(|p| {
            json!([p.pip_version_string, p.python_version_string, self.pip_ver_date.get()])
        });
        storage.insert("pip_version_info".into(), pv.unwrap_or(Value::Null));
        storage
    }

    fn get_file_hash(filename: Option<&PathBuf>) -> Option<String> {
        use sha2::Digest;
        let f = filename?;
        if !f.is_file() {
            return None;
        }
        std::fs::read(f).ok().map(|d| hex::encode(sha2::Sha256::digest(&d)))
    }

    async fn install_packages(&self, packages: &[String]) {
        self.base.notify_status("Installing system dependencies...", false);
        if let Err(e) = self.base.cmd_helper.install_packages(packages, 3600.0, true).await {
            self.base.log_exc(&format!("Error updating packages\n{}", e.message), true);
        }
    }

    pub async fn update_python_requirements(&self, requirements: PipPackages<'_>) {
        let pip_cmd = match &self.pip_cmd {
            None => return,
            Some(p) => p.clone(),
        };
        let pip_exec = AsyncPipExecutor::new(&pip_cmd, Some(self.base.cmd_helper.notify_cb()));
        self.update_pip(&pip_exec).await;
        self.base.notify_status("Updating python packages...", false);
        if let Err(e) = pip_exec.install_packages(&requirements, self.pip_env_vars.as_ref()).await {
            self.base.log_exc(&format!("Error updating python requirements\n{}", e.message), true);
        }
    }

    pub async fn update_pip(&self, pip_exec: &AsyncPipExecutor) {
        self.base.notify_status("Checking pip version...", false);
        let now = crate::utils::time_now();
        let check_days = ((now - self.pip_ver_date.get()) / 86400.0).floor() as i64;
        let res: Result<(), ServerError> = async {
            if self.pip_version_info.borrow().is_none() || check_days > PIPVER_CHECK_DAYS {
                let info = pip_exec.get_pip_version().await?;
                *self.pip_version_info.borrow_mut() = Some(info);
                self.pip_ver_date.set(crate::utils::time_now());
            }
            let info = self.pip_version_info.borrow().clone().unwrap();
            if info.needs_pip_update() {
                self.base.notify_status(
                    &format!(
                        "Updating pip from version {} to {}...",
                        info.pip_version_string,
                        info.max_pip_version_string()
                    ),
                    false,
                );
                pip_exec.update_pip().await;
                let info = pip_exec.get_pip_version().await?;
                *self.pip_version_info.borrow_mut() = Some(info);
                self.pip_ver_date.set(crate::utils::time_now());
            } else {
                self.base.notify_status("Pip version up to date", false);
            }
            Ok(())
        }
        .await;
        if let Err(e) = res {
            self.base.notify_status(&format!("Pip Version Check Error: {}", e.message), false);
            self.base.log_exc("Pip Version Check Error", true);
        }
    }

    pub fn collect_dependency_info(&self) -> Value {
        let pkg_deps = self.read_system_dependencies();
        let pyreqs = self.read_python_reqs();
        let npm_hash = Self::get_file_hash(self.npm_pkg_json.as_ref());
        log::debug!(
            "\nApplication {}: Pre-update dependencies:\nPackages: {}\nPython Requirements: {}",
            self.name(),
            str_list(&pkg_deps),
            str_list(&pyreqs)
        );
        json!({"system_packages": pkg_deps, "python_modules": pyreqs, "npm_hash": npm_hash})
    }

    pub async fn update_dependencies(&self, dep_info: &Value, force: bool) {
        let mut packages = self.read_system_dependencies();
        let mut modules = self.read_python_reqs();
        log::debug!(
            "\nApplication {}: Post-update dependencies:\nPackages: {}\nPython Requirements: {}",
            self.name(),
            str_list(&packages),
            str_list(&modules)
        );
        if !force {
            let prev = |k: &str| -> Vec<String> {
                dep_info.get(k).and_then(|v| v.as_array()).map(|a| a.iter().map(crate::pyfmt::py_str).collect()).unwrap_or_default()
            };
            let pp = prev("system_packages");
            let pm = prev("python_modules");
            let mut seen = Vec::new();
            packages.retain(|p| !pp.contains(p) && !seen.contains(p) && {
                seen.push(p.clone());
                true
            });
            let mut seen = Vec::new();
            modules.retain(|m| !pm.contains(m) && !seen.contains(m) && {
                seen.push(m.clone());
                true
            });
        }
        log::debug!(
            "\nApplication {}: Dependencies to install:\nPackages: {}\nPython Requirements: {}\nForce All: {}",
            self.name(),
            str_list(&packages),
            str_list(&modules),
            crate::pyfmt::py_bool(force)
        );
        if !packages.is_empty() {
            self.install_packages(&packages).await;
        }
        if !modules.is_empty() {
            match &self.python_reqs {
                Some(p) => self.update_python_requirements(PipPackages::File(p)).await,
                None => self.update_python_requirements(PipPackages::List(&modules)).await,
            }
        }
        let npm_hash = dep_info.get("npm_hash").and_then(|v| v.as_str()).map(String::from);
        let cur_hash = Self::get_file_hash(self.npm_pkg_json.as_ref());
        let need = matches!((&npm_hash, &cur_hash), (Some(a), Some(b)) if a != b);
        if force || need {
            if self.npm_pkg_json.is_some() {
                self.base.notify_status("Updating Node Packages...", false);
                let r = self
                    .base
                    .cmd_helper
                    .run_cmd("npm ci --only=prod", 600.0, true, 1, None, Some(self.path.display().to_string()), 1, false)
                    .await;
                if r.is_err() {
                    self.base.notify_status("Node Package Update failed", false);
                }
            }
        }
    }
}

/// str(datetime.fromtimestamp(ts, timezone.utc))
pub fn py_utc_datetime(ts: f64) -> String {
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(ts.floor() as i64, ((ts.fract()) * 1e9) as u32).unwrap_or_default();
    if dt.timestamp_subsec_micros() == 0 {
        dt.format("%Y-%m-%d %H:%M:%S+00:00").to_string()
    } else {
        dt.format("%Y-%m-%d %H:%M:%S%.6f+00:00").to_string()
    }
}

// ***** Base configuration *****

fn get_base_configuration(config: &ConfigHelper) -> Result<ConfigHelper, ConfigError> {
    let kconn = server().klippy();
    let kpath = kconn.path();
    let mut base: IndexMap<String, IndexMap<String, String>> = IndexMap::new();
    let mut m = IndexMap::new();
    m.insert("origin".into(), "https://github.com/arksine/moonraker.git".into());
    m.insert("requirements".into(), "scripts/moonraker-requirements.txt".into());
    m.insert("venv_args".into(), "-p python3".into());
    m.insert("system_dependencies".into(), "scripts/system-dependencies.json".into());
    m.insert("virtualenv".into(), String::new());
    m.insert("pip_environment_variables".into(), "SKIP_CYTHON=Y".into());
    m.insert("path".into(), String::new());
    m.insert("managed_services".into(), "moonraker".into());
    // Moonruster is not a python source tree, it is never self updated
    m.insert("type".into(), AppType::None.name().into());
    base.insert("moonraker".into(), m);
    let mut k = IndexMap::new();
    k.insert("moved_origin".into(), "https://github.com/kevinoconnor/klipper.git".into());
    k.insert("origin".into(), "https://github.com/Klipper3d/klipper.git".into());
    k.insert("requirements".into(), "scripts/klippy-requirements.txt".into());
    k.insert("venv_args".into(), "-p python3".into());
    k.insert("install_script".into(), "scripts/install-octopi.sh".into());
    k.insert("managed_services".into(), "klipper".into());
    k.insert("path".into(), kpath.display().to_string());
    k.insert("env".into(), kconn.executable().display().to_string());
    k.insert("type".into(), AppType::detect(Some(&kpath)).name().into());
    base.insert("klipper".into(), k);
    let default_channel = config.get_opt("channel")?;
    for (app_name, opts) in base.iter_mut() {
        if let Some(c) = &default_channel {
            opts.insert("channel".into(), c.clone());
        }
        let section = format!("update_manager {}", app_name);
        if !config.has_section(&section) {
            continue;
        }
        let app_cfg = config.getsection(&section);
        for opt in OPTION_OVERRIDES {
            if app_cfg.has_option(opt) {
                opts.insert(opt.into(), app_cfg.get(opt)?);
            }
        }
    }
    config.read_supplemental_dict(&json!(base))
}

fn get_deploy(
    app_type: &str,
    cfg: &ConfigHelper,
    cmd_helper: &Rc<CommandHelper>,
) -> Result<Option<Rc<dyn Deployer>>, ConfigError> {
    let t = AppType::from_string(app_type);
    Ok(Some(match t {
        Some(AppType::Web) | Some(AppType::Zip) | Some(AppType::Executable) => {
            Rc::new(net_deploy::NetDeploy::new(cfg, cmd_helper)?) as Rc<dyn Deployer>
        }
        Some(AppType::GitRepo) => Rc::new(git_deploy::GitDeploy::new(cfg, cmd_helper)?),
        Some(AppType::Python) => Rc::new(python_deploy::PythonDeploy::new(cfg, cmd_helper)?),
        Some(AppType::Swupdate) => Rc::new(swu_deploy::SwuDeploy::new(cfg, cmd_helper)?),
        _ => return Ok(None),
    }))
}

fn plain(cfg: &ConfigHelper, cmd_helper: &Rc<CommandHelper>) -> Result<Rc<dyn Deployer>, ConfigError> {
    Ok(Rc::new(PlainDeploy { base: BaseDeploy::new(cfg, cmd_helper, None, "", None)? }))
}

// ***** Instance tracking *****

struct FileLock {
    lock_path: PathBuf,
    fd: i32,
}

impl FileLock {
    /// AsyncExclusiveFileLock.acquire
    async fn acquire(file_path: &Path, timeout: f64) -> Result<Option<FileLock>, ServerError> {
        let name = file_path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let lock_path = file_path.parent().unwrap_or(Path::new("/")).join(format!(".{}.lock", name));
        let start = std::time::Instant::now();
        let mut logged = false;
        loop {
            let cpath = std::ffi::CString::new(lock_path.to_string_lossy().as_bytes()).unwrap();
            let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC, 0o644) };
            if fd < 0 {
                log::info!(
                    "Failed to acquire advisory lock, allowing unlocked entry.Error: {}",
                    std::io::Error::last_os_error()
                );
                return Ok(None);
            }
            unsafe { libc::fchmod(fd, 0o644) };
            if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                let mut st: libc::stat = unsafe { std::mem::zeroed() };
                unsafe { libc::fstat(fd, &mut st) };
                if st.st_nlink != 0 {
                    return Ok(Some(FileLock { lock_path, fd }));
                }
            }
            unsafe { libc::close(fd) };
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            if !logged {
                logged = true;
                log::info!("File lock {} is currently acquired by another process, waiting for release.", lock_path.display());
            }
            // mirrors the upstream check, which times out on the first wait
            if timeout > 0.0 && timeout >= start.elapsed().as_secs_f64() {
                return Err(ServerError::bad(format!("Attempt to acquire lock '{}' timed out", lock_path.display())));
            }
        }
    }

    fn release(self) {
        let _ = std::fs::remove_file(&self.lock_path);
        unsafe {
            libc::flock(self.fd, libc::LOCK_UN);
            libc::close(self.fd);
        }
    }
}

struct InstanceTracker {
    inst_id: RefCell<String>,
    inst_file_path: PathBuf,
}

impl InstanceTracker {
    fn new() -> Self {
        InstanceTracker { inst_id: RefCell::new(String::new()), inst_file_path: std::env::temp_dir().join("moonraker_instance_ids") }
    }

    fn get_instance_id(&self) -> String {
        let srv = server();
        let machine: Option<Rc<Machine>> = srv.lookup_component("machine");
        let cur_name: String = machine.map(|m| m.unit_name()).unwrap_or_default().split_whitespace().collect();
        let uuid = srv.get_app_args().get("instance_uuid").map(crate::pyfmt::py_str).unwrap_or_default();
        format!("{}:{}:{}", cur_name, uuid, std::process::id())
    }

    fn read_ids(&self) -> Vec<String> {
        std::fs::read_to_string(&self.inst_file_path)
            .map(|d| d.trim().lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
            .unwrap_or_default()
    }

    async fn set_instance_id(&self) {
        match FileLock::acquire(&self.inst_file_path, 2.0).await {
            Err(e) => log::info!("{}", e.message),
            Ok(lock) => {
                *self.inst_id.borrow_mut() = self.get_instance_id();
                let mut iids = self.read_ids();
                let id = self.inst_id.borrow().clone();
                if !iids.contains(&id) {
                    iids.push(id);
                }
                let s = iids.join("\n");
                if iids.len() > 1 {
                    server().add_log_rollover_item(
                        "um_multi_instance_msg",
                        &format!("Multiple instances of Moonraker have the update manager enabled.\n{}", s),
                        true,
                    );
                }
                if let Err(e) = std::fs::write(&self.inst_file_path, s) {
                    log::error!("Failed to set instance id\n{}", e);
                }
                if let Some(l) = lock {
                    l.release();
                }
            }
        }
    }

    async fn close(&self) {
        match FileLock::acquire(&self.inst_file_path, 2.0).await {
            Err(e) => log::info!("{}", e.message),
            Ok(lock) => {
                let mut iids = self.read_ids();
                let id = self.inst_id.borrow().clone();
                iids.retain(|i| *i != id);
                if let Err(e) = std::fs::write(&self.inst_file_path, iids.join("\n")) {
                    log::error!("Failed to remove instance id\n{}", e);
                }
                if let Some(l) = lock {
                    l.release();
                }
            }
        }
    }
}

// ***** Update Manager *****

pub struct UpdateManager {
    instance_tracker: InstanceTracker,
    app_config: ConfigHelper,
    refresh_window: (i64, i64),
    cmd_helper: Rc<CommandHelper>,
    cmd_request_lock: tokio::sync::Mutex<()>,
    initial_refresh_complete: Cell<bool>,
    klippy_identified_evt: RefCell<Option<Rc<crate::utils::AsyncEvent>>>,
    auto_refresh_enabled: bool,
    refresh_timer: RefCell<Option<tokio::task::JoinHandle<()>>>,
}

impl UpdateManager {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, ConfigError> {
        let srv = server();
        let app_config = get_base_configuration(config)?;
        let auto_refresh_enabled = config.getboolean_or("enable_auto_refresh", false)?;
        let rw = config.getintlist_opt("refresh_window", Some(vec![0, 5]), Some("-"), Some(2))?.unwrap_or(vec![0, 5]);
        if !(0..=23).contains(&rw[0]) || !(0..=23).contains(&rw[1]) {
            return Err(ConfigHelper::error("The hours specified in 'refresh_window' must be between 0 and 23."));
        }
        if rw[0] == rw[1] {
            return Err(ConfigHelper::error("The start and end hours specified in 'refresh_window' cannot be the same."));
        }
        let cmd_helper = CommandHelper::new(config)?;
        let mut updaters: IndexMap<String, Rc<dyn Deployer>> = IndexMap::new();
        if config.getboolean_or("enable_system_updates", true)? {
            let pd = system_deploy::PackageDeploy::new(config, &cmd_helper)?;
            *cmd_helper.pkg_updater.borrow_mut() = Some(pd.clone());
            updaters.insert("system".into(), pd);
        }
        let mcfg = app_config.getsection("moonraker");
        let kcfg = app_config.getsection("klipper");
        let mtype = mcfg.get("type")?;
        let m = match get_deploy(&mtype, &mcfg, &cmd_helper)? {
            Some(d) => d,
            None => plain(&mcfg, &cmd_helper)?,
        };
        updaters.insert("moonraker".into(), m);
        let mut kdeploy = None;
        if Path::new(&kcfg.get("path")?).exists() && Path::new(&kcfg.get("env")?).exists() {
            kdeploy = get_deploy(&kcfg.get("type")?, &kcfg, &cmd_helper)?;
        }
        let k = match kdeploy {
            Some(d) => d,
            None => plain(&kcfg, &cmd_helper)?,
        };
        updaters.insert("klipper".into(), k);
        if config.get_opt("client_repo")?.is_some() || config.get_opt("client_path")?.is_some() {
            return Err(ConfigHelper::error(
                "The deprecated 'client_repo' and 'client_path' options\nhave been removed.  See Moonraker's configuration docs\nfor details on client configuration.",
            ));
        }
        for section in config.get_prefix_sections("update_manager ") {
            let cfg = config.getsection(&section);
            let name = BaseDeploy::parse_name(&cfg);
            if updaters.contains_key(&name) {
                if name != "klipper" && name != "moonraker" {
                    srv.add_warning(&format!("[update_manager]: Extension {} already added", name), None, true);
                }
                continue;
            }
            let res: Result<(), ConfigError> = (|| {
                let client_type = cfg.get("type")?;
                match get_deploy(&client_type, &cfg, &cmd_helper)? {
                    None => {
                        srv.add_warning(&format!("Invalid type '{}' for section [{}]", client_type, section), None, true);
                    }
                    Some(d) => {
                        updaters.insert(name.clone(), d);
                    }
                }
                Ok(())
            })();
            if let Err(e) = res {
                srv.add_warning(&format!("[update_manager]: Failed to load extension {}: {}", name, e.0), None, true);
            }
        }
        *cmd_helper.updaters.borrow_mut() = updaters;
        let um = Rc::new(UpdateManager {
            instance_tracker: InstanceTracker::new(),
            app_config,
            refresh_window: (rw[0], rw[1]),
            cmd_helper,
            cmd_request_lock: tokio::sync::Mutex::new(()),
            initial_refresh_complete: Cell::new(false),
            klippy_identified_evt: RefCell::new(None),
            auto_refresh_enabled,
            refresh_timer: RefCell::new(None),
        });
        for ep in ["moonraker", "klipper", "system", "client", "upgrade"] {
            let u = um.clone();
            let _ = srv.register_endpoint(
                &format!("/machine/update/{}", ep),
                RequestType::POST,
                callback(move |req| {
                    let u = u.clone();
                    async move { u.handle_update_request(req).await }
                }),
            );
        }
        macro_rules! ep {
            ($path:expr, $rt:expr, $method:ident) => {{
                let u = um.clone();
                let _ = srv.register_endpoint(
                    $path,
                    $rt,
                    callback(move |req| {
                        let u = u.clone();
                        async move { u.$method(req).await }
                    }),
                );
            }};
        }
        // registration order matches upstream
        ep!("/machine/update/full", RequestType::POST, handle_full_update_request);
        ep!("/machine/update/status", RequestType::GET, handle_status_request);
        ep!("/machine/update/refresh", RequestType::POST, handle_refresh_request);
        ep!("/machine/update/recover", RequestType::POST, handle_repo_recovery);
        ep!("/machine/update/rollback", RequestType::POST, handle_rollback);
        srv.register_notification("update_manager:update_response", None);
        srv.register_notification("update_manager:update_refreshed", None);
        let u = Rc::downgrade(&um);
        srv.on_event("server:klippy_identified", move |_| {
            if let Some(u) = u.upgrade() {
                u.set_klipper_repo();
            }
        });
        Ok(um)
    }

    pub fn register_updater(&self, name: &str, config: Value) -> Result<(), ServerError> {
        if self.get_updater(name).is_some() {
            return Err(ServerError::bad(format!("Updater {} already registered", name)));
        }
        let mut m = Map::new();
        m.insert(name.to_string(), config);
        let cfg = self
            .app_config
            .read_supplemental_dict(&Value::Object(m))
            .map_err(|e| ServerError::bad(e.0))?
            .getsection(name);
        let utype = cfg.get("type").map_err(|e| ServerError::bad(e.0))?;
        let d = get_deploy(&utype, &cfg, &self.cmd_helper)
            .map_err(|e| ServerError::bad(e.0))?
            .ok_or_else(|| ServerError::bad(format!("Invalid type '{}'", utype)))?;
        self.cmd_helper.updaters.borrow_mut().insert(name.to_string(), d);
        Ok(())
    }

    pub async fn refresh_updater(&self, name: &str, force: bool) {
        let updater = match self.get_updater(name) {
            None => return,
            Some(u) => u,
        };
        let _g = self.cmd_request_lock.lock().await;
        if updater.base().needs_refresh(false) || force {
            let _ = updater.refresh().await;
        }
    }

    fn updaters(&self) -> Vec<(String, Rc<dyn Deployer>)> {
        self.cmd_helper.updaters.borrow().iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    fn get_updater(&self, name: &str) -> Option<Rc<dyn Deployer>> {
        self.cmd_helper.updaters.borrow().get(name).cloned()
    }

    fn set_klipper_repo(self: &Rc<Self>) {
        if let Some(evt) = self.klippy_identified_evt.borrow().as_ref() {
            evt.set();
        }
        let kconn = server().klippy();
        let kpath = kconn.path();
        let kexec = kconn.executable();
        let kupdater = self.get_updater("klipper");
        let app_type = AppType::detect(Some(&kpath));
        let is_app = kupdater.as_ref().and_then(|k| k.app().map(|_| ())).is_some();
        if let Some(k) = &kupdater {
            if let Some(a) = k.app() {
                if a.check_same_paths(&kpath, &kexec) {
                    return;
                }
            } else if app_type == AppType::None && k.kind() == DeployKind::Base {
                return;
            }
        }
        let kcfg = self.app_config.getsection("klipper");
        let _ = kcfg.set_option("path", &kpath.display().to_string());
        let _ = kcfg.set_option("env", &kexec.display().to_string());
        let _ = kcfg.set_option("type", app_type.name());
        let notify = !is_app;
        let new_updater = match get_deploy(app_type.name(), &kcfg, &self.cmd_helper) {
            Ok(Some(d)) => d,
            Ok(None) => match plain(&kcfg, &self.cmd_helper) {
                Ok(d) => d,
                Err(e) => {
                    log::error!("{}", e.0);
                    return;
                }
            },
            Err(e) => {
                log::error!("{}", e.0);
                return;
            }
        };
        let u = self.clone();
        spawn(async move {
            {
                let _g = u.cmd_request_lock.lock().await;
                u.cmd_helper.updaters.borrow_mut().insert("klipper".into(), new_updater.clone());
                let _ = u.cmd_helper.umdb.pop("klipper", Some(Value::Null));
                new_updater.initialize().await;
                let _ = new_updater.refresh().await;
            }
            if notify {
                u.cmd_helper.notify_update_refreshed();
            }
        });
    }

    fn is_within_refresh_window(&self) -> bool {
        use chrono::Timelike;
        let cur_hour = chrono::Local::now().hour() as i64;
        let (start, end) = self.refresh_window;
        if start < end {
            return start <= cur_hour && cur_hour < end;
        }
        cur_hour >= start || cur_hour < end
    }

    async fn handle_auto_refresh(self: &Rc<Self>) {
        let srv = server();
        let mut log_remaining_time = true;
        if self.initial_refresh_complete.get() {
            log_remaining_time = false;
            if !self.is_within_refresh_window() {
                log::debug!("update_manager: current time is outside of the refresh window, auto refresh rescheduled");
                return;
            }
            if srv.klippy().is_printing() {
                log::info!("Klippy is printing, auto refresh aborted");
                return;
            }
        }
        let mut need_notify = false;
        let machine: Option<Rc<Machine>> = srv.lookup_component("machine");
        if machine.map(|m| m.validation_enabled()).unwrap_or(false) {
            log::info!("update_manger: Install validation pending, bypassing initial refresh");
            self.initial_refresh_complete.set(true);
            return;
        }
        {
            let _g = self.cmd_request_lock.lock().await;
            for (_, updater) in self.updaters() {
                if updater.base().needs_refresh(log_remaining_time) {
                    if let Err(e) = updater.refresh().await {
                        log::error!("Unable to Refresh Status\n{}", e.message);
                        self.initial_refresh_complete.set(true);
                        return;
                    }
                    need_notify = true;
                }
            }
            self.initial_refresh_complete.set(true);
        }
        if need_notify {
            self.cmd_helper.notify_update_refreshed();
        }
    }

    async fn handle_update_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let srv = server();
        if srv.klippy().is_printing() {
            return Err(ServerError::new("Update Refused: Klippy is printing", 503));
        }
        let mut app = req.get_endpoint().rsplit('/').next().unwrap_or("").to_string();
        if app == "upgrade" || app == "client" {
            match req.get_str_opt("name") {
                None => return self.handle_full_update_request(req).await,
                Some(n) => app = n,
            }
        }
        if self.cmd_helper.is_app_updating(&app) {
            return Err(ServerError::new(format!("Item {} is currently updating", app), 503));
        }
        let updater = self.get_updater(&app).ok_or_else(|| ServerError::new(format!("Updater {} not available", app), 404))?;
        let _g = self.cmd_request_lock.lock().await;
        self.cmd_helper.set_update_info(&app, crate::components::websockets::next_uid(), false);
        let res = updater.update().await;
        if let Err(e) = &res {
            self.cmd_helper.notify_update_response(&format!("Error updating {}: {}", app, e.message), true);
        }
        self.cmd_helper.clear_update_info();
        res?;
        Ok(json!("ok"))
    }

    async fn handle_full_update_request(self: Rc<Self>, _req: WebRequest) -> ApiResult {
        let _g = self.cmd_request_lock.lock().await;
        let ch = self.cmd_helper.clone();
        ch.set_update_info("full", crate::components::websockets::next_uid(), true);
        ch.notify_update_response("Preparing full software update...", false);
        let mut app_name = String::new();
        let res: Result<(), ServerError> = async {
            if let Some(sys) = self.get_updater("system") {
                app_name = "system".into();
                sys.update().await?;
            }
            for (name, updater) in self.updaters() {
                if ["klipper", "moonraker", "system"].contains(&name.as_str()) {
                    continue;
                }
                app_name = name.clone();
                updater.update().await?;
            }
            app_name = "klipper".into();
            if let Some(k) = self.get_updater("klipper") {
                if let Some(app) = k.app() {
                    let evt = crate::utils::AsyncEvent::new();
                    *self.klippy_identified_evt.borrow_mut() = Some(evt.clone());
                    let mut check_restart = k.update().await?;
                    if ch.needs_service_restart(&app_name) {
                        app.restart_service().await?;
                        check_restart = true;
                    }
                    if check_restart {
                        ch.notify_update_response("Waiting for Klippy to reconnect (this may take up to 2 minutes)...", false);
                        if tokio::time::timeout(std::time::Duration::from_secs(120), evt.wait()).await.is_err() {
                            ch.notify_update_response("Klippy reconnect timed out...", false);
                        } else {
                            ch.notify_update_response("Klippy Reconnected", false);
                        }
                        *self.klippy_identified_evt.borrow_mut() = None;
                    }
                }
            }
            app_name = "moonraker".into();
            if let Some(m) = self.get_updater("moonraker") {
                m.update().await?;
                if ch.needs_service_restart(&app_name) {
                    if let Some(app) = m.app() {
                        app.restart_service().await?;
                    }
                }
            }
            ch.set_full_complete(true);
            ch.notify_update_response("Full Update Complete", true);
            Ok(())
        }
        .await;
        if let Err(e) = &res {
            ch.set_full_complete(true);
            ch.notify_update_response(&format!("Error updating {}: {}", app_name, e.message), true);
        }
        ch.clear_update_info();
        res?;
        Ok(json!("ok"))
    }

    fn is_busy(&self) -> bool {
        let srv = server();
        let machine: Option<Rc<Machine>> = srv.lookup_component("machine");
        machine.map(|m| m.validation_enabled()).unwrap_or(false)
            || self.cmd_helper.is_update_busy()
            || srv.klippy().is_printing()
            || !self.initial_refresh_complete.get()
    }

    async fn handle_status_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let mut check_refresh = req.get_boolean_or("refresh", false)?;
        if self.is_busy() {
            if check_refresh {
                log::info!("update_manager: bypassing refresh request");
            }
            check_refresh = false;
        }
        let mut guard = None;
        if check_refresh {
            guard = Some(self.cmd_request_lock.lock().await);
            let lrt = self.updaters().iter().map(|(_, u)| u.base().last_refresh_time.get()).fold(f64::MIN, f64::max);
            if crate::utils::time_now() < lrt + 60.0 {
                log::debug!("update_manager: refresh bypassed due to spam");
                check_refresh = false;
                guard = None;
            }
        }
        let mut vinfo = Map::new();
        for (name, updater) in self.updaters() {
            if check_refresh {
                updater.refresh().await?;
            }
            vinfo.insert(name, updater.get_update_status());
        }
        drop(guard);
        let mut ret = self.cmd_helper.get_rate_limit_stats();
        ret.insert("version_info".into(), Value::Object(vinfo));
        ret.insert("busy".into(), json!(self.cmd_helper.is_update_busy()));
        if check_refresh {
            self.cmd_helper.notify_update_refreshed_delayed();
        }
        Ok(Value::Object(ret))
    }

    async fn handle_refresh_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let name = req.get_str_opt("name");
        if let Some(n) = &name {
            if self.get_updater(n).is_none() {
                return Err(ServerError::bad(format!("No updater registered for '{}'", n)));
            }
        }
        if self.is_busy() {
            return Err(ServerError::new("Server is busy, cannot perform refresh", 503));
        }
        let _g = self.cmd_request_lock.lock().await;
        let mut vinfo = Map::new();
        for (uname, updater) in self.updaters() {
            if name.is_none() || name.as_deref() == Some(uname.as_str()) {
                updater.refresh().await?;
            }
            vinfo.insert(uname, updater.get_update_status());
        }
        let mut ret = self.cmd_helper.get_rate_limit_stats();
        ret.insert("version_info".into(), Value::Object(vinfo));
        ret.insert("busy".into(), json!(self.cmd_helper.is_update_busy()));
        self.cmd_helper.notify_update_refreshed_delayed();
        Ok(Value::Object(ret))
    }

    async fn handle_repo_recovery(self: Rc<Self>, req: WebRequest) -> ApiResult {
        if server().klippy().is_printing() {
            return Err(ServerError::bad("Recovery Attempt Refused: Klippy is printing"));
        }
        let app = req.get_str("name")?;
        let hard = req.get_boolean_or("hard", false)?;
        let update_deps = req.get_boolean_or("update_deps", false)?;
        let updater = self.get_updater(&app).ok_or_else(|| ServerError::new(format!("Updater {} not available", app), 404))?;
        if updater.kind() != DeployKind::Git {
            return Err(ServerError::bad(format!("Updater {} is not a Git Repo Type", app)));
        }
        let _g = self.cmd_request_lock.lock().await;
        self.cmd_helper.set_update_info(&format!("recover_{}", app), crate::components::websockets::next_uid(), false);
        let res = updater.recover(hard, update_deps).await;
        if let Err(e) = &res {
            self.cmd_helper.notify_update_response(&format!("Error Recovering {}", app), false);
            self.cmd_helper.notify_update_response(&e.message, true);
        }
        self.cmd_helper.clear_update_info();
        res?;
        Ok(json!("ok"))
    }

    async fn handle_rollback(self: Rc<Self>, req: WebRequest) -> ApiResult {
        if server().klippy().is_printing() {
            return Err(ServerError::bad("Rollback Attempt Refused: Klippy is printing"));
        }
        let app = req.get_str("name")?;
        let updater = self.get_updater(&app).ok_or_else(|| ServerError::new(format!("Updater {} not available", app), 404))?;
        let _g = self.cmd_request_lock.lock().await;
        self.cmd_helper.set_update_info(&format!("rollback_{}", app), crate::components::websockets::next_uid(), false);
        let res = updater.rollback().await;
        if let Err(e) = &res {
            self.cmd_helper.notify_update_response(&format!("Error Rolling Back {}", app), false);
            self.cmd_helper.notify_update_response(&e.message, true);
        }
        self.cmd_helper.clear_update_info();
        res?;
        Ok(json!("ok"))
    }
}

#[async_trait(?Send)]
impl Component for UpdateManager {
    fn has_init(&self) -> bool {
        true
    }

    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        self.instance_tracker.set_instance_id().await;
        let umdb = self.cmd_helper.umdb();
        for key in umdb.keys() {
            if self.get_updater(&key).is_none() {
                log::info!("Removing stale update_manager data: {}", key);
                let _ = umdb.pop(&key, Some(Value::Null));
            }
        }
        for (_, updater) in self.updaters() {
            updater.initialize().await;
        }
        let u = Rc::downgrade(&self);
        let repeat = self.auto_refresh_enabled;
        let h = spawn(async move {
            loop {
                match u.upgrade() {
                    Some(u) => u.handle_auto_refresh().await,
                    None => break,
                }
                if !repeat {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs_f64(UPDATE_REFRESH_INTERVAL)).await;
            }
        });
        *self.refresh_timer.borrow_mut() = Some(h);
        Ok(())
    }

    async fn close(self: Rc<Self>) {
        if let Some(t) = self.refresh_timer.borrow_mut().take() {
            t.abort();
        }
        self.instance_tracker.close().await;
        for (_, updater) in self.updaters() {
            updater.close().await;
        }
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<UpdateManager>, ConfigError> {
    UpdateManager::new(config)
}

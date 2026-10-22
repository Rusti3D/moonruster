// Machine manipulation request handlers (port of moonraker/components/machine.py)

use crate::common::*;
use crate::components::dbus_manager::DbusManager;
use crate::components::shell_command::{ExecOpts, ShellCommand, ShellCommandFactory};
use crate::confighelper::ConfigHelper;
use crate::pyfmt::{py_repr, py_str};
use crate::server::{server, spawn, Component};
use async_trait::async_trait;
use futures::future::LocalBoxFuture;
use futures::StreamExt;
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

const CGROUP_PATH: &str = "/proc/1/cgroup";
const SCHED_PATH: &str = "/proc/1/sched";
const SD_CID_PATH: &str = "/sys/block/mmcblk0/device/cid";
const SD_CSD_PATH: &str = "/sys/block/mmcblk0/device/csd";
const NETWORK_UPDATE_SEQUENCE: u64 = 10;
const SERVICE_PROPERTIES: [&str; 9] =
    ["Requires", "After", "SupplementaryGroups", "EnvironmentFiles", "ExecStart", "WorkingDirectory", "FragmentPath", "Description", "User"];
const USB_IDS_URL: &str = "http://www.linux-usb.org/usb.ids";
const SYSTEMD_BUS: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH_OBJ: &str = "/org/freedesktop/systemd1";
const SYSTEMD_MGR: &str = "org.freedesktop.systemd1.Manager";
const LOGIN_BUS: &str = "org.freedesktop.login1";
const LOGIN_PATH: &str = "/org/freedesktop/login1";
const LOGIN_MGR: &str = "org.freedesktop.login1.Manager";
// sysvinit provider (COSMOS patch to moonraker)
const SYSV_INIT_PATH: &str = "/etc/init.d";
const SYSV_PID_PATH: &str = "/var/run";
const SYSV_ACTIONS: [&str; 3] = ["start", "stop", "restart"];
const SYSV_ACTION_TIMEOUT: f64 = 60.0;
const SYSV_STATUS_TIMEOUT: f64 = 5.0;
const SYSV_STATUS_SEQUENCE: u64 = 5;

pub type SudoCallback = Rc<dyn Fn() -> LocalBoxFuture<'static, Result<(String, bool), ServerError>>>;

fn has_sysctl() -> bool {
    which("systemctl").is_some()
}

pub fn which(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
    path.split(':').map(|p| Path::new(p).join(cmd)).find(|p| p.exists())
}

// ***** distro emulation *****

pub(crate) fn parse_os_release() -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    for path in ["/etc/os-release", "/usr/lib/os-release"] {
        if let Ok(data) = std::fs::read_to_string(path) {
            for line in data.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = line.split_once('=') {
                    let v = crate::components::shell_command::shlex_split(v).join(" ");
                    out.insert(k.to_lowercase(), v);
                }
            }
            break;
        }
    }
    if let Some(v) = out.get("version").cloned() {
        let re = Regex::new(r"\((\D+)\)|,\s*(\D+)$").unwrap();
        if let Some(c) = re.captures(&v) {
            let cn = c.get(1).or(c.get(2)).map(|m| m.as_str().trim().to_string()).unwrap_or_default();
            out.entry("codename".into()).or_insert(cn);
        }
    }
    if let Some(cn) = out.get("version_codename").or(out.get("ubuntu_codename")).cloned() {
        out.insert("codename".into(), cn);
    }
    out
}

fn distro_release_info() -> Value {
    let ignore = [
        "debian_version", "lsb-release", "oem-release", "os-release", "system-release", "plesk-release", "iredmail-release",
        "board-release", "ec2_version",
    ];
    let mut files: Vec<PathBuf> = crate::utils::path_glob(Path::new("/etc"), "*-release");
    files.extend(crate::utils::path_glob(Path::new("/etc"), "*_release"));
    files.extend(crate::utils::path_glob(Path::new("/etc"), "*-version"));
    files.extend(crate::utils::path_glob(Path::new("/etc"), "*_version"));
    files.sort();
    let re = Regex::new(r"(?:Linux)?\s*(?P<name>.+?)\s*release\s*(?P<version_id>[\d.]+)[^(]*(?:\((?P<codename>[^)]*)\))?").unwrap();
    for f in files {
        let fname = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if ignore.contains(&fname.as_str()) {
            continue;
        }
        if let Ok(data) = std::fs::read_to_string(&f) {
            let line = data.lines().next().unwrap_or("").trim().to_string();
            let id = fname.split(|c| c == '-' || c == '_').next().unwrap_or("").to_string();
            let mut info = Map::new();
            if let Some(c) = re.captures(&line) {
                info.insert("name".into(), json!(c.name("name").map(|m| m.as_str()).unwrap_or("")));
                info.insert("version_id".into(), json!(c.name("version_id").map(|m| m.as_str()).unwrap_or("")));
                if let Some(cn) = c.name("codename") {
                    info.insert("codename".into(), json!(cn.as_str()));
                }
            } else if !line.is_empty() {
                info.insert("name".into(), json!(line));
            }
            if !info.is_empty() {
                info.insert("id".into(), json!(id));
                return Value::Object(info);
            }
        }
    }
    json!({})
}

fn dist_info() -> Map<String, Value> {
    let osr = parse_os_release();
    let pretty = osr.get("pretty_name").or(osr.get("name")).cloned().unwrap_or_default();
    let id = osr.get("id").cloned().unwrap_or_default().to_lowercase().replace(' ', "_");
    let version = osr.get("version_id").cloned().unwrap_or_default();
    let vre = Regex::new(r"(\d+)\.?(\d+)?\.?(\d+)?").unwrap();
    let (major, minor, build) = match vre.captures(&version) {
        Some(c) => (
            c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default(),
            c.get(2).map(|m| m.as_str().to_string()).unwrap_or_default(),
            c.get(3).map(|m| m.as_str().to_string()).unwrap_or_default(),
        ),
        None => (String::new(), String::new(), String::new()),
    };
    let mut m = Map::new();
    m.insert("name".into(), json!(pretty));
    m.insert("id".into(), json!(id));
    m.insert("version".into(), json!(version));
    m.insert("version_parts".into(), json!({"major": major, "minor": minor, "build_number": build}));
    m.insert("like".into(), json!(osr.get("id_like").cloned().unwrap_or_default()));
    m.insert("codename".into(), json!(osr.get("codename").cloned().unwrap_or_default()));
    m.insert("release_info".into(), distro_release_info());
    m.insert("kernel_version".into(), json!(kernel_release()));
    m
}

fn kernel_release() -> String {
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    unsafe {
        if libc::uname(&mut uts) == 0 {
            return std::ffi::CStr::from_ptr(uts.release.as_ptr()).to_string_lossy().to_string();
        }
    }
    String::new()
}

fn machine_arch() -> String {
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    unsafe {
        if libc::uname(&mut uts) == 0 {
            return std::ffi::CStr::from_ptr(uts.machine.as_ptr()).to_string_lossy().to_string();
        }
    }
    String::new()
}

// ***** Providers *****

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProviderKind {
    None,
    SystemdCli,
    SystemdDbus,
    SupervisordCli,
    Sysvinit,
}

pub struct Provider {
    kind: ProviderKind,
    shutdown_action: String,
    available_services: RefCell<IndexMap<String, Value>>,
    svc_cmd: RefCell<Option<Rc<ShellCommand>>>,
    status_cmds: RefCell<IndexMap<String, Rc<ShellCommand>>>,
    status_pidfiles: RefCell<IndexMap<String, PathBuf>>,
    spv_conf: String,
    dbus_ready: Cell<bool>,
    login_mgr: Cell<bool>,
    watchers: RefCell<Vec<tokio::task::JoinHandle<()>>>,
}

impl Provider {
    fn shell(&self) -> Rc<ShellCommandFactory> {
        server().lookup_component("shell_command").expect("shell_command")
    }

    fn dbus(&self) -> Option<Rc<DbusManager>> {
        server().lookup_component("dbus_manager")
    }

    async fn exec_sudo(&self, cmd: &str) -> Result<String, ServerError> {
        let machine: Rc<Machine> = server().lookup_component("machine").ok_or_else(|| ServerError::new("machine", 500))?;
        machine.exec_sudo_command(cmd, 1, 2.0).await
    }

    pub async fn initialize(self: &Rc<Self>) {
        match self.kind {
            ProviderKind::None => {}
            ProviderKind::SystemdCli => {
                self.detect_active_services_cli().await;
                if !self.available_services.borrow().is_empty() {
                    let svcs: Vec<String> = self.available_services.borrow().keys().cloned().collect();
                    *self.svc_cmd.borrow_mut() = Some(self.shell().build_shell_command(
                        &format!("systemctl show -p ActiveState,SubState --value {}", svcs.join(" ")),
                        None,
                        None,
                        None,
                        false,
                        None,
                    ));
                    self.update_service_status(0, true).await;
                    self.register_status_cb();
                }
            }
            ProviderKind::SystemdDbus => self.initialize_dbus().await,
            ProviderKind::SupervisordCli => {
                self.detect_active_services_spv().await;
                let keys: Vec<String> = self.available_services.borrow().keys().cloned().collect();
                let cmd = if !self.spv_conf.is_empty() {
                    format!("supervisorctl -c {} status {}", self.spv_conf, keys.join(" "))
                } else {
                    format!("supervisorctl status {}", keys.join(" "))
                };
                *self.svc_cmd.borrow_mut() = Some(self.shell().build_shell_command(&cmd, None, None, None, false, None));
                self.update_service_status(0, true).await;
                self.register_status_cb();
            }
            ProviderKind::Sysvinit => {
                let machine: Rc<Machine> = match server().lookup_component("machine") {
                    Some(m) => m,
                    None => return,
                };
                // COSMOS's zram script replaces /sbin/init with a shell shim,
                // which trips container detection
                machine.inside_container.set(false);
                for svc in machine.allowed_services.borrow().iter() {
                    let script = Path::new(SYSV_INIT_PATH).join(svc);
                    if script.is_file() && crate::utils::access_exec(&script) {
                        self.available_services
                            .borrow_mut()
                            .insert(svc.clone(), json!({"active_state": "unknown", "sub_state": "unknown"}));
                        let cmd = self.shell().build_shell_command(&format!("{}/{} status", SYSV_INIT_PATH, svc), None, None, None, false, None);
                        self.status_cmds.borrow_mut().insert(svc.clone(), cmd);
                        if let Some(pidfile) = std::fs::read_to_string(&script).ok().and_then(|t| sysv_status_pidfile(&t)) {
                            self.status_pidfiles.borrow_mut().insert(svc.clone(), pidfile);
                        }
                    }
                }
                if !self.available_services.borrow().is_empty() {
                    self.update_service_status(0, true).await;
                    self.register_status_cb();
                }
            }
        }
    }

    async fn update_sysv_state(&self, svc: &str, notify: bool) {
        let pidfile = self.status_pidfiles.borrow().get(svc).cloned();
        let running = match pidfile {
            // what the script's status does, without starting a shell
            Some(p) => std::fs::read_to_string(&p)
                .ok()
                .and_then(|t| t.trim().parse::<libc::pid_t>().ok())
                .map(|pid| unsafe { libc::kill(pid, 0) } == 0)
                .unwrap_or(false),
            None => {
                let cmd = match self.status_cmds.borrow().get(svc) {
                    Some(c) => c.clone(),
                    None => return,
                };
                cmd.run(&self.shell(), SYSV_STATUS_TIMEOUT, false, false, 1, None, None).await
            }
        };
        let new_state = if running {
            json!({"active_state": "active", "sub_state": "running"})
        } else {
            json!({"active_state": "inactive", "sub_state": "dead"})
        };
        self.apply_state(svc, new_state, notify);
    }

    fn register_status_cb(self: &Rc<Self>) {
        if let Some(ps) = server().lookup_component::<crate::components::proc_stats::ProcStats>("proc_stats") {
            let p = Rc::downgrade(self);
            ps.register_stat_callback(Rc::new(move |seq| {
                let p = p.clone();
                Some(Box::pin(async move {
                    if let Some(p) = p.upgrade() {
                        p.update_service_status(seq, true).await;
                    }
                }))
            }));
        }
    }

    async fn initialize_dbus(self: &Rc<Self>) {
        let srv = server();
        let dbus = match self.dbus() {
            Some(d) if d.is_connected() => d,
            _ => {
                srv.add_warning(
                    "[machine]: DBus Connection Not available, systemd  service tracking and actions are disabled",
                    None,
                    true,
                );
                return;
            }
        };
        if let Err(e) = dbus.has_interface(SYSTEMD_BUS, SYSTEMD_PATH_OBJ, SYSTEMD_MGR).await {
            log::error!("Failed to get systemd manager interface: {}", e);
            return;
        }
        self.dbus_ready.set(true);
        dbus.check_permission("org.freedesktop.systemd1.manage-units", "System Service Management (start, stop, restart) will be disabled")
            .await;
        if self.shutdown_action == "poweroff" {
            dbus.check_permission("org.freedesktop.login1.power-off", "The shutdown API will be disabled").await;
            dbus.check_permission(
                "org.freedesktop.login1.power-off-multiple-sessions",
                "The shutdown API will be disabled if multiple user sessions are open.",
            )
            .await;
        } else {
            dbus.check_permission("org.freedesktop.login1.halt", "The shutdown API will be disabled").await;
            dbus.check_permission(
                "org.freedesktop.login1.halt-multiple-sessions",
                "The shutdown API will be disabled if multiple user sessions are open.",
            )
            .await;
        }
        match dbus.has_interface(LOGIN_BUS, LOGIN_PATH, LOGIN_MGR).await {
            Err(e) => {
                log::info!(
                    "Unable to acquire the systemd-logind D-Bus interface, falling back to CLI Reboot and Shutdown APIs. {}",
                    e
                );
                self.login_mgr.set(false);
            }
            Ok(_) => {
                self.login_mgr.set(true);
                dbus.check_permission("org.freedesktop.login1.reboot", "The reboot API will be disabled").await;
                dbus.check_permission(
                    "org.freedesktop.login1.reboot-multiple-sessions",
                    "The reboot API will be disabled if multiple user sessions are open.",
                )
                .await;
            }
        }
        self.detect_active_services_dbus().await;
    }

    async fn detect_active_services_dbus(self: &Rc<Self>) {
        let dbus = match self.dbus() {
            Some(d) => d,
            None => return,
        };
        let machine: Rc<Machine> = match server().lookup_component("machine") {
            Some(m) => m,
            None => return,
        };
        type Unit = (String, String, String, String, String, String, zvariant::OwnedObjectPath, u32, String, zvariant::OwnedObjectPath);
        let units: Vec<Unit> = match dbus.call(SYSTEMD_BUS, SYSTEMD_PATH_OBJ, SYSTEMD_MGR, "ListUnitsFiltered", &(vec!["loaded"],)).await {
            Ok(u) => u,
            Err(e) => {
                log::error!("Failed to list systemd units: {}", e);
                return;
            }
        };
        for unit in units {
            let name = unit.0.split('.').next().unwrap_or("").to_string();
            if !machine.is_service_allowed(&name) {
                continue;
            }
            if self.available_services.borrow().contains_key(&name) {
                continue;
            }
            self.available_services
                .borrow_mut()
                .insert(name.clone(), json!({"active_state": unit.3, "sub_state": unit.4}));
            // property change monitoring
            let conn = match dbus.connection() {
                Some(c) => c,
                None => continue,
            };
            let path = unit.6.to_string();
            let p = Rc::downgrade(self);
            let handle = spawn(async move {
                let proxy = match zbus::fdo::PropertiesProxy::builder(&conn).destination(SYSTEMD_BUS).and_then(|b| b.path(path.as_str())) {
                    Ok(b) => match b.build().await {
                        Ok(p) => p,
                        Err(_) => return,
                    },
                    Err(_) => return,
                };
                let mut stream = match proxy.receive_properties_changed().await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                while let Some(sig) = stream.next().await {
                    let args = match sig.args() {
                        Ok(a) => a,
                        Err(_) => continue,
                    };
                    if args.interface_name().as_str() != "org.freedesktop.systemd1.Unit" {
                        continue;
                    }
                    let changed: Map<String, Value> = args
                        .changed_properties()
                        .iter()
                        .map(|(k, v)| (k.to_string(), crate::components::dbus_manager::zv_to_json(v)))
                        .collect();
                    match p.upgrade() {
                        Some(prov) => prov.on_service_update(&name, &changed),
                        None => return,
                    }
                }
            });
            self.watchers.borrow_mut().push(handle);
        }
    }

    fn on_service_update(&self, name: &str, changed: &Map<String, Value>) {
        let mut svcs = self.available_services.borrow_mut();
        let svc = match svcs.get_mut(name) {
            Some(s) => s,
            None => return,
        };
        let mut notify = false;
        for (prop, field) in [("ActiveState", "active_state"), ("SubState", "sub_state")] {
            if let Some(v) = changed.get(prop) {
                if svc.get(field) != Some(v) {
                    notify = true;
                    svc[field] = v.clone();
                }
            }
        }
        if notify {
            let mut m = Map::new();
            m.insert(name.to_string(), svc.clone());
            let _ = server().send_event("machine:service_state_changed", vec![Value::Object(m)]);
        }
    }

    async fn detect_active_services_cli(&self) {
        let machine: Rc<Machine> = match server().lookup_component("machine") {
            Some(m) => m,
            None => return,
        };
        let services: Vec<String> = match self.shell().exec("systemctl list-units --all --type=service --plain --no-legend", 2.0).await {
            Ok(resp) => resp
                .split('\n')
                .filter(|l| l.trim().contains(".service"))
                .filter_map(|l| l.split_whitespace().next().map(|s| s.trim().to_string()))
                .collect(),
            Err(_) => Vec::new(),
        };
        for svc in services {
            let sname = svc.rsplit_once('.').map(|(a, _)| a.to_string()).unwrap_or(svc.clone());
            if machine.is_service_allowed(&sname) {
                self.available_services.borrow_mut().insert(sname, json!({"active_state": "unknown", "sub_state": "unknown"}));
            }
        }
    }

    fn spv_active_state(sub: &str) -> &'static str {
        match sub {
            "stopping" => "deactivating",
            "running" => "active",
            _ => "inactive",
        }
    }

    async fn exec_supervisorctl(&self, args: &str, tries: usize, timeout: f64, success_codes: Option<&[i32]>) -> Result<String, ServerError> {
        let cmd = if !self.spv_conf.is_empty() {
            format!("supervisorctl -c {} {}", self.spv_conf, args)
        } else {
            format!("supervisorctl {}", args)
        };
        self.shell()
            .exec_cmd(&cmd, ExecOpts { timeout, attempts: tries, log_complete: false, success_codes, ..Default::default() })
            .await
            .map_err(ServerError::from)
    }

    async fn get_process_info(&self, names: Option<Vec<String>>) -> IndexMap<String, Value> {
        let cmd = match names {
            Some(n) => format!("status {}", n.join(" ")),
            None => "status".into(),
        };
        let mut units = IndexMap::new();
        let resp = match self.exec_supervisorctl(&cmd, 1, 6.0, Some(&[0, 3])).await {
            Ok(r) => r,
            Err(_) => return units,
        };
        for line in resp.split('\n').map(|l| l.trim()).filter(|l| !l.is_empty()) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 {
                continue;
            }
            let state = parts[1].to_lowercase();
            if state == "running" && parts.len() >= 6 {
                let pid: i64 = parts[3].trim_end_matches(',').parse().unwrap_or(0);
                units.insert(parts[0].to_string(), json!({"state": state, "pid": pid, "uptime": parts[5]}));
            } else {
                units.insert(parts[0].to_string(), json!({"state": state}));
            }
        }
        units
    }

    async fn detect_active_services_spv(&self) {
        let machine: Rc<Machine> = match server().lookup_component("machine") {
            Some(m) => m,
            None => return,
        };
        for (unit, info) in self.get_process_info(None).await {
            if machine.is_service_allowed(&unit) {
                let st = info["state"].as_str().unwrap_or("").to_string();
                self.available_services
                    .borrow_mut()
                    .insert(unit, json!({"active_state": Self::spv_active_state(&st), "sub_state": st}));
            }
        }
    }

    async fn update_service_status(&self, sequence: u64, notify: bool) {
        if self.kind == ProviderKind::Sysvinit {
            if sequence % SYSV_STATUS_SEQUENCE == 0 {
                let svcs: Vec<String> = self.available_services.borrow().keys().cloned().collect();
                for svc in svcs {
                    self.update_sysv_state(&svc, notify).await;
                }
            }
            return;
        }
        if sequence % 2 != 0 {
            return;
        }
        let cmd = match self.svc_cmd.borrow().clone() {
            Some(c) => c,
            None => return,
        };
        let svcs: Vec<String> = self.available_services.borrow().keys().cloned().collect();
        let shell = self.shell();
        if self.kind == ProviderKind::SupervisordCli {
            let resp = match cmd.run_with_response(&shell, 6.0, 1, false, 1, None, Some(&[0, 3])).await {
                Ok(r) => r,
                Err(e) => {
                    log::error!("Error processing service state update\n{}", e);
                    return;
                }
            };
            for (svc, state) in svcs.iter().zip(resp.trim().split('\n')) {
                let sub = state.split_whitespace().nth(1).unwrap_or("").to_lowercase();
                let new_state = json!({"active_state": Self::spv_active_state(&sub), "sub_state": sub});
                self.apply_state(svc, new_state, notify);
            }
        } else {
            let resp = match cmd.run_with_response(&shell, 2.0, 1, false, 1, None, None).await {
                Ok(r) => r,
                Err(e) => {
                    log::error!("Error processing service state update\n{}", e);
                    return;
                }
            };
            for (svc, state) in svcs.iter().zip(resp.trim().split("\n\n")) {
                let (a, s) = match state.split_once('\n') {
                    Some(x) => x,
                    None => {
                        log::error!("Error processing service state update");
                        return;
                    }
                };
                self.apply_state(svc, json!({"active_state": a, "sub_state": s}), notify);
            }
        }
    }

    fn apply_state(&self, svc: &str, new_state: Value, notify: bool) {
        let changed = self.available_services.borrow().get(svc) != Some(&new_state);
        if changed {
            self.available_services.borrow_mut().insert(svc.to_string(), new_state.clone());
            if notify {
                let mut m = Map::new();
                m.insert(svc.to_string(), new_state);
                let _ = server().send_event("machine:service_state_changed", vec![Value::Object(m)]);
            }
        }
    }

    pub async fn shutdown(&self) -> Result<(), ServerError> {
        if self.kind == ProviderKind::Sysvinit {
            return self.shell().exec(&self.shutdown_action, 2.0).await.map(|_| ()).map_err(ServerError::from);
        }
        if self.kind == ProviderKind::SystemdDbus && self.login_mgr.get() {
            let method = if self.shutdown_action == "poweroff" { "PowerOff" } else { "Halt" };
            let dbus = self.dbus().ok_or_else(|| ServerError::new("DBus Not Connected, ", 503))?;
            let _: () = dbus.call(LOGIN_BUS, LOGIN_PATH, LOGIN_MGR, method, &(false,)).await?;
            return Ok(());
        }
        if self.kind == ProviderKind::SystemdDbus {
            // python calls super().shutdown() then falls through to the login manager (None) and raises
            let act = &self.shutdown_action;
            let cmd = if has_sysctl() { format!("systemctl {}", act) } else { act.clone() };
            self.exec_sudo(&cmd).await?;
            return Err(ServerError::exc("'NoneType' object has no attribute 'call_power_off'"));
        }
        let act = &self.shutdown_action;
        let cmd = if has_sysctl() { format!("systemctl {}", act) } else { act.clone() };
        self.exec_sudo(&cmd).await.map(|_| ())
    }

    pub async fn reboot(&self) -> Result<(), ServerError> {
        if self.kind == ProviderKind::Sysvinit {
            return self.shell().exec("reboot", 2.0).await.map(|_| ()).map_err(ServerError::from);
        }
        if self.kind == ProviderKind::SystemdDbus && self.login_mgr.get() {
            let dbus = self.dbus().ok_or_else(|| ServerError::new("DBus Not Connected, ", 503))?;
            let _: () = dbus.call(LOGIN_BUS, LOGIN_PATH, LOGIN_MGR, "Reboot", &(false,)).await?;
            return Ok(());
        }
        let cmd = if has_sysctl() { "systemctl reboot" } else { "reboot" };
        self.exec_sudo(cmd).await?;
        if self.kind == ProviderKind::SystemdDbus {
            return Err(ServerError::exc("'NoneType' object has no attribute 'call_reboot'"));
        }
        Ok(())
    }

    pub async fn do_service_action(&self, action: &str, service_name: &str) -> Result<(), ServerError> {
        match self.kind {
            ProviderKind::None => Err(ServerError::new("Service Actions Not Available", 503)),
            ProviderKind::SystemdCli => self.exec_sudo(&format!("systemctl {} {}", action, service_name)).await.map(|_| ()),
            ProviderKind::SupervisordCli => self.exec_supervisorctl(&format!("{} {}", action, service_name), 1, 6.0, None).await.map(|_| ()),
            ProviderKind::Sysvinit => {
                if !SYSV_ACTIONS.contains(&action) {
                    return Err(ServerError::bad(format!("Invalid service action '{}'", action)));
                }
                if !self.is_service_available(service_name) {
                    return Err(ServerError::bad(format!("Service '{}' not available", service_name)));
                }
                let script = format!("{}/{}", SYSV_INIT_PATH, service_name);
                let machine: Option<Rc<Machine>> = server().lookup_component("machine");
                if machine.map(|m| m.unit_name() == service_name).unwrap_or(false) {
                    // the init script stops us, so it must outlive this process
                    use std::os::unix::process::CommandExt;
                    std::process::Command::new(&script)
                        .arg(action)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .process_group(0)
                        .spawn()?;
                    return Ok(());
                }
                self.shell()
                    .exec_cmd(&format!("{} {}", script, action), ExecOpts { timeout: SYSV_ACTION_TIMEOUT, log_complete: false, ..Default::default() })
                    .await?;
                self.update_sysv_state(service_name, true).await;
                Ok(())
            }
            ProviderKind::SystemdDbus => {
                let dbus = match self.dbus() {
                    Some(d) if d.is_connected() && self.dbus_ready.get() => d,
                    _ => return Err(ServerError::new("DBus Not Connected, ", 503)),
                };
                let mut svc = service_name.to_string();
                if !svc.ends_with(".service") {
                    svc.push_str(".service");
                }
                let method = match action {
                    "start" => "StartUnit",
                    "stop" => "StopUnit",
                    "restart" => "RestartUnit",
                    _ => return Err(ServerError::bad(format!("Invalid service action: {}", action))),
                };
                let _: zvariant::OwnedObjectPath = dbus.call(SYSTEMD_BUS, SYSTEMD_PATH_OBJ, SYSTEMD_MGR, method, &(svc.as_str(), "replace")).await?;
                Ok(())
            }
        }
    }

    pub async fn check_virt_status(&self) -> Value {
        match self.kind {
            ProviderKind::None | ProviderKind::SupervisordCli | ProviderKind::Sysvinit => {
                json!({"virt_type": "unknown", "virt_identifier": "unknown"})
            }
            ProviderKind::SystemdCli => {
                let mut virt_id = "none".to_string();
                let mut virt_type = "none".to_string();
                if let Ok(r) = self.shell().exec("systemd-detect-virt", 2.0).await {
                    virt_id = r.trim().to_string();
                }
                if virt_id != "none" {
                    match self.shell().exec("systemd-detect-virt --container", 2.0).await {
                        Err(_) => virt_type = "vm".into(),
                        Ok(r) => virt_type = if virt_id == r.trim() { "container".into() } else { "vm".into() },
                    }
                    log::info!("Virtualized Environment Detected, Type: {} id: {}", virt_type, virt_id);
                } else {
                    log::info!("No Virtualization Detected");
                }
                json!({"virt_type": virt_type, "virt_identifier": virt_id})
            }
            ProviderKind::SystemdDbus => {
                let dbus = match self.dbus() {
                    Some(d) if d.is_connected() && self.dbus_ready.get() => d,
                    _ => return json!({"virt_type": "unknown", "virt_identifier": "unknown"}),
                };
                let virt = dbus
                    .get_property(SYSTEMD_BUS, SYSTEMD_PATH_OBJ, SYSTEMD_MGR, "Virtualization")
                    .await
                    .ok()
                    .and_then(|v| v.as_str().map(|s| s.trim().to_string()))
                    .unwrap_or_default();
                let (mut virt_id, mut virt_type) = ("none".to_string(), "none".to_string());
                if !virt.is_empty() {
                    virt_id = virt.clone();
                    let containers = ["openvz", "lxc", "lxc-libvirt", "systemd-nspawn", "docker", "podman", "rkt", "wsl", "proot", "pouch"];
                    virt_type = if containers.contains(&virt.as_str()) { "container".into() } else { "vm".into() };
                    log::info!("Virtualized Environment Detected, Type: {} id: {}", virt_type, virt_id);
                } else {
                    log::info!("No Virtualization Detected");
                }
                json!({"virt_type": virt_type, "virt_identifier": virt_id})
            }
        }
    }

    pub fn is_service_available(&self, service: &str) -> bool {
        self.available_services.borrow().contains_key(service)
    }

    pub fn get_available_services(&self) -> IndexMap<String, Value> {
        self.available_services.borrow().clone()
    }

    pub async fn extract_service_info(&self, service_name: &str, pid: i64, properties: Option<Vec<String>>, raw: bool) -> Value {
        let props: Vec<String> = properties.unwrap_or_else(|| SERVICE_PROPERTIES.iter().map(|s| s.to_string()).collect());
        let expected = format!("{}.service", service_name);
        match self.kind {
            ProviderKind::None => json!({}),
            ProviderKind::SystemdCli => {
                let res: Result<Value, ServerError> = async {
                    let resp = self.shell().exec(&format!("systemctl status {}", pid), 2.0).await?;
                    let unit_name = resp.split_whitespace().nth(1).ok_or_else(|| ServerError::exc("list index out of range"))?.to_string();
                    let mut info = Map::new();
                    info.insert("unit_name".into(), json!(unit_name));
                    info.insert("is_default".into(), json!(true));
                    info.insert("manager".into(), json!("systemd"));
                    if unit_name != expected {
                        info.insert("is_default".into(), json!(false));
                        log::info!("Detected alternate unit name for {}: {}", service_name, unit_name);
                    }
                    let out = self
                        .shell()
                        .exec_cmd(&format!("systemctl show -p {} {}", props.join(","), unit_name), ExecOpts { attempts: 5, timeout: 10.0, ..Default::default() })
                        .await?;
                    let mut raw_props = Map::new();
                    for line in out.split('\n').map(|l| l.trim()).filter(|l| !l.is_empty()) {
                        if let Some((k, v)) = line.split_once('=') {
                            raw_props.insert(k.trim().to_string(), json!(v.trim()));
                        }
                    }
                    info.insert("properties".into(), if raw { Value::Object(raw_props) } else { Self::process_cli_props(&raw_props) });
                    Ok(Value::Object(info))
                }
                .await;
                match res {
                    Ok(v) => v,
                    Err(e) => {
                        log::error!("Error extracting service info\n{}", e);
                        json!({})
                    }
                }
            }
            ProviderKind::SystemdDbus => {
                if !self.dbus_ready.get() {
                    return json!({});
                }
                let dbus = match self.dbus() {
                    Some(d) => d,
                    None => return json!({}),
                };
                let res: Result<Value, ServerError> = async {
                    let path: zvariant::OwnedObjectPath = dbus.call(SYSTEMD_BUS, SYSTEMD_PATH_OBJ, SYSTEMD_MGR, "GetUnitByPID", &(pid as u32,)).await?;
                    let unit_props = dbus.get_all_properties(SYSTEMD_BUS, path.as_str(), "org.freedesktop.systemd1.Unit").await?;
                    // get_interfaces() fails for units without a Service interface (scopes)
                    dbus.has_interface(SYSTEMD_BUS, path.as_str(), "org.freedesktop.systemd1.Unit").await?;
                    dbus.has_interface(SYSTEMD_BUS, path.as_str(), "org.freedesktop.systemd1.Service").await?;
                    let svc_props = dbus.get_all_properties(SYSTEMD_BUS, path.as_str(), "org.freedesktop.systemd1.Service").await?;
                    let unit_name = unit_props.get("Id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let mut info = Map::new();
                    info.insert("unit_name".into(), json!(unit_name));
                    info.insert("is_default".into(), json!(true));
                    info.insert("manager".into(), json!("systemd"));
                    if unit_name != expected {
                        info.insert("is_default".into(), json!(false));
                        log::info!("Detected alternate unit name for {}: {}", service_name, unit_name);
                    }
                    let mut raw_props = Map::new();
                    for key in &props {
                        if let Some(v) = unit_props.get(key).or_else(|| svc_props.get(key)) {
                            raw_props.insert(key.clone(), v.clone());
                        }
                    }
                    info.insert("properties".into(), if raw { Value::Object(raw_props) } else { Self::process_dbus_props(&raw_props) });
                    Ok(Value::Object(info))
                }
                .await;
                match res {
                    Ok(v) => v,
                    Err(e) => {
                        log::error!("Error Extracting Service Info\n{}", e);
                        json!({})
                    }
                }
            }
            ProviderKind::SupervisordCli => self.extract_spv_info(service_name, pid).await,
            ProviderKind::Sysvinit => {
                let pid_file = Path::new(SYSV_PID_PATH).join(format!("{}.pid", service_name));
                let file_pid = std::fs::read_to_string(&pid_file).ok().and_then(|s| s.trim().parse::<i64>().ok());
                if file_pid == Some(pid) {
                    return json!({
                        "unit_name": service_name,
                        "is_default": true,
                        "manager": "sysvinit",
                        "properties": {
                            "InitScript": format!("{}/{}", SYSV_INIT_PATH, service_name),
                            "PIDFile": pid_file.to_string_lossy(),
                        }
                    });
                }
                log::info!("Unable to locate service info for {}, pid: {}", service_name, pid);
                json!({})
            }
        }
    }

    async fn extract_spv_info(&self, service_name: &str, pid: i64) -> Value {
        let mut info = Map::new();
        let mut found = false;
        for _ in 0..5 {
            let names: Vec<String> = self.available_services.borrow().keys().cloned().collect();
            let proc_info = self.get_process_info(Some(names)).await;
            info.insert("unit_name".into(), json!(service_name));
            info.insert("is_default".into(), json!(true));
            info.insert("manager".into(), json!("supervisord"));
            let mut need_retry = false;
            for (name, pi) in &proc_info {
                match pi.get("pid").and_then(|v| v.as_i64()) {
                    None => need_retry |= pi["state"] == "starting",
                    Some(p) if p == pid => {
                        if name != service_name {
                            info.insert("unit_name".into(), json!(name));
                            info.insert("is_default".into(), json!(false));
                            log::info!("Detected alternate unit name for {}: {}", service_name, name);
                        }
                        found = true;
                        break;
                    }
                    _ => {}
                }
            }
            if found {
                break;
            }
            if need_retry {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            } else {
                break;
            }
        }
        if !found {
            log::info!("Unable to locate service info for {}, pid: {}", service_name, pid);
            return json!({});
        }
        let spv_path = if !self.spv_conf.is_empty() {
            let p = PathBuf::from(&self.spv_conf);
            if !p.is_file() {
                log::info!("Invalid supervisord configuration file: {}", self.spv_conf);
                return Value::Object(info);
            }
            p
        } else {
            match ["/etc/supervisord.conf", "/etc/supervisor/supervisord.conf"].iter().map(PathBuf::from).find(|p| p.is_file()) {
                Some(p) => p,
                None => {
                    log::info!("Failed to locate supervisord.conf");
                    return Value::Object(info);
                }
            }
        };
        let mut cp = crate::configparser::ConfigParser::new();
        let _ = cp.read_string(&std::fs::read_to_string(&spv_path).unwrap_or_default(), "<string>");
        let unit = info["unit_name"].as_str().unwrap_or("").to_string();
        let section = format!("program:{}", unit);
        if !cp.has_section(&section) {
            log::info!("Unable to location supervisor section {}", section);
            return Value::Object(info);
        }
        let props: Map<String, Value> = cp.items(&section).unwrap_or_default().into_iter().map(|(k, v)| (k, json!(v))).collect();
        info.insert("properties".into(), Value::Object(props));
        Value::Object(info)
    }

    fn process_cli_props(raw: &Map<String, Value>) -> Value {
        let mut out = Map::new();
        let re = Regex::new(r"argv\[\]=([^;]+);").unwrap();
        for (k, v) in raw {
            let s = v.as_str().unwrap_or("").to_string();
            let mut val = v.clone();
            if k == "ExecStart" {
                if let Some(c) = re.captures(&s) {
                    val = json!(c[1].trim());
                }
            } else if k == "EnvironmentFiles" {
                if !s.is_empty() {
                    val = json!(s.split_whitespace().next().unwrap_or(""));
                }
            } else if ["Requires", "After", "SupplementaryGroups"].contains(&k.as_str()) {
                val = json!(s.split_whitespace().collect::<Vec<_>>());
            }
            out.insert(k.clone(), val);
        }
        Value::Object(out)
    }

    fn process_dbus_props(raw: &Map<String, Value>) -> Value {
        let mut out = Map::new();
        for (k, v) in raw {
            let mut val = v.clone();
            if k == "ExecStart" {
                if let Some(argv) = v.get(0).and_then(|e| e.get(1)).and_then(|a| a.as_array()) {
                    val = json!(argv.iter().map(py_str).collect::<Vec<_>>().join(" "));
                }
            } else if k == "EnvironmentFiles" {
                if let Some(p) = v.get(0).and_then(|e| e.get(0)) {
                    val = p.clone();
                }
            }
            out.insert(k.clone(), val);
        }
        Value::Object(out)
    }
}

// ***** Machine *****

pub struct Machine {
    allowed_services: RefCell<Vec<String>>,
    inside_container: Cell<bool>,
    moonraker_service_info: RefCell<Value>,
    sudo_req_lock: tokio::sync::Mutex<()>,
    periph_lock: tokio::sync::Mutex<()>,
    sudo_password: RefCell<Option<String>>,
    public_ip: RefCell<String>,
    pub system_info: RefCell<Map<String, Value>>,
    provider_type: String,
    provider: Rc<Provider>,
    validator: Rc<InstallValidator>,
    sudo_requests: RefCell<Vec<(SudoCallback, String)>>,
    addr_cmd: Rc<ShellCommand>,
    iwgetid_cmd: Rc<ShellCommand>,
    init_done: Cell<bool>,
    init_notify: tokio::sync::Notify,
}

#[async_trait(?Send)]
impl Component for Machine {
    fn has_init(&self) -> bool {
        true
    }
    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        let m = self.clone();
        spawn(async move { m.update_usb_ids(false).await });
        self.validator.validation_init().await;
        self.provider.initialize().await;
        if !self.inside_container.get() {
            let virt = self.provider.check_virt_status().await;
            self.system_info.borrow_mut().insert("virtualization".into(), virt);
        }
        self.parse_network_interfaces(0, false).await;
        if let Some(ps) = server().lookup_component::<crate::components::proc_stats::ProcStats>("proc_stats") {
            let m = Rc::downgrade(&self);
            ps.register_stat_callback(Rc::new(move |seq| {
                let m = m.clone();
                Some(Box::pin(async move {
                    if let Some(m) = m.upgrade() {
                        m.parse_network_interfaces(seq, true).await;
                    }
                }))
            }));
        }
        let avail = self.provider.get_available_services();
        let names: Vec<Value> = avail.keys().map(|k| json!(k)).collect();
        self.system_info.borrow_mut().insert("available_services".into(), Value::Array(names));
        self.system_info
            .borrow_mut()
            .insert("service_state".into(), Value::Object(avail.into_iter().collect()));
        let svc_info = self.provider.extract_service_info("moonraker", std::process::id() as i64, None, false).await;
        *self.moonraker_service_info.borrow_mut() = svc_info.clone();
        self.log_service_info(&svc_info);
        self.init_done.set(true);
        self.init_notify.notify_waiters();
        Ok(())
    }
    async fn on_exit(self: Rc<Self>) {
        self.validator.remove_announcement().await;
    }
}

macro_rules! machine_endpoint {
    ($srv:expr, $m:expr, $path:expr, $rt:expr, $method:ident) => {{
        let mm = $m.clone();
        $srv.register_endpoint($path, $rt, callback(move |req: WebRequest| {
            let mm = mm.clone();
            async move { mm.$method(req).await }
        }))?;
    }};
}

impl Machine {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, crate::confighelper::ConfigError> {
        let srv = server();
        let sudo_password = match config.gettemplate_opt("sudo_password", Some(None), false)? {
            Some(t) => Some(t.render(&json!({})).map_err(|e| ConfigHelper::error(e.message))?),
            None => None,
        };
        let provider_type = config.get_or("provider", "systemd_dbus")?;
        let kind = match provider_type.as_str() {
            "none" => ProviderKind::None,
            "systemd_cli" => ProviderKind::SystemdCli,
            "systemd_dbus" => ProviderKind::SystemdDbus,
            "supervisord_cli" => ProviderKind::SupervisordCli,
            "sysvinit" => ProviderKind::Sysvinit,
            other => return Err(ConfigHelper::error(format!("Invalid Provider: {}", other))),
        };
        let shutdown_action = config.getchoice("shutdown_action", &["halt", "poweroff"], Some("poweroff"), true)?;
        srv.load_component(config, "shell_command")?;
        let spv_conf = if kind == ProviderKind::SupervisordCli { config.get_or("supervisord_config_path", "")? } else { String::new() };
        let provider = Rc::new(Provider {
            kind,
            shutdown_action,
            available_services: RefCell::new(IndexMap::new()),
            svc_cmd: RefCell::new(None),
            status_cmds: RefCell::new(IndexMap::new()),
            status_pidfiles: RefCell::new(IndexMap::new()),
            spv_conf,
            dbus_ready: Cell::new(false),
            login_mgr: Cell::new(false),
            watchers: RefCell::new(Vec::new()),
        });
        log::info!("Using System Provider: {}", provider_type);
        let validator = InstallValidator::new(config)?;
        let shell: Rc<ShellCommandFactory> = srv.lookup_component("shell_command").unwrap();
        let addr_cmd = shell.build_shell_command("ip -json -det address", None, None, None, false, None);
        let iwgetbin = if Path::new("/sbin/iwgetid").exists() { "/sbin/iwgetid" } else { "iwgetid" };
        let iwgetid_cmd = shell.build_shell_command(iwgetbin, None, None, None, false, None);
        let m = Rc::new(Machine {
            allowed_services: RefCell::new(Vec::new()),
            inside_container: Cell::new(false),
            moonraker_service_info: RefCell::new(json!({})),
            sudo_req_lock: tokio::sync::Mutex::new(()),
            periph_lock: tokio::sync::Mutex::new(()),
            sudo_password: RefCell::new(sudo_password),
            public_ip: RefCell::new(String::new()),
            system_info: RefCell::new(Map::new()),
            provider_type: provider_type.clone(),
            provider,
            validator,
            sudo_requests: RefCell::new(Vec::new()),
            addr_cmd,
            iwgetid_cmd,
            init_done: Cell::new(false),
            init_notify: tokio::sync::Notify::new(),
        });
        m.init_allowed_services();
        {
            let (pyver, pystr) = python_version();
            let mut si = m.system_info.borrow_mut();
            si.insert("python".into(), json!({"version": pyver, "version_string": pystr}));
            si.insert("cpu_info".into(), get_cpu_info());
            si.insert("sd_info".into(), get_sdcard_info());
            si.insert("distribution".into(), Value::Object(dist_info()));
            let virt = m.check_inside_container();
            si.insert("virtualization".into(), virt);
            si.insert("network".into(), json!({}));
            si.insert("canbus".into(), json!({}));
        }
        m.update_log_rollover(true);
        m.system_info.borrow_mut().insert("provider".into(), json!(provider_type));
        machine_endpoint!(srv, m, "/machine/reboot", RequestType::POST, handle_machine_request);
        machine_endpoint!(srv, m, "/machine/shutdown", RequestType::POST, handle_machine_request);
        machine_endpoint!(srv, m, "/machine/services/restart", RequestType::POST, handle_service_request);
        machine_endpoint!(srv, m, "/machine/services/stop", RequestType::POST, handle_service_request);
        machine_endpoint!(srv, m, "/machine/services/start", RequestType::POST, handle_service_request);
        machine_endpoint!(srv, m, "/machine/system_info", RequestType::GET, handle_sysinfo_request);
        machine_endpoint!(srv, m, "/machine/sudo/info", RequestType::GET, handle_sudo_info);
        machine_endpoint!(srv, m, "/machine/sudo/password", RequestType::POST, set_sudo_password);
        machine_endpoint!(srv, m, "/machine/peripherals/serial", RequestType::GET, handle_serial_request);
        machine_endpoint!(srv, m, "/machine/peripherals/usb", RequestType::GET, handle_usb_request);
        machine_endpoint!(srv, m, "/machine/peripherals/canbus", RequestType::GET, handle_can_query);
        machine_endpoint!(srv, m, "/machine/peripherals/video", RequestType::GET, handle_video_request);
        srv.register_notification("machine:service_state_changed", None);
        srv.register_notification("machine:sudo_alert", None);
        let p = m.provider.clone();
        srv.register_remote_method(
            "shutdown_machine",
            Rc::new(move |_| {
                let p = p.clone();
                Some(Box::pin(async move {
                    let _ = p.shutdown().await;
                }))
            }),
        )?;
        let p = m.provider.clone();
        srv.register_remote_method(
            "reboot_machine",
            Rc::new(move |_| {
                let p = p.clone();
                Some(Box::pin(async move {
                    let _ = p.reboot().await;
                }))
            }),
        )?;
        let has_libcam = false;
        srv.add_log_rollover_item("libcamera", if has_libcam { "Found libcamera" } else { "Module libcamera unavailable, import failed" }, true);
        Ok(m)
    }

    fn init_allowed_services(&self) {
        let srv = server();
        let fpath = PathBuf::from(srv.data_path()).join("moonraker.asvc");
        if let Some(fm) = srv.lookup_component::<crate::components::file_manager::FileManager>("file_manager") {
            fm.add_reserved_path("allowed_services", &fpath, false);
        }
        let default_svcs = crate::assets::read_asset("default_allowed_services").unwrap_or_default();
        let data = (|| -> std::io::Result<String> {
            if !fpath.exists() {
                std::fs::write(&fpath, &default_svcs)?;
            }
            std::fs::read_to_string(&fpath)
        })()
        .unwrap_or_else(|e| {
            log::error!("Failed to read moonraker.asvc\n{}", e);
            default_svcs.clone()
        });
        let mut allowed = self.allowed_services.borrow_mut();
        for svc in data.split('\n').map(|s| s.trim()).filter(|s| !s.is_empty()) {
            let svc = svc.strip_suffix(".service").unwrap_or(svc).to_string();
            if !allowed.contains(&svc) {
                allowed.push(svc);
            }
        }
    }

    fn update_log_rollover(&self, log_it: bool) {
        let mut msg = String::from("\nSystem Info:");
        for (header, info) in self.system_info.borrow().iter() {
            msg.push_str(&format!("\n\n***{}***", header));
            match info {
                Value::Object(o) => {
                    for (k, v) in o {
                        msg.push_str(&format!("\n  {}: {}", k, py_str(v)));
                    }
                }
                other => msg.push_str(&format!("\n  {}", py_repr(other))),
            }
        }
        msg.push_str("\n\n***Allowed Services***");
        for s in self.allowed_services.borrow().iter() {
            msg.push_str(&format!("\n  {}", s));
        }
        server().add_log_rollover_item("system_info", &msg, log_it);
    }

    pub fn public_ip(&self) -> Option<String> {
        let ip = self.public_ip.borrow().clone();
        if ip.is_empty() {
            None
        } else {
            Some(ip)
        }
    }

    pub fn unit_name(&self) -> String {
        let si = self.moonraker_service_info.borrow();
        let un = si.get("unit_name").and_then(|v| v.as_str()).unwrap_or("moonraker.service");
        un.split('.').next().unwrap_or(un).to_string()
    }

    pub fn is_service_allowed(&self, service: &str) -> bool {
        static RE1: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        static RE2: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        let r1 = RE1.get_or_init(|| Regex::new(r"^moonraker[_-]?\d*").unwrap());
        let r2 = RE2.get_or_init(|| Regex::new(r"^klipper[_-]?\d*").unwrap());
        self.allowed_services.borrow().iter().any(|s| s == service) || r1.is_match(service) || r2.is_match(service)
    }

    pub fn validation_enabled(&self) -> bool {
        self.validator.validation_enabled.get()
    }

    pub fn is_inside_container(&self) -> bool {
        self.inside_container.get()
    }

    pub fn get_provider_type(&self) -> String {
        self.provider_type.clone()
    }

    pub fn get_moonraker_service_info(&self) -> Value {
        self.moonraker_service_info.borrow().clone()
    }

    pub async fn wait_for_init(&self, timeout: Option<f64>) {
        if self.init_done.get() {
            return;
        }
        match timeout {
            Some(t) => {
                let _ = tokio::time::timeout(std::time::Duration::from_secs_f64(t), self.init_notify.notified()).await;
            }
            None => self.init_notify.notified().await,
        }
    }

    pub async fn validate_installation(self: &Rc<Self>) -> bool {
        self.validator.perform_validation().await
    }

    pub async fn extract_service_info(&self, name: &str, pid: i64) -> Value {
        self.provider.extract_service_info(name, pid, None, false).await
    }

    pub async fn extract_service_info_ext(&self, name: &str, pid: i64, props: Option<Vec<String>>, raw: bool) -> Value {
        self.provider.extract_service_info(name, pid, props, raw).await
    }

    pub fn get_system_provider(&self) -> Rc<Provider> {
        self.provider.clone()
    }

    async fn handle_machine_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let ep = req.get_endpoint().to_string();
        if self.inside_container.get() {
            let virt_id = self.system_info.borrow()["virtualization"].get("virt_identifier").map(py_str).unwrap_or_else(|| "none".into());
            return Err(ServerError::bad(format!("Cannot {} from within a {} container", ep.rsplit('/').next().unwrap_or(""), virt_id)));
        }
        match ep.as_str() {
            "/machine/shutdown" => self.provider.shutdown().await?,
            "/machine/reboot" => self.provider.reboot().await?,
            _ => return Err(ServerError::bad("Unsupported machine request")),
        }
        Ok(json!("ok"))
    }

    pub async fn do_service_action(&self, action: &str, service_name: &str) -> Result<(), ServerError> {
        self.provider.do_service_action(action, service_name).await
    }

    pub fn restart_moonraker_service(self: &Rc<Self>) {
        let m = self.clone();
        spawn(async move {
            let _ = m.do_service_action("restart", &m.unit_name()).await;
        });
    }

    async fn handle_service_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let name = req.get_str("service")?;
        let action = req.get_endpoint().rsplit('/').next().unwrap_or("").to_string();
        if name == self.unit_name() {
            if action != "restart" {
                return Err(ServerError::bad(format!("Service action '{}' not available for moonraker", action)));
            }
            self.restart_moonraker_service();
        } else if self.provider.is_service_available(&name) {
            self.do_service_action(&action, &name).await?;
        } else {
            if self.allowed_services.borrow().contains(&name) {
                return Err(ServerError::bad(format!("Service '{}' not installed", name)));
            }
            return Err(ServerError::bad(format!("Service '{}' not allowed", name)));
        }
        Ok(json!("ok"))
    }

    async fn handle_sysinfo_request(self: Rc<Self>, _req: WebRequest) -> ApiResult {
        let mut si = self.get_system_info();
        si.insert("instance_ids".into(), json!({"moonraker": self.unit_name(), "klipper": server().klippy().unit_name()}));
        Ok(json!({"system_info": si}))
    }

    async fn set_sudo_password(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let _g = self.sudo_req_lock.lock().await;
        *self.sudo_password.borrow_mut() = Some(req.get_str("password")?);
        if !self.check_sudo_access(&[]).await {
            *self.sudo_password.borrow_mut() = None;
            return Err(ServerError::bad("Invalid password, sudo access was denied"));
        }
        let mut responses = vec!["Sudo password successfully set.".to_string()];
        let mut restart = false;
        let mut failed = Vec::new();
        let mut failed_msgs = Vec::new();
        if !self.sudo_requests.borrow().is_empty() {
            loop {
                let next = {
                    let mut r = self.sudo_requests.borrow_mut();
                    if r.is_empty() {
                        None
                    } else {
                        Some(r.remove(0))
                    }
                };
                let (cb, msg) = match next {
                    Some(n) => n,
                    None => break,
                };
                match cb().await {
                    Ok((m, need_restart)) => {
                        responses.push(m);
                        restart |= need_restart;
                    }
                    Err(e) => {
                        failed.push((cb, msg));
                        failed_msgs.push(e.message);
                    }
                }
            }
            if !failed.is_empty() {
                restart = false;
            }
            *self.sudo_requests.borrow_mut() = failed;
            if !restart && responses.len() > 1 {
                let m = self.clone();
                spawn(async move {
                    crate::utils::sleep_ordered(std::time::Duration::from_millis(50)).await;
                    let _ = server().send_event(
                        "machine:sudo_alert",
                        vec![json!({"sudo_requested": m.sudo_requested(), "request_messages": m.sudo_request_messages()})],
                    );
                });
            }
            if !failed_msgs.is_empty() {
                return Err(ServerError::new(failed_msgs.join("\n"), 500));
            }
            if restart {
                self.restart_moonraker_service();
                responses.push("Moonraker is currently in the process of restarting.".into());
            }
        }
        Ok(json!({"sudo_responses": responses, "is_restarting": restart}))
    }

    async fn handle_sudo_info(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let check = req.get_boolean_or("check_access", false)?;
        let has_sudo = if check { Value::Bool(self.check_sudo_access(&[]).await) } else { Value::Null };
        Ok(json!({
            "sudo_access": has_sudo,
            "linux_user": self.linux_user(),
            "sudo_requested": self.sudo_requested(),
            "request_messages": self.sudo_request_messages(),
        }))
    }

    async fn handle_serial_request(self: Rc<Self>, _req: WebRequest) -> ApiResult {
        let _g = self.periph_lock.lock().await;
        let devs = tokio::task::spawn_blocking(crate::sysfs_devs::find_serial_devices).await.unwrap_or_default();
        Ok(json!({"serial_devices": devs}))
    }

    async fn handle_usb_request(self: Rc<Self>, _req: WebRequest) -> ApiResult {
        let _g = self.periph_lock.lock().await;
        let usb_ids = PathBuf::from(server().data_path()).join("misc/usb.ids");
        let devs = tokio::task::spawn_blocking(move || {
            let mut data = crate::sysfs_devs::UsbIdData::new(&usb_ids);
            let mut list = crate::sysfs_devs::find_usb_devices();
            for dev in list.iter_mut() {
                let ids: Vec<String> = dev
                    .shift_remove("class_ids")
                    .and_then(|v| v.as_array().cloned())
                    .map(|a| a.iter().map(py_str).collect())
                    .unwrap_or_default();
                if ids.len() == 3 {
                    if let Value::Object(ci) = data.get_class_info(&ids[0], &ids[1], &ids[2]) {
                        for (k, v) in ci {
                            dev.insert(k, v);
                        }
                    }
                }
                let vid = dev.get("vendor_id").map(py_str).unwrap_or_default();
                let pid = dev.get("product_id").map(py_str).unwrap_or_default();
                if let Value::Object(pi) = data.get_product_info(&vid, &pid) {
                    for (k, v) in pi {
                        if dev.get(&k).map(|x| x.is_null()).unwrap_or(true) {
                            dev.insert(k, v);
                        }
                    }
                }
            }
            list
        })
        .await
        .unwrap_or_default();
        Ok(json!({"usb_devices": devs}))
    }

    async fn handle_can_query(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let iface = req.get_str_or("interface", "can0");
        let _g = self.periph_lock.lock().await;
        let uuids = crate::sysfs_devs::query_can_uuids(&iface).await?;
        Ok(json!({"can_uuids": uuids}))
    }

    async fn handle_video_request(self: Rc<Self>, _req: WebRequest) -> ApiResult {
        let _g = self.periph_lock.lock().await;
        let devs = tokio::task::spawn_blocking(crate::sysfs_devs::find_video_devices).await.unwrap_or_default();
        Ok(json!({"v4l2_devices": devs, "libcamera_devices": []}))
    }

    pub fn get_system_info(&self) -> Map<String, Value> {
        let mut si = self.system_info.borrow().clone();
        // python stores the provider's service dict itself, so the state is live
        if si.contains_key("service_state") {
            si.insert("service_state".into(), Value::Object(self.provider.get_available_services().into_iter().collect()));
        }
        si
    }

    pub fn sudo_password(&self) -> Option<String> {
        self.sudo_password.borrow().clone()
    }

    pub fn set_sudo_password_value(&self, pw: Option<String>) {
        *self.sudo_password.borrow_mut() = pw;
    }

    pub fn sudo_requested(&self) -> bool {
        !self.sudo_requests.borrow().is_empty()
    }

    pub fn linux_user(&self) -> String {
        crate::utils::get_user()
    }

    pub fn sudo_request_messages(&self) -> Vec<String> {
        self.sudo_requests.borrow().iter().map(|r| r.1.clone()).collect()
    }

    pub fn register_sudo_request(&self, cb: SudoCallback, message: &str) {
        self.sudo_requests.borrow_mut().push((cb, message.to_string()));
        let _ = server().send_event(
            "machine:sudo_alert",
            vec![json!({"sudo_requested": true, "request_messages": self.sudo_request_messages()})],
        );
    }

    pub async fn check_sudo_access(&self, cmds: &[&str]) -> bool {
        let cmds: Vec<&str> = if cmds.is_empty() { vec!["systemctl --version", "ls /root"] } else { cmds.to_vec() };
        for c in cmds {
            if self.exec_sudo_command(c, 1, 10.0).await.is_err() {
                return false;
            }
        }
        true
    }

    pub async fn exec_sudo_command(&self, command: &str, tries: usize, timeout: f64) -> Result<String, ServerError> {
        let pw = self.sudo_password.borrow().clone();
        let (full_cmd, input) = match &pw {
            Some(p) => (format!("sudo -S {}", command), Some(p.clone())),
            None => (format!("sudo {}", command), None),
        };
        let shell: Rc<ShellCommandFactory> = server().lookup_component("shell_command").ok_or_else(|| ServerError::new("shell", 500))?;
        shell
            .exec_cmd(&full_cmd, ExecOpts { proc_input: input.as_deref(), log_complete: false, attempts: tries, timeout, ..Default::default() })
            .await
            .map_err(ServerError::from)
    }

    fn check_inside_container(&self) -> Value {
        let (mut virt_type, mut virt_id) = ("none".to_string(), "none".to_string());
        if let Ok(data) = std::fs::read_to_string(CGROUP_PATH) {
            for ct in ["docker", "lxc"] {
                if data.contains(ct) {
                    self.inside_container.set(true);
                    virt_type = "container".into();
                    virt_id = ct.into();
                    log::info!("Container detected via cgroup: {}", ct);
                    break;
                }
            }
        }
        if !self.inside_container.get() {
            if let Ok(data) = std::fs::read_to_string(SCHED_PATH) {
                let proc_name = data.trim().lines().next().and_then(|l| l.split_whitespace().next()).unwrap_or("").to_string();
                if proc_name != "init" && proc_name != "systemd" {
                    self.inside_container.set(true);
                    virt_type = "container".into();
                    virt_id = "lxc".into();
                    if Path::new("/.dockerenv").exists() || Path::new("/.dockerinit").exists() {
                        virt_id = "docker".into();
                    }
                    log::info!("Container detected via sched: {}", virt_id);
                }
            }
        }
        json!({"virt_type": virt_type, "virt_identifier": virt_id})
    }

    async fn parse_network_interfaces(&self, sequence: u64, notify: bool) {
        if sequence % NETWORK_UPDATE_SEQUENCE != 0 {
            return;
        }
        // read from the kernel; `ip` is only needed for CAN bus details
        let (network, canbus) = match native_network() {
            Some(network) => (network, Map::new()),
            None => match self.ip_network().await {
                Some(r) => r,
                None => return,
            },
        };
        let prev = self.system_info.borrow().get("network").cloned().unwrap_or(json!({}));
        let network_v = Value::Object(network);
        if network_v != prev {
            self.find_public_ip();
            if notify {
                let _ = server().send_event("machine:net_state_changed", vec![network_v.clone()]);
            }
        }
        let mut si = self.system_info.borrow_mut();
        si.insert("network".into(), network_v);
        si.insert("canbus".into(), Value::Object(canbus));
    }

    async fn ip_network(&self) -> Option<(Map<String, Value>, Map<String, Value>)> {
        let shell: Rc<ShellCommandFactory> = server().lookup_component("shell_command")?;
        let resp = match self.addr_cmd.run_with_response(&shell, 2.0, 1, false, 1, None, None).await {
            Ok(r) => r,
            Err(e) => {
                log::error!("Error processing network update\n{}", e);
                return None;
            }
        };
        let decoded: Vec<Value> = match serde_json::from_str(&resp) {
            Ok(v) => v,
            Err(e) => {
                log::error!("Error processing network update\n{}", e);
                return None;
            }
        };
        let mut network = Map::new();
        let mut canbus = Map::new();
        for intf in decoded {
            if intf.get("operstate").and_then(|v| v.as_str()) != Some("UP") {
                continue;
            }
            let link_type = intf.get("link_type").and_then(|v| v.as_str()).unwrap_or("");
            let ifname = intf.get("ifname").map(py_str).unwrap_or_default();
            if link_type == "can" {
                let info = intf.get("linkinfo").and_then(|l| l.get("info_data")).cloned().unwrap_or(json!({}));
                canbus.insert(
                    ifname,
                    json!({
                        "tx_queue_len": intf.get("txqlen").cloned().unwrap_or(Value::Null),
                        "bitrate": info.get("bittiming").and_then(|b| b.get("bitrate")).cloned().unwrap_or(json!(-1)),
                        "driver": info.get("bittiming_const").and_then(|b| b.get("name")).cloned().unwrap_or(json!("unknown")),
                    }),
                );
            } else if link_type == "ether" && intf.get("address").is_some() {
                let addrs: Vec<Value> = intf
                    .get("addr_info")
                    .and_then(|a| a.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .filter(|a| a.get("family").is_some() && a.get("local").is_some())
                    .filter_map(|a| {
                        let fam = match a["family"].as_str()? {
                            "inet" => "ipv4",
                            "inet6" => "ipv6",
                            _ => return None,
                        };
                        Some(json!({
                            "family": fam,
                            "address": a["local"],
                            "is_link_local": a.get("scope").and_then(|s| s.as_str()) == Some("link"),
                        }))
                    })
                    .collect();
                if addrs.is_empty() {
                    continue;
                }
                network.insert(ifname, json!({"mac_address": intf["address"], "ip_addresses": addrs}));
            }
        }
        Some((network, canbus))
    }

    pub async fn get_public_network(&self) -> Value {
        let wifis = self.get_wifi_interfaces().await;
        let mut intf = self.find_public_interface();
        let ifname = intf["ifname"].as_str().unwrap_or("").to_string();
        let is_wifi = wifis.contains_key(&ifname);
        intf["is_wifi"] = json!(is_wifi);
        if is_wifi {
            intf["ssid"] = wifis[&ifname].clone();
        }
        intf["hostname"] = json!(crate::utils::hostname());
        intf
    }

    fn find_public_interface(&self) -> Value {
        let src_ip = self.find_public_ip();
        let networks = self.system_info.borrow().get("network").cloned().unwrap_or(json!({}));
        if let Some(n) = networks.as_object() {
            for (ifname, info) in n {
                for ai in info["ip_addresses"].as_array().cloned().unwrap_or_default() {
                    if ai["is_link_local"].as_bool().unwrap_or(false) {
                        continue;
                    }
                    let fam = ai["family"].as_str().unwrap_or("");
                    let addr = ai["address"].as_str().unwrap_or("");
                    if fam == "ipv6" && src_ip.is_empty() {
                        if let Ok(std::net::IpAddr::V6(ip)) = addr.parse::<std::net::IpAddr>() {
                            if is_global_v6(&ip) {
                                return json!({"ifname": ifname, "address": addr, "family": fam});
                            }
                        }
                    } else if src_ip == addr {
                        return json!({"ifname": ifname, "address": addr, "family": fam});
                    }
                }
            }
        }
        json!({"ifname": "", "address": src_ip, "family": ""})
    }

    fn find_public_ip(&self) -> String {
        let mut src_ip = String::new();
        let attempts: [(&str, u16, bool); 3] = [("255.255.255.255", 0, true), ("10.255.255.255", 1, false), ("2001:db8::1234", 1, false)];
        for (addr, port, broadcast) in attempts {
            let bind = if addr.contains(':') { "[::]:0" } else { "0.0.0.0:0" };
            let sock = match std::net::UdpSocket::bind(bind) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if broadcast && sock.set_broadcast(true).is_err() {
                continue;
            }
            let target = if addr.contains(':') { format!("[{}]:{}", addr, port) } else { format!("{}:{}", addr, port) };
            if sock.connect(&target).is_err() {
                continue;
            }
            match sock.local_addr() {
                Ok(a) => {
                    src_ip = a.ip().to_string();
                    log::info!("Detected Local IP: {}", src_ip);
                    break;
                }
                Err(_) => continue,
            }
        }
        if src_ip != *self.public_ip.borrow() {
            *self.public_ip.borrow_mut() = src_ip.clone();
            let _ = server().send_event("machine:public_ip_changed", vec![json!(src_ip)]);
        }
        if src_ip.is_empty() {
            log::info!("Failed to detect local IP address");
        }
        src_ip
    }

    async fn get_wifi_interfaces(&self) -> Map<String, Value> {
        let shell: Rc<ShellCommandFactory> = match server().lookup_component("shell_command") {
            Some(s) => s,
            None => return Map::new(),
        };
        let resp = match self.iwgetid_cmd.run_with_response(&shell, 2.0, 1, false, 1, None, None).await {
            Ok(r) => r,
            Err(_) => {
                log::info!("Failed to run 'iwgetid' command");
                return Map::new();
            }
        };
        let mut out = Map::new();
        for line in resp.split('\n') {
            let mut parts = line.trim().splitn(2, char::is_whitespace);
            if let (Some(a), Some(b)) = (parts.next(), parts.next()) {
                out.insert(a.to_string(), json!(b.trim().rsplit(':').next().unwrap_or("").trim_matches('"')));
            }
        }
        out
    }

    pub fn log_service_info(&self, svc_info: &Value) {
        let o = match svc_info.as_object() {
            Some(o) if !o.is_empty() => o,
            _ => return,
        };
        let name = o.get("unit_name").map(py_str).unwrap_or_else(|| "unknown".into());
        let manager = o.get("manager").map(py_str).unwrap_or_else(|| "systemd".into());
        let manager = capitalize(&manager);
        let mut msg = format!("\n{} unit {}:", manager, name);
        for (k, v) in o {
            if k == "properties" {
                msg.push_str("\nProperties:");
                if let Some(p) = v.as_object() {
                    for (pk, pv) in p {
                        msg.push_str(&format!("\n**{}={}", pk, py_str(pv)));
                    }
                }
            } else {
                msg.push_str(&format!("\n{}: {}", k, py_str(v)));
            }
        }
        server().add_log_rollover_item(&name, &msg, true);
    }

    pub async fn update_usb_ids(&self, force: bool) {
        let _g = self.periph_lock.lock().await;
        let srv = server();
        let usb_ids_path = PathBuf::from(srv.data_path()).join("misc/usb.ids");
        if usb_ids_path.is_file() && !force {
            return;
        }
        let db = srv.database();
        let mut info = db.get_item_sync("moonraker", Some(&json!("usb_id_req_info")), Some(json!({}))).unwrap_or(json!({}));
        let etag = info.as_object_mut().and_then(|o| o.shift_remove("etag"));
        let last_modified = info.as_object_mut().and_then(|o| o.shift_remove("last_modified"));
        let mut headers = vec![("Accept".to_string(), "text/plain".to_string())];
        if let (Some(e), true) = (&etag, usb_ids_path.is_file()) {
            headers.push(("If-None-Match".into(), py_str(e)));
        }
        if let (Some(l), true) = (&last_modified, usb_ids_path.is_file()) {
            headers.push(("If-Modified-Since".into(), py_str(l)));
        }
        log::info!("Fetching latest usb.ids file...");
        let client: Rc<crate::components::http_client::HttpClient> = match srv.lookup_component("http_client") {
            Some(c) => c,
            None => return,
        };
        let resp = client.get(USB_IDS_URL, &headers, false).await;
        if resp.has_error() {
            log::info!("Failed to retrieve usb.ids file");
            return;
        }
        if resp.status_code == 304 {
            log::info!("USB IDs file up to date");
            return;
        }
        if let Some(o) = info.as_object_mut() {
            if let Some(e) = resp.etag() {
                o.insert("etag".into(), json!(e));
            }
            if let Some(l) = resp.last_modified() {
                o.insert("last_modified".into(), json!(l));
            }
        }
        let _ = db.insert_item_sync("moonraker", &json!("usb_id_req_info"), info);
        log::info!("Writing usb.ids file...");
        if let Some(p) = usb_ids_path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        let _ = std::fs::write(&usb_ids_path, &resp.content);
    }
}

/// The pid file of an init script whose `status)` is only the usual
/// `[ -f $PIDFILE ] && kill -0 $(cat $PIDFILE)` check
fn sysv_status_pidfile(script: &str) -> Option<PathBuf> {
    let pidfile = script.lines().find_map(|l| l.trim().strip_prefix("PIDFILE="))?.trim_matches('"').to_string();
    if !pidfile.starts_with('/') || pidfile.contains('$') {
        return None;
    }
    let status = script.split_once("status)")?.1;
    let status = status.split(";;").next()?;
    let only_pid_check = status.contains("kill -0")
        && status.lines().filter(|l| l.contains("kill -0")).all(|l| l.contains("PIDFILE") || l.contains(&pidfile));
    only_pid_check.then(|| PathBuf::from(pidfile))
}

/// The interfaces `ip -json -det address` reports: up, ethernet type, with
/// addresses (getifaddrs order, as ip's).  None when a CAN interface is up.
fn native_network() -> Option<Map<String, Value>> {
    let sys = |ifname: &str, f: &str| std::fs::read_to_string(format!("/sys/class/net/{}/{}", ifname, f)).ok().map(|s| s.trim().to_string());
    let mut ifaces: Vec<(i64, String)> = std::fs::read_dir("/sys/class/net")
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| sys(n, "operstate").as_deref() == Some("up"))
        .map(|n| (sys(&n, "ifindex").and_then(|i| i.parse().ok()).unwrap_or(0), n))
        .collect();
    ifaces.sort();
    let mut addrs: IndexMap<String, Vec<Value>> = IndexMap::new();
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return None;
        }
        let mut cur = head;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy().to_string();
            let entry = match (*ifa.ifa_addr).sa_family as i32 {
                libc::AF_INET => {
                    let a = std::net::Ipv4Addr::from(u32::from_be((*(ifa.ifa_addr as *const libc::sockaddr_in)).sin_addr.s_addr));
                    json!({"family": "ipv4", "address": a.to_string(), "is_link_local": a.is_link_local()})
                }
                libc::AF_INET6 => {
                    let a = std::net::Ipv6Addr::from((*(ifa.ifa_addr as *const libc::sockaddr_in6)).sin6_addr.s6_addr);
                    json!({"family": "ipv6", "address": a.to_string(), "is_link_local": a.segments()[0] & 0xffc0 == 0xfe80})
                }
                _ => continue,
            };
            addrs.entry(name).or_default().push(entry);
        }
        libc::freeifaddrs(head);
    }
    let mut network = Map::new();
    for (_, ifname) in ifaces {
        match sys(&ifname, "type").as_deref() {
            Some("280") => return None,
            Some("1") => {}
            _ => continue,
        }
        let ips = addrs.shift_remove(&ifname).unwrap_or_default();
        let mac = sys(&ifname, "address");
        if ips.is_empty() || mac.is_none() {
            continue;
        }
        network.insert(ifname, json!({"mac_address": mac, "ip_addresses": ips}));
    }
    Some(network)
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
        None => String::new(),
    }
}

fn is_global_v6(ip: &std::net::Ipv6Addr) -> bool {
    let seg = ip.segments();
    !(ip.is_loopback() || ip.is_unspecified() || (seg[0] & 0xfe00) == 0xfc00 || (seg[0] & 0xffc0) == 0xfe80 || (seg[0] == 0x2001 && seg[1] == 0xdb8))
        && (seg[0] & 0xe000) == 0x2000
}

fn python_version() -> (Value, String) {
    let out = std::process::Command::new("python3")
        .args(["-c", "import sys,json;print(json.dumps([list(sys.version_info), sys.version.replace(chr(10), ' ')]))"])
        .output();
    if let Ok(o) = out {
        if let Ok(v) = serde_json::from_slice::<Value>(&o.stdout) {
            if let Some(a) = v.as_array() {
                if a.len() == 2 {
                    return (a[0].clone(), py_str(&a[1]));
                }
            }
        }
    }
    (json!([0, 0, 0, "final", 0]), "unknown".into())
}

fn get_cpu_info() -> Value {
    let arch = machine_arch();
    let bits = if std::mem::size_of::<usize>() == 8 { "64bit" } else { "32bit" };
    let mut info = json!({
        "cpu_count": std::thread::available_parallelism().map(|n| n.get()).ok(),
        "bits": bits,
        "processor": arch,
        "cpu_desc": "",
        "serial_number": "",
        "hardware_desc": "",
        "model": "",
        "total_memory": null,
        "memory_units": "",
    });
    if let Ok(cnt) = std::fs::read_to_string("/proc/cpuinfo") {
        let items: Vec<&str> = cnt.trim().split("\n\n").map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
        let desc_re = Regex::new(r"model name\s+:\s+(.+)").unwrap();
        for item in &items {
            if let Some(c) = desc_re.captures(item) {
                info["cpu_desc"] = json!(c[1].trim());
                break;
            }
        }
        if let Some(last) = items.last() {
            if let Some(c) = Regex::new(r"Hardware\s+:\s+(.+)").unwrap().captures(last) {
                info["hardware_desc"] = json!(c[1].trim());
            }
            if let Some(c) = Regex::new(r"Serial\s+:\s+0*(.+)").unwrap().captures(last) {
                info["serial_number"] = json!(c[1].trim());
            }
            if let Some(c) = Regex::new(r"Model\s+:\s+(.+)").unwrap().captures(last) {
                info["model"] = json!(c[1].trim());
            }
        }
    }
    if let Ok(mem) = std::fs::read_to_string("/proc/meminfo") {
        for line in mem.trim().split('\n') {
            let line = line.trim();
            if line.starts_with("MemTotal:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 3 {
                    info["total_memory"] = json!(parts[1].parse::<i64>().unwrap_or(0));
                    info["memory_units"] = json!(parts[2]);
                }
                break;
            }
        }
    }
    info
}

fn get_sdcard_info() -> Value {
    let cid = match std::fs::read_to_string(SD_CID_PATH) {
        Ok(c) => c.trim().to_lowercase(),
        Err(_) => return json!({}),
    };
    let parse = || -> Option<Map<String, Value>> {
        let mut m = Map::new();
        let mid = cid.get(..2)?.to_string();
        m.insert("manufacturer_id".into(), json!(mid));
        m.insert(
            "manufacturer".into(),
            json!(match mid.as_str() {
                "1b" => "Samsung",
                "03" => "Sandisk",
                "74" => "PNY",
                _ => "Unknown",
            }),
        );
        m.insert("oem_id".into(), json!(cid.get(2..6)?));
        let pn = hex::decode(cid.get(6..16)?).ok()?;
        m.insert("product_name".into(), json!(pn.iter().filter(|b| b.is_ascii()).map(|b| *b as char).collect::<String>()));
        let r1 = u8::from_str_radix(cid.get(16..17)?, 16).ok()?;
        let r2 = u8::from_str_radix(cid.get(17..18)?, 16).ok()?;
        m.insert("product_revision".into(), json!(format!("{}.{}", r1, r2)));
        m.insert("serial_number".into(), json!(cid.get(18..26)?));
        let year = i64::from_str_radix(cid.get(27..29)?, 16).ok()? + 2000;
        let month = i64::from_str_radix(cid.get(29..30)?, 16).ok()?;
        m.insert("manufacturer_date".into(), json!(format!("{}/{}", month, year)));
        Some(m)
    };
    let mut sd = match parse() {
        Some(m) => m,
        None => {
            log::info!("Error reading SDCard CID Register");
            return json!({});
        }
    };
    sd.insert("capacity".into(), json!("Unknown"));
    sd.insert("total_bytes".into(), json!(0));
    let csd = std::fs::read_to_string(SD_CSD_PATH).ok().and_then(|c| hex::decode(c.trim()).ok());
    match csd {
        Some(r) if r.len() >= 11 => {
            let t = (r[0] >> 6) & 0x3;
            let (total, cap): (u64, String) = match t {
                0 => {
                    let max_block_len = 2u64.pow((r[5] & 0xF) as u32);
                    let c_size = (((r[6] & 0x3) as u64) << 10) | ((r[7] as u64) << 2) | (((r[8] >> 6) & 0x3) as u64);
                    let c_mult_reg = (((r[9] & 0x3) as u32) << 1) | ((r[10] >> 7) as u32);
                    let total = (c_size + 1) * 2u64.pow(c_mult_reg + 2) * max_block_len;
                    (total, format!("{:.1} MiB", total as f64 / 1024f64.powi(2)))
                }
                1 => {
                    let c_size = (((r[7] & 0x3F) as u64) << 16) | ((r[8] as u64) << 8) | r[9] as u64;
                    let total = (c_size + 1) * 512 * 1024;
                    (total, format!("{:.1} GiB", total as f64 / 1024f64.powi(3)))
                }
                2 => {
                    let c_size = (((r[6] & 0xF) as u64) << 24) | ((r[7] as u64) << 16) | ((r[8] as u64) << 8) | r[9] as u64;
                    let total = (c_size + 1) * 512 * 1024;
                    (total, format!("{:.1} TiB", total as f64 / 1024f64.powi(4)))
                }
                _ => return Value::Object(sd),
            };
            sd.insert("capacity".into(), json!(cap));
            sd.insert("total_bytes".into(), json!(total));
        }
        _ => log::info!("Error Reading SDCard CSD Register"),
    }
    Value::Object(sd)
}

// ***** Install validation *****

const INSTALL_VERSION: i64 = 1;
const SERVICE_VERSION: i64 = 1;

pub struct InstallValidator {
    config: ConfigHelper,
    force_validation: bool,
    sc_enabled: Cell<bool>,
    cc_enabled: Cell<bool>,
    data_path: RefCell<PathBuf>,
    backup_path: RefCell<Option<PathBuf>>,
    data_path_valid: Cell<bool>,
    sudo_requested: Cell<bool>,
    announcement_id: RefCell<String>,
    pub validation_enabled: Cell<bool>,
}

impl InstallValidator {
    fn new(config: &ConfigHelper) -> Result<Rc<Self>, crate::confighelper::ConfigError> {
        let srv = server();
        srv.load_component(config, "template")?;
        let v = Rc::new(InstallValidator {
            config: config.clone(),
            force_validation: config.getboolean_or("force_validation", false)?,
            sc_enabled: Cell::new(config.getboolean_or("validate_service", true)?),
            cc_enabled: Cell::new(config.getboolean_or("validate_config", true)?),
            data_path: RefCell::new(PathBuf::from(srv.data_path())),
            backup_path: RefCell::new(None),
            data_path_valid: Cell::new(true),
            sudo_requested: Cell::new(false),
            announcement_id: RefCell::new(String::new()),
            validation_enabled: Cell::new(false),
        });
        v.update_backup_path();
        Ok(v)
    }

    fn update_backup_path(&self) {
        let str_time = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let mut bp = self.backup_path.borrow_mut();
        if bp.as_ref().map(|p| !p.exists()).unwrap_or(true) {
            *bp = Some(self.data_path.borrow().join(format!("backup/{}", str_time)));
        }
    }

    async fn validation_init(&self) {
        let db = server().database();
        let key = json!("validate_install.install_version");
        let mut install_ver = db.get_item_sync("moonraker", Some(&key), Some(Value::Null)).unwrap_or(Value::Null);
        if install_ver.is_null() {
            let _ = db.insert_item_sync("moonraker", &key, json!(INSTALL_VERSION));
            install_ver = json!(INSTALL_VERSION);
        }
        if install_ver.as_i64().unwrap_or(0) < INSTALL_VERSION {
            log::info!("Validation version in database out of date");
            self.validation_enabled.set(true);
        } else {
            let mut msg = "Installation version in database up to date".to_string();
            if self.force_validation {
                msg.push_str(", force is enabled");
            }
            log::info!("{}", msg);
            self.validation_enabled.set(self.force_validation);
        }
        let is_bkp = server().get_app_arg("is_backup_config").and_then(|v| v.as_bool()).unwrap_or(false);
        if self.validation_enabled.get() && is_bkp {
            server().add_warning(
                "Backup configuration loaded, aborting install validation. Please correct the configuration issue and restart moonraker.",
                None,
                true,
            );
            self.validation_enabled.set(false);
        }
    }

    async fn perform_validation(self: &Rc<Self>) -> bool {
        if !self.validation_enabled.get() {
            return false;
        }
        let srv = server();
        let fm: Option<Rc<crate::components::file_manager::FileManager>> = srv.lookup_component("file_manager");
        let mut name = "service";
        let res = match self.check_service_file().await {
            Ok(r1) => {
                name = "config";
                self.check_configuration().map(|r2| r1 | r2)
            }
            Err(e) => Err(e),
        };
        let mut need_restart = false;
        match res {
            Err(ValidationResult::Validation(msg)) => {
                srv.add_warning(&msg, None, true);
                if let Some(f) = &fm {
                    f.disable_write_access();
                }
                return false;
            }
            Err(ValidationResult::Other(msg)) => {
                srv.add_warning(&format!("Failed to validate {}: {}", name, msg), None, true);
                if let Some(f) = &fm {
                    f.disable_write_access();
                }
                return false;
            }
            Ok(r) => {
                need_restart = r;
                self.validation_enabled.set(false);
                let _ = srv.database().insert_item_sync("moonraker", &json!("validate_install.install_version"), json!(INSTALL_VERSION));
            }
        }
        if need_restart {
            if let Some(m) = srv.lookup_component::<Machine>("machine") {
                m.restart_moonraker_service();
            }
            return true;
        }
        false
    }

    async fn check_service_file(self: &Rc<Self>) -> Result<bool, ValidationResult> {
        if !self.sc_enabled.get() {
            return Ok(false);
        }
        let srv = server();
        let machine: Rc<Machine> = srv.lookup_component("machine").ok_or_else(|| ValidationResult::Other("machine".into()))?;
        if machine.is_inside_container() {
            return Err(ValidationResult::Validation(
                "Moonraker instance running inside a container, cannot validate service file.".into(),
            ));
        }
        if machine.get_provider_type() == "none" {
            return Err(ValidationResult::Validation("No machine provider configured, cannot validate service file.".into()));
        }
        log::info!("Performing Service Validation...");
        let svc_info = machine.get_moonraker_service_info();
        if svc_info.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            return Err(ValidationResult::Validation(
                "Unable to retrieve Moonraker service info.  Service file must be updated manually.".into(),
            ));
        }
        let props = svc_info.get("properties").cloned().unwrap_or(json!({}));
        let fragment = match props.get("FragmentPath") {
            Some(f) => py_str(f),
            None => {
                return Err(ValidationResult::Validation(
                    "Unable to locate path to Moonraker's service unit. Service file must be must be updated manually.".into(),
                ))
            }
        };
        let desc = props.get("Description").map(py_str).unwrap_or_default();
        if let Some(c) = Regex::new(r"^API Server for Klipper SV(\d+)").unwrap().captures(&desc) {
            if c[1].parse::<i64>().unwrap_or(0) == SERVICE_VERSION {
                log::info!("Service file validated and up to date");
                return Ok(false);
            }
        }
        let unit = svc_info.get("unit_name").map(py_str).unwrap_or_default().split('.').next().unwrap_or("").to_string();
        if unit.is_empty() {
            return Err(ValidationResult::Validation(
                "Unable to retrieve service unit name.  Service file must be updated manually.".into(),
            ));
        }
        let app_args = srv.get_app_args();
        if unit != "moonraker" {
            log::info!("Custom service file detected: {}", unit);
            if app_args.get("is_default_data_path").and_then(|v| v.as_bool()).unwrap_or(false) && self.data_path_valid.get() {
                let mut df = format!("~/{}_data", unit);
                if let Some(c) = Regex::new(r"^moonraker[-_]?(\d+)").unwrap().captures(&unit) {
                    df = format!("~/printer_{}_data", &c[1]);
                }
                let new_dp = crate::utils::resolve_path(&crate::utils::expand_user(&df));
                if new_dp.exists() && !check_path_bare(&new_dp) {
                    return Err(ValidationResult::Validation(format!(
                        "Cannot resolve data path for custom unit '{}', data path '{}' already exists.  Service file must be updated manually.",
                        unit,
                        new_dp.display()
                    )));
                }
                let cur = self.data_path.borrow().clone();
                if cur.exists() && check_path_bare(&cur) {
                    let _ = std::fs::remove_dir_all(&cur);
                }
                *self.data_path.borrow_mut() = new_dp.clone();
                if !new_dp.exists() {
                    log::info!("New data path created at {}", new_dp.display());
                    let _ = std::fs::create_dir(&new_dp);
                }
                self.data_path_valid.set(false);
            }
        }
        let user = props.get("User").map(py_str).ok_or_else(|| ValidationResult::Other("'User'".into()))?;
        let mut has_sudo = false;
        if machine.check_sudo_access(&[]).await {
            has_sudo = true;
            log::info!("Moonraker has sudo access");
        } else if user == "pi" && machine.sudo_password().is_none() {
            machine.set_sudo_password_value(Some("raspberry".into()));
            has_sudo = machine.check_sudo_access(&[]).await;
        }
        if !has_sudo {
            self.request_sudo_access();
            return Err(ValidationResult::Validation(
                "Moonraker requires sudo permission to update the system service. Please check your notifications for further instructions.".into(),
            ));
        }
        self.sudo_requested.set(false);
        let svc_dest = PathBuf::from(&fragment);
        let tmp_svc = std::env::temp_dir().join(format!("{}-tmp.svc", unit));
        let data_path = self.data_path.borrow().clone();
        let sysd_data = data_path.join("systemd");
        let _ = std::fs::create_dir_all(&sysd_data);
        let env_file = sysd_data.join("moonraker.env");
        let mut env_vars: IndexMap<String, String> = IndexMap::new();
        env_vars.insert("MOONRAKER_DATA_PATH".into(), data_path.to_string_lossy().to_string());
        let cfg_file = PathBuf::from(app_args.get("config_file").map(py_str).unwrap_or_default());
        let fm: Option<Rc<crate::components::file_manager::FileManager>> = srv.lookup_component("file_manager");
        let cfg_path = fm.as_ref().map(|f| f.get_directory("config")).unwrap_or_default();
        let log_path = fm.as_ref().map(|f| f.get_directory("logs")).unwrap_or_default();
        let same = |a: &Path, b: &str| -> bool {
            !b.is_empty() && std::fs::canonicalize(a).ok() == std::fs::canonicalize(b).ok()
        };
        if cfg_path.is_empty() || !same(cfg_file.parent().unwrap_or(Path::new("/")), &cfg_path) {
            env_vars.insert("MOONRAKER_CONFIG_PATH".into(), cfg_file.to_string_lossy().to_string());
        } else if cfg_file.file_name().map(|n| n != "moonraker.conf").unwrap_or(false) {
            let f = data_path.join(format!("config/{}", cfg_file.file_name().unwrap().to_string_lossy()));
            env_vars.insert("MOONRAKER_CONFIG_PATH".into(), f.to_string_lossy().to_string());
        }
        let log_file = app_args.get("log_file").map(py_str).unwrap_or_default();
        if log_file.is_empty() {
            env_vars.insert("MOONRAKER_DISABLE_FILE_LOG".into(), "y".into());
        } else {
            let lf = PathBuf::from(&log_file);
            if log_path.is_empty() || !same(lf.parent().unwrap_or(Path::new("/")), &log_path) || lf.file_name().map(|n| n != "moonraker.log").unwrap_or(false) {
                env_vars.insert("MOONRAKER_LOG_PATH".into(), log_file.clone());
            }
        }
        self.update_backup_path();
        let bp = self.backup_path.borrow().clone().unwrap();
        let svc_bkp = bp.join("service");
        let _ = std::fs::create_dir_all(&svc_bkp);
        if env_file.exists() {
            let _ = std::fs::copy(&env_file, svc_bkp.join(env_file.file_name().unwrap()));
        }
        let _ = std::fs::copy(&svc_dest, svc_bkp.join(svc_dest.file_name().unwrap_or_default()));
        let exec_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/usr/local/bin/moonruster"));
        let unit_text = format!(
            "\n# systemd service file for moonraker\n[Unit]\nDescription=API Server for Klipper SV{}\nRequires=network-online.target\nAfter=network-online.target\n\n[Install]\nWantedBy=multi-user.target\n\n[Service]\nType=simple\nUser={}\nSupplementaryGroups=moonraker-admin\nRemainAfterExit=yes\nEnvironmentFile={}\nExecStart={} $MOONRAKER_ARGS\nRestart=always\nRestartSec=10\n",
            SERVICE_VERSION,
            user,
            env_file.display(),
            exec_path.display()
        );
        let _ = std::fs::write(&tmp_svc, unit_text);
        let envout: Vec<String> = env_vars.iter().map(|(k, v)| format!("{}=\"{}\"", k, v)).collect();
        let res: Result<(), ServerError> = async {
            std::fs::write(&env_file, envout.join("\n"))?;
            machine.exec_sudo_command(&format!("cp -f {} {}", tmp_svc.display(), svc_dest.display()), 5, 60.0).await?;
            machine.exec_sudo_command("systemctl daemon-reload", 5, 60.0).await?;
            Ok(())
        }
        .await;
        let _ = std::fs::remove_file(&tmp_svc);
        if let Err(e) = res {
            log::error!("Failed to update moonraker service unit\n{}", e);
            return Err(ValidationResult::Validation(format!(
                "Failed to update service unit file '{}'. Update must be performed manually.",
                svc_dest.display()
            )));
        }
        self.data_path_valid.set(true);
        self.sc_enabled.set(false);
        Ok(true)
    }

    fn link_data_subfolder(&self, folder: &str, source: &str, exist_ok: bool) -> Result<(), ValidationResult> {
        let source_dir = crate::utils::resolve_path(&crate::utils::expand_user(source));
        let subfolder = self.data_path.borrow().join(folder);
        if !source_dir.exists() {
            log::info!("Source path '{}' does not exist.  Falling back to default folder {}", source_dir.display(), subfolder.display());
            return Ok(());
        }
        if !source_dir.is_dir() {
            return Err(ValidationResult::Validation(format!(
                "Failed to link subfolder '{}' to source path '{}'.  The requested path is not a valid directory.",
                folder,
                source_dir.display()
            )));
        }
        if subfolder.is_symlink() {
            if std::fs::canonicalize(&subfolder).ok() != std::fs::canonicalize(&source_dir).ok() {
                if exist_ok {
                    log::info!("Folder {} already linked, aborting link to {}", subfolder.display(), source_dir.display());
                    return Ok(());
                }
                return Err(ValidationResult::Validation(format!(
                    "Failed to link subfolder '{}' to '{}'.  '{}' already exists and is linked to {}.  This conflict requires manual resolution.",
                    folder,
                    source_dir.display(),
                    folder,
                    subfolder.display()
                )));
            }
            return Ok(());
        }
        if !subfolder.exists() {
            std::os::unix::fs::symlink(&source_dir, &subfolder).map_err(|e| ValidationResult::Other(e.to_string()))?;
            return Ok(());
        }
        if subfolder.is_dir() && std::fs::read_dir(&subfolder).map(|mut r| r.next().is_none()).unwrap_or(false) {
            let _ = std::fs::remove_dir(&subfolder);
            std::os::unix::fs::symlink(&source_dir, &subfolder).map_err(|e| ValidationResult::Other(e.to_string()))?;
            return Ok(());
        }
        if exist_ok {
            log::info!("Path at {} exists, aborting link to {}", subfolder.display(), source_dir.display());
            return Ok(());
        }
        Err(ValidationResult::Validation(format!(
            "Failed to link subfolder '{}' to '{}'.  Folder '{}' already exists.  This conflict requires manual resolution.",
            folder,
            source_dir.display(),
            folder
        )))
    }

    fn link_data_file(&self, data_file: &Path, target: &str) -> Result<(), ValidationResult> {
        let target = crate::utils::resolve_path(&crate::utils::expand_user(target));
        let name = data_file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if !target.exists() {
            log::info!("Target file {} does not exist.  Aborting symbolic link to {}.", target.display(), name);
            return Ok(());
        }
        if !target.is_file() {
            return Err(ValidationResult::Validation(format!(
                "Failed to link data file {}.  Target {} is not a valid file.",
                name,
                target.display()
            )));
        }
        if data_file.is_symlink() {
            if std::fs::canonicalize(data_file).ok() != std::fs::canonicalize(&target).ok() {
                return Err(ValidationResult::Validation(format!(
                    "Failed to link data file {}.  Link to {} already exists. This conflict must be resolved manually.",
                    name,
                    crate::utils::resolve_path(data_file).display()
                )));
            }
            return Ok(());
        }
        if !data_file.exists() {
            std::os::unix::fs::symlink(&target, data_file).map_err(|e| ValidationResult::Other(e.to_string()))?;
            return Ok(());
        }
        Err(ValidationResult::Validation(format!(
            "Failed to link data file {}.  File already exists. This conflict must be resolved manually.",
            name
        )))
    }

    fn check_configuration(&self) -> Result<bool, ValidationResult> {
        if !self.cc_enabled.get() || !self.data_path_valid.get() {
            return Ok(false);
        }
        let db = server().database();
        let source = self.config.source.clone();
        source.backup_source();
        let res: Result<(), ValidationResult> = (|| {
            self.update_backup_path();
            let bkp = self.backup_path.borrow().clone().unwrap().join("config");
            std::fs::create_dir_all(&bkp).map_err(|e| ValidationResult::Other(e.to_string()))?;
            source.write_config(&bkp).map_err(|e| ValidationResult::Other(e.to_string()))?;
            let server_cfg = self.config.getsection("server");
            let db_cfg = self.config.getsection("database");
            let opt = |c: &ConfigHelper, o: &str| c.get_opt(o).ok().flatten();
            let db_path = opt(&db_cfg, "database_path");
            let default_db = crate::utils::expand_user("~/.moonraker_database");
            match db_path {
                None if default_db.exists() => self.link_data_subfolder("database", &default_db.to_string_lossy(), true)?,
                Some(p) => {
                    self.link_data_subfolder("database", &p, false)?;
                    let _ = source.remove_option("database", "database_path");
                }
                _ => {}
            }
            let fm_cfg = self.config.getsection("file_manager");
            if let Some(p) = opt(&fm_cfg, "config_path").or_else(|| opt(&server_cfg, "config_path")) {
                self.link_data_subfolder("config", &p, false)?;
                let _ = source.remove_option("server", "config_path");
                let _ = source.remove_option("file_manager", "config_path");
            }
            if let Some(p) = opt(&fm_cfg, "log_path").or_else(|| opt(&server_cfg, "log_path")) {
                self.link_data_subfolder("logs", &p, false)?;
                let _ = source.remove_option("server", "log_path");
                let _ = source.remove_option("file_manager", "log_path");
            }
            let gc = db.get_item_sync("moonraker", Some(&json!("file_manager.gcode_path")), Some(Value::Null)).unwrap_or(Value::Null);
            if !gc.is_null() {
                self.link_data_subfolder("gcodes", &py_str(&gc), false)?;
                let _ = db.delete_item_sync("moonraker", &json!("file_manager.gcode_path"));
            }
            let sec_cfg = self.config.getsection("secrets");
            if let Some(sp) = opt(&sec_cfg, "secrets_path") {
                let dest = self.data_path.borrow().join("moonraker.secrets");
                self.link_data_file(&dest, &sp)?;
                let _ = source.remove_option("secrets", "secrets_path");
            }
            let certs = self.data_path.borrow().join("certs");
            let _ = std::fs::create_dir_all(&certs);
            if let Some(c) = opt(&server_cfg, "ssl_certificate_path") {
                self.link_data_file(&certs.join("moonraker.cert"), &c)?;
                let _ = source.remove_option("server", "ssl_certificate_path");
            }
            if let Some(k) = opt(&server_cfg, "ssl_key_path") {
                self.link_data_file(&certs.join("moonraker.key"), &k)?;
                let _ = source.remove_option("server", "ssl_key_path");
            }
            if server_cfg.has_option("enable_debug_logging") {
                let _ = source.remove_option("server", "enable_debug_logging");
            }
            let um = self.config.getsection("update_manager");
            if um.has_option("enable_repo_debug") {
                let _ = source.remove_option("update_manager", "enable_repo_debug");
            }
            Ok(())
        })();
        self.cc_enabled.set(false);
        if let Err(e) = res {
            let _ = source.cancel();
            return Err(e);
        }
        Ok(source.save())
    }

    fn request_sudo_access(self: &Rc<Self>) {
        if self.sudo_requested.get() {
            return;
        }
        self.sudo_requested.set(true);
        let srv = server();
        let machine: Rc<Machine> = match srv.lookup_component("machine") {
            Some(m) => m,
            None => return,
        };
        let v = self.clone();
        machine.register_sudo_request(
            Rc::new(move || {
                let v = v.clone();
                Box::pin(async move { v.on_password_received().await })
            }),
            "Sudo password required to update Moonraker's systemd service.",
        );
        if machine.public_ip().is_none() {
            let v = Rc::downgrade(self);
            srv.on_event_async("machine:public_ip_changed", move |args| {
                let v = v.clone();
                async move {
                    let ip = args.first().map(py_str).unwrap_or_default();
                    if ip.is_empty() {
                        return;
                    }
                    if let Some(v) = v.upgrade() {
                        v.remove_announcement().await;
                        v.announce_sudo_request();
                    }
                }
            });
        }
        self.announce_sudo_request();
    }

    fn announce_sudo_request(&self) {
        let srv = server();
        let machine: Rc<Machine> = match srv.lookup_component("machine") {
            Some(m) => m,
            None => return,
        };
        let host_info = srv.get_host_info();
        let host_addr = host_info["address"].as_str().unwrap_or("").to_string();
        let mut address = if !["all", "0.0.0.0", "::"].contains(&host_addr.to_lowercase().as_str()) {
            host_addr
        } else {
            machine.public_ip().unwrap_or_default()
        };
        if address.is_empty() {
            address = format!("{}.local", host_info["hostname"].as_str().unwrap_or(""));
        } else if address.contains(':') {
            address = format!("[{}]", address);
        }
        let scheme = if srv.app().https_enabled() { "https" } else { "http" };
        let url = format!("{}://{}:{}/", scheme, address, host_info["port"]);
        if let Some(an) = srv.lookup_component::<crate::components::announcements::Announcements>("announcements") {
            let entry = an.add_internal_announcement(
                "Sudo Password Required",
                "Moonraker requires sudo access to finish updating. Please click on the attached link and follow the instructions.",
                &url,
                "high",
                "machine",
            );
            *self.announcement_id.borrow_mut() = entry.get("entry_id").map(py_str).unwrap_or_default();
        }
        let msg = format!(
            "!! ATTENTION: Moonraker requires sudo access to complete the update.  Go to the following URL and provide your linux password: {}",
            url
        );
        let _ = srv.send_event("server:gcode_response", vec![json!(msg)]);
    }

    pub async fn remove_announcement(&self) {
        let id = self.announcement_id.borrow().clone();
        if id.is_empty() {
            return;
        }
        if let Some(an) = server().lookup_component::<crate::components::announcements::Announcements>("announcements") {
            let _ = an.remove_announcement(&id).await;
        }
        self.announcement_id.borrow_mut().clear();
    }

    async fn on_password_received(self: Rc<Self>) -> Result<(String, bool), ServerError> {
        let mut name = "Service";
        let res = match self.check_service_file().await {
            Ok(_) => {
                name = "Config";
                self.check_configuration().map(|_| ())
            }
            Err(e) => Err(e),
        };
        if res.is_err() {
            log::error!("{} validation failed", name);
            return Err(ServerError::new(format!("{} validation failed", name), 500));
        }
        self.remove_announcement().await;
        let _ = server().database().insert_item_sync("moonraker", &json!("validate_install.install_version"), json!(INSTALL_VERSION));
        self.validation_enabled.set(false);
        Ok(("System update complete.".into(), true))
    }
}

enum ValidationResult {
    Validation(String),
    Other(String),
}

fn check_path_bare(path: &Path) -> bool {
    if !path.exists() {
        return true;
    }
    if let Ok(rd) = std::fs::read_dir(path) {
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if p.is_file() || p.is_symlink() || !["gcodes", "config", "logs", "certs"].contains(&name.as_str()) {
                return false;
            }
            if p.is_dir() && std::fs::read_dir(&p).map(|mut r| r.next().is_some()).unwrap_or(false) {
                return false;
            }
        }
    }
    true
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<Machine>, crate::confighelper::ConfigError> {
    Machine::new(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sysv_status_pidfile_detection() {
        let klipper = "PIDFILE=\"/var/run/klipper.pid\"\ncase \"$1\" in\n    status)\n        if [ -f $PIDFILE ] && kill -0 $(cat $PIDFILE) 2>/dev/null; then\n            echo running\n        else\n            exit 1\n        fi\n        ;;\nesac\n";
        assert_eq!(sysv_status_pidfile(klipper), Some(PathBuf::from("/var/run/klipper.pid")));
        let gui = "PIDFILE=/var/run/gui.pid\n    status)\n        if [ -f \"/var/run/gui.pid\" ] && kill -0 \"$(cat \"/var/run/gui.pid\")\" 2>/dev/null; then\n        fi\n        ;;\n";
        assert_eq!(sysv_status_pidfile(gui), Some(PathBuf::from("/var/run/gui.pid")));
        let gpio = "PIDFILE=/var/run/x.pid\n    status)\n        gpio_value=$(cat /sys/class/gpio/gpio1/value)\n        ;;\n";
        assert_eq!(sysv_status_pidfile(gpio), None);
        assert_eq!(sysv_status_pidfile("PIDFILE=$RUN/x.pid\nstatus)\nkill -0 $(cat $PIDFILE)\n;;"), None);
    }
}

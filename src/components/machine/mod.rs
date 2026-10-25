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

mod libcamera;
mod providers;
mod sysinfo;
mod validation;

use providers::*;
pub(crate) use sysinfo::parse_os_release;
use sysinfo::*;
use validation::*;

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
    // the system's libcamera python bindings import (Machine.libcam)
    libcam: Cell<bool>,
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
        self.provider.initialize().await?;
        if !self.inside_container.get() {
            let virt = self.provider.check_virt_status().await;
            self.system_info.borrow_mut().insert("virtualization".into(), virt);
        }
        self.parse_network_interfaces(0, false).await;
        if let Some(ps) = server().get::<crate::components::proc_stats::ProcStats>() {
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
            // COSMOS only: upstream Moonraker rejects it (Invalid Provider)
            "sysvinit" if crate::utils::is_cosmos() => ProviderKind::Sysvinit,
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
            watchers: Default::default(),
        });
        log::info!("Using System Provider: {}", provider_type);
        let validator = InstallValidator::new(config)?;
        let shell = srv.get::<ShellCommandFactory>().unwrap();
        let addr_cmd = shell.build_shell_command("ip -json -det address", None, None, None, false, None).map_err(|e| ConfigHelper::error(e.message))?;
        let iwgetbin = if Path::new("/sbin/iwgetid").exists() { "/sbin/iwgetid" } else { "iwgetid" };
        let iwgetid_cmd = shell.build_shell_command(iwgetbin, None, None, None, false, None).map_err(|e| ConfigHelper::error(e.message))?;
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
            libcam: Cell::new(false),
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
        // _try_import_libcamera: only where the bindings are installed
        if libcamera::bindings_present() {
            match libcamera::probe(false) {
                Ok(v) => {
                    srv.add_log_rollover_item(
                        "libcamera",
                        &format!("Found libcamera Python module, version: {}", py_str(&v["version"])),
                        true,
                    );
                    m.libcam.set(true);
                }
                Err(e) => {
                    if srv.is_verbose_enabled() {
                        log::error!("Failed to import libcamera
{}", e);
                    }
                }
            }
        }
        if !m.libcam.get() {
            srv.add_log_rollover_item("libcamera", "Module libcamera unavailable, import failed", true);
        }
        Ok(m)
    }

    fn init_allowed_services(&self) {
        let srv = server();
        let fpath = PathBuf::from(srv.data_path()).join("moonraker.asvc");
        if let Some(fm) = srv.get::<crate::components::file_manager::FileManager>() {
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

    pub async fn validate_installation(self: &Rc<Self>) -> bool {
        self.validator.perform_validation().await
    }

    pub async fn extract_service_info(&self, name: &str, pid: i64) -> Value {
        self.provider.extract_service_info(name, pid, None, false).await
    }

    // used by the COSMOS patches (swu_deploy)
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
                    server().send_event(
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
        Ok(json!({"serial_devices": self.detect_serial_devices().await}))
    }

    async fn handle_usb_request(self: Rc<Self>, _req: WebRequest) -> ApiResult {
        Ok(json!({"usb_devices": self.detect_usb_devices().await}))
    }

    pub async fn detect_serial_devices(&self) -> Value {
        let _g = self.periph_lock.lock().await;
        json!(tokio::task::spawn_blocking(crate::sysfs_devs::find_serial_devices).await.unwrap_or_default())
    }

    pub async fn detect_usb_devices(&self) -> Value {
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
        json!(devs)
    }

    async fn handle_can_query(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let iface = req.get_str_or("interface", "can0");
        Ok(json!({"can_uuids": self.query_can_uuids(&iface).await?}))
    }

    pub async fn query_can_uuids(&self, iface: &str) -> Result<Value, ServerError> {
        let _g = self.periph_lock.lock().await;
        Ok(json!(crate::sysfs_devs::query_can_uuids(iface).await?))
    }

    async fn handle_video_request(self: Rc<Self>, _req: WebRequest) -> ApiResult {
        Ok(self.detect_video_devices().await)
    }

    pub async fn detect_video_devices(&self) -> Value {
        let _g = self.periph_lock.lock().await;
        let devs = tokio::task::spawn_blocking(crate::sysfs_devs::find_video_devices).await.unwrap_or_default();
        let libcam = self.libcam.get();
        let libcam_devs = tokio::task::spawn_blocking(move || Self::libcamera_devices(libcam)).await.unwrap_or_default();
        json!({"v4l2_devices": devs, "libcamera_devices": libcam_devs})
    }

    /// get_libcamera_devices (blocking): the cameras the bindings list, none
    /// without them
    pub fn libcamera_devices(libcam: bool) -> Value {
        if !libcam {
            return json!([]);
        }
        match libcamera::probe(true) {
            Ok(v) => v.get("devices").cloned().unwrap_or(json!([])),
            Err(e) => {
                log::error!("Failed to list libcamera devices
{}", e);
                json!([])
            }
        }
    }

    pub fn has_libcamera(&self) -> bool {
        self.libcam.get()
    }

    /// Machine.wait_for_init: component_init finished, or the timeout passed
    pub async fn wait_for_init(&self, timeout: Option<f64>) {
        let n = self.init_notify.notified();
        if self.init_done.get() {
            return;
        }
        match timeout {
            Some(t) => {
                let _ = tokio::time::timeout(std::time::Duration::from_secs_f64(t.max(0.0)), n).await;
            }
            None => n.await,
        }
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
        server().send_event(
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
        let shell = server().get::<ShellCommandFactory>().ok_or_else(|| ServerError::new("shell", 500))?;
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
        if !sequence.is_multiple_of(NETWORK_UPDATE_SEQUENCE) {
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
                server().emit::<crate::events::NetStateChanged>(network_v.clone());
            }
        }
        let mut si = self.system_info.borrow_mut();
        si.insert("network".into(), network_v);
        si.insert("canbus".into(), Value::Object(canbus));
    }

    async fn ip_network(&self) -> Option<(Map<String, Value>, Map<String, Value>)> {
        let shell = server().get::<ShellCommandFactory>()?;
        let resp = match self.addr_cmd.run_with_response(&shell, crate::components::shell_command::RunOpts { log_complete: false, ..Default::default() }).await {
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
            server().emit::<crate::events::PublicIpChanged>(src_ip.clone());
        }
        if src_ip.is_empty() {
            log::info!("Failed to detect local IP address");
        }
        src_ip
    }

    async fn get_wifi_interfaces(&self) -> Map<String, Value> {
        let shell = match server().get::<ShellCommandFactory>() {
            Some(s) => s,
            None => return Map::new(),
        };
        let resp = match self.iwgetid_cmd.run_with_response(&shell, crate::components::shell_command::RunOpts { log_complete: false, ..Default::default() }).await {
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
        let client = match srv.get::<crate::components::http_client::HttpClient>() {
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

/// The interfaces `ip -json -det address` reports: up, ethernet type, with
/// addresses (getifaddrs order, as ip's).  None when a CAN interface is up.
pub fn load_component(config: &ConfigHelper) -> Result<Rc<Machine>, crate::confighelper::ConfigError> {
    Machine::new(config)
}

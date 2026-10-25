// Service providers: systemd (cli and dbus), supervisord, none

use super::*;

// ***** Providers *****

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProviderKind {
    None,
    SystemdCli,
    SystemdDbus,
    SupervisordCli,
}

pub struct Provider {
    pub(super) kind: ProviderKind,
    pub(super) shutdown_action: String,
    pub(super) available_services: RefCell<IndexMap<String, Value>>,
    pub(super) svc_cmd: RefCell<Option<Rc<ShellCommand>>>,
    pub(super) spv_conf: String,
    pub(super) dbus_ready: Cell<bool>,
    pub(super) login_mgr: Cell<bool>,
    pub(super) watchers: crate::server::TaskSet,
}

impl Provider {
    pub(super) fn shell(&self) -> Rc<ShellCommandFactory> {
        server().get::<crate::components::shell_command::ShellCommandFactory>().expect("shell_command")
    }

    pub(super) fn dbus(&self) -> Option<Rc<DbusManager>> {
        server().get::<crate::components::dbus_manager::DbusManager>()
    }

    pub(super) async fn exec_sudo(&self, cmd: &str) -> Result<String, ServerError> {
        let machine = server().get::<Machine>().ok_or_else(|| ServerError::new("machine", 500))?;
        machine.exec_sudo_command(cmd, 1, 2.0).await
    }

    pub async fn initialize(self: &Rc<Self>) -> Result<(), ServerError> {
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
                    )?);
                    self.update_service_status(0, true).await;
                    self.register_status_cb();
                }
            }
            ProviderKind::SystemdDbus => {
                self.initialize_dbus().await;
            }
            ProviderKind::SupervisordCli => {
                self.detect_active_services_spv().await;
                let keys: Vec<String> = self.available_services.borrow().keys().cloned().collect();
                let cmd = if !self.spv_conf.is_empty() {
                    format!("supervisorctl -c {} status {}", self.spv_conf, keys.join(" "))
                } else {
                    format!("supervisorctl status {}", keys.join(" "))
                };
                *self.svc_cmd.borrow_mut() = Some(self.shell().build_shell_command(&cmd, None, None, None, false, None)?);
                self.update_service_status(0, true).await;
                self.register_status_cb();
            }
        }
        Ok(())
    }

    pub(super) fn register_status_cb(self: &Rc<Self>) {
        if let Some(ps) = server().get::<crate::components::proc_stats::ProcStats>() {
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

    pub(super) async fn initialize_dbus(self: &Rc<Self>) {
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

    pub(super) async fn detect_active_services_dbus(self: &Rc<Self>) {
        let dbus = match self.dbus() {
            Some(d) => d,
            None => return,
        };
        let machine = match server().get::<Machine>() {
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
            self.watchers.push(handle);
        }
    }

    pub(super) fn on_service_update(&self, name: &str, changed: &Map<String, Value>) {
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
            server().send_event("machine:service_state_changed", vec![Value::Object(m)]);
        }
    }

    pub(super) async fn detect_active_services_cli(&self) {
        let machine = match server().get::<Machine>() {
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

    pub(super) fn spv_active_state(sub: &str) -> &'static str {
        match sub {
            "stopping" => "deactivating",
            "running" => "active",
            _ => "inactive",
        }
    }

    pub(super) async fn exec_supervisorctl(&self, args: &str, tries: usize, timeout: f64, success_codes: Option<&[i32]>) -> Result<String, ServerError> {
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

    pub(super) async fn get_process_info(&self, names: Option<Vec<String>>) -> IndexMap<String, Value> {
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

    pub(super) async fn detect_active_services_spv(&self) {
        let machine = match server().get::<Machine>() {
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

    pub(super) async fn update_service_status(&self, sequence: u64, notify: bool) {
        if !sequence.is_multiple_of(2) {
            return;
        }
        let cmd = match self.svc_cmd.borrow().clone() {
            Some(c) => c,
            None => return,
        };
        let svcs: Vec<String> = self.available_services.borrow().keys().cloned().collect();
        let shell = self.shell();
        if self.kind == ProviderKind::SupervisordCli {
            let resp = match cmd.run_with_response(&shell, crate::components::shell_command::RunOpts { timeout: 6.0, log_complete: false, success_codes: Some(&[0, 3]), ..Default::default() }).await {
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
            let resp = match cmd.run_with_response(&shell, crate::components::shell_command::RunOpts { log_complete: false, ..Default::default() }).await {
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

    pub(super) fn apply_state(&self, svc: &str, new_state: Value, notify: bool) {
        let changed = self.available_services.borrow().get(svc) != Some(&new_state);
        if changed {
            self.available_services.borrow_mut().insert(svc.to_string(), new_state.clone());
            if notify {
                let mut m = Map::new();
                m.insert(svc.to_string(), new_state);
                server().send_event("machine:service_state_changed", vec![Value::Object(m)]);
            }
        }
    }

    pub async fn shutdown(&self) -> Result<(), ServerError> {
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
            ProviderKind::None | ProviderKind::SupervisordCli => json!({"virt_type": "unknown", "virt_identifier": "unknown"}),
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
        }
    }

    pub(super) async fn extract_spv_info(&self, service_name: &str, pid: i64) -> Value {
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

    pub(super) fn process_cli_props(raw: &Map<String, Value>) -> Value {
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

    pub(super) fn process_dbus_props(raw: &Map<String, Value>) -> Value {
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

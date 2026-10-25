// Provides System Package Updates (port of update_manager/system_deploy.py)

use super::pk_enums as pk;
use super::*;
use crate::components::dbus_manager::DbusManager;
use crate::components::shell_command::ExecOpts;
use futures::StreamExt;

const APT_CMD: &str = "sudo DEBIAN_FRONTEND=noninteractive apt-get";
const PK_BUS: &str = "org.freedesktop.PackageKit";

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProviderKind {
    None,
    Apt,
    PackageKit,
}

pub struct PackageDeploy {
    base: BaseDeploy,
    use_packagekit: bool,
    available_packages: RefCell<Vec<String>>,
    provider: Cell<ProviderKind>,
}

impl PackageDeploy {
    pub fn new(config: &ConfigHelper, cmd_helper: &Rc<CommandHelper>) -> Result<Rc<Self>, ConfigError> {
        let base = BaseDeploy::new(config, cmd_helper, Some("system"), "", Some(""))?;
        let use_packagekit = config.getboolean_or("enable_packagekit", true)?;
        Ok(Rc::new(PackageDeploy {
            base,
            use_packagekit,
            available_packages: RefCell::new(vec![]),
            provider: Cell::new(ProviderKind::None),
        }))
    }

    fn dbus() -> Option<Rc<DbusManager>> {
        server().get::<crate::components::dbus_manager::DbusManager>()
    }

    async fn init_packagekit(&self) -> Result<(), ServerError> {
        let dbus = Self::dbus().ok_or_else(|| ServerError::bad("DBus Connection Not available"))?;
        if !dbus.is_connected() {
            return Err(ServerError::bad("DBus Connection Not available"));
        }
        dbus.check_permission(
            "org.freedesktop.packagekit.system-sources-refresh",
            "The Update Manager will fail to fetch package updates",
        )
        .await;
        dbus.check_permission("org.freedesktop.packagekit.package-install", "The Update Manager will fail to install packages")
            .await;
        dbus.check_permission("org.freedesktop.packagekit.system-update", "The Update Manager will fail to update packages")
            .await;
        dbus.has_interface(PK_BUS, "/org/freedesktop/PackageKit", "org.freedesktop.PackageKit").await
    }

    async fn get_fallback_provider(&self) -> bool {
        let shell = self.base.cmd_helper.shell();
        match shell.exec_cmd("sh -c 'command -v apt'", ExecOpts { log_complete: true, ..Default::default() }).await {
            Err(_) => false,
            Ok(ret) => {
                self.base.log_debug(&format!("APT package manager detected: {}", ret));
                true
            }
        }
    }

    async fn refresh_packages(&self, notify: bool) -> Result<(), ServerError> {
        match self.provider.get() {
            ProviderKind::None => Err(ServerError::bad("Cannot refresh packages, no provider set")),
            ProviderKind::Apt => self.base.cmd_helper.run_cmd_simple(&format!("{} update", APT_CMD), 600.0, notify, 1).await,
            ProviderKind::PackageKit => {
                self.run_transaction("RefreshCache", TxArgs::Bool(false), notify).await.map(|_| ())
            }
        }
    }

    async fn get_packages(&self) -> Result<Vec<String>, ServerError> {
        match self.provider.get() {
            ProviderKind::None => Err(ServerError::bad("Cannot retrieve packages, no provider set")),
            ProviderKind::Apt => {
                let res = self
                    .base
                    .cmd_helper
                    .shell()
                    .exec_cmd("apt list --upgradable", ExecOpts { timeout: 60.0, ..Default::default() })
                    .await?;
                let pkgs: Vec<&str> = res.split('\n').map(|p| p.trim()).filter(|p| !p.is_empty()).collect();
                if pkgs.is_empty() {
                    return Ok(vec![]);
                }
                Ok(pkgs.iter().skip(2).map(|p| p.split('/').next().unwrap_or("").to_string()).collect())
            }
            ProviderKind::PackageKit => {
                let filter = pk::pk_value_of(pk::FILTER, "NONE");
                let pkgs = self.run_transaction("GetUpdates", TxArgs::Filter(filter), false).await?;
                Ok(pk_ids(&pkgs).iter().map(|id| id.split(';').next().unwrap_or("").to_string()).collect())
            }
        }
    }

    pub async fn install_packages(&self, packages: &[String], timeout: f64, retries: usize, notify: bool) -> Result<(), ServerError> {
        match self.provider.get() {
            ProviderKind::None => Err(ServerError::bad("Cannot install packages, no provider set")),
            ProviderKind::Apt => {
                self.refresh_packages(notify).await?;
                let resolved = self.resolve_apt_packages(packages).await?;
                if resolved.is_empty() {
                    self.base.cmd_helper.notify_update_response("No packages detected", false);
                    return Ok(());
                }
                log::debug!("Resolved packages: {}", str_list(&resolved));
                self.base
                    .cmd_helper
                    .run_cmd_simple(&format!("{} install --yes {}", APT_CMD, resolved.join(" ")), timeout, notify, retries)
                    .await
            }
            ProviderKind::PackageKit => {
                self.refresh_packages(notify).await?;
                let flags = ["NEWEST", "NOT_INSTALLED", "BASENAME", "ARCH"].iter().map(|f| pk::pk_value_of(pk::FILTER, f)).sum();
                let pkgs = self.run_transaction("Resolve", TxArgs::FilterList(flags, packages.to_vec()), false).await?;
                let ids = pk_ids(&pkgs);
                if !ids.is_empty() {
                    log::debug!("Installing Packages: {}", str_list(&ids));
                    let tflag = pk::pk_value_of(pk::TRANSACTIONFLAG, "ONLY_TRUSTED");
                    self.run_transaction("InstallPackages", TxArgs::FilterList(tflag, ids), notify).await?;
                }
                Ok(())
            }
        }
    }

    async fn resolve_apt_packages(&self, packages: &[String]) -> Result<Vec<String>, ServerError> {
        self.base.cmd_helper.notify_update_response("Resolving packages...", false);
        let search = packages.iter().map(|p| format!("^{}$", p)).collect::<Vec<_>>().join("|");
        let ret = self
            .base
            .cmd_helper
            .shell()
            .exec_cmd(&format!("apt-cache search --names-only \"{}\"", search), ExecOpts { timeout: 600.0, ..Default::default() })
            .await?;
        let resolved: Vec<String> =
            ret.split('\n').filter(|l| !l.trim().is_empty()).map(|l| l.split_whitespace().next().unwrap_or("").to_string()).collect();
        Ok(packages.iter().filter(|p| resolved.contains(p)).cloned().collect())
    }

    async fn upgrade_system(&self) -> Result<(), ServerError> {
        match self.provider.get() {
            ProviderKind::None => Err(ServerError::bad("Cannot upgrade packages, no provider set")),
            ProviderKind::Apt => self.base.cmd_helper.run_cmd_simple(&format!("{} upgrade --yes", APT_CMD), 3600.0, true, 1).await,
            ProviderKind::PackageKit => {
                let filter = pk::pk_value_of(pk::FILTER, "NONE");
                let pkgs = self.run_transaction("GetUpdates", TxArgs::Filter(filter), false).await?;
                let ids = pk_ids(&pkgs);
                if !ids.is_empty() {
                    log::debug!("Upgrading Packages: {}", str_list(&ids));
                    let tflag = pk::pk_value_of(pk::TRANSACTIONFLAG, "ONLY_TRUSTED");
                    self.run_transaction("UpdatePackages", TxArgs::FilterList(tflag, ids), true).await?;
                }
                Ok(())
            }
        }
    }

    async fn update_package_cache(&self, force: bool, notify: bool) -> Result<(), ServerError> {
        if force || crate::utils::time_now() > self.base.last_refresh_time.get() + 3600.0 {
            self.refresh_packages(notify).await?;
        }
        Ok(())
    }

    async fn run_transaction(&self, method: &str, args: TxArgs, notify: bool) -> Result<Vec<Value>, ServerError> {
        let dbus = Self::dbus().ok_or_else(|| ServerError::bad("PackageKit Interface Not Available"))?;
        let conn = dbus.connection().ok_or_else(|| ServerError::bad("PackageKit Interface Not Available"))?;
        let tx = PkTransaction::new(self.base.cmd_helper.clone(), notify);
        tx.run(&conn, method, args).await
    }
}

fn pk_ids(pkgs: &[Value]) -> Vec<String> {
    pkgs.iter().filter_map(|p| p.get("package_id").and_then(|v| v.as_str()).map(String::from)).collect()
}

enum TxArgs {
    Bool(bool),
    Filter(u64),
    FilterList(u64, Vec<String>),
}

/// A single PackageKit transaction (PackageKitTransaction)
struct PkTransaction {
    cmd_helper: Rc<CommandHelper>,
    notify: Cell<bool>,
    status: Cell<u32>,
    role: Cell<u32>,
    percentage: Cell<u32>,
    dl_remaining: Cell<u64>,
    speed: Cell<u64>,
    result: RefCell<Vec<Value>>,
    err_msg: RefCell<String>,
}

fn pk_err(e: impl std::fmt::Display) -> ServerError {
    ServerError::new(e.to_string(), 500)
}

impl PkTransaction {
    fn new(cmd_helper: Rc<CommandHelper>, notify: bool) -> Self {
        PkTransaction {
            cmd_helper,
            notify: Cell::new(notify),
            status: Cell::new(0),
            role: Cell::new(0),
            percentage: Cell::new(101),
            dl_remaining: Cell::new(0),
            speed: Cell::new(0),
            result: RefCell::new(vec![]),
            err_msg: RefCell::new(String::new()),
        }
    }

    fn role_in(&self, names: &[&str]) -> bool {
        let r = pk::pk_name(pk::ROLE, self.role.get());
        names.iter().any(|n| *n == r)
    }

    async fn run(&self, conn: &zbus::Connection, method: &str, args: TxArgs) -> Result<Vec<Value>, ServerError> {
        let tid: zvariant::OwnedObjectPath = conn
            .call_method(Some(PK_BUS), "/org/freedesktop/PackageKit", Some(PK_BUS), "CreateTransaction", &())
            .await
            .map_err(pk_err)?
            .body()
            .deserialize()
            .map_err(pk_err)?;
        let tproxy = zbus::Proxy::new(conn, PK_BUS, tid.clone(), "org.freedesktop.PackageKit.Transaction").await.map_err(pk_err)?;
        let pproxy = zbus::Proxy::new(conn, PK_BUS, tid, "org.freedesktop.DBus.Properties").await.map_err(pk_err)?;
        let mut tsignals = tproxy.receive_all_signals().await.map_err(pk_err)?;
        let mut psignals = pproxy.receive_signal("PropertiesChanged").await.map_err(pk_err)?;
        log::debug!("PackageKit: Running transaction call_{}", method);
        let call = match args {
            TxArgs::Bool(b) => tproxy.call_method(method, &(b,)).await,
            TxArgs::Filter(f) => tproxy.call_method(method, &(f,)).await,
            TxArgs::FilterList(f, l) => tproxy.call_method(method, &(f, l)).await,
        };
        call.map_err(pk_err)?;
        loop {
            tokio::select! {
                msg = tsignals.next() => {
                    let msg = match msg {
                        None => return Err(ServerError::bad("PackageKit transaction ended unexpectedly")),
                        Some(m) => m,
                    };
                    let member = msg.header().member().map(|m| m.to_string()).unwrap_or_default();
                    let body = msg.body();
                    match member.as_str() {
                        "Package" => {
                            if let Ok((info_code, package_id, summary)) = body.deserialize::<(u32, String, String)>() {
                                self.on_package(info_code, &package_id, &summary);
                            }
                        }
                        "RepoDetail" => {
                            if let Ok((repo_id, desc, enabled)) = body.deserialize::<(String, String, bool)>() {
                                self.on_repo_detail(&repo_id, &desc, enabled);
                            }
                        }
                        "ErrorCode" => {
                            if let Ok((code, details)) = body.deserialize::<(u32, String)>() {
                                *self.err_msg.borrow_mut() = format!("{}: {}", pk::pk_name(pk::ERROR, code), details);
                            }
                        }
                        "Finished" => {
                            let (exit_code, run_time) = body.deserialize::<(u32, u32)>().unwrap_or((0, 0));
                            return self.on_finished(exit_code, run_time);
                        }
                        _ => {}
                    }
                }
                msg = psignals.next() => {
                    if let Some(m) = msg {
                        if let Ok((_iface, changed, _inv)) =
                            m.body().deserialize::<(String, HashMap<String, zvariant::OwnedValue>, Vec<String>)>()
                        {
                            for (name, val) in changed {
                                self.on_property(&name, &val);
                            }
                        }
                    }
                }
            }
        }
    }

    fn on_package(&self, info_code: u32, package_id: &str, summary: &str) {
        let mut info = info_code & 0xFFFF;
        let severity = (info_code >> 16) & 0xFFFF;
        if pk::pk_name(pk::INFO, info) == "UNKNOWN" {
            info = severity;
        }
        if self.role_in(&["RESOLVE", "GET_PACKAGES", "GET_UPDATES"]) {
            self.result.borrow_mut().push(json!({
                "package_id": package_id,
                "info": pk_desc(pk::INFO, info),
                "summary": summary,
            }));
        } else if self.notify.get() && pk::pk_name(pk::INFO, info) != "FINISHED" {
            let parts: Vec<&str> = package_id.split(';').collect();
            self.cmd_helper.notify_update_response(
                &format!("{}: {} ({})", pk_desc(pk::INFO, info), parts[0], parts.get(1).unwrap_or(&"")),
                false,
            );
        }
    }

    fn on_repo_detail(&self, repo_id: &str, description: &str, enabled: bool) {
        if pk::pk_name(pk::ROLE, self.role.get()) == "GET_REPO_LIST" {
            self.result.borrow_mut().push(json!({"repo_id": repo_id, "description": description, "enabled": enabled}));
        } else if self.notify.get() {
            let id = if repo_id.trim().is_empty() { description } else { repo_id };
            self.cmd_helper.notify_update_response(&format!("GET: {}", id), false);
        }
    }

    fn on_finished(&self, exit_code: u32, run_time: u32) -> Result<Vec<Value>, ServerError> {
        let secs = run_time as f64 / 1000.0;
        let ext = pk::pk_name(pk::EXIT, exit_code);
        let msg = format!(
            "Transaction {}: Exit {}, Run time: {:.2} seconds",
            pk_desc(pk::ROLE, self.role.get()),
            pk_desc(pk::EXIT, exit_code),
            secs
        );
        if self.notify.get() {
            self.cmd_helper.notify_update_response(&msg, false);
        }
        log::debug!("{}", msg);
        if ext == "SUCCESS" {
            Ok(self.result.borrow().clone())
        } else {
            let err = self.err_msg.borrow().clone();
            Err(ServerError::bad(if err.is_empty() { pk_desc(pk::EXIT, exit_code) } else { err }))
        }
    }

    fn on_property(&self, name: &str, val: &zvariant::OwnedValue) {
        let num = || -> u64 {
            match &**val {
                zvariant::Value::U32(v) => *v as u64,
                zvariant::Value::U64(v) => *v,
                zvariant::Value::I32(v) => *v as u64,
                _ => 0,
            }
        };
        match name {
            "Role" => {
                self.role.set(num() as u32);
                if self.role_in(&["RESOLVE", "GET_PACKAGES", "GET_UPDATES", "GET_REPO_LIST"]) {
                    self.notify.set(false);
                }
                if self.notify.get() {
                    self.cmd_helper
                        .notify_update_response(&format!("Transaction {} started...", pk_desc(pk::ROLE, self.role.get())), false);
                }
                log::debug!("PackageKit: Current Role: {}", pk_desc(pk::ROLE, self.role.get()));
            }
            "Status" => {
                self.status.set(num() as u32);
                self.percentage.set(101);
                self.speed.set(0);
                log::debug!("PackageKit: Current Status: {}", pk_desc(pk::STATUS, self.status.get()));
            }
            "Percentage" => {
                self.percentage.set(num() as u32);
                let s = pk::pk_name(pk::STATUS, self.status.get());
                if ["RUNNING", "INSTALL", "UPDATE"].contains(&s.as_str()) {
                    self.notify_progress();
                }
            }
            "DownloadSizeRemaining" => {
                self.dl_remaining.set(num());
                self.notify_progress();
            }
            "Speed" => self.speed.set(num()),
            _ => {}
        }
    }

    fn notify_progress(&self) {
        if !self.notify.get() || self.percentage.get() > 100 {
            return;
        }
        let mut msg = format!("{}...{}%", pk_desc(pk::STATUS, self.status.get()), self.percentage.get());
        let rem = self.dl_remaining.get();
        if pk::pk_name(pk::STATUS, self.status.get()) == "DOWNLOAD" && rem > 0 {
            if rem < 1024 {
                msg.push_str(&format!(", Remaining: {} B", rem));
            } else if rem < 1048576 {
                msg.push_str(&format!(", Remaining: {} KiB", rem / 1024));
            } else {
                msg.push_str(&format!(", Remaining: {} MiB", rem / 1048576));
            }
            if self.speed.get() > 0 {
                let speed = self.speed.get() / 8;
                if speed < 1024 {
                    msg.push_str(&format!(", Speed: {} B/s", speed));
                } else if speed < 1048576 {
                    msg.push_str(&format!(", Speed: {} KiB/s", speed / 1024));
                } else {
                    msg.push_str(&format!(", Speed: {} MiB/s", speed / 1048576));
                }
            }
        }
        self.cmd_helper.notify_update_response(&msg, false);
    }
}

fn pk_desc(names: &[&str], idx: u32) -> String {
    super::common::pk_desc(names, idx)
}

#[async_trait(?Send)]
impl Deployer for PackageDeploy {
    fn base(&self) -> &BaseDeploy {
        &self.base
    }
    fn kind(&self) -> DeployKind {
        DeployKind::System
    }

    async fn initialize(&self) -> Map<String, Value> {
        let storage = self.base.initialize();
        *self.available_packages.borrow_mut() = storage
            .get("packages")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().map(crate::pyfmt::py_str).collect())
            .unwrap_or_default();
        let mut try_fallback = true;
        if self.use_packagekit && self.init_packagekit().await.is_ok() {
            self.base.log_info("PackageDeploy: PackageKit Provider Configured");
            *self.base.prefix.borrow_mut() = "PackageKit: ".into();
            self.provider.set(ProviderKind::PackageKit);
            try_fallback = false;
        }
        if try_fallback {
            if self.get_fallback_provider().await {
                self.base.log_info("PackageDeploy: Using APT CLI Provider");
                *self.base.prefix.borrow_mut() = "Package Manager APT: ".into();
                self.provider.set(ProviderKind::Apt);
            } else {
                self.provider.set(ProviderKind::None);
                let machine = server().get::<Machine>();
                let dist_id = machine
                    .map(|m| m.get_system_info())
                    .and_then(|i| i.get("distribution").and_then(|d| d.get("id")).map(crate::pyfmt::py_str))
                    .unwrap_or_default()
                    .to_lowercase();
                server().add_warning(&format!("Unable to initialize System Update Provider for distribution: {}", dist_id), None, true);
            }
        }
        storage
    }

    async fn refresh(&self) -> Result<(), ServerError> {
        let res: Result<(), ServerError> = async {
            if server().is_running() {
                self.update_package_cache(true, false).await?;
            }
            let pkgs = self.get_packages().await?;
            self.base.log_info(&format!("Detected {} package updates:\n{}", pkgs.len(), pkgs.join("\n")));
            *self.available_packages.borrow_mut() = pkgs;
            Ok(())
        }
        .await;
        if let Err(e) = res {
            self.base.log_exc(&format!("Error Refreshing System Packages\n{}", e.message), true);
        }
        save_state(self);
        Ok(())
    }

    fn get_persistent_data(&self) -> Map<String, Value> {
        let mut storage = self.base.persistent_data();
        storage.insert("packages".into(), json!(*self.available_packages.borrow()));
        storage
    }

    async fn update(&self) -> Result<bool, ServerError> {
        if self.available_packages.borrow().is_empty() {
            return Ok(false);
        }
        self.base.cmd_helper.notify_update_response("Updating packages...", false);
        let res = async {
            self.update_package_cache(true, true).await?;
            self.upgrade_system().await
        }
        .await;
        if res.is_err() {
            return Err(ServerError::bad("Error updating system packages"));
        }
        self.available_packages.borrow_mut().clear();
        save_state(self);
        self.base.cmd_helper.notify_update_response("Package update finished...", true);
        Ok(true)
    }

    fn get_update_status(&self) -> Value {
        let pkgs = self.available_packages.borrow();
        json!({
            "name": self.base.name,
            "configured_type": "system",
            "package_count": pkgs.len(),
            "package_list": *pkgs,
        })
    }
}

// Install validation (machine.py InstallValidator)

use super::*;

// ***** Install validation *****

pub(super) const INSTALL_VERSION: i64 = 1;
pub(super) const SERVICE_VERSION: i64 = 1;

pub struct InstallValidator {
    pub(super) config: ConfigHelper,
    pub(super) force_validation: bool,
    pub(super) sc_enabled: Cell<bool>,
    pub(super) cc_enabled: Cell<bool>,
    pub(super) data_path: RefCell<PathBuf>,
    pub(super) backup_path: RefCell<Option<PathBuf>>,
    pub(super) data_path_valid: Cell<bool>,
    pub(super) sudo_requested: Cell<bool>,
    pub(super) announcement_id: RefCell<String>,
    pub validation_enabled: Cell<bool>,
}

impl InstallValidator {
    pub(super) fn new(config: &ConfigHelper) -> Result<Rc<Self>, crate::confighelper::ConfigError> {
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

    pub(super) fn update_backup_path(&self) {
        let str_time = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let mut bp = self.backup_path.borrow_mut();
        if bp.as_ref().map(|p| !p.exists()).unwrap_or(true) {
            *bp = Some(self.data_path.borrow().join(format!("backup/{}", str_time)));
        }
    }

    pub(super) async fn validation_init(&self) {
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

    pub(super) async fn perform_validation(self: &Rc<Self>) -> bool {
        if !self.validation_enabled.get() {
            return false;
        }
        let srv = server();
        let fm = srv.get::<crate::components::file_manager::FileManager>();
        let mut name = "service";
        let res = match self.check_service_file().await {
            Ok(r1) => {
                name = "config";
                self.check_configuration().map(|r2| r1 | r2)
            }
            Err(e) => Err(e),
        };
        let need_restart;
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
            if let Some(m) = srv.get::<Machine>() {
                m.restart_moonraker_service();
            }
            return true;
        }
        false
    }

    pub(super) async fn check_service_file(self: &Rc<Self>) -> Result<bool, ValidationResult> {
        if !self.sc_enabled.get() {
            return Ok(false);
        }
        let srv = server();
        let machine = srv.get::<Machine>().ok_or_else(|| ValidationResult::Other("machine".into()))?;
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
        let fm = srv.get::<crate::components::file_manager::FileManager>();
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

    pub(super) fn link_data_subfolder(&self, folder: &str, source: &str, exist_ok: bool) -> Result<(), ValidationResult> {
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

    pub(super) fn link_data_file(&self, data_file: &Path, target: &str) -> Result<(), ValidationResult> {
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

    pub(super) fn check_configuration(&self) -> Result<bool, ValidationResult> {
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

    pub(super) fn request_sudo_access(self: &Rc<Self>) {
        if self.sudo_requested.get() {
            return;
        }
        self.sudo_requested.set(true);
        let srv = server();
        let machine = match srv.get::<Machine>() {
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

    pub(super) fn announce_sudo_request(&self) {
        let srv = server();
        let machine = match srv.get::<Machine>() {
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
        if let Some(an) = srv.get::<crate::components::announcements::Announcements>() {
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
        if let Some(an) = server().get::<crate::components::announcements::Announcements>() {
            let _ = an.remove_announcement(&id).await;
        }
        self.announcement_id.borrow_mut().clear();
    }

    pub(super) async fn on_password_received(self: Rc<Self>) -> Result<(String, bool), ServerError> {
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

pub(super) enum ValidationResult {
    Validation(String),
    Other(String),
}

pub(super) fn check_path_bare(path: &Path) -> bool {
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

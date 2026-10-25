// AppDeploy (app_deploy.py)

use super::*;

pub struct AppDeploy {
    pub base: BaseDeploy,
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

pub(super) fn resolve(p: &Path) -> PathBuf {
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
            if let Some(fm) = server().get::<crate::components::file_manager::FileManager>() {
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
            let machine = srv.get::<Machine>();
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
        let machine = srv.get::<Machine>().ok_or_else(|| ServerError::new("machine", 500))?;
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

    pub(super) fn get_file_hash(filename: Option<&PathBuf>) -> Option<String> {
        use sha2::Digest;
        let f = filename?;
        if !f.is_file() {
            return None;
        }
        std::fs::read(f).ok().map(|d| hex::encode(sha2::Sha256::digest(&d)))
    }

    pub(super) async fn install_packages(&self, packages: &[String]) {
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

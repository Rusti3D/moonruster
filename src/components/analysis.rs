// Printer GCode Analysis using Klipper Estimator (port of moonraker/components/analysis.py)

use crate::common::*;
use crate::components::file_manager::FileManager;
use crate::components::shell_command::{ExecOpts, ShellCommandFactory};
use crate::confighelper::{ConfigError, ConfigHelper};
use crate::server::{server, spawn, Component};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

const ESTIMATOR_URL: &str = "https://github.com/Annex-Engineering/klipper_estimator/releases/latest/download/";
const IDENT_REGEX: &str = r"^; Processed by klipper_estimator (?P<version>v?\d+(?:\.\d+)*)";

/// shlex.quote
fn sh_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

pub fn check_processed(gc_path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let size = std::fs::metadata(gc_path).ok()?.len();
    let start = size.saturating_sub(64 * 1024);
    let mut f = std::fs::File::open(gc_path).ok()?;
    if start > 0 {
        f.seek(SeekFrom::Start(start)).ok()?;
    }
    let mut data = Vec::new();
    f.read_to_end(&mut data).ok()?;
    let text = String::from_utf8_lossy(&data);
    let re = regex::Regex::new(&format!("(?m){}", IDENT_REGEX)).unwrap();
    re.captures_iter(&text).last().map(|c| c["version"].to_string())
}

pub struct GcodeAnalysis {
    cmd_lock: tokio::sync::Mutex<()>,
    fm: Rc<FileManager>,
    estimator_timeout: i64,
    auto_analyze: bool,
    auto_dump_defcfg: bool,
    default_config: PathBuf,
    estimator_config: PathBuf,
    estimator_path: Option<PathBuf>,
    estimator_ready: Cell<bool>,
    estimator_version: RefCell<String>,
    updater_registered: bool,
    proc_config: RefCell<Value>,
}

impl GcodeAnalysis {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, ConfigError> {
        let srv = server();
        let fm: Rc<FileManager> = srv.lookup_component("file_manager").ok_or_else(|| ConfigHelper::error("file_manager"))?;
        let tool_folder = PathBuf::from(srv.data_path()).join("tools/klipper_estimator");
        if !tool_folder.exists() {
            std::fs::create_dir_all(&tool_folder).map_err(|e| ConfigHelper::error(e.to_string()))?;
        }
        let estimator_timeout = config.getint_or("estimator_timeout", 600)?;
        let auto_analyze = config.getboolean_or("enable_auto_analysis", false)?;
        let auto_dump_defcfg = config.getboolean_or("auto_dump_default_config", false)?;
        let default_config = tool_folder.join("default_estimator_cfg.json");
        let mut estimator_config = default_config.clone();
        if let Some(est) = config.get_opt("estimator_config")? {
            let est_path = fm.get_full_path("config", est.trim_matches('/')).map_err(|e| ConfigHelper::error(e.message))?;
            if est_path.components().any(|c| c.as_os_str() == "..") {
                return Err(ConfigHelper::error("Value for option 'estimator_config' must not contain a '..' segment"));
            }
            if !est_path.exists() {
                return Err(ConfigHelper::error(format!("File '{}' does not exist in 'config' root", est)));
            }
            estimator_config = est_path;
        }
        let pltform = config.getchoice("platform", &["rpi", "linux", "osx", "auto"], Some("auto"), true)?;
        let estimator_path = if pltform == "auto" {
            Self::detect_platform().map(|p| tool_folder.join(format!("klipper_estimator_{}", p)))
        } else {
            Some(tool_folder.join(format!("klipper_estimator_{}", pltform)))
        };
        let enable_updates = config.getboolean_or("enable_estimator_updates", false)?;
        let mut updater_registered = false;
        if enable_updates {
            if estimator_path.is_none() {
                log::info!("Klipper estimator platform not detected, updates disabled");
            } else if !config.has_section("update_manager") {
                log::info!("Update Manager not configured, updates disabled");
            } else {
                let res: Result<(), ServerError> = (|| {
                    srv.load_component(config, "update_manager").map_err(|e| ServerError::bad(e.0))?;
                    let um: Rc<crate::components::update_manager::UpdateManager> =
                        srv.lookup_component("update_manager").ok_or_else(|| ServerError::bad("update_manager"))?;
                    um.register_updater(
                        "klipper_estimator",
                        json!({
                            "type": "executable",
                            "channel": "stable",
                            "repo": "Annex-Engineering/klipper_estimator",
                            "is_system_service": "False",
                            "path": tool_folder.display().to_string(),
                        }),
                    )
                    .map_err(ServerError::from)
                })();
                match res {
                    Err(e) => log::error!("Klipper Estimator update registration failed\n{}", e.message),
                    Ok(()) => updater_registered = true,
                }
            }
        }
        if !updater_registered {
            fm.add_reserved_path("analysis", &tool_folder, false);
        }
        let ep = estimator_path.as_ref().map(|p| p.display().to_string()).unwrap_or("None".into());
        let proc_config = json!({
            "name": "klipper_estimator",
            "command": [ep, "--config_file", estimator_config.display().to_string(), "post-process", "{gcode_file_path}"],
            "timeout": estimator_timeout,
            "version": "?",
            "ident": {"regex": IDENT_REGEX, "location": "footer"},
            "enabled": false,
        });
        fm.get_metadata_storage()
            .register_gcode_processor("klipper_estimator", Some(proc_config.clone()))
            .map_err(|e| ConfigHelper::error(e.message))?;
        let ga = Rc::new(GcodeAnalysis {
            cmd_lock: tokio::sync::Mutex::new(()),
            fm,
            estimator_timeout,
            auto_analyze,
            auto_dump_defcfg,
            default_config,
            estimator_config,
            estimator_path,
            estimator_ready: Cell::new(false),
            estimator_version: RefCell::new("?".into()),
            updater_registered,
            proc_config: RefCell::new(proc_config),
        });
        let g = ga.clone();
        let _ = srv.register_endpoint(
            "/server/analysis/status",
            RequestType::GET,
            callback(move |_req| {
                let g = g.clone();
                async move { Ok(g.status()) }
            }),
        );
        for ep in ["estimate", "process"] {
            let g = ga.clone();
            let _ = srv.register_endpoint(
                &format!("/server/analysis/{}", ep),
                RequestType::POST,
                callback(move |req| {
                    let g = g.clone();
                    async move { g.handle_estimator_request(req).await }
                }),
            );
        }
        let g = ga.clone();
        let _ = srv.register_endpoint(
            "/server/analysis/dump_config",
            RequestType::POST,
            callback(move |req| {
                let g = g.clone();
                async move { g.handle_dump_cfg_request(req).await }
            }),
        );
        let g = Rc::downgrade(&ga);
        srv.on_event("server:klippy_ready", move |_| {
            if let Some(g) = g.upgrade() {
                g.on_klippy_ready();
            }
        });
        Ok(ga)
    }

    fn detect_platform() -> Option<&'static str> {
        // SAFETY: a C struct of byte arrays: all zero is a valid value
        let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
        // SAFETY: uts is exclusively borrowed for uname to fill
        let arch = if unsafe { libc::uname(&mut uts) } == 0 {
            // SAFETY: uname succeeded, so machine is NUL-terminated within the array
            unsafe { std::ffi::CStr::from_ptr(uts.machine.as_ptr()) }.to_string_lossy().to_string()
        } else {
            String::new()
        };
        let srv = server();
        if arch.is_empty() {
            srv.add_warning(
                "[analysis]: Failed to detect CPU architecture.  Manual configuration of the 'platform' option is required.",
                Some("analysis_estimator"),
                true,
            );
            return None;
        }
        match arch.as_str() {
            "x86_64" => Some("linux"),
            "armv7l" | "aarch64" => Some("rpi"),
            _ => {
                srv.add_warning(
                    &format!(
                        "[analysis]: Unsupported CPU architecture '{}'.  Manual configuration of the 'platform' option is required.",
                        arch
                    ),
                    Some("analysis_estimator"),
                    true,
                );
                None
            }
        }
    }

    fn version(&self) -> String {
        self.estimator_version.borrow().clone()
    }

    fn on_klippy_ready(self: &Rc<Self>) {
        if !self.estimator_ready.get() {
            return;
        }
        if self.auto_dump_defcfg || !self.default_config.exists() {
            log::info!("Dumping default Klipper Estimator configuration to {}", self.default_config.display());
            let update = self.auto_analyze && self.default_config == self.estimator_config && !self.default_config.exists();
            let g = self.clone();
            spawn(async move {
                let dest = g.default_config.clone();
                if let Err(e) = g.dump_estimator_config(&dest).await {
                    log::error!("{}", e.message);
                }
                if update {
                    g.update_gcode_proc_config();
                }
            });
        }
    }

    fn update_gcode_proc_config(&self) {
        let mut enabled = false;
        if self.auto_analyze {
            if !self.estimator_ready.get() {
                log::info!("Klipper Estimator executable failed validation, auto analysis disabled.");
            } else if !self.estimator_config.is_file() {
                log::info!("Klipper Estimator config file does not exist, auto analysis disabled.");
            } else {
                log::info!("Klipper Estimator Auto Analysis Enabled");
                enabled = true;
            }
        }
        let cfg = {
            let mut pc = self.proc_config.borrow_mut();
            pc["version"] = json!(self.version());
            pc["enabled"] = json!(enabled);
            pc.clone()
        };
        self.fm.get_metadata_storage().update_gcode_processor("klipper_estimator", cfg);
    }

    fn check_estimator_perms(path: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        if !path.is_file() {
            return false;
        }
        let req = 0o100 | 0o010;
        if let Ok(md) = std::fs::metadata(path) {
            let mode = md.permissions().mode() & 0o7777;
            if mode & req != req {
                log::info!("Setting executable permissions for Klipper Estimator...");
                if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | req)) {
                    log::error!("Failed to set Klipper Estimator Permissions\n{}", e);
                }
            }
        }
        crate::utils::access_exec(path)
    }

    fn shell() -> Rc<ShellCommandFactory> {
        server().lookup_component("shell_command").expect("shell_command")
    }

    async fn detect_estimator_version(&self) -> Result<(), ServerError> {
        let cmd = format!("{} --version", self.estimator_path.as_ref().unwrap().display());
        let ret = Self::shell().exec_cmd(&cmd, ExecOpts { timeout: 10.0, ..Default::default() }).await?;
        let re = regex::Regex::new(r"^klipper_estimator (v?\d+(?:\.\d+)*)").unwrap();
        *self.estimator_version.borrow_mut() = re.captures(&ret).map(|c| c[1].to_string()).unwrap_or("?".into());
        Ok(())
    }

    fn get_moonraker_url(&self) -> String {
        let srv = server();
        let hi = srv.get_host_info();
        let host_addr = hi["address"].as_str().unwrap_or("").to_lowercase();
        let mut address = match host_addr.as_str() {
            "all" | "0.0.0.0" | "localhost" | "127.0.0.1" => "127.0.0.1".to_string(),
            "::" => "::1".to_string(),
            _ => srv
                .lookup_component::<crate::components::machine::Machine>("machine")
                .and_then(|m| m.public_ip())
                .unwrap_or_default(),
        };
        if address.is_empty() {
            address = format!("{}.local", hi["hostname"].as_str().unwrap_or(""));
        } else if address.contains(':') {
            address = format!("[{}]", address);
        }
        format!("http://{}:{}{}", address, hi["port"], srv.app().route_prefix())
    }

    pub fn gen_estimator_cmd(&self, gc_path: &Path, est_cfg: &Path, post_process: bool) -> Result<String, ServerError> {
        let exe = match (&self.estimator_path, self.estimator_ready.get()) {
            (Some(p), true) => p,
            _ => return Err(ServerError::bad("Klipper Estimator not available")),
        };
        if !est_cfg.exists() {
            return Err(ServerError::bad(format!(
                "Klipper Estimator config {} does not exist",
                est_cfg.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
            )));
        }
        let action = if post_process { "post-process" } else { "estimate -f json" };
        Ok(format!(
            "{} --config_file {} {} {}",
            exe.display(),
            sh_quote(&est_cfg.display().to_string()),
            action,
            sh_quote(&gc_path.display().to_string())
        ))
    }

    fn gen_dump_cmd(&self) -> Result<String, ServerError> {
        let exe = match (&self.estimator_path, self.estimator_ready.get()) {
            (Some(p), true) => p,
            _ => return Err(ServerError::bad("Klipper Estimator not available")),
        };
        let mut opts = format!("--config_moonraker_url {}", self.get_moonraker_url());
        if let Some(auth) = server().lookup_component::<crate::components::authorization::Authorization>("authorization") {
            if let Some(k) = auth.get_api_key() {
                opts = format!("{} --config_moonraker_api_key {}", opts, k);
            }
        }
        Ok(format!("{} {} dump-config", exe.display(), opts))
    }

    async fn dump_estimator_config(&self, dest: &Path) -> Result<Value, ServerError> {
        let _g = self.cmd_lock.lock().await;
        if !server().klippy().is_ready() {
            return Err(ServerError::new("Klipper Estimator cannot dump configuration, Klippy not ready", 504));
        }
        let cmd = self.gen_dump_cmd()?;
        let ret = Self::shell()
            .exec_cmd(&cmd, ExecOpts { timeout: 10.0, log_complete: false, log_stderr: true, ..Default::default() })
            .await
            .map_err(|_| ServerError::new("Klipper Estimator dump-config failed", 500))?;
        std::fs::write(dest, &ret).map_err(|_| ServerError::new("Klipper Estimator dump-config failed", 500))?;
        serde_json::from_str(&ret).map_err(|e| ServerError::exc(e.to_string()))
    }

    /// estimate_file / post_process_file's checks: the estimator config to
    /// use (the [analysis] one when none is given)
    pub fn check_estimate_args(&self, gc_path: &Path, est_config: Option<&Path>) -> Result<PathBuf, ServerError> {
        let est_config = est_config.unwrap_or(&self.estimator_config);
        if !est_config.is_file() {
            return Err(ServerError::bad(format!("Estimator config file '{}' does not exist", est_config.display())));
        }
        if !gc_path.is_file() {
            return Err(ServerError::bad(format!("GCode File '{}' does not exist", gc_path.display())));
        }
        Ok(est_config.to_path_buf())
    }

    /// The lock estimator commands run under
    pub fn cmd_lock(&self) -> &tokio::sync::Mutex<()> {
        &self.cmd_lock
    }

    pub fn estimator_timeout(&self) -> i64 {
        self.estimator_timeout
    }

    pub fn estimator_version(&self) -> String {
        self.version()
    }

    async fn estimate_file(&self, gc_path: &Path, est_config: &Path) -> ApiResult {
        let _g = self.cmd_lock.lock().await;
        self.check_estimate_args(gc_path, Some(est_config))?;
        let cmd = self.gen_estimator_cmd(gc_path, est_config, false)?;
        let ret = Self::shell().exec_cmd(&cmd, ExecOpts { timeout: self.estimator_timeout as f64, ..Default::default() }).await?;
        let data: Value = serde_json::from_str(&ret).map_err(|e| ServerError::exc(e.to_string()))?;
        data.get("sequences").and_then(|s| s.get(0)).cloned().ok_or_else(|| ServerError::exc("'sequences'"))
    }

    async fn post_process_file(&self, gc_path: &Path, est_config: &Path, force: bool) -> ApiResult {
        let _g = self.cmd_lock.lock().await;
        self.check_estimate_args(gc_path, Some(est_config))?;
        let mut proc_ver = check_processed(gc_path);
        let processed = proc_ver.is_some();
        let mut bypassed = processed;
        if !processed || force {
            let cmd = self.gen_estimator_cmd(gc_path, est_config, true)?;
            Self::shell().exec_cmd(&cmd, ExecOpts { timeout: self.estimator_timeout as f64, ..Default::default() }).await?;
            proc_ver = Some(self.version());
            bypassed = false;
        } else {
            log::info!(
                "File {} already processed, aborting",
                gc_path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
            );
        }
        Ok(json!({"prev_processed": processed, "version": proc_ver, "bypassed": bypassed}))
    }

    fn status(&self) -> Value {
        let est_exec = self
            .estimator_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or("unknown".into());
        json!({
            "estimator_executable": est_exec,
            "estimator_ready": self.estimator_ready.get(),
            "estimator_version": self.version(),
            "estimator_config_exists": self.estimator_config.exists(),
            "using_default_config": self.estimator_config == self.default_config,
        })
    }

    async fn handle_estimator_request(&self, req: WebRequest) -> ApiResult {
        let gcode_file = req.get_str("filename")?.trim_matches('/').to_string();
        let estimator_config = req.get_str_opt("estimator_config");
        let gc_path = self.fm.get_full_path("gcodes", &gcode_file)?;
        if !gc_path.is_file() {
            return Err(ServerError::bad(format!("GCode File '{}' does not exit in gcodes path", gcode_file)));
        }
        let mut est_cfg = self.estimator_config.clone();
        if let Some(ec) = estimator_config {
            est_cfg = self.fm.get_full_path("config", ec.trim_matches('/'))?;
            if est_cfg.components().any(|c| c.as_os_str() == "..") {
                return Err(ServerError::bad("Invalid value for param 'estimator_config', '..' segments are not allowed"));
            }
        }
        match req.get_endpoint().rsplit('/').next().unwrap_or("") {
            "estimate" => self.estimate_file(&gc_path, &est_cfg).await,
            "process" => {
                let force = req.get_boolean_or("force", false)?;
                self.post_process_file(&gc_path, &est_cfg, force).await
            }
            ep => Err(ServerError::new(format!("Unknown request {}", ep), 404)),
        }
    }

    async fn handle_dump_cfg_request(&self, req: WebRequest) -> ApiResult {
        let dest = req.get_str_opt("dest_config");
        let (root, dest_name, dest_path) = match dest {
            Some(d) => {
                let d = d.trim_matches('/').to_string();
                if Path::new(&d).components().any(|c| c.as_os_str() == "..") {
                    return Err(ServerError::bad("Parameter 'dest_config' may not contain '..' parts"));
                }
                let p = self.fm.get_full_path("config", &d)?;
                (Some("config"), d, p)
            }
            None => (
                None,
                self.default_config.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                self.default_config.clone(),
            ),
        };
        let result = self.dump_estimator_config(&dest_path).await?;
        Ok(json!({"dest_root": root, "dest_config_path": dest_name, "klipper_estimator_config": result}))
    }
}

#[async_trait(?Send)]
impl Component for GcodeAnalysis {
    fn has_init(&self) -> bool {
        true
    }

    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        if let Some(path) = self.estimator_path.clone() {
            if !path.exists() {
                let _g = self.cmd_lock.lock().await;
                let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                log::info!("Downloading latest {}...", name);
                let http: Rc<crate::components::http_client::HttpClient> =
                    server().lookup_component("http_client").ok_or_else(|| ServerError::new("http_client", 500))?;
                match http
                    .download_file(&format!("{}{}", ESTIMATOR_URL, name), "application/octet-stream", Some(path.clone()), -1, None, 180.0, 1, 1.0)
                    .await
                {
                    Err(e) => log::error!("Failed to download Klipper estimator\n{}", e.message),
                    Ok(_) => log::info!("Klipper Estimator download complete."),
                }
            }
            if !Self::check_estimator_perms(&path) {
                server().add_warning("[analysis]: Moonraker lacks permission to execute Klipper Estimator", None, true);
            } else {
                self.detect_estimator_version().await?;
            }
            if self.version() == "?" {
                log::info!("Failed to initialize Klipper Estimator");
            } else {
                let rinfo = path.parent().unwrap_or(Path::new("/")).join("release_info.json");
                if !rinfo.is_file() {
                    log::info!("Creating release_info.json for Klipper Estimator...");
                    let data = json!({
                        "project_name": "klipper_estimator",
                        "project_owner": "Annex-Engineering",
                        "version": self.version(),
                        "asset_name": path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                    });
                    let _ = std::fs::write(&rinfo, data.to_string());
                    if self.updater_registered {
                        log::info!("Refreshing Klipper Estimator Updater Instance...");
                        spawn(async {
                            if let Some(um) = server().lookup_component::<crate::components::update_manager::UpdateManager>("update_manager") {
                                let _ = um.refresh_updater("klipper_estimator", true).await;
                            }
                        });
                    }
                }
                self.estimator_ready.set(true);
                log::info!("Klipper Estimator Version {} detected", self.version());
            }
        }
        self.update_gcode_proc_config();
        Ok(())
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<GcodeAnalysis>, ConfigError> {
    GcodeAnalysis::new(config)
}

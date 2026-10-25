// Net Hosted Application Deployment (port of update_manager/net_deploy.py)

use super::common::*;
use super::*;

type DlInfo = (String, String, i64);

struct NetState {
    repo: String,
    owner: String,
    project_name: String,
    asset_name: Option<String>,
    warnings: Vec<String>,
    anomalies: Vec<String>,
    version: String,
    remote_version: String,
    rollback_version: String,
    rollback_repo: String,
    last_error: String,
    dl_info: DlInfo,
    is_fallback: bool,
    is_prerelease: bool,
    path_writable: bool,
}

pub struct NetDeploy {
    app: AppDeploy,
    persistent_files: Vec<String>,
    st: RefCell<NetState>,
}

fn find_git_repo(p: &Path) -> Option<PathBuf> {
    p.ancestors().find(|a| a.join(".git").exists()).map(|a| a.to_path_buf())
}

impl NetDeploy {
    pub fn new(config: &ConfigHelper, cmd_helper: &Rc<CommandHelper>) -> Result<Self, ConfigError> {
        let mut app = AppDeploy::new(config, cmd_helper, "Zip Application")?;
        app.configure_path(config, false)?;
        match app.type_ {
            AppType::Zip => {
                app.configure_virtualenv(config)?;
                app.configure_dependencies(config, false)?;
                app.configure_managed_services(config)?;
            }
            AppType::Web => *app.base.prefix.borrow_mut() = format!("Web Client {}: ", app.name()),
            AppType::Executable => {
                *app.base.prefix.borrow_mut() = format!("Executable {}: ", app.name());
                app.configure_sysdeps(config)?;
                app.configure_managed_services(config)?;
            }
            _ => {}
        }
        let repo = config.get("repo")?.trim().trim_matches('/').to_string();
        let (owner, project_name) = match repo.split_once('/') {
            Some((o, p)) => (o.to_string(), p.to_string()),
            None => {
                return Err(ConfigHelper::error("not enough values to unpack (expected 2, got 1)"));
            }
        };
        let mut persistent_files: Vec<String> = Vec::new();
        if app.type_ != AppType::Executable {
            if let Some(pfiles) = config.getlist_opt("persistent_files", None, Some("\n"))? {
                persistent_files = pfiles.iter().map(|p| p.trim_matches('/').to_string()).collect();
                for fname in [".version", "release_info.json"] {
                    if persistent_files.iter().any(|p| p == fname) {
                        return Err(ConfigHelper::error(format!(
                            "Invalid value for option 'persistent_files': '{}' can not be persistent.",
                            fname
                        )));
                    }
                }
            }
            if app.type_ == AppType::Zip {
                if let Some(venv) = &app.virtualenv {
                    if venv.ancestors().skip(1).any(|a| a == app.path) {
                        let rel = venv.strip_prefix(&app.path).map(|r| r.display().to_string()).unwrap_or_default();
                        if !persistent_files.contains(&rel) {
                            persistent_files.push(rel);
                        }
                    }
                }
            }
            if !persistent_files.is_empty() {
                app.base.log_info(&format!("Configured persistent files: {}", str_list(&persistent_files)));
            }
        }
        Ok(NetDeploy {
            app,
            persistent_files,
            st: RefCell::new(NetState {
                repo,
                owner,
                project_name,
                asset_name: None,
                warnings: vec![],
                anomalies: vec![],
                version: "?".into(),
                remote_version: "?".into(),
                rollback_version: "?".into(),
                rollback_repo: "?".into(),
                last_error: "?".into(),
                dl_info: ("?".into(), "?".into(), 0),
                is_fallback: false,
                is_prerelease: false,
                path_writable: false,
            }),
        })
    }

    fn set_detected_repo(&self, owner: &str, project: &str) {
        let detected = format!("{}/{}", owner, project);
        let mut st = self.st.borrow_mut();
        if st.repo.to_lowercase() != detected.to_lowercase() {
            let msg = format!(
                "Value at option 'repo: {}' does not match detected repo '{}', falling back to detected version.",
                st.repo, detected
            );
            st.anomalies.push(msg);
            st.repo = detected;
            st.owner = owner.to_string();
            st.project_name = project.to_string();
        }
    }

    fn validate_release_info(&self) {
        self.app.is_valid.set(false);
        {
            let mut st = self.st.borrow_mut();
            st.is_fallback = false;
            st.warnings.clear();
        }
        let path = &self.app.path;
        let repo_parent = find_git_repo(path);
        let homedir = crate::utils::expand_user("~");
        let writable = self.st.borrow().path_writable;
        if !writable {
            self.st.borrow_mut().warnings.push(format!("Location at option 'path: {}' is not writable.", path.display()));
        } else if !path.is_dir() {
            self.st.borrow_mut().warnings.push(format!("Location at option 'path: {}' is not a directory.", path.display()));
        } else if repo_parent.as_ref().map(|r| *r != homedir).unwrap_or(false) {
            let r = repo_parent.unwrap();
            self.st.borrow_mut().warnings.push(format!(
                "Location at option 'path: {}' is within a git repo. Found .git folder at '{}'",
                path.display(),
                r.join(".git").display()
            ));
        } else {
            let rinfo = path.join("release_info.json");
            if rinfo.is_file() {
                let parsed: Result<(String, String, String, Option<String>), String> = (|| {
                    let data = std::fs::read_to_string(&rinfo).map_err(|e| e.to_string())?;
                    let u: Value = serde_json::from_str(&data).map_err(|e| e.to_string())?;
                    let g = |k: &str| u.get(k).map(crate::pyfmt::py_str).ok_or_else(|| format!("KeyError: '{}'", k));
                    Ok((g("project_name")?, g("project_owner")?, g("version")?, u.get("asset_name").and_then(|v| v.as_str()).map(String::from)))
                })();
                match parsed {
                    Err(e) => log::error!("Failed to load release_info.json.\n{}", e),
                    Ok((project_name, owner, version, asset_name)) => {
                        {
                            let mut st = self.st.borrow_mut();
                            st.version = version;
                            st.asset_name = asset_name.clone();
                        }
                        self.app.is_valid.set(true);
                        self.set_detected_repo(&owner, &project_name);
                        if self.app.type_ == AppType::Executable {
                            match asset_name {
                                None => {
                                    self.st
                                        .borrow_mut()
                                        .warnings
                                        .push("Executable types require the 'asset_name' field in release_info.json".into());
                                    self.app.is_valid.set(false);
                                }
                                Some(fname) => {
                                    if !path.join(&fname).exists() {
                                        self.st
                                            .borrow_mut()
                                            .warnings
                                            .push(format!("File {} not found in configured path for executable type", fname));
                                        self.app.is_valid.set(false);
                                    }
                                }
                            }
                        }
                    }
                }
            } else if self.app.type_ == AppType::Web {
                let vpath = path.join(".version");
                if vpath.is_file() {
                    if let Ok(v) = std::fs::read_to_string(&vpath) {
                        self.st.borrow_mut().version = v.trim().to_string();
                    }
                }
                self.app.is_valid.set(self.detect_fallback());
            }
        }
        if !self.app.is_valid.get() {
            self.st.borrow_mut().warnings.push("Failed to validate installation".into());
            if server().is_debug_enabled() {
                self.app.base.log_info("Debug Enabled, overriding validity checks");
            }
        }
    }

    fn detect_fallback(&self) -> bool {
        for fname in ["manifest.json", "manifest.webmanifest"] {
            let manifest = self.app.path.join(fname);
            if !manifest.is_file() {
                continue;
            }
            let proj = std::fs::read_to_string(&manifest)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .and_then(|m| m.get("name").and_then(|n| n.as_str()).map(|n| n.to_lowercase()));
            let proj_name = match proj {
                Some(p) => p,
                None => {
                    self.app.base.log_exc(&format!("Failed to load json from {}", manifest.display()), true);
                    continue;
                }
            };
            let owner = match proj_name.as_str() {
                "mainsail" => "mainsail-crew",
                "fluidd" => "fluidd-core",
                _ => continue,
            };
            let detected = format!("{}/{}", owner, proj_name);
            let mut st = self.st.borrow_mut();
            if detected != st.repo.to_lowercase() {
                let msg = format!(
                    "Value at option 'repo: {}' does not match detected repo '{}', falling back to detected version.",
                    st.repo, detected
                );
                st.anomalies.push(msg);
                st.repo = detected;
                st.owner = owner.to_string();
                st.project_name = proj_name.clone();
            }
            st.is_fallback = true;
            return true;
        }
        false
    }

    async fn fetch_github_version(&self, repo: Option<&str>, tag: Option<&str>) -> Map<String, Value> {
        let repo = match repo {
            Some(r) => r.to_string(),
            None => {
                if !self.app.is_valid.get() {
                    self.app.base.log_info("Invalid Installation, aborting remote refresh");
                    return Map::new();
                }
                self.st.borrow().repo.clone()
            }
        };
        let resource = match tag {
            Some(t) => format!("repos/{}/releases/tags/{}", repo, t),
            None if self.app.channel == Channel::Stable => format!("repos/{}/releases/latest", repo),
            None => format!("repos/{}/releases?per_page=1", repo),
        };
        let resp = match self.app.base.cmd_helper.http().github_api_request(&resource, 3, 0.5).await {
            Ok(r) => r,
            Err(e) => {
                self.app.base.log_info(&format!("Github Request Error - {}", e.message));
                self.st.borrow_mut().last_error = e.message;
                return Map::new();
            }
        };
        let release = if resp.status_code == 304 {
            if resp.content.is_empty() {
                return Map::new();
            }
            resp.json().unwrap_or(json!({}))
        } else if resp.has_error() {
            self.app.base.log_info(&format!("Github Request Error - {}", resp.error_string()));
            self.st.borrow_mut().last_error = resp.error_string();
            return Map::new();
        } else {
            resp.json().unwrap_or(json!({}))
        };
        let result = match release {
            Value::Array(a) => a.first().and_then(|v| v.as_object()).cloned().unwrap_or_default(),
            Value::Object(o) => o,
            _ => Map::new(),
        };
        self.st.borrow_mut().last_error = String::new();
        result
    }

    async fn get_remote_version(&self) {
        let result = self.fetch_github_version(None, None).await;
        if result.is_empty() {
            return;
        }
        let rv = result
            .get("tag_name")
            .or(result.get("name"))
            .filter(|v| crate::pyfmt::py_truthy(v))
            .map(crate::pyfmt::py_str)
            .unwrap_or("?".into());
        let assets: Vec<Value> = result.get("assets").and_then(|v| v.as_array()).cloned().unwrap_or(vec![json!({})]);
        let mut release_asset = assets.first().cloned().unwrap_or(json!({}));
        let asset_name = self.st.borrow().asset_name.clone();
        if let Some(name) = asset_name {
            match assets.iter().find(|a| a.get("name").and_then(|n| n.as_str()) == Some(name.as_str())) {
                Some(a) => release_asset = a.clone(),
                None => log::info!("Asset '{}' not found", name),
            }
        }
        let mut st = self.st.borrow_mut();
        st.remote_version = rv;
        st.dl_info = (
            release_asset.get("browser_download_url").and_then(|v| v.as_str()).unwrap_or("?").to_string(),
            release_asset.get("content_type").and_then(|v| v.as_str()).unwrap_or("?").to_string(),
            release_asset.get("size").and_then(|v| v.as_i64()).unwrap_or(0),
        );
        st.is_prerelease = result.get("prerelease").and_then(|v| v.as_bool()).unwrap_or(false);
    }

    fn log_app_info(&self) {
        let st = self.st.borrow();
        let mut warn_str = String::new();
        if !st.warnings.is_empty() || !st.anomalies.is_empty() {
            warn_str = "\nWarnings:\n".into();
            warn_str.push_str(&st.warnings.iter().chain(st.anomalies.iter()).map(|w| format!(" {}", w)).collect::<Vec<_>>().join("\n"));
        }
        let (dl_url, content_type, size) = &st.dl_info;
        self.app.base.log_info(&format!(
            "Detected\nRepo: {}\nChannel: {}\nPath: {}\nLocal Version: {}\nRemote Version: {}\nValid: {}\nFallback Detected: {}\nPre-release: {}\nDownload Url: {}\nDownload Size: {}\nContent Type: {}\nRollback Version: {}\nRollback Repo: {}{}",
            st.repo,
            self.app.channel.name(),
            self.app.path.display(),
            st.version,
            st.remote_version,
            crate::pyfmt::py_bool(self.app.is_valid.get()),
            crate::pyfmt::py_bool(st.is_fallback),
            crate::pyfmt::py_bool(st.is_prerelease),
            dl_url,
            size,
            content_type,
            st.rollback_version,
            st.rollback_repo,
            warn_str
        ));
    }

    fn extract_release(&self, persist_dir: &Path, release_file: &Path) -> Result<(), ServerError> {
        let path = &self.app.path;
        if !persist_dir.exists() {
            std::fs::create_dir(persist_dir)?;
        }
        if path.is_dir() {
            for entry in std::fs::read_dir(path)?.flatten() {
                let fname = entry.file_name().to_string_lossy().to_string();
                if self.persistent_files.contains(&fname) {
                    let dest = persist_dir.join(&fname);
                    if let Some(d) = dest.parent() {
                        std::fs::create_dir_all(d)?;
                    }
                    move_path(&entry.path(), &dest)?;
                }
            }
            std::fs::remove_dir_all(path)?;
        }
        std::fs::create_dir(path)?;
        let file = std::fs::File::open(release_file)?;
        let mut zf = zip::ZipArchive::new(file).map_err(|e| ServerError::exc(e.to_string()))?;
        for i in 0..zf.len() {
            let mut entry = zf.by_index(i).map_err(|e| ServerError::exc(e.to_string()))?;
            let rel = match entry.enclosed_name() {
                Some(p) => p,
                None => continue,
            };
            let dest = path.join(rel);
            if entry.is_dir() {
                std::fs::create_dir_all(&dest)?;
            } else {
                if let Some(d) = dest.parent() {
                    std::fs::create_dir_all(d)?;
                }
                let mut out = std::fs::File::create(&dest)?;
                std::io::copy(&mut entry, &mut out)?;
            }
            let mode = entry.unix_mode().unwrap_or(0) & 0o777;
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(mode));
        }
        for entry in std::fs::read_dir(persist_dir)?.flatten() {
            let dest = path.join(entry.file_name());
            if let Some(d) = dest.parent() {
                std::fs::create_dir_all(d)?;
            }
            move_path(&entry.path(), &dest)?;
        }
        Ok(())
    }

    fn set_exec_perms(exec: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let req = 0o100 | 0o010;
        if let Ok(md) = std::fs::metadata(exec) {
            let mode = md.permissions().mode() & 0o7777;
            if mode & req != req {
                if let Err(e) = std::fs::set_permissions(exec, std::fs::Permissions::from_mode(mode | req)) {
                    log::error!("Failed to set executable permission for file {}\n{}", exec.display(), e);
                }
            }
        }
    }

    fn finalize_executable(&self, tmp_file: &Path, new_ver: &str) -> Result<(), ServerError> {
        if self.app.type_ != AppType::Executable {
            return Ok(());
        }
        let fname = tmp_file.file_name().unwrap_or_default();
        let exec_path = self.app.path.join(fname);
        if exec_path.is_file() {
            std::fs::remove_file(&exec_path)?;
        }
        move_path(tmp_file, &exec_path)?;
        Self::set_exec_perms(&exec_path);
        let rinfo = self.app.path.join("release_info.json");
        if !rinfo.is_file() {
            return Ok(());
        }
        let mut uinfo: Value = serde_json::from_str(&std::fs::read_to_string(&rinfo)?).map_err(|e| ServerError::exc(e.to_string()))?;
        if uinfo.get("version").map(crate::pyfmt::py_str).as_deref() != Some(new_ver) {
            uinfo["version"] = json!(new_ver);
            std::fs::write(&rinfo, uinfo.to_string())?;
        }
        Ok(())
    }

    async fn do_update(&self, rollback_info: Option<DlInfo>, is_recover: bool, force_dep_update: bool) -> Result<bool, ServerError> {
        let prefix = self.app.base.prefix.borrow().clone();
        if !self.app.is_valid.get() {
            return Err(ServerError::bad(format!("{}Invalid install detected, aborting update", prefix)));
        }
        let (dl_url, content_type, size, new_ver, start_msg) = match &rollback_info {
            Some((u, c, s)) => {
                let st = self.st.borrow();
                let msg = if is_recover { "Recovering..." } else { "Rolling Back..." };
                let nv = if is_recover { st.version.clone() } else { st.rollback_version.clone() };
                (u.clone(), c.clone(), *s, nv, msg)
            }
            None => {
                if self.st.borrow().remote_version == "?" {
                    self.get_remote_version().await;
                    if self.st.borrow().remote_version == "?" {
                        return Err(ServerError::bad(format!("{}Unable to locate update", prefix)));
                    }
                }
                let st = self.st.borrow();
                if st.version == st.remote_version {
                    return Ok(false);
                }
                let (u, c, s) = st.dl_info.clone();
                (u, c, s, st.remote_version.clone(), "Updating...")
            }
        };
        if dl_url == "?" {
            return Err(ServerError::bad(format!("{}Invalid download url", prefix)));
        }
        let current_version = self.st.borrow().version.clone();
        self.app.base.notify_status(start_msg, false);
        self.app.base.notify_status("Downloading Release...", false);
        let dep_info = if matches!(self.app.type_, AppType::Zip | AppType::Executable) {
            Some(self.app.collect_dependency_info())
        } else {
            None
        };
        let ch = self.app.base.cmd_helper.clone();
        let tempdir = ch.create_tempdir(self.app.name(), "app")?;
        let res: Result<(), ServerError> = async {
            let asset_name = self.st.borrow().asset_name.clone();
            let tmp_file = match &asset_name {
                Some(a) => tempdir.join(a),
                None => tempdir.join(format!("{}.zip", self.app.name())),
            };
            let persist = tempdir.join(self.app.name());
            ch.http()
                .download_file(&dl_url, &content_type, Some(tmp_file.clone()), size, Some(ch.on_download_progress()), 180.0, 1, 1.0)
                .await?;
            self.app.base.notify_status(&format!("Download Complete, extracting release to '{}'", self.app.path.display()), false);
            if self.app.type_ == AppType::Executable {
                self.finalize_executable(&tmp_file, &new_ver)
            } else {
                self.extract_release(&persist, &tmp_file)
            }
        }
        .await;
        let _ = std::fs::remove_dir_all(&tempdir);
        res?;
        if let Some(d) = dep_info {
            self.app.update_dependencies(&d, force_dep_update).await;
        }
        self.st.borrow_mut().version = new_ver;
        self.validate_release_info();
        if self.app.is_valid.get() && rollback_info.is_none() {
            let mut st = self.st.borrow_mut();
            st.rollback_version = current_version;
            st.rollback_repo = st.repo.clone();
        }
        self.log_app_info();
        save_state(self);
        self.app.restart_service().await?;
        let msg = if rollback_info.is_none() { "Update Finished..." } else { "Rollback Complete" };
        self.app.base.notify_status(msg, true);
        Ok(true)
    }
}

fn move_path(src: &Path, dest: &Path) -> Result<(), ServerError> {
    if std::fs::rename(src, dest).is_ok() {
        return Ok(());
    }
    // cross device move
    if src.is_dir() {
        copy_dir(src, dest)?;
        std::fs::remove_dir_all(src)?;
    } else {
        std::fs::copy(src, dest)?;
        std::fs::remove_file(src)?;
    }
    Ok(())
}

fn copy_dir(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)?.flatten() {
        let p = entry.path();
        let d = dest.join(entry.file_name());
        if p.is_dir() {
            copy_dir(&p, &d)?;
        } else {
            std::fs::copy(&p, &d)?;
        }
    }
    Ok(())
}

#[async_trait(?Send)]
impl Deployer for NetDeploy {
    fn base(&self) -> &BaseDeploy {
        &self.app.base
    }
    fn kind(&self) -> DeployKind {
        DeployKind::Net
    }
    fn app(&self) -> Option<&AppDeploy> {
        Some(&self.app)
    }

    async fn initialize(&self) -> Map<String, Value> {
        let storage = self.app.initialize();
        let srv = server();
        let fm = srv.get::<crate::components::file_manager::FileManager>();
        let path_str = self.app.path.display().to_string();
        let writable = fm.as_ref().map(|f| !f.is_reserved_path(&path_str, true)).unwrap_or(true);
        self.st.borrow_mut().path_writable = writable;
        if writable && !self.app.path.join(".writeable").is_file() {
            if let Some(f) = &fm {
                f.add_reserved_path(&format!("update_manager {}", self.app.name()), &self.app.path, true);
            }
        }
        self.validate_release_info();
        let s = |k: &str, d: &str| storage.get(k).and_then(|v| v.as_str()).unwrap_or(d).to_string();
        {
            let valid = self.app.is_valid.get();
            let mut st = self.st.borrow_mut();
            if st.version == "?" {
                st.version = s("version", "?");
            }
            st.remote_version = s("remote_version", "?");
            let v = st.version.clone();
            st.rollback_version = s("rollback_version", &v);
            let def_repo = if valid { st.repo.clone() } else { "?".into() };
            st.rollback_repo = s("rollback_repo", &def_repo);
            st.last_error = s("last_error", "");
            if let Some(d) = storage.get("dl_info").and_then(|v| v.as_array()) {
                st.dl_info = (
                    d.first().map(crate::pyfmt::py_str).unwrap_or("?".into()),
                    d.get(1).map(crate::pyfmt::py_str).unwrap_or("?".into()),
                    d.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
                );
            }
        }
        if !self.app.base.needs_refresh(false) {
            self.log_app_info();
        }
        storage
    }

    fn get_persistent_data(&self) -> Map<String, Value> {
        let mut storage = self.app.get_persistent_data();
        let st = self.st.borrow();
        storage.insert("version".into(), json!(st.version));
        storage.insert("remote_version".into(), json!(st.remote_version));
        storage.insert("rollback_version".into(), json!(st.rollback_version));
        storage.insert("rollback_repo".into(), json!(st.rollback_repo));
        storage.insert("dl_info".into(), json!([st.dl_info.0, st.dl_info.1, st.dl_info.2]));
        storage.insert("last_error".into(), json!(st.last_error));
        storage
    }

    async fn refresh(&self) -> Result<(), ServerError> {
        self.validate_release_info();
        self.get_remote_version().await;
        self.log_app_info();
        save_state(self);
        Ok(())
    }

    async fn update(&self) -> Result<bool, ServerError> {
        self.do_update(None, false, false).await
    }

    async fn recover(&self, _hard: bool, force_dep_update: bool) -> Result<(), ServerError> {
        let dl = self.st.borrow().dl_info.clone();
        self.do_update(Some(dl), true, force_dep_update).await.map(|_| ())
    }

    async fn rollback(&self) -> Result<bool, ServerError> {
        let (rv, rr, v) = {
            let st = self.st.borrow();
            (st.rollback_version.clone(), st.rollback_repo.clone(), st.version.clone())
        };
        if rv == "?" || rr == "?" {
            return Err(ServerError::new("Incomplete Rollback Data", 0));
        }
        if rv == v {
            return Ok(false);
        }
        let result = self.fetch_github_version(Some(&rr), Some(&rv)).await;
        if result.is_empty() {
            return Err(ServerError::bad("Failed to retrieve release asset data"));
        }
        let asset = result.get("assets").and_then(|a| a.get(0)).cloned().unwrap_or(json!({}));
        let dl = (
            asset.get("browser_download_url").and_then(|v| v.as_str()).unwrap_or("?").to_string(),
            asset.get("content_type").and_then(|v| v.as_str()).unwrap_or("?").to_string(),
            asset.get("size").and_then(|v| v.as_i64()).unwrap_or(0),
        );
        self.do_update(Some(dl), false, false).await
    }

    fn get_update_status(&self) -> Value {
        let mut status = self.app.get_update_status();
        let st = self.st.borrow();
        let anomalies: Vec<String> = if self.app.report_anomalies { st.anomalies.clone() } else { vec![] };
        status.insert("name".into(), json!(self.app.name()));
        status.insert("repo_name".into(), json!(st.project_name));
        status.insert("owner".into(), json!(st.owner));
        status.insert("version".into(), json!(st.version));
        status.insert("remote_version".into(), json!(st.remote_version));
        status.insert("rollback_version".into(), json!(st.rollback_version));
        status.insert("last_error".into(), json!(st.last_error));
        status.insert("warnings".into(), json!(st.warnings));
        status.insert("anomalies".into(), json!(anomalies));
        Value::Object(status)
    }
}

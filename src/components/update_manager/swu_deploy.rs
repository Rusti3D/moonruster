// SWUpdate Firmware Deployment (port of COSMOS's update_manager/swu_deploy.py,
// a patch to moonraker that updates COSMOS images from GitHub releases)

use super::common::*;
use super::*;

type DlInfo = (String, String, i64);

const SHORT_HASH_LEN: usize = 10;
const DOWNLOAD_TIMEOUT: f64 = 3600.0;
const INSTALL_TIMEOUT: f64 = 1800.0;
const REBOOT_DELAY: f64 = 2.0;
const GIT_MAX_LOG_CNT: usize = 100;

struct SwuState {
    version: String,
    remote_version: String,
    remote_hash: String,
    rollback_version: String,
    branch: String,
    last_error: String,
    dl_info: DlInfo,
    commits_behind: Vec<Value>,
    commits_behind_count: i64,
    warnings: Vec<String>,
    reboot_pending: bool,
}

pub struct SwuDeploy {
    app: AppDeploy,
    repo: String,
    owner: String,
    project_name: String,
    asset_name: String,
    install_cmd: String,
    nightly_tag: String,
    st: RefCell<SwuState>,
}

fn dl_info_from(v: Option<&Value>) -> DlInfo {
    let d = v.and_then(|v| v.as_array()).cloned().unwrap_or_default();
    (
        d.first().map(crate::pyfmt::py_str).unwrap_or("?".into()),
        d.get(1).map(crate::pyfmt::py_str).unwrap_or("?".into()),
        d.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
    )
}

impl SwuDeploy {
    pub fn new(config: &ConfigHelper, cmd_helper: &Rc<CommandHelper>) -> Result<Self, ConfigError> {
        let app = AppDeploy::new(config, cmd_helper, "SWUpdate")?;
        let repo = config.get("repo")?.trim().trim_matches('/').to_string();
        let (owner, project_name) = match repo.split_once('/') {
            Some((o, p)) => (o.to_string(), p.to_string()),
            None => return Err(ConfigHelper::error("not enough values to unpack (expected 2, got 1)")),
        };
        Ok(SwuDeploy {
            app,
            asset_name: config.get("asset_name")?,
            install_cmd: config.get_or("install_command", "flash")?,
            nightly_tag: config.get_or("nightly_release_tag", "0.0.0")?,
            repo,
            owner,
            project_name,
            st: RefCell::new(SwuState {
                version: "?".into(),
                remote_version: "?".into(),
                remote_hash: "?".into(),
                rollback_version: "?".into(),
                branch: "?".into(),
                last_error: String::new(),
                dl_info: ("?".into(), "?".into(), 0),
                commits_behind: vec![],
                commits_behind_count: 0,
                warnings: vec![],
                reboot_pending: false,
            }),
        })
    }

    fn validate_install(&self) {
        let mut st = self.st.borrow_mut();
        st.warnings.clear();
        self.app.is_valid.set(true);
        st.version = crate::components::machine::parse_os_release()
            .get("version_id")
            .filter(|v| !v.is_empty())
            .cloned()
            .unwrap_or("?".into());
        if st.version == "?" {
            st.warnings.push("Unable to detect installed version from VERSION_ID in os-release".into());
            self.app.is_valid.set(false);
        }
        // a command shlex cannot split is not found either
        let cmd = crate::components::shell_command::shlex_split(&self.install_cmd).unwrap_or_default();
        if cmd.first().and_then(|c| crate::components::machine::which(c)).is_none() {
            st.warnings.push(format!("Install command '{}' not found", self.install_cmd));
            self.app.is_valid.set(false);
        }
    }

    async fn github_request(&self, resource: &str) -> Option<Value> {
        let resp = match self.app.base.cmd_helper.http().github_api_request(resource, 3, 0.5).await {
            Ok(r) => r,
            Err(e) => {
                self.app.base.log_info(&format!("Github Request Error - {}", e.message));
                self.st.borrow_mut().last_error = e.message;
                return None;
            }
        };
        if resp.status_code == 304 {
            return if resp.content.is_empty() { None } else { resp.json().ok() };
        }
        if resp.has_error() {
            self.app.base.log_info(&format!("Github Request Error - {}", resp.error_string()));
            self.st.borrow_mut().last_error = resp.error_string();
            return None;
        }
        self.st.borrow_mut().last_error = String::new();
        resp.json().ok().filter(crate::pyfmt::py_truthy)
    }

    fn find_asset(&self, release: &Value) -> DlInfo {
        let assets = release.get("assets").and_then(|a| a.as_array()).cloned().unwrap_or_default();
        match assets.iter().find(|a| a.get("name").and_then(|n| n.as_str()) == Some(self.asset_name.as_str())) {
            Some(a) => (
                a.get("browser_download_url").and_then(|v| v.as_str()).unwrap_or("?").to_string(),
                a.get("content_type").and_then(|v| v.as_str()).unwrap_or("?").to_string(),
                a.get("size").and_then(|v| v.as_i64()).unwrap_or(0),
            ),
            None => ("?".into(), "?".into(), 0),
        }
    }

    async fn refresh_stable(&self) {
        let release = match self.github_request(&format!("repos/{}/releases/latest", self.repo)).await {
            Some(r) => r,
            None => return,
        };
        let mut st = self.st.borrow_mut();
        st.remote_version = release.get("tag_name").map(crate::pyfmt::py_str).unwrap_or("?".into());
        st.remote_hash = "?".into();
        st.branch = release.get("target_commitish").map(crate::pyfmt::py_str).unwrap_or("?".into());
        st.dl_info = self.find_asset(&release);
        st.commits_behind.clear();
        st.commits_behind_count = 0;
    }

    async fn refresh_nightly(&self) {
        let release = match self.github_request(&format!("repos/{}/releases/tags/{}", self.repo, self.nightly_tag)).await {
            Some(r) => r,
            None => return,
        };
        {
            let mut st = self.st.borrow_mut();
            st.branch = release.get("target_commitish").map(crate::pyfmt::py_str).unwrap_or("?".into());
            st.dl_info = self.find_asset(&release);
        }
        let commits = match self.github_request(&format!("repos/{}/commits?sha={}&per_page=1", self.repo, self.nightly_tag)).await {
            Some(c) => c,
            None => return,
        };
        let sha = commits.get(0).and_then(|c| c.get("sha")).and_then(|v| v.as_str()).unwrap_or("?");
        let remote_hash: String = sha.chars().take(SHORT_HASH_LEN).collect();
        let version = {
            let mut st = self.st.borrow_mut();
            st.remote_hash = remote_hash.clone();
            st.remote_version = remote_hash.clone();
            st.version.clone()
        };
        if version == "?" {
            return;
        }
        let comparison = self.github_request(&format!("repos/{}/compare/{}...{}", self.repo, version, remote_hash)).await;
        let mut st = self.st.borrow_mut();
        let comparison = match comparison {
            Some(c) => c,
            None => {
                if !st.last_error.is_empty() {
                    st.commits_behind.clear();
                    st.commits_behind_count = 0;
                }
                return;
            }
        };
        st.commits_behind_count = comparison.get("ahead_by").and_then(|v| v.as_i64()).unwrap_or(0);
        let new_commits = comparison.get("commits").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let skip = new_commits.len().saturating_sub(GIT_MAX_LOG_CNT);
        st.commits_behind = new_commits[skip..].iter().rev().map(Self::format_commit).collect();
    }

    fn format_commit(commit: &Value) -> Value {
        let info = commit.get("commit").cloned().unwrap_or(json!({}));
        let author = info.get("author").cloned().unwrap_or(json!({}));
        let s = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let message = s(&info, "message");
        let (subject, body) = message.split_once('\n').unwrap_or((&message, ""));
        let timestamp = chrono::DateTime::parse_from_rfc3339(&s(&author, "date")).map(|d| d.timestamp()).unwrap_or(0);
        json!({
            "sha": s(commit, "sha"),
            "author": s(&author, "name"),
            "date": timestamp.to_string(),
            "subject": subject.trim(),
            "message": body.trim(),
            "tag": null,
        })
    }

    fn log_app_info(&self) {
        let st = self.st.borrow();
        let mut warn_str = String::new();
        if !st.warnings.is_empty() {
            warn_str = format!("\nWarnings:\n{}", st.warnings.iter().map(|w| format!(" {}", w)).collect::<Vec<_>>().join("\n"));
        }
        let (dl_url, content_type, size) = &st.dl_info;
        self.app.base.log_info(&format!(
            "Detected\nRepo: {}\nChannel: {}\nLocal Version: {}\nRemote Version: {}\nRemote Hash: {}\nCommits Behind: {}\nValid: {}\nDownload Url: {}\nDownload Size: {}\nContent Type: {}\nRollback Version: {}{}",
            self.repo,
            self.app.channel.name(),
            st.version,
            st.remote_version,
            st.remote_hash,
            st.commits_behind_count,
            crate::pyfmt::py_bool(self.app.is_valid.get()),
            dl_url,
            size,
            content_type,
            st.rollback_version,
            warn_str
        ));
    }

    fn update_available(&self) -> bool {
        let st = self.st.borrow();
        if self.app.channel == Channel::Dev {
            return st.commits_behind_count > 0;
        }
        st.remote_version != "?" && st.remote_version != st.version
    }

    async fn do_update(&self, rollback_info: Option<DlInfo>) -> Result<bool, ServerError> {
        let prefix = self.app.base.prefix.borrow().clone();
        if self.st.borrow().reboot_pending {
            return Err(ServerError::bad(format!("{}Reboot pending, aborting update", prefix)));
        }
        if !self.app.is_valid.get() {
            return Err(ServerError::bad(format!("{}Invalid install detected, aborting update", prefix)));
        }
        let is_rollback = rollback_info.is_some();
        let (dl_info, new_ver, start_msg) = match rollback_info {
            Some(dl) => (dl, self.st.borrow().rollback_version.clone(), "Rolling Back..."),
            None => {
                if !self.update_available() {
                    return Ok(false);
                }
                let st = self.st.borrow();
                (st.dl_info.clone(), st.remote_version.clone(), "Updating...")
            }
        };
        let (dl_url, content_type, size) = dl_info;
        if dl_url == "?" {
            return Err(ServerError::bad(format!("{}Release asset '{}' not found", prefix, self.asset_name)));
        }
        let current_version = self.st.borrow().version.clone();
        self.app.base.notify_status(start_msg, false);
        self.app.base.notify_status("Downloading Release...", false);
        let ch = self.app.base.cmd_helper.clone();
        let tempdir = ch.create_tempdir(self.app.name(), "app")?;
        let res: Result<(), ServerError> = async {
            let swu_file = tempdir.join(&self.asset_name);
            ch.http()
                .download_file(
                    &dl_url,
                    &content_type,
                    crate::components::http_client::DownloadOpts {
                        destination: Some(swu_file.clone()),
                        download_size: size,
                        progress: Some(ch.on_download_progress()),
                        request_timeout: DOWNLOAD_TIMEOUT,
                        attempts: 3,
                        ..Default::default()
                    },
                )
                .await?;
            self.app.base.notify_status("Download Complete, installing firmware...", false);
            let quoted = format!("'{}'", swu_file.to_string_lossy().replace('\'', "'\\''"));
            ch.run_cmd(&format!("{} {}", self.install_cmd, quoted), INSTALL_TIMEOUT, true, 1, None, None, 1, true).await
        }
        .await;
        let _ = std::fs::remove_dir_all(&tempdir);
        res?;
        {
            let mut st = self.st.borrow_mut();
            if !is_rollback {
                st.rollback_version = current_version;
            }
            st.reboot_pending = true;
        }
        save_state(self);
        self.app.base.notify_status(&format!("Installed version {}, rebooting...", new_ver), false);
        // reboot once the update manager has finished reporting this update
        let ch2 = ch.clone();
        spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs_f64(REBOOT_DELAY)).await;
                if !ch2.is_update_busy() {
                    break;
                }
            }
            log::info!("SWUpdate: Rebooting to complete firmware update");
            if let Some(m) = server().get::<Machine>() {
                if let Err(e) = m.get_system_provider().reboot().await {
                    log::error!("SWUpdate: Reboot failed: {}", e.message);
                }
            }
        });
        let msg = if is_rollback { "Rollback Complete" } else { "Update Finished..." };
        self.app.base.notify_status(msg, true);
        Ok(true)
    }
}

#[async_trait(?Send)]
impl Deployer for SwuDeploy {
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
        self.validate_install();
        let s = |k: &str, d: &str| storage.get(k).and_then(|v| v.as_str()).unwrap_or(d).to_string();
        let version_changed = {
            let mut st = self.st.borrow_mut();
            st.remote_version = s("remote_version", "?");
            st.remote_hash = s("remote_hash", "?");
            st.rollback_version = s("rollback_version", "?");
            st.branch = s("branch", "?");
            st.last_error = s("last_error", "");
            st.dl_info = dl_info_from(storage.get("dl_info"));
            st.commits_behind = storage.get("commits_behind").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            st.commits_behind_count = storage.get("cbh_count").and_then(|v| v.as_i64()).unwrap_or(0);
            let changed = s("version", &st.version) != st.version;
            if changed {
                st.commits_behind.clear();
                st.commits_behind_count = 0;
            }
            changed
        };
        if version_changed {
            self.app.base.last_refresh_time.set(0.0);
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
        storage.insert("remote_hash".into(), json!(st.remote_hash));
        storage.insert("rollback_version".into(), json!(st.rollback_version));
        storage.insert("branch".into(), json!(st.branch));
        storage.insert("dl_info".into(), json!([st.dl_info.0, st.dl_info.1, st.dl_info.2]));
        storage.insert("commits_behind".into(), json!(st.commits_behind));
        storage.insert("cbh_count".into(), json!(st.commits_behind_count));
        storage.insert("last_error".into(), json!(st.last_error));
        storage
    }

    async fn refresh(&self) -> Result<(), ServerError> {
        self.validate_install();
        if self.app.channel == Channel::Dev {
            self.refresh_nightly().await;
        } else {
            self.refresh_stable().await;
        }
        self.log_app_info();
        save_state(self);
        Ok(())
    }

    async fn update(&self) -> Result<bool, ServerError> {
        self.do_update(None).await
    }

    async fn rollback(&self) -> Result<bool, ServerError> {
        let (rv, v) = {
            let st = self.st.borrow();
            (st.rollback_version.clone(), st.version.clone())
        };
        if rv == "?" || rv == v {
            return Ok(false);
        }
        let release = self
            .github_request(&format!("repos/{}/releases/tags/{}", self.repo, rv))
            .await
            .ok_or_else(|| ServerError::bad(format!("No release found for rollback version {}", rv)))?;
        self.do_update(Some(self.find_asset(&release))).await
    }

    fn get_update_status(&self) -> Value {
        let mut status = self.app.get_update_status();
        let st = self.st.borrow();
        let mut warnings = st.warnings.clone();
        if st.remote_version != "?" && st.dl_info.0 == "?" {
            warnings.push(format!("Asset '{}' not found in remote release", self.asset_name));
        }
        status.insert("name".into(), json!(self.app.name()));
        status.insert("owner".into(), json!(self.owner));
        status.insert("repo_name".into(), json!(self.project_name));
        status.insert("version".into(), json!(st.version));
        status.insert("remote_version".into(), json!(st.remote_version));
        status.insert("rollback_version".into(), json!(st.rollback_version));
        status.insert("last_error".into(), json!(st.last_error));
        status.insert("warnings".into(), json!(warnings));
        status.insert("anomalies".into(), json!([]));
        if self.app.channel != Channel::Dev {
            status.insert("configured_type".into(), json!(AppType::Web.name()));
            return Value::Object(status);
        }
        let repo_url = format!("https://github.com/{}", self.repo);
        let extra = json!({
            "configured_type": AppType::GitRepo.name(),
            "detected_type": AppType::GitRepo.name(),
            "repo_detected": true,
            "remote_alias": "origin",
            "branch": st.branch,
            "remote_url": repo_url,
            "recovery_url": repo_url,
            "current_hash": st.version,
            "remote_hash": st.remote_hash,
            "is_dirty": false,
            "detached": false,
            "commits_behind": st.commits_behind,
            "commits_behind_count": st.commits_behind_count,
            "git_messages": [],
            "full_version_string": st.version,
            "pristine": true,
            "corrupt": false,
        });
        for (k, v) in extra.as_object().unwrap() {
            status.insert(k.clone(), v.clone());
        }
        Value::Object(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_commit() {
        let c = SwuDeploy::format_commit(&json!({
            "sha": "abc",
            "commit": {"message": "Subject\n\nBody text\n", "author": {"name": "A", "date": "2026-10-01T00:00:00Z"}}
        }));
        assert_eq!(c["subject"], "Subject");
        assert_eq!(c["message"], "Body text");
        assert_eq!(c["date"], "1790812800");
        assert_eq!(c["author"], "A");
    }
}

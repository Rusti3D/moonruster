// Moonruster self update: the built-in `moonraker` entry.  Moonraker updates
// itself from git; Moonruster is a single binary, so it replaces itself with
// the release asset for its architecture and restarts the service.

use super::common::*;
use super::*;

const REPO: &str = "Rusti3D/moonruster";
const DOWNLOAD_TIMEOUT: f64 = 600.0;

/// Release asset name for the architecture this binary was built for
fn asset_name() -> Option<&'static str> {
    if cfg!(target_arch = "aarch64") {
        Some("moonruster-linux-aarch64")
    } else if cfg!(all(target_arch = "arm", target_feature = "v7")) {
        Some("moonruster-linux-armv7")
    } else if cfg!(target_arch = "arm") {
        Some("moonruster-linux-armv6")
    } else if cfg!(target_arch = "x86_64") {
        Some("moonruster-linux-x86_64")
    } else {
        None
    }
}

struct SelfState {
    remote_version: String,
    dl_info: (String, String, i64),
    last_error: String,
}

pub struct SelfDeploy {
    app: AppDeploy,
    version: String,
    exe: Option<PathBuf>,
    st: RefCell<SelfState>,
}

impl SelfDeploy {
    pub fn new(config: &ConfigHelper, cmd_helper: &Rc<CommandHelper>) -> Result<Self, ConfigError> {
        let mut app = AppDeploy::new(config, cmd_helper, "Moonruster")?;
        app.configure_managed_services(config)?;
        let exe = std::env::current_exe().ok();
        app.is_valid.set(exe.is_some() && asset_name().is_some());
        Ok(SelfDeploy {
            app,
            version: format!("v{}", env!("CARGO_PKG_VERSION")),
            exe,
            st: RefCell::new(SelfState { remote_version: "?".into(), dl_info: ("?".into(), "?".into(), 0), last_error: String::new() }),
        })
    }

    fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        if asset_name().is_none() {
            w.push("No release binaries for this architecture".to_string());
        }
        if self.exe.is_none() {
            w.push("Unable to locate the moonruster executable".to_string());
        }
        w
    }

    async fn fetch_release(&self) {
        // stable: latest release, beta/dev: newest including pre-releases
        let resource = if self.app.channel == Channel::Stable {
            format!("repos/{}/releases/latest", REPO)
        } else {
            format!("repos/{}/releases?per_page=1", REPO)
        };
        let resp = match self.app.base.cmd_helper.http().github_api_request(&resource, 3, 0.5).await {
            Ok(r) => r,
            Err(e) => {
                self.st.borrow_mut().last_error = e.message;
                return;
            }
        };
        if resp.has_error() {
            self.st.borrow_mut().last_error = resp.error_string();
            return;
        }
        let release = match resp.json().unwrap_or(Value::Null) {
            Value::Array(a) => a.into_iter().next().unwrap_or(Value::Null),
            v => v,
        };
        let mut st = self.st.borrow_mut();
        st.last_error = String::new();
        if release.is_null() {
            return;
        }
        st.remote_version = release.get("tag_name").and_then(|v| v.as_str()).unwrap_or("?").to_string();
        let asset = release
            .get("assets")
            .and_then(|a| a.as_array())
            .and_then(|a| a.iter().find(|x| x.get("name").and_then(|n| n.as_str()) == asset_name()).cloned())
            .unwrap_or(Value::Null);
        st.dl_info = (
            asset.get("browser_download_url").and_then(|v| v.as_str()).unwrap_or("?").to_string(),
            asset.get("content_type").and_then(|v| v.as_str()).unwrap_or("?").to_string(),
            asset.get("size").and_then(|v| v.as_i64()).unwrap_or(0),
        );
    }

    /// Replaces the running binary: renamed into place when its folder is
    /// writable (running as root), otherwise `sudo install` (Debian, /usr/local/bin)
    async fn install(&self, exe: &Path, dl: &(String, String, i64)) -> Result<(), ServerError> {
        let ch = self.app.base.cmd_helper.clone();
        let dir = exe.parent().unwrap_or(Path::new("/"));
        if crate::utils::access_write(dir) {
            let tmp = dir.join(".moonruster.update");
            ch.http()
                .download_file(&dl.0, &dl.1, Some(tmp.clone()), dl.2, Some(ch.on_download_progress()), DOWNLOAD_TIMEOUT, 3, 1.0)
                .await?;
            use std::os::unix::fs::PermissionsExt;
            let res = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).and_then(|_| std::fs::rename(&tmp, exe));
            if res.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            return res.map_err(ServerError::from);
        }
        let tempdir = ch.create_tempdir("moonruster", "app")?;
        let tmp = tempdir.join("moonruster");
        let res: Result<(), ServerError> = async {
            ch.http()
                .download_file(&dl.0, &dl.1, Some(tmp.clone()), dl.2, Some(ch.on_download_progress()), DOWNLOAD_TIMEOUT, 3, 1.0)
                .await?;
            let machine: Rc<Machine> = server().lookup_component("machine").ok_or_else(|| ServerError::new("machine", 500))?;
            machine.exec_sudo_command(&format!("install -m 755 {} {}", tmp.display(), exe.display()), 1, 30.0).await?;
            Ok(())
        }
        .await;
        let _ = std::fs::remove_dir_all(&tempdir);
        res
    }
}

#[async_trait(?Send)]
impl Deployer for SelfDeploy {
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
        let mut st = self.st.borrow_mut();
        st.remote_version = storage.get("remote_version").and_then(|v| v.as_str()).unwrap_or("?").to_string();
        st.last_error = storage.get("last_error").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if let Some(d) = storage.get("dl_info").and_then(|v| v.as_array()) {
            st.dl_info = (
                d.first().map(crate::pyfmt::py_str).unwrap_or("?".into()),
                d.get(1).map(crate::pyfmt::py_str).unwrap_or("?".into()),
                d.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
            );
        }
        storage
    }

    fn get_persistent_data(&self) -> Map<String, Value> {
        let mut storage = self.app.get_persistent_data();
        let st = self.st.borrow();
        storage.insert("remote_version".into(), json!(st.remote_version));
        storage.insert("dl_info".into(), json!([st.dl_info.0, st.dl_info.1, st.dl_info.2]));
        storage.insert("last_error".into(), json!(st.last_error));
        storage
    }

    async fn refresh(&self) -> Result<(), ServerError> {
        if self.app.is_valid.get() {
            self.fetch_release().await;
        }
        let st = self.st.borrow();
        self.app.base.log_info(&format!(
            "Detected\nRepo: {}\nChannel: {}\nLocal Version: {}\nRemote Version: {}\nDownload Url: {}\nValid: {}",
            REPO,
            self.app.channel.name(),
            self.version,
            st.remote_version,
            st.dl_info.0,
            crate::pyfmt::py_bool(self.app.is_valid.get())
        ));
        drop(st);
        save_state(self);
        Ok(())
    }

    async fn update(&self) -> Result<bool, ServerError> {
        let prefix = self.app.base.prefix.borrow().clone();
        let exe = match (&self.exe, self.app.is_valid.get()) {
            (Some(e), true) => e.clone(),
            _ => return Err(ServerError::bad(format!("{}Invalid install detected, aborting update", prefix))),
        };
        let (remote, dl) = {
            let st = self.st.borrow();
            (st.remote_version.clone(), st.dl_info.clone())
        };
        if remote == "?" || remote == self.version {
            return Ok(false);
        }
        if dl.0 == "?" {
            return Err(ServerError::bad(format!("{}Release {} has no {}", prefix, remote, asset_name().unwrap_or("?"))));
        }
        self.app.base.notify_status("Updating...", false);
        self.app.base.notify_status("Downloading Release...", false);
        self.install(&exe, &dl).await?;
        self.app.base.notify_status(&format!("Installed {}", remote), false);
        save_state(self);
        self.app.restart_service().await?;
        self.app.base.notify_status("Update Finished...", true);
        Ok(true)
    }

    fn get_update_status(&self) -> Value {
        let mut status = self.app.get_update_status();
        let st = self.st.borrow();
        let (owner, repo_name) = REPO.split_once('/').unwrap_or(("", REPO));
        status.insert("name".into(), json!(self.app.name()));
        status.insert("owner".into(), json!(owner));
        status.insert("repo_name".into(), json!(repo_name));
        status.insert("version".into(), json!(self.version));
        status.insert("remote_version".into(), json!(st.remote_version));
        status.insert("rollback_version".into(), json!(self.version));
        status.insert("last_error".into(), json!(st.last_error));
        status.insert("warnings".into(), json!(self.warnings()));
        status.insert("anomalies".into(), json!([]));
        Value::Object(status)
    }
}

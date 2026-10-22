// Git Deployment implementation (port of update_manager/git_deploy.py)

use super::common::*;
use super::*;
use crate::components::shell_command::{ExecOpts, ShellCommandError};

const GIT_ASYNC_TIMEOUT: f64 = 300.0;
const GIT_MAX_LOG_CNT: i64 = 100;
const GIT_LOG_FMT: &str = "\"sha:%H%x1Dauthor:%an%x1Ddate:%ct%x1Dsubject:%s%x1Dmessage:%b%x1E\"";
const GIT_REF_FMT: &str =
    "'%(if)%(*objecttype)%(then)%(*objecttype) %(*objectname)%(else)%(objecttype) %(objectname)%(end) %(refname)'";
const SRC_EXTS: [&str; 3] = [".py", ".c", ".cpp"];

pub struct GitDeploy {
    app: AppDeploy,
    primary_branch: String,
    repo: Rc<GitRepo>,
}

impl GitDeploy {
    pub fn new(config: &ConfigHelper, cmd_helper: &Rc<CommandHelper>) -> Result<Self, ConfigError> {
        let mut app = AppDeploy::new(config, cmd_helper, "Git Repo")?;
        app.configure_path(config, true)?;
        app.configure_virtualenv(config)?;
        app.configure_dependencies(config, false)?;
        app.configure_managed_services(config)?;
        let origin = config.get("origin")?;
        let moved_origin = config.get_opt("moved_origin")?;
        let primary_branch = config.get_or("primary_branch", "master")?;
        let mut pinned_commit = config.get_opt("pinned_commit")?;
        if let Some(p) = &pinned_commit {
            let p = p.to_lowercase();
            if p.len() < 8 {
                return Err(ConfigHelper::error(format!(
                    "[{}]: Value for option 'commit' must be a minimum of 8 characters.",
                    config.get_name()
                )));
            }
            pinned_commit = Some(p);
        }
        let repo = Rc::new(GitRepo::new(
            cmd_helper,
            &app.path,
            app.name(),
            &origin,
            moved_origin,
            &primary_branch,
            app.channel,
            pinned_commit,
        ));
        Ok(GitDeploy { app, primary_branch, repo })
    }

    async fn update_repo_state(&self, need_fetch: bool, raise_exc: bool) -> Result<(), ServerError> {
        self.app.is_valid.set(false);
        let res = self.repo.refresh_repo_state(need_fetch).await;
        if res.is_ok() {
            self.app.is_valid.set(self.repo.is_valid());
        }
        self.app.base.log_info(&format!("Channel: {}", self.app.channel.name()));
        if !self.app.is_valid.get() {
            self.app.base.log_info("Repo validation check failed, updates disabled");
        } else {
            self.app.base.log_info("Validity check for git repo passed");
        }
        save_state(self);
        match res {
            Err(e) if raise_exc => Err(e),
            _ => Ok(()),
        }
    }

    async fn pull_repo(&self) -> Result<(), ServerError> {
        self.app.base.notify_status("Updating Repo...", false);
        let rb_state = self.repo.capture_state_for_rollback();
        let res: Result<(), ServerError> = async {
            self.repo.fetch().await?;
            if self.repo.is_detached() {
                self.repo.checkout(None).await?;
            } else if self.repo.check_diverged().await? {
                self.app.base.notify_status("Repo has diverged, attempting git reset", false);
                self.repo.reset(None).await?;
            } else {
                self.repo.pull().await?;
            }
            Ok(())
        }
        .await;
        match res {
            Err(e) => {
                if self.repo.st.borrow().repo_corrupt {
                    self.app.is_valid.set(false);
                    save_state(self);
                    self.app.base.cmd_helper.notify_update_refreshed_delayed();
                }
                Err(self.app.base.log_exc(&e.message, true))
            }
            Ok(()) => {
                self.repo.set_rollback_state(Some(rb_state));
                Ok(())
            }
        }
    }
}

#[async_trait(?Send)]
impl Deployer for GitDeploy {
    fn base(&self) -> &BaseDeploy {
        &self.app.base
    }
    fn kind(&self) -> DeployKind {
        DeployKind::Git
    }
    fn app(&self) -> Option<&AppDeploy> {
        Some(&self.app)
    }

    async fn initialize(&self) -> Map<String, Value> {
        let storage = self.app.initialize();
        self.repo.restore_state(&storage);
        self.app.is_valid.set(self.repo.is_valid());
        if !self.app.base.needs_refresh(false) {
            self.repo.log_repo_info();
        }
        storage
    }

    async fn refresh(&self) -> Result<(), ServerError> {
        self.update_repo_state(true, false).await
    }

    async fn update(&self) -> Result<bool, ServerError> {
        self.repo.wait_for_refresh().await;
        if !self.app.is_valid.get() {
            return Err(self.app.base.log_exc("Update aborted, repo not valid", false));
        }
        if self.repo.is_dirty() {
            return Err(self.app.base.log_exc("Update aborted, repo has been modified", false));
        }
        if self.repo.is_current() {
            return Ok(false);
        }
        self.app.base.cmd_helper.notify_update_response(&format!("Updating Application {}...", self.app.name()), false);
        let dep_info = self.app.collect_dependency_info();
        self.pull_repo().await?;
        self.app.update_dependencies(&dep_info, false).await;
        self.update_repo_state(false, true).await?;
        self.app.restart_service().await?;
        self.app.base.notify_status("Update Finished...", true);
        Ok(true)
    }

    async fn recover(&self, hard: bool, force_dep_update: bool) -> Result<(), ServerError> {
        self.app.base.notify_status("Attempting Repo Recovery...", false);
        let dep_info = self.app.collect_dependency_info();
        if hard {
            self.repo.clone_repo().await?;
            self.update_repo_state(true, true).await?;
        } else {
            self.app.base.notify_status("Resetting Git Repo...", false);
            let reset_ref = self.repo.get_recovery_ref(None).await?;
            if self.repo.is_dirty() {
                let _ = self.repo.checkout(Some("-- .")).await;
            }
            self.repo.checkout(Some(&self.primary_branch)).await?;
            self.repo.reset(Some(&reset_ref)).await?;
            self.update_repo_state(true, true).await?;
        }
        self.repo.set_rollback_state(None);
        if self.repo.is_dirty() || !self.app.is_valid.get() {
            return Err(ServerError::new("Recovery attempt failed, repo state not pristine", 500));
        }
        self.app.update_dependencies(&dep_info, force_dep_update).await;
        self.app.restart_service().await?;
        self.app.base.notify_status("Reinstall Complete", true);
        Ok(())
    }

    async fn rollback(&self) -> Result<bool, ServerError> {
        let dep_info = self.app.collect_dependency_info();
        let ret = self.repo.rollback().await?;
        let msg = if ret {
            self.app.update_dependencies(&dep_info, false).await;
            self.update_repo_state(false, true).await?;
            self.app.restart_service().await?;
            "Rollback Complete"
        } else {
            "Rollback not performed"
        };
        self.app.base.notify_status(msg, true);
        Ok(ret)
    }

    fn get_update_status(&self) -> Value {
        let mut status = self.app.get_update_status();
        for (k, v) in self.repo.get_repo_status(self.app.report_anomalies) {
            status.insert(k, v);
        }
        status.insert("name".into(), json!(self.app.name()));
        Value::Object(status)
    }

    fn get_persistent_data(&self) -> Map<String, Value> {
        let mut storage = self.app.get_persistent_data();
        for (k, v) in self.repo.get_persistent_data() {
            storage.insert(k, v);
        }
        storage
    }
}

struct RepoState {
    valid_git_repo: bool,
    git_owner: String,
    git_repo_name: String,
    git_remote: String,
    git_branch: String,
    current_version: Version,
    upstream_version: Version,
    current_commit: String,
    upstream_commit: String,
    upstream_url: String,
    recovery_url: String,
    branches: Vec<String>,
    head_detached: bool,
    git_messages: Vec<String>,
    commits_behind: Vec<Value>,
    commits_behind_count: i64,
    diverged: bool,
    repo_corrupt: bool,
    modified_files: Vec<String>,
    untracked_files: Vec<String>,
    rollback_commit: String,
    rollback_branch: String,
    rollback_version: Version,
    pinned_commit_valid: bool,
    is_shallow: bool,
    git_folder_path: PathBuf,
    repo_warnings: Vec<String>,
    repo_anomalies: Vec<String>,
}

pub struct RollbackState {
    commit: String,
    branch: String,
    version: Version,
}

pub struct GitRepo {
    cmd_helper: Rc<CommandHelper>,
    alias: String,
    src_path: PathBuf,
    backup_path: PathBuf,
    origin_url: String,
    moved_origin_url: Option<String>,
    primary_branch: String,
    channel: Channel,
    pinned_commit: Option<String>,
    refresh_lock: tokio::sync::Mutex<()>,
    git_operation_lock: tokio::sync::Mutex<()>,
    fetch_input_recd: Rc<Cell<bool>>,
    st: Rc<RefCell<RepoState>>,
}

fn ok_str(r: Result<String, ShellCommandError>) -> Result<String, ServerError> {
    r.map_err(ServerError::from)
}

impl GitRepo {
    #[allow(clippy::too_many_arguments)]
    fn new(
        cmd_helper: &Rc<CommandHelper>,
        src_path: &Path,
        alias: &str,
        origin_url: &str,
        moved_origin_url: Option<String>,
        primary_branch: &str,
        channel: Channel,
        pinned_commit: Option<String>,
    ) -> Self {
        let git_dir = src_path.parent().unwrap_or(Path::new("/"));
        let git_base = src_path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut origin = origin_url.to_string();
        if !origin.ends_with(".git") {
            origin.push_str(".git");
        }
        GitRepo {
            cmd_helper: cmd_helper.clone(),
            alias: alias.to_string(),
            src_path: src_path.to_path_buf(),
            backup_path: git_dir.join(format!(".{}_repo_backup", git_base)),
            origin_url: origin,
            moved_origin_url,
            primary_branch: primary_branch.to_string(),
            channel,
            pinned_commit,
            refresh_lock: tokio::sync::Mutex::new(()),
            git_operation_lock: tokio::sync::Mutex::new(()),
            fetch_input_recd: Rc::new(Cell::new(false)),
            st: Rc::new(RefCell::new(RepoState {
                valid_git_repo: false,
                git_owner: "?".into(),
                git_repo_name: "?".into(),
                git_remote: "?".into(),
                git_branch: "?".into(),
                current_version: Version::git("?"),
                upstream_version: Version::git("?"),
                current_commit: "?".into(),
                upstream_commit: "?".into(),
                upstream_url: "?".into(),
                recovery_url: "?".into(),
                branches: vec![],
                head_detached: false,
                git_messages: vec![],
                commits_behind: vec![],
                commits_behind_count: 0,
                diverged: false,
                repo_corrupt: false,
                modified_files: vec![],
                untracked_files: vec![],
                rollback_commit: "?".into(),
                rollback_branch: "?".into(),
                rollback_version: Version::git("?"),
                pinned_commit_valid: true,
                is_shallow: false,
                git_folder_path: src_path.join(".git"),
                repo_warnings: vec![],
                repo_anomalies: vec![],
            })),
        }
    }

    fn restore_state(&self, storage: &Map<String, Value>) {
        let s = |k: &str, d: &str| storage.get(k).and_then(|v| v.as_str()).unwrap_or(d).to_string();
        let b = |k: &str, d: bool| storage.get(k).and_then(|v| v.as_bool()).unwrap_or(d);
        let l = |k: &str| -> Vec<String> {
            storage.get(k).and_then(|v| v.as_array()).map(|a| a.iter().map(crate::pyfmt::py_str).collect()).unwrap_or_default()
        };
        {
            let mut st = self.st.borrow_mut();
            st.valid_git_repo = b("repo_valid", false);
            st.git_owner = s("git_owner", "?");
            st.git_repo_name = s("git_repo_name", "?");
            st.git_remote = s("git_remote", "?");
            st.git_branch = s("git_branch", "?");
            st.current_version = if storage.contains_key("full_version_string") {
                Version::git(&s("full_version_string", "?"))
            } else {
                Version::git(&s("current_version", "?"))
            };
            st.upstream_version = Version::git(&s("upstream_version", "?"));
            st.current_commit = s("current_commit", "?");
            st.upstream_commit = s("upstream_commit", "?");
            st.upstream_url = s("upstream_url", "?");
            let def_recovery = if st.git_remote == "origin" { st.upstream_url.clone() } else { "?".into() };
            st.recovery_url = s("recovery_url", &def_recovery);
            st.branches = l("branches");
            st.head_detached = b("head_detached", false);
            st.git_messages = l("git_messages");
            st.commits_behind = storage.get("commits_behind").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            st.commits_behind_count = storage.get("cbh_count").and_then(|v| v.as_i64()).unwrap_or(0);
            st.diverged = b("diverged", false);
            st.repo_corrupt = b("corrupt", false);
            st.modified_files = l("modified_files");
            st.untracked_files = l("untracked_files");
        }
        let def_rbs = self.capture_state_for_rollback();
        {
            let mut st = self.st.borrow_mut();
            let cc = st.current_commit.clone();
            st.rollback_commit = s("rollback_commit", &cc);
            st.rollback_branch = s("rollback_branch", &def_rbs.branch);
            let rbv = storage.get("rollback_version").map(crate::pyfmt::py_str).unwrap_or_else(|| st.current_version.to_string());
            st.rollback_version = Version::git(&rbv);
            st.pinned_commit_valid = b("pinned_commit_valid", true);
        }
        if !self.detect_git_dir() {
            self.st.borrow_mut().valid_git_repo = false;
        }
        self.check_warnings();
    }

    fn get_persistent_data(&self) -> Map<String, Value> {
        let st = self.st.borrow();
        let v = json!({
            "repo_valid": st.valid_git_repo,
            "git_owner": st.git_owner,
            "git_repo_name": st.git_repo_name,
            "git_remote": st.git_remote,
            "git_branch": st.git_branch,
            "current_version": st.current_version.full_version(),
            "upstream_version": st.upstream_version.full_version(),
            "current_commit": st.current_commit,
            "upstream_commit": st.upstream_commit,
            "rollback_commit": st.rollback_commit,
            "rollback_branch": st.rollback_branch,
            "rollback_version": st.rollback_version.full_version(),
            "upstream_url": st.upstream_url,
            "recovery_url": st.recovery_url,
            "branches": st.branches,
            "head_detached": st.head_detached,
            "git_messages": st.git_messages,
            "commits_behind": st.commits_behind,
            "cbh_count": st.commits_behind_count,
            "diverged": st.diverged,
            "corrupt": st.repo_corrupt,
            "modified_files": st.modified_files,
            "untracked_files": st.untracked_files,
            "pinned_commit_valid": st.pinned_commit_valid,
        });
        v.as_object().cloned().unwrap()
    }

    async fn refresh_repo_state(&self, mut need_fetch: bool) -> Result<(), ServerError> {
        let in_progress = self.refresh_lock.try_lock().is_err();
        let _g = self.refresh_lock.lock().await;
        if in_progress {
            return Ok(());
        }
        {
            let mut st = self.st.borrow_mut();
            st.pinned_commit_valid = true;
            st.current_commit = "?".into();
            st.upstream_commit = "?".into();
            st.current_version = Version::git("?");
            st.upstream_version = Version::git("?");
            st.git_messages.clear();
        }
        let res: Result<(), ServerError> = async {
            self.check_repo_status().await;
            self.verify_repo(false)?;
            self.find_current_branch().await?;
            let remote = self.st.borrow().git_remote.clone();
            let url = self.remote(&format!("get-url {}", remote), true).await?;
            self.st.borrow_mut().upstream_url = url;
            if self.check_moved_origin().await? {
                need_fetch = true;
            }
            if self.st.borrow().git_remote == "origin" {
                let u = self.st.borrow().upstream_url.clone();
                self.st.borrow_mut().recovery_url = u;
            } else {
                let remotes = self.remote("", false).await?;
                let remote_list: Vec<String> = remotes.lines().map(String::from).collect();
                log::debug!("Git Repo {}: Detected Remotes - {}", self.alias, py_repr(&json!(remote_list)));
                if remote_list.iter().any(|r| r == "origin") {
                    let u = self.remote("get-url origin", false).await?;
                    self.st.borrow_mut().recovery_url = u;
                } else {
                    log::info!("Git Repo {}: Unable to detect recovery URL, Hard Recovery not available", self.alias);
                    self.st.borrow_mut().recovery_url = "?".into();
                }
            }
            if need_fetch {
                self.fetch().await?;
            }
            let diverged = self.check_diverged().await?;
            self.st.borrow_mut().diverged = diverged;
            let url = self.st.borrow().upstream_url.clone();
            let owner = regex::Regex::new(r"^https?://[^/]+/([^/]+)").unwrap().captures(&url).map(|c| c[1].to_string());
            let name = regex::Regex::new(r"^.*/([^\.]*).*").unwrap().captures(&url).map(|c| c[1].to_string());
            {
                let mut st = self.st.borrow_mut();
                st.git_owner = owner.unwrap_or("?".into());
                st.git_repo_name = name.unwrap_or("?".into());
            }
            let head = self.rev_parse("HEAD").await?;
            self.st.borrow_mut().current_commit = head;
            let git_desc = self.describe("--always --tags --long --dirty --abbrev=8").await?;
            let cur_ver = Version::git(git_desc.trim());
            let upstream_ver = self.get_upstream_version().await?;
            self.set_versions(cur_ver, upstream_ver).await?;
            self.st.borrow_mut().commits_behind.clear();
            if self.st.borrow().commits_behind_count > 0 {
                let cbh = self.get_commits_behind().await?;
                let tagged = self.get_tagged_commits(100).await?;
                let dbg: Vec<String> = tagged.iter().map(|(k, v)| format!("{}: {}", k, v)).collect();
                log::debug!("Git Repo {}: Tagged Commits\n{}", self.alias, dbg.join("\n"));
                let mut out = Vec::new();
                for (i, mut commit) in cbh.into_iter().enumerate() {
                    let sha = commit.get("sha").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let tag = tagged.get(&sha).cloned();
                    if i < 30 || tag.is_some() {
                        commit.insert("tag".into(), json!(tag));
                        out.push(Value::Object(commit));
                    }
                }
                self.st.borrow_mut().commits_behind = out;
            }
            self.check_warnings();
            Ok(())
        }
        .await;
        match res {
            Err(e) => {
                log::error!("Git Repo {}: Initialization failure\n{}", self.alias, e.message);
                self.check_warnings();
                Err(e)
            }
            Ok(()) => {
                self.st.borrow_mut().repo_corrupt = false;
                let reset = {
                    let st = self.st.borrow();
                    st.rollback_commit == "?" || st.rollback_branch == "?"
                };
                if reset {
                    self.set_rollback_state(None);
                }
                self.log_repo_info();
                Ok(())
            }
        }
    }

    async fn check_repo_status(&self) -> bool {
        let _g = self.git_operation_lock.lock().await;
        self.st.borrow_mut().valid_git_repo = false;
        if !self.detect_git_dir() {
            log::info!("Git Repo {}: path '{}' is not a valid git repo", self.alias, self.src_path.display());
            return false;
        }
        self.wait_for_lock_release(60).await;
        let mut attempts = 3;
        let mut resp: Option<String> = None;
        while attempts > 0 {
            self.st.borrow_mut().git_messages.clear();
            match self.run_git_cmd("status --porcelain -b", 20.0, 1, None, Some("fatal:"), true).await {
                Ok(r) => {
                    resp = Some(r);
                    break;
                }
                Err(_) => {
                    attempts -= 1;
                    resp = None;
                    if attempts > 0 && self.st.borrow().repo_corrupt && !self.repair_loose_objects(false).await {
                        return false;
                    }
                }
            }
        }
        let resp = match resp {
            None => return false,
            Some(r) => r,
        };
        let mut st = self.st.borrow_mut();
        st.modified_files.clear();
        st.untracked_files.clear();
        for line in resp.lines() {
            let line = line.trim();
            let parts: Vec<&str> = line.splitn(2, char::is_whitespace).collect();
            if parts.len() != 2 {
                continue;
            }
            let (prefix, fname) = (parts[0].trim(), parts[1].trim());
            if prefix == "M" {
                st.modified_files.push(fname.to_string());
            } else if prefix == "??" {
                let ext = Path::new(fname).extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
                if SRC_EXTS.contains(&ext.as_str()) {
                    st.untracked_files.push(fname.to_string());
                }
            }
        }
        st.valid_git_repo = true;
        true
    }

    fn detect_git_dir(&self) -> bool {
        let gfp = self.st.borrow().git_folder_path.clone();
        if gfp.is_file() {
            let data = std::fs::read_to_string(&gfp).unwrap_or_default();
            let (ident, gitdir) = match data.split_once(':') {
                Some((a, b)) => (a, b.trim()),
                None => (data.as_str(), ""),
            };
            if ident.trim() != "gitdir" || gitdir.is_empty() {
                log::warn!("not a .git file: '{}' '{}' in '{}'", ident, gitdir, data);
                return false;
            }
            let gitdir_path = crate::utils::expand_user(gitdir);
            let resolved = crate::utils::resolve_path(&gfp.parent().unwrap_or(Path::new("/")).join(gitdir_path));
            log::info!(
                "detecting git folder path '{}' leads to '{}' resolves to '{}'",
                gfp.display(),
                gitdir,
                resolved.display()
            );
            self.st.borrow_mut().git_folder_path = resolved;
        }
        let gfp = self.st.borrow().git_folder_path.clone();
        if gfp.is_dir() {
            self.st.borrow_mut().is_shallow = gfp.join("shallow").is_file();
            return true;
        }
        false
    }

    async fn find_current_branch(&self) -> Result<(), ServerError> {
        let blist = self.list_branches().await?;
        let mut current_branch = "?".to_string();
        let mut branches = Vec::new();
        for branch in blist {
            let mut branch = branch.trim().to_string();
            if branch.is_empty() {
                continue;
            }
            if branch.starts_with('*') {
                branch = branch.get(2..).unwrap_or("").trim().to_string();
                current_branch = branch.clone();
            }
            if branch.starts_with('(') {
                continue;
            }
            branches.push(branch);
        }
        self.st.borrow_mut().branches = branches;
        if current_branch.starts_with('(') {
            self.st.borrow_mut().head_detached = true;
            let mut ref_name = String::new();
            if current_branch.starts_with("(HEAD detached") {
                let last = current_branch.split_whitespace().last().unwrap_or("");
                ref_name = last[..last.len().saturating_sub(1)].to_string();
            }
            let remotes = self.remote("", false).await?;
            let mut found = false;
            for remote in remotes.lines() {
                let remote = remote.trim();
                if remote.is_empty() {
                    continue;
                }
                if ref_name.starts_with(remote) {
                    let mut st = self.st.borrow_mut();
                    st.git_branch = ref_name.get(remote.len() + 1..).unwrap_or("").to_string();
                    st.git_remote = remote.to_string();
                    found = true;
                    break;
                }
            }
            if !found {
                let st = self.st.borrow();
                let msg = if st.git_remote == "?" {
                    "Resolve by manually checking out a branch via SSH.".to_string()
                } else {
                    format!("Defaulting to previously tracked {}/{}.", st.git_remote, st.git_branch)
                };
                log::info!("Git Repo {}: {} {}", self.alias, current_branch, msg);
            }
        } else {
            {
                let mut st = self.st.borrow_mut();
                st.head_detached = false;
                st.git_branch = current_branch.clone();
            }
            let rkey = format!("branch.{}.remote", current_branch);
            let remote = self.config_get(&rkey, "", false, false).await?;
            self.st.borrow_mut().git_remote = remote.filter(|r| !r.is_empty()).unwrap_or("?".into());
        }
        Ok(())
    }

    async fn check_moved_origin(&self) -> Result<bool, ServerError> {
        let upstream_url = self.st.borrow().upstream_url.clone();
        let mut detected = upstream_url.to_lowercase().trim().to_string();
        if !detected.ends_with(".git") {
            detected.push_str(".git");
        }
        if server().is_debug_enabled() || !detected.starts_with("http") || detected == self.origin_url.to_lowercase() {
            return Ok(false);
        }
        let mut moved = false;
        let check_url = detected[..detected.len() - 4].to_string();
        log::info!("Git repo {}: Performing moved origin check - {}", self.alias, check_url);
        let resp = self.cmd_helper.http().get(&check_url, &[], false).await;
        if !resp.has_error() {
            let mut final_url = resp.final_url.to_lowercase();
            if !final_url.ends_with(".git") {
                final_url.push_str(".git");
            }
            log::debug!("Git repo {}: Resolved url - {}", self.alias, final_url);
            if final_url == self.origin_url.to_lowercase() {
                log::info!("Git Repo {}: Moved Repo Detected, Moving from {} to {}", self.alias, upstream_url, self.origin_url);
                moved = true;
                let remote = self.st.borrow().git_remote.clone();
                self.remote(&format!("set-url {} {}", remote, self.origin_url), true).await?;
                self.st.borrow_mut().upstream_url = self.origin_url.clone();
                if let Some(mo) = &self.moved_origin_url {
                    let mut mo = mo.to_lowercase().trim().to_string();
                    if !mo.ends_with(".git") {
                        mo.push_str(".git");
                    }
                    if mo != detected {
                        server().add_warning(
                            &format!(
                                "Git Repo {}: Origin URL does not not match configured 'moved_origin'option. Expected: {}",
                                self.alias, detected
                            ),
                            None,
                            true,
                        );
                    }
                }
            }
        } else {
            log::debug!("Move Request Failed: {}", resp.error_string());
        }
        Ok(moved)
    }

    async fn get_upstream_version(&self) -> Result<Version, ServerError> {
        self.st.borrow_mut().commits_behind_count = 0;
        let upstream_ver_str;
        let (remote, branch, current_commit) = {
            let st = self.st.borrow();
            (st.git_remote.clone(), st.git_branch.clone(), st.current_commit.clone())
        };
        if let Some(pinned) = &self.pinned_commit {
            self.st.borrow_mut().upstream_commit = current_commit.clone();
            if !current_commit.to_lowercase().starts_with(pinned.as_str()) {
                if !self.check_commit_exists(pinned).await? {
                    self.st.borrow_mut().pinned_commit_valid = false;
                } else if self.is_ancestor(&current_commit, pinned, 3).await? {
                    self.st.borrow_mut().upstream_commit = pinned.clone();
                }
            }
            let uc = self.st.borrow().upstream_commit.clone();
            upstream_ver_str = self.describe(&format!("{} --always --tags --long --abbrev=8", uc)).await?;
        } else if self.channel == Channel::Dev {
            let uc = self.rev_parse(&format!("{}/{}", remote, branch)).await?;
            self.st.borrow_mut().upstream_commit = uc;
            upstream_ver_str = self.describe(&format!("{}/{} --always --tags --long --abbrev=8", remote, branch)).await?;
        } else {
            let tagged = self.get_tagged_commits(100).await?;
            let mut upstream_commit = "?".to_string();
            let mut ver_str = "?".to_string();
            for (sha, tag) in &tagged {
                let ver = Version::git(tag);
                if !ver.is_valid_version() {
                    continue;
                }
                if (self.channel == Channel::Stable && ver.is_final_release())
                    || (self.channel == Channel::Beta && !ver.is_alpha_release())
                {
                    upstream_commit = sha.clone();
                    ver_str = tag.clone();
                    break;
                }
            }
            self.st.borrow_mut().upstream_commit = upstream_commit;
            upstream_ver_str = ver_str;
        }
        let uc = self.st.borrow().upstream_commit.clone();
        if uc != "?" {
            let cnt = self.rev_list(&format!("HEAD..{} --count", uc)).await?;
            let n = cnt.trim().parse::<i64>().map_err(|_| {
                ServerError::exc(format!("invalid literal for int() with base 10: '{}'", cnt))
            })?;
            self.st.borrow_mut().commits_behind_count = n;
        }
        Ok(Version::git(&upstream_ver_str))
    }

    async fn set_versions(&self, mut current: Version, mut upstream: Version) -> Result<(), ServerError> {
        if !current.is_valid_version() {
            let mut log_msg = format!("Git repo {}: Failed to detect current version, got '{}'. ", self.alias, current);
            let tag = upstream.infer_last_tag();
            let count = self.rev_list("HEAD --count").await?;
            let cc = self.st.borrow().current_commit.clone();
            let sha_part = if current.is_fallback() {
                format!("-g{}", current)
            } else if cc != "?" && !cc.is_empty() {
                format!("-g{}", &cc[..cc.len().min(8)])
            } else {
                String::new()
            };
            current = Version::git(&format!("{}-{}{}-inferred", tag, count, sha_part));
            log_msg.push_str(&format!("Falling back to inferred version: {}", current));
            log::info!("{}", log_msg);
        }
        if self.channel == Channel::Dev {
            if !upstream.is_valid_version() {
                let mut log_msg = format!("Git repo {}: Failed to detect upstream version, got '{}'. ", self.alias, upstream);
                let tag = current.tag().to_string();
                let count = if current.inferred() {
                    let uc = self.st.borrow().upstream_commit.clone();
                    self.rev_list(&format!("{} --count", uc)).await?
                } else {
                    log_msg.push_str("\nRemote has diverged, approximating dev count. ");
                    (self.st.borrow().commits_behind_count + current.dev_count()).to_string()
                };
                upstream = Version::git(&format!("{}-{}-inferred", tag, count));
                log_msg.push_str(&format!("Falling back to inferred version: {}", upstream));
                log::info!("{}", log_msg);
            }
        } else if !upstream.is_valid_version() {
            let cc = self.st.borrow().current_commit.clone();
            self.st.borrow_mut().upstream_commit = cc;
            upstream = current.clone();
        } else if upstream.le(&current)? {
            let cc = self.st.borrow().current_commit.clone();
            self.st.borrow_mut().upstream_commit = cc;
        }
        let mut st = self.st.borrow_mut();
        st.current_version = current;
        st.upstream_version = upstream;
        Ok(())
    }

    async fn wait_for_refresh(&self) {
        if self.refresh_lock.try_lock().is_err() {
            let _g = self.refresh_lock.lock().await;
        }
    }

    async fn is_ancestor(&self, ancestor: &str, descendent: &str, attempts: usize) -> Result<bool, ServerError> {
        self.verify_repo(false)?;
        let cmd = format!("merge-base --is-ancestor {} {}", ancestor, descendent);
        let _g = self.git_operation_lock.lock().await;
        for _ in 0..attempts {
            match self.run_git_cmd(&cmd, 20.0, 1, None, Some("error: "), true).await {
                Err(e) => {
                    if e.return_code == Some(1) {
                        return Ok(false);
                    }
                    if self.st.borrow().repo_corrupt {
                        return Err(e.into());
                    }
                }
                Ok(_) => break,
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        Ok(true)
    }

    async fn check_diverged(&self) -> Result<bool, ServerError> {
        self.verify_repo(true)?;
        let (detached, remote, branch) = {
            let st = self.st.borrow();
            (st.head_detached, st.git_remote.clone(), st.git_branch.clone())
        };
        if detached {
            return Ok(false);
        }
        Ok(!self.is_ancestor("HEAD", &format!("{}/{}", remote, branch), 3).await?)
    }

    fn log_repo_info(&self) {
        let mut warnings = self.generate_warn_msg();
        if !warnings.is_empty() {
            warnings = format!("\nRepo Warnings:\n{}", warnings);
        }
        let st = self.st.borrow();
        log::info!(
            "Git Repo {} Detected:\nOwner: {}\nRepository Name: {}\nPath: {}\nRemote: {}\nBranch: {}\nRemote URL: {}\nRecovery URL: {}\nCurrent Commit SHA: {}\nUpstream Commit SHA: {}\nCurrent Version: {}\nUpstream Version: {}\nRollback Commit: {}\nRollback Branch: {}\nRollback Version: {}\nIs Dirty: {}\nIs Detached: {}\nIs Shallow: {}\nCommits Behind Count: {}\nDiverged: {}\nPinned Commit: {}{}",
            self.alias,
            st.git_owner,
            st.git_repo_name,
            self.src_path.display(),
            st.git_remote,
            st.git_branch,
            st.upstream_url,
            st.recovery_url,
            st.current_commit,
            st.upstream_commit,
            st.current_version,
            st.upstream_version,
            st.rollback_commit,
            st.rollback_branch,
            st.rollback_version,
            crate::pyfmt::py_bool(st.current_version.dirty()),
            crate::pyfmt::py_bool(st.head_detached),
            crate::pyfmt::py_bool(st.is_shallow),
            st.commits_behind_count,
            crate::pyfmt::py_bool(st.diverged),
            self.pinned_commit.clone().unwrap_or("None".into()),
            warnings
        );
    }

    fn check_warnings(&self) {
        let debug = server().is_debug_enabled();
        {
            let mut st = self.st.borrow_mut();
            let mut warnings = Vec::new();
            let mut anomalies = Vec::new();
            if let Some(p) = &self.pinned_commit {
                if !st.pinned_commit_valid {
                    anomalies.push(format!("Pinned Commit {} does not exist", p));
                }
            }
            if !st.valid_git_repo {
                warnings.push("No git repo detected at configured path".to_string());
            }
            if st.repo_corrupt {
                warnings.push("Repo is corrupt".to_string());
            }
            if st.git_branch == "?" {
                warnings.push("Failed to detect git branch".to_string());
            } else if st.git_remote == "?" {
                warnings.push(format!("Failed to detect tracking remote for branch {}", st.git_branch));
            } else if st.git_branch != self.primary_branch || st.git_remote != "origin" {
                anomalies.push(format!(
                    "Repo not on official remote/branch, expected: origin/{}, detected: {}/{}",
                    self.primary_branch, st.git_remote, st.git_branch
                ));
            }
            if st.upstream_url == "?" {
                warnings.push("Failed to detect repo url".to_string());
            } else {
                let mut u = st.upstream_url.to_lowercase();
                if !u.ends_with(".git") {
                    u.push_str(".git");
                }
                if u != self.origin_url.to_lowercase() {
                    anomalies.push(format!("Unofficial remote url: {}", st.upstream_url));
                }
            }
            if !st.untracked_files.is_empty() {
                anomalies.push(format!("Repo has untracked source files: {}", str_list(&st.untracked_files)));
            }
            if st.diverged {
                anomalies.push("Repo has diverged from remote".to_string());
            }
            if st.head_detached {
                let msg = "Detached HEAD detected".to_string();
                if debug {
                    anomalies.push(msg);
                } else {
                    warnings.push(msg);
                }
            }
            if st.current_version.dirty() {
                warnings.push(format!(
                    "Repo is dirty.  Detected the following modified files: {}",
                    str_list(&st.modified_files)
                ));
            }
            st.repo_warnings = warnings;
            st.repo_anomalies = anomalies;
        }
        self.generate_warn_msg();
    }

    fn generate_warn_msg(&self) -> String {
        let st = self.st.borrow();
        let mut ro_msg = format!("Git Repo {}: No warnings detected", self.alias);
        let mut warn_msg = String::new();
        if !st.repo_warnings.is_empty() || !st.repo_anomalies.is_empty() {
            ro_msg = format!("Git Repo {}: Warnings detected:\n", self.alias);
            warn_msg = st.repo_warnings.iter().chain(st.repo_anomalies.iter()).map(|w| format!("  {}", w)).collect::<Vec<_>>().join("\n");
            ro_msg.push_str(&warn_msg);
        }
        server().add_log_rollover_item(&format!("umgr_{}_warn", self.alias), &ro_msg, false);
        warn_msg
    }

    fn verify_repo(&self, check_remote: bool) -> Result<(), ServerError> {
        let st = self.st.borrow();
        if !st.valid_git_repo {
            return Err(ServerError::bad(format!("Git Repo {}: repo not initialized", self.alias)));
        }
        if check_remote && st.git_remote == "?" {
            return Err(ServerError::bad(format!("Git Repo {}: No valid git remote detected", self.alias)));
        }
        Ok(())
    }

    async fn reset(&self, r: Option<&str>) -> Result<(), ServerError> {
        let _g = self.git_operation_lock.lock().await;
        let r = match r {
            Some(r) => r.to_string(),
            None => {
                let st = self.st.borrow();
                if self.channel != Channel::Dev || self.pinned_commit.is_some() {
                    st.upstream_commit.clone()
                } else {
                    if st.git_remote == "?" || st.git_branch == "?" {
                        return Err(ServerError::bad("Cannot reset, unknown remote/branch"));
                    }
                    format!("{}/{}", st.git_remote, st.git_branch)
                }
            }
        };
        ok_str(self.run_git_cmd(&format!("reset --hard {}", r), 20.0, 2, None, None, true).await)?;
        self.st.borrow_mut().repo_corrupt = false;
        Ok(())
    }

    async fn fetch(&self) -> Result<(), ServerError> {
        self.verify_repo(true)?;
        let _g = self.git_operation_lock.lock().await;
        let remote = self.st.borrow().git_remote.clone();
        self.run_git_cmd_async(&format!("fetch {} --prune --progress", remote), 5, true, true).await
    }

    async fn pull(&self) -> Result<(), ServerError> {
        self.verify_repo(false)?;
        if self.st.borrow().head_detached {
            return Err(ServerError::bad(format!("Git Repo {}: Cannot perform pull on a detached HEAD", self.alias)));
        }
        let mut cmd = "pull --progress".to_string();
        if server().is_debug_enabled() {
            cmd.push_str(" --rebase");
        }
        if self.channel != Channel::Dev || self.pinned_commit.is_some() {
            let st = self.st.borrow();
            cmd = format!("{} {} {}", cmd, st.git_remote, st.upstream_commit);
        }
        let _g = self.git_operation_lock.lock().await;
        self.run_git_cmd_async(&cmd, 5, true, true).await
    }

    async fn list_branches(&self) -> Result<Vec<String>, ServerError> {
        self.verify_repo(false)?;
        let _g = self.git_operation_lock.lock().await;
        let resp = ok_str(self.run_git_cmd("branch --list --no-color", 20.0, 5, None, None, true).await)?;
        Ok(resp.trim().split('\n').map(String::from).collect())
    }

    async fn check_commit_exists(&self, commit: &str) -> Result<bool, ServerError> {
        self.verify_repo(false)?;
        let _g = self.git_operation_lock.lock().await;
        Ok(self.run_git_cmd(&format!("cat-file -e {}^{{commit}}", commit), 20.0, 1, None, None, true).await.is_ok())
    }

    async fn remote(&self, command: &str, validate: bool) -> Result<String, ServerError> {
        self.verify_repo(validate)?;
        let _g = self.git_operation_lock.lock().await;
        Ok(ok_str(self.run_git_cmd(&format!("remote {}", command), 20.0, 5, None, None, true).await)?.trim().to_string())
    }

    async fn simple(&self, cmd: &str, args: &str) -> Result<String, ServerError> {
        self.verify_repo(false)?;
        let _g = self.git_operation_lock.lock().await;
        let full = format!("{} {}", cmd, args);
        Ok(ok_str(self.run_git_cmd(full.trim(), 20.0, 5, None, None, true).await)?.trim().to_string())
    }

    async fn describe(&self, args: &str) -> Result<String, ServerError> {
        self.simple("describe", args).await
    }

    async fn rev_parse(&self, args: &str) -> Result<String, ServerError> {
        self.simple("rev-parse", args).await
    }

    async fn rev_list(&self, args: &str) -> Result<String, ServerError> {
        self.simple("rev-list", args).await
    }

    async fn config_get(&self, key: &str, pattern: &str, get_all: bool, local_only: bool) -> Result<Option<String>, ServerError> {
        let local = if local_only { "--local " } else { "" };
        let cmd = if get_all { format!("{}--get-all", local) } else { format!("{}--get", local) };
        let args = if pattern.is_empty() { format!("{} {}", cmd, key) } else { format!("{} {} '{}'", cmd, key, pattern) };
        match self.config_cmd(&args).await {
            Ok(r) => Ok(Some(r)),
            Err(e) if e.return_code == Some(1) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn config_cmd(&self, args: &str) -> Result<String, ShellCommandError> {
        if let Err(e) = self.verify_repo(false) {
            return Err(ShellCommandError { message: e.message, return_code: None, stdout: vec![], stderr: vec![], status_code: 400 });
        }
        let verbose = server().is_verbose_enabled();
        let _g = self.git_operation_lock.lock().await;
        for attempt in 0..3 {
            match self.run_git_cmd(&format!("config {}", args), 20.0, 1, None, None, verbose).await {
                Ok(r) => return Ok(r),
                Err(e) => {
                    let rc = e.return_code.unwrap_or(10);
                    if (1..=6).contains(&rc) || attempt == 2 {
                        return Err(e);
                    }
                }
            }
        }
        Err(ShellCommandError { message: "Failed to run git-config".into(), return_code: None, stdout: vec![], stderr: vec![], status_code: 400 })
    }

    async fn checkout(&self, branch: Option<&str>) -> Result<(), ServerError> {
        self.verify_repo(false)?;
        let mut reset_commit = None;
        {
            let _g = self.git_operation_lock.lock().await;
            let branch = match branch {
                Some(b) => b.to_string(),
                None => {
                    let st = self.st.borrow();
                    if self.channel != Channel::Dev || self.pinned_commit.is_some() {
                        reset_commit = Some(st.upstream_commit.clone());
                    }
                    format!("{}/{}", st.git_remote, st.git_branch)
                }
            };
            ok_str(self.run_git_cmd(&format!("checkout -q {}", branch), 20.0, 5, None, None, true).await)?;
        }
        if let Some(rc) = reset_commit {
            self.reset(Some(&rc)).await?;
        }
        Ok(())
    }

    fn is_submodule_or_worktree(&self) -> bool {
        let gfp = self.st.borrow().git_folder_path.clone();
        self.src_path.join(".git").is_file()
            && gfp.parent().and_then(|p| p.file_name()).map(|n| n == "modules" || n == "worktrees").unwrap_or(false)
    }

    fn get_repo_type(&self) -> &'static str {
        let gfp = self.st.borrow().git_folder_path.clone();
        match gfp.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).as_deref() {
            Some("modules") => "submodule",
            Some("worktrees") => "worktree",
            _ => "repo",
        }
    }

    async fn clone_repo(&self) -> Result<(), ServerError> {
        if self.is_submodule_or_worktree() {
            return Err(ServerError::bad(format!(
                "Cannot clone git repo {}, it is a {} of another git repo.",
                self.alias,
                self.get_repo_type()
            )));
        }
        {
            let _g = self.git_operation_lock.lock().await;
            let recovery_url = self.st.borrow().recovery_url.clone();
            if recovery_url == "?" {
                return Err(ServerError::bad("Recovery url has not been detected, clone aborted"));
            }
            self.cmd_helper.notify_update_response(&format!("Git Repo {}: Starting Clone Recovery...", self.alias), false);
            if self.backup_path.exists() {
                let _ = std::fs::remove_dir_all(&self.backup_path);
            }
            self.check_lock_file_exists(true);
            let cmd = format!(
                "clone --branch {} --filter=blob:none {} {}",
                self.primary_branch,
                recovery_url,
                self.backup_path.display()
            );
            if self.run_git_cmd_async(&cmd, 1, false, false).await.is_err() {
                self.cmd_helper.notify_update_response(&format!("Git Repo {}: Git Clone Failed", self.alias), false);
                return Err(ServerError::bad("Git Clone Error"));
            }
            if self.src_path.exists() {
                let _ = std::fs::remove_dir_all(&self.src_path);
            }
            std::fs::rename(&self.backup_path, &self.src_path)?;
            {
                let mut st = self.st.borrow_mut();
                st.repo_corrupt = false;
                st.valid_git_repo = true;
            }
            self.cmd_helper.notify_update_response(&format!("Git Repo {}: Git Clone Complete", self.alias), false);
        }
        let reset_commit = self.get_recovery_ref(Some("HEAD")).await?;
        if reset_commit != "HEAD" {
            let cc = self.st.borrow().current_commit.clone();
            self.cmd_helper
                .notify_update_response(&format!("Git Repo {}: Moving HEAD to previous commit {}", self.alias, cc), false);
            self.reset(Some(&reset_commit)).await?;
        }
        Ok(())
    }

    async fn rollback(&self) -> Result<bool, ServerError> {
        let (rc, rb, branch, cc) = {
            let st = self.st.borrow();
            (st.rollback_commit.clone(), st.rollback_branch.clone(), st.git_branch.clone(), st.current_commit.clone())
        };
        if rc == "?" || rb == "?" {
            return Err(ServerError::bad("Incomplete rollback data stored, cannot rollback"));
        }
        if rb != branch {
            self.checkout(Some(&rb)).await?;
        } else if rc == cc {
            return Ok(false);
        }
        self.reset(Some(&rc)).await?;
        Ok(true)
    }

    fn capture_state_for_rollback(&self) -> RollbackState {
        let st = self.st.borrow();
        let mut branch = st.git_branch.clone();
        if st.head_detached {
            let valid = st.git_remote != "?" && st.git_branch != "?";
            branch = if valid { format!("{}/{}", st.git_remote, st.git_branch) } else { "?".into() };
        }
        RollbackState { commit: st.current_commit.clone(), branch, version: st.current_version.clone() }
    }

    fn set_rollback_state(&self, rb: Option<RollbackState>) {
        let rb = rb.unwrap_or_else(|| self.capture_state_for_rollback());
        let mut st = self.st.borrow_mut();
        st.rollback_commit = rb.commit;
        st.rollback_branch = rb.branch;
        st.rollback_version = rb.version;
    }

    async fn get_commits_behind(&self) -> Result<Vec<Map<String, Value>>, ServerError> {
        self.verify_repo(false)?;
        if self.is_current() {
            return Ok(vec![]);
        }
        let _g = self.git_operation_lock.lock().await;
        let (r, cc) = {
            let st = self.st.borrow();
            let r = if self.channel != Channel::Dev || self.pinned_commit.is_some() {
                st.upstream_commit.clone()
            } else {
                format!("{}/{}", st.git_remote, st.git_branch)
            };
            (r, st.current_commit.clone())
        };
        let resp = ok_str(
            self.run_git_cmd(&format!("log {}..{} --format={} --max-count={}", cc, r, GIT_LOG_FMT, GIT_MAX_LOG_CNT), 20.0, 5, None, None, true)
                .await,
        )?;
        let mut out = Vec::new();
        for entry in resp.split('\x1E') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let mut commit = Map::new();
            for li in entry.split('\x1D').map(|l| l.trim()).filter(|l| !l.is_empty()) {
                let mut kv = li.splitn(2, ':');
                let k = kv.next().unwrap_or("");
                // python's dict() raises on a missing value, a value is always present for this format
                commit.insert(k.to_string(), json!(kv.next().unwrap_or("")));
            }
            out.push(commit);
        }
        Ok(out)
    }

    async fn get_tagged_commits(&self, count: i64) -> Result<IndexMap<String, String>, ServerError> {
        self.verify_repo(true)?;
        let tip = {
            let st = self.st.borrow();
            format!("{}/{}", st.git_remote, st.git_branch)
        };
        let cnt_arg = if count > 0 { format!("--count={} ", count) } else { String::new() };
        let _g = self.git_operation_lock.lock().await;
        let resp = ok_str(
            self.run_git_cmd(
                &format!("for-each-ref {}--sort='-creatordate' --contains=HEAD --merged={} --format={} 'refs/tags'", cnt_arg, tip, GIT_REF_FMT),
                20.0,
                5,
                None,
                None,
                true,
            )
            .await,
        )?;
        let mut tagged = IndexMap::new();
        for line in resp.split('\n') {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() != 3 || parts[0] != "commit" {
                continue;
            }
            let tag = parts[2].rsplit('/').next().unwrap_or("").to_string();
            tagged.insert(parts[1].to_string(), tag);
        }
        Ok(tagged)
    }

    fn get_repo_status(&self, rpt_anomalies: bool) -> Map<String, Value> {
        let st = self.st.borrow();
        let no_untrk = st.untracked_files.is_empty();
        let anomalies: Vec<String> = if rpt_anomalies { st.repo_anomalies.clone() } else { vec![] };
        json!({
            "detected_type": "git_repo",
            "repo_detected": st.valid_git_repo,
            "remote_alias": st.git_remote,
            "branch": st.git_branch,
            "owner": st.git_owner,
            "repo_name": st.git_repo_name,
            "remote_url": st.upstream_url,
            "recovery_url": st.recovery_url,
            "version": st.current_version.short_version(),
            "remote_version": st.upstream_version.short_version(),
            "rollback_version": st.rollback_version.short_version(),
            "current_hash": st.current_commit,
            "remote_hash": st.upstream_commit,
            "is_dirty": st.current_version.dirty(),
            "detached": st.head_detached,
            "commits_behind": st.commits_behind,
            "commits_behind_count": st.commits_behind_count,
            "git_messages": st.git_messages,
            "full_version_string": st.current_version.full_version(),
            "pristine": no_untrk && !st.current_version.dirty(),
            "corrupt": st.repo_corrupt,
            "warnings": st.repo_warnings,
            "anomalies": anomalies,
        })
        .as_object()
        .cloned()
        .unwrap()
    }

    fn is_detached(&self) -> bool {
        self.st.borrow().head_detached
    }

    fn is_dirty(&self) -> bool {
        self.st.borrow().current_version.dirty()
    }

    fn is_current(&self) -> bool {
        let st = self.st.borrow();
        st.current_commit == st.upstream_commit
    }

    fn is_valid(&self) -> bool {
        let st = self.st.borrow();
        let detached_err = if server().is_debug_enabled() { false } else { st.head_detached };
        ![&st.git_branch, &st.git_remote, &st.upstream_commit].iter().any(|s| s.as_str() == "?")
            && !(!st.valid_git_repo || st.repo_corrupt)
            && !(st.diverged || st.current_version.dirty() || detached_err)
    }

    async fn get_recovery_ref(&self, upstream_ref: Option<&str>) -> Result<String, ServerError> {
        let upstream_ref = match upstream_ref {
            Some(r) => r.to_string(),
            None => {
                let remote = self.config_get(&format!("branch.{}.remote", self.primary_branch), "", false, false).await?;
                match remote {
                    None => {
                        return Err(ServerError::bad(format!(
                            "Failed to find remote for primary branch '{}'",
                            self.primary_branch
                        )))
                    }
                    Some(r) => format!("{}/{}", r, self.primary_branch),
                }
            }
        };
        let mut reset_commits = Vec::new();
        if let Some(p) = &self.pinned_commit {
            reset_commits.push(p.clone());
        }
        let cc = self.st.borrow().current_commit.clone();
        if cc != "?" {
            reset_commits.push(cc);
        }
        for commit in reset_commits {
            if self.is_ancestor(&commit, &upstream_ref, 1).await.unwrap_or(false) {
                return Ok(commit);
            }
        }
        Ok(upstream_ref)
    }

    fn check_lock_file_exists(&self, remove: bool) -> bool {
        let lock_path = self.st.borrow().git_folder_path.join("index.lock");
        if lock_path.is_file() {
            if remove {
                log::info!("Git Repo {}: Git lock file found after git process exited, removing", self.alias);
                let _ = std::fs::remove_file(&lock_path);
            }
            return true;
        }
        false
    }

    async fn wait_for_lock_release(&self, mut timeout: i64) {
        while timeout > 0 {
            if self.check_lock_file_exists(false) {
                if timeout % 10 == 0 {
                    log::info!("Git Repo {}: Git lock file exists, {} seconds remaining before removal.", self.alias, timeout);
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                timeout -= 1;
            } else {
                return;
            }
        }
        self.check_lock_file_exists(true);
    }

    async fn repair_loose_objects(&self, notify: bool) -> bool {
        if notify {
            self.cmd_helper.notify_update_response("Attempting to repair loose objects...", false);
        }
        let res: Result<(), ServerError> = async {
            self.cmd_helper
                .shell()
                .exec_cmd(
                    "find .git/objects/ -type f -empty | xargs rm",
                    ExecOpts { timeout: 10.0, attempts: 1, cwd: Some(self.src_path.display().to_string()), ..Default::default() },
                )
                .await?;
            Box::pin(self.run_git_cmd_async("fetch --all -p", 1, true, false)).await?;
            ok_str(self.run_git_cmd("fsck --full", 300.0, 1, None, None, true).await)?;
            Ok(())
        }
        .await;
        if res.is_err() {
            let msg = "Attempt to repair loose objects failed, hard recovery is required";
            log::error!("{}", msg);
            if notify {
                self.cmd_helper.notify_update_response(msg, false);
            }
            return false;
        }
        if notify {
            self.cmd_helper.notify_update_response("Loose objects repaired", false);
        }
        self.st.borrow_mut().repo_corrupt = false;
        true
    }

    async fn run_git_cmd_async(&self, cmd: &str, mut attempts: usize, need_git_path: bool, mut fix_loose: bool) -> Result<(), ServerError> {
        self.wait_for_lock_release(60).await;
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.insert("GIT_HTTP_LOW_SPEED_LIMIT".into(), "1000".into());
        // upstream sets this key with a trailing space
        env.insert("GIT_HTTP_LOW_SPEED_TIME ".into(), "20".into());
        let git_cmd = if need_git_path { format!("git -C {} {}", self.src_path.display(), cmd) } else { format!("git {}", cmd) };
        let st = Rc::downgrade(&self.st);
        let recd = self.fetch_input_recd.clone();
        let ch = Rc::downgrade(&self.cmd_helper);
        let alias = self.alias.clone();
        let cb: OutputCallback = Rc::new(move |output: Vec<u8>| {
            recd.set(true);
            let out = String::from_utf8_lossy(&output).trim().to_string();
            if out.is_empty() {
                return;
            }
            if let Some(st) = st.upgrade() {
                let mut st = st.borrow_mut();
                if out.starts_with("fatal: ") && out.contains("corrupt") {
                    st.repo_corrupt = true;
                }
                st.git_messages.push(out.clone());
            }
            if let Some(ch) = ch.upgrade() {
                ch.notify_update_response(&out, false);
            }
            log::debug!("Git Repo {}: {}", alias, out);
        });
        let shell = self.cmd_helper.shell();
        let scmd = shell.build_shell_command(&git_cmd, Some(cb), None, Some(env), false, None);
        while attempts > 0 {
            self.st.borrow_mut().git_messages.clear();
            self.fetch_input_recd.set(false);
            // Terminate the process only when no output is received for GIT_ASYNC_TIMEOUT
            let watch_cmd = scmd.clone();
            let recd = self.fetch_input_recd.clone();
            let alias = self.alias.clone();
            let cname = cmd.to_string();
            let watchdog = spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs_f64(GIT_ASYNC_TIMEOUT)).await;
                    if watch_cmd.get_return_code().is_some() {
                        log::debug!("Git Repo {}: {} returned", alias, cname);
                        return;
                    }
                    if recd.get() {
                        log::debug!("Git Repo {}: {} active, rescheduling", alias, cname);
                        recd.set(false);
                    } else {
                        log::debug!("Git Repo {}: {} timed out", alias, cname);
                        watch_cmd.cancel(2).await;
                        return;
                    }
                }
            });
            scmd.run(&shell, 0.0, false, true, 1, None, None).await;
            watchdog.abort();
            if scmd.get_return_code() == Some(0) {
                self.st.borrow_mut().git_messages.clear();
                return Ok(());
            } else if self.st.borrow().repo_corrupt && fix_loose {
                if Box::pin(self.repair_loose_objects(true)).await {
                    fix_loose = false;
                    attempts = 2;
                } else {
                    return Err(ServerError::bad("Unable to repair loose objects, use hard recovery"));
                }
            }
            attempts -= 1;
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            self.check_lock_file_exists(true);
        }
        Err(ServerError::bad(format!("Git Command '{}' failed", cmd)))
    }

    async fn run_git_cmd(
        &self,
        git_args: &str,
        timeout: f64,
        attempts: usize,
        env: Option<HashMap<String, String>>,
        corrupt_hdr: Option<&str>,
        log_complete: bool,
    ) -> Result<String, ShellCommandError> {
        let res = self
            .cmd_helper
            .shell()
            .exec_cmd(
                &format!("git -C {} {}", self.src_path.display(), git_args),
                ExecOpts { timeout, attempts, env, sig_idx: 2, log_complete, ..Default::default() },
            )
            .await;
        if let Err(e) = &res {
            let stdout = String::from_utf8_lossy(&e.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&e.stderr).trim().to_string();
            let mut msg_lines: Vec<String> = Vec::new();
            let mut st = self.st.borrow_mut();
            if !stdout.is_empty() {
                msg_lines.extend(stdout.split('\n').map(String::from));
                st.git_messages.push(stdout.clone());
            }
            if !stderr.is_empty() {
                // upstream appends stdout lines here as well
                msg_lines.extend(stdout.split('\n').map(String::from));
                st.git_messages.push(stderr);
            }
            if let Some(hdr) = corrupt_hdr {
                for line in msg_lines {
                    let line = line.trim().to_lowercase();
                    if line.starts_with(hdr) && line.contains("corrupt") {
                        st.repo_corrupt = true;
                        break;
                    }
                }
            }
        }
        res
    }
}

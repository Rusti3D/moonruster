// Git Deployment implementation (port of update_manager/git_deploy.py)

use super::common::*;
use super::*;
use crate::components::shell_command::{ExecOpts, ShellCommandError};

mod repo;

pub use repo::*;

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

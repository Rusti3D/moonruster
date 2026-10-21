// Python Package Update Deployment (port of update_manager/python_deploy.py
// and the distribution helpers in utils/source_info.py)

use super::common::*;
use super::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PackageSource {
    Pip,
    Github,
    Unknown,
}

impl PackageSource {
    fn name(&self) -> &'static str {
        match self {
            PackageSource::Pip => "PIP",
            PackageSource::Github => "GITHUB",
            PackageSource::Unknown => "UNKNOWN",
        }
    }
}

pub fn normalize_project_name(name: &str) -> String {
    regex::Regex::new(r"[-_.]+").unwrap().replace_all(name, "-").to_lowercase().replace('-', "_")
}

/// Parsed package metadata (email header format)
struct Metadata {
    headers: Vec<(String, String)>,
}

impl Metadata {
    fn parse(text: &str) -> Metadata {
        let mut headers: Vec<(String, String)> = Vec::new();
        for line in text.lines() {
            if line.is_empty() {
                break;
            }
            if line.starts_with(' ') || line.starts_with('\t') {
                if let Some(last) = headers.last_mut() {
                    last.1.push('\n');
                    last.1.push_str(line.trim());
                }
                continue;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        Metadata { headers }
    }
    fn get(&self, key: &str) -> Option<String> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.clone())
    }
    fn get_all(&self, key: &str) -> Vec<String> {
        self.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.clone()).collect()
    }
}

struct PackageInfo {
    metadata: Metadata,
    release_info: Option<Value>,
    direct_url_data: Option<Value>,
}

fn load_distribution_info(venv_path: &Path, project_name: &str) -> Result<PackageInfo, ServerError> {
    let norm = normalize_project_name(project_name);
    let mut site_dirs = crate::utils::path_glob(&venv_path.join("lib"), "python*");
    site_dirs.sort();
    for py_dir in site_dirs {
        let site = py_dir.join("site-packages");
        if !site.is_dir() {
            continue;
        }
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&site).map(|r| r.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        entries.sort();
        for dist in entries {
            let fname = dist.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let (stem, meta_file) = if let Some(s) = fname.strip_suffix(".dist-info") {
                (s.to_string(), "METADATA")
            } else if let Some(s) = fname.strip_suffix(".egg-info") {
                (s.to_string(), "PKG-INFO")
            } else {
                continue;
            };
            let dist_name = stem.split('-').next().unwrap_or("");
            if normalize_project_name(dist_name) != norm {
                continue;
            }
            let text = match std::fs::read_to_string(dist.join(meta_file)) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let metadata = Metadata::parse(&text);
            if normalize_project_name(&metadata.get("Name").unwrap_or_default()) != norm {
                continue;
            }
            let mut release_info = None;
            if let Ok(record) = std::fs::read_to_string(dist.join("RECORD")) {
                for line in record.lines() {
                    let rel = line.split(',').next().unwrap_or("");
                    let p = Path::new(rel);
                    let first = p.components().next().map(|c| c.as_os_str().to_string_lossy().to_string()).unwrap_or_default();
                    if first == ".." || first == "/" || rel.starts_with('/') {
                        continue;
                    }
                    if p.file_name().map(|n| n == "release_info").unwrap_or(false) {
                        log::info!("Package {}: Detected release_info json file", first);
                        match std::fs::read_to_string(site.join(rel)).map_err(|e| e.to_string()).and_then(|t| {
                            serde_json::from_str::<Value>(&t).map_err(|e| e.to_string())
                        }) {
                            Ok(v) => {
                                release_info = Some(v);
                                break;
                            }
                            Err(e) => log::error!("Failed to load release_info from {}\n{}", rel, e),
                        }
                    }
                }
            }
            let direct_url_data =
                std::fs::read_to_string(dist.join("direct_url.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
            return Ok(PackageInfo { metadata, release_info, direct_url_data });
        }
    }
    Err(ServerError::bad(format!("Failed to find distribution info for project {}", project_name)))
}

struct PyState {
    source: PackageSource,
    repo_url: String,
    repo_owner: String,
    repo_name: String,
    current_version: Version,
    git_version: Version,
    current_sha: String,
    upstream_version: Version,
    upstream_sha: String,
    rollback_version: Version,
    rollback_ref: String,
    warnings: Vec<String>,
    changelog: String,
    system_deps: Vec<String>,
}

pub struct PythonDeploy {
    app: AppDeploy,
    primary_branch: Option<String>,
    project_name: String,
    extras: Option<String>,
    st: RefCell<PyState>,
}

impl PythonDeploy {
    pub fn new(config: &ConfigHelper, cmd_helper: &Rc<CommandHelper>) -> Result<Self, ConfigError> {
        let mut app = AppDeploy::new(config, cmd_helper, "Python Package")?;
        app.configure_virtualenv(config)?;
        let venv = match &app.virtualenv {
            Some(v) => v.clone(),
            None => {
                return Err(ConfigHelper::error(format!(
                    "[{}]: Option 'virtualenv' must specify a valid the path to a Python virtualenv",
                    config.get_name()
                )))
            }
        };
        if let Some(fm) = server().lookup_component::<crate::components::file_manager::FileManager>("file_manager") {
            fm.add_reserved_path(&format!("update_manager {}", app.name()), &venv, true);
        }
        app.configure_managed_services(config)?;
        let primary_branch = config.get_opt("primary_branch")?;
        let mut project_name = config.get_or("project_name", app.name())?;
        let mut extras = None;
        if let Some(c) = regex::Regex::new(r"^([^\[]+)\[([^\]]+)\]").unwrap().captures(&project_name.clone()) {
            project_name = c[1].to_string();
            extras = Some(c[2].to_string());
        }
        let d = PythonDeploy {
            app,
            primary_branch,
            project_name,
            extras,
            st: RefCell::new(PyState {
                source: PackageSource::Unknown,
                repo_url: String::new(),
                repo_owner: "?".into(),
                repo_name: "?".into(),
                current_version: Version::py("?"),
                git_version: Version::git("?"),
                current_sha: "?".into(),
                upstream_version: Version::py("?"),
                upstream_sha: "?".into(),
                rollback_version: Version::py("?"),
                rollback_ref: "?".into(),
                warnings: vec![],
                changelog: String::new(),
                system_deps: vec![],
            }),
        };
        // upstream raises here when the distribution is missing
        let info = load_distribution_info(&venv, &d.project_name).map_err(|e| ConfigHelper::error(e.message))?;
        d.apply_package_info(&info);
        Ok(d)
    }

    fn apply_package_info(&self, info: &PackageInfo) {
        self.detect_update_source(info);
        self.update_current_version(info);
        let changelog = self.get_url(&["changelog"], info);
        let deps = Self::parse_system_dependencies(info);
        let mut st = self.st.borrow_mut();
        st.changelog = changelog;
        st.system_deps = deps;
        self.app.is_valid.set(st.warnings.is_empty());
    }

    fn add_warning(&self, msg: &str) {
        self.st.borrow_mut().warnings.push(msg.to_string());
        self.app.base.log_info(msg);
    }

    fn detect_update_source(&self, info: &PackageInfo) {
        self.st.borrow_mut().source = PackageSource::Unknown;
        let du = match &info.direct_url_data {
            None => {
                let url = self.get_url(&["repository", "repo"], info);
                let mut st = self.st.borrow_mut();
                st.source = PackageSource::Pip;
                st.repo_url = url;
                drop(st);
                self.match_repo_url();
                return;
            }
            Some(d) => d,
        };
        self.app.base.log_debug(&format!("Direct URL info: {}", py_repr(du)));
        let vcs = du.get("vcs_info").cloned().unwrap_or(json!({}));
        if vcs.get("vcs").and_then(|v| v.as_str()).unwrap_or("") != "git" {
            self.add_warning(&format!("Package installed from source other than pypi or git: {}", py_repr(du)));
            return;
        }
        match (vcs.get("commit_id"), du.get("url")) {
            (Some(c), Some(u)) => {
                let mut st = self.st.borrow_mut();
                st.current_sha = crate::pyfmt::py_str(c);
                st.repo_url = crate::pyfmt::py_str(u);
            }
            _ => {
                self.add_warning("Failed to retrieve direct_url vcs info");
                return;
            }
        }
        if !self.match_repo_url() {
            let url = self.st.borrow().repo_url.clone();
            self.add_warning(&format!("Invalid repo url: {}", url));
            return;
        }
        self.st.borrow_mut().source = PackageSource::Github;
    }

    fn match_repo_url(&self) -> bool {
        let re = regex::Regex::new(r"(?i)^https://(?:www\.)?github\.com/(?P<owner>.+?)/(?P<proj>.+?)(?:\.git|$)").unwrap();
        let mut st = self.st.borrow_mut();
        let caps = match re.captures(&st.repo_url) {
            None => return false,
            Some(c) => (c["owner"].to_string(), c["proj"].to_string()),
        };
        st.repo_owner = if caps.0.is_empty() { "?".into() } else { caps.0 };
        st.repo_name = if caps.1.is_empty() { "?".into() } else { caps.1 };
        true
    }

    fn get_url(&self, keys: &[&str], info: &PackageInfo) -> String {
        let primary = keys[0];
        if let Some(ri) = &info.release_info {
            if let Some(Value::Object(urls)) = ri.get("urls") {
                for (name, url) in urls {
                    if keys.contains(&name.to_lowercase().as_str()) {
                        return crate::pyfmt::py_str(url);
                    }
                }
            }
            self.app.base.log_debug(&format!("Unable to find {} url in release info", primary));
        }
        for url in info.metadata.get_all("Project-URL") {
            if let Some((key, u)) = url.split_once(',') {
                if keys.contains(&key.to_lowercase().trim()) {
                    return u.trim().to_string();
                }
            }
        }
        self.app.base.log_info(&format!("Unable to find {} url in metadata", primary));
        String::new()
    }

    fn update_current_version(&self, info: &PackageInfo) -> bool {
        let mut pkg_version = String::new();
        if let Some(ri) = &info.release_info {
            let mut st = self.st.borrow_mut();
            if st.current_sha == "?" {
                st.current_sha = ri.get("commit_sha").map(crate::pyfmt::py_str).unwrap_or("?".into());
            }
            st.git_version = Version::git(&ri.get("git_version").map(crate::pyfmt::py_str).unwrap_or("?".into()));
            pkg_version = ri.get("package_version").map(crate::pyfmt::py_str).unwrap_or_default();
        }
        if let Some(v) = info.metadata.get("Version") {
            pkg_version = v;
        }
        if pkg_version.is_empty() {
            self.add_warning("Failed to detect package version");
            return false;
        }
        let cv = Version::py(&pkg_version);
        let valid = cv.is_valid_version();
        self.st.borrow_mut().current_version = cv.clone();
        if !valid {
            self.add_warning("Failed to parse package version");
            return false;
        }
        let mut st = self.st.borrow_mut();
        if st.current_sha == "?" {
            if st.source != PackageSource::Github {
                st.current_sha = "not-specified".into();
            } else if let Some(local) = cv.local.as_ref().filter(|l| !l.is_empty()) {
                st.current_sha = local.get(1..).unwrap_or("").split('.').next().unwrap_or("").to_string();
            }
        }
        if !st.git_version.is_valid_version() {
            st.git_version = cv.convert_to_git();
        }
        true
    }

    fn parse_system_dependencies(info: &PackageInfo) -> Vec<String> {
        match &info.release_info {
            None => vec![],
            Some(ri) => SysDepsParser::new().parse_dependencies(&ri.get("system_dependencies").cloned().unwrap_or(json!({}))),
        }
    }

    fn update_local_state(&self) {
        self.st.borrow_mut().warnings.clear();
        let venv = self.app.virtualenv.clone().unwrap_or_default();
        match load_distribution_info(&venv, &self.project_name) {
            Err(_) => self.add_warning("Failed to parse package info"),
            Ok(info) => {
                {
                    let mut st = self.st.borrow_mut();
                    st.git_version = Version::git("?");
                    st.current_sha = "?".into();
                    st.current_version = Version::py("?");
                }
                self.apply_package_info(&info);
                return;
            }
        }
        self.app.is_valid.set(self.st.borrow().warnings.is_empty());
    }

    async fn refresh_pip(&self) -> Result<(), ServerError> {
        self.app.base.log_info("Requesting package info via PIP...");
        let norm = normalize_project_name(&self.project_name);
        let mut pip_args = "install -U --quiet".to_string();
        if self.app.channel == Channel::Beta {
            pip_args.push_str(" --pre");
        }
        pip_args = format!("{} --dry-run --no-deps --report - {}", pip_args, norm);
        let pip_exec = AsyncPipExecutor::new(self.app.pip_cmd.as_deref().unwrap_or(""), None);
        self.app.update_pip(&pip_exec).await;
        let resp = pip_exec.call_pip_with_response(&pip_args, 1200.0, 3, None).await?;
        let data: Value = serde_json::from_str(&resp).map_err(|e| ServerError::new(e.to_string(), 500))?;
        let install_data = data.get("install").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        if install_data.is_empty() {
            let mut st = self.st.borrow_mut();
            st.upstream_version = st.current_version.clone();
            return Ok(());
        }
        let mut metadata = install_data[0].get("metadata").cloned().unwrap_or(json!({}));
        let name = normalize_project_name(metadata.get("name").and_then(|v| v.as_str()).unwrap_or(""));
        if install_data.len() > 1 && name != norm {
            let found = install_data[1..].iter().find_map(|inst| {
                let md = inst.get("metadata").cloned().unwrap_or(json!({}));
                (normalize_project_name(md.get("name").and_then(|v| v.as_str()).unwrap_or("")) == norm).then_some(md)
            });
            metadata = found.ok_or_else(|| ServerError::bad("Failed to find metadata for package"))?;
        }
        let version = metadata.get("version").map(crate::pyfmt::py_str).unwrap_or("?".into());
        let uv = Version::py(&version);
        let lt = self.st.borrow().current_version.lt(&uv)?;
        let mut st = self.st.borrow_mut();
        st.upstream_version = uv;
        st.upstream_sha = if lt { "update-available".into() } else { st.current_sha.clone() };
        Ok(())
    }

    async fn refresh_github(&self) -> Result<(), ServerError> {
        let repo = {
            let st = self.st.borrow();
            format!("{}/{}", st.repo_owner, st.repo_name)
        };
        let http = self.app.base.cmd_helper.http();
        if self.app.channel == Channel::Dev {
            let mut resource = format!("/repos/{}/commits?per_page=1", repo);
            if let Some(b) = &self.primary_branch {
                resource.push_str(&format!("&sha={}", b));
            }
            let resp = http.github_api_request(&resource, 3, 0.5).await?;
            if resp.status_code != 304 && resp.has_error() {
                self.app.base.log_info(&format!("Github Request Error - {}", resp.error_string()));
                return Ok(());
            }
            let commits = resp.json()?.as_array().cloned().unwrap_or_default();
            if commits.is_empty() {
                self.app.base.log_info("No commits found");
                return Ok(());
            }
            let sha = commits[0].get("sha").map(crate::pyfmt::py_str).ok_or_else(|| ServerError::new("KeyError: 'sha'", 500))?;
            let mut st = self.st.borrow_mut();
            st.upstream_sha = sha.clone();
            st.upstream_version = st.current_version.clone();
            if sha != st.current_sha {
                let local = format!("g{}", &sha[..sha.len().min(8)]);
                st.upstream_version = st.current_version.bump_local_version(&local);
            }
            return Ok(());
        }
        let resource = if self.app.channel == Channel::Stable {
            format!("repos/{}/releases/latest", repo)
        } else {
            format!("repos/{}/releases?per_page=1", repo)
        };
        let resp = http.github_api_request(&resource, 3, 0.5).await?;
        if resp.status_code != 304 && resp.has_error() {
            self.app.base.log_info(&format!("Github Request Error - {}", resp.error_string()));
            return Ok(());
        }
        let result = match resp.json()? {
            Value::Array(a) => a.first().cloned().unwrap_or(json!({})),
            other => other,
        };
        if result.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            self.app.base.log_info("No releases found");
            let mut st = self.st.borrow_mut();
            st.upstream_sha = st.current_sha.clone();
            st.upstream_version = st.current_version.clone();
            return Ok(());
        }
        let tag = result.get("tag_name").map(crate::pyfmt::py_str).ok_or_else(|| ServerError::new("KeyError: 'tag_name'", 500))?;
        let uv = Version::py(&tag);
        let gt = uv.gt(&self.st.borrow().current_version)?;
        let mut st = self.st.borrow_mut();
        st.upstream_version = uv;
        if gt {
            st.upstream_sha = "update-available".into();
        }
        Ok(())
    }

    async fn do_update(&self, rollback: bool) -> Result<bool, ServerError> {
        let mut project_name = normalize_project_name(&self.project_name);
        if let Some(e) = &self.extras {
            project_name = format!("{}[{}]", project_name, e);
        }
        let (current_version, mut current_ref, install_ver, source) = {
            let st = self.st.borrow();
            let iv = if rollback { st.rollback_version.clone() } else { st.upstream_version.clone() };
            (st.current_version.clone(), st.current_version.tag().to_string(), iv, st.source)
        };
        if !install_ver.is_valid_version()
            || (current_version.is_valid_version() && current_version.eq(&install_ver).unwrap_or(false))
        {
            return Ok(false);
        }
        let pip_exec = AsyncPipExecutor::new(self.app.pip_cmd.as_deref().unwrap_or(""), Some(self.app.base.cmd_helper.notify_cb()));
        let mut pip_args = "install -U --upgrade-strategy eager".to_string();
        let rollback_ref = self.st.borrow().rollback_ref.clone();
        match source {
            PackageSource::Pip => {
                if self.app.channel == Channel::Beta {
                    pip_args.push_str(" --pre");
                }
                pip_args = format!("{} {}", pip_args, project_name);
                if rollback {
                    pip_args.push_str(&format!("=={}", rollback_ref));
                }
            }
            PackageSource::Github => {
                let mut repo = {
                    let st = self.st.borrow();
                    format!("{}/{}", st.repo_owner, st.repo_name)
                };
                if rollback {
                    repo.push_str(&format!("@{}", rollback_ref));
                } else if self.app.channel == Channel::Dev {
                    current_ref = self.st.borrow().current_sha.clone();
                    if let Some(b) = &self.primary_branch {
                        repo.push_str(&format!("@{}", b));
                    }
                } else {
                    repo.push_str(&format!("@{}", self.st.borrow().upstream_version.tag()));
                }
                pip_args = format!("{} '{} @ git+https://github.com/{}'", pip_args, project_name, repo);
            }
            PackageSource::Unknown => return Err(ServerError::bad("Cannot update, package source is unknown")),
        }
        self.app.update_pip(&pip_exec).await;
        let sys_deps = self.st.borrow().system_deps.clone();
        self.app.base.notify_status(&format!("Updating Python Package {} from {}...", self.app.name(), source.name()), false);
        pip_exec.call_pip(&pip_args, 3600.0, 3, self.app.pip_env_vars.as_ref()).await?;
        self.update_local_state();
        if !rollback {
            let mut st = self.st.borrow_mut();
            st.rollback_version = current_version;
            st.rollback_ref = current_ref;
            st.upstream_sha = st.current_sha.clone();
            st.upstream_version = st.current_version.clone();
        }
        self.update_sys_deps(&sys_deps).await;
        self.log_package_info();
        save_state(self);
        self.app.restart_service().await?;
        self.app.base.notify_status("Update Finished...", true);
        Ok(true)
    }

    async fn update_sys_deps(&self, prev: &[String]) {
        let new_deps = self.st.borrow().system_deps.clone();
        let mut diff: Vec<String> = Vec::new();
        for d in &new_deps {
            if !prev.contains(d) && !diff.contains(d) {
                diff.push(d.clone());
            }
        }
        if !new_deps.is_empty() || !prev.is_empty() {
            self.app.base.log_debug(&format!(
                "Pre-update system dependencies: {}\nPost-update system dependencies: {}\nDifference to be installed: {}",
                str_list(prev),
                str_list(&new_deps),
                str_list(&diff)
            ));
        }
        if !diff.is_empty() {
            self.app.base.notify_status("Installing system dependencies...", false);
            if let Err(e) = self.app.base.cmd_helper.install_packages(&diff, 3600.0, true).await {
                self.app.base.log_exc(&format!("Error updating packages\n{}", e.message), true);
            }
        }
    }

    fn log_package_info(&self) {
        let st = self.st.borrow();
        log::info!(
            "Python Package {} detected:\nChannel: {}\nPackage Source: {}\nRepo Owner: {}\nRepo Name: {}\nRepo URL: {}\nChangelog URL: {}\nFull Version String: {}\nCurrent Version: {}\nCurrent Commit SHA: {}\nUpstream Version: {}\nUpstream Commit SHA: {}\nConverted Git Version: {}\nRollback Version: {}\nRollback Ref: {}\n",
            self.app.name(),
            self.app.channel.name(),
            st.source.name(),
            st.repo_owner,
            st.repo_name,
            st.repo_url,
            st.changelog,
            st.current_version.full_version(),
            st.current_version.short_version(),
            st.current_sha,
            st.upstream_version.short_version(),
            st.upstream_sha,
            st.git_version,
            st.rollback_version,
            st.rollback_ref
        );
    }
}

#[async_trait(?Send)]
impl Deployer for PythonDeploy {
    fn base(&self) -> &BaseDeploy {
        &self.app.base
    }
    fn kind(&self) -> DeployKind {
        DeployKind::Python
    }
    fn app(&self) -> Option<&AppDeploy> {
        Some(&self.app)
    }

    async fn initialize(&self) -> Map<String, Value> {
        let storage = self.app.initialize();
        let s = |k: &str| storage.get(k).and_then(|v| v.as_str()).unwrap_or("?").to_string();
        {
            let mut st = self.st.borrow_mut();
            st.upstream_sha = s("upstream_commit");
            st.upstream_version = Version::py(&s("upstream_version"));
            st.rollback_ref = s("rollback_ref");
            st.rollback_version = Version::py(&s("rollback_version"));
        }
        if !self.app.base.needs_refresh(false) {
            self.log_package_info();
        }
        storage
    }

    fn get_persistent_data(&self) -> Map<String, Value> {
        let mut storage = self.app.get_persistent_data();
        let st = self.st.borrow();
        storage.insert("upstream_commit".into(), json!(st.upstream_sha));
        storage.insert("upstream_version".into(), json!(st.upstream_version.full_version()));
        storage.insert("rollback_ref".into(), json!(st.rollback_ref));
        storage.insert("rollback_version".into(), json!(st.rollback_version.full_version()));
        storage
    }

    fn get_update_status(&self) -> Value {
        let mut status = self.app.get_update_status();
        let st = self.st.borrow();
        let v = json!({
            "detected_type": "python_package",
            "name": self.app.name(),
            "branch": self.primary_branch,
            "owner": st.repo_owner,
            "repo_name": st.repo_name,
            "version": st.current_version.short_version(),
            "remote_version": st.upstream_version.short_version(),
            "rollback_version": st.rollback_version.short_version(),
            "current_hash": st.current_sha,
            "remote_hash": st.upstream_sha,
            "is_dirty": st.git_version.dirty(),
            "changelog_url": st.changelog,
            "full_version_string": st.current_version.full_version(),
            "pristine": !st.git_version.dirty(),
            "warnings": st.warnings,
        });
        for (k, val) in v.as_object().unwrap() {
            status.insert(k.clone(), val.clone());
        }
        Value::Object(status)
    }

    async fn refresh(&self) -> Result<(), ServerError> {
        self.update_local_state();
        let source = self.st.borrow().source;
        let res = match source {
            PackageSource::Pip => self.refresh_pip().await,
            PackageSource::Github => self.refresh_github().await,
            PackageSource::Unknown => {
                self.app.base.log_info("Cannot refresh, package source is unknown");
                Ok(())
            }
        };
        if let Err(e) = res {
            self.app.base.log_exc(&format!("Error Refreshing Python Package: {}\n{}", self.app.name(), e.message), true);
        }
        self.log_package_info();
        save_state(self);
        Ok(())
    }

    async fn update(&self) -> Result<bool, ServerError> {
        self.do_update(false).await
    }

    async fn recover(&self, _hard: bool, _force: bool) -> Result<(), ServerError> {
        Ok(())
    }

    async fn rollback(&self) -> Result<bool, ServerError> {
        let ok = {
            let st = self.st.borrow();
            st.rollback_ref != "?" && st.rollback_version.is_valid_version()
        };
        if !ok {
            return Ok(false);
        }
        self.do_update(true).await?;
        Ok(true)
    }
}

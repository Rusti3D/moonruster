// Shared update manager helpers: app types, channels, version parsing
// (moonraker/utils/versions.py), pip helpers (utils/pip_utils.py) and system
// dependency parsing (utils/sysdeps_parser.py)

use crate::common::ServerError;
use crate::components::shell_command::{ExecOpts, OutputCallback, ShellCommandFactory};
use crate::server::server;
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::LazyLock;

// ***** AppType / Channel *****

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AppType {
    None,
    Web,
    GitRepo,
    Zip,
    Python,
    Executable,
}

impl AppType {
    pub fn name(&self) -> &'static str {
        match self {
            AppType::None => "none",
            AppType::Web => "web",
            AppType::GitRepo => "git_repo",
            AppType::Zip => "zip",
            AppType::Python => "python",
            AppType::Executable => "executable",
        }
    }
    pub fn from_string(s: &str) -> Option<AppType> {
        Self::all().into_iter().find(|t| t.name() == s.to_lowercase())
    }
    pub fn all() -> Vec<AppType> {
        vec![AppType::None, AppType::Web, AppType::GitRepo, AppType::Zip, AppType::Python, AppType::Executable]
    }
    pub fn valid_types() -> Vec<AppType> {
        Self::all().into_iter().filter(|t| *t != AppType::None).collect()
    }
    pub fn detect(app_path: Option<&Path>) -> AppType {
        match app_path {
            Some(p) if crate::utils::expand_user(&p.to_string_lossy()).join(".git").exists() => AppType::GitRepo,
            _ => AppType::None,
        }
    }
    pub fn supported_channels(&self) -> Vec<Channel> {
        match self {
            AppType::None => vec![],
            AppType::Web | AppType::Zip | AppType::Executable => vec![Channel::Stable, Channel::Beta],
            _ => vec![Channel::Stable, Channel::Beta, Channel::Dev],
        }
    }
    pub fn default_channel(&self) -> Channel {
        if *self == AppType::GitRepo {
            Channel::Dev
        } else {
            Channel::Stable
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Channel {
    Stable,
    Beta,
    Dev,
}

impl Channel {
    pub fn name(&self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
            Channel::Dev => "dev",
        }
    }
    pub fn from_string(s: &str) -> Option<Channel> {
        [Channel::Stable, Channel::Beta, Channel::Dev].into_iter().find(|c| c.name() == s)
    }
}

pub fn pretty_print_time(seconds: i64) -> String {
    if seconds == 0 {
        return "0 Seconds".into();
    }
    let days = seconds.div_euclid(86400);
    let rem = seconds.rem_euclid(86400);
    let parts = [("Day", days), ("Hour", rem / 3600), ("Minute", (rem % 3600) / 60), ("Second", rem % 60)];
    parts
        .iter()
        .filter(|(_, v)| *v != 0)
        .map(|(n, v)| if *v == 1 { format!("{} {}", v, n) } else { format!("{} {}s", v, n) })
        .collect::<Vec<_>>()
        .join(", ")
}

// ***** Versions *****

const RT_FINAL: u32 = 1;
const RT_ALPHA: u32 = 2;
const RT_BETA: u32 = 4;
const RT_RC: u32 = 8;
const RT_POST: u32 = 16;
const RT_DEV: u32 = 32;

static PY_VERSION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?xi)^\s*
        v?
        (?:
            (?:(?P<epoch>[0-9]+)!)?
            (?P<release>[0-9]+(?:\.[0-9]+)*)
            (?P<pre>
                [-_\.]?
                (?P<pre_l>(a|b|c|rc|alpha|beta|pre|preview))
                [-_\.]?
                (?P<pre_n>[0-9]+)?
            )?
            (?P<post>
                (?:-(?P<post_n1>[0-9]+))
                |
                (?:
                    [-_\.]?
                    (?P<post_l>post|rev|r)
                    [-_\.]?
                    (?P<post_n2>[0-9]+)?
                )
            )?
            (?P<dev>
                [-_\.]?
                (?P<dev_l>dev)
                [-_\.]?
                (?P<dev_n>[0-9]+)?
            )?
        )
        (?:\+(?P<local>[a-z0-9]+(?:[-_\.][a-z0-9]+)*))?
        \s*$",
    )
    .unwrap()
});

static GIT_VERSION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?xi)^\s*
        (?P<tag>
            v?
            (?P<release>[0-9]+(?:\.[0-9]+)*)
            (?P<pre>
                [-_\.]?
                (?P<pre_l>(a|b|c|rc|alpha|beta|pre|preview))
                [-_\.]?
                (?P<pre_n>[0-9]+)?
            )?
        )
        (?:
            (?:-(?P<dev_n>[0-9]+))
            (?:-g(?P<hash>[a-fA-F0-9]+))?
        )?
        (?P<dirty>-dirty)?
        (?P<inferred>-(?:inferred|shallow))?
        \s*$",
    )
    .unwrap()
});

fn pre_conv(lbl: char) -> i64 {
    match lbl {
        'a' => 1,
        'b' => 2,
        'c' | 'r' | 'p' => 3,
        _ => 0,
    }
}

#[derive(Clone, Debug)]
pub struct Version {
    release: String,
    release_type: u32,
    tag: String,
    orig: String,
    release_tup: Vec<i64>,
    extra_tup: Vec<i64>,
    has_dev_part: bool,
    dev_count: i64,
    valid: bool,
    is_git: bool,
    // git only
    dirty: bool,
    inferred: bool,
    // python only
    pub local: Option<String>,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.orig)
    }
}

impl Version {
    fn base(version: &str, is_git: bool) -> Version {
        Version {
            release: "?".into(),
            release_type: 0,
            tag: "?".into(),
            orig: version.trim().to_string(),
            release_tup: vec![],
            extra_tup: vec![],
            has_dev_part: false,
            dev_count: 0,
            valid: false,
            is_git,
            dirty: false,
            inferred: false,
            local: None,
        }
    }

    pub fn git(version: &str) -> Version {
        let mut v = Self::base(version, true);
        let caps = match GIT_VERSION_RE.captures(version) {
            Some(c) => c,
            None => {
                let fb = Regex::new(r"^(?P<hash>[a-fA-F0-9]+)(?P<dirty>-dirty)?").unwrap();
                if let Some(c) = fb.captures(&v.orig) {
                    v.tag = String::new();
                    v.release = c["hash"].to_string();
                    v.dirty = c.name("dirty").is_some();
                    v.inferred = true;
                }
                return v;
            }
        };
        let release = caps["release"].to_string();
        v.valid = true;
        v.tag = caps.name("tag").map(|m| m.as_str().to_string()).unwrap_or("?".into());
        v.release_tup = release.split('.').map(|p| p.parse().unwrap_or(0)).collect();
        v.release = release;
        v.extra_tup = vec![1, 0, 0];
        if caps.name("pre").is_some() {
            let lbl = caps["pre_l"].chars().next().unwrap_or(' ').to_ascii_lowercase();
            let n = caps.name("pre_n").map(|m| m.as_str().parse().unwrap_or(0)).unwrap_or(0);
            v.extra_tup = vec![0, pre_conv(lbl), n];
            let idx = match pre_conv(lbl) {
                0 => 1,
                i => i,
            };
            v.release_type = 1 << idx;
        }
        v.has_dev_part = true;
        v.dev_count = caps.name("dev_n").map(|m| m.as_str().parse().unwrap_or(0)).unwrap_or(0);
        if v.dev_count > 0 {
            v.release_type |= RT_DEV;
        }
        if v.release_type == 0 {
            v.release_type = RT_FINAL;
        }
        v.inferred = caps.name("inferred").is_some();
        v.dirty = caps.name("dirty").is_some();
        v
    }

    pub fn py(version: &str) -> Version {
        let mut v = Self::base(version, false);
        let caps = match PY_VERSION_RE.captures(&v.orig) {
            Some(c) => c,
            None => return v,
        };
        let release = caps["release"].to_string();
        v.valid = true;
        v.tag = if v.orig.to_lowercase().starts_with('v') { format!("v{}", release) } else { release.clone() };
        v.release_tup = release.split('.').map(|p| p.parse().unwrap_or(0)).collect();
        v.release = release;
        v.extra_tup = vec![1, 0, 0];
        let num = |a: &str, b: &str| -> i64 {
            caps.name(a).or(caps.name(b)).map(|m| m.as_str().parse().unwrap_or(0)).unwrap_or(0)
        };
        if let Some(pre) = caps.name("pre") {
            let lbl = caps["pre_l"].chars().next().unwrap_or(' ').to_ascii_lowercase();
            let n = caps.name("pre_n").map(|m| m.as_str().parse().unwrap_or(0)).unwrap_or(0);
            v.extra_tup = vec![0, pre_conv(lbl), n];
            v.tag.push_str(pre.as_str());
            let idx = match pre_conv(lbl) {
                0 => 1,
                i => i,
            };
            v.release_type |= 1 << idx;
            if let Some(post) = caps.name("post") {
                v.extra_tup.push(num("post_n1", "post_n2"));
                v.tag.push_str(post.as_str());
                v.release_type |= RT_POST;
            }
        } else if let Some(post) = caps.name("post") {
            v.extra_tup = vec![2, num("post_n1", "post_n2"), 0];
            v.tag.push_str(post.as_str());
            v.release_type |= RT_POST;
        }
        v.has_dev_part = caps.name("dev").is_some();
        if v.has_dev_part {
            v.release_type |= RT_DEV;
        } else if v.release_type == 0 {
            v.release_type = RT_FINAL;
        } else if v.release_type == RT_POST {
            v.release_type |= RT_FINAL;
        }
        v.dev_count = caps.name("dev_n").map(|m| m.as_str().parse().unwrap_or(0)).unwrap_or(0);
        v.local = caps.name("local").map(|m| m.as_str().to_string());
        v
    }

    pub fn full_version(&self) -> String {
        self.orig.clone()
    }
    pub fn short_version(&self) -> String {
        if !self.valid {
            return "?".into();
        }
        if !self.is_git && self.dev_count == 0 {
            return self.tag.clone();
        }
        format!("{}-{}", self.tag, self.dev_count)
    }
    pub fn tag(&self) -> &str {
        &self.tag
    }
    pub fn dev_count(&self) -> i64 {
        self.dev_count
    }
    pub fn is_valid_version(&self) -> bool {
        self.valid
    }
    pub fn is_final_release(&self) -> bool {
        self.release_type & RT_FINAL != 0
    }
    pub fn is_alpha_release(&self) -> bool {
        self.release_type & RT_ALPHA != 0
    }
    pub fn is_pre_release(&self) -> bool {
        self.release_type & (RT_ALPHA | RT_BETA | RT_RC) != 0
    }
    pub fn dirty(&self) -> bool {
        self.dirty
    }
    pub fn inferred(&self) -> bool {
        self.inferred
    }
    pub fn is_fallback(&self) -> bool {
        self.inferred && !self.valid
    }

    fn validate(&self, other: &Version) -> Result<(), ServerError> {
        let tname = if self.is_git { "GitVersion" } else { "PyVersion" };
        for v in [self, other] {
            if !v.valid {
                return Err(ServerError::new(
                    format!("Version {} is not a valid version string for type {}", v.orig, tname),
                    500,
                ));
            }
        }
        Ok(())
    }

    pub fn eq(&self, o: &Version) -> Result<bool, ServerError> {
        self.validate(o)?;
        Ok(self.release_tup == o.release_tup
            && self.extra_tup == o.extra_tup
            && self.has_dev_part == o.has_dev_part
            && self.dev_count == o.dev_count)
    }

    pub fn lt(&self, o: &Version) -> Result<bool, ServerError> {
        self.validate(o)?;
        if self.release_tup != o.release_tup {
            return Ok(self.release_tup < o.release_tup);
        }
        if self.extra_tup != o.extra_tup {
            return Ok(self.extra_tup < o.extra_tup);
        }
        if self.has_dev_part != o.has_dev_part {
            return Ok(self.has_dev_part);
        }
        Ok(self.dev_count < o.dev_count)
    }

    pub fn le(&self, o: &Version) -> Result<bool, ServerError> {
        self.validate(o)?;
        if self.release_tup > o.release_tup {
            return Ok(false);
        }
        if self.extra_tup > o.extra_tup {
            return Ok(false);
        }
        if self.has_dev_part != o.has_dev_part {
            return Ok(self.has_dev_part);
        }
        Ok(self.dev_count <= o.dev_count)
    }

    pub fn gt(&self, o: &Version) -> Result<bool, ServerError> {
        self.validate(o)?;
        if self.release_tup != o.release_tup {
            return Ok(self.release_tup > o.release_tup);
        }
        if self.extra_tup != o.extra_tup {
            return Ok(self.extra_tup > o.extra_tup);
        }
        if self.has_dev_part != o.has_dev_part {
            return Ok(o.has_dev_part);
        }
        Ok(self.dev_count > o.dev_count)
    }

    pub fn infer_last_tag(&self) -> String {
        if !self.valid {
            return "v0.0.0".into();
        }
        if self.inferred {
            return self.tag.clone();
        }
        if self.is_pre_release() && self.extra_tup.as_slice() > [0, 1, 0].as_slice() {
            let mut type_idx = self.extra_tup[1];
            let mut type_count = self.extra_tup[2];
            if type_count == 0 {
                type_idx -= 1;
            } else {
                type_count -= 1;
            }
            let pretype = match type_idx {
                1 => "a",
                2 => "b",
                _ => "rc",
            };
            return format!("{}.{}{}", self.release, pretype, type_count);
        }
        let mut parts: Vec<i64> = self.release.split('.').map(|p| p.parse().unwrap_or(0)).collect();
        let mut need_decrement = true;
        for part in parts.iter_mut().rev() {
            if *part > 0 && need_decrement {
                need_decrement = false;
                *part -= 1;
            }
        }
        format!("v{}", parts.iter().map(|p| p.to_string()).collect::<Vec<_>>().join("."))
    }

    fn py_info(&self) -> Option<regex::Captures<'_>> {
        PY_VERSION_RE.captures(&self.orig)
    }

    pub fn bump_local_version(&self, new_local: &str) -> Version {
        let caps = match self.py_info() {
            Some(c) => c,
            None => return Version::py("?"),
        };
        let mut new_ver = caps["release"].to_string();
        if let Some((major, minor)) = new_ver.clone().rsplit_once('.') {
            if !major.is_empty() && self.dev_count == 0 {
                new_ver = format!("v{}.{}", major, minor.parse::<i64>().unwrap_or(0) + 1);
            }
        }
        if let Some(p) = caps.name("pre") {
            new_ver.push_str(p.as_str());
        }
        if let Some(p) = caps.name("post") {
            new_ver.push_str(p.as_str());
        }
        if self.dev_count != 0 {
            new_ver.push_str(&format!(".dev{}", self.dev_count + 1));
        } else {
            new_ver.push_str(".dev1");
        }
        new_ver.push_str(&format!("+{}", new_local));
        Version::py(&new_ver)
    }

    pub fn convert_to_git(&self) -> Version {
        let caps = match self.py_info() {
            Some(c) => c,
            None => return Version::git("?"),
        };
        let mut git_version = caps["release"].to_string();
        if self.orig.to_lowercase().starts_with('v') {
            git_version = format!("v{}", git_version);
        }
        let local = caps.name("local").map(|m| m.as_str().to_string()).unwrap_or_default();
        if self.dev_count > 0 {
            if let Some((major, minor)) = git_version.clone().rsplit_once('.') {
                if !major.is_empty() {
                    git_version = format!("v{}.{}", major, (minor.parse::<i64>().unwrap_or(0) - 1).max(0));
                }
            }
        }
        if let Some(p) = caps.name("pre") {
            git_version.push_str(p.as_str());
        }
        git_version = format!("{}-{}", git_version, self.dev_count);
        if !local.is_empty() {
            let parts: Vec<&str> = Regex::new(r"[-_\.]").unwrap().split(&local).collect();
            if !parts[0].is_empty() {
                git_version = format!("{}-{}", git_version, parts[0]);
            }
            if parts.len() > 1 {
                git_version.push_str("-dirty");
            }
        }
        Version::git(&git_version)
    }
}

// ***** Pip helpers *****

pub const MAX_PIP_VERSION: (i64, i64) = (25, 2);

#[derive(Clone, Debug)]
pub struct PipVersionInfo {
    pub pip_version_string: String,
    pub python_version_string: String,
}

fn ver_tuple(s: &str) -> Vec<i64> {
    s.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

impl PipVersionInfo {
    pub fn max_pip_version(&self) -> Vec<i64> {
        let py = ver_tuple(&self.python_version_string);
        if py < vec![3, 7] {
            vec![20, 3, 4]
        } else if py < vec![3, 8] {
            vec![24, 0]
        } else if py < vec![3, 9] {
            vec![25, 0, 1]
        } else {
            vec![MAX_PIP_VERSION.0, MAX_PIP_VERSION.1]
        }
    }
    pub fn needs_pip_update(&self) -> bool {
        ver_tuple(&self.pip_version_string) < self.max_pip_version()
    }
    pub fn max_pip_version_string(&self) -> String {
        self.max_pip_version().iter().map(|p| p.to_string()).collect::<Vec<_>>().join(".")
    }
}

pub fn parse_pip_version(resp: &str) -> Result<PipVersionInfo, ServerError> {
    let re = Regex::new(r"^pip ([0-9.]+) from .+? \(python ([0-9.]+)\)$").unwrap();
    let c = re.captures(resp.trim()).ok_or_else(|| ServerError::new("Unable to parse pip version from response", 500))?;
    Ok(PipVersionInfo { pip_version_string: c[1].trim().to_string(), python_version_string: c[2].trim().to_string() })
}

pub fn read_requirements_file(path: &Path) -> Result<Vec<String>, ServerError> {
    if !path.is_file() {
        return Err(ServerError::new(format!("Requirements file {} not found", path.display()), 500));
    }
    let data = std::fs::read_to_string(path)?;
    let re = Regex::new(r"\s#").unwrap();
    let mut modules = Vec::new();
    for line in data.split('\n') {
        let mut line = line.trim().to_string();
        if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
            continue;
        }
        if let Some(m) = re.find(&line) {
            line = line[..m.start()].trim().to_string();
        }
        modules.push(line);
    }
    Ok(modules)
}

pub enum PipPackages<'a> {
    File(&'a Path),
    List(&'a [String]),
}

pub fn prepare_install_args(packages: &PipPackages) -> Result<String, ServerError> {
    match packages {
        PipPackages::File(p) => {
            if !p.is_file() {
                return Err(ServerError::new(format!("Invalid path to requirements_file '{}'", p.display()), 500));
            }
            Ok(format!("-U -r {}", p.display()))
        }
        PipPackages::List(reqs) => {
            Ok(format!("-U {}", reqs.iter().map(|r| format!("\"{}\"", r.replace('"', "'"))).collect::<Vec<_>>().join(" ")))
        }
    }
}

fn merged_env(vars: Option<&HashMap<String, String>>) -> Option<HashMap<String, String>> {
    vars.map(|v| {
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.extend(v.clone());
        env
    })
}

pub struct AsyncPipExecutor {
    pip_cmd: String,
    notify: Option<OutputCallback>,
}

impl AsyncPipExecutor {
    pub fn new(pip_cmd: &str, notify: Option<OutputCallback>) -> Self {
        AsyncPipExecutor { pip_cmd: pip_cmd.to_string(), notify }
    }

    fn shell() -> Rc<ShellCommandFactory> {
        server().get::<crate::components::shell_command::ShellCommandFactory>().expect("shell_command")
    }

    pub async fn call_pip_with_response(
        &self,
        args: &str,
        timeout: f64,
        attempts: usize,
        env: Option<&HashMap<String, String>>,
    ) -> Result<String, ServerError> {
        Self::shell()
            .exec_cmd(
                &format!("{} {}", self.pip_cmd, args),
                ExecOpts { timeout, attempts, env: merged_env(env), log_stderr: true, ..Default::default() },
            )
            .await
            .map_err(|e| ServerError::new(e.message, 500))
    }

    pub async fn call_pip(&self, args: &str, timeout: f64, attempts: usize, env: Option<&HashMap<String, String>>) -> Result<(), ServerError> {
        Self::shell()
            .run_cmd_async(
                &format!("{} {}", self.pip_cmd, args),
                self.notify.clone(),
                None,
                timeout,
                attempts,
                false,
                1,
                None,
                true,
                true,
                merged_env(env),
                None,
                None,
            )
            .await
            .map_err(|e| ServerError::new(e.message, 500))
    }

    pub async fn get_pip_version(&self) -> Result<PipVersionInfo, ServerError> {
        let resp = Self::shell()
            .exec_cmd(
                &format!("{} --version", self.pip_cmd),
                ExecOpts { timeout: 30.0, attempts: 3, log_stderr: true, ..Default::default() },
            )
            .await
            .map_err(|e| ServerError::new(e.message, 500))?;
        parse_pip_version(&resp)
    }

    pub async fn update_pip(&self) {
        let version = format!("{}.{}", MAX_PIP_VERSION.0, MAX_PIP_VERSION.1);
        if let Err(e) = self.call_pip(&format!("install -U pip<={}", version), 1200.0, 1, None).await {
            log::error!("Failed to Update Pip\n{}", e.message);
        }
    }

    pub async fn install_packages(&self, packages: &PipPackages<'_>, env: Option<&HashMap<String, String>>) -> Result<(), ServerError> {
        let args = prepare_install_args(packages)?;
        self.call_pip(&format!("install {}", args), 1200.0, 3, env).await
    }
}

// ***** System dependency parsing *****

#[derive(Clone, Debug, PartialEq, PartialOrd)]
enum VerPart {
    Int(i64),
    Str(String),
}

fn convert_version(version: &str) -> Vec<VerPart> {
    let version = version.trim();
    let re = Regex::new(r"^\d+(\.\d+)*((?:-|\.).+)?").unwrap();
    if re.is_match(version) {
        return Regex::new(r"\.|-")
            .unwrap()
            .split(version)
            .map(|p| if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) { VerPart::Int(p.parse().unwrap_or(0)) } else { VerPart::Str(p.to_string()) })
            .collect();
    }
    vec![VerPart::Str(version.to_string())]
}

/// python tuple comparison, None when an int is compared with a str
fn cmp_versions(a: &[VerPart], b: &[VerPart]) -> Option<std::cmp::Ordering> {
    for (x, y) in a.iter().zip(b.iter()) {
        let o = match (x, y) {
            (VerPart::Int(i), VerPart::Int(j)) => i.cmp(j),
            (VerPart::Str(i), VerPart::Str(j)) => i.cmp(j),
            _ => return None,
        };
        if o != std::cmp::Ordering::Equal {
            return Some(o);
        }
    }
    Some(a.len().cmp(&b.len()))
}

pub struct SysDepsParser {
    distro_id: String,
    aliases: Vec<String>,
    distro_version: Vec<VerPart>,
    vendor: String,
    exclusions: Vec<String>,
}

impl SysDepsParser {
    pub fn new() -> Self {
        let osr = crate::components::machine::parse_os_release();
        let mut distro_id = osr.get("id").cloned().unwrap_or_default().to_lowercase().replace(' ', "_");
        distro_id = match distro_id.as_str() {
            "ol" => "oracle".into(),
            "opensuse-leap" => "opensuse".into(),
            _ => distro_id,
        };
        let aliases = osr.get("id_like").map(|s| s.split_whitespace().map(String::from).collect()).unwrap_or_default();
        let version = osr.get("version_id").cloned().unwrap_or_default();
        let distro_version = if version.is_empty() { vec![] } else { convert_version(&version) };
        let mut vendor = std::env::var("MOONRAKER_VENDOR").unwrap_or_default();
        if vendor.is_empty() && Path::new("/etc/rpi-issue").is_file() {
            vendor = "raspberry-pi".into();
        }
        let exclusions =
            std::env::var("MOONRAKER_EXCLUDED_PKGS").unwrap_or_default().split_whitespace().map(String::from).collect();
        SysDepsParser { distro_id, aliases, distro_version, vendor, exclusions }
    }

    fn parse_spec(&self, full_spec: &str) -> Option<String> {
        let mut parts = full_spec.splitn(2, ';');
        let pkg_name = parts.next().unwrap_or("").trim().to_string();
        if pkg_name.is_empty() || self.exclusions.contains(&pkg_name) {
            log::info!("Package '{}' excluded by environment", full_spec);
            return None;
        }
        let spec = match parts.next() {
            None => return Some(pkg_name),
            Some(s) => s.trim().to_string(),
        };
        // re.split with a capture group keeps the separators
        let sep = Regex::new(r"( and | or )").unwrap();
        let mut expressions: Vec<String> = Vec::new();
        let mut last = 0;
        for m in sep.find_iter(&spec) {
            expressions.push(spec[last..m.start()].to_string());
            expressions.push(m.as_str().to_string());
            last = m.end();
        }
        expressions.push(spec[last..].to_string());
        if expressions.len() & 1 == 0 {
            log::info!("Requirement specifier is missing an expression between logical operators : {}", full_spec);
            return None;
        }
        let mut last_result = true;
        let mut last_logical_op: Option<String> = Some("and".into());
        let op_re = Regex::new(r"(==|!=|<=|>=|<|>)").unwrap();
        for (idx, exp) in expressions.iter().enumerate() {
            if idx & 1 == 1 {
                if last_logical_op.is_some() {
                    log::info!("Requirement specifier contains sequential logical operators: {}", full_spec);
                    return None;
                }
                let op = exp.trim().to_string();
                if op != "and" && op != "or" {
                    log::info!("Invalid logical operator {} in requirement specifier: {}", op, full_spec);
                    return None;
                }
                last_logical_op = Some(op);
                continue;
            } else if last_logical_op.is_none() {
                log::info!(
                    "Requirement specifier contains two sequential expressions without a logical operator: {}",
                    full_spec
                );
                return None;
            }
            let exp = exp.trim();
            let mut dep_parts: Vec<String> = Vec::new();
            let mut l = 0;
            for m in op_re.find_iter(exp) {
                dep_parts.push(exp[l..m.start()].to_string());
                dep_parts.push(m.as_str().to_string());
                l = m.end();
            }
            dep_parts.push(exp[l..].to_string());
            let req_var = dep_parts[0].trim().to_lowercase();
            if dep_parts.len() != 3 {
                log::info!("Invalid comparison, must be 3 parts: {}", full_spec);
                return None;
            }
            let right_raw = dep_parts[2].trim().trim_matches(|c| c == '"' || c == '\'').to_string();
            let ord: Option<std::cmp::Ordering> = match req_var.as_str() {
                "distro_id" => Some(self.distro_id.as_str().cmp(right_raw.as_str())),
                "vendor" => Some(self.vendor.as_str().cmp(right_raw.as_str())),
                "distro_version" => {
                    if self.distro_version.is_empty() {
                        log::info!("Distro Version not detected, cannot satisfy requirement: {}", full_spec);
                        return None;
                    }
                    cmp_versions(&self.distro_version, &convert_version(&right_raw))
                }
                _ => {
                    log::info!("Invalid requirement specifier: {}", full_spec);
                    return None;
                }
            };
            let operator = dep_parts[1].trim();
            let result = match ord {
                None if operator == "==" => false,
                None if operator == "!=" => true,
                None => {
                    log::error!("Error comparing requirements: {}", full_spec);
                    return None;
                }
                Some(o) => {
                    use std::cmp::Ordering::*;
                    match operator {
                        "<" => o == Less,
                        ">" => o == Greater,
                        "==" => o == Equal,
                        "!=" => o != Equal,
                        ">=" => o != Less,
                        "<=" => o != Greater,
                        _ => false,
                    }
                }
            };
            if last_logical_op.as_deref() == Some("and") {
                last_result &= result;
            } else {
                last_result |= result;
            }
            last_logical_op = None;
        }
        if last_result {
            Some(pkg_name)
        } else {
            None
        }
    }

    pub fn parse_dependencies(&self, sys_deps: &Value) -> Vec<String> {
        if self.distro_id.is_empty() {
            log::info!("Failed to detect current distro ID, cannot parse dependencies");
            return vec![];
        }
        let mut all_ids = vec![self.distro_id.clone()];
        all_ids.extend(self.aliases.iter().cloned());
        for id in &all_ids {
            if let Some(deps) = sys_deps.get(id) {
                let list = deps.as_array().cloned().unwrap_or_default();
                if list.is_empty() {
                    log::info!("Dependency data contains an empty package definition for linux distro '{}'", id);
                    continue;
                }
                return list.iter().filter_map(|d| d.as_str()).filter_map(|d| self.parse_spec(d)).collect();
            }
        }
        log::info!("Dependency data has no package definition for linux distro '{}'", self.distro_id);
        vec![]
    }
}

// ***** PackageKit enum helpers *****

pub fn pk_name(names: &[&str], index: u32) -> String {
    names.get(index as usize).map(|s| s.to_string()).unwrap_or_default()
}

/// PkFlag.desc for a single member
pub fn pk_desc(names: &[&str], index: u32) -> String {
    let d = pk_name(names, index).to_lowercase().replace('_', " ");
    let mut c = d.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => d,
    }
}

pub fn pk_value(names: &[&str], name: &str) -> u64 {
    names.iter().position(|n| *n == name).map(|i| 1u64 << i).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_versions() {
        let v = Version::git("v0.11.0-3-g9e676eb2");
        assert!(v.is_valid_version() && !v.is_final_release());
        assert_eq!(v.short_version(), "v0.11.0-3");
        let t = Version::git("v0.11.0");
        assert!(t.is_final_release());
        assert!(t.lt(&v).unwrap());
        assert_eq!(Version::git("v0.11.0-2-gabc-dirty").dirty(), true);
        assert_eq!(Version::git("v1.2.0").infer_last_tag(), "v1.1.0");
        assert_eq!(Version::git("v1.0.0").infer_last_tag(), "v0.0.0");
        let fb = Version::git("9e676eb2");
        assert!(fb.is_fallback());
        assert_eq!(Version::git("v0.9.0-beta.1").is_pre_release(), true);
    }

    #[test]
    fn py_versions() {
        let v = Version::py("0.9.3.dev2+g1234abcd");
        assert_eq!(v.short_version(), "0.9.3-2");
        assert_eq!(v.local.as_deref(), Some("g1234abcd"));
        assert_eq!(Version::py("1.2.3").convert_to_git().full_version(), "1.2.3-0");
        assert_eq!(Version::py("v1.2.3").bump_local_version("gabc").full_version(), "v1.2.4.dev1+gabc");
        assert!(Version::py("1.0").lt(&Version::py("1.1")).unwrap());
    }

    #[test]
    fn misc() {
        assert_eq!(pretty_print_time(90061), "1 Day, 1 Hour, 1 Minute, 1 Second");
        assert_eq!(pretty_print_time(7200), "2 Hours");
        assert_eq!(pk_desc(&["UNKNOWN", "GET_UPDATES"], 1), "Get updates");
        let p = parse_pip_version("pip 24.0 from /x/pip (python 3.11)").unwrap();
        assert!(p.needs_pip_update());
    }
}

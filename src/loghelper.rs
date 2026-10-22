// Log Management (port of moonraker/loghelper.py)

use crate::common::*;
use crate::server::Server;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Mutex;

struct FileState {
    path: PathBuf,
    file: Option<File>,
    day: chrono::NaiveDate,
}

struct LogState {
    file: Option<FileState>,
    structured: bool,
    app_args: Map<String, Value>,
    rollover_info: IndexMap<String, String>,
    // size based rotation instead of daily (COSMOS patch to moonraker)
    max_bytes: Option<u64>,
}

const LOG_BACKUPS: usize = 2;

/// MOONRUSTER_LOG_MAX_BYTES: rotate at this size like COSMOS's moonraker,
/// set by scripts/install-cosmos.sh
pub fn log_max_bytes() -> Option<u64> {
    static MAX: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *MAX.get_or_init(|| std::env::var("MOONRUSTER_LOG_MAX_BYTES").ok().and_then(|v| v.parse().ok()).filter(|&b| b > 0))
}

static STATE: Mutex<Option<LogState>> = Mutex::new(None);

pub struct MoonLogger;

static LOGGER: MoonLogger = MoonLogger;

fn platform_str() -> String {
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    unsafe {
        if libc::uname(&mut uts) == 0 {
            let sys = std::ffi::CStr::from_ptr(uts.sysname.as_ptr()).to_string_lossy().to_string();
            let rel = std::ffi::CStr::from_ptr(uts.release.as_ptr()).to_string_lossy().to_string();
            let mach = std::ffi::CStr::from_ptr(uts.machine.as_ptr()).to_string_lossy().to_string();
            // python's platform.platform() -> "Linux-<release>-<machine>-with-glibc<ver>".
            // The binary may be static (musl), so ask the host for its glibc
            let glibc = std::process::Command::new("getconf")
                .arg("GNU_LIBC_VERSION")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().replace("glibc ", ""))
                .filter(|v| !v.is_empty());
            return match glibc {
                Some(v) => format!("{}-{}-{}-with-glibc{}", sys, rel, mach, v),
                None => format!("{}-{}-{}", sys, rel, mach),
            };
        }
    }
    "Linux".into()
}

fn py_val(v: &Value) -> String {
    crate::pyfmt::py_str(v)
}

fn format_asctime() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S,%3f").to_string()
}

fn level_name(l: log::Level) -> &'static str {
    match l {
        log::Level::Error => "ERROR",
        log::Level::Warn => "WARNING",
        log::Level::Info => "INFO",
        log::Level::Debug => "DEBUG",
        log::Level::Trace => "DEBUG",
    }
}

impl LogState {
    fn header(&self) -> String {
        let strtime = chrono::Utc::now().format("%a %b %e %H:%M:%S %Y").to_string();
        let mut h = format!("{} Log Start | {} {}\n", "-".repeat(20), strtime, "-".repeat(20));
        h.push_str(&format!("platform: {}\n", platform_str()));
        let app: Vec<String> = self.app_args.iter().map(|(k, v)| format!("{}: {}", k, py_val(v))).collect();
        h.push_str(&app.join("\n"));
        h.push('\n');
        let lines: Vec<&String> = self.rollover_info.values().filter(|l| !l.is_empty()).collect();
        if !lines.is_empty() {
            h.push_str(&lines.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n"));
            h.push('\n');
        }
        h
    }

    fn write_header(&mut self) {
        if self.structured {
            let mut parts = vec![format!("platform: {}", platform_str())];
            parts.extend(self.app_args.iter().map(|(k, v)| format!("{}: {}", k, py_val(v))));
            parts.extend(self.rollover_info.values().filter(|l| !l.is_empty()).cloned());
            let line = structured_line("INFO", "loghelper.py", "write_header", 93, &parts.join("\n"));
            if let Some(fs) = self.file.as_mut() {
                if let Some(f) = fs.file.as_mut() {
                    let _ = writeln!(f, "{}", line);
                }
            }
            return;
        }
        let h = self.header();
        if let Some(fs) = self.file.as_mut() {
            if let Some(f) = fs.file.as_mut() {
                let _ = f.write_all(h.as_bytes());
            }
        }
    }

    fn do_rollover(&mut self) {
        if self.max_bytes.is_some() {
            if let Some(fs) = self.file.as_mut() {
                // python RotatingFileHandler: log -> log.1 -> log.2
                fs.file = None;
                let numbered = |i: usize| PathBuf::from(format!("{}.{}", fs.path.display(), i));
                for i in (1..LOG_BACKUPS).rev() {
                    let _ = std::fs::rename(numbered(i), numbered(i + 1));
                }
                let _ = std::fs::rename(&fs.path, numbered(1));
                fs.file = OpenOptions::new().create(true).append(true).open(&fs.path).ok();
            }
            self.write_header();
            return;
        }
        if let Some(fs) = self.file.as_mut() {
            fs.file = None;
            let suffix = fs.day.format("%Y-%m-%d").to_string();
            let dest = PathBuf::from(format!("{}.{}", fs.path.display(), suffix));
            if dest.exists() {
                let _ = std::fs::remove_file(&dest);
            }
            let _ = std::fs::rename(&fs.path, &dest);
            // keep 2 backups
            if let (Some(parent), Some(name)) = (fs.path.parent(), fs.path.file_name()) {
                let prefix = format!("{}.", name.to_string_lossy());
                let mut backups: Vec<PathBuf> = std::fs::read_dir(parent)
                    .map(|rd| {
                        rd.filter_map(|e| e.ok())
                            .map(|e| e.path())
                            .filter(|p| {
                                p.file_name()
                                    .map(|n| {
                                        let n = n.to_string_lossy();
                                        n.starts_with(&prefix)
                                            && chrono::NaiveDate::parse_from_str(&n[prefix.len()..], "%Y-%m-%d").is_ok()
                                    })
                                    .unwrap_or(false)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                backups.sort();
                while backups.len() > LOG_BACKUPS {
                    let _ = std::fs::remove_file(backups.remove(0));
                }
            }
            fs.file = OpenOptions::new().create(true).append(true).open(&fs.path).ok();
            fs.day = chrono::Local::now().date_naive();
        }
        self.write_header();
    }
}

fn structured_line(level: &str, file: &str, func: &str, line: u32, msg: &str) -> String {
    json!({
        "time": format_asctime(),
        "level": level,
        "file": file,
        "function": func,
        "line": line,
        "message": msg.trim(),
    })
    .to_string()
}

impl log::Log for MoonLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // Filter noisy dependency logs
        let target = record.target();
        if !target.starts_with("moonruster") {
            return;
        }
        let file = record.file().map(|f| f.rsplit('/').next().unwrap_or(f)).unwrap_or("");
        let file = file.replace(".rs", ".py");
        let func = target.rsplit("::").next().unwrap_or(target);
        let msg = record.args().to_string();
        println!("[{}:{}()] - {}", file, func, msg);
        let mut guard = match STATE.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(state) = guard.as_mut() {
            let today = chrono::Local::now().date_naive();
            let need_rollover = match state.max_bytes {
                Some(max) => state
                    .file
                    .as_ref()
                    .and_then(|f| f.file.as_ref())
                    .and_then(|f| f.metadata().ok())
                    .map(|m| m.len() >= max)
                    .unwrap_or(false),
                None => state.file.as_ref().map(|f| f.day != today).unwrap_or(false),
            };
            if need_rollover {
                state.do_rollover();
            }
            let structured = state.structured;
            if let Some(fs) = state.file.as_mut() {
                if let Some(f) = fs.file.as_mut() {
                    let line = if structured {
                        structured_line(level_name(record.level()), &file, func, record.line().unwrap_or(0), &msg)
                    } else {
                        format!("{} [{}:{}()] - {}", format_asctime(), file, func, msg)
                    };
                    let _ = writeln!(f, "{}", line);
                }
            }
        }
    }

    fn flush(&self) {
        if let Ok(mut g) = STATE.lock() {
            if let Some(s) = g.as_mut() {
                if let Some(fs) = s.file.as_mut() {
                    if let Some(f) = fs.file.as_mut() {
                        let _ = f.flush();
                    }
                }
            }
        }
    }
}

/// Initialize logging, returns a startup warning if the log file could not be created
pub fn init(app_args: &Map<String, Value>) -> Option<String> {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Info);
    let mut app_str = format!("platform: {}\n", platform_str());
    app_str.push_str(&app_args.iter().map(|(k, v)| format!("{}: {}", k, py_val(v))).collect::<Vec<_>>().join("\n"));
    println!("\nApplication Info:\n{}", app_str);
    let log_file = app_args.get("log_file").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let structured = app_args.get("structured_logging").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut warning = None;
    let mut file_state = None;
    if !log_file.is_empty() {
        let path = PathBuf::from(&log_file);
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => {
                let day = std::fs::metadata(&path)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .map(|t| chrono::DateTime::<chrono::Local>::from(t).date_naive())
                    .unwrap_or_else(|| chrono::Local::now().date_naive());
                file_state = Some(FileState { path, file: Some(f), day });
            }
            Err(_) => {
                let norm = crate::utils::normpath(&crate::utils::absolute(&path));
                let dir = norm.parent().map(|p| p.display().to_string()).unwrap_or_default();
                warning = Some(format!(
                    "Unable to create log file at '{}'. Make sure that the folder '{}' exists and Moonraker has Read/Write access to the folder. ",
                    log_file, dir
                ));
            }
        }
    }
    let mut state = LogState { file: file_state, structured, app_args: app_args.clone(), rollover_info: IndexMap::new(), max_bytes: log_max_bytes() };
    if state.file.is_some() {
        state.write_header();
    }
    *STATE.lock().unwrap() = Some(state);
    warning
}

pub fn set_rollover_info(name: &str, item: &str) {
    if let Ok(mut g) = STATE.lock() {
        if let Some(s) = g.as_mut() {
            if s.file.is_some() {
                s.rollover_info.insert(name.to_string(), item.to_string());
            }
        }
    }
}

pub fn rollover_log() -> Result<(), ServerError> {
    let mut g = STATE.lock().map_err(|_| ServerError::new("Log lock poisoned", 500))?;
    match g.as_mut() {
        Some(s) if s.file.is_some() => {
            s.do_rollover();
            Ok(())
        }
        _ => Err(ServerError::bad("File Logging Disabled")),
    }
}

pub fn set_server(server: &Rc<Server>) {
    let _ = server.register_endpoint(
        "/server/logs/rollover",
        RequestType::POST,
        callback(|req| async move { handle_log_rollover(req).await }),
    );
}

async fn handle_log_rollover(req: WebRequest) -> ApiResult {
    let mut log_apps = vec!["moonraker", "klipper"];
    let app = req.get_str_opt("application");
    let mut rolled = Vec::new();
    let mut failed = Map::new();
    if let Some(a) = &app {
        if !log_apps.contains(&a.as_str()) {
            return Err(ServerError::bad(format!("Unknown application {}", a)));
        }
        log_apps = vec![if a == "moonraker" { "moonraker" } else { "klipper" }];
    }
    if log_apps.contains(&"moonraker") {
        match rollover_log() {
            Ok(_) => rolled.push("moonraker"),
            Err(e) => {
                failed.insert("moonraker".into(), Value::String(e.message));
            }
        }
    }
    if log_apps.contains(&"klipper") {
        let kc = crate::server::server().klippy();
        match kc.rollover_log().await {
            Ok(_) => rolled.push("klipper"),
            Err(e) => {
                failed.insert("klipper".into(), Value::String(e.message));
            }
        }
    }
    Ok(json!({"rolled_over": rolled, "failed": failed}))
}

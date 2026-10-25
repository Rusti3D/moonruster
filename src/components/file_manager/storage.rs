// Gcode metadata storage (MetadataStorage from moonraker/components/file_manager/file_manager.py)

use crate::common::ServerError;
use crate::components::database::{MoonrakerDatabase, NamespaceWrapper};
use crate::components::file_manager::{path_join, VALID_GCODE_EXTS};
use crate::confighelper::ConfigHelper;
use crate::server::{server, spawn};
use crate::utils::AsyncEvent;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;

pub const METADATA_NAMESPACE: &str = "gcode_metadata";
pub const METADATA_VERSION: i64 = 3;

pub struct MetadataStorage {
    enable_object_proc: bool,
    default_timeout: f64,
    gc_path: RefCell<String>,
    mddb: NamespaceWrapper,
    metadata: RefCell<IndexMap<String, Value>>,
    pending_requests: RefCell<IndexMap<String, (Map<String, Value>, Rc<AsyncEvent>)>>,
    busy: Cell<bool>,
    processors: RefCell<IndexMap<String, Value>>,
    me: RefCell<std::rc::Weak<MetadataStorage>>,
}

impl MetadataStorage {
    pub fn new(config: &ConfigHelper, db: &Rc<MoonrakerDatabase>) -> Result<Rc<Self>, crate::confighelper::ConfigError> {
        let enable_object_proc = config.getboolean_or("enable_object_processing", false)?;
        let default_timeout = config.getfloat_or("default_metadata_parser_timeout", 20.0)?;
        db.register_local_namespace(METADATA_NAMESPACE, false, false)?;
        let mddb = db.wrap_namespace(METADATA_NAMESPACE, false)?;
        let version = db
            .get_item_sync("moonraker", Some(&json!("file_manager.metadata_version")), Some(json!(0)))
            .unwrap_or(json!(0));
        if version != json!(METADATA_VERSION) {
            let _ = mddb.clear();
            let _ = db.insert_item_sync("moonraker", &json!("file_manager.metadata_version"), json!(METADATA_VERSION));
        }
        let metadata: IndexMap<String, Value> = mddb.as_dict().into_iter().collect();
        let ms = Rc::new(MetadataStorage {
            enable_object_proc,
            default_timeout,
            gc_path: RefCell::new(String::new()),
            mddb,
            metadata: RefCell::new(metadata),
            pending_requests: RefCell::new(IndexMap::new()),
            busy: Cell::new(false),
            processors: RefCell::new(IndexMap::new()),
            me: RefCell::new(std::rc::Weak::new()),
        });
        *ms.me.borrow_mut() = Rc::downgrade(&ms);
        Ok(ms)
    }

    pub fn prune_storage(&self) {
        let gc = self.gc_path.borrow().clone();
        if gc.is_empty() {
            return;
        }
        let mut del_keys = Vec::new();
        let keys: Vec<String> = self.metadata.borrow().keys().cloned().collect();
        for fname in keys {
            let fpath = path_join(&gc, &fname);
            if !Path::new(&fpath).is_file() {
                self.metadata.borrow_mut().shift_remove(&fname);
                del_keys.push(fname);
            } else {
                let mut need_sync = false;
                let mut md = self.metadata.borrow_mut();
                if let Some(thumbs) = md.get_mut(&fname).and_then(|m| m.get_mut("thumbnails")).and_then(|t| t.as_array_mut()) {
                    for t in thumbs.iter_mut() {
                        if let Some(o) = t.as_object_mut() {
                            if o.shift_remove("data").is_some() {
                                need_sync = true;
                            }
                        }
                    }
                }
                if need_sync {
                    let v = md.get(&fname).cloned().unwrap();
                    drop(md);
                    let _ = self.mddb.insert(&fname, v);
                }
            }
        }
        if !del_keys.is_empty() {
            let ret = self.mddb.delete_batch(&del_keys).unwrap_or_default();
            self.remove_thumbs(&ret);
            let pruned: Vec<&String> = ret.keys().collect();
            if !pruned.is_empty() {
                log::info!(
                    "Pruned metadata for the following:\n{}",
                    pruned.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n")
                );
            }
        }
    }

    pub fn update_gcode_path(&self, path: &str) {
        if path == *self.gc_path.borrow() {
            return;
        }
        if !self.gc_path.borrow().is_empty() {
            self.metadata.borrow_mut().clear();
            let _ = self.mddb.clear();
        }
        *self.gc_path.borrow_mut() = path.to_string();
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        self.metadata.borrow().get(key).cloned()
    }

    pub fn insert(&self, key: &str, value: Value) {
        self.metadata.borrow_mut().insert(key.to_string(), value.clone());
        let _ = self.mddb.insert(key, value);
    }

    pub fn is_processing(&self) -> bool {
        !self.pending_requests.borrow().is_empty()
    }

    pub fn is_file_processing(&self, fname: &str) -> bool {
        self.pending_requests.borrow().contains_key(fname)
    }

    /// Replaces the configuration of a registered processor (python
    /// components mutate the registered dict in place)
    pub fn update_gcode_processor(&self, name: &str, config: Value) {
        if let Some(p) = self.processors.borrow_mut().get_mut(name) {
            *p = config;
        }
    }

    pub fn register_gcode_processor(&self, name: &str, config: Option<Value>) -> Result<(), ServerError> {
        let cfg = match config {
            None => {
                self.processors.borrow_mut().shift_remove(name);
                return Ok(());
            }
            Some(c) => c,
        };
        if self.processors.borrow().contains_key(name) {
            return Err(ServerError::bad(format!("File processor {} already registered", name)));
        }
        for f in ["name", "command", "timeout"] {
            if cfg.get(f).is_none() {
                return Err(ServerError::bad(format!("File processor configuration requires a `{}` field", f)));
            }
        }
        self.processors.borrow_mut().insert(name.to_string(), cfg);
        log::info!("GCode Processor {} registered", name);
        Ok(())
    }

    fn has_valid_data(&self, fname: &str, path_info: &Map<String, Value>) -> bool {
        if path_info.get("ufp_path").map(|v| !v.is_null()).unwrap_or(false) {
            return false;
        }
        let md = self.metadata.borrow();
        let default = json!({"size": "", "modified": 0});
        let m = md.get(fname).unwrap_or(&default);
        for field in ["size", "modified"] {
            if m.get(field) != path_info.get(field) {
                return false;
            }
        }
        true
    }

    pub fn remove_directory_metadata(&self, dir_name: &str) {
        let mut dir_name = dir_name.to_string();
        if !dir_name.ends_with('/') {
            dir_name.push('/');
        }
        let mut del = Map::new();
        let keys: Vec<String> = self.metadata.borrow().keys().cloned().collect();
        for f in keys {
            if f.starts_with(&dir_name) {
                if let Some(md) = self.metadata.borrow_mut().shift_remove(&f) {
                    if md.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
                        del.insert(f, md);
                    }
                }
            }
        }
        if !del.is_empty() {
            let keys: Vec<String> = del.keys().cloned().collect();
            let _ = self.mddb.delete_batch(&keys);
            self.remove_thumbs(&del);
        }
    }

    pub fn remove_file_metadata(&self, fname: &str) {
        let md = self.metadata.borrow_mut().shift_remove(fname);
        if let Some(md) = md {
            let _ = self.mddb.pop(fname, Some(Value::Null));
            let mut m = Map::new();
            m.insert(fname.to_string(), md);
            self.remove_thumbs(&m);
        }
    }

    fn remove_thumbs(&self, records: &Map<String, Value>) {
        let gc = self.gc_path.borrow().clone();
        for (fname, md) in records {
            let full = path_join(&gc, fname);
            let fdir = crate::pycompat::posixpath::dirname(&full).to_string();
            if let Some(thumbs) = md.get("thumbnails").and_then(|t| t.as_array()) {
                for t in thumbs {
                    let rel = match t.get("relative_path").and_then(|p| p.as_str()) {
                        Some(r) => r,
                        None => continue,
                    };
                    let tp = path_join(&fdir, rel);
                    if !Path::new(&tp).is_file() {
                        continue;
                    }
                    if std::fs::remove_file(&tp).is_err() {
                        log::debug!("Error removing thumb at {}", tp);
                    }
                }
            }
        }
    }

    pub fn move_directory_metadata(&self, prev_dir: &str, new_dir: &str) {
        let mut prev = prev_dir.to_string();
        if !prev.ends_with('/') {
            prev.push('/');
        }
        let mut src = Vec::new();
        let mut dst = Vec::new();
        let keys: Vec<String> = self.metadata.borrow().keys().cloned().collect();
        for f in keys {
            if f.starts_with(&prev) {
                let new_f = path_join(new_dir, &f[prev.len()..]);
                let md = self.metadata.borrow_mut().shift_remove(&f);
                if let Some(md) = md {
                    self.metadata.borrow_mut().insert(new_f.clone(), md);
                    src.push(f);
                    dst.push(new_f);
                }
            }
        }
        if !src.is_empty() {
            let _ = self.mddb.move_batch(&src, &dst);
        }
    }

    /// Returns Ok(true) when metadata was moved (thumbnail move pending in the
    /// returned future), Ok(false) when a rescan is required
    pub async fn move_file_metadata(&self, prev: &str, new: &str) -> bool {
        let md = self.metadata.borrow_mut().shift_remove(prev);
        let md = match md {
            None => {
                if self.metadata.borrow_mut().shift_remove(new).is_some() {
                    let _ = self.mddb.pop(new, Some(Value::Null));
                }
                return false;
            }
            Some(m) => m,
        };
        self.metadata.borrow_mut().insert(new.to_string(), md.clone());
        let _ = self.mddb.move_batch(&[prev.to_string()], &[new.to_string()]);
        self.move_thumbnails(prev, new, &md).await;
        true
    }

    async fn move_thumbnails(&self, prev: &str, new: &str, md: &Value) {
        let gc = self.gc_path.borrow().clone();
        let pfull = path_join(&gc, prev);
        let nfull = path_join(&gc, new);
        let prev_dir = crate::pycompat::posixpath::dirname(&pfull).to_string();
        let new_dir = crate::pycompat::posixpath::dirname(&nfull).to_string();
        if let Some(thumbs) = md.get("thumbnails").and_then(|t| t.as_array()) {
            for t in thumbs {
                let rel = match t.get("relative_path").and_then(|p| p.as_str()) {
                    Some(r) => r.to_string(),
                    None => continue,
                };
                let tp = path_join(&prev_dir, &rel);
                if !Path::new(&tp).is_file() {
                    continue;
                }
                let np = path_join(&new_dir, &rel);
                let parent = crate::pycompat::posixpath::dirname(&np);
                if !Path::new(parent).exists() {
                    if let Err(e) = std::fs::create_dir(parent) {
                        log::error!("Error moving thumb from {} to {}\n{}", tp, np, e);
                        continue;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                if let Err(e) = crate::components::file_manager::shutil_move(&tp, &np) {
                    log::error!("Error moving thumb from {} to {}\n{}", tp, np, e);
                }
            }
        }
    }

    pub fn parse_metadata(&self, fname: &str, path_info: Map<String, Value>) -> Rc<AsyncEvent> {
        if let Some((_, e)) = self.pending_requests.borrow().get(fname) {
            return e.clone();
        }
        let evt = AsyncEvent::new();
        let ext = crate::utils::splitext_ext(fname);
        if !VALID_GCODE_EXTS.contains(&ext.as_str()) || self.has_valid_data(fname, &path_info) {
            evt.set();
            return evt;
        }
        self.pending_requests.borrow_mut().insert(fname.to_string(), (path_info, evt.clone()));
        if self.busy.get() {
            return evt;
        }
        self.busy.set(true);
        if let Some(me) = self.me.borrow().upgrade() {
            spawn(async move { me.process_metadata_update().await });
        }
        evt
    }

    async fn process_metadata_update(self: Rc<Self>) {
        loop {
            let first = self.pending_requests.borrow().get_index(0).map(|(k, (p, e))| (k.clone(), p.clone(), e.clone()));
            let (fname, path_info, evt) = match first {
                Some(f) => f,
                None => break,
            };
            if self.has_valid_data(&fname, &path_info) {
                // python leaves the request pending, which loops until it is
                // processed elsewhere; remove it to avoid spinning
                self.pending_requests.borrow_mut().shift_remove(&fname);
                evt.set();
                continue;
            }
            let ufp = path_info.get("ufp_path").and_then(|v| v.as_str()).map(String::from);
            let mut retries = 3;
            let mut ok = false;
            while retries > 0 {
                match self.run_extract_metadata(&fname, ufp.as_deref()).await {
                    Ok(_) => {
                        server().send_event("file_manager:metadata_processed", vec![json!(fname)]).await;
                        ok = true;
                        break;
                    }
                    Err(e) => {
                        log::error!("Error running extract_metadata.py\n{}", e);
                        retries -= 1;
                    }
                }
            }
            if !ok {
                if ufp.is_none() {
                    let md = json!({
                        "size": path_info.get("size").cloned().unwrap_or(json!(0)),
                        "modified": path_info.get("modified").cloned().unwrap_or(json!(0)),
                        "print_start_time": null,
                        "job_id": null,
                    });
                    self.insert(&fname, md);
                }
                log::info!("Unable to extract metadata from file: {}", fname);
            }
            self.pending_requests.borrow_mut().shift_remove(&fname);
            evt.set();
        }
        self.busy.set(false);
    }

    async fn run_extract_metadata(&self, filename: &str, ufp_path: Option<&str>) -> Result<(), ServerError> {
        let processors: Vec<Value> = self.processors.borrow().values().cloned().collect();
        let config = json!({
            "filename": filename,
            "gcode_dir": self.gc_path.borrow().clone(),
            "check_objects": self.enable_object_proc,
            "ufp_path": ufp_path,
            "processors": processors,
        });
        let mut timeout = self.default_timeout;
        if ufp_path.is_some() || self.enable_object_proc {
            timeout = timeout.max(300.0);
        }
        if !processors.is_empty() {
            let pt: f64 = processors.iter().map(|p| p.get("timeout").and_then(|t| t.as_f64()).unwrap_or(0.0)).sum();
            timeout = timeout.max(pt);
        }
        let cfg_path = std::env::temp_dir().join(format!("metacfg-{}.json", uuid::Uuid::new_v4().simple()));
        std::fs::write(&cfg_path, config.to_string())?;
        // a component's replacement METADATA_SCRIPT runs as Moonraker runs it:
        // `sys.executable METADATA_SCRIPT -c <cfg>`
        #[cfg(feature = "plugins")]
        let script = crate::plugins::metadata_script_override();
        #[cfg(not(feature = "plugins"))]
        let script: Option<String> = None;
        let cmd = match script {
            Some(prefix) => format!("{} -c {}", prefix, shell_quote(&cfg_path.to_string_lossy())),
            None => {
                let exe = std::env::current_exe().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| "moonruster".into());
                format!("{} --metadata -c {}", shell_quote(&exe), shell_quote(&cfg_path.to_string_lossy()))
            }
        };
        let result: Rc<RefCell<Vec<u8>>> = Rc::new(RefCell::new(Vec::new()));
        let r2 = result.clone();
        let shell: Rc<crate::components::shell_command::ShellCommandFactory> =
            server().get::<crate::components::shell_command::ShellCommandFactory>().ok_or_else(|| ServerError::new("shell_command not loaded", 500))?;
        let scmd = shell.build_shell_command(&cmd, Some(Rc::new(move |line: Vec<u8>| r2.borrow_mut().extend(line))), None, None, true, None)?;
        let ok = scmd.run(&shell, timeout, true, true, 1, None, None).await;
        let _ = std::fs::remove_file(&cfg_path);
        if !ok {
            return Err(ServerError::bad("Extract Metadata returned with error"));
        }
        let raw = result.borrow().clone();
        let decoded: Value = serde_json::from_slice(String::from_utf8_lossy(&raw).trim().as_bytes()).map_err(|e| {
            log::debug!("Invalid metadata response:\n{}", String::from_utf8_lossy(&raw));
            ServerError::new(e.to_string(), 500)
        })?;
        let path = decoded.get("file").and_then(|f| f.as_str()).ok_or_else(|| ServerError::new("'file'", 500))?.to_string();
        let mut md = decoded.get("metadata").and_then(|m| m.as_object()).cloned().unwrap_or_default();
        if md.is_empty() {
            return Err(ServerError::bad("Unable to extract metadata"));
        }
        md.insert("print_start_time".into(), Value::Null);
        md.insert("job_id".into(), Value::Null);
        self.insert(&path, Value::Object(md));
        Ok(())
    }
}

/// python's shlex.quote
pub fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

// Enhanced gcode file management and analysis
// (port of moonraker/components/file_manager/file_manager.py)

pub mod observer;
pub mod storage;
pub mod upload;

use crate::common::*;
use crate::confighelper::ConfigHelper;
use crate::server::{server, spawn, Component};
use crate::utils::{self, AsyncEvent};
use async_trait::async_trait;
use indexmap::{IndexMap, IndexSet};
use observer::Observer;
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
pub use storage::MetadataStorage;

pub const VALID_GCODE_EXTS: [&str; 5] = [".gcode", ".g", ".gco", ".ufp", ".nc"];

/// python's os.stat().st_mtime
pub fn st_mtime(meta: &std::fs::Metadata) -> f64 {
    use std::os::unix::fs::MetadataExt;
    meta.mtime() as f64 + meta.mtime_nsec() as f64 * 1e-9
}

// Python's os.path, as Moonraker's file manager uses it
pub use crate::pycompat::posixpath::{abspath, normpath as os_normpath, relpath};

/// os.path.join of two components
pub fn path_join(a: &str, b: &str) -> String {
    crate::pycompat::posixpath::join(a, &[b])
}


// ***** Notify Sync Lock *****

#[derive(Default)]
struct SyncState {
    action: String,
    dest_path: Option<PathBuf>,
    check_pending: bool,
    move_copy_evt: Option<Rc<AsyncEvent>>,
    move_copy_interrupted: bool,
    sync_waiters: Vec<tokio::sync::oneshot::Sender<()>>,
    pending_paths: IndexSet<PathBuf>,
    acquired_paths: IndexSet<PathBuf>,
}

pub struct NotifySyncLock {
    mutex: tokio::sync::Mutex<()>,
    locked: Cell<bool>,
    state: RefCell<SyncState>,
}

pub struct SyncGuard<'a> {
    _guard: tokio::sync::MutexGuard<'a, ()>,
    lock: &'a NotifySyncLock,
}

impl Drop for SyncGuard<'_> {
    fn drop(&mut self) {
        self.lock.locked.set(false);
        self.lock.finish();
    }
}

impl NotifySyncLock {
    fn new() -> Rc<Self> {
        Rc::new(NotifySyncLock { mutex: tokio::sync::Mutex::new(()), locked: Cell::new(false), state: RefCell::new(SyncState::default()) })
    }

    pub async fn acquire(&self) -> SyncGuard<'_> {
        let g = self.mutex.lock().await;
        self.locked.set(true);
        SyncGuard { _guard: g, lock: self }
    }

    pub fn setup(&self, action: &str, path: &Path, move_copy: bool) -> Result<(), ServerError> {
        if !self.locked.get() {
            return Err(ServerError::bad("Cannot call setup unless the lock has been acquired"));
        }
        if self.state.borrow().dest_path.is_some() {
            log::debug!("NotifySync Error: Setup requested while a path is still pending");
            self.finish();
        }
        let mut st = self.state.borrow_mut();
        st.dest_path = Some(path.to_path_buf());
        st.action = action.to_string();
        st.check_pending = move_copy;
        Ok(())
    }

    pub async fn wait_inotify_event(&self, current_path: &Path) {
        if !self.state.borrow().check_pending {
            return;
        }
        self.state.borrow_mut().dest_path = Some(current_path.to_path_buf());
        if self.state.borrow().acquired_paths.contains(current_path) {
            return;
        }
        let evt = AsyncEvent::new();
        {
            let mut st = self.state.borrow_mut();
            st.move_copy_evt = Some(evt.clone());
            st.move_copy_interrupted = false;
        }
        let mut has_pending = self.state.borrow().pending_paths.contains(current_path);
        let mut timeout = if has_pending { 1200.0 } else { 1.0 };
        let mut ok = false;
        for _ in 0..5 {
            match tokio::time::timeout(std::time::Duration::from_secs_f64(timeout), evt.wait()).await {
                Ok(_) => {
                    // wait_for(shield(fut)) resumes a couple of loop iterations
                    // after the future is set, letting the observer queue its
                    // other notifications before the request finishes
                    tokio::task::yield_now().await;
                    tokio::task::yield_now().await;
                    ok = true;
                    break;
                }
                Err(_) => {
                    if timeout > 2.0 {
                        ok = true;
                        break;
                    }
                    has_pending = self.state.borrow().pending_paths.contains(current_path);
                    timeout = if has_pending { 1200.0 } else { 1.0 };
                }
            }
        }
        if !ok {
            log::info!("Failed to receive an inotify event, dest path: {}", current_path.display());
        }
        self.state.borrow_mut().move_copy_evt = None;
    }

    pub fn finish(&self) {
        let mut st = self.state.borrow_mut();
        for w in st.sync_waiters.drain(..) {
            let _ = w.send(());
        }
        st.dest_path = None;
        st.action.clear();
        st.pending_paths.clear();
        st.acquired_paths.clear();
        if let Some(e) = st.move_copy_evt.take() {
            st.move_copy_interrupted = true;
            e.set();
        }
        st.check_pending = false;
    }

    pub fn add_pending_path(&self, action: &str, pending: &Path) {
        let mut st = self.state.borrow_mut();
        let dest = match &st.dest_path {
            Some(d) if st.check_pending && action == st.action => d.clone(),
            _ => return,
        };
        if dest == pending || Some(dest.as_path()) == pending.parent() {
            st.pending_paths.insert(pending.to_path_buf());
        }
    }

    pub fn check_in_request(&self, action: &str, path: &Path) -> Option<tokio::sync::oneshot::Receiver<()>> {
        let mut st = self.state.borrow_mut();
        let dest = st.dest_path.clone()?;
        let mut need_waiter = false;
        if st.check_pending {
            if let Some(evt) = &st.move_copy_evt {
                if dest == path {
                    evt.set();
                    need_waiter = true;
                }
            } else if dest == path || Some(dest.as_path()) == path.parent() {
                st.acquired_paths.insert(path.to_path_buf());
                need_waiter = true;
            }
        } else if dest == path {
            need_waiter = true;
        }
        if !need_waiter {
            return None;
        }
        if action != st.action {
            log::info!(
                "\nInotify action mismatch:\nExpected action: {}, Inotify action: {}\nRequested path: {}\nInotify path: {}\nIs move/copy: {}",
                st.action,
                action,
                dest.display(),
                path.display(),
                crate::pyfmt::py_bool(st.check_pending)
            );
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        st.sync_waiters.push(tx);
        Some(rx)
    }
}

// ***** File Manager *****

pub struct FileManager {
    reserved_paths: RefCell<IndexMap<String, (PathBuf, bool)>>,
    full_access_roots: RefCell<IndexSet<String>>,
    file_paths: RefCell<IndexMap<String, String>>,
    datapath: PathBuf,
    pub gcode_metadata: Rc<MetadataStorage>,
    pub sync_lock: Rc<NotifySyncLock>,
    pub observer: Rc<Observer>,
    scheduled_notifications: RefCell<HashMap<String, tokio::task::JoinHandle<()>>>,
    fixed_path_args: RefCell<Map<String, Value>>,
    queue_gcodes: bool,
    check_klipper_path: bool,
}

#[async_trait(?Send)]
impl Component for FileManager {
    async fn close(self: Rc<Self>) {
        for (_, h) in self.scheduled_notifications.borrow_mut().drain() {
            h.abort();
        }
        self.observer.close();
    }
}

macro_rules! fm_endpoint {
    ($srv:expr, $fm:expr, $path:expr, $rt:expr, $method:ident) => {{
        let f = $fm.clone();
        $srv.register_endpoint($path, $rt, callback(move |req: WebRequest| {
            let f = f.clone();
            async move { f.$method(req).await }
        }))?;
    }};
}

impl FileManager {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, crate::confighelper::ConfigError> {
        let srv = server();
        let datapath = PathBuf::from(srv.data_path());
        srv.load_component(config, "database")?;
        let db = srv.database();
        let gcode_metadata = MetadataStorage::new(config, &db)?;
        let sync_lock = NotifySyncLock::new();
        let observer_name = config.get_or("file_system_observer", "inotify")?.to_lowercase();
        let mut use_inotify = observer_name == "inotify";
        if observer_name != "inotify" && observer_name != "none" {
            srv.add_warning(
                &format!(
                    "[file_manager]: Invalid value '{}' for option 'file_system_observer'.  Falling back to no observer.",
                    observer_name
                ),
                None,
                true,
            );
            use_inotify = false;
        }
        if observer_name == "none" {
            log::info!("File System Observation is disabled");
        } else {
            log::info!("Using File System Observer: {}", observer_name);
        }
        let observer = Observer::new(config, use_inotify)?;
        let fm = Rc::new(FileManager {
            reserved_paths: RefCell::new(IndexMap::new()),
            full_access_roots: RefCell::new(IndexSet::new()),
            file_paths: RefCell::new(IndexMap::new()),
            datapath: datapath.clone(),
            gcode_metadata,
            sync_lock,
            observer,
            scheduled_notifications: RefCell::new(HashMap::new()),
            fixed_path_args: RefCell::new(Map::new()),
            queue_gcodes: config.getboolean_or("queue_gcode_uploads", false)?,
            check_klipper_path: config.getboolean_or("check_klipper_config_path", true)?,
        });
        fm.observer.set_file_manager(&fm);
        let srcdir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("/usr/local/bin"));
        fm.add_reserved_path("moonraker", &srcdir, false);
        fm.add_reserved_path("database", Path::new(&db.get_database_path()), false);
        fm.add_reserved_path("certs", &datapath.join("certs"), false);
        fm.add_reserved_path("systemd", &datapath.join("systemd"), false);
        fm.add_reserved_path("backup", &datapath.join("backup"), false);
        fm_endpoint!(srv, fm, "/server/files/list", RequestType::GET, handle_filelist_request);
        fm_endpoint!(srv, fm, "/server/files/metadata", RequestType::GET, handle_metadata_request);
        fm_endpoint!(srv, fm, "/server/files/metascan", RequestType::POST, handle_metascan_request);
        fm_endpoint!(srv, fm, "/server/files/thumbnails", RequestType::GET, handle_list_thumbs);
        fm_endpoint!(srv, fm, "/server/files/roots", RequestType::GET, handle_list_roots);
        fm_endpoint!(srv, fm, "/server/files/directory", RequestType::all(), handle_directory_request);
        fm_endpoint!(srv, fm, "/server/files/move", RequestType::POST, handle_file_move_copy);
        fm_endpoint!(srv, fm, "/server/files/copy", RequestType::POST, handle_file_move_copy);
        fm_endpoint!(srv, fm, "/server/files/zip", RequestType::POST, handle_zip_files);
        let f = fm.clone();
        srv.register_endpoint_ext(
            "/server/files/delete_file",
            RequestType::DELETE,
            callback(move |req: WebRequest| {
                let f = f.clone();
                async move {
                    let path = req.get_str("path")?;
                    f.delete_file(&path).await
                }
            }),
            TransportType::WEBSOCKET,
            true,
            None,
            true,
        )?;
        srv.register_notification("file_manager:filelist_changed", None);
        let f = Rc::downgrade(&fm);
        srv.on_event("server:klippy_identified", move |_| {
            if let Some(fm) = f.upgrade() {
                fm.update_fixed_paths();
            }
        });
        srv.load_component(config, "secrets")?;
        let secrets: Rc<crate::components::secrets::Secrets> = srv.lookup_component("secrets").unwrap();
        fm.add_reserved_path("secrets", &secrets.get_secrets_file(), false);
        config.get_opt_dep("config_path", true)?;
        let cfg_writable = config.getboolean_or("enable_config_write_access", true)?;
        fm.register_data_folder("config", cfg_writable);
        config.get_opt_dep("log_path", true)?;
        fm.register_data_folder("logs", false);
        let gc_path = fm.register_data_folder("gcodes", true);
        if gc_path.is_dir() {
            let mut prune = true;
            let saved = db
                .get_item_sync("moonraker", Some(&json!("file_manager.gcode_path")), Some(json!("")))
                .unwrap_or(json!(""));
            let saved = crate::pyfmt::py_str(&saved);
            let is_empty = std::fs::read_dir(&gc_path).map(|mut r| r.next().is_none()).unwrap_or(true);
            if is_empty && !saved.is_empty() {
                let sp = PathBuf::from(&saved);
                if sp.is_dir() && std::fs::read_dir(&sp).map(|mut r| r.next().is_some()).unwrap_or(false) {
                    log::info!("Legacy GCode Path found at '{}', aborting metadata prune", sp.display());
                    prune = false;
                }
            }
            if prune {
                fm.gcode_metadata.prune_storage();
            }
        }
        Ok(fm)
    }

    pub fn start_file_observer(self: &Rc<Self>) {
        self.observer.initialize();
    }

    fn update_fixed_paths(self: &Rc<Self>) {
        let kinfo = server().get_klippy_info();
        let mut paths = Map::new();
        for k in ["klipper_path", "python_path", "log_file", "config_file"] {
            paths.insert(k.into(), kinfo.get(k).cloned().unwrap_or(Value::Null));
        }
        if paths == *self.fixed_path_args.borrow() {
            return;
        }
        *self.fixed_path_args.borrow_mut() = paths.clone();
        let s: Vec<String> = paths.iter().map(|(k, v)| format!("{}: {}", k, crate::pyfmt::py_str(v))).collect();
        log::debug!("\nUpdating Fixed Paths:\n{}", s.join("\n"));
        if let Some(kp) = paths.get("klipper_path").and_then(|v| v.as_str()) {
            self.reserved_paths.borrow_mut().shift_remove("klipper");
            self.add_reserved_path("klipper", Path::new(kp), true);
            self.register_directory("config_examples", Some(&path_join(kp, "config")), false);
            self.register_directory("docs", Some(&path_join(kp, "docs")), false);
        }
        if let Some(lf) = paths.get("log_file").and_then(|v| v.as_str()) {
            let lp = abspath(&utils::expand_user(lf).to_string_lossy());
            server().app().register_static_file_handler("klippy.log", &lp, true);
        }
        if self.check_klipper_path {
            let cfg_file = paths.get("config_file").and_then(|v| v.as_str()).map(String::from);
            let cfg_parent = self.file_paths.borrow().get("config").cloned();
            if let (Some(cf), Some(cp)) = (cfg_file, cfg_parent) {
                let cfg_path = utils::expand_user(&cf);
                let par = PathBuf::from(&cp);
                let in_parents = cfg_path.ancestors().skip(1).any(|a| a == par)
                    || utils::resolve_path(&cfg_path).ancestors().skip(1).any(|a| a == utils::resolve_path(&par));
                if in_parents {
                    server().remove_warning("klipper_config");
                } else {
                    server().add_warning(
                        &format!(
                            "file_manager: Klipper configuration file not located in 'config' folder.\n\nKlipper Config Path: {}\n\nConfig Folder: {}",
                            cfg_path.display(),
                            par.display()
                        ),
                        Some("klipper_config"),
                        true,
                    );
                }
            }
        }
    }

    pub fn validate_gcode_path(&self, gc_path: &str) {
        let gc_dir = utils::expand_user(gc_path);
        let expected = match self.file_paths.borrow().get("gcodes").cloned() {
            Some(e) => e,
            None => return,
        };
        let same = gc_dir.exists() && std::fs::canonicalize(&gc_dir).ok() == std::fs::canonicalize(&expected).ok();
        if !same {
            server().add_warning(
                &format!(
                    "GCode path received from Klipper does not match expected location.\n\nReceived: '{}'\nExpected: '{}'\n\nModify the [virtual_sdcard] section Klipper's configuration to correct this error.\n\n[virtual_sdcard]\npath: {}",
                    gc_dir.display(),
                    expected,
                    expected
                ),
                Some("gcode_path"),
                true,
            );
        } else {
            server().remove_warning("gcode_path");
        }
    }

    pub fn register_data_folder(self: &Rc<Self>, folder_name: &str, full_access: bool) -> PathBuf {
        let new_path = self.datapath.join(folder_name);
        if !new_path.exists() {
            let _ = std::fs::create_dir(&new_path);
        }
        self.register_directory(folder_name, Some(&new_path.to_string_lossy()), full_access);
        new_path
    }

    pub fn disable_write_access(&self) {
        self.full_access_roots.borrow_mut().clear();
    }

    pub fn check_write_enabled(&self) -> Result<(), ServerError> {
        if self.full_access_roots.borrow().is_empty() {
            return Err(ServerError::bad("Write access is currently disabled.  Check notifications for warnings."));
        }
        Ok(())
    }

    pub fn register_directory(self: &Rc<Self>, root: &str, path: Option<&str>, full_access: bool) -> bool {
        let path = match path {
            Some(p) => p,
            None => return false,
        };
        let mut path = abspath(&utils::expand_user(path).to_string_lossy());
        if Path::new(&path).is_symlink() {
            path = std::fs::canonicalize(&path).map(|p| p.to_string_lossy().to_string()).unwrap_or(path);
        }
        if !Path::new(&path).is_dir() || path == "/" {
            server().add_warning(
                &format!(
                    "Supplied path ({}) for ({}) is invalid. Make sure\nthat the path exists and is not the file system root.",
                    path, root
                ),
                None,
                true,
            );
            return false;
        }
        let mut missing = Vec::new();
        match std::fs::read_dir(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => missing.push("READ"),
            Err(e) => log::error!("Error testing read access for root {}\n{}", root, e),
            Ok(_) => {}
        }
        if full_access {
            if !utils::access_write(Path::new(&path)) {
                missing.push("WRITE");
            }
            self.full_access_roots.borrow_mut().insert(root.to_string());
        }
        if !missing.is_empty() {
            server().add_log_rollover_item(
                &format!("fm_reg_perms_{}", root),
                &format!(
                    "file_manager: Moonraker has detected the following missing permissions for root folder '{}': {}",
                    root,
                    missing.join(" | ")
                ),
                true,
            );
        }
        let cur = self.file_paths.borrow().get(root).cloned().unwrap_or_default();
        if path != cur {
            self.file_paths.borrow_mut().insert(root.to_string(), path.clone());
            server().app().register_static_file_handler(root, &path, false);
            if root == "gcodes" {
                self.gcode_metadata.update_gcode_path(&path);
            }
            if full_access {
                self.observer.add_root_watch(root, &path);
            } else if server().is_running() {
                self.sched_changed_event("root_update", root, &path, None, None, true);
            }
        }
        true
    }

    pub fn check_reserved_path(&self, req_path: &str, need_write: bool) -> Result<(), ServerError> {
        if self.check_reserved_path_inner(req_path, need_write, true)? {
            // unreachable with raise_error
        }
        Ok(())
    }

    pub fn is_reserved_path(&self, req_path: &str, need_write: bool) -> bool {
        self.check_reserved_path_inner(req_path, need_write, false).unwrap_or(true)
    }

    fn check_reserved_path_inner(&self, req_path: &str, need_write: bool, raise_error: bool) -> Result<bool, ServerError> {
        let rp = utils::resolve_path(&utils::expand_user(req_path));
        if rp.components().any(|c| c.as_os_str() == ".git") {
            if raise_error {
                return Err(ServerError::new("Access to .git folders is forbidden", 403));
            }
            return Ok(true);
        }
        for (name, (res_path, can_read)) in self.reserved_paths.borrow().iter() {
            let contained = res_path == &rp || rp.ancestors().skip(1).any(|a| a == res_path);
            if contained && (need_write || !can_read) {
                if !raise_error {
                    return Ok(true);
                }
                let fname = rp.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                return Err(ServerError::new(format!("Access to file {} forbidden by reserved path '{}'", fname, name), 403));
            }
        }
        Ok(false)
    }

    pub fn add_reserved_path(&self, name: &str, res_path: &Path, read_access: bool) -> bool {
        if self.reserved_paths.borrow().contains_key(name) {
            return false;
        }
        let rp = utils::resolve_path(&utils::expand_user(&res_path.to_string_lossy()));
        self.reserved_paths.borrow_mut().insert(name.to_string(), (rp, read_access));
        true
    }

    pub fn get_directory(&self, root: &str) -> String {
        self.file_paths.borrow().get(root).cloned().unwrap_or_default()
    }

    /// file_paths: root -> path
    pub fn file_paths(&self) -> Map<String, Value> {
        self.file_paths.borrow().iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect()
    }

    pub fn full_access_roots(&self) -> Vec<String> {
        self.full_access_roots.borrow().iter().cloned().collect()
    }

    pub fn get_registered_dirs(&self) -> Vec<String> {
        self.file_paths.borrow().keys().cloned().collect()
    }

    pub fn get_relative_path(&self, root: &str, full_path: &str) -> String {
        match self.file_paths.borrow().get(root) {
            Some(rd) if full_path.starts_with(rd.as_str()) => relpath(full_path, rd),
            _ => String::new(),
        }
    }

    pub fn get_full_path(&self, root: &str, relative: &str) -> Result<PathBuf, ServerError> {
        let rd = self.file_paths.borrow().get(root).cloned().ok_or_else(|| ServerError::bad(format!("Unknown root {}", root)))?;
        Ok(PathBuf::from(rd).join(relative))
    }

    pub fn get_metadata_storage(&self) -> Rc<MetadataStorage> {
        self.gcode_metadata.clone()
    }

    pub fn check_file_exists(&self, root: &str, filename: &str, modified: Option<f64>) -> bool {
        let rd = match self.file_paths.borrow().get(root) {
            Some(r) => r.clone(),
            None => return false,
        };
        let fp = PathBuf::from(rd).join(filename);
        if fp.is_file() {
            return match modified {
                None => true,
                Some(m) => std::fs::metadata(&fp).map(|meta| is_close(st_mtime(&meta), m)).unwrap_or(false),
            };
        }
        false
    }

    pub fn can_access_path(&self, path: &str) -> bool {
        let p = utils::resolve_path(&utils::expand_user(path));
        for reg in self.file_paths.borrow().values() {
            let rr = utils::resolve_path(Path::new(reg));
            if p.ancestors().skip(1).any(|a| a == rr) {
                return !self.is_reserved_path(&p.to_string_lossy(), false);
            }
        }
        false
    }

    pub fn get_fixed_path_args(&self) -> Map<String, Value> {
        self.fixed_path_args.borrow().clone()
    }

    pub fn upload_queue_enabled(&self) -> bool {
        self.queue_gcodes
    }

    pub fn full_access(&self, root: &str) -> bool {
        self.full_access_roots.borrow().contains(root)
    }

    async fn handle_filelist_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let root = req.get_str_or("root", "gcodes");
        Ok(Value::Array(self.get_file_list_formatted(&root)?))
    }

    async fn handle_metadata_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let f = req.get_str("filename")?;
        let mut md = self
            .gcode_metadata
            .get(&f)
            .ok_or_else(|| ServerError::new(format!("Metadata not available for <{}>", f), 404))?;
        if let Some(o) = md.as_object_mut() {
            o.insert("filename".into(), json!(f));
        }
        Ok(md)
    }

    async fn handle_metascan_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let _g = self.sync_lock.acquire().await;
        let f = req.get_str("filename")?;
        let gcpath = PathBuf::from(self.get_directory("gcodes")).join(&f);
        if !gcpath.is_file() {
            return Err(ServerError::new(format!("File '{}' does not exist", f), 404));
        }
        let suffix = crate::pycompat::pathlib::path_suffix(&gcpath);
        if !VALID_GCODE_EXTS.contains(&suffix.as_str()) {
            return Err(ServerError::bad(format!("File {} is not a valid gcode file", gcpath.display())));
        }
        self.gcode_metadata.remove_file_metadata(&f);
        let info = self.get_path_info(&gcpath.to_string_lossy(), "gcodes", true)?;
        let evt = self.gcode_metadata.parse_metadata(&f, info);
        evt.wait().await;
        let mut md = self
            .gcode_metadata
            .get(&f)
            .ok_or_else(|| ServerError::new(format!("Failed to parse metadata for file '{}'", f), 500))?;
        if let Some(o) = md.as_object_mut() {
            o.insert("filename".into(), json!(f));
        }
        Ok(md)
    }

    async fn handle_list_roots(self: Rc<Self>, _req: WebRequest) -> ApiResult {
        let list: Vec<Value> = self
            .file_paths
            .borrow()
            .iter()
            .map(|(name, path)| json!({"name": name, "path": path, "permissions": if self.full_access(name) { "rw" } else { "r" }}))
            .collect();
        Ok(Value::Array(list))
    }

    async fn handle_list_thumbs(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let f = req.get_str("filename")?;
        let md = match self.gcode_metadata.get(&f) {
            Some(m) => m,
            None => return Ok(json!([])),
        };
        let mut thumbs = match md.get("thumbnails").and_then(|t| t.as_array()) {
            Some(t) => t.clone(),
            None => return Ok(json!([])),
        };
        let parent = Path::new(&f).parent().map(|p| p.to_path_buf()).unwrap_or_default();
        for info in thumbs.iter_mut() {
            if let Some(o) = info.as_object_mut() {
                if let Some(rel) = o.shift_remove("relative_path") {
                    let tp = parent.join(crate::pyfmt::py_str(&rel));
                    o.insert("thumbnail_path".into(), json!(tp.to_string_lossy()));
                }
            }
        }
        Ok(Value::Array(thumbs))
    }

    async fn handle_directory_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let directory = req.get_str_or("path", "gcodes");
        let (root, dir_path) = self.convert_request_path(&directory)?;
        let rt = req.get_request_type();
        if rt == RequestType::GET {
            let ext = req.get_boolean_or("extended", false)?;
            return self.list_directory(&dir_path, &root, ext).map(Value::Object);
        }
        let _g = self.sync_lock.acquire().await;
        self.check_reserved_path(&dir_path, true)?;
        let action;
        if rt == RequestType::POST && self.full_access(&root) {
            action = "create_dir";
            self.sync_lock.setup("create_dir", Path::new(&dir_path), false)?;
            std::fs::create_dir(&dir_path).map_err(|e| ServerError::bad(py_os_error(&e, &dir_path)))?;
            self.observer.on_item_create(&root, &dir_path, true);
        } else if rt == RequestType::DELETE && self.full_access(&root) {
            action = "delete_dir";
            if directory.trim_matches('/') == root {
                return Err(ServerError::bad("Cannot delete root directory"));
            }
            if !Path::new(&dir_path).is_dir() {
                return Err(ServerError::bad(format!("Directory does not exist ({})", directory)));
            }
            self.sync_lock.setup("delete_dir", Path::new(&dir_path), false)?;
            if req.get_boolean_or("force", false)? {
                self.handle_operation_check(&dir_path)?;
                let dp = dir_path.clone();
                tokio::task::spawn_blocking(move || std::fs::remove_dir_all(dp))
                    .await
                    .map_err(|e| ServerError::exc(e.to_string()))?
                    .map_err(|e| ServerError::exc(py_os_error(&e, &dir_path)))?;
            } else {
                std::fs::remove_dir(&dir_path).map_err(|e| ServerError::bad(py_os_error(&e, &dir_path)))?;
            }
            self.observer.on_item_delete(&root, &dir_path, true);
        } else {
            return Err(ServerError::new("Operation Not Supported", 405));
        }
        Ok(self.sched_changed_event(action, &root, &dir_path, None, None, false))
    }

    pub fn handle_operation_check(&self, requested_path: &str) -> Result<bool, ServerError> {
        if self.get_relative_path("gcodes", requested_path).is_empty() {
            return Ok(true);
        }
        let srv = server();
        let kconn = srv.klippy();
        let stats = srv.lookup_component::<crate::components::job_state::JobState>("job_state").map(|j| j.get_last_stats()).unwrap_or_default();
        let loaded = stats.get("filename").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let state = stats.get("state").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let gc_path = self.get_directory("gcodes");
        let full_path = path_join(&gc_path, &loaded);
        let is_printing = kconn.is_ready() && (state == "printing" || state == "paused");
        if !loaded.is_empty() && is_printing {
            if Path::new(requested_path).is_dir() {
                if full_path.starts_with(requested_path) {
                    return Err(ServerError::new("File currently in use", 403));
                }
            } else if full_path == requested_path {
                return Err(ServerError::new("File currently in use", 403));
            }
        }
        Ok(!is_printing)
    }

    pub fn convert_request_path(&self, request_path: &str) -> Result<(String, String), ServerError> {
        let norm = os_normpath(request_path);
        let trimmed = norm.trim_matches('/');
        let mut parts = trimmed.splitn(2, '/');
        let root = parts.next().unwrap_or("").to_string();
        let rest = parts.next();
        let root_path = self
            .file_paths
            .borrow()
            .get(&root)
            .cloned()
            .ok_or_else(|| ServerError::bad(format!("Invalid root path ({})", root)))?;
        let mut dest = root_path.clone();
        if let Some(r) = rest {
            dest = abspath(&path_join(&dest, r));
            if !dest.starts_with(&root_path) {
                return Err(ServerError::bad(format!(
                    "Invalid path request, '{}'' is outside root '{}'",
                    request_path, root
                )));
            }
        }
        Ok((root, dest))
    }

    async fn handle_file_move_copy(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let source = req.get_str("source")?;
        let destination = req.get_str("dest")?;
        let ep = req.get_endpoint().to_string();
        let (src_root, src_path) = self.convert_request_path(&source)?;
        let (dest_root, dest_path) = self.convert_request_path(&destination)?;
        if !self.full_access(&dest_root) {
            return Err(ServerError::bad(format!("Destination path is read-only: {}", dest_root)));
        }
        self.check_reserved_path(&src_path, false)?;
        self.check_reserved_path(&dest_path, true)?;
        let _g = self.sync_lock.acquire().await;
        if !Path::new(&src_path).exists() {
            return Err(ServerError::bad(format!("File {} does not exist", src_path)));
        }
        if Path::new(&dest_path).exists() {
            self.handle_operation_check(&dest_path)?;
        }
        let mut src_info: (Option<String>, Option<String>) = (None, None);
        let action;
        let is_move;
        if ep == "/server/files/move" {
            if !self.full_access(&src_root) {
                return Err(ServerError::bad(format!("Source path is read-only, cannot move: {}", src_root)));
            }
            self.handle_operation_check(&src_path)?;
            action = if Path::new(&src_path).is_dir() { "move_dir" } else { "move_file" };
            src_info = (Some(src_root.clone()), Some(src_path.clone()));
            is_move = true;
        } else if ep == "/server/files/copy" {
            is_move = false;
            if Path::new(&src_path).is_dir() {
                action = "create_dir";
            } else {
                let base = crate::pycompat::posixpath::basename(&src_path).to_string();
                action = if Path::new(&dest_path).is_file() || Path::new(&path_join(&dest_path, &base)).is_file() {
                    "modify_file"
                } else {
                    "create_file"
                };
            }
        } else {
            return Err(ServerError::bad(format!("Invalid endpoint {}", ep)));
        }
        self.sync_lock.setup(action, Path::new(&dest_path), true)?;
        let (s, d) = (src_path.clone(), dest_path.clone());
        let is_dir_copy = action == "create_dir";
        let res = tokio::task::spawn_blocking(move || {
            if is_move {
                shutil_move(&s, &d)
            } else if is_dir_copy {
                shutil_copytree(&s, &d)
            } else {
                shutil_copy2(&s, &d)
            }
        })
        .await
        .map_err(|e| ServerError::bad(e.to_string()))?;
        let full_dest = res.map_err(ServerError::bad)?;
        if dest_root == "gcodes" && self.observer.has_fast_observe() {
            self.sync_lock.wait_inotify_event(Path::new(&full_dest)).await;
        }
        if action.starts_with("move") {
            self.observer.on_item_move(&src_root, &dest_root, &src_path, &full_dest).await;
        } else {
            self.observer.on_item_copy(&dest_root, &full_dest).await;
        }
        Ok(self.sched_changed_event(action, &dest_root, &full_dest, src_info.0.as_deref(), src_info.1.as_deref(), false))
    }

    async fn handle_zip_files(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let _g = self.sync_lock.acquire().await;
        let store_only = req.get_boolean_or("store_only", false)?;
        let suffix = utils::local_strftime("%Y%m%d-%H%M%S");
        let dest = req.get_str_or("dest", &format!("config/collection-{}.zip", suffix));
        let (dest_root, dest_str) = self.convert_request_path(&dest)?;
        if !self.full_access(&dest_root) {
            return Err(ServerError::bad(format!("Destination Root '{}' is read-only", dest_root)));
        }
        let dest_path = PathBuf::from(&dest_str);
        self.check_reserved_path(&dest_str, true)?;
        if dest_path.is_dir() {
            return Err(ServerError::bad(format!("Cannot create archive at '{}'.  Path exists as a directory.", dest_path.display())));
        } else if !dest_path.parent().map(|p| p.exists()).unwrap_or(false) {
            return Err(ServerError::bad(format!(
                "Cannot create archive at '{}'.  Parent directory does not exist.",
                dest_path.display()
            )));
        }
        let items = req.get_list("items")?;
        if items.is_empty() {
            return Err(ServerError::bad("At least one file or directory must be specified"));
        }
        self.sync_lock.setup("create_file", &dest_path, false)?;
        self.zip_files(&items, &dest_path, store_only)?;
        self.observer.on_item_create(&dest_root, &dest_str, false);
        let ret = self.sched_changed_event("create_file", &dest_root, &dest_str, None, None, false);
        Ok(json!({"destination": ret["item"], "action": "zip_files"}))
    }

    fn zip_files(&self, items: &[String], destination: &Path, store_only: bool) -> Result<(), ServerError> {
        use std::io::Write;
        use std::os::unix::fs::MetadataExt;
        let temp_dest = std::env::temp_dir().join(destination.file_name().unwrap_or_default());
        let file = std::fs::File::create(&temp_dest)?;
        let mut zw = zip::ZipWriter::new(file);
        let method = if store_only { zip::CompressionMethod::Stored } else { zip::CompressionMethod::Deflated };
        let mut processed = std::collections::HashSet::new();
        // Err(None) is a PermissionError, which is skipped for directory children
        let add = |zw: &mut zip::ZipWriter<std::fs::File>, path: &Path, arc: &str| -> Result<bool, Option<ServerError>> {
            let data = match std::fs::read(path) {
                Ok(d) => d,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    return Err(None);
                }
                Err(e) => return Err(Some(e.into())),
            };
            let meta = std::fs::metadata(path).map_err(|e| Some(e.into()))?;
            let mtime = chrono::DateTime::from_timestamp(meta.mtime(), 0).map(|d| d.with_timezone(&chrono::Local).naive_local());
            let mut opts = zip::write::SimpleFileOptions::default().compression_method(method).unix_permissions(meta.mode() & 0o7777);
            if let Some(m) = mtime {
                use chrono::{Datelike, Timelike};
                if let Ok(dt) = zip::DateTime::from_date_and_time(
                    m.year() as u16,
                    m.month() as u8,
                    m.day() as u8,
                    m.hour() as u8,
                    m.minute() as u8,
                    m.second() as u8,
                ) {
                    opts = opts.last_modified_time(dt);
                }
            }
            zw.start_file(arc, opts).map_err(|e| Some(ServerError::exc(e.to_string())))?;
            zw.write_all(&data).map_err(|e| Some(e.into()))?;
            Ok(true)
        };
        for item in items {
            let (root, sp) = self.convert_request_path(item)?;
            let root_path = PathBuf::from(self.get_directory(&root));
            let root_parent = root_path.parent().unwrap_or(Path::new("/")).to_path_buf();
            let ip = PathBuf::from(&sp);
            self.check_reserved_path(&sp, false)?;
            if !ip.exists() {
                return Err(ServerError::bad(format!("No file/directory exits at '{}'", item)));
            }
            if ip.is_file() {
                let st = std::fs::metadata(&ip)?;
                if !processed.insert((st.dev(), st.ino())) {
                    continue;
                }
                let rel = ip.strip_prefix(&root_parent).unwrap_or(&ip).to_string_lossy().to_string();
                add(&mut zw, &ip, &rel).map_err(|e| e.unwrap_or_else(|| ServerError::exc(format!("[Errno 13] Permission denied: '{}'", ip.display()))))?;
                continue;
            } else if !ip.is_dir() {
                return Err(ServerError::bad(format!("Item at path '{}' is not a valid file or directory", item)));
            }
            for e in std::fs::read_dir(&ip)?.flatten() {
                let cp = e.path();
                if cp.is_file() {
                    if self.is_reserved_path(&cp.to_string_lossy(), false) {
                        continue;
                    }
                    let st = std::fs::metadata(&cp)?;
                    if !processed.insert((st.dev(), st.ino())) {
                        continue;
                    }
                    let rel = cp.strip_prefix(&root_parent).unwrap_or(&cp).to_string_lossy().to_string();
                    match add(&mut zw, &cp, &rel) {
                        Err(Some(e)) => return Err(e),
                        _ => {}
                    }
                }
            }
        }
        zw.finish().map_err(|e| ServerError::exc(e.to_string()))?;
        shutil_move(&temp_dest.to_string_lossy(), &destination.to_string_lossy()).map_err(ServerError::bad)?;
        Ok(())
    }

    pub fn list_directory(&self, path: &str, root: &str, is_extended: bool) -> Result<Map<String, Value>, ServerError> {
        if !Path::new(path).is_dir() {
            return Err(ServerError::bad(format!("Directory does not exist ({})", path)));
        }
        self.check_reserved_path(path, false)?;
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for e in std::fs::read_dir(path)?.flatten() {
            let fname = e.file_name().to_string_lossy().to_string();
            let full = path_join(path, &fname);
            let fp = Path::new(&full);
            if !fp.exists() {
                continue;
            }
            let mut info = self.get_path_info(&full, root, true)?;
            if fp.is_dir() {
                info.insert("dirname".into(), json!(fname));
                dirs.push(Value::Object(info));
            } else if fp.is_file() {
                info.insert("filename".into(), json!(fname));
                let ext = utils::splitext_ext(&fname).to_lowercase();
                if root == "gcodes" && VALID_GCODE_EXTS.contains(&ext.as_str()) && is_extended {
                    let rel = self.get_relative_path(root, &full);
                    if let Some(Value::Object(md)) = self.gcode_metadata.get(&rel) {
                        for (k, v) in md {
                            info.insert(k, v);
                        }
                    }
                }
                files.push(Value::Object(info));
            }
        }
        let mut out = Map::new();
        out.insert("dirs".into(), Value::Array(dirs));
        out.insert("files".into(), Value::Array(files));
        out.insert("disk_usage".into(), disk_usage(path));
        out.insert("root_info".into(), json!({"name": root, "permissions": if self.full_access(root) { "rw" } else { "r" }}));
        Ok(out)
    }

    pub fn get_path_info(&self, path: &str, root: &str, raise_error: bool) -> Result<Map<String, Value>, ServerError> {
        let p = Path::new(path);
        let real = utils::resolve_path(p);
        let meta = match std::fs::metadata(p) {
            Ok(m) => m,
            Err(e) => {
                if raise_error {
                    return Err(ServerError::exc(py_os_error(&e, path)));
                }
                let mut m = Map::new();
                m.insert("modified".into(), json!(0));
                m.insert("size".into(), json!(0));
                m.insert("permissions".into(), json!(""));
                return Ok(m);
            }
        };
        let permissions = if real.components().any(|c| c.as_os_str() == ".git") {
            ""
        } else {
            let mut perm = "rw";
            if !self.full_access(root) || (p.is_symlink() && p.is_file()) {
                perm = "r";
            }
            for (_, (res_path, can_read)) in self.reserved_paths.borrow().iter() {
                if res_path == &real || real.ancestors().skip(1).any(|a| a == res_path) {
                    if !can_read {
                        perm = "";
                        break;
                    }
                    perm = "r";
                }
            }
            perm
        };
        let mut m = Map::new();
        m.insert("modified".into(), utils::float_value(st_mtime(&meta)));
        m.insert("size".into(), json!(meta.len()));
        m.insert("permissions".into(), json!(permissions));
        Ok(m)
    }

    pub fn gen_temp_upload_path(&self) -> String {
        std::env::temp_dir().join(format!("moonraker.upload-{}.mru", utils::monotonic() as i64)).to_string_lossy().to_string()
    }

    pub async fn finalize_upload(self: &Rc<Self>, form_args: Map<String, Value>, user: Option<UserInfo>) -> ApiResult {
        let _g = self.sync_lock.acquire().await;
        let tmp = form_args.get("tmp_file_path").map(crate::pyfmt::py_str).unwrap_or_default();
        let res: ApiResult = async {
            let info = self.parse_upload_args(&form_args)?;
            self.check_reserved_path(&info.dest_path, true)?;
            self.sync_lock.setup("create_file", Path::new(&info.dest_path), false)?;
            if !self.full_access(&info.root) {
                return Err(ServerError::bad(format!("Invalid root request: {}", info.root)));
            }
            if info.root == "gcodes" && VALID_GCODE_EXTS.contains(&info.ext.as_str()) {
                self.finish_gcode_upload(&info, user.clone()).await
            } else {
                self.finish_standard_upload(&info).await
            }
        }
        .await;
        if res.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        res
    }

    fn parse_upload_args(&self, args: &Map<String, Value>) -> Result<UploadInfo, ServerError> {
        let filename_arg = args.get("filename").map(crate::pyfmt::py_str).ok_or_else(|| ServerError::bad("No file name specified in upload form"))?;
        let root = args.get("root").map(crate::pyfmt::py_str).unwrap_or_else(|| "gcodes".into()).to_lowercase();
        let root_path = self.file_paths.borrow().get(&root).cloned().ok_or_else(|| ServerError::bad(format!("Root {} not available", root)))?;
        let mut dir_path = args.get("path").map(crate::pyfmt::py_str).unwrap_or_default().trim_start_matches('/').to_string();
        let (mut filename, mut dest_path);
        if Path::new(&root_path).is_file() {
            filename = crate::pycompat::posixpath::basename(&root_path).to_string();
            dest_path = root_path.clone();
            dir_path = String::new();
        } else {
            filename = filename_arg.trim().trim_start_matches('/').to_string();
            if !dir_path.is_empty() {
                filename = path_join(&dir_path, &filename);
            }
            dest_path = abspath(&path_join(&root_path, &filename));
        }
        if !dest_path.starts_with(&root_path) {
            return Err(ServerError::bad(format!("Cannot write to path: {}", dest_path)));
        }
        let start_print = args.get("print").map(crate::pyfmt::py_str).as_deref() == Some("true");
        let ext = utils::splitext_ext(&dest_path).to_lowercase();
        let unzip_ufp = ext == ".ufp" && root == "gcodes";
        if unzip_ufp {
            filename = format!("{}.gcode", utils::splitext_root(&filename));
            dest_path = format!("{}.gcode", utils::splitext_root(&dest_path));
        }
        if Path::new(&dest_path).is_file() && !utils::access_write(Path::new(&dest_path)) {
            log::info!("Destination file exists and appears to be read-only: {}", dest_path);
        }
        Ok(UploadInfo {
            root,
            filename,
            dir_path,
            is_link: Path::new(&dest_path).is_symlink(),
            dest_path,
            tmp_file_path: args.get("tmp_file_path").map(crate::pyfmt::py_str).unwrap_or_default(),
            start_print,
            unzip_ufp,
            ext,
        })
    }

    async fn finish_gcode_upload(self: &Rc<Self>, info: &UploadInfo, user: Option<UserInfo>) -> ApiResult {
        let mut can_start = false;
        match self.handle_operation_check(&info.dest_path) {
            Ok(c) => can_start = c,
            Err(e) if e.status_code == 403 => return Err(ServerError::new("File is loaded, upload not permitted", 403)),
            Err(_) => {}
        }
        let finfo = self.process_uploaded_file(info).await?;
        self.gcode_metadata.parse_metadata(&info.filename, finfo).wait().await;
        let mut started = false;
        let mut queued = false;
        if info.start_print {
            if can_start {
                if server().klippy_apis().start_print(&info.filename, false, user.clone()).await.is_ok() {
                    started = true;
                }
            }
            if self.queue_gcodes && !started {
                if let Some(jq) = server().lookup_component::<crate::components::job_queue::JobQueue>("job_queue") {
                    jq.queue_job(vec![info.filename.clone()], false, false, user.clone()).await?;
                    queued = true;
                }
            }
        }
        self.observer.on_item_create("gcodes", &info.dest_path, false);
        let mut result = self.sched_changed_event("create_file", "gcodes", &info.dest_path, None, None, info.is_link);
        result["print_started"] = json!(started);
        result["print_queued"] = json!(queued);
        Ok(result)
    }

    async fn finish_standard_upload(self: &Rc<Self>, info: &UploadInfo) -> ApiResult {
        self.process_uploaded_file(info).await?;
        self.observer.on_item_create(&info.root, &info.dest_path, false);
        Ok(self.sched_changed_event("create_file", &info.root, &info.dest_path, None, None, info.is_link))
    }

    async fn process_uploaded_file(&self, info: &UploadInfo) -> Result<Map<String, Value>, ServerError> {
        let res: Result<Map<String, Value>, String> = async {
            if !info.dir_path.is_empty() {
                let mut cur = self.get_directory(&info.root);
                for sub in info.dir_path.trim_matches('/').split('/') {
                    cur = path_join(&cur, sub);
                    if Path::new(&cur).exists() {
                        continue;
                    }
                    std::fs::create_dir(&cur).map_err(|e| e.to_string())?;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
            if info.unzip_ufp {
                let mut finfo = self.get_path_info(&info.tmp_file_path, &info.root, true).map_err(|e| e.message)?;
                finfo.insert("ufp_path".into(), json!(info.tmp_file_path));
                Ok(finfo)
            } else {
                let mut dest = info.dest_path.clone();
                if info.is_link {
                    dest = std::fs::canonicalize(&dest).map(|p| p.to_string_lossy().to_string()).unwrap_or(dest);
                }
                shutil_move(&info.tmp_file_path, &dest)?;
                self.get_path_info(&info.dest_path, &info.root, true).map_err(|e| e.message)
            }
        }
        .await;
        res.map_err(|e| {
            log::error!("Upload Write Error\n{}", e);
            ServerError::new("Unable to save file", 500)
        })
    }

    /// get_file_list(root) as a mapping of relative paths to path info
    pub fn get_file_list(&self, root: &str) -> Result<IndexMap<String, Map<String, Value>>, ServerError> {
        use std::os::unix::fs::MetadataExt;
        let path = self.file_paths.borrow().get(root).cloned();
        let path = match path {
            Some(p) if Path::new(&p).is_dir() => p,
            other => {
                let msg = format!(
                    "Failed to build file list, invalid path: {}: {}",
                    root,
                    other.unwrap_or_else(|| "None".into())
                );
                log::info!("{}", msg);
                return Err(ServerError::bad(msg));
            }
        };
        log::info!("Updating File List <{}>...", root);
        let st = std::fs::metadata(&path)?;
        let mut visited = std::collections::HashSet::new();
        visited.insert((st.dev(), st.ino()));
        let mut filelist = IndexMap::new();
        let mut stack = vec![path.clone()];
        // os.walk is top down, process directories in discovery order
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(stack.pop().unwrap());
        while let Some(dir) = queue.pop_front() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let mut dirs = Vec::new();
            let mut files = Vec::new();
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let full = path_join(&dir, &name);
                if Path::new(&full).is_dir() {
                    dirs.push(name);
                } else {
                    files.push(name);
                }
            }
            let mut scan = Vec::new();
            for d in dirs {
                let full = path_join(&dir, &d);
                let meta = match std::fs::metadata(&full) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if visited.insert((meta.dev(), meta.ino())) && !self.is_reserved_path(&full, false) {
                    scan.push(full);
                }
            }
            for name in files {
                let ext = utils::splitext_ext(&name).to_lowercase();
                if root == "gcodes" && !VALID_GCODE_EXTS.contains(&ext.as_str()) {
                    continue;
                }
                let full = path_join(&dir, &name);
                if !Path::new(&full).exists() {
                    continue;
                }
                let fname = full[path.len() + 1..].to_string();
                let info = self.get_path_info(&full, root, true)?;
                filelist.insert(fname, info);
            }
            // os.walk visits subdirectories depth first after the parent
            for s in scan.into_iter().rev() {
                queue.push_front(s);
            }
        }
        Ok(filelist)
    }

    pub fn get_file_list_formatted(&self, root: &str) -> Result<Vec<Value>, ServerError> {
        let fl = self.get_file_list(root)?;
        let mut names: Vec<&String> = fl.keys().collect();
        names.sort_by_key(|n| n.to_lowercase());
        Ok(names
            .into_iter()
            .map(|n| {
                let mut m = Map::new();
                m.insert("path".into(), json!(n));
                for (k, v) in &fl[n] {
                    m.insert(k.clone(), v.clone());
                }
                Value::Object(m)
            })
            .collect())
    }

    pub fn get_file_metadata(&self, filename: &str) -> Value {
        let mut f = filename.trim_start_matches('/').to_string();
        if let Some(r) = f.strip_prefix("gcodes/") {
            f = r.to_string();
        }
        self.gcode_metadata.get(&f).unwrap_or(json!({}))
    }

    pub fn list_dir(&self, directory: &str, simple_format: bool) -> Result<Value, ServerError> {
        let directory = directory.strip_prefix('/').unwrap_or(directory);
        let mut parts = directory.splitn(2, '/');
        let root = parts.next().unwrap_or("").to_string();
        let path = self.file_paths.borrow().get(&root).cloned().ok_or_else(|| ServerError::bad(format!("Invalid Directory Request: {}", directory)))?;
        let dir_path = match parts.next() {
            None => path,
            Some(r) => path_join(&path, r),
        };
        if !Path::new(&dir_path).is_dir() {
            return Err(ServerError::bad(format!("Directory does not exist ({})", dir_path)));
        }
        let fl = self.list_directory(&dir_path, &root, false)?;
        if simple_format {
            let mut out = Vec::new();
            for d in fl["dirs"].as_array().unwrap() {
                out.push(json!(format!("*{}", crate::pyfmt::py_str(&d["dirname"]))));
            }
            for f in fl["files"].as_array().unwrap() {
                let fname = crate::pyfmt::py_str(&f["filename"]);
                let ext = utils::splitext_ext(&fname).to_lowercase();
                if root == "gcodes" && VALID_GCODE_EXTS.contains(&ext.as_str()) {
                    out.push(json!(fname));
                }
            }
            return Ok(Value::Array(out));
        }
        Ok(Value::Object(fl))
    }

    pub async fn delete_file(self: &Rc<Self>, path: &str) -> ApiResult {
        let _g = self.sync_lock.acquire().await;
        let (root, full_path) = self.convert_request_path(path)?;
        self.check_reserved_path(&full_path, true)?;
        if !self.full_access(&root) {
            return Err(ServerError::new(format!("Path not available for DELETE: {}", path), 405));
        }
        if !Path::new(&full_path).is_file() {
            return Err(ServerError::bad(format!("Invalid file path: {}", path)));
        }
        if let Err(e) = self.handle_operation_check(&full_path) {
            if e.status_code == 403 {
                return Err(e);
            }
        }
        self.sync_lock.setup("delete_file", Path::new(&full_path), false)?;
        std::fs::remove_file(&full_path).map_err(|e| ServerError::exc(py_os_error(&e, &full_path)))?;
        self.observer.on_item_delete(&root, &full_path, false);
        Ok(self.sched_changed_event("delete_file", &root, &full_path, None, None, false))
    }

    pub fn sched_changed_event(
        self: &Rc<Self>,
        action: &str,
        root: &str,
        full_path: &str,
        source_root: Option<&str>,
        source_path: Option<&str>,
        immediate: bool,
    ) -> Value {
        let rel = self.get_relative_path(root, full_path);
        let mut info = self.get_path_info(full_path, root, false).unwrap_or_default();
        info.insert("path".into(), json!(rel));
        info.insert("root".into(), json!(root));
        let mut notify = json!({"action": action, "item": Value::Object(info)});
        if let (Some(sr), Some(sp)) = (source_root, source_path) {
            let srel = self.get_relative_path(sr, sp);
            notify["source_item"] = json!({"path": srel, "root": sr});
        }
        let immediate = immediate || !self.observer.has_fast_observe();
        let delay = if immediate { 0.005 } else { 1.0 };
        let key = format!("{}-{}-{}", action, root, rel);
        let fm = Rc::downgrade(self);
        let n = notify.clone();
        let k = key.clone();
        let handle = spawn(async move {
            crate::utils::sleep_ordered(std::time::Duration::from_secs_f64(delay)).await;
            if let Some(fm) = fm.upgrade() {
                fm.scheduled_notifications.borrow_mut().remove(&k);
            }
            let _ = server().send_event("file_manager:filelist_changed", vec![n]);
        });
        if !immediate {
            self.scheduled_notifications.borrow_mut().insert(key, handle);
        }
        notify
    }

    pub fn cancel_notification(&self, key: &str) {
        if let Some(h) = self.scheduled_notifications.borrow_mut().remove(key) {
            h.abort();
        }
    }
}

pub struct UploadInfo {
    pub root: String,
    pub filename: String,
    pub dir_path: String,
    pub dest_path: String,
    pub tmp_file_path: String,
    pub start_print: bool,
    pub unzip_ufp: bool,
    pub ext: String,
    pub is_link: bool,
}

fn is_close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-09 * a.abs().max(b.abs())
}

// Python's str(OSError) and shutil, as Moonraker's file manager uses them
pub use crate::pycompat::shutil::{py_os_error, shutil_copy2, shutil_copytree, shutil_move};

fn disk_usage(path: &str) -> Value {
    let c = match std::ffi::CString::new(path) {
        Ok(c) => c,
        Err(_) => return json!({"total": 0, "used": 0, "free": 0}),
    };
    // SAFETY: a C struct of integers: all zero is a valid value
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c is a NUL-terminated CString alive for the call; st is exclusively borrowed for statvfs to fill
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return json!({"total": 0, "used": 0, "free": 0});
    }
    let frsize = st.f_frsize as u64;
    let total = st.f_blocks as u64 * frsize;
    let free = st.f_bavail as u64 * frsize;
    let used = (st.f_blocks as u64 - st.f_bfree as u64) * frsize;
    json!({"total": total, "used": used, "free": free})
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<FileManager>, crate::confighelper::ConfigError> {
    FileManager::new(config)
}

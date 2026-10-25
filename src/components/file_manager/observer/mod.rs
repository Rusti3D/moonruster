// File system observers (BaseFileSystemObserver / InotifyObserver from
// moonraker/components/file_manager/file_manager.py)

use super::{path_join, FileManager, VALID_GCODE_EXTS};
use crate::confighelper::ConfigHelper;
use crate::server::{server, spawn};
use crate::utils::{self, AsyncEvent};
use futures::future::LocalBoxFuture;
use indexmap::{IndexMap, IndexSet};
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::rc::{Rc, Weak};

const INOTIFY_BUNDLE_TIME: f64 = 0.25;
const INOTIFY_MOVE_TIME: f64 = 1.0;

const IN_MODIFY: u32 = 0x2;
const IN_CLOSE_WRITE: u32 = 0x8;
const IN_MOVED_FROM: u32 = 0x40;
const IN_MOVED_TO: u32 = 0x80;
const IN_CREATE: u32 = 0x100;
const IN_DELETE: u32 = 0x200;
const IN_IGNORED: u32 = 0x8000;
const IN_ISDIR: u32 = 0x4000_0000;
const IN_ONLYDIR: u32 = 0x0100_0000;
const WATCH_FLAGS: u32 = IN_CREATE | IN_DELETE | IN_MODIFY | IN_MOVED_TO | IN_MOVED_FROM | IN_ONLYDIR | IN_CLOSE_WRITE;

const FLAG_NAMES: [(u32, &str); 13] = [
    (0x1, "ACCESS"),
    (0x2, "MODIFY"),
    (0x4, "ATTRIB"),
    (0x8, "CLOSE_WRITE"),
    (0x10, "CLOSE_NOWRITE"),
    (0x20, "OPEN"),
    (0x40, "MOVED_FROM"),
    (0x80, "MOVED_TO"),
    (0x100, "CREATE"),
    (0x200, "DELETE"),
    (0x400, "DELETE_SELF"),
    (0x800, "MOVE_SELF"),
    (0x4000_0000, "ISDIR"),
];

type Coro = LocalBoxFuture<'static, ()>;

#[derive(Clone)]
pub struct NotifyArgs {
    action: String,
    root: String,
    path: String,
    src_root: Option<String>,
    src_path: Option<String>,
}

mod events;
mod node;

pub use node::*;
use events::*;

// ***** Observer *****

/// A move-from event awaiting its move-to: the parent node, the name and the timeout
type PendingMove = (Rc<Node>, String, tokio::task::JoinHandle<()>);

struct InotifyState {
    fd: i32,
    watched_roots: RefCell<IndexMap<String, Rc<Node>>>,
    watched_nodes: RefCell<IndexMap<i32, Rc<Node>>>,
    pending_moves: RefCell<HashMap<u32, PendingMove>>,
    initialized: Cell<bool>,
    processing_gcode_files: RefCell<IndexSet<String>>,
    pending_coroutines: RefCell<VecDeque<Coro>>,
    gc_notify_running: Cell<bool>,
    reader: crate::server::TaskSlot,
}

pub struct Observer {
    enable_warn: bool,
    fm: RefCell<Weak<FileManager>>,
    inotify: Option<InotifyState>,
    me: RefCell<Weak<Observer>>,
}

impl Observer {
    pub fn new(config: &ConfigHelper, use_inotify: bool) -> Result<Rc<Self>, crate::confighelper::ConfigError> {
        let mut enable_warn = config.getboolean_or("enable_observer_warnings", true)?;
        let mut inotify = None;
        if use_inotify {
            enable_warn = config.getboolean_dep("enable_inotify_warnings", enable_warn, true)?;
            // SAFETY: inotify_init1 takes no pointers
            let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
            if fd >= 0 {
                inotify = Some(InotifyState {
                    fd,
                    watched_roots: RefCell::new(IndexMap::new()),
                    watched_nodes: RefCell::new(IndexMap::new()),
                    pending_moves: RefCell::new(HashMap::new()),
                    initialized: Cell::new(false),
                    processing_gcode_files: RefCell::new(IndexSet::new()),
                    pending_coroutines: RefCell::new(VecDeque::new()),
                    gc_notify_running: Cell::new(false),
                    reader: Default::default(),
                });
            } else {
                log::error!("Failed to initialize inotify");
            }
        }
        let obs = Rc::new(Observer { enable_warn, fm: RefCell::new(Weak::new()), inotify, me: RefCell::new(Weak::new()) });
        *obs.me.borrow_mut() = Rc::downgrade(&obs);
        if let Some(st) = &obs.inotify {
            let o = Rc::downgrade(&obs);
            let fd = st.fd;
            st.reader.set(spawn(async move { inotify_reader(fd, o).await }));
        }
        Ok(obs)
    }

    pub fn set_file_manager(&self, fm: &Rc<FileManager>) {
        *self.fm.borrow_mut() = Rc::downgrade(fm);
    }

    fn fm(&self) -> Rc<FileManager> {
        self.fm.borrow().upgrade().expect("file manager")
    }

    fn me(&self) -> Rc<Observer> {
        self.me.borrow().upgrade().expect("observer")
    }

    pub fn has_fast_observe(&self) -> bool {
        self.inotify.is_some()
    }

    pub fn initialize(&self) {
        let st = match &self.inotify {
            Some(s) => s,
            None => return,
        };
        if st.initialized.get() {
            return;
        }
        let roots: Vec<(String, Rc<Node>)> = st.watched_roots.borrow().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (root, node) in &roots {
            let mut visited = std::collections::HashSet::new();
            let evts = node.scan_node(&mut visited);
            if evts.is_empty() {
                continue;
            }
            let root_path = node.get_path();
            self.spawn_root_updated(evts, root.clone(), root_path);
        }
        if !roots.is_empty() {
            self.log_nodes();
        }
        st.initialized.set(true);
    }

    fn spawn_root_updated(&self, evts: Vec<Rc<AsyncEvent>>, root: String, root_path: String) {
        let obs = self.me();
        spawn(async move {
            for e in evts {
                e.wait().await;
            }
            let st = obs.inotify.as_ref().unwrap();
            let cur = st.watched_roots.borrow().get(&root).map(|n| n.get_path());
            if server().is_running() && cur.as_deref() == Some(root_path.as_str()) {
                obs.notify_filelist_changed("root_update", &root, &root_path, None, None);
            }
        });
    }

    pub fn add_root_watch(&self, root: &str, root_path: &str) {
        let st = match &self.inotify {
            None => {
                if server().is_running() {
                    self.fm().sched_changed_event("root_update", root, root_path, None, None, true);
                }
                return;
            }
            Some(s) => s,
        };
        if let Some(old) = st.watched_roots.borrow_mut().shift_remove(root) {
            old.clear_watches();
            old.clear_events(true);
        }
        let node = match Node::create(&self.me(), None, root_path, Some(root)) {
            Ok(n) => n,
            Err(_) => {
                server().add_warning(
                    &format!("file_manager: Failed to create inotify root node {}. See moonraker.log for details.", root),
                    None,
                    true,
                );
                return;
            }
        };
        st.watched_roots.borrow_mut().insert(root.to_string(), node.clone());
        if st.initialized.get() {
            let mut visited = std::collections::HashSet::new();
            let evts = node.scan_node(&mut visited);
            self.log_nodes();
            self.spawn_root_updated(evts, root.to_string(), root_path.to_string());
        }
    }

    fn add_watch(&self, node: &Rc<Node>) -> Result<i32, ()> {
        let st = self.inotify.as_ref().ok_or(())?;
        let dir_path = node.get_path();
        let c = std::ffi::CString::new(dir_path.clone()).map_err(|_| ())?;
        // SAFETY: st.fd is the inotify fd (never closed); c is a NUL-terminated CString alive for the call
        let wd = unsafe { libc::inotify_add_watch(st.fd, c.as_ptr(), WATCH_FLAGS) };
        if wd < 0 {
            let msg = format!("Error adding inotify watch to root '{}', path: {}", node.get_root(), dir_path);
            log::error!("{}", msg);
            if self.enable_warn {
                server().add_warning(&format!("file_manager: {}", msg), None, false);
            }
            return Err(());
        }
        if let Some(cur) = st.watched_nodes.borrow().get(&wd) {
            let msg = format!(
                "Inotify watch already exists for path '{}' in root '{}', cannot add watch to requested root '{}'.  This indicates that the roots overlap.",
                dir_path,
                cur.get_root(),
                node.get_root()
            );
            if self.enable_warn {
                server().add_warning(&format!("file_manager: {}", msg), None, true);
            } else {
                log::info!("{}", msg);
            }
            return Err(());
        }
        st.watched_nodes.borrow_mut().insert(wd, node.clone());
        Ok(wd)
    }

    fn remove_watch(&self, wd: i32, need_low_level_rm: bool) {
        let st = match &self.inotify {
            Some(s) => s,
            None => return,
        };
        let node = st.watched_nodes.borrow_mut().shift_remove(&wd);
        if let (true, Some(node)) = (need_low_level_rm, &node) {
            // SAFETY: st.fd is the inotify fd (never closed); a stale wd only makes the call fail
            if unsafe { libc::inotify_rm_watch(st.fd, wd) } < 0 {
                log::error!("Error removing watch: '{}'", node.get_path());
            }
        }
    }

    fn log_nodes(&self) {
        if !server().is_verbose_enabled() {
            return;
        }
        if let Some(st) = &self.inotify {
            let mut msg = String::from("Inotify Watches After Scan:");
            for (wd, node) in st.watched_nodes.borrow().iter() {
                msg.push_str(&format!("\nRoot: {}, Directory: {},  Watch: {}", node.get_root(), node.get_path(), wd));
            }
            log::debug!("{}", msg);
        }
    }

    // ***** Base observer behaviour *****

    /// Returns false when the metadata needs a rescan
    fn try_move_metadata_dir(&self, prev_root: &str, new_root: &str, prev_path: &str, new_path: &str) -> bool {
        let fm = self.fm();
        if new_root == "gcodes" {
            if prev_root == "gcodes" {
                let pr = fm.get_relative_path("gcodes", prev_path);
                let nr = fm.get_relative_path("gcodes", new_path);
                fm.gcode_metadata.move_directory_metadata(&pr, &nr);
            } else {
                self.clear_metadata(prev_root, prev_path, true);
                return false;
            }
        } else if prev_root == "gcodes" {
            self.clear_metadata(prev_root, prev_path, true);
        }
        true
    }

    /// File variant: Ok(Some(fut)) when metadata moved and thumbnails pending
    async fn try_move_metadata_file(&self, prev_root: &str, new_root: &str, prev_path: &str, new_path: &str) -> bool {
        let fm = self.fm();
        if new_root == "gcodes" {
            if prev_root == "gcodes" {
                let pr = fm.get_relative_path("gcodes", prev_path);
                let nr = fm.get_relative_path("gcodes", new_path);
                return fm.gcode_metadata.move_file_metadata(&pr, &nr).await;
            } else {
                self.clear_metadata(prev_root, prev_path, false);
                return false;
            }
        } else if prev_root == "gcodes" {
            self.clear_metadata(prev_root, prev_path, false);
        }
        true
    }

    fn clear_metadata(&self, root: &str, path: &str, is_dir: bool) {
        if root == "gcodes" {
            let fm = self.fm();
            let rel = fm.get_relative_path(root, path);
            if is_dir {
                fm.gcode_metadata.remove_directory_metadata(&rel);
            } else {
                fm.gcode_metadata.remove_file_metadata(&rel);
            }
        }
    }

    pub fn parse_gcode_metadata(&self, file_path: &str) -> Rc<AsyncEvent> {
        let fm = self.fm();
        let mut rel = fm.get_relative_path("gcodes", file_path);
        let ext = utils::splitext_ext(&rel).to_lowercase();
        let mut info = fm.get_path_info(file_path, "gcodes", true).unwrap_or_default();
        let size = info.get("size").and_then(|v| v.as_i64()).unwrap_or(0);
        if !VALID_GCODE_EXTS.contains(&ext.as_str()) || size == 0 {
            return AsyncEvent::new_set();
        }
        if ext == ".ufp" {
            rel = format!("{}.gcode", utils::splitext_root(&rel));
            info.insert("ufp_path".into(), json!(file_path));
        }
        fm.gcode_metadata.parse_metadata(&rel, info)
    }

    async fn scan_directory_metadata(&self, start: &str) {
        use std::os::unix::fs::MetadataExt;
        let st = match std::fs::metadata(start) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut visited = std::collections::HashSet::new();
        visited.insert((st.dev(), st.ino()));
        let mut evts = Vec::new();
        let mut stack = vec![start.to_string()];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let mut subdirs = Vec::new();
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let full = path_join(&dir, &name);
                let p = Path::new(&full);
                if p.is_dir() {
                    if let Ok(m) = std::fs::metadata(p) {
                        if visited.insert((m.dev(), m.ino())) {
                            subdirs.push(full);
                        }
                    }
                } else if p.is_file() {
                    let suffix = crate::pycompat::pathlib::path_suffix(p);
                    if VALID_GCODE_EXTS.contains(&suffix.as_str()) {
                        evts.push(self.parse_gcode_metadata(&full));
                    }
                }
            }
            for s in subdirs.into_iter().rev() {
                stack.push(s);
            }
        }
        for e in evts {
            e.wait().await;
        }
    }

    pub async fn on_item_copy(&self, root: &str, item_path: &str) {
        if self.has_fast_observe() || root != "gcodes" {
            return;
        }
        let p = Path::new(item_path);
        let suffix = crate::pycompat::pathlib::path_suffix(p);
        if p.is_file() && VALID_GCODE_EXTS.contains(&suffix.as_str()) {
            self.parse_gcode_metadata(item_path).wait().await;
        } else if p.is_dir() {
            self.scan_directory_metadata(item_path).await;
        }
    }

    pub async fn on_item_move(&self, src_root: &str, dest_root: &str, src_path: &str, dest_path: &str) {
        if self.has_fast_observe() {
            return;
        }
        let dp = Path::new(dest_path);
        let is_dir = dp.is_dir();
        let moved = if is_dir {
            self.try_move_metadata_dir(src_root, dest_root, src_path, dest_path)
        } else {
            self.try_move_metadata_file(src_root, dest_root, src_path, dest_path).await
        };
        if !moved {
            if is_dir {
                self.scan_directory_metadata(dest_path).await;
            } else {
                let suffix = crate::pycompat::pathlib::path_suffix(dp);
                if dp.is_file() && VALID_GCODE_EXTS.contains(&suffix.as_str()) {
                    self.parse_gcode_metadata(dest_path).wait().await;
                }
            }
        }
    }

    pub fn on_item_create(&self, _root: &str, _item_path: &str, _is_dir: bool) {}

    pub fn on_item_delete(&self, root: &str, item_path: &str, is_dir: bool) {
        if self.has_fast_observe() {
            return;
        }
        self.clear_metadata(root, item_path, is_dir);
    }

    pub fn close(&self) {
        if let Some(st) = &self.inotify {
            st.pending_coroutines.borrow_mut().clear();
            st.reader.cancel();
            for wd in st.watched_nodes.borrow().keys() {
                // SAFETY: st.fd is the inotify fd (never closed); a stale wd only makes the call fail
                unsafe { libc::inotify_rm_watch(st.fd, *wd) };
            }
        }
    }
}

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

// ***** Inotify node tree *****

pub struct Node {
    obs: Weak<Observer>,
    name: RefCell<String>,
    parent: RefCell<Option<Weak<Node>>>,
    root_name: Option<String>,
    children: RefCell<IndexMap<String, Rc<Node>>>,
    wd: Cell<i32>,
    pending_node_events: RefCell<HashMap<String, Option<tokio::task::JoinHandle<()>>>>,
    pending_deleted_children: RefCell<IndexSet<(String, bool)>>,
    pending_file_events: RefCell<HashMap<String, String>>,
    queued_move_notifications: RefCell<Vec<NotifyArgs>>,
    is_processing_metadata: Cell<bool>,
    me: RefCell<Weak<Node>>,
}

impl Node {
    fn create(obs: &Rc<Observer>, parent: Option<&Rc<Node>>, name: &str, root_name: Option<&str>) -> Result<Rc<Node>, ()> {
        let node = Rc::new(Node {
            obs: Rc::downgrade(obs),
            name: RefCell::new(name.to_string()),
            parent: RefCell::new(parent.map(Rc::downgrade)),
            root_name: root_name.map(String::from),
            children: RefCell::new(IndexMap::new()),
            wd: Cell::new(-1),
            pending_node_events: RefCell::new(HashMap::new()),
            pending_deleted_children: RefCell::new(IndexSet::new()),
            pending_file_events: RefCell::new(HashMap::new()),
            queued_move_notifications: RefCell::new(Vec::new()),
            is_processing_metadata: Cell::new(false),
            me: RefCell::new(Weak::new()),
        });
        *node.me.borrow_mut() = Rc::downgrade(&node);
        let wd = obs.add_watch(&node)?;
        node.wd.set(wd);
        Ok(node)
    }

    fn me(&self) -> Rc<Node> {
        self.me.borrow().upgrade().expect("node")
    }

    fn obs(&self) -> Rc<Observer> {
        self.obs.upgrade().expect("observer")
    }

    fn parent(&self) -> Option<Rc<Node>> {
        self.parent.borrow().as_ref().and_then(|p| p.upgrade())
    }

    fn is_root(&self) -> bool {
        self.root_name.is_some()
    }

    pub fn get_path(&self) -> String {
        if self.is_root() {
            return self.name.borrow().clone();
        }
        match self.parent() {
            Some(p) => path_join(&p.get_path(), &self.name.borrow()),
            None => self.name.borrow().clone(),
        }
    }

    pub fn get_root(&self) -> String {
        if let Some(r) = &self.root_name {
            return r.clone();
        }
        self.parent().map(|p| p.get_root()).unwrap_or_default()
    }

    fn is_processing(&self) -> bool {
        if self.is_processing_metadata.get() {
            return true;
        }
        if self.is_root() {
            return false;
        }
        self.parent().map(|p| p.is_processing()).unwrap_or(false)
    }

    async fn finish_create_node(self: Rc<Self>) {
        if self.pending_node_events.borrow_mut().remove("create_node").is_none() {
            return;
        }
        let node_path = self.get_path();
        let root = self.get_root();
        self.is_processing_metadata.set(true);
        let mut visited = std::collections::HashSet::new();
        let mevts = self.scan_node(&mut visited);
        for e in mevts {
            e.wait().await;
        }
        self.is_processing_metadata.set(false);
        let obs = self.obs();
        obs.log_nodes();
        obs.notify_filelist_changed("create_dir", &root, &node_path, None, None);
        let queued: Vec<NotifyArgs> = self.queued_move_notifications.borrow_mut().drain(..).collect();
        for a in queued {
            obs.notify_filelist_changed(&a.action, &a.root, &a.path, a.src_root.as_deref(), a.src_path.as_deref());
        }
    }

    fn finish_delete_child(&self) {
        if self.pending_node_events.borrow_mut().remove("delete_child").is_none() {
            self.pending_deleted_children.borrow_mut().clear();
            return;
        }
        let node_path = self.get_path();
        let root = self.get_root();
        let obs = self.obs();
        let items: Vec<(String, bool)> = self.pending_deleted_children.borrow_mut().drain(..).collect();
        for (name, is_node) in items {
            let item_path = path_join(&node_path, &name);
            let item_type = if is_node { "dir" } else { "file" };
            obs.clear_metadata(&root, &item_path, is_node);
            obs.notify_filelist_changed(&format!("delete_{}", item_type), &root, &item_path, None, None);
        }
    }

    fn scan_node(&self, visited: &mut std::collections::HashSet<(u64, u64)>) -> Vec<Rc<AsyncEvent>> {
        use std::os::unix::fs::MetadataExt;
        let dir_path = self.get_path();
        let st = match std::fs::metadata(&dir_path) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let key = (st.dev(), st.ino());
        // python compares a stat_result to tuples, which never matches
        let _ = visited.contains(&key);
        let mut events = Vec::new();
        visited.insert(key);
        let obs = self.obs();
        let entries = match std::fs::read_dir(&dir_path) {
            Ok(r) => r,
            Err(_) => return events,
        };
        for e in entries.flatten() {
            let fname = e.file_name().to_string_lossy().to_string();
            let item_path = path_join(&dir_path, &fname);
            let p = Path::new(&item_path);
            if p.is_dir() {
                let fm = obs.fm();
                if fm.is_reserved_path(&item_path, true) {
                    continue;
                }
                if let Some(child) = self.create_child_node(&fname, false) {
                    events.extend(child.scan_node(visited));
                }
            } else if p.is_file() && self.get_root() == "gcodes" {
                events.push(obs.parse_gcode_metadata(&item_path));
            }
        }
        events
    }

    fn move_child_node(&self, child_name: &str, new_name: &str, new_parent: &Rc<Node>) {
        self.flush_delete();
        let child = match self.children.borrow_mut().shift_remove(child_name) {
            Some(c) => c,
            None => {
                log::info!("No child for node at path: {}", self.get_path());
                return;
            }
        };
        let prev_path = child.get_path();
        let prev_root = child.get_root();
        *child.name.borrow_mut() = new_name.to_string();
        new_parent.add_child_node(&child);
        let new_path = child.get_path();
        let new_root = child.get_root();
        log::debug!("Moving node from '{}' to '{}'", prev_path, new_path);
        let obs = self.obs();
        let move_res = obs.try_move_metadata_dir(&prev_root, &new_root, &prev_path, &new_path);
        if new_root == "gcodes" {
            let obs2 = obs.clone();
            obs.queue_gcode_notification(Box::pin(async move {
                if !move_res {
                    let mut visited = std::collections::HashSet::new();
                    for e in child.scan_node(&mut visited) {
                        e.wait().await;
                    }
                }
                obs2.notify_filelist_changed("move_dir", &new_root, &new_path, Some(&prev_root), Some(&prev_path));
            }));
        } else {
            obs.notify_filelist_changed("move_dir", &new_root, &new_path, Some(&prev_root), Some(&prev_path));
        }
    }

    fn schedule_file_event(&self, file_name: &str, evt_name: &str) {
        if self.pending_file_events.borrow().contains_key(file_name) {
            return;
        }
        if let Some(pn) = self.search_pending_event("create_node") {
            pn.stop_event("create_node");
        }
        self.pending_file_events.borrow_mut().insert(file_name.to_string(), evt_name.to_string());
    }

    fn clear_file_event(&self, file_name: &str) -> Option<String> {
        let evt = self.pending_file_events.borrow_mut().remove(file_name);
        if evt.is_some() {
            if let Some(pn) = self.search_pending_event("create_node") {
                pn.reset_event("create_node", INOTIFY_BUNDLE_TIME);
            }
        }
        evt
    }

    fn complete_file_write(&self, file_name: &str) {
        self.flush_delete();
        let evt_name = match self.pending_file_events.borrow_mut().remove(file_name) {
            Some(e) => e,
            None => {
                log::info!("Invalid file write event: {}", file_name);
                return;
            }
        };
        if self.is_processing() {
            log::debug!("Metadata is processing, suppressing write event: {}", file_name);
            return;
        }
        if let Some(pn) = self.search_pending_event("create_node") {
            pn.reset_event("create_node", INOTIFY_BUNDLE_TIME);
            return;
        }
        let file_path = path_join(&self.get_path(), file_name);
        let root = self.get_root();
        let obs = self.obs();
        if root == "gcodes" {
            if obs.need_create_notify(&file_path) {
                let obs2 = obs.clone();
                obs.queue_gcode_notification(Box::pin(async move {
                    let e = obs2.parse_gcode_metadata(&file_path);
                    e.wait().await;
                    obs2.notify_filelist_changed(&evt_name, &root, &file_path, None, None);
                    obs2.clear_processing_file(&file_path);
                }));
            }
        } else {
            obs.notify_filelist_changed(&evt_name, &root, &file_path, None, None);
        }
    }

    fn add_child_node(&self, node: &Rc<Node>) {
        self.children.borrow_mut().insert(node.name.borrow().clone(), node.clone());
        *node.parent.borrow_mut() = Some(self.me.borrow().clone());
    }

    fn has_child_node(&self, name: &str) -> bool {
        self.children.borrow().contains_key(name)
    }

    fn create_child_node(&self, name: &str, notify: bool) -> Option<Rc<Node>> {
        self.flush_delete();
        if let Some(c) = self.children.borrow().get(name) {
            return Some(c.clone());
        }
        let obs = self.obs();
        let child = Node::create(&obs, Some(&self.me()), name, None).ok()?;
        self.children.borrow_mut().insert(name.to_string(), child.clone());
        if notify {
            match self.search_pending_event("create_node") {
                None => child.add_event("create_node", INOTIFY_BUNDLE_TIME),
                Some(pn) => pn.reset_event("create_node", INOTIFY_BUNDLE_TIME),
            }
        }
        Some(child)
    }

    fn schedule_child_delete(&self, child_name: &str, is_node: bool) {
        if is_node {
            let child = match self.children.borrow_mut().shift_remove(child_name) {
                Some(c) => c,
                None => return,
            };
            self.obs().remove_watch(child.wd.get(), false);
            child.remove_event("delete_child");
        }
        self.pending_deleted_children.borrow_mut().insert((child_name.to_string(), is_node));
        self.add_event("delete_child", INOTIFY_BUNDLE_TIME);
    }

    fn clear_watches(&self) {
        let children: Vec<Rc<Node>> = self.children.borrow().values().cloned().collect();
        for c in children {
            c.clear_watches();
        }
        self.obs().remove_watch(self.wd.get(), true);
    }

    fn schedule(&self, evt_name: &str, timeout: f64) -> tokio::task::JoinHandle<()> {
        let me = self.me.borrow().clone();
        let evt = evt_name.to_string();
        spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs_f64(timeout)).await;
            if let Some(n) = me.upgrade() {
                if evt == "create_node" {
                    n.finish_create_node().await;
                } else {
                    n.finish_delete_child();
                }
            }
        })
    }

    fn add_event(&self, evt_name: &str, timeout: f64) {
        if self.pending_node_events.borrow().contains_key(evt_name) {
            self.reset_event(evt_name, timeout);
            return;
        }
        let h = self.schedule(evt_name, timeout);
        self.pending_node_events.borrow_mut().insert(evt_name.to_string(), Some(h));
    }

    fn reset_event(&self, evt_name: &str, timeout: f64) {
        if !self.pending_node_events.borrow().contains_key(evt_name) {
            return;
        }
        if let Some(Some(h)) = self.pending_node_events.borrow_mut().get_mut(evt_name).map(|h| h.take()) {
            h.abort();
        }
        let h = self.schedule(evt_name, timeout);
        self.pending_node_events.borrow_mut().insert(evt_name.to_string(), Some(h));
    }

    fn stop_event(&self, evt_name: &str) {
        if let Some(slot) = self.pending_node_events.borrow_mut().get_mut(evt_name) {
            if let Some(h) = slot.take() {
                h.abort();
            }
        }
    }

    fn remove_event(&self, evt_name: &str) {
        if let Some(Some(h)) = self.pending_node_events.borrow_mut().remove(evt_name) {
            h.abort();
        }
    }

    fn flush_delete(&self) {
        let h = match self.pending_node_events.borrow_mut().get_mut("delete_child") {
            None => return,
            Some(slot) => slot.take(),
        };
        if let Some(h) = h {
            h.abort();
        }
        self.finish_delete_child();
    }

    fn clear_events(&self, include_children: bool) {
        if include_children {
            let children: Vec<Rc<Node>> = self.children.borrow().values().cloned().collect();
            for c in children {
                c.clear_events(true);
            }
        }
        for (_, h) in self.pending_node_events.borrow_mut().drain() {
            if let Some(h) = h {
                h.abort();
            }
        }
        self.pending_deleted_children.borrow_mut().clear();
        self.pending_file_events.borrow_mut().clear();
    }

    fn search_pending_event(&self, name: &str) -> Option<Rc<Node>> {
        if self.pending_node_events.borrow().contains_key(name) {
            return Some(self.me());
        }
        if self.is_root() {
            return None;
        }
        self.parent()?.search_pending_event(name)
    }

    fn find_pending_node(&self) -> Option<Rc<Node>> {
        if self.is_processing_metadata.get() || self.pending_node_events.borrow().contains_key("create_node") {
            return Some(self.me());
        }
        if self.is_root() {
            return None;
        }
        self.parent()?.find_pending_node()
    }

    fn queue_move_notification(&self, args: NotifyArgs) {
        if self.is_processing_metadata.get() || self.pending_node_events.borrow().contains_key("create_node") {
            self.queued_move_notifications.borrow_mut().push(args);
        } else {
            if server().is_verbose_enabled() {
                log::debug!(
                    "Node {} received a move notification queue request, however node is not pending: [{}, {}, {}]",
                    self.get_path(),
                    args.action,
                    args.root,
                    args.path
                );
            }
            self.obs().notify_filelist_changed(&args.action, &args.root, &args.path, args.src_root.as_deref(), args.src_path.as_deref());
        }
    }
}

// ***** Observer *****

struct InotifyState {
    fd: i32,
    watched_roots: RefCell<IndexMap<String, Rc<Node>>>,
    watched_nodes: RefCell<IndexMap<i32, Rc<Node>>>,
    pending_moves: RefCell<HashMap<u32, (Rc<Node>, String, tokio::task::JoinHandle<()>)>>,
    initialized: Cell<bool>,
    processing_gcode_files: RefCell<IndexSet<String>>,
    pending_coroutines: RefCell<VecDeque<Coro>>,
    gc_notify_running: Cell<bool>,
    reader: RefCell<Option<tokio::task::JoinHandle<()>>>,
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
                    reader: RefCell::new(None),
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
            *st.reader.borrow_mut() = Some(spawn(async move { inotify_reader(fd, o).await }));
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
        if need_low_level_rm && node.is_some() {
            // SAFETY: st.fd is the inotify fd (never closed); a stale wd only makes the call fail
            if unsafe { libc::inotify_rm_watch(st.fd, wd) } < 0 {
                log::error!("Error removing watch: '{}'", node.unwrap().get_path());
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
                    let suffix = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
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
        let suffix = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
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
                let suffix = dp.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
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
            if let Some(r) = st.reader.borrow_mut().take() {
                r.abort();
            }
            for wd in st.watched_nodes.borrow().keys() {
                // SAFETY: st.fd is the inotify fd (never closed); a stale wd only makes the call fail
                unsafe { libc::inotify_rm_watch(st.fd, *wd) };
            }
        }
    }

    // ***** Inotify event processing *****

    fn handle_move_timeout(&self, cookie: u32, is_dir: bool) {
        let st = self.inotify.as_ref().unwrap();
        let (parent, name, _) = match st.pending_moves.borrow_mut().remove(&cookie) {
            Some(p) => p,
            None => return,
        };
        let item_path = path_join(&parent.get_path(), &name);
        let root = parent.get_root();
        self.clear_metadata(&root, &item_path, is_dir);
        let mut action = "delete_file";
        if is_dir {
            let child = match parent.children.borrow_mut().shift_remove(&name) {
                Some(c) => c,
                None => return,
            };
            child.clear_watches();
            child.clear_events(true);
            self.log_nodes();
            action = "delete_dir";
        } else {
            parent.clear_file_event(&name);
        }
        self.notify_filelist_changed(action, &root, &item_path, None, None);
    }

    fn schedule_pending_move(&self, cookie: u32, name: &str, parent: &Rc<Node>, is_dir: bool) {
        let st = self.inotify.as_ref().unwrap();
        let o = self.me.borrow().clone();
        let h = spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs_f64(INOTIFY_MOVE_TIME)).await;
            if let Some(o) = o.upgrade() {
                o.handle_move_timeout(cookie, is_dir);
            }
        });
        if let Some((_, _, old)) = st.pending_moves.borrow_mut().insert(cookie, (parent.clone(), name.to_string(), h)) {
            old.abort();
        }
    }

    fn handle_event(&self, wd: i32, mask: u32, cookie: u32, name: &str) {
        let st = self.inotify.as_ref().unwrap();
        if mask & IN_IGNORED != 0 {
            return;
        }
        let node = match st.watched_nodes.borrow().get(&wd).cloned() {
            Some(n) => n,
            None => {
                let flags: Vec<&str> = FLAG_NAMES.iter().filter(|(b, _)| mask & b != 0).map(|(_, n)| *n).collect();
                log::info!(
                    "Error, inotify watch descriptor {} not currently tracked: name: {}, flags: {}",
                    wd,
                    name,
                    flags.iter().map(|f| format!("flags.{}", f)).collect::<Vec<_>>().join(" ")
                );
                return;
            }
        };
        if mask & IN_ISDIR != 0 {
            self.process_dir_event(mask, cookie, name, &node);
        } else {
            self.process_file_event(mask, cookie, name, &node);
        }
    }

    fn process_dir_event(&self, mask: u32, cookie: u32, name: &str, node: &Rc<Node>) {
        if name == "." || name == ".." {
            return;
        }
        let fm = self.fm();
        let st = self.inotify.as_ref().unwrap();
        let root = node.get_root();
        let node_path = node.get_path();
        let full_path = path_join(&node_path, name);
        if mask & IN_CREATE != 0 {
            log::debug!("Inotify directory create: {}, {}, {}", root, node_path, name);
            if fm.is_reserved_path(&full_path, true) {
                log::debug!("Inotify - ignoring create watch at reserved path: {}", full_path);
            } else {
                fm.sync_lock.add_pending_path("create_dir", Path::new(&full_path));
                node.create_child_node(name, true);
            }
        } else if mask & IN_DELETE != 0 {
            log::debug!("Inotify directory delete: {}, {}, {}", root, node_path, name);
            node.schedule_child_delete(name, true);
        } else if mask & IN_MOVED_FROM != 0 {
            log::debug!("Inotify directory move from: {}, {}, {}", root, node_path, name);
            if node.has_child_node(name) {
                self.schedule_pending_move(cookie, name, node, true);
            } else {
                log::debug!("Inotify - Child node with name {} does not exist", name);
            }
        } else if mask & IN_MOVED_TO != 0 {
            log::debug!("Inotify directory move to: {}, {}, {}", root, node_path, name);
            let moved = st.pending_moves.borrow_mut().remove(&cookie);
            match moved {
                Some((prev_parent, child_name, h)) => {
                    fm.sync_lock.add_pending_path("move_dir", Path::new(&full_path));
                    h.abort();
                    if fm.is_reserved_path(&full_path, true) {
                        log::debug!("Inotify - deleting prev folder {} moved to reserved path: {}", child_name, full_path);
                        prev_parent.schedule_child_delete(&child_name, true);
                    } else {
                        prev_parent.move_child_node(&child_name, name, node);
                    }
                }
                None => {
                    if fm.is_reserved_path(&full_path, true) {
                        log::debug!("Inotify - ignoring moved folder to reserved path: {}", full_path);
                    } else {
                        fm.sync_lock.add_pending_path("create_dir", Path::new(&full_path));
                        node.create_child_node(name, true);
                    }
                }
            }
        }
    }

    fn process_file_event(&self, mask: u32, cookie: u32, name: &str, node: &Rc<Node>) {
        let fm = self.fm();
        let st = self.inotify.as_ref().unwrap();
        let ext = utils::splitext_ext(name).to_lowercase();
        let root = node.get_root();
        let node_path = node.get_path();
        let file_path = path_join(&node_path, name);
        if mask & IN_CREATE != 0 {
            log::debug!("Inotify file create: {}, {}, {}", root, node_path, name);
            fm.sync_lock.add_pending_path("create_file", Path::new(&file_path));
            node.schedule_file_event(name, "create_file");
            if Path::new(&file_path).is_symlink() {
                log::debug!("Inotify symlink create: {}", file_path);
                node.complete_file_write(name);
            }
        } else if mask & IN_DELETE != 0 {
            log::debug!("Inotify file delete: {}, {}, {}", root, node_path, name);
            if root == "gcodes" && ext == ".ufp" {
                return;
            }
            node.schedule_child_delete(name, false);
        } else if mask & IN_MOVED_FROM != 0 {
            log::debug!("Inotify file move from: {}, {}, {}", root, node_path, name);
            self.schedule_pending_move(cookie, name, node, false);
        } else if mask & IN_MOVED_TO != 0 {
            log::debug!("Inotify file move to: {}, {}, {}", root, node_path, name);
            node.flush_delete();
            let moved = st.pending_moves.borrow_mut().remove(&cookie);
            let pending_node = node.find_pending_node();
            match moved {
                Some((prev_parent, prev_name, h)) => {
                    fm.sync_lock.add_pending_path("move_file", Path::new(&file_path));
                    h.abort();
                    let prev_root = prev_parent.get_root();
                    let prev_path = path_join(&prev_parent.get_path(), &prev_name);
                    if let Some(prev_evt) = prev_parent.clear_file_event(&prev_name) {
                        node.schedule_file_event(name, &prev_evt);
                        if prev_evt == "create_file" {
                            self.clear_metadata(&prev_root, &prev_path, false);
                            return;
                        }
                    }
                    let obs = self.me();
                    let args = NotifyArgs {
                        action: "move_file".into(),
                        root: root.clone(),
                        path: file_path.clone(),
                        src_root: Some(prev_root.clone()),
                        src_path: Some(prev_path.clone()),
                    };
                    if root == "gcodes" {
                        let obs2 = obs.clone();
                        let fp = file_path.clone();
                        self.queue_gcode_notification(Box::pin(async move {
                            let moved = obs2.try_move_metadata_file(&prev_root, &root, &prev_path, &fp).await;
                            if !moved {
                                obs2.parse_gcode_metadata(&fp).wait().await;
                            }
                            match pending_node {
                                None => obs2.notify_filelist_changed(&args.action, &args.root, &args.path, args.src_root.as_deref(), args.src_path.as_deref()),
                                Some(pn) => pn.queue_move_notification(args),
                            }
                        }));
                    } else {
                        // the metadata move is synchronous outside the gcodes root
                        let obs2 = obs.clone();
                        let (pr, ro, pp, fp) = (prev_root.clone(), root.clone(), prev_path.clone(), file_path.clone());
                        spawn(async move {
                            obs2.try_move_metadata_file(&pr, &ro, &pp, &fp).await;
                        });
                        match pending_node {
                            None => self.notify_filelist_changed(&args.action, &args.root, &args.path, args.src_root.as_deref(), args.src_path.as_deref()),
                            Some(pn) => pn.queue_move_notification(args),
                        }
                    }
                }
                None => {
                    if let Some(pn) = pending_node {
                        log::debug!("Parent node is processing, suppressing 'create from move' notification: {}", file_path);
                        pn.reset_event("create_node", INOTIFY_BUNDLE_TIME);
                        if root == "gcodes" {
                            self.parse_gcode_metadata(&file_path);
                        }
                        return;
                    }
                    fm.sync_lock.add_pending_path("create_file", Path::new(&file_path));
                    if root == "gcodes" {
                        if self.need_create_notify(&file_path) {
                            let obs = self.me();
                            self.queue_gcode_notification(Box::pin(async move {
                                obs.parse_gcode_metadata(&file_path).wait().await;
                                obs.notify_filelist_changed("create_file", "gcodes", &file_path, None, None);
                                obs.clear_processing_file(&file_path);
                            }));
                        }
                    } else {
                        self.notify_filelist_changed("create_file", &root, &file_path, None, None);
                    }
                }
            }
        } else if mask & IN_MODIFY != 0 {
            fm.sync_lock.add_pending_path("modify_file", Path::new(&file_path));
            node.schedule_file_event(name, "modify_file");
        } else if mask & IN_CLOSE_WRITE != 0 {
            log::debug!("Inotify writable file closed: {}", file_path);
            node.complete_file_write(name);
        }
    }

    fn need_create_notify(&self, file_path: &str) -> bool {
        let ext = utils::splitext_ext(file_path).to_lowercase();
        if ext == ".ufp" {
            self.parse_gcode_metadata(file_path);
            return false;
        } else if !VALID_GCODE_EXTS.contains(&ext.as_str()) {
            return true;
        }
        let fm = self.fm();
        let rel = fm.get_relative_path("gcodes", file_path);
        let st = self.inotify.as_ref().unwrap();
        if fm.gcode_metadata.is_file_processing(&rel) && st.processing_gcode_files.borrow().contains(&rel) {
            log::debug!(
                "Inotify file create event received for file '{}' during metadata processing.  Suppressing notification.",
                rel
            );
            return false;
        }
        st.processing_gcode_files.borrow_mut().insert(rel);
        true
    }

    fn clear_processing_file(&self, file_path: &str) {
        let rel = self.fm().get_relative_path("gcodes", file_path);
        if let Some(st) = &self.inotify {
            st.processing_gcode_files.borrow_mut().shift_remove(&rel);
        }
    }

    fn queue_gcode_notification(&self, coro: Coro) {
        let st = self.inotify.as_ref().unwrap();
        st.pending_coroutines.borrow_mut().push_back(coro);
        if st.gc_notify_running.get() {
            return;
        }
        st.gc_notify_running.set(true);
        let obs = self.me();
        spawn(async move {
            loop {
                let next = obs.inotify.as_ref().unwrap().pending_coroutines.borrow_mut().pop_front();
                match next {
                    Some(c) => c.await,
                    None => break,
                }
            }
            obs.inotify.as_ref().unwrap().gc_notify_running.set(false);
        });
    }

    pub fn notify_filelist_changed(&self, action: &str, root: &str, full_path: &str, source_root: Option<&str>, source_path: Option<&str>) {
        let fm = self.fm();
        let rel = fm.get_relative_path(root, full_path);
        let sync_fut = fm.sync_lock.check_in_request(action, Path::new(full_path));
        let mut info: Map<String, Value> = Map::new();
        info.insert("size".into(), json!(0));
        info.insert("modified".into(), json!(0));
        info.insert("permissions".into(), json!(""));
        if Path::new(full_path).exists() {
            match fm.get_path_info(full_path, root, true) {
                Ok(i) => info = i,
                Err(_) => {
                    log::debug!("Invalid Filelist Notification Request, root: {}, path: {} - Failed to get path info", root, full_path);
                    return;
                }
            }
        } else if action != "delete_file" && action != "delete_dir" {
            log::debug!(
                "Invalid Filelist Notification Request, root: {}, path: {} - Action {} received for file that does not exit",
                root,
                full_path,
                action
            );
            return;
        }
        info.insert("path".into(), json!(rel));
        info.insert("root".into(), json!(root));
        let mut result = json!({"action": action, "item": Value::Object(info)});
        if let (Some(sr), Some(sp)) = (source_root, source_path) {
            let srel = fm.get_relative_path(sr, sp);
            result["source_item"] = json!({"path": srel, "root": sr});
        }
        let key = format!("{}-{}-{}", action, root, rel);
        let fmw = Rc::downgrade(&fm);
        spawn(async move {
            if let Some(f) = sync_fut {
                log::debug!("Syncing notification: {}", key);
                let _ = f.await;
            }
            if let Some(fm) = fmw.upgrade() {
                fm.cancel_notification(&key);
            }
            crate::utils::sleep_ordered(std::time::Duration::from_millis(5)).await;
            let _ = server().send_event("file_manager:filelist_changed", vec![result]);
        });
    }
}

async fn inotify_reader(fd: i32, obs: Weak<Observer>) {
    // SAFETY: the inotify fd is never closed, it lives as long as the process
    let afd = match unsafe { tokio::io::unix::AsyncFd::register(fd) } {
        Ok(a) => a,
        Err(e) => {
            log::error!("Unable to poll inotify fd: {}", e);
            return;
        }
    };
    let mut buf = vec![0u8; 65536];
    loop {
        let mut guard = match afd.readable().await {
            Ok(g) => g,
            Err(_) => return,
        };
        // SAFETY: buf is buf.len() writable bytes; the inotify fd is never closed
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            guard.clear_ready();
            continue;
        }
        let n = n as usize;
        let mut off = 0;
        let mut events = Vec::new();
        while off + 16 <= n {
            let wd = i32::from_ne_bytes(buf[off..off + 4].try_into().unwrap());
            let mask = u32::from_ne_bytes(buf[off + 4..off + 8].try_into().unwrap());
            let cookie = u32::from_ne_bytes(buf[off + 8..off + 12].try_into().unwrap());
            let len = u32::from_ne_bytes(buf[off + 12..off + 16].try_into().unwrap()) as usize;
            let name_bytes = &buf[off + 16..(off + 16 + len).min(n)];
            let end = name_bytes.iter().position(|b| *b == 0).unwrap_or(name_bytes.len());
            let name = String::from_utf8_lossy(&name_bytes[..end]).to_string();
            events.push((wd, mask, cookie, name));
            off += 16 + len;
        }
        let o = match obs.upgrade() {
            Some(o) => o,
            None => return,
        };
        for (wd, mask, cookie, name) in events {
            o.handle_event(wd, mask, cookie, &name);
        }
    }
}

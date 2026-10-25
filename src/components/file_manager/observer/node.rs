// The inotify node tree: one node per watched directory

use super::*;

pub struct Node {
    pub(super) obs: Weak<Observer>,
    pub(super) name: RefCell<String>,
    pub(super) parent: RefCell<Option<Weak<Node>>>,
    pub(super) root_name: Option<String>,
    pub(super) children: RefCell<IndexMap<String, Rc<Node>>>,
    pub(super) wd: Cell<i32>,
    pub(super) pending_node_events: RefCell<HashMap<String, Option<tokio::task::JoinHandle<()>>>>,
    pub(super) pending_deleted_children: RefCell<IndexSet<(String, bool)>>,
    pub(super) pending_file_events: RefCell<HashMap<String, String>>,
    pub(super) queued_move_notifications: RefCell<Vec<NotifyArgs>>,
    pub(super) is_processing_metadata: Cell<bool>,
    pub(super) me: RefCell<Weak<Node>>,
}

impl Node {
    pub(super) fn create(obs: &Rc<Observer>, parent: Option<&Rc<Node>>, name: &str, root_name: Option<&str>) -> Result<Rc<Node>, ()> {
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

    pub(super) fn me(&self) -> Rc<Node> {
        self.me.borrow().upgrade().expect("node")
    }

    pub(super) fn obs(&self) -> Rc<Observer> {
        self.obs.upgrade().expect("observer")
    }

    pub(super) fn parent(&self) -> Option<Rc<Node>> {
        self.parent.borrow().as_ref().and_then(|p| p.upgrade())
    }

    pub(super) fn is_root(&self) -> bool {
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

    pub(super) fn is_processing(&self) -> bool {
        if self.is_processing_metadata.get() {
            return true;
        }
        if self.is_root() {
            return false;
        }
        self.parent().map(|p| p.is_processing()).unwrap_or(false)
    }

    pub(super) async fn finish_create_node(self: Rc<Self>) {
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

    pub(super) fn finish_delete_child(&self) {
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

    pub(super) fn scan_node(&self, visited: &mut std::collections::HashSet<(u64, u64)>) -> Vec<Rc<AsyncEvent>> {
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

    pub(super) fn move_child_node(&self, child_name: &str, new_name: &str, new_parent: &Rc<Node>) {
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

    pub(super) fn schedule_file_event(&self, file_name: &str, evt_name: &str) {
        if self.pending_file_events.borrow().contains_key(file_name) {
            return;
        }
        if let Some(pn) = self.search_pending_event("create_node") {
            pn.stop_event("create_node");
        }
        self.pending_file_events.borrow_mut().insert(file_name.to_string(), evt_name.to_string());
    }

    pub(super) fn clear_file_event(&self, file_name: &str) -> Option<String> {
        let evt = self.pending_file_events.borrow_mut().remove(file_name);
        if evt.is_some() {
            if let Some(pn) = self.search_pending_event("create_node") {
                pn.reset_event("create_node", INOTIFY_BUNDLE_TIME);
            }
        }
        evt
    }

    pub(super) fn complete_file_write(&self, file_name: &str) {
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

    pub(super) fn add_child_node(&self, node: &Rc<Node>) {
        self.children.borrow_mut().insert(node.name.borrow().clone(), node.clone());
        *node.parent.borrow_mut() = Some(self.me.borrow().clone());
    }

    pub(super) fn has_child_node(&self, name: &str) -> bool {
        self.children.borrow().contains_key(name)
    }

    pub(super) fn create_child_node(&self, name: &str, notify: bool) -> Option<Rc<Node>> {
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

    pub(super) fn schedule_child_delete(&self, child_name: &str, is_node: bool) {
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

    pub(super) fn clear_watches(&self) {
        let children: Vec<Rc<Node>> = self.children.borrow().values().cloned().collect();
        for c in children {
            c.clear_watches();
        }
        self.obs().remove_watch(self.wd.get(), true);
    }

    pub(super) fn schedule(&self, evt_name: &str, timeout: f64) -> tokio::task::JoinHandle<()> {
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

    pub(super) fn add_event(&self, evt_name: &str, timeout: f64) {
        if self.pending_node_events.borrow().contains_key(evt_name) {
            self.reset_event(evt_name, timeout);
            return;
        }
        let h = self.schedule(evt_name, timeout);
        self.pending_node_events.borrow_mut().insert(evt_name.to_string(), Some(h));
    }

    pub(super) fn reset_event(&self, evt_name: &str, timeout: f64) {
        if !self.pending_node_events.borrow().contains_key(evt_name) {
            return;
        }
        if let Some(Some(h)) = self.pending_node_events.borrow_mut().get_mut(evt_name).map(|h| h.take()) {
            h.abort();
        }
        let h = self.schedule(evt_name, timeout);
        self.pending_node_events.borrow_mut().insert(evt_name.to_string(), Some(h));
    }

    pub(super) fn stop_event(&self, evt_name: &str) {
        if let Some(slot) = self.pending_node_events.borrow_mut().get_mut(evt_name) {
            if let Some(h) = slot.take() {
                h.abort();
            }
        }
    }

    pub(super) fn remove_event(&self, evt_name: &str) {
        if let Some(Some(h)) = self.pending_node_events.borrow_mut().remove(evt_name) {
            h.abort();
        }
    }

    pub(super) fn flush_delete(&self) {
        let h = match self.pending_node_events.borrow_mut().get_mut("delete_child") {
            None => return,
            Some(slot) => slot.take(),
        };
        if let Some(h) = h {
            h.abort();
        }
        self.finish_delete_child();
    }

    pub(super) fn clear_events(&self, include_children: bool) {
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

    pub(super) fn search_pending_event(&self, name: &str) -> Option<Rc<Node>> {
        if self.pending_node_events.borrow().contains_key(name) {
            return Some(self.me());
        }
        if self.is_root() {
            return None;
        }
        self.parent()?.search_pending_event(name)
    }

    pub(super) fn find_pending_node(&self) -> Option<Rc<Node>> {
        if self.is_processing_metadata.get() || self.pending_node_events.borrow().contains_key("create_node") {
            return Some(self.me());
        }
        if self.is_root() {
            return None;
        }
        self.parent()?.find_pending_node()
    }

    pub(super) fn queue_move_notification(&self, args: NotifyArgs) {
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

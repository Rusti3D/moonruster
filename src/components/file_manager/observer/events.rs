// Inotify event processing

use super::*;

impl Observer {
    pub(super) fn handle_move_timeout(&self, cookie: u32, is_dir: bool) {
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

    pub(super) fn schedule_pending_move(&self, cookie: u32, name: &str, parent: &Rc<Node>, is_dir: bool) {
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

    pub(super) fn handle_event(&self, wd: i32, mask: u32, cookie: u32, name: &str) {
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

    pub(super) fn process_dir_event(&self, mask: u32, cookie: u32, name: &str, node: &Rc<Node>) {
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

    pub(super) fn process_file_event(&self, mask: u32, cookie: u32, name: &str, node: &Rc<Node>) {
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

    pub(super) fn need_create_notify(&self, file_path: &str) -> bool {
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

    pub(super) fn clear_processing_file(&self, file_path: &str) {
        let rel = self.fm().get_relative_path("gcodes", file_path);
        if let Some(st) = &self.inotify {
            st.processing_gcode_files.borrow_mut().shift_remove(&rel);
        }
    }

    pub(super) fn queue_gcode_notification(&self, coro: Coro) {
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
            server().send_event("file_manager:filelist_changed", vec![result]);
        });
    }
}


pub(super) async fn inotify_reader(fd: i32, obs: Weak<Observer>) {
    // SAFETY: the inotify fd is never closed, it lives as long as the process
    let afd = match unsafe { tokio::io::unix::AsyncFd::register(fd) } {
        Ok(a) => a,
        Err(e) => {
            log::error!("Unable to poll inotify fd: {}", e);
            return;
        }
    };
    // room for several events: inotify(7) needs at least one with the
    // longest name (16 + NAME_MAX + 1 bytes), and a burst is read in turns
    // (the fd stays ready until it is drained)
    let mut buf = vec![0u8; 4096];
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

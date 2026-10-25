// Print handling: downloading and starting a print

use super::*;

impl SimplyPrint {
    pub(super) async fn download_sp_file(self: Rc<Self>, url: &str, start: bool) {
        let srv = server();
        let fm = match srv.get::<crate::components::file_manager::FileManager>() {
            Some(f) => f,
            None => return,
        };
        let gc_path = PathBuf::from(fm.get_directory("gcodes"));
        if !gc_path.is_dir() {
            log::debug!("GCode Path Not Registered: {}", gc_path.display());
            self.send_sp("file_progress", json!({"state": "error", "message": "GCode Path not Registered"}));
            return;
        }
        self.on_download_progress(0);
        log::debug!("Downloading URL: {}", url);
        let w = Rc::downgrade(&self);
        let cb: crate::components::http_client::ProgressCallback = Rc::new(move |pct, _size, _recd| {
            if let Some(s) = w.upgrade() {
                s.on_download_progress(pct);
            }
        });
        let res = Self::http().download_file(url, "text/plain,applicaton/octet-stream", None, -1, Some(cb), 3600.0, 1, 1.0).await;
        self.download_progress.set(-1);
        let tmp_path = match res {
            Ok(p) => p,
            Err(e) => {
                log::error!("Failed to download file: {}\n{}", url, e.message);
                self.send_sp("file_progress", json!({"state": "error", "message": "Network Error"}));
                return;
            }
        };
        log::debug!("Simplyprint: Download Complete");
        let fname = tmp_path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut fpath = gc_path.join(&fname);
        if self.cache.borrow().job_info.get("filename").map(crate::pyfmt::py_str).unwrap_or_default() == fpath.display().to_string() {
            let stem = crate::pycompat::pathlib::stem(&fname);
            let suffix = crate::pycompat::pathlib::suffix(&fname);
            let mut count = 0;
            while fpath.exists() {
                fpath = gc_path.join(format!("{}_copy_{}.{}", stem, count, suffix));
                count += 1;
            }
        }
        let fpath_name = fpath.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut args = Map::new();
        args.insert("filename".into(), json!(fpath_name));
        args.insert("tmp_file_path".into(), json!(tmp_path.display().to_string()));
        let mut state = "pending";
        if self.cache.borrow().state == "operational" {
            args.insert("print".into(), json!(if start { "true" } else { "false" }));
        }
        let ret = match fm.finalize_upload(args, None).await {
            Ok(r) => r,
            Err(e) => {
                log::error!("GCode Finalization Failed\n{}", e.message);
                self.send_sp("file_progress", json!({"state": "error", "message": format!("GCode Finalization Failed: {}", e.message)}));
                return;
            }
        };
        *self.pending_file.borrow_mut() = fpath_name;
        if ret.get("print_started").and_then(|v| v.as_bool()).unwrap_or(false) {
            state = "started";
            *self.last_started.borrow_mut() = std::mem::take(&mut *self.pending_file.borrow_mut());
        } else if !start && self.check_can_print().await {
            state = "ready";
        }
        if state == "pending" {
            self.print_ready_event.clear();
            if tokio::time::timeout(Duration::from_secs(10), self.print_ready_event.wait()).await.is_err() {
                self.pending_file.borrow_mut().clear();
                self.send_sp("file_progress", json!({"state": "error", "message": "Pending print timed out"}));
                return;
            }
            if start {
                self.start_print().await;
                return;
            }
            state = "ready";
        }
        self.send_sp("file_progress", json!({"state": state}));
    }

    pub(super) async fn start_print(&self) {
        let pending = std::mem::take(&mut *self.pending_file.borrow_mut());
        if pending.is_empty() {
            return;
        }
        let user = UserInfo::new("SimplyPrint", "");
        let mut data = json!({"state": "started"});
        match server().klippy_apis().start_print(&pending, false, Some(user)).await {
            Err(e) => {
                log::error!("Print Failed to start\n{}", e.message);
                data["state"] = json!("error");
                data["message"] = json!("Failed to start print");
            }
            Ok(_) => *self.last_started.borrow_mut() = pending,
        }
        self.send_sp("file_progress", data);
    }

    pub(super) async fn check_can_print(&self) -> bool {
        if server().klippy().state() != KlippyState::READY {
            return false;
        }
        let mut q = Map::new();
        q.insert("print_stats".into(), Value::Null);
        match server().klippy_apis().query_objects(&q).await {
            Err(_) => false,
            Ok(r) => match r.get("print_stats").and_then(|p| p.get("state")).and_then(|v| v.as_str()) {
                None => false,
                Some(s) => s != "printing" && s != "paused",
            },
        }
    }

    pub(super) fn on_download_progress(&self, percent: i64) {
        if percent == self.download_progress.get() {
            return;
        }
        self.download_progress.set(percent);
        self.send_sp("file_progress", json!({"state": "downloading", "percent": percent}));
    }
}

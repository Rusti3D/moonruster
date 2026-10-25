// Helpers: the flex timer, the protocol logger, the report cache, layer and ambient detection

use super::*;

/// FlexTimer equivalent: the callback returns the delay until the next call
pub(super) struct Timer {
    pub(super) handle: RefCell<Option<tokio::task::JoinHandle<()>>>,
}

impl Timer {
    pub(super) fn new() -> Self {
        Timer { handle: RefCell::new(None) }
    }
    pub(super) fn start(&self, delay: f64, cb: Rc<dyn Fn() -> f64>) {
        self.stop();
        let h = spawn(async move {
            let mut d = delay;
            loop {
                if d > 0.0 {
                    tokio::time::sleep(Duration::from_secs_f64(d)).await;
                }
                d = cb();
            }
        });
        *self.handle.borrow_mut() = Some(h);
    }
    pub(super) fn stop(&self) {
        if let Some(h) = self.handle.borrow_mut().take() {
            h.abort();
        }
    }
    pub(super) fn is_running(&self) -> bool {
        self.handle.borrow().is_some()
    }
}

/// Protocol logger, active only with verbose logging
pub(super) struct ProtoLogger {
    pub(super) path: Option<PathBuf>,
}

impl ProtoLogger {
    pub(super) fn new() -> Self {
        let srv = server();
        if !srv.is_verbose_enabled() {
            return ProtoLogger { path: None };
        }
        let root = srv
            .lookup_component::<crate::components::file_manager::FileManager>("file_manager")
            .map(|f| f.get_directory("logs"))
            .unwrap_or_default();
        let parent = if root.is_empty() { std::env::temp_dir() } else { PathBuf::from(root) };
        ProtoLogger { path: Some(parent.join("simplyprint.log")) }
    }
    pub(super) fn write(&self, func: &str, msg: &str) {
        let path = match &self.path {
            Some(p) => p,
            None => return,
        };
        // TimedRotatingFileHandler(when='midnight', backupCount=2)
        if let Ok(md) = std::fs::metadata(path) {
            if let Ok(m) = md.modified() {
                let day = chrono::DateTime::<chrono::Local>::from(m).date_naive();
                if day != chrono::Local::now().date_naive() {
                    let dest = PathBuf::from(format!("{}.{}", path.display(), day.format("%Y-%m-%d")));
                    let _ = std::fs::rename(path, &dest);
                    if let Some(parent) = path.parent() {
                        let prefix = format!("{}.", path.file_name().unwrap().to_string_lossy());
                        let mut olds: Vec<PathBuf> = std::fs::read_dir(parent)
                            .map(|r| r.flatten().map(|e| e.path()).filter(|p| p.file_name().map(|n| n.to_string_lossy().starts_with(&prefix)).unwrap_or(false)).collect())
                            .unwrap_or_default();
                        olds.sort();
                        while olds.len() > 2 {
                            let _ = std::fs::remove_file(olds.remove(0));
                        }
                    }
                }
            }
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S,%3f");
            let _ = writeln!(f, "{} [{}()] - {}", ts, func, msg);
        }
    }
    pub(super) fn info(&self, msg: &str) {
        self.write("info", msg);
    }
    pub(super) fn debug(&self, _msg: &str) {}
}

#[derive(Default)]
pub(super) struct ReportCache {
    pub(super) state: String,
    pub(super) temps: Map<String, Value>,
    pub(super) metadata: Value,
    pub(super) mesh: Value,
    pub(super) job_info: Map<String, Value>,
    pub(super) exclude_object_status: Map<String, Value>,
    pub(super) active_extruder: String,
    pub(super) firmware_info: Map<String, Value>,
    pub(super) machine_info: Map<String, Value>,
    pub(super) cpu_info: Map<String, Value>,
    pub(super) throttled_state: Value,
    pub(super) current_wsid: Option<u64>,
    pub(super) filament_state: String,
}

impl ReportCache {
    pub(super) fn reset_print_state(&mut self) {
        self.temps = Map::new();
        self.mesh = json!({});
        self.job_info = Map::new();
        self.exclude_object_status = Map::new();
    }
}

pub(super) struct LayerDetect {
    pub(super) layer: i64,
    pub(super) layer_z: f64,
    pub(super) active: bool,
    pub(super) layer_height: f64,
    pub(super) fl_height: f64,
    pub(super) layer_count: i64,
    pub(super) check_next: bool,
}

impl LayerDetect {
    pub(super) fn new() -> Self {
        LayerDetect { layer: 0, layer_z: 0.0, active: false, layer_height: 0.0, fl_height: 0.0, layer_count: 99999999999, check_next: false }
    }
    pub(super) fn update(&mut self, pos: &Value) {
        let z = pos.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0);
        if !self.active || self.layer_z == z || self.layer_height == 0.0 {
            self.check_next = false;
            return;
        }
        if !self.check_next {
            self.check_next = true;
            return;
        }
        self.check_next = false;
        let layer = 1 + ((z - self.fl_height) / self.layer_height + 0.5) as i64;
        self.layer = layer.min(self.layer_count);
        self.layer_z = z;
    }
    pub(super) fn start(&mut self, md: &Value) {
        *self = LayerDetect::new();
        let lh = md.get("layer_height").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let flh = md.get("first_layer_height").and_then(|v| v.as_f64()).unwrap_or(lh);
        if lh > 0.000001 && flh > 0.000001 {
            self.active = true;
            self.layer_height = lh;
            self.fl_height = flh;
            if let Some(lc) = md.get("layer_count").and_then(|v| v.as_i64()) {
                self.layer_count = lc;
            } else if let Some(oh) = md.get("object_height").and_then(|v| v.as_f64()) {
                self.layer_count = ((oh - flh) / lh + 0.5) as i64;
            }
        }
    }
    pub(super) fn resume(&mut self) {
        if self.layer_height > 0.0 {
            self.active = true;
        }
    }
    pub(super) fn stop(&mut self) {
        self.active = false;
    }
}

pub(super) struct AmbientDetect {
    pub(super) initial_sample: Cell<f64>,
    pub(super) ambient: Cell<i64>,
    pub(super) last_sample_time: Cell<f64>,
    pub(super) update_interval: Cell<f64>,
    pub(super) timer: Timer,
    pub(super) sensor_name: String,
}

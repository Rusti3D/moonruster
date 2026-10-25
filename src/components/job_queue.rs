// Print Job Queue Implementation (port of moonraker/components/job_queue.py)

use crate::common::*;
use crate::confighelper::ConfigHelper;
use crate::server::{server, spawn, Component};
use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[derive(Clone)]
pub struct QueuedJob {
    pub filename: String,
    pub job_id: String,
    pub time_added: f64,
    pub user: Option<UserInfo>,
}

impl QueuedJob {
    fn new(filename: &str, user: Option<UserInfo>) -> Self {
        let id = crate::components::websockets::next_uid();
        QueuedJob { filename: filename.into(), job_id: format!("{:016X}", id), time_added: crate::utils::time_now(), user }
    }
    fn as_dict(&self, cur_time: f64) -> Value {
        json!({
            "filename": self.filename,
            "job_id": self.job_id,
            "time_added": self.time_added,
            "time_in_queue": cur_time - self.time_added,
        })
    }
}

pub struct JobQueue {
    queued_jobs: RefCell<IndexMap<String, QueuedJob>>,
    lock: tokio::sync::Mutex<()>,
    pause_requested: Cell<bool>,
    load_on_start: bool,
    automatic: bool,
    queue_state: RefCell<String>,
    job_delay: f64,
    job_transition_gcode: String,
    pop_queue_handle: RefCell<Option<tokio::task::JoinHandle<()>>>,
    me: RefCell<std::rc::Weak<JobQueue>>,
}

#[async_trait(?Send)]
impl Component for JobQueue {
    async fn close(self: Rc<Self>) {
        self.pause_queue().await;
    }
}

macro_rules! jq_endpoint {
    ($srv:expr, $jq:expr, $path:expr, $rt:expr, $method:ident) => {{
        let j = $jq.clone();
        $srv.register_endpoint($path, $rt, callback(move |req: WebRequest| {
            let j = j.clone();
            async move { j.$method(req).await }
        }))?;
    }};
}

impl JobQueue {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, crate::confighelper::ConfigError> {
        let srv = server();
        let load_on_start = config.getboolean_or("load_on_startup", false)?;
        let automatic = config.getboolean_or("automatic_transition", false)?;
        let job_delay = config.getfloat_or("job_transition_delay", 0.01)?;
        if job_delay <= 0.0 {
            return Err(ConfigHelper::error("Value for option 'job_transition_delay' in section [job_queue] must be above 0.0"));
        }
        let jq = Rc::new(JobQueue {
            queued_jobs: RefCell::new(IndexMap::new()),
            lock: tokio::sync::Mutex::new(()),
            pause_requested: Cell::new(false),
            load_on_start,
            automatic,
            queue_state: RefCell::new("paused".into()),
            job_delay,
            job_transition_gcode: config.get_or("job_transition_gcode", "")?.trim().to_string(),
            pop_queue_handle: RefCell::new(None),
            me: RefCell::new(std::rc::Weak::new()),
        });
        *jq.me.borrow_mut() = Rc::downgrade(&jq);
        let j = Rc::downgrade(&jq);
        srv.on_event_async("server:klippy_ready", move |_| {
            let j = j.clone();
            async move {
                if let Some(jq) = j.upgrade() {
                    jq.handle_ready().await;
                }
            }
        });
        let j = Rc::downgrade(&jq);
        srv.on_event_async("server:klippy_shutdown", move |_| {
            let j = j.clone();
            async move {
                if let Some(jq) = j.upgrade() {
                    let had = jq.pause_requested.get();
                    jq.pause_queue().await;
                    jq.pause_requested.set(had);
                }
            }
        });
        let j = Rc::downgrade(&jq);
        srv.on_event_async("job_state:state_changed", move |args| {
            let j = j.clone();
            async move {
                if let Some(jq) = j.upgrade() {
                    let ev = args.first().and_then(|v| v.as_str()).and_then(|s| JobEvent::from_string(s).ok());
                    match ev {
                        Some(JobEvent::Complete) => jq.on_job_complete().await,
                        Some(e) if e.aborted() => {
                            let _g = jq.lock.lock().await;
                            if !jq.queued_jobs.borrow().is_empty() {
                                jq.set_queue_state("paused");
                            }
                        }
                        _ => {}
                    }
                }
            }
        });
        srv.register_notification("job_queue:job_queue_changed", None);
        let j = jq.clone();
        srv.register_remote_method(
            "pause_job_queue",
            Rc::new(move |_| {
                let j = j.clone();
                Some(Box::pin(async move { j.pause_queue().await }))
            }),
        )?;
        let j = jq.clone();
        srv.register_remote_method(
            "start_job_queue",
            Rc::new(move |_| {
                let j = j.clone();
                Some(Box::pin(async move { j.start_queue().await }))
            }),
        )?;
        jq_endpoint!(srv, jq, "/server/job_queue/job", RequestType::POST | RequestType::DELETE, handle_job_request);
        jq_endpoint!(srv, jq, "/server/job_queue/pause", RequestType::POST, handle_pause_queue);
        jq_endpoint!(srv, jq, "/server/job_queue/start", RequestType::POST, handle_start_queue);
        jq_endpoint!(srv, jq, "/server/job_queue/status", RequestType::GET, handle_queue_status);
        jq_endpoint!(srv, jq, "/server/job_queue/jump", RequestType::POST, handle_jump);
        Ok(jq)
    }

    fn schedule_pop(&self, delay: f64, need_transition: bool) {
        let me = self.me.borrow().clone();
        let h = spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs_f64(delay)).await;
            if let Some(jq) = me.upgrade() {
                jq.pop_job(need_transition).await;
            }
        });
        *self.pop_queue_handle.borrow_mut() = Some(h);
    }

    async fn handle_ready(&self) {
        let _g = self.lock.lock().await;
        if !self.load_on_start || self.queued_jobs.borrow().is_empty() {
            return;
        }
        let st = self.queue_state.borrow().clone();
        if st == "ready" || st == "paused" {
            self.set_queue_state("loading");
            self.schedule_pop(1.0, false);
        }
    }

    async fn on_job_complete(&self) {
        if !self.automatic {
            return;
        }
        let _g = self.lock.lock().await;
        if *self.queue_state.borrow() == "ready" && !self.queued_jobs.borrow().is_empty() {
            self.set_queue_state("loading");
            self.schedule_pop(self.job_delay, true);
        }
    }

    async fn pop_job(&self, need_transition: bool) {
        *self.pop_queue_handle.borrow_mut() = None;
        let _g = self.lock.lock().await;
        if *self.queue_state.borrow() == "paused" {
            return;
        }
        let first = self.queued_jobs.borrow().get_index(0).map(|(k, v)| (k.clone(), v.clone()));
        let (uid, job) = match first {
            Some(f) => f,
            None => {
                self.set_queue_state("paused");
                return;
            }
        };
        let apis = server().klippy_apis();
        let can_print = self.check_can_print().await;
        if !can_print || *self.queue_state.borrow() != "loading" {
            self.set_queue_state("paused");
            return;
        }
        let res: Result<(), ServerError> = async {
            if !self.job_transition_gcode.is_empty() && need_transition {
                apis.run_gcode(&self.job_transition_gcode).await?;
                if *self.queue_state.borrow() != "loading" {
                    self.set_queue_state("paused");
                    return Err(ServerError::bad("Queue State Changed during Transition Gcode"));
                }
            }
            self.set_queue_state("starting");
            apis.start_print(&job.filename, true, job.user.clone()).await?;
            Ok(())
        }
        .await;
        match res {
            Err(e) => {
                log::error!("Error Loading print: {}\n{}", job.filename, e);
                self.set_queue_state("paused");
            }
            Ok(_) => {
                self.queued_jobs.borrow_mut().shift_remove(&uid);
                if *self.queue_state.borrow() == "starting" {
                    let set_ready = !self.queued_jobs.borrow().is_empty() && self.automatic;
                    *self.queue_state.borrow_mut() = if set_ready { "ready".into() } else { "paused".into() };
                }
                self.send_queue_event("job_loaded");
            }
        }
    }

    async fn check_can_print(&self) -> bool {
        let mut q = serde_json::Map::new();
        q.insert("print_stats".into(), Value::Null);
        match server().klippy_apis().query_objects(&q).await {
            Err(_) => false,
            Ok(r) => match r.get("print_stats") {
                None => false,
                Some(ps) => {
                    let st = ps.get("state").and_then(|v| v.as_str()).unwrap_or("");
                    !(st == "printing" || st == "paused")
                }
            },
        }
    }

    pub async fn queue_job(&self, filenames: Vec<String>, check_exists: bool, reset: bool, user: Option<UserInfo>) -> Result<(), ServerError> {
        let _g = self.lock.lock().await;
        if check_exists {
            for f in &filenames {
                self.check_job_file(f)?;
            }
        }
        if reset {
            self.queued_jobs.borrow_mut().clear();
        }
        for f in &filenames {
            let job = QueuedJob::new(f, user.clone());
            self.queued_jobs.borrow_mut().insert(job.job_id.clone(), job);
        }
        self.send_queue_event("jobs_added");
        if self.automatic && !self.pause_requested.get() {
            if let Some(js) = server().get::<crate::components::job_state::JobState>() {
                let ev = js.get_last_job_event();
                if ev.is_printing() || ev == JobEvent::Paused {
                    self.set_queue_state("ready");
                }
            }
        }
        Ok(())
    }

    pub async fn delete_job(&self, job_ids: Vec<String>, all: bool) {
        let _g = self.lock.lock().await;
        if self.queued_jobs.borrow().is_empty() {
            return;
        }
        if all {
            self.queued_jobs.borrow_mut().clear();
        } else if !job_ids.is_empty() {
            for id in job_ids {
                self.queued_jobs.borrow_mut().shift_remove(&id);
            }
        } else {
            return;
        }
        self.send_queue_event("jobs_removed");
    }

    pub async fn pause_queue(&self) {
        self.set_queue_state("paused");
        if let Some(h) = self.pop_queue_handle.borrow_mut().take() {
            h.abort();
        }
        let _g = self.lock.lock().await;
        self.pause_requested.set(true);
    }

    pub async fn start_queue(&self) {
        let _g = self.lock.lock().await;
        self.pause_requested.set(false);
        let st = self.queue_state.borrow().clone();
        if st == "ready" || st == "paused" {
            if self.queued_jobs.borrow().is_empty() {
                self.set_queue_state("paused");
            } else if self.check_can_print().await {
                self.set_queue_state("loading");
                self.schedule_pop(0.01, false);
            } else {
                self.set_queue_state(if self.automatic { "ready" } else { "paused" });
            }
        }
    }

    fn job_map_to_list(&self) -> Vec<Value> {
        let now = crate::utils::time_now();
        self.queued_jobs.borrow().values().map(|j| j.as_dict(now)).collect()
    }

    fn check_job_file(&self, name: &str) -> Result<(), ServerError> {
        let fm: Rc<crate::components::file_manager::FileManager> =
            server().get::<crate::components::file_manager::FileManager>().ok_or_else(|| ServerError::new("file_manager", 500))?;
        if !fm.check_file_exists("gcodes", name, None) {
            return Err(ServerError::bad(format!("G-Code File {} does not exist", name)));
        }
        Ok(())
    }

    fn set_queue_state(&self, new_state: &str) {
        if *self.queue_state.borrow() != new_state {
            *self.queue_state.borrow_mut() = new_state.to_string();
            self.send_queue_event("state_changed");
        }
    }

    fn send_queue_event(&self, action: &str) {
        let updated = if action != "state_changed" { Value::Array(self.job_map_to_list()) } else { Value::Null };
        let payload = json!({"action": action, "updated_queue": updated, "queue_state": self.queue_state.borrow().clone()});
        spawn(async move {
            crate::utils::sleep_ordered(std::time::Duration::from_millis(50)).await;
            server().send_event("job_queue:job_queue_changed", vec![payload]);
        });
    }

    fn status(&self) -> Value {
        json!({"queued_jobs": self.job_map_to_list(), "queue_state": self.queue_state.borrow().clone()})
    }

    async fn handle_job_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let rt = req.get_request_type();
        if rt == RequestType::POST {
            let files = req.get_list("filenames")?;
            let reset = req.get_boolean_or("reset", false)?;
            let user = req.get_current_user().cloned();
            self.queue_job(files, true, reset, user).await?;
        } else if rt == RequestType::DELETE {
            if req.get_boolean_or("all", false)? {
                self.delete_job(vec![], true).await;
            } else {
                let ids = req.get_list("job_ids")?;
                self.delete_job(ids, false).await;
            }
        } else {
            return Err(ServerError::bad(format!("Invalid request type: {}", rt)));
        }
        Ok(self.status())
    }

    async fn handle_pause_queue(self: Rc<Self>, _req: WebRequest) -> ApiResult {
        self.pause_queue().await;
        Ok(self.status())
    }

    async fn handle_start_queue(self: Rc<Self>, _req: WebRequest) -> ApiResult {
        self.start_queue().await;
        Ok(self.status())
    }

    async fn handle_queue_status(self: Rc<Self>, _req: WebRequest) -> ApiResult {
        Ok(self.status())
    }

    async fn handle_jump(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let job_id = crate::pyfmt::py_str(&req.get("job_id")?);
        {
            let _g = self.lock.lock().await;
            let job = self
                .queued_jobs
                .borrow_mut()
                .shift_remove(&job_id)
                .ok_or_else(|| ServerError::bad(format!("Invalid job id: {}", job_id)))?;
            let mut nq = IndexMap::new();
            nq.insert(job_id.clone(), job);
            for (k, v) in self.queued_jobs.borrow_mut().drain(..) {
                nq.insert(k, v);
            }
            *self.queued_jobs.borrow_mut() = nq;
        }
        Ok(self.status())
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<JobQueue>, crate::confighelper::ConfigError> {
    JobQueue::new(config)
}

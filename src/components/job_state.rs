// Klippy job state event handlers (port of moonraker/components/job_state.py)

use crate::common::*;
use crate::confighelper::ConfigHelper;
use crate::server::{server, Component, EventObject};
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

pub struct JobState {
    last_print_stats: RefCell<Map<String, Value>>,
    last_event: Cell<JobEvent>,
}

impl Component for JobState {}

impl JobState {
    pub fn new(_config: &ConfigHelper) -> Rc<Self> {
        let js = Rc::new(JobState { last_print_stats: RefCell::new(Map::new()), last_event: Cell::new(JobEvent::Standby) });
        let srv = server();
        let j = Rc::downgrade(&js);
        srv.on_event_async("server:klippy_started", move |args| {
            let j = j.clone();
            async move {
                if let Some(js) = j.upgrade() {
                    js.handle_started(args).await;
                }
            }
        });
        let j = Rc::downgrade(&js);
        srv.on_event("server:klippy_disconnect", move |_| {
            if let Some(js) = j.upgrade() {
                let mut ps = js.last_print_stats.borrow_mut();
                let state = ps.get("state").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if state == "printing" || state == "paused" {
                    ps.insert("state".into(), json!("error"));
                    js.last_event.set(JobEvent::Error);
                }
            }
        });
        js
    }

    async fn handle_started(self: Rc<Self>, args: Vec<Value>) {
        if args.first().and_then(|v| v.as_str()) != Some("ready") {
            return;
        }
        let kapis = server().klippy_apis();
        let mut sub = Map::new();
        sub.insert("print_stats".into(), Value::Null);
        let j = Rc::downgrade(&self);
        let cb: crate::components::klippy_apis::SubCallback = Rc::new(move |data, _et| {
            if let Some(js) = j.upgrade() {
                js.status_update(&data);
            }
            None
        });
        let result = match kapis.subscribe_objects(&sub, Some(cb)).await {
            Ok(r) => r,
            Err(_) => {
                log::info!("Error subscribing to print_stats");
                return;
            }
        };
        let ps = result.get("print_stats").and_then(|v| v.as_object()).cloned().unwrap_or_default();
        *self.last_print_stats.borrow_mut() = ps.clone();
        if let Some(state) = ps.get("state") {
            log::info!("Job state initialized: {}", crate::pyfmt::py_str(state));
        }
    }

    fn status_update(&self, data: &Map<String, Value>) {
        let ps = match data.get("print_stats").and_then(|v| v.as_object()) {
            Some(p) => p.clone(),
            None => return,
        };
        if let Some(new_state_v) = ps.get("state") {
            let prev_ps = self.last_print_stats.borrow().clone();
            let old_state = prev_ps.get("state").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let mut new_state = new_state_v.as_str().unwrap_or("").to_string();
            let mut new_ps = prev_ps.clone();
            for (k, v) in &ps {
                new_ps.insert(k.clone(), v.clone());
            }
            if new_state != old_state {
                if Self::check_idle(&old_state) {
                    if new_state == "printing" {
                        new_state = "started".into();
                    } else if new_state != "standby" {
                        log::info!("Job started event inferred: transition from {} to {} state", old_state, new_state);
                        self.send_job_event("started", &prev_ps, &new_ps);
                    }
                } else if new_state == "printing" {
                    new_state = if Self::check_resumed(&prev_ps, &new_ps) { "resumed".into() } else { "started".into() };
                }
                log::debug!("Job State Changed - Prev State: {}, New State: {}", old_state, new_state);
                self.send_job_event(&new_state, &prev_ps, &new_ps);
            }
        }
        if let Some(info) = ps.get("info").and_then(|v| v.as_object()) {
            if let Some(cur_layer) = info.get("current_layer").filter(|v| !v.is_null()) {
                let total = info.get("total_layer").cloned().unwrap_or(json!(0));
                let _ = server().send_event("job_state:layer_changed", vec![cur_layer.clone(), total]);
            }
        }
        let mut lps = self.last_print_stats.borrow_mut();
        for (k, v) in ps {
            lps.insert(k, v);
        }
    }

    fn check_resumed(prev: &Map<String, Value>, new: &Map<String, Value>) -> bool {
        let pd = prev.get("total_duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let nd = new.get("total_duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
        prev.get("state").and_then(|v| v.as_str()) == Some("paused") && prev.get("filename") == new.get("filename") && pd < nd
    }

    fn check_idle(state: &str) -> bool {
        matches!(state, "standby" | "complete" | "cancelled" | "error")
    }

    fn send_job_event(&self, state: &str, prev_ps: &Map<String, Value>, new_ps: &Map<String, Value>) {
        if state == "started" {
            log::info!("Job Started: {}", new_ps.get("filename").map(crate::pyfmt::py_str).unwrap_or_default());
        }
        let srv = server();
        let _ = srv.send_event(&format!("job_state:{}", state), vec![Value::Object(prev_ps.clone()), Value::Object(new_ps.clone())]);
        let ev = match JobEvent::from_string(state) {
            Ok(e) => e,
            Err(e) => {
                log::error!("{}", e);
                return;
            }
        };
        self.last_event.set(ev);
        let _ = srv.send_event_object(
            "job_state:state_changed",
            vec![json!(ev.as_str()), Value::Object(prev_ps.clone()), Value::Object(new_ps.clone())],
            EventObject::JobEvent(ev.as_str().to_string()),
        );
    }

    pub fn get_last_stats(&self) -> Map<String, Value> {
        self.last_print_stats.borrow().clone()
    }

    pub fn get_last_job_event(&self) -> JobEvent {
        self.last_event.get()
    }
}

pub fn load_component(config: &ConfigHelper) -> Rc<JobState> {
    JobState::new(config)
}

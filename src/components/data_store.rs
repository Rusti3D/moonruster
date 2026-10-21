// Klipper data logging and storage (port of moonraker/components/data_store.py)

use crate::common::*;
use crate::confighelper::ConfigHelper;
use crate::server::{server, spawn, Component};
use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

const TEMP_UPDATE_TIME: f64 = 1.0;

fn round_null(v: &Value) -> Value {
    crate::pyfmt::round_value(v, 2)
}

pub struct DataStore {
    temp_store_size: usize,
    gcode_store_size: usize,
    gcode_queue: RefCell<VecDeque<Value>>,
    temperature_store: RefCell<IndexMap<String, IndexMap<String, VecDeque<Value>>>>,
    temp_monitors: RefCell<Vec<String>>,
    timer: RefCell<Option<tokio::task::JoinHandle<()>>>,
}

#[async_trait(?Send)]
impl Component for DataStore {
    async fn close(self: Rc<Self>) {
        self.stop_timer();
    }
}

impl DataStore {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, crate::confighelper::ConfigError> {
        let ds = Rc::new(DataStore {
            temp_store_size: config.getint_or("temperature_store_size", 1200)?.max(0) as usize,
            gcode_store_size: config.getint_or("gcode_store_size", 1000)?.max(0) as usize,
            gcode_queue: RefCell::new(VecDeque::new()),
            temperature_store: RefCell::new(IndexMap::new()),
            temp_monitors: RefCell::new(Vec::new()),
            timer: RefCell::new(None),
        });
        let srv = server();
        let d = Rc::downgrade(&ds);
        srv.on_event("server:gcode_response", move |args| {
            if let Some(ds) = d.upgrade() {
                let resp = args.first().cloned().unwrap_or(Value::Null);
                ds.push_gcode(json!({"message": resp, "time": crate::utils::time_now(), "type": "response"}));
            }
        });
        let d = Rc::downgrade(&ds);
        srv.on_event_async("server:klippy_ready", move |_| {
            let d = d.clone();
            async move {
                if let Some(ds) = d.upgrade() {
                    ds.init_sensors().await;
                }
            }
        });
        let d = Rc::downgrade(&ds);
        srv.on_event("klippy_connection:gcode_received", move |args| {
            if let Some(ds) = d.upgrade() {
                let script = args.first().and_then(|v| v.as_str()).unwrap_or("").to_string();
                if !script.trim().is_empty() {
                    ds.push_gcode(json!({"message": script, "time": crate::utils::time_now(), "type": "command"}));
                }
            }
        });
        let d = ds.clone();
        srv.register_endpoint("/server/temperature_store", RequestType::GET, callback(move |req| {
            let d = d.clone();
            async move { d.handle_temp_store_request(req) }
        }))?;
        let d = ds.clone();
        srv.register_endpoint("/server/gcode_store", RequestType::GET, callback(move |req| {
            let d = d.clone();
            async move { d.handle_gcode_store_request(req) }
        }))?;
        Ok(ds)
    }

    fn push_gcode(&self, item: Value) {
        let mut q = self.gcode_queue.borrow_mut();
        if self.gcode_store_size == 0 {
            return;
        }
        q.push_back(item);
        while q.len() > self.gcode_store_size {
            q.pop_front();
        }
    }

    fn stop_timer(&self) {
        if let Some(t) = self.timer.borrow_mut().take() {
            t.abort();
        }
    }

    fn push_limited(&self, store: &mut VecDeque<Value>, v: Value) {
        if self.temp_store_size == 0 {
            return;
        }
        store.push_back(v);
        while store.len() > self.temp_store_size {
            store.pop_front();
        }
    }

    async fn init_sensors(self: Rc<Self>) {
        let apis = server().klippy_apis();
        let mut q = Map::new();
        q.insert("heaters".into(), Value::Null);
        let result = match apis.query_objects(&q).await {
            Ok(r) => r,
            Err(e) => {
                log::info!("Error Configuring Sensors: {}", e);
                return;
            }
        };
        let heaters = result.get("heaters").cloned().unwrap_or(json!({}));
        let to_vec = |v: Option<&Value>| -> Vec<String> {
            v.and_then(|x| x.as_array()).map(|a| a.iter().map(crate::pyfmt::py_str).collect()).unwrap_or_default()
        };
        let mut sensors = to_vec(heaters.get("available_sensors"));
        let monitors = to_vec(heaters.get("available_monitors"));
        *self.temp_monitors.borrow_mut() = monitors.clone();
        sensors.extend(monitors);
        if sensors.is_empty() {
            log::info!("No sensors found");
            self.temperature_store.borrow_mut().clear();
            self.temp_monitors.borrow_mut().clear();
            self.stop_timer();
            return;
        }
        let sub: Map<String, Value> = sensors.iter().map(|s| (s.clone(), Value::Null)).collect();
        let status = match apis.subscribe_objects(&sub, None).await {
            Ok(s) => s,
            Err(e) => {
                log::info!("Error subscribing to sensors: {}", e);
                return;
            }
        };
        log::info!(
            "Configuring available sensors: {}",
            crate::pyfmt::py_repr(&Value::Array(sensors.iter().map(|s| json!(s)).collect()))
        );
        let valid_fields = ["temperature", "target", "power", "speed"];
        let mut new_store: IndexMap<String, IndexMap<String, VecDeque<Value>>> = IndexMap::new();
        let mut old_store = std::mem::take(&mut *self.temperature_store.borrow_mut());
        for sensor in &sensors {
            let sstatus = status.get(sensor).and_then(|v| v.as_object()).cloned().unwrap_or_default();
            let reported: Vec<String> = sstatus.keys().filter(|f| valid_fields.contains(&f.as_str())).cloned().collect();
            if reported.is_empty() {
                log::info!("No valid fields reported for sensor: {}", sensor);
                old_store.shift_remove(sensor);
                continue;
            }
            let mut entry = match old_store.shift_remove(sensor) {
                Some(mut existing) => {
                    let fields: Vec<String> = existing.keys().cloned().collect();
                    for field in fields {
                        if !reported.contains(&field) {
                            existing.shift_remove(&field);
                        } else {
                            let v = round_null(sstatus.get(&field).unwrap_or(&Value::Null));
                            let store = existing.get_mut(&field).unwrap();
                            self.push_limited(store, v);
                        }
                    }
                    existing
                }
                None => IndexMap::new(),
            };
            for field in &reported {
                if !entry.contains_key(field) {
                    let mut dq = VecDeque::new();
                    self.push_limited(&mut dq, round_null(sstatus.get(field).unwrap_or(&Value::Null)));
                    entry.insert(field.clone(), dq);
                }
            }
            new_store.insert(sensor.clone(), entry);
        }
        *self.temperature_store.borrow_mut() = new_store;
        self.stop_timer();
        let d = Rc::downgrade(&self);
        *self.timer.borrow_mut() = Some(spawn(async move {
            let mut iv = tokio::time::interval(std::time::Duration::from_secs_f64(TEMP_UPDATE_TIME));
            iv.tick().await;
            loop {
                iv.tick().await;
                match d.upgrade() {
                    Some(ds) => ds.update_temperature_store(),
                    None => return,
                }
            }
        }));
    }

    fn update_temperature_store(&self) {
        let cache = server().klippy().get_subscription_cache();
        let mut ts = self.temperature_store.borrow_mut();
        for (name, sensor) in ts.iter_mut() {
            let sdata = cache.get(name).cloned().unwrap_or(json!({}));
            for (field, store) in sensor.iter_mut() {
                let last = store.back().cloned().unwrap_or(Value::Null);
                let v = round_null(sdata.get(field).unwrap_or(&last));
                if self.temp_store_size > 0 {
                    store.push_back(v);
                    while store.len() > self.temp_store_size {
                        store.pop_front();
                    }
                }
            }
        }
    }

    fn handle_temp_store_request(&self, req: WebRequest) -> ApiResult {
        let include_monitors = req.get_boolean_or("include_monitors", false)?;
        let monitors = self.temp_monitors.borrow();
        let mut out = Map::new();
        for (name, sensor) in self.temperature_store.borrow().iter() {
            if !include_monitors && monitors.contains(name) {
                continue;
            }
            let m: Map<String, Value> =
                sensor.iter().map(|(k, v)| (format!("{}s", k), Value::Array(v.iter().cloned().collect()))).collect();
            out.insert(name.clone(), Value::Object(m));
        }
        Ok(Value::Object(out))
    }

    fn handle_gcode_store_request(&self, req: WebRequest) -> ApiResult {
        let count = req.get_int_opt("count")?;
        let q = self.gcode_queue.borrow();
        let all: Vec<Value> = q.iter().cloned().collect();
        let items = match count {
            Some(c) => py_slice_tail(&all, c),
            None => all,
        };
        Ok(json!({"gcode_store": items}))
    }

    pub fn get_temperature_store(&self) -> Value {
        let mut out = Map::new();
        for (name, sensor) in self.temperature_store.borrow().iter() {
            let m: Map<String, Value> =
                sensor.iter().map(|(k, v)| (format!("{}s", k), Value::Array(v.iter().cloned().collect()))).collect();
            out.insert(name.clone(), Value::Object(m));
        }
        Value::Object(out)
    }
}

/// python's list[-count:]
pub fn py_slice_tail<T: Clone>(items: &[T], count: i64) -> Vec<T> {
    let len = items.len() as i64;
    let start = if count == 0 {
        0
    } else if count > 0 {
        (len - count).max(0)
    } else {
        (-count).min(len)
    };
    items[start as usize..].to_vec()
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<DataStore>, crate::confighelper::ConfigError> {
    DataStore::new(config)
}

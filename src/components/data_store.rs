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

/// One temperature store sample: round_null(value, 2), 16 bytes instead of
/// a json Value (48 on armv7; a store holds 1200 per sensor field). Klipper
/// reports numbers or null for these fields; any other value is kept as it
/// came, as before.
#[derive(Clone)]
enum Sample {
    Null,
    Int(i64),
    Float(f64),
    Other(Box<Value>),
}

impl Sample {
    /// python's round(value, 2), None kept as None
    fn rounded(v: &Value) -> Sample {
        match v {
            Value::Null => Sample::Null,
            // round() of a bool is an int
            Value::Bool(b) => Sample::Int(*b as i64),
            Value::Number(n) if n.is_f64() => {
                Sample::Float(crate::pyfmt::py_round(n.as_f64().unwrap_or_default(), 2))
            }
            Value::Number(n) => match n.as_i64() {
                Some(i) => Sample::Int(i),
                None => Sample::Other(Box::new(v.clone())),
            },
            other => Sample::Other(Box::new(other.clone())),
        }
    }

    fn value(&self) -> Value {
        match self {
            Sample::Null => Value::Null,
            Sample::Int(i) => Value::from(*i),
            Sample::Float(f) => crate::pyfmt::float_value(*f),
            Sample::Other(v) => (**v).clone(),
        }
    }
}

/// A gcode store entry ({"message", "time", "type"})
struct GcodeEntry {
    message: String,
    time: f64,
    kind: &'static str,
}

impl GcodeEntry {
    fn value(&self) -> Value {
        json!({"message": self.message, "time": self.time, "type": self.kind})
    }
}

type SensorStore = IndexMap<String, VecDeque<Sample>>;

pub struct DataStore {
    temp_store_size: usize,
    gcode_store_size: usize,
    gcode_queue: RefCell<VecDeque<GcodeEntry>>,
    temperature_store: RefCell<IndexMap<String, SensorStore>>,
    temp_monitors: RefCell<Vec<String>>,
    timer: crate::server::TaskSlot,
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
            timer: Default::default(),
        });
        let srv = server();
        let d = Rc::downgrade(&ds);
        srv.on::<crate::events::GcodeResponse>(move |resp| {
            if let Some(ds) = d.upgrade() {
                ds.push_gcode(crate::pyfmt::py_str(resp), "response");
            }
        });
        let d = Rc::downgrade(&ds);
        srv.on_async::<crate::events::KlippyReady, _, _>(move |_| {
            let d = d.clone();
            async move {
                if let Some(ds) = d.upgrade() {
                    ds.init_sensors().await;
                }
            }
        });
        let d = Rc::downgrade(&ds);
        srv.on::<crate::events::GcodeReceived>(move |script| {
            if let Some(ds) = d.upgrade() {
                if !script.trim().is_empty() {
                    ds.push_gcode(script.clone(), "command");
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

    fn push_gcode(&self, message: String, kind: &'static str) {
        let mut q = self.gcode_queue.borrow_mut();
        if self.gcode_store_size == 0 {
            return;
        }
        q.push_back(GcodeEntry { message, time: crate::utils::time_now(), kind });
        while q.len() > self.gcode_store_size {
            q.pop_front();
        }
    }

    fn stop_timer(&self) {
        self.timer.cancel();
    }

    fn push_limited(&self, store: &mut VecDeque<Sample>, v: Sample) {
        if self.temp_store_size == 0 {
            return;
        }
        store_push(store, v, self.temp_store_size);
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
        let mut new_store: IndexMap<String, SensorStore> = IndexMap::new();
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
                            let v = Sample::rounded(sstatus.get(&field).unwrap_or(&Value::Null));
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
                    self.push_limited(&mut dq, Sample::rounded(sstatus.get(field).unwrap_or(&Value::Null)));
                    entry.insert(field.clone(), dq);
                }
            }
            new_store.insert(sensor.clone(), entry);
        }
        *self.temperature_store.borrow_mut() = new_store;
        self.stop_timer();
        let d = Rc::downgrade(&self);
        self.timer.set(spawn(async move {
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
        // read in place: copying the whole subscription cache each second
        // for a few temperatures was all garbage
        let klippy = server().klippy();
        let cache = klippy.subscription_cache();
        let mut ts = self.temperature_store.borrow_mut();
        for (name, sensor) in ts.iter_mut() {
            let sdata = cache.get(name);
            for (field, store) in sensor.iter_mut() {
                let v = match sdata.and_then(|d| d.get(field)) {
                    Some(v) => Sample::rounded(v),
                    // the last value, already rounded
                    None => store.back().cloned().unwrap_or(Sample::Null),
                };
                if self.temp_store_size > 0 {
                    store_push(store, v, self.temp_store_size);
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
                sensor.iter().map(|(k, v)| (format!("{}s", k), Value::Array(v.iter().map(|s| s.value()).collect()))).collect();
            out.insert(name.clone(), Value::Object(m));
        }
        Ok(Value::Object(out))
    }

    fn handle_gcode_store_request(&self, req: WebRequest) -> ApiResult {
        let count = req.get_int_opt("count")?;
        let q = self.gcode_queue.borrow();
        let start = match count {
            Some(c) => py_tail_start(q.len(), c),
            None => 0,
        };
        let items: Vec<Value> = q.iter().skip(start).map(GcodeEntry::value).collect();
        Ok(json!({"gcode_store": items}))
    }
}

/// Appends to a store of at most `limit` samples. It grows by doubling as a
/// VecDeque does, but never past `limit`: plain doubling would hold 1200
/// samples in room for 2048
fn store_push(store: &mut VecDeque<Sample>, v: Sample, limit: usize) {
    // the oldest sample goes first, as from a deque with a maxlen: appending
    // to a full store would grow it before it shrinks
    while store.len() >= limit.max(1) {
        store.pop_front();
    }
    if store.len() == store.capacity() {
        let target = (store.capacity().max(4) * 2).min(limit.max(1));
        store.reserve_exact(target - store.len());
    }
    store.push_back(v);
}

/// Where python's list[-count:] starts
fn py_tail_start(len: usize, count: i64) -> usize {
    let len = len as i64;
    let start = if count == 0 {
        0
    } else if count > 0 {
        (len - count).max(0)
    } else {
        (-count).min(len)
    };
    start as usize
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<DataStore>, crate::confighelper::ConfigError> {
    DataStore::new(config)
}

#[cfg(test)]
mod tests {
    use super::Sample;
    use serde_json::{json, Value};

    fn r(v: Value) -> Value {
        Sample::rounded(&v).value()
    }

    // what python's round(value, 2) gives (None stays None)
    #[test]
    fn samples_round_as_python() {
        assert_eq!(r(json!(215.376)), json!(215.38));
        assert_eq!(r(json!(0.125)), json!(0.12)); // ties to even on the double
        assert_eq!(r(json!(-0.004)), json!(-0.0));
        assert_eq!(r(json!(60)), json!(60)); // an int stays an int
        assert_eq!(r(Value::Null), Value::Null);
        assert_eq!(r(json!(true)), json!(1)); // round(True, 2) == 1
        assert_eq!(r(json!(u64::MAX)), json!(u64::MAX));
        assert_eq!(r(json!("x")), json!("x"));
        // rounding a rounded value changes nothing (the last-value fallback)
        assert_eq!(Sample::rounded(&r(json!(215.376))).value(), json!(215.38));
        assert_eq!(std::mem::size_of::<Sample>(), 16);
    }

    // a store grows as it fills and stops at its limit, without spare room
    #[test]
    fn store_capacity_stops_at_limit() {
        let mut q = std::collections::VecDeque::new();
        for i in 0..5000 {
            super::store_push(&mut q, Sample::Int(i), 1200);
            assert!(q.capacity() <= 1200);
            assert!(q.capacity() <= (q.len() * 2).max(8), "len {} cap {}", q.len(), q.capacity());
        }
        assert_eq!(q.len(), 1200);
        assert_eq!(q.capacity(), 1200);
        assert!(matches!(q.front(), Some(Sample::Int(3800))));
    }
}

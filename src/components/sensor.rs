// Generic sensor support (port of moonraker/components/sensor.py)

use crate::common::*;
use crate::components::history::{History, HistoryFieldData};
use crate::components::mqtt::{MqttCallback, MqttClient};
use crate::confighelper::{Bounds, ConfigError, ConfigHelper};
use crate::server::{server, spawn, Component};
use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

const SENSOR_UPDATE_TIME: f64 = 1.0;
const SENSOR_EVENT_NAME: &str = "sensors:sensor_update";

type Measurements = Rc<RefCell<Map<String, Value>>>;

/// python dict equality (1 == 1.0)
fn values_equal(a: &Map<String, Value>, b: &Map<String, Value>) -> bool {
    a.len() == b.len()
        && a.iter().all(|(k, v)| match (v, b.get(k)) {
            (Value::Number(x), Some(Value::Number(y))) => x.as_f64() == y.as_f64(),
            (x, Some(y)) => x == y,
            _ => false,
        })
}

/// python float() of a template value
fn to_py_float(v: &minijinja::Value) -> Result<Value, String> {
    let jv: Value = serde_json::to_value(v).unwrap_or(Value::Null);
    match &jv {
        Value::Number(_) | Value::Bool(_) => Ok(jv),
        Value::String(s) => {
            let t = s.trim();
            let parsed = match t.to_lowercase().as_str() {
                "inf" | "+inf" | "infinity" | "+infinity" => Some(f64::INFINITY),
                "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
                _ => t.replace('_', "").parse::<f64>().ok().filter(|f| f.is_finite()),
            };
            match parsed {
                // json can't hold inf/nan, python would emit Infinity
                Some(f) => Ok(serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)),
                None => Err(format!("could not convert string to float: {}", crate::pyfmt::py_repr(&jv))),
            }
        }
        other => Err(format!(
            "float() argument must be a string or a real number, not '{}'",
            crate::pyfmt::py_type_name(other)
        )),
    }
}

struct Sensor {
    id: String,
    type_: String,
    name: String,
    error_state: RefCell<Option<String>>,
    last_measurements: Measurements,
    last_value: RefCell<Map<String, Value>>,
    store_size: usize,
    values: RefCell<IndexMap<String, VecDeque<Value>>>,
    param_info: Vec<Value>,
    field_info: IndexMap<String, Vec<Rc<HistoryFieldData>>>,
    // MQTT sensor
    mqtt: Rc<MqttClient>,
    state_topic: String,
    state_response: crate::components::template::JinjaTemplate,
    qos: Option<u8>,
}

impl Sensor {
    fn new(config: &ConfigHelper) -> Result<Rc<Self>, String> {
        let srv = server();
        let cfg_name = config.get_name().to_string();
        let id = cfg_name.split_whitespace().last().unwrap_or("").to_string();
        let type_ = config.get("type").map_err(|e| e.0)?;
        let name = config.get_or("name", &id).map_err(|e| e.0)?;
        let store_size = config.getint_or("sensor_store_size", 1200).map_err(|e| e.0)?;
        let history: Rc<History> =
            srv.get::<crate::components::history::History>().ok_or_else(|| "Component (history) not found".to_string())?;
        let last_measurements: Measurements = Rc::new(RefCell::new(Map::new()));
        let mut param_info = Vec::new();
        let mut field_info: IndexMap<String, Vec<Rc<HistoryFieldData>>> = IndexMap::new();
        for opt in config.get_options().keys() {
            if let Some(pname) = opt.strip_prefix("parameter_") {
                let mut data = config.getdict(opt, None, (Some("\n"), "="), false).map_err(|e| e.0)?;
                data["name"] = json!(pname);
                param_info.push(data);
                continue;
            }
            let fname = match opt.strip_prefix("history_field_") {
                Some(f) => f,
                None => continue,
            };
            let mut field_cfg = match config.getdict(opt, None, (Some("\n"), "="), false).map_err(|e| e.0)? {
                Value::Object(m) => m,
                _ => Map::new(),
            };
            let mut pop = |k: &str| field_cfg.shift_remove(k).map(|v| crate::pyfmt::py_str(&v));
            let ident = pop("parameter")
                .ok_or_else(|| format!("[{}]: option '{}', key 'parameter' must bespecified", cfg_name, opt))?;
            let do_init = pop("init_tracker").unwrap_or("false".into()).to_lowercase() == "true";
            let reset_cb: Option<crate::components::history::ResetCallback> = if do_init {
                let lm = last_measurements.clone();
                let key = ident.clone();
                Some(Rc::new(move || lm.borrow().get(&key).cloned().unwrap_or(json!(0))))
            } else {
                None
            };
            let excl_paused = pop("exclude_paused").unwrap_or("false".into()).to_lowercase() == "true";
            let report_total = pop("report_total").unwrap_or("false".into()).to_lowercase() == "true";
            let report_max = pop("report_maximum").unwrap_or("false".into()).to_lowercase() == "true";
            let precision = pop("precision");
            let desc = pop("desc").unwrap_or_else(|| format!("{} tracker", ident));
            let strategy = pop("strategy").unwrap_or("basic".into());
            let units = pop("units");
            let cfg_err = |e: String| {
                format!("[{}]: option '{}', error encountered during sensor field configuration: {}", cfg_name, opt, e)
            };
            let precision = match precision {
                None => None,
                Some(p) => Some(
                    p.trim()
                        .parse::<usize>()
                        .map_err(|_| cfg_err(format!("invalid literal for int() with base 10: '{}'", p)))?,
                ),
            };
            let fdata = HistoryFieldData::new(
                fname,
                &cfg_name,
                &desc,
                &strategy,
                units.as_deref(),
                reset_cb,
                excl_paused,
                report_total,
                report_max,
                precision,
            )
            .map_err(|e| cfg_err(e.message))?;
            for key in field_cfg.keys() {
                srv.add_warning(&format!("[{}]: Option '{}' contains invalid key '{}'", cfg_name, opt, key), None, true);
            }
            field_info.entry(ident).or_default().push(fdata.clone());
            history.register_auxiliary_field(fdata).map_err(|e| e.message)?;
        }
        // MQTTSensor
        srv.load_component(config, "mqtt").map_err(|e| e.0)?;
        let mqtt = srv.get::<MqttClient>().ok_or_else(|| "Component (mqtt) not found".to_string())?;
        let state_topic = config.get("state_topic").map_err(|e| e.0)?;
        let state_response = config.gettemplate("state_response_template", false).map_err(|e| e.0)?;
        let qos = config.getint_opt("qos", Bounds::range(0.0, 2.0)).map_err(|e| e.0)?.map(|q| q as u8);
        let sensor = Rc::new(Sensor {
            id,
            type_,
            name,
            error_state: RefCell::new(None),
            last_measurements,
            last_value: RefCell::new(Map::new()),
            store_size: store_size.max(0) as usize,
            values: RefCell::new(IndexMap::new()),
            param_info,
            field_info,
            mqtt,
            state_topic,
            state_response,
            qos,
        });
        let s = Rc::downgrade(&sensor);
        srv.on_event("mqtt:disconnected", move |_| {
            if let Some(s) = s.upgrade() {
                *s.error_state.borrow_mut() = Some("MQTT Disconnected".into());
                s.last_measurements.borrow_mut().clear();
            }
        });
        Ok(sensor)
    }

    fn on_state_update(&self, payload: Vec<u8>) {
        let store: Arc<Mutex<Map<String, Value>>> = Arc::new(Mutex::new(Map::new()));
        let st = store.clone();
        let set_result = minijinja::Value::from_function(move |name: String, value: minijinja::Value| {
            let v = to_py_float(&value).map_err(|e| minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, e))?;
            st.lock().unwrap().insert(name, v);
            Ok::<_, minijinja::Error>(minijinja::Value::from(()))
        });
        let ctx = json!({"payload": String::from_utf8_lossy(&payload)});
        match self.state_response.render_with(&ctx, vec![("set_result", set_result)]) {
            Err(e) => {
                log::error!("Error updating sensor results: {}", e.message);
                *self.error_state.borrow_mut() = Some(e.message);
            }
            Ok(_) => {
                *self.error_state.borrow_mut() = None;
                let measurements = store.lock().unwrap().clone();
                for (name, value) in &measurements {
                    if let Some(fl) = self.field_info.get(name) {
                        for f in fl {
                            f.update(value.clone());
                        }
                    }
                }
                *self.last_measurements.borrow_mut() = measurements;
            }
        }
    }

    fn update_sensor_value(&self) {
        let lm = self.last_measurements.borrow().clone();
        let mut values = self.values.borrow_mut();
        for (key, value) in &lm {
            let dq = values.entry(key.clone()).or_default();
            dq.push_back(value.clone());
            while dq.len() > self.store_size {
                dq.pop_front();
            }
        }
        *self.last_value.borrow_mut() = lm;
    }

    fn initialize(self: &Rc<Self>) -> bool {
        log::info!("Registered sensor '{}'", self.name);
        let s = Rc::downgrade(self);
        let cb: MqttCallback = Rc::new(move |payload| {
            if let Some(s) = s.upgrade() {
                s.on_state_update(payload);
            }
        });
        match self.mqtt.subscribe_topic(&self.state_topic, cb, self.qos) {
            Ok(_) => {
                *self.error_state.borrow_mut() = None;
                true
            }
            Err(e) => {
                *self.error_state.borrow_mut() = Some(e.message);
                false
            }
        }
    }

    fn get_sensor_info(&self, extended: bool) -> Value {
        let mut ret = json!({
            "id": self.id,
            "friendly_name": self.name,
            "type": self.type_,
            "values": Value::Object(self.last_measurements.borrow().clone()),
        });
        if extended {
            ret["parameter_info"] = json!(self.param_info);
            let mut history_fields = Vec::new();
            for (parameter, fl) in &self.field_info {
                for f in fl {
                    let mut c = f.get_configuration();
                    c["parameter"] = json!(parameter);
                    history_fields.push(c);
                }
            }
            ret["history_fields"] = json!(history_fields);
        }
        ret
    }

    fn get_sensor_measurements(&self) -> Value {
        let values = self.values.borrow();
        Value::Object(values.iter().map(|(k, v)| (k.clone(), json!(v.iter().collect::<Vec<_>>()))).collect())
    }
}

pub struct Sensors {
    sensors: IndexMap<String, Rc<Sensor>>,
    timer: crate::server::TaskSlot,
}

impl Sensors {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, ConfigError> {
        let srv = server();
        srv.register_notification(SENSOR_EVENT_NAME, None);
        let mut sensors = IndexMap::new();
        for section in config.get_prefix_sections("sensor ") {
            let cfg = config.getsection(&section);
            let res: Result<(String, Rc<Sensor>), String> = (|| {
                let name = match cfg.get_name().split_once(char::is_whitespace) {
                    Some((_, n)) if !n.trim().is_empty() => n.trim().to_string(),
                    _ => return Err(format!("Invalid section name: {}", cfg.get_name())),
                };
                log::info!("Configuring sensor: {}", name);
                let sensor_type = cfg.get("type").map_err(|e| e.0)?;
                if sensor_type.to_uppercase() != "MQTT" {
                    return Err(format!("Unsupported sensor type: {}", sensor_type));
                }
                Ok((name, Sensor::new(&cfg)?))
            })();
            match res {
                Ok((name, s)) => {
                    sensors.insert(name, s);
                }
                Err(e) => {
                    srv.add_warning(&format!("Failed to configure sensor [{}]\n{}", cfg.get_name(), e), None, true);
                }
            }
        }
        let comp = Rc::new(Sensors { sensors, timer: Default::default() });
        for (ep, which) in [("/server/sensors/list", 0), ("/server/sensors/info", 1), ("/server/sensors/measurements", 2)] {
            let c = comp.clone();
            let _ = srv.register_endpoint(
                ep,
                RequestType::GET,
                callback(move |req| {
                    let c = c.clone();
                    async move {
                        match which {
                            0 => c.handle_list(req),
                            1 => c.handle_info(req),
                            _ => c.handle_measurements(req),
                        }
                    }
                }),
            );
        }
        Ok(comp)
    }

    fn update_sensor_values(&self) {
        let mut changed = Map::new();
        for (name, sensor) in &self.sensors {
            let base = sensor.last_value.borrow().clone();
            sensor.update_sensor_value();
            let lv = sensor.last_value.borrow();
            if !values_equal(&base, &lv) {
                changed.insert(name.clone(), Value::Object(lv.clone()));
            }
        }
        if !changed.is_empty() {
            server().send_event(SENSOR_EVENT_NAME, vec![Value::Object(changed)]);
        }
    }

    fn handle_list(&self, req: WebRequest) -> ApiResult {
        let extended = req.get_boolean_or("extended", false)?;
        let sensors: Map<String, Value> = self.sensors.iter().map(|(k, s)| (k.clone(), s.get_sensor_info(extended))).collect();
        Ok(json!({"sensors": sensors}))
    }

    fn handle_info(&self, req: WebRequest) -> ApiResult {
        let name = req.get_str("sensor")?;
        let extended = req.get_boolean_or("extended", false)?;
        match self.sensors.get(&name) {
            Some(s) => Ok(s.get_sensor_info(extended)),
            None => Err(ServerError::bad(format!("No valid sensor named {}", name))),
        }
    }

    fn handle_measurements(&self, req: WebRequest) -> ApiResult {
        let name = req.get_str_or("sensor", "");
        if !name.is_empty() {
            return match self.sensors.get(&name) {
                Some(s) => Ok(json!({name: s.get_sensor_measurements()})),
                None => Err(ServerError::bad(format!("No valid sensor named {}", name))),
            };
        }
        Ok(Value::Object(self.sensors.iter().map(|(k, s)| (k.clone(), s.get_sensor_measurements())).collect()))
    }
}

#[async_trait(?Send)]
impl Component for Sensors {
    fn has_init(&self) -> bool {
        true
    }

    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        let srv = server();
        for sensor in self.sensors.values() {
            if !sensor.initialize() {
                srv.add_warning(&format!("Sensor '{}' failed to initialize", sensor.name), None, true);
            }
        }
        let c = Rc::downgrade(&self);
        let h = spawn(async move {
            loop {
                match c.upgrade() {
                    Some(c) => c.update_sensor_values(),
                    None => break,
                }
                tokio::time::sleep(std::time::Duration::from_secs_f64(SENSOR_UPDATE_TIME)).await;
            }
        });
        self.timer.set(h);
        Ok(())
    }

    async fn close(self: Rc<Self>) {
        self.timer.cancel();
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<Sensors>, ConfigError> {
    Sensors::new(config)
}

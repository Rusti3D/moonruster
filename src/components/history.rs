// History cache for printer jobs (port of moonraker/components/history.py,
// including the field trackers from moonraker/common.py)

use crate::common::*;
use crate::components::database::{MoonrakerDatabase, SqlTableDefinition};
use crate::confighelper::ConfigHelper;
use crate::server::{server, Component};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const HIST_TABLE: &str = "job_history";
const TOTALS_TABLE: &str = "job_totals";

fn base_totals() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("total_jobs".into(), json!(0));
    m.insert("total_time".into(), json!(0.0));
    m.insert("total_print_time".into(), json!(0.0));
    m.insert("total_filament_used".into(), json!(0.0));
    m.insert("longest_job".into(), json!(0.0));
    m.insert("longest_print".into(), json!(0.0));
    m
}

// ***** python numeric helpers *****

fn is_num(v: &Value) -> bool {
    v.is_number()
}

pub fn num_add(a: &Value, b: &Value) -> Value {
    match (a.as_i64(), b.as_i64(), a.is_f64() || b.is_f64()) {
        (Some(x), Some(y), false) => json!(x + y),
        _ => crate::utils::float_value(a.as_f64().unwrap_or(0.0) + b.as_f64().unwrap_or(0.0)),
    }
}

pub fn num_sub(a: &Value, b: &Value) -> Value {
    match (a.as_i64(), b.as_i64(), a.is_f64() || b.is_f64()) {
        (Some(x), Some(y), false) => json!(x - y),
        _ => crate::utils::float_value(a.as_f64().unwrap_or(0.0) - b.as_f64().unwrap_or(0.0)),
    }
}

fn num_max(a: &Value, b: &Value) -> Value {
    if b.as_f64().unwrap_or(f64::MIN) > a.as_f64().unwrap_or(f64::MIN) {
        b.clone()
    } else {
        a.clone()
    }
}

fn num_min(a: &Value, b: &Value) -> Value {
    if b.as_f64().unwrap_or(f64::MAX) < a.as_f64().unwrap_or(f64::MAX) {
        b.clone()
    } else {
        a.clone()
    }
}

fn truthy_or_zero(v: &Value) -> Value {
    match v {
        Value::Null => json!(0),
        Value::Number(n) if n.as_f64() == Some(0.0) => json!(0),
        other => other.clone(),
    }
}

// ***** Field trackers *****

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrackingStrategy {
    Basic,
    Delta,
    Accumulate,
    Average,
    Maximum,
    Minimum,
    Collect,
}

impl TrackingStrategy {
    pub fn from_string(s: &str) -> Result<Self, ServerError> {
        Ok(match s.to_uppercase().as_str() {
            "BASIC" => Self::Basic,
            "DELTA" => Self::Delta,
            "ACCUMULATE" => Self::Accumulate,
            "AVERAGE" => Self::Average,
            "MAXIMUM" => Self::Maximum,
            "MINIMUM" => Self::Minimum,
            "COLLECT" => Self::Collect,
            _ => return Err(ServerError::bad(format!("No enum member named {}", s))),
        })
    }
    fn name(&self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Delta => "delta",
            Self::Accumulate => "accumulate",
            Self::Average => "average",
            Self::Maximum => "maximum",
            Self::Minimum => "minimum",
            Self::Collect => "collect",
        }
    }
}

pub type ResetCallback = Rc<dyn Fn() -> Value>;

pub struct Tracker {
    strategy: TrackingStrategy,
    pub tracked_value: Value,
    last_value: Option<Value>,
    count: i64,
    initialized: bool,
    pub exclude_paused: bool,
    pub reset_callback: Option<ResetCallback>,
}

fn tracking_enabled(exclude_paused: bool) -> bool {
    server().lookup_component::<History>("history").map(|h| h.tracking_enabled(exclude_paused)).unwrap_or(false)
}

impl Tracker {
    fn new(strategy: TrackingStrategy, reset_callback: Option<ResetCallback>, exclude_paused: bool) -> Self {
        let tracked_value = match strategy {
            TrackingStrategy::Basic => Value::Null,
            TrackingStrategy::Collect => json!([]),
            _ => json!(0),
        };
        Tracker { strategy, tracked_value, last_value: None, count: 0, initialized: false, exclude_paused, reset_callback }
    }

    pub fn reset(&mut self) {
        let cb = self.reset_callback.clone();
        match self.strategy {
            TrackingStrategy::Basic => {
                if let Some(cb) = cb {
                    self.tracked_value = cb();
                }
            }
            TrackingStrategy::Delta => {
                self.tracked_value = json!(0);
                self.last_value = None;
                if let Some(cb) = cb {
                    let v = cb();
                    if is_num(&v) {
                        self.last_value = Some(v);
                    } else {
                        log::info!("DeltaTracker reset to invalid type");
                    }
                }
            }
            TrackingStrategy::Accumulate | TrackingStrategy::Average => {
                match cb {
                    Some(cb) => {
                        let v = cb();
                        if is_num(&v) {
                            self.tracked_value = v;
                        } else {
                            let name = if self.strategy == TrackingStrategy::Average { "AveragingTracker" } else { "CumulativeTracker" };
                            log::info!("{} reset to invalid type", name);
                            self.tracked_value = json!(0);
                        }
                    }
                    None => self.tracked_value = json!(0),
                }
                if self.strategy == TrackingStrategy::Average {
                    self.count = 0;
                }
            }
            TrackingStrategy::Maximum | TrackingStrategy::Minimum => {
                self.initialized = false;
                match cb {
                    Some(cb) => {
                        let v = cb();
                        if is_num(&v) {
                            self.tracked_value = v;
                            self.initialized = true;
                        } else {
                            self.tracked_value = json!(0);
                            let name = if self.strategy == TrackingStrategy::Maximum { "MaximumTracker" } else { "MinimumTracker" };
                            log::info!("{} reset to invalid type", name);
                        }
                    }
                    None => self.tracked_value = json!(0),
                }
            }
            TrackingStrategy::Collect => match cb {
                Some(cb) => {
                    let v = cb();
                    if v.is_array() {
                        self.tracked_value = v;
                    } else {
                        log::info!("CollectionTracker reset to invalid type");
                        self.tracked_value = json!([]);
                    }
                }
                None => self.tracked_value = json!([]),
            },
        }
    }

    pub fn update(&mut self, value: Value) {
        match self.strategy {
            TrackingStrategy::Basic => {
                if tracking_enabled(self.exclude_paused) {
                    self.tracked_value = value;
                }
            }
            TrackingStrategy::Delta => {
                if !is_num(&value) {
                    return;
                }
                if tracking_enabled(self.exclude_paused) {
                    if let Some(lv) = &self.last_value {
                        self.tracked_value = num_add(&self.tracked_value, &num_sub(&value, lv));
                    }
                }
                self.last_value = Some(value);
            }
            TrackingStrategy::Accumulate => {
                if is_num(&value) && tracking_enabled(self.exclude_paused) {
                    self.tracked_value = num_add(&self.tracked_value, &value);
                }
            }
            TrackingStrategy::Average => {
                if is_num(&value) && tracking_enabled(self.exclude_paused) {
                    let lv = self.tracked_value.as_f64().unwrap_or(0.0);
                    self.count += 1;
                    let v = (lv * (self.count - 1) as f64 + value.as_f64().unwrap_or(0.0)) / self.count as f64;
                    self.tracked_value = crate::utils::float_value(v);
                }
            }
            TrackingStrategy::Maximum | TrackingStrategy::Minimum => {
                if is_num(&value) && tracking_enabled(self.exclude_paused) {
                    if !self.initialized {
                        self.tracked_value = value;
                        self.initialized = true;
                    } else if self.strategy == TrackingStrategy::Maximum {
                        self.tracked_value = num_max(&self.tracked_value, &value);
                    } else {
                        self.tracked_value = num_min(&self.tracked_value, &value);
                    }
                }
            }
            TrackingStrategy::Collect => {
                let arr = self.tracked_value.as_array().cloned().unwrap_or_default();
                if arr.contains(&value) {
                    return;
                }
                if tracking_enabled(self.exclude_paused) {
                    let mut arr = arr;
                    arr.push(value);
                    if arr.len() > 100 {
                        arr.remove(0);
                    }
                    self.tracked_value = Value::Array(arr);
                }
            }
        }
    }

    pub fn has_totals(&self) -> bool {
        match self.strategy {
            TrackingStrategy::Basic => is_num(&self.tracked_value),
            TrackingStrategy::Collect => false,
            _ => true,
        }
    }
}

pub struct HistoryFieldData {
    name: String,
    provider: String,
    desc: String,
    strategy: TrackingStrategy,
    units: Option<String>,
    pub tracker: RefCell<Tracker>,
    report_total: bool,
    report_maximum: bool,
    precision: Option<usize>,
}

impl HistoryFieldData {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        field_name: &str,
        provider: &str,
        desc: &str,
        strategy: &str,
        units: Option<&str>,
        reset_callback: Option<ResetCallback>,
        exclude_paused: bool,
        report_total: bool,
        report_maximum: bool,
        precision: Option<usize>,
    ) -> Result<Rc<Self>, ServerError> {
        let st = TrackingStrategy::from_string(strategy)?;
        Ok(Rc::new(HistoryFieldData {
            name: field_name.into(),
            provider: provider.into(),
            desc: desc.into(),
            strategy: st,
            units: units.map(String::from),
            tracker: RefCell::new(Tracker::new(st, reset_callback, exclude_paused)),
            report_total,
            report_maximum,
            precision,
        }))
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn update(&self, value: Value) {
        self.tracker.borrow_mut().update(value);
    }

    pub fn get_configuration(&self) -> Value {
        let t = self.tracker.borrow();
        json!({
            "field": self.name,
            "provider": self.provider,
            "description": self.desc,
            "strategy": self.strategy.name(),
            "units": self.units,
            "init_tracker": t.reset_callback.is_some(),
            "exclude_paused": t.exclude_paused,
            "report_total": self.report_total,
            "report_maximum": self.report_maximum,
            "precision": self.precision,
        })
    }

    pub fn as_dict(&self) -> Value {
        let mut val = self.tracker.borrow().tracked_value.clone();
        if let (Some(p), true) = (self.precision, val.is_f64()) {
            val = crate::pyfmt::round_value(&val, p);
        }
        json!({"provider": self.provider, "name": self.name, "value": val, "description": self.desc, "units": self.units})
    }

    pub fn has_totals(&self) -> bool {
        self.tracker.borrow().has_totals() && (self.report_total || self.report_maximum)
    }

    pub fn get_totals(&self, last_totals: &[Value], reset: bool) -> Value {
        if !self.has_totals() {
            return json!({});
        }
        let (mut maximum, mut total);
        if reset {
            maximum = if self.report_maximum { json!(0) } else { Value::Null };
            total = if self.report_total { json!(0) } else { Value::Null };
        } else {
            let cur = self.tracker.borrow().tracked_value.clone();
            maximum = if self.report_maximum { cur.clone() } else { Value::Null };
            total = if self.report_total { cur.clone() } else { Value::Null };
            for obj in last_totals {
                if obj.get("provider").and_then(|v| v.as_str()) == Some(&self.provider)
                    && obj.get("field").and_then(|v| v.as_str()) == Some(&self.name)
                {
                    if !maximum.is_null() {
                        maximum = num_max(&cur, &truthy_or_zero(obj.get("maximum").unwrap_or(&Value::Null)));
                    }
                    if !total.is_null() {
                        total = num_add(&cur, &truthy_or_zero(obj.get("total").unwrap_or(&Value::Null)));
                    }
                    break;
                }
            }
            if let Some(p) = self.precision {
                if !maximum.is_null() {
                    maximum = crate::pyfmt::round_value(&maximum, p);
                }
                if !total.is_null() {
                    total = crate::pyfmt::round_value(&total, p);
                }
            }
        }
        json!({"provider": self.provider, "field": self.name, "maximum": maximum, "total": total})
    }
}

// ***** Tables *****

fn create_totals_list(job_totals: &Map<String, Value>, aux_totals: &[Value]) -> Vec<Vec<rusqlite::types::Value>> {
    use rusqlite::types::Value as SV;
    let mut out = Vec::new();
    for (k, v) in job_totals {
        let (maximum, total) = if k.starts_with("total_") { (Value::Null, v.clone()) } else { (v.clone(), Value::Null) };
        out.push(vec![
            SV::Text("history".into()),
            SV::Text(k.clone()),
            crate::components::database::json_to_sql(&maximum),
            crate::components::database::json_to_sql(&total),
            SV::Text("default".into()),
        ]);
    }
    for item in aux_totals {
        if !item.is_object() {
            continue;
        }
        out.push(vec![
            crate::components::database::json_to_sql(&item["provider"]),
            crate::components::database::json_to_sql(&item["field"]),
            crate::components::database::json_to_sql(&item["maximum"]),
            crate::components::database::json_to_sql(&item["total"]),
            SV::Text("default".into()),
        ]);
    }
    out
}

struct TotalsSqlDefinition;

impl SqlTableDefinition for TotalsSqlDefinition {
    fn name(&self) -> &str {
        TOTALS_TABLE
    }
    fn version(&self) -> i64 {
        1
    }
    fn prototype(&self) -> String {
        format!(
            "{} (\n    provider TEXT NOT NULL,\n    field TEXT NOT NULL,\n    maximum REAL,\n    total REAL,\n    instance_id TEXT NOT NULL,\n    PRIMARY KEY (provider, field, instance_id)\n)",
            TOTALS_TABLE
        )
    }
    fn migrate(&self, last_version: i64, db: &MoonrakerDatabase) -> Result<(), ServerError> {
        if last_version != 0 {
            return Ok(());
        }
        log::info!("Migrating history totals from moonraker namespace...");
        let hist_ns = db.get_item_sync("moonraker", Some(&json!("history")), Some(json!({}))).unwrap_or(json!({}));
        let job_totals = hist_ns.get("job_totals").and_then(|v| v.as_object()).cloned().unwrap_or_else(base_totals);
        let aux_totals = hist_ns.get("aux_totals").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let list = create_totals_list(&job_totals, &aux_totals);
        db.sql_executemany(&format!("INSERT OR IGNORE INTO {} VALUES(?, ?, ?, ?, ?)", TOTALS_TABLE), &list)?;
        let _ = db.delete_item_sync("moonraker", &json!("history"));
        Ok(())
    }
}

struct HistorySqlDefinition;

impl SqlTableDefinition for HistorySqlDefinition {
    fn name(&self) -> &str {
        HIST_TABLE
    }
    fn version(&self) -> i64 {
        1
    }
    fn prototype(&self) -> String {
        format!(
            "{} (\n    job_id INTEGER PRIMARY KEY ASC,\n    user TEXT NOT NULL,\n    filename TEXT,\n    status TEXT NOT NULL,\n    start_time REAL NOT NULL,\n    end_time REAL,\n    print_duration REAL NOT NULL,\n    total_duration REAL NOT NULL,\n    filament_used REAL NOT NULL,\n    metadata pyjson,\n    auxiliary_data pyjson NOT NULL,\n    instance_id TEXT NOT NULL\n)",
            HIST_TABLE
        )
    }
    fn migrate(&self, last_version: i64, db: &MoonrakerDatabase) -> Result<(), ServerError> {
        use crate::components::database::json_to_sql;
        if last_version != 0 {
            return Ok(());
        }
        let get = |e: &Value, n: &str, d: Value| -> Value {
            match e.get(n) {
                None | Some(Value::Null) => d,
                Some(v) => v.clone(),
            }
        };
        let mut rows = Vec::new();
        for (key, entry) in db.get_namespace("history", false)? {
            if !entry.is_object() {
                log::info!("History migration, skipping invalid value: {} {}", key, crate::pyfmt::py_str(&entry));
                continue;
            }
            rows.push(vec![
                rusqlite::types::Value::Null,
                json_to_sql(&get(&entry, "user", json!("No User"))),
                json_to_sql(&get(&entry, "filename", json!("unknown"))),
                json_to_sql(&get(&entry, "status", json!("error"))),
                json_to_sql(&get(&entry, "start_time", json!(0.0))),
                json_to_sql(&get(&entry, "end_time", json!(0.0))),
                json_to_sql(&get(&entry, "print_duration", json!(0.0))),
                json_to_sql(&get(&entry, "total_duration", json!(0.0))),
                json_to_sql(&get(&entry, "filament_used", json!(0.0))),
                json_to_sql(&get(&entry, "metadata", json!({}))),
                json_to_sql(&get(&entry, "auxiliary_data", json!([]))),
                rusqlite::types::Value::Text("default".into()),
            ]);
        }
        if !rows.is_empty() {
            db.sql_executemany(&format!("INSERT INTO {} VALUES(?,?,?,?,?,?,?,?,?,?,?,?)", HIST_TABLE), &rows)?;
        }
        db.wipe_local_namespace("history")?;
        Ok(())
    }
}

// ***** Printer Job *****

#[derive(Clone)]
pub struct PrinterJob {
    fields: Map<String, Value>,
}

impl PrinterJob {
    fn new(data: &Map<String, Value>) -> Self {
        let mut f = Map::new();
        f.insert("end_time".into(), Value::Null);
        f.insert("filament_used".into(), json!(0));
        f.insert("filename".into(), json!(""));
        f.insert("metadata".into(), json!({}));
        f.insert("print_duration".into(), json!(0.0));
        f.insert("status".into(), json!("in_progress"));
        f.insert("start_time".into(), crate::utils::float_value(crate::utils::time_now()));
        f.insert("total_duration".into(), json!(0.0));
        f.insert("auxiliary_data".into(), json!([]));
        f.insert("user".into(), json!("No User"));
        let mut j = PrinterJob { fields: f };
        j.update_from_ps(data);
        j
    }

    fn update_from_ps(&mut self, data: &Map<String, Value>) {
        for (k, v) in data {
            if self.fields.contains_key(k) && !v.is_null() {
                self.fields.insert(k.clone(), v.clone());
            }
        }
    }

    fn finish(&mut self, status: &str, ps: &Map<String, Value>) {
        self.fields.insert("end_time".into(), crate::utils::float_value(crate::utils::time_now()));
        self.fields.insert("status".into(), json!(status));
        self.update_from_ps(ps);
    }

    fn get(&self, n: &str) -> Value {
        self.fields.get(n).cloned().unwrap_or(Value::Null)
    }

    fn set(&mut self, n: &str, v: Value) {
        if self.fields.contains_key(n) {
            self.fields.insert(n.into(), v);
        }
    }

    fn filename(&self) -> String {
        crate::pyfmt::py_str(&self.get("filename"))
    }
}

// ***** History *****

pub struct History {
    request_lock: tokio::sync::Mutex<()>,
    auxiliary_fields: RefCell<Vec<Rc<HistoryFieldData>>>,
    job_totals: RefCell<Map<String, Value>>,
    aux_totals: RefCell<Vec<Value>>,
    current_job: RefCell<Option<PrinterJob>>,
    current_job_id: Cell<Option<i64>>,
    job_user: RefCell<String>,
    job_paused: Cell<bool>,
}

#[async_trait(?Send)]
impl Component for History {
    fn has_init(&self) -> bool {
        true
    }
    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        let db = server().database();
        let valid: Vec<(String, String)> = self
            .auxiliary_fields
            .borrow()
            .iter()
            .filter(|f| f.has_totals())
            .map(|f| (f.provider.clone(), f.name.clone()))
            .collect();
        for row in db.sql_query(&format!("SELECT * from {}", TOTALS_TABLE), &[])? {
            let provider = row.get("provider").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let field = row.get("field").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let maximum = row.get("maximum").cloned().unwrap_or(Value::Null);
            let total = row.get("total").cloned().unwrap_or(Value::Null);
            if provider == "history" {
                self.job_totals.borrow_mut().insert(field, if maximum.is_null() { total } else { maximum });
            } else if valid.contains(&(provider, field)) {
                let mut item = row.clone();
                item.shift_remove("instance_id");
                self.aux_totals.borrow_mut().push(Value::Object(item));
            }
        }
        let interrupted: Vec<i64> = db
            .sql_query(&format!("SELECT job_id FROM {} WHERE status = 'in_progress'", HIST_TABLE), &[])?
            .iter()
            .filter_map(|r| r.get("job_id").and_then(|v| v.as_i64()))
            .collect();
        if !interrupted.is_empty() {
            db.sql_execute(&format!("UPDATE {} SET status = 'interrupted' WHERE status = 'in_progress'", HIST_TABLE), &[])?;
            server().add_log_rollover_item(
                "interrupted_history",
                &format!(
                    "The following jobs were detected as interrupted: [{}]",
                    interrupted.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
                ),
                true,
            );
        }
        Ok(())
    }
    async fn on_exit(self: Rc<Self>) {
        if self.current_job.borrow().is_none() {
            return;
        }
        let ps = Self::last_stats();
        self.finish_job("server_exit", ps).await;
    }
}

macro_rules! hist_endpoint {
    ($srv:expr, $h:expr, $path:expr, $rt:expr, $method:ident) => {{
        let hh = $h.clone();
        $srv.register_endpoint($path, $rt, callback(move |req: WebRequest| {
            let hh = hh.clone();
            async move { hh.$method(req).await }
        }))?;
    }};
}

impl History {
    pub fn new(_config: &ConfigHelper) -> Result<Rc<Self>, crate::confighelper::ConfigError> {
        let srv = server();
        let db = srv.database();
        db.register_table(&HistorySqlDefinition)?;
        db.register_table(&TotalsSqlDefinition)?;
        let h = Rc::new(History {
            request_lock: tokio::sync::Mutex::new(()),
            auxiliary_fields: RefCell::new(Vec::new()),
            job_totals: RefCell::new(base_totals()),
            aux_totals: RefCell::new(Vec::new()),
            current_job: RefCell::new(None),
            current_job_id: Cell::new(None),
            job_user: RefCell::new("No User".into()),
            job_paused: Cell::new(false),
        });
        let hw = Rc::downgrade(&h);
        srv.on_event_async("server:klippy_disconnect", move |_| {
            let hw = hw.clone();
            async move {
                if let Some(h) = hw.upgrade() {
                    h.finish_job("klippy_disconnect", Self::last_stats()).await;
                }
            }
        });
        let hw = Rc::downgrade(&h);
        srv.on_event_async("server:klippy_shutdown", move |_| {
            let hw = hw.clone();
            async move {
                if let Some(h) = hw.upgrade() {
                    h.finish_job("klippy_shutdown", Self::last_stats()).await;
                }
            }
        });
        let hw = Rc::downgrade(&h);
        srv.on_event_async("job_state:state_changed", move |args| {
            let hw = hw.clone();
            async move {
                if let Some(h) = hw.upgrade() {
                    h.on_job_state_changed(args).await;
                }
            }
        });
        let hw = Rc::downgrade(&h);
        srv.on_event("klippy_apis:job_start_complete", move |args| {
            if let Some(h) = hw.upgrade() {
                let username = args
                    .first()
                    .and_then(|u| u.get("username"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("No User")
                    .to_string();
                *h.job_user.borrow_mut() = username.clone();
                if let Some(j) = h.current_job.borrow_mut().as_mut() {
                    j.set("user", json!(username));
                }
            }
        });
        srv.register_notification("history:history_changed", None);
        hist_endpoint!(srv, h, "/server/history/job", RequestType::GET | RequestType::DELETE, handle_job_request);
        hist_endpoint!(srv, h, "/server/history/list", RequestType::GET, handle_jobs_list);
        hist_endpoint!(srv, h, "/server/history/totals", RequestType::GET, handle_job_totals);
        hist_endpoint!(srv, h, "/server/history/reset_totals", RequestType::POST, handle_job_total_reset);
        Ok(h)
    }

    fn last_stats() -> Map<String, Value> {
        server().lookup_component::<crate::components::job_state::JobState>("job_state").map(|j| j.get_last_stats()).unwrap_or_default()
    }

    fn fm() -> Rc<crate::components::file_manager::FileManager> {
        server().lookup_component("file_manager").expect("file_manager")
    }

    fn parse_uid(job_id: &str) -> Result<i64, ServerError> {
        i64::from_str_radix(job_id.trim(), 16).map_err(|_| ServerError::exc(format!("invalid literal for int() with base 16: '{}'", job_id)))
    }

    async fn handle_job_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let _g = self.request_lock.lock().await;
        let db = server().database();
        let rt = req.get_request_type();
        if rt == RequestType::GET {
            let job_id = req.get_str("uid")?;
            let rows = db.sql_query(&format!("SELECT * FROM {} WHERE job_id = ?", HIST_TABLE), &[rusqlite::types::Value::Integer(Self::parse_uid(&job_id)?)])?;
            let job = rows.into_iter().next().ok_or_else(|| ServerError::new(format!("Invalid job uid: {}", job_id), 404))?;
            return Ok(json!({"job": self.prep_requested_job(job, &job_id)}));
        }
        if rt == RequestType::DELETE {
            if req.get_boolean_or("all", false)? {
                let rows = db.sql_query(
                    &format!("SELECT job_id FROM {} WHERE instance_id = ?", HIST_TABLE),
                    &[rusqlite::types::Value::Text("default".into())],
                )?;
                let deljobs: Vec<String> = rows.iter().filter_map(|r| r.get("job_id").and_then(|v| v.as_i64())).map(|i| format!("{:06X}", i)).collect();
                db.sql_execute(&format!("DELETE FROM {} WHERE instance_id = ?", HIST_TABLE), &[rusqlite::types::Value::Text("default".into())])?;
                return Ok(json!({"deleted_jobs": deljobs}));
            }
            let job_id = req.get_str("uid")?;
            let (n, _) = db.sql_execute(&format!("DELETE FROM {} WHERE job_id = ?", HIST_TABLE), &[rusqlite::types::Value::Integer(Self::parse_uid(&job_id)?)])?;
            if n < 1 {
                return Err(ServerError::new(format!("Invalid job uid: {}", job_id), 404));
            }
            return Ok(json!({"deleted_jobs": [job_id]}));
        }
        Err(ServerError::bad("Invalid Request Method"))
    }

    async fn handle_jobs_list(self: Rc<Self>, req: WebRequest) -> ApiResult {
        use rusqlite::types::Value as SV;
        let _g = self.request_lock.lock().await;
        let before = req.get_float_or("before", -1.0)?;
        let since = req.get_float_or("since", -1.0)?;
        let limit = req.get_int_or("limit", 50)?;
        let start = req.get_int_or("start", 0)?;
        let order = req.get_str_or("order", "desc").to_uppercase();
        if order != "ASC" && order != "DESC" {
            return Err(ServerError::new(format!("Invalid `order` value: {}", order), 400));
        }
        let mut values = vec![SV::Text("default".into())];
        let mut sql = format!("SELECT * FROM {} WHERE instance_id = ?", HIST_TABLE);
        if before != -1.0 {
            sql.push_str(" and end_time < ?");
            values.push(SV::Real(before));
        }
        if since != -1.0 {
            sql.push_str(" and start_time > ?");
            values.push(SV::Real(since));
        }
        sql.push_str(&format!(" ORDER BY job_id {}", order));
        if limit > 0 {
            sql.push_str(" LIMIT ? OFFSET ?");
            values.push(SV::Integer(limit));
            values.push(SV::Integer(start));
        }
        let rows = server().database().sql_query(&sql, &values)?;
        let jobs: Vec<Value> = rows
            .into_iter()
            .map(|r| {
                let id = r.get("job_id").and_then(|v| v.as_i64()).unwrap_or(0);
                self.prep_requested_job(r, &format!("{:06X}", id))
            })
            .collect();
        Ok(json!({"count": jobs.len(), "jobs": jobs}))
    }

    async fn handle_job_totals(self: Rc<Self>, _req: WebRequest) -> ApiResult {
        Ok(json!({"job_totals": Value::Object(self.job_totals.borrow().clone()), "auxiliary_totals": self.aux_totals.borrow().clone()}))
    }

    async fn handle_job_total_reset(self: Rc<Self>, _req: WebRequest) -> ApiResult {
        if self.current_job.borrow().is_some() {
            return Err(ServerError::bad("Job in progress, cannot reset totals"));
        }
        let last_totals = std::mem::replace(&mut *self.job_totals.borrow_mut(), base_totals());
        let last_aux = self.aux_totals.borrow().clone();
        self.update_aux_totals(true);
        let list = create_totals_list(&self.job_totals.borrow(), &self.aux_totals.borrow());
        let db = server().database();
        db.sql_execute(&format!("DELETE FROM {} WHERE instance_id = ?", TOTALS_TABLE), &[rusqlite::types::Value::Text("default".into())])?;
        db.sql_executemany(&format!("INSERT INTO {} VALUES(?, ?, ?, ?, ?)", TOTALS_TABLE), &list)?;
        Ok(json!({"last_totals": Value::Object(last_totals), "last_auxiliary_totals": last_aux}))
    }

    async fn on_job_state_changed(&self, args: Vec<Value>) {
        let ev = match args.first().and_then(|v| v.as_str()).and_then(|s| JobEvent::from_string(s).ok()) {
            Some(e) => e,
            None => return,
        };
        let prev = args.get(1).and_then(|v| v.as_object()).cloned().unwrap_or_default();
        let new = args.get(2).and_then(|v| v.as_object()).cloned().unwrap_or_default();
        self.job_paused.set(ev == JobEvent::Paused);
        match ev {
            JobEvent::Started => {
                if self.current_job.borrow().is_some() {
                    self.finish_job("cancelled", prev).await;
                }
                self.add_job(PrinterJob::new(&new)).await;
            }
            JobEvent::Complete => self.finish_job("completed", new).await,
            JobEvent::Error => self.finish_job("error", new).await,
            JobEvent::Cancelled | JobEvent::Standby => self.finish_job("cancelled", prev).await,
            _ => {}
        }
    }

    async fn add_job(&self, job: PrinterJob) {
        let _g = self.request_lock.lock().await;
        let mut job = job;
        job.set("user", json!(self.job_user.borrow().clone()));
        *self.current_job.borrow_mut() = Some(job);
        self.current_job_id.set(None);
        self.grab_job_metadata();
        for f in self.auxiliary_fields.borrow().iter() {
            f.tracker.borrow_mut().reset();
        }
        self.set_aux_data();
        let job = self.current_job.borrow().clone().unwrap();
        let new_id = match self.save_job(&job, None) {
            Some(i) => i,
            None => {
                log::info!("Error saving job, filename '{}'", job.filename());
                return;
            }
        };
        self.current_job_id.set(Some(new_id));
        let jid = format!("{:06X}", new_id);
        self.update_metadata(&jid);
        log::debug!("History Job Added - Id: {}, File: {}", jid, job.filename());
        self.send_history_event("added");
    }

    fn set_aux_data(&self) {
        let data: Vec<Value> = self.auxiliary_fields.borrow().iter().map(|f| f.as_dict()).collect();
        if let Some(j) = self.current_job.borrow_mut().as_mut() {
            j.set("auxiliary_data", Value::Array(data));
        }
    }

    fn save_job(&self, job: &PrinterJob, job_id: Option<i64>) -> Option<i64> {
        use crate::components::database::json_to_sql;
        use rusqlite::types::Value as SV;
        let values = vec![
            job_id.map(SV::Integer).unwrap_or(SV::Null),
            json_to_sql(&job.get("user")),
            json_to_sql(&job.get("filename")),
            json_to_sql(&job.get("status")),
            json_to_sql(&job.get("start_time")),
            json_to_sql(&job.get("end_time")),
            json_to_sql(&job.get("print_duration")),
            json_to_sql(&job.get("total_duration")),
            json_to_sql(&job.get("filament_used")),
            json_to_sql(&job.get("metadata")),
            json_to_sql(&job.get("auxiliary_data")),
            SV::Text("default".into()),
        ];
        match server().database().sql_execute(&format!("REPLACE INTO {} VALUES(?,?,?,?,?,?,?,?,?,?,?,?)", HIST_TABLE), &values) {
            Ok((_, rowid)) => Some(rowid),
            Err(e) => {
                log::error!("History save error: {}", e);
                None
            }
        }
    }

    pub async fn delete_job(&self, job_id: &str) -> Result<(), ServerError> {
        let id = Self::parse_uid(job_id)?;
        server().database().sql_execute(&format!("DELETE FROM {} WHERE job_id = ?", HIST_TABLE), &[rusqlite::types::Value::Integer(id)])?;
        Ok(())
    }

    async fn finish_job(&self, status: &str, pstats: Map<String, Value>) {
        let _g = self.request_lock.lock().await;
        let has_job = self.current_job.borrow().is_some();
        let id = match (has_job, self.current_job_id.get()) {
            (true, Some(id)) => id,
            _ => {
                self.reset_current_job();
                return;
            }
        };
        let mut pstats = pstats;
        {
            let job = self.current_job.borrow();
            let j = job.as_ref().unwrap();
            let td = pstats.get("total_duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
            if pstats.get("filename") != Some(&j.get("filename")) || td < j.get("total_duration").as_f64().unwrap_or(0.0) {
                pstats = Map::new();
            }
        }
        {
            let mut job = self.current_job.borrow_mut();
            let j = job.as_mut().unwrap();
            j.set("user", json!(self.job_user.borrow().clone()));
            j.finish(status, &pstats);
        }
        self.grab_job_metadata();
        self.set_aux_data();
        let jid = format!("{:06X}", id);
        let job = self.current_job.borrow().clone().unwrap();
        self.save_job(&job, Some(id));
        self.update_metadata(&jid);
        self.update_job_totals();
        log::debug!("History Job Finished - Id: {}, File: {}, Status: {}", jid, job.filename(), status);
        self.send_history_event("finished");
        self.reset_current_job();
    }

    fn reset_current_job(&self) {
        *self.current_job.borrow_mut() = None;
        self.current_job_id.set(None);
        *self.job_user.borrow_mut() = "No User".into();
    }

    pub async fn get_job(&self, job_id: &str) -> Result<Option<Value>, ServerError> {
        let id = Self::parse_uid(job_id)?;
        let rows = server().database().sql_query(&format!("SELECT * FROM {} WHERE job_id = ?", HIST_TABLE), &[rusqlite::types::Value::Integer(id)])?;
        Ok(rows.into_iter().next().map(Value::Object))
    }

    fn grab_job_metadata(&self) {
        let fname = match self.current_job.borrow().as_ref() {
            Some(j) => j.filename(),
            None => return,
        };
        let mut md = Self::fm().get_metadata_storage().get(&fname).unwrap_or(json!({}));
        if let Some(o) = md.as_object_mut() {
            o.shift_remove("print_start_time");
            o.shift_remove("job_id");
            if let Some(thumbs) = o.get_mut("thumbnails").and_then(|t| t.as_array_mut()) {
                for t in thumbs.iter_mut() {
                    if let Some(to) = t.as_object_mut() {
                        to.shift_remove("data");
                    }
                }
            }
        }
        if let Some(j) = self.current_job.borrow_mut().as_mut() {
            j.set("metadata", md);
        }
    }

    fn update_metadata(&self, job_id: &str) {
        let (fname, start) = match self.current_job.borrow().as_ref() {
            Some(j) => (j.filename(), j.get("start_time")),
            None => return,
        };
        let mdst = Self::fm().get_metadata_storage();
        let mut md = mdst.get(&fname).unwrap_or(json!({}));
        if md.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
            md["print_start_time"] = start;
            md["job_id"] = json!(job_id);
            mdst.insert(&fname, md);
        }
    }

    fn update_job_totals(&self) {
        let job = match self.current_job.borrow().clone() {
            Some(j) => j,
            None => return,
        };
        {
            let mut t = self.job_totals.borrow_mut();
            let acc = |t: &mut Map<String, Value>, k: &str, v: Value| {
                let cur = t.get(k).cloned().unwrap_or(json!(0));
                t.insert(k.into(), num_add(&cur, &v));
            };
            acc(&mut t, "total_jobs", json!(1));
            acc(&mut t, "total_time", job.get("total_duration"));
            acc(&mut t, "total_print_time", job.get("print_duration"));
            acc(&mut t, "total_filament_used", job.get("filament_used"));
            let mx = |t: &mut Map<String, Value>, k: &str, v: Value| {
                let cur = t.get(k).cloned().unwrap_or(json!(0));
                t.insert(k.into(), num_max(&cur, &v));
            };
            mx(&mut t, "longest_job", job.get("total_duration"));
            mx(&mut t, "longest_print", job.get("print_duration"));
        }
        self.update_aux_totals(false);
        let list = create_totals_list(&self.job_totals.borrow(), &self.aux_totals.borrow());
        if let Err(e) = server().database().sql_executemany(&format!("REPLACE INTO {} VALUES(?, ?, ?, ?, ?)", TOTALS_TABLE), &list) {
            log::error!("Error updating job totals: {}", e);
        }
    }

    fn update_aux_totals(&self, reset: bool) {
        let last = self.aux_totals.borrow().clone();
        let new: Vec<Value> = self.auxiliary_fields.borrow().iter().filter(|f| f.has_totals()).map(|f| f.get_totals(&last, reset)).collect();
        *self.aux_totals.borrow_mut() = new;
    }

    fn send_history_event(&self, action: &str) {
        let (job, id) = match (self.current_job.borrow().clone(), self.current_job_id.get()) {
            (Some(j), Some(id)) => (j, id),
            _ => return,
        };
        let job = self.prep_requested_job(job.fields, &format!("{:06X}", id));
        let _ = server().send_event("history:history_changed", vec![json!({"action": action, "job": job})]);
    }

    fn prep_requested_job(&self, mut job: Map<String, Value>, job_id: &str) -> Value {
        let mtime = job.get("metadata").and_then(|m| m.get("modified")).and_then(|v| v.as_f64());
        let fname = job.get("filename").map(crate::pyfmt::py_str).unwrap_or_default();
        job.insert("exists".into(), json!(Self::fm().check_file_exists("gcodes", &fname, mtime)));
        job.insert("job_id".into(), json!(job_id));
        job.shift_remove("instance_id");
        Value::Object(job)
    }

    pub fn register_auxiliary_field(&self, field: Rc<HistoryFieldData>) -> Result<(), ServerError> {
        if field.provider == "history" {
            return Err(ServerError::bad("Provider name 'history' is reserved"));
        }
        for f in self.auxiliary_fields.borrow().iter() {
            if f.provider == field.provider && f.name == field.name {
                return Err(ServerError::bad(format!("Field {} already registered by provider {}.", f.name, f.provider)));
            }
        }
        self.auxiliary_fields.borrow_mut().push(field);
        Ok(())
    }

    pub fn tracking_enabled(&self, check_paused: bool) -> bool {
        if self.current_job.borrow().is_none() {
            return false;
        }
        if check_paused {
            !self.job_paused.get()
        } else {
            true
        }
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<History>, crate::confighelper::ConfigError> {
    History::new(config)
}

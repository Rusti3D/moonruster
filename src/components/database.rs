// Sqlite database for Moonruster persistent storage (port of moonraker/components/database.py)
//
// The on-disk format is identical to Moonraker's so existing databases can be
// used directly.

use crate::common::*;
use crate::confighelper::ConfigHelper;
use crate::server::{server, Component};
use async_trait::async_trait;
use rusqlite::types::ValueRef;
use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub const DATABASE_VERSION: i64 = 2;
pub const SQL_DB_FILENAME: &str = "moonraker-sql.db";
pub const NAMESPACE_TABLE: &str = "namespace_store";
pub const REGISTRATION_TABLE: &str = "table_registry";

// ***** Record encoding *****

pub fn encode_record(value: &Value) -> Result<Vec<u8>, ServerError> {
    match value {
        Value::Null => Ok(vec![0u8]),
        Value::Bool(b) => Ok(vec![b'?', *b as u8]),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                let mut v = vec![b'q'];
                v.extend_from_slice(&i.to_ne_bytes());
                Ok(v)
            } else if n.is_u64() {
                Err(ServerError::bad(format!("Error encoding val: {}, type: <class 'int'>", n)))
            } else {
                let mut v = vec![b'd'];
                v.extend_from_slice(&n.as_f64().unwrap_or(0.0).to_ne_bytes());
                Ok(v)
            }
        }
        Value::String(s) => {
            let mut v = vec![b's'];
            v.extend_from_slice(s.as_bytes());
            Ok(v)
        }
        Value::Array(_) | Value::Object(_) => Ok(value.to_string().into_bytes()),
    }
}

pub fn decode_record(b: &[u8]) -> Result<Value, ServerError> {
    let fmt = *b.first().ok_or_else(|| ServerError::bad("Error decoding empty value"))?;
    let err = || ServerError::bad(format!("Error decoding value {}, format: {}", String::from_utf8_lossy(b), fmt as char));
    match fmt {
        b'q' => {
            let arr: [u8; 8] = b.get(1..9).ok_or_else(err)?.try_into().map_err(|_| err())?;
            Ok(json!(i64::from_ne_bytes(arr)))
        }
        b'd' => {
            let arr: [u8; 8] = b.get(1..9).ok_or_else(err)?.try_into().map_err(|_| err())?;
            Ok(crate::utils::float_value(f64::from_ne_bytes(arr)))
        }
        b'?' => Ok(Value::Bool(b.get(1).map(|x| *x != 0).ok_or_else(err)?)),
        b's' => Ok(Value::String(String::from_utf8(b[1..].to_vec()).map_err(|_| err())?)),
        b'[' | b'{' => serde_json::from_slice(b).map_err(|_| err()),
        0 => Ok(Value::Null),
        _ => Err(err()),
    }
}

fn value_ref_bytes(v: ValueRef) -> Vec<u8> {
    match v {
        ValueRef::Blob(b) => b.to_vec(),
        ValueRef::Text(t) => t.to_vec(),
        ValueRef::Integer(i) => i.to_string().into_bytes(),
        ValueRef::Real(r) => r.to_string().into_bytes(),
        ValueRef::Null => vec![0],
    }
}

/// Converts a sql column value into json, emulating python's sqlite3
/// converters for declared column types.
pub fn column_to_json(v: ValueRef, decl_type: Option<&str>) -> Value {
    let dt = decl_type.map(|d| d.to_lowercase());
    match dt.as_deref() {
        Some("record") => decode_record(&value_ref_bytes(v)).unwrap_or(Value::Null),
        Some("pyjson") => match v {
            ValueRef::Null => Value::Null,
            other => serde_json::from_slice(&value_ref_bytes(other)).unwrap_or(Value::Null),
        },
        Some("pybool") => match v {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(i) => Value::Bool(i != 0),
            other => Value::Bool(!value_ref_bytes(other).is_empty()),
        },
        _ => match v {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(i) => json!(i),
            ValueRef::Real(r) => crate::utils::float_value(r),
            ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).to_string()),
            ValueRef::Blob(b) => Value::Array(b.iter().map(|x| json!(x)).collect()),
        },
    }
}

/// Converts a json value into a sql parameter (python adapters: list/dict -> json)
pub fn json_to_sql(v: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as SV;
    match v {
        Value::Null => SV::Null,
        Value::Bool(b) => SV::Integer(*b as i64),
        Value::Number(n) => match n.as_i64() {
            Some(i) => SV::Integer(i),
            None => SV::Real(n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => SV::Text(s.clone()),
        Value::Array(_) | Value::Object(_) => SV::Blob(v.to_string().into_bytes()),
    }
}

pub fn parse_namespace_key(key: &Value) -> Result<Vec<String>, ServerError> {
    let key_list: Vec<String> = match key {
        Value::Array(a) => a.iter().map(crate::pyfmt::py_str).collect(),
        Value::String(s) => s.split('.').map(String::from).collect(),
        _ => Vec::new(),
    };
    if key_list.is_empty() || key_list.iter().any(|k| k.is_empty()) {
        return Err(ServerError::bad(format!("Invalid Key Format: '{}'", crate::pyfmt::py_str(key))));
    }
    Ok(key_list)
}

fn getitem<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(o) => o.get(key),
        _ => None,
    }
}

// ***** Table definitions *****

pub trait SqlTableDefinition {
    fn name(&self) -> &str;
    fn version(&self) -> i64;
    fn prototype(&self) -> String;
    fn migrate(&self, last_version: i64, db: &MoonrakerDatabase) -> Result<(), ServerError>;
}

/// python's inspect.cleandoc applied to a table prototype
pub fn cleandoc(s: &str) -> String {
    let s = s.trim();
    let lines: Vec<&str> = s.lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    let margin = lines[1..]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let mut out = vec![lines[0].trim_start().to_string()];
    for l in &lines[1..] {
        if l.len() >= margin {
            out.push(l[margin..].to_string());
        } else {
            out.push(l.trim_start().to_string());
        }
    }
    while out.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        out.pop();
    }
    out.join("\n")
}

// ***** Database *****

pub struct MoonrakerDatabase {
    conn: RefCell<Connection>,
    sql_db: PathBuf,
    namespaces: RefCell<BTreeSet<String>>,
    tables: RefCell<BTreeSet<String>>,
    registered_namespaces: RefCell<BTreeSet<String>>,
    registered_tables: RefCell<BTreeSet<String>>,
    protected_namespaces: RefCell<BTreeSet<String>>,
    forbidden_namespaces: RefCell<BTreeSet<String>>,
    debug_counter: RefCell<Map<String, Value>>,
    unsafe_shutdowns: i64,
    restored: Cell<bool>,
    initialized: Cell<bool>,
    backup_lock: tokio::sync::Mutex<()>,
}

#[async_trait(?Send)]
impl Component for MoonrakerDatabase {
    fn has_init(&self) -> bool {
        true
    }
    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        self.initialized.set(true);
        self.insert_item_sync("database", &json!("unsafe_shutdowns"), json!(self.unsafe_shutdowns + 1))?;
        Ok(())
    }
    async fn close(self: Rc<Self>) {
        if !self.restored.get() {
            let _ = self.insert_item_sync("database", &json!("unsafe_shutdowns"), json!(self.unsafe_shutdowns));
        }
    }
}

macro_rules! endpoint {
    ($srv:expr, $db:expr, $path:expr, $rt:expr, $method:ident, $debug:expr) => {{
        let d = $db.clone();
        let cb = callback(move |req: WebRequest| {
            let d = d.clone();
            async move { d.$method(req).await }
        });
        if $debug {
            let _ = $srv.register_debug_endpoint($path, $rt, cb);
        } else {
            let _ = $srv.register_endpoint($path, $rt, cb);
        }
    }};
}

impl MoonrakerDatabase {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, crate::confighelper::ConfigError> {
        let srv = server();
        let instance_id = srv.app_arg_str("instance_uuid");
        let db_path = Self::get_database_folder(config)?;
        let sql_db = db_path.join(SQL_DB_FILENAME);
        let conn = Connection::open(&sql_db).map_err(|e| ConfigHelper::error(format!("Unable to open database: {}", e)))?;
        let _ = conn.busy_timeout(std::time::Duration::from_secs(1));
        let db = Rc::new(MoonrakerDatabase {
            conn: RefCell::new(conn),
            sql_db,
            namespaces: RefCell::new(BTreeSet::new()),
            tables: RefCell::new(BTreeSet::new()),
            registered_namespaces: RefCell::new(["moonraker", "database"].iter().map(|s| s.to_string()).collect()),
            registered_tables: RefCell::new([NAMESPACE_TABLE, REGISTRATION_TABLE].iter().map(|s| s.to_string()).collect()),
            protected_namespaces: RefCell::new(BTreeSet::new()),
            forbidden_namespaces: RefCell::new(BTreeSet::new()),
            debug_counter: RefCell::new(Map::new()),
            unsafe_shutdowns: 0,
            restored: Cell::new(false),
            initialized: Cell::new(false),
            backup_lock: tokio::sync::Mutex::new(()),
        });
        db.setup_database().map_err(|e| ConfigHelper::error(e.message))?;
        let stored_iid = db.get_item_sync("moonraker", Some(&json!("instance_id")), Some(Value::Null)).unwrap_or(Value::Null);
        if !stored_iid.is_null() {
            if stored_iid.as_str() != Some(instance_id.as_str()) {
                srv.add_log_rollover_item(
                    "uuid_mismatch",
                    &format!(
                        "Database: Stored Instance ID does not match current Instance ID.\n\nCurrent UUID: {}\nStored UUID: {}",
                        instance_id,
                        crate::pyfmt::py_str(&stored_iid)
                    ),
                    true,
                );
            }
        } else {
            let _ = db.insert_item_sync("moonraker", &json!("instance_id"), json!(instance_id));
        }
        let dbinfo = db.get_item_sync("database", None, Some(json!({}))).unwrap_or(json!({}));
        let mut ptns: BTreeSet<String> = dbinfo
            .get("protected_namespaces")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let mut fbns: BTreeSet<String> = dbinfo
            .get("forbidden_namespaces")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        ptns.insert("moonraker".into());
        fbns.insert("database".into());
        *db.protected_namespaces.borrow_mut() = ptns;
        *db.forbidden_namespaces.borrow_mut() = fbns;
        config.getboolean_dep("enable_database_debug", false, true)?;
        let mut counter = Map::new();
        counter.insert("get".into(), json!(0));
        counter.insert("post".into(), json!(0));
        counter.insert("delete".into(), json!(0));
        if let Some(Value::Object(dc)) = dbinfo.get("debug_counter") {
            for (k, v) in dc {
                counter.insert(k.clone(), v.clone());
            }
            srv.add_log_rollover_item(
                "database_debug_counter",
                &format!("Database Debug Counter: {}", crate::pyfmt::py_repr(&Value::Object(counter.clone()))),
                true,
            );
        }
        *db.debug_counter.borrow_mut() = counter;
        let unsafe_shutdowns = dbinfo.get("unsafe_shutdowns").and_then(|v| v.as_i64()).unwrap_or(0);
        // safe: no other references exist yet
        let db = {
            let mut d = Rc::try_unwrap(db).ok().expect("database not shared");
            d.unsafe_shutdowns = unsafe_shutdowns;
            Rc::new(d)
        };
        srv.add_log_rollover_item("database", &format!("Unsafe Shutdown Count: {}", unsafe_shutdowns), true);
        let _ = db.insert_item_sync("database", &json!("database_version"), json!(DATABASE_VERSION));
        endpoint!(srv, db, "/server/database/list", RequestType::GET, handle_list_request, false);
        endpoint!(srv, db, "/server/database/item", RequestType::all(), handle_item_request, false);
        endpoint!(srv, db, "/server/database/backup", RequestType::POST | RequestType::DELETE, handle_backup_request, false);
        endpoint!(srv, db, "/server/database/restore", RequestType::POST, handle_restore_request, false);
        endpoint!(srv, db, "/server/database/compact", RequestType::POST, handle_compact_request, false);
        endpoint!(srv, db, "/debug/database/list", RequestType::GET, handle_list_request, true);
        endpoint!(srv, db, "/debug/database/item", RequestType::all(), handle_item_request, true);
        endpoint!(srv, db, "/debug/database/table", RequestType::GET, handle_table_request, true);
        Ok(db)
    }

    fn get_database_folder(config: &ConfigHelper) -> Result<PathBuf, crate::confighelper::ConfigError> {
        let srv = server();
        let dep_path = config.get_opt_dep("database_path", true)?;
        let mut db_path = PathBuf::from(srv.data_path()).join("database");
        let is_default = srv.get_app_arg("is_default_data_path").and_then(|v| v.as_bool()).unwrap_or(false);
        if is_default && !db_path.join(SQL_DB_FILENAME).exists() {
            let dep = dep_path.unwrap_or_else(|| "~/.moonraker_database".into());
            let legacy = crate::utils::resolve_path(&crate::utils::expand_user(&dep));
            let same = match (std::fs::canonicalize(&legacy), std::fs::canonicalize(&db_path)) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            };
            if !same && legacy.join("data.mdb").is_file() {
                log::info!("Reverting to legacy database folder: {}", legacy.display());
                db_path = legacy;
            }
        }
        if !db_path.is_dir() {
            std::fs::create_dir(&db_path)
                .map_err(|e| ConfigHelper::error(format!("Unable to create database folder {}: {}", db_path.display(), e)))?;
        }
        Ok(db_path)
    }

    fn setup_database(&self) -> Result<(), ServerError> {
        server().add_log_rollover_item(
            "sqlite_intro",
            &format!("Loading Sqlite database provider. Sqlite Version: {}", rusqlite::version()),
            true,
        );
        let tables: BTreeSet<String> = {
            let conn = self.conn.borrow();
            let mut stmt = conn.prepare("SELECT name FROM sqlite_master WHERE type='table'")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        *self.tables.borrow_mut() = tables.clone();
        log::debug!("Detected SQL Tables: {:?}", tables);
        if !tables.contains(NAMESPACE_TABLE) {
            self.create_default_tables()?;
            if self.sql_db.parent().map(|p| p.join("data.mdb").is_file()).unwrap_or(false) {
                server().add_warning(
                    "Database: Legacy LMDB database detected.  Moonruster does not convert LMDB databases, run Moonraker once to migrate.",
                    None,
                    true,
                );
            }
        } else if !tables.contains(REGISTRATION_TABLE) {
            self.create_registration_table()?;
        }
        let namespaces: BTreeSet<String> = {
            let conn = self.conn.borrow();
            let mut stmt = conn.prepare(&format!("SELECT DISTINCT namespace FROM {}", NAMESPACE_TABLE))?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        log::debug!("Detected namespaces: {:?}", namespaces);
        *self.namespaces.borrow_mut() = namespaces;
        Ok(())
    }

    fn create_default_tables(&self) -> Result<(), ServerError> {
        self.create_registration_table()?;
        if self.tables.borrow().contains(NAMESPACE_TABLE) {
            return Ok(());
        }
        let proto = format!(
            "{} (\n    namespace TEXT NOT NULL,\n    key TEXT NOT NULL,\n    value record NOT NULL,\n    PRIMARY KEY (namespace, key)\n)",
            NAMESPACE_TABLE
        );
        self.conn.borrow().execute(&format!("CREATE TABLE {}", proto), [])?;
        self.save_registered_table(NAMESPACE_TABLE, &proto, 1)?;
        server().add_log_rollover_item("db_default_table", &format!("Created default SQL table {}", NAMESPACE_TABLE), true);
        Ok(())
    }

    fn create_registration_table(&self) -> Result<(), ServerError> {
        if self.tables.borrow().contains(REGISTRATION_TABLE) {
            return Ok(());
        }
        let proto = format!(
            "{} (\n    name TEXT NOT NULL PRIMARY KEY,\n    prototype TEXT NOT NULL,\n    version INT\n)",
            REGISTRATION_TABLE
        );
        self.conn.borrow().execute(&format!("CREATE TABLE {}", proto), [])?;
        self.tables.borrow_mut().insert(REGISTRATION_TABLE.into());
        Ok(())
    }

    fn save_registered_table(&self, name: &str, prototype: &str, version: i64) -> Result<(), ServerError> {
        self.conn.borrow().execute(
            &format!(
                "INSERT INTO {} VALUES(?, ?, ?) ON CONFLICT(name) DO UPDATE SET prototype=excluded.prototype, version=excluded.version",
                REGISTRATION_TABLE
            ),
            params![name, prototype, version],
        )?;
        self.tables.borrow_mut().insert(name.to_string());
        Ok(())
    }

    fn lookup_registered_table(&self, name: &str) -> (String, i64) {
        let conn = self.conn.borrow();
        conn.query_row(
            &format!("SELECT prototype, version FROM {} WHERE name = ?", REGISTRATION_TABLE),
            params![name],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0))),
        )
        .unwrap_or((String::new(), 0))
    }

    pub fn get_database_path(&self) -> String {
        self.sql_db.to_string_lossy().to_string()
    }

    // ***** Record level ops *****

    fn insert_record(&self, namespace: &str, key: &str, val: &Value) -> bool {
        if val.is_null() {
            return false;
        }
        let enc = match encode_record(val) {
            Ok(e) => e,
            Err(e) => {
                log::info!("{}", e);
                return false;
            }
        };
        let res = self.conn.borrow().execute(
            &format!(
                "INSERT INTO {} VALUES(?, ?, ?) ON CONFLICT(namespace, key) DO UPDATE SET value=excluded.value",
                NAMESPACE_TABLE
            ),
            params![namespace, key, enc],
        );
        if res.is_err() {
            if server().is_verbose_enabled() {
                log::error!("Error inserting record for key");
            }
            return false;
        }
        true
    }

    fn get_record(&self, namespace: &str, key: &str) -> Result<Option<Value>, ServerError> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare_cached(&format!("SELECT value FROM {} WHERE namespace = ? and key = ?", NAMESPACE_TABLE))?;
        let mut rows = stmt.query(params![namespace, key])?;
        match rows.next()? {
            None => Ok(None),
            Some(row) => Ok(Some(decode_record(&value_ref_bytes(row.get_ref(0)?))?)),
        }
    }

    fn get_record_or_404(&self, namespace: &str, key: &str) -> Result<Value, ServerError> {
        self.get_record(namespace, key)?
            .ok_or_else(|| ServerError::new(format!("Key '{}' in namespace '{}' not found", key, namespace), 404))
    }

    pub fn get_namespace(&self, namespace: &str, must_exist: bool) -> Result<Map<String, Value>, ServerError> {
        if !self.namespaces.borrow().contains(namespace) {
            if !must_exist {
                return Ok(Map::new());
            }
            return Err(ServerError::new(format!("Namespace {} not found", namespace), 404));
        }
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(&format!("SELECT key, value FROM {} WHERE namespace = ?", NAMESPACE_TABLE))?;
        let mut rows = stmt.query(params![namespace])?;
        let mut out = Map::new();
        while let Some(row) = rows.next()? {
            let k: String = row.get(0)?;
            let v = decode_record(&value_ref_bytes(row.get_ref(1)?))?;
            out.insert(k, v);
        }
        Ok(out)
    }

    pub fn clear_namespace(&self, namespace: &str) -> Result<(), ServerError> {
        self.conn.borrow().execute(&format!("DELETE FROM {} WHERE namespace = ?", NAMESPACE_TABLE), params![namespace])?;
        Ok(())
    }

    pub fn drop_empty_namespace(&self, namespace: &str) -> Result<(), ServerError> {
        if self.namespaces.borrow().contains(namespace) && self.get_namespace_length(namespace)? == 0 {
            self.namespaces.borrow_mut().remove(namespace);
        }
        Ok(())
    }

    pub fn get_namespace_length(&self, namespace: &str) -> Result<i64, ServerError> {
        Ok(self.conn.borrow().query_row(
            &format!("SELECT COUNT(namespace) FROM {} WHERE namespace = ?", NAMESPACE_TABLE),
            params![namespace],
            |r| r.get(0),
        )?)
    }

    pub fn get_namespace_keys(&self, namespace: &str) -> Result<Vec<String>, ServerError> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(&format!("SELECT key FROM {} WHERE namespace = ?", NAMESPACE_TABLE))?;
        let rows = stmt.query_map(params![namespace], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn get_namespace_values(&self, namespace: &str) -> Result<Vec<Value>, ServerError> {
        Ok(self.get_namespace_items(namespace)?.into_iter().map(|(_, v)| v).collect())
    }

    pub fn get_namespace_items(&self, namespace: &str) -> Result<Vec<(String, Value)>, ServerError> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(&format!("SELECT key, value FROM {} WHERE namespace = ?", NAMESPACE_TABLE))?;
        let mut rows = stmt.query(params![namespace])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push((row.get::<_, String>(0)?, decode_record(&value_ref_bytes(row.get_ref(1)?))?));
        }
        Ok(out)
    }

    pub fn insert_item_sync(&self, namespace: &str, key: &Value, value: Value) -> Result<(), ServerError> {
        let key_list = parse_namespace_key(key)?;
        let record = if key_list.len() > 1 {
            let mut record = self.get_record(namespace, &key_list[0])?.unwrap_or(json!({}));
            if !record.is_object() {
                log::info!(
                    "Warning: Key {} contains a value of type <class '{}'>. Overwriting with an object.",
                    key_list[0],
                    crate::pyfmt::py_type_name(&record)
                );
                record = json!({});
            }
            {
                let mut item = &mut record;
                for k in &key_list[1..key_list.len() - 1] {
                    let tname = crate::pyfmt::py_type_name(item);
                    let obj = item
                        .as_object_mut()
                        .ok_or_else(|| ServerError::bad(format!("Cannot reduce a value of type <class '{}'>", tname)))?;
                    item = obj.entry(k.clone()).or_insert_with(|| json!({}));
                }
                match item.as_object_mut() {
                    Some(o) => {
                        o.insert(key_list.last().unwrap().clone(), value);
                    }
                    None => {
                        let rpt_key = key_list[..key_list.len() - 1].join(".");
                        return Err(ServerError::bad(format!(
                            "Item at key '{}' in namespace '{}'is not a dictionary object, cannot insert",
                            rpt_key, namespace
                        )));
                    }
                }
            }
            record
        } else {
            value
        };
        if !self.insert_record(namespace, &key_list[0], &record) {
            log::info!("Error inserting key '{}' in namespace '{}'", crate::pyfmt::py_str(key), namespace);
        } else {
            self.namespaces.borrow_mut().insert(namespace.to_string());
        }
        Ok(())
    }

    pub fn delete_item_sync(&self, namespace: &str, key: &Value) -> Result<Value, ServerError> {
        let key_list = parse_namespace_key(key)?;
        let mut record = self.get_record_or_404(namespace, &key_list[0])?;
        let mut val = record.clone();
        let mut remove_record = true;
        if key_list.len() > 1 {
            let not_found = || ServerError::new(format!("Key '{}' in namespace '{}' not found", crate::pyfmt::py_str(key), namespace), 404);
            if !record.is_object() {
                return Err(not_found());
            }
            {
                let mut item = &mut record;
                for k in &key_list[1..key_list.len() - 1] {
                    item = item.as_object_mut().and_then(|o| o.get_mut(k)).ok_or_else(not_found)?;
                }
                val = item
                    .as_object_mut()
                    .and_then(|o| o.shift_remove(key_list.last().unwrap()))
                    .ok_or_else(not_found)?;
            }
            remove_record = record.as_object().map(|o| o.is_empty()).unwrap_or(true);
        }
        if remove_record {
            self.conn.borrow().execute(
                &format!("DELETE FROM {} WHERE namespace = ? and key = ?", NAMESPACE_TABLE),
                params![namespace, key_list[0]],
            )?;
        } else if !self.insert_record(namespace, &key_list[0], &record) {
            log::info!("Error deleting key '{}' from namespace '{}'", crate::pyfmt::py_str(key), namespace);
        }
        Ok(val)
    }

    /// get_item with optional default (None = MISSING)
    pub fn get_item_sync(&self, namespace: &str, key: Option<&Value>, default: Option<Value>) -> Result<Value, ServerError> {
        let res: Result<Value, ServerError> = (|| {
            let key = match key {
                None => return Ok(Value::Object(self.get_namespace(namespace, true)?)),
                Some(k) => k,
            };
            let key_list = parse_namespace_key(key)?;
            let rec = self.get_record_or_404(namespace, &key_list[0])?;
            let mut cur = &rec;
            for k in &key_list[1..] {
                cur = getitem(cur, k)
                    .ok_or_else(|| ServerError::new(format!("Key '{}' in namespace '{}' not found", crate::pyfmt::py_str(key), namespace), 404))?;
            }
            Ok(cur.clone())
        })();
        match res {
            Ok(v) => Ok(v),
            Err(e) => match default {
                Some(d) => Ok(d),
                None => Err(e),
            },
        }
    }

    /// SqliteProvider.update_item: dict values merge into a dict record or
    /// item, anything else replaces it
    pub fn update_item_sync(&self, namespace: &str, key: &Value, value: Value) -> Result<(), ServerError> {
        let key_list = parse_namespace_key(key)?;
        let mut record = self.get_record_or_404(namespace, &key_list[0])?;
        let merge = |slot: &mut Value, value: Value| match (slot.as_object_mut(), value) {
            (Some(o), Value::Object(v)) => o.extend(v),
            (_, v) => *slot = v,
        };
        if key_list.len() == 1 {
            merge(&mut record, value);
        } else {
            let not_found = || ServerError::new(format!("Key '{}' in namespace '{}' not found", crate::pyfmt::py_str(key), namespace), 404);
            if !record.is_object() {
                return Err(not_found());
            }
            let mut item = &mut record;
            for k in &key_list[1..key_list.len() - 1] {
                item = item.as_object_mut().and_then(|o| o.get_mut(k)).ok_or_else(not_found)?;
            }
            let last = key_list.last().unwrap();
            match item.as_object_mut().and_then(|o| o.get_mut(last)) {
                Some(slot) => merge(slot, value),
                None => {
                    return Err(ServerError::bad(format!(
                        "Item at key '{}' in namespace '{}'is not a dictionary object, cannot update",
                        key_list[..key_list.len() - 1].join("."),
                        namespace
                    )))
                }
            }
        }
        if !self.insert_record(namespace, &key_list[0], &record) {
            log::info!("Error updating key '{}' in namespace '{}'", crate::pyfmt::py_str(key), namespace);
        }
        Ok(())
    }

    /// SqliteProvider.insert_batch: every record in one transaction
    pub fn insert_batch_sync(&self, namespace: &str, records: &Map<String, Value>) -> Result<(), ServerError> {
        let encoded = records.iter().map(|(k, v)| Ok((k, encode_record(v)?))).collect::<Result<Vec<_>, ServerError>>()?;
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;
        for (k, enc) in encoded {
            tx.execute(
                &format!("INSERT INTO {} VALUES(?, ?, ?) ON CONFLICT(namespace, key) DO UPDATE SET value=excluded.value", NAMESPACE_TABLE),
                params![namespace, k, enc],
            )?;
        }
        tx.commit()?;
        self.namespaces.borrow_mut().insert(namespace.to_string());
        Ok(())
    }

    /// SqliteProvider.sync_namespace: replaces the namespace's records. As
    /// there, the values go in through sqlite3's adapters, not encode_record
    pub fn sync_namespace_sync(&self, namespace: &str, values: &Map<String, Value>) -> Result<(), ServerError> {
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;
        tx.execute(&format!("DELETE FROM {} WHERE namespace = ?", NAMESPACE_TABLE), params![namespace])?;
        for (k, v) in values {
            tx.execute(&format!("INSERT INTO {} VALUES(?, ?, ?)", NAMESPACE_TABLE), params![namespace, k, json_to_sql(v)])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// SqliteProvider.is_alive(): its thread starts in component_init, before
    /// that calls run in place
    pub fn is_alive(&self) -> bool {
        self.initialized.get()
    }

    pub fn move_batch_sync(&self, namespace: &str, source: &[String], dest: &[String]) -> Result<(), ServerError> {
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;
        for (s, d) in source.iter().zip(dest.iter()) {
            tx.execute(
                &format!("UPDATE OR REPLACE {} SET key = ? WHERE namespace = ? and key = ?", NAMESPACE_TABLE),
                params![d, namespace, s],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_batch_sync(&self, namespace: &str, keys: &[String]) -> Result<Map<String, Value>, ServerError> {
        let vals = self.get_batch_sync(namespace, keys)?;
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;
        for k in keys {
            tx.execute(&format!("DELETE FROM {} WHERE namespace = ? and key = ?", NAMESPACE_TABLE), params![namespace, k])?;
        }
        tx.commit()?;
        Ok(vals)
    }

    pub fn get_batch_sync(&self, namespace: &str, keys: &[String]) -> Result<Map<String, Value>, ServerError> {
        let mut out = Map::new();
        if keys.is_empty() {
            return Ok(out);
        }
        let placeholders = vec!["?"; keys.len()].join(",");
        let sql = format!("SELECT key, value FROM {} WHERE namespace = ? and key IN ({})", NAMESPACE_TABLE, placeholders);
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(&sql)?;
        let mut p: Vec<&dyn rusqlite::ToSql> = vec![&namespace];
        for k in keys {
            p.push(k);
        }
        let mut rows = stmt.query(p.as_slice())?;
        while let Some(row) = rows.next()? {
            out.insert(row.get::<_, String>(0)?, decode_record(&value_ref_bytes(row.get_ref(1)?))?);
        }
        Ok(out)
    }

    // ***** Async wrappers matching the python API *****

    pub async fn insert_item(&self, namespace: &str, key: &Value, value: Value) -> Result<(), ServerError> {
        self.insert_item_sync(namespace, key, value)
    }

    pub fn register_local_namespace(self: &Rc<Self>, namespace: &str, forbidden: bool, parse_keys: bool) -> Result<NamespaceWrapper, ServerError> {
        if self.registered_namespaces.borrow().contains(namespace) {
            return Err(ServerError::bad(format!("Namespace '{}' already registered", namespace)));
        }
        self.registered_namespaces.borrow_mut().insert(namespace.to_string());
        self.namespaces.borrow_mut().insert(namespace.to_string());
        if forbidden {
            if !self.forbidden_namespaces.borrow().contains(namespace) {
                self.forbidden_namespaces.borrow_mut().insert(namespace.to_string());
                let list: Vec<Value> = self.forbidden_namespaces.borrow().iter().map(|s| json!(s)).collect();
                let _ = self.insert_item_sync("database", &json!("forbidden_namespaces"), Value::Array(list));
            }
        } else if !self.protected_namespaces.borrow().contains(namespace) {
            self.protected_namespaces.borrow_mut().insert(namespace.to_string());
            let list: Vec<Value> = self.protected_namespaces.borrow().iter().map(|s| json!(s)).collect();
            let _ = self.insert_item_sync("database", &json!("protected_namespaces"), Value::Array(list));
        }
        Ok(NamespaceWrapper { namespace: namespace.to_string(), db: self.clone(), parse_keys })
    }

    pub fn wrap_namespace(self: &Rc<Self>, namespace: &str, parse_keys: bool) -> Result<NamespaceWrapper, ServerError> {
        if !self.namespaces.borrow().contains(namespace) {
            return Err(ServerError::new(format!("Namespace '{}' not found", namespace), 404));
        }
        Ok(NamespaceWrapper { namespace: namespace.to_string(), db: self.clone(), parse_keys })
    }

    pub fn unregister_local_namespace(&self, namespace: &str) {
        self.registered_namespaces.borrow_mut().remove(namespace);
        if self.forbidden_namespaces.borrow_mut().remove(namespace) {
            let list: Vec<Value> = self.forbidden_namespaces.borrow().iter().map(|s| json!(s)).collect();
            let _ = self.insert_item_sync("database", &json!("forbidden_namespaces"), Value::Array(list));
        }
        if self.protected_namespaces.borrow_mut().remove(namespace) {
            let list: Vec<Value> = self.protected_namespaces.borrow().iter().map(|s| json!(s)).collect();
            let _ = self.insert_item_sync("database", &json!("protected_namespaces"), Value::Array(list));
        }
    }

    pub fn wipe_local_namespace(&self, namespace: &str) -> Result<(), ServerError> {
        self.clear_namespace(namespace)?;
        self.drop_empty_namespace(namespace)?;
        self.unregister_local_namespace(namespace);
        Ok(())
    }

    pub fn get_backup_dir(&self) -> PathBuf {
        crate::utils::resolve_path(&PathBuf::from(server().data_path()).join("backup/database"))
    }

    pub fn register_table(&self, table_def: &dyn SqlTableDefinition) -> Result<(), ServerError> {
        let name = table_def.name().to_string();
        if self.registered_tables.borrow().contains(&name) {
            return Err(ServerError::bad(format!("Table '{}' already registered", name)));
        }
        self.registered_tables.borrow_mut().insert(name.clone());
        if self.initialized.get() {
            return Err(ServerError::bad("Table registration must occur during during init."));
        }
        let prototype = cleandoc(&table_def.prototype());
        let (detected_proto, version) = if self.tables.borrow().contains(&name) {
            log::info!("Found registered table {}", name);
            if name == NAMESPACE_TABLE || name == REGISTRATION_TABLE {
                return Err(ServerError::bad(format!("Cannot register table '{}', it is reserved", name)));
            }
            self.lookup_registered_table(&name)
        } else {
            log::info!("Creating table {}...", name);
            self.conn.borrow().execute(&format!("CREATE TABLE {}", prototype), [])?;
            (prototype.clone(), 0)
        };
        if table_def.version() > version {
            table_def.migrate(version, self)?;
            self.save_registered_table(&name, &prototype, table_def.version())?;
        } else if detected_proto != prototype {
            server().add_warning(
                &format!("Table '{}' definition does not match stored definition.  See the log for details.", name),
                None,
                true,
            );
            log::info!("Expected table prototype:\n{}\n\nStored table prototype:\n{}", prototype, detected_proto);
        }
        Ok(())
    }

    /// Execute a statement returning rows as json objects.  Columns are
    /// converted according to their declared types.
    pub fn sql_query(&self, sql: &str, params: &[rusqlite::types::Value]) -> Result<Vec<Map<String, Value>>, ServerError> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(sql)?;
        let cols: Vec<(String, Option<String>)> =
            stmt.columns().iter().map(|c| (c.name().to_string(), c.decl_type().map(String::from))).collect();
        let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let mut m = Map::new();
            for (i, (name, dt)) in cols.iter().enumerate() {
                m.insert(name.clone(), column_to_json(row.get_ref(i)?, dt.as_deref()));
            }
            out.push(m);
        }
        Ok(out)
    }

    /// Execute a statement, returning (rowcount, lastrowid)
    pub fn sql_execute(&self, sql: &str, params: &[rusqlite::types::Value]) -> Result<(usize, i64), ServerError> {
        let conn = self.conn.borrow();
        let n = conn.execute(sql, rusqlite::params_from_iter(params.iter()))?;
        Ok((n, conn.last_insert_rowid()))
    }

    pub fn sql_executemany(&self, sql: &str, params: &[Vec<rusqlite::types::Value>]) -> Result<usize, ServerError> {
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;
        let mut total = 0;
        {
            let mut stmt = tx.prepare(sql)?;
            for p in params {
                total += stmt.execute(rusqlite::params_from_iter(p.iter()))?;
            }
        }
        tx.commit()?;
        Ok(total)
    }

    pub fn compact_database(&self) -> Result<Value, ServerError> {
        if self.restored.get() {
            return Err(ServerError::bad("Cannot compact restored database, awaiting restart"));
        }
        let cur_size = std::fs::metadata(&self.sql_db).map(|m| m.len()).unwrap_or(0);
        self.conn.borrow().execute_batch("VACUUM")?;
        let new_size = std::fs::metadata(&self.sql_db).map(|m| m.len()).unwrap_or(0);
        Ok(json!({"previous_size": cur_size, "new_size": new_size}))
    }

    pub fn backup_database(&self, bkp_path: &Path) -> Result<(), ServerError> {
        if self.restored.get() {
            return Err(ServerError::bad("Cannot backup restored database, awaiting restart"));
        }
        if let Some(parent) = bkp_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            } else if bkp_path.exists() {
                std::fs::remove_file(bkp_path)?;
            }
        }
        let mut dst = Connection::open(bkp_path)?;
        let conn = self.conn.borrow();
        let backup = rusqlite::backup::Backup::new(&conn, &mut dst)?;
        backup.run_to_completion(100, std::time::Duration::from_millis(0), None)?;
        Ok(())
    }

    pub fn restore_database(&self, restore_path: &Path) -> Result<Value, ServerError> {
        if self.restored.get() {
            return Err(ServerError::bad("Database already restored"));
        }
        if !restore_path.is_file() {
            return Err(ServerError::bad(format!("Restoration File {} does not exist", restore_path.display())));
        }
        let src = Connection::open(restore_path)?;
        let tables: Vec<String> = {
            let mut stmt = src.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        if !tables.iter().any(|t| t == NAMESPACE_TABLE) {
            return Err(ServerError::bad(format!("Invalid database for restoration, missing table '{}'", NAMESPACE_TABLE)));
        }
        let missing: Vec<String> = self.tables.borrow().iter().filter(|t| !tables.contains(t)).cloned().collect();
        if !missing.is_empty() {
            log::info!("Database to restore missing tables: {:?}", missing);
        }
        let namespaces: Vec<String> = {
            let mut stmt = src.prepare(&format!("SELECT DISTINCT namespace FROM {}", NAMESPACE_TABLE))?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        let missing_ns: Vec<String> = self.namespaces.borrow().iter().filter(|n| !namespaces.contains(n)).cloned().collect();
        if !missing_ns.is_empty() {
            log::info!("Database to restore missing namespaces: {:?}", missing_ns);
        }
        {
            let mut conn = self.conn.borrow_mut();
            let backup = rusqlite::backup::Backup::new(&src, &mut conn)?;
            backup.run_to_completion(100, std::time::Duration::from_millis(0), None)?;
        }
        self.restored.set(true);
        Ok(json!({"restored_tables": tables, "restored_namespaces": namespaces}))
    }

    pub fn tables(&self) -> Vec<String> {
        self.tables.borrow().iter().cloned().collect()
    }

    pub fn namespaces(&self) -> Vec<String> {
        self.namespaces.borrow().iter().cloned().collect()
    }

    pub fn registered_namespaces(&self) -> Vec<String> {
        self.registered_namespaces.borrow().iter().cloned().collect()
    }

    pub fn protected_namespaces(&self) -> Vec<String> {
        self.protected_namespaces.borrow().iter().cloned().collect()
    }

    pub fn forbidden_namespaces(&self) -> Vec<String> {
        self.forbidden_namespaces.borrow().iter().cloned().collect()
    }

    // ***** Endpoints *****

    fn is_printing() -> bool {
        server().klippy().is_printing()
    }

    async fn handle_compact_request(self: Rc<Self>, _req: WebRequest) -> ApiResult {
        if Self::is_printing() {
            return Err(ServerError::bad("Cannot compact when Klipper is printing"));
        }
        let _g = self.backup_lock.lock().await;
        self.compact_database()
    }

    fn check_backup_name(&self, db_name: &str) -> Result<PathBuf, ServerError> {
        let bkp_dir = self.get_backup_dir();
        let bkp_path = crate::utils::resolve_path(&bkp_dir.join(db_name));
        if !bkp_path.ancestors().skip(1).any(|p| p == bkp_dir) {
            return Err(ServerError::bad(format!("Invalid name {}.", db_name)));
        }
        Ok(bkp_path)
    }

    async fn handle_backup_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let _g = self.backup_lock.lock().await;
        let rt = req.get_request_type();
        let bkp_path;
        if rt == RequestType::POST {
            if Self::is_printing() {
                return Err(ServerError::bad("Cannot backup when Klipper is printing"));
            }
            let suffix = crate::utils::local_strftime("%Y%m%d-%H%M%S");
            let db_name = req.get_str_or("filename", &format!("sqldb-backup-{}.db", suffix));
            bkp_path = self.check_backup_name(&db_name)?;
            self.backup_database(&bkp_path)?;
        } else if rt == RequestType::DELETE {
            let db_name = req.get_str("filename")?;
            bkp_path = self.check_backup_name(&db_name)?;
            if !bkp_path.is_file() {
                return Err(ServerError::new(format!("Backup file {} does not exist", db_name), 404));
            }
            std::fs::remove_file(&bkp_path)?;
        } else {
            return Err(ServerError::bad("Invalid request type"));
        }
        Ok(json!({"backup_path": bkp_path.to_string_lossy()}))
    }

    async fn handle_restore_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        if Self::is_printing() {
            return Err(ServerError::bad("Cannot restore when Klipper is printing"));
        }
        let _g = self.backup_lock.lock().await;
        let db_name = req.get_str("filename")?;
        let restore_path = self.check_backup_name(&db_name)?;
        let info = self.restore_database(&restore_path)?;
        server().restart(Some(0.1));
        Ok(info)
    }

    async fn handle_list_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let path = req.get_endpoint().to_string();
        let mut ns_list: Vec<String> = self.namespaces();
        let bkp_dir = self.get_backup_dir();
        let mut backups = Vec::new();
        if bkp_dir.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&bkp_dir) {
                for e in rd.flatten() {
                    if e.path().is_file() {
                        backups.push(e.file_name().to_string_lossy().to_string());
                    }
                }
            }
        }
        if !path.starts_with("/debug/") {
            let fb = self.forbidden_namespaces.borrow();
            ns_list.retain(|n| !fb.contains(n));
            Ok(json!({"namespaces": ns_list, "backups": backups}))
        } else {
            Ok(json!({"namespaces": ns_list, "backups": backups, "tables": self.tables()}))
        }
    }

    async fn handle_item_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let req_type = req.get_request_type();
        let is_debug = req.get_endpoint().starts_with("/debug/");
        let namespace = req.get_str("namespace")?;
        if self.forbidden_namespaces.borrow().contains(&namespace) && !is_debug {
            return Err(ServerError::new(format!("Read/Write access to namespace '{}' is forbidden", namespace), 403));
        }
        let key;
        let val;
        if req_type == RequestType::GET {
            key = req.get_opt("key").unwrap_or(Value::Null);
            if !key.is_null() && !(key.is_array() || key.is_string()) {
                return Err(ServerError::bad(format!(
                    "Value for argument 'key' is an invalid type: {}",
                    crate::pyfmt::py_type_name(&key)
                )));
            }
            val = self.get_item_sync(&namespace, if key.is_null() { None } else { Some(&key) }, None)?;
        } else {
            if self.protected_namespaces.borrow().contains(&namespace) && !is_debug {
                return Err(ServerError::new(format!("Write access to namespace '{}' is forbidden", namespace), 403));
            }
            key = req.get("key")?;
            if !(key.is_array() || key.is_string()) {
                return Err(ServerError::bad(format!(
                    "Value for argument 'key' is an invalid type: {}",
                    crate::pyfmt::py_type_name(&key)
                )));
            }
            if req_type == RequestType::POST {
                val = req.get("value")?;
                self.insert_item_sync(&namespace, &key, val.clone())?;
            } else if req_type == RequestType::DELETE {
                val = self.delete_item_sync(&namespace, &key)?;
                self.drop_empty_namespace(&namespace)?;
            } else {
                return Err(ServerError::bad(format!("Invalid request type {}", req_type)));
            }
        }
        if is_debug {
            let name = req_type.name().unwrap_or("").to_lowercase();
            {
                let mut dc = self.debug_counter.borrow_mut();
                let cur = dc.get(&name).and_then(|v| v.as_i64()).unwrap_or(0);
                dc.insert(name, json!(cur + 1));
            }
            let dc = Value::Object(self.debug_counter.borrow().clone());
            self.insert_item_sync("database", &json!("debug_counter"), dc.clone())?;
            server().add_log_rollover_item(
                "database_debug_counter",
                &format!("Database Debug Counter: {}", crate::pyfmt::py_repr(&dc)),
                false,
            );
        }
        Ok(json!({"namespace": namespace, "key": key, "value": val}))
    }

    async fn handle_table_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let table = req.get_str("table")?;
        if !self.tables.borrow().contains(&table) {
            return Err(ServerError::new(format!("Table name '{}' does not exist", table), 404));
        }
        let rows = self.sql_query(&format!("SELECT rowid, * FROM {}", table), &[])?;
        Ok(json!({"table_name": table, "rows": rows}))
    }
}

// ***** Namespace Wrapper *****

#[derive(Clone)]
pub struct NamespaceWrapper {
    pub namespace: String,
    pub db: Rc<MoonrakerDatabase>,
    pub parse_keys: bool,
}

impl NamespaceWrapper {
    fn key(&self, key: &str) -> Value {
        if self.parse_keys {
            Value::String(key.to_string())
        } else {
            json!([key])
        }
    }

    pub fn insert(&self, key: &str, value: Value) -> Result<(), ServerError> {
        self.db.insert_item_sync(&self.namespace, &self.key(key), value)
    }

    pub fn get(&self, key: &str, default: Value) -> Value {
        self.db.get_item_sync(&self.namespace, Some(&self.key(key)), Some(default.clone())).unwrap_or(default)
    }

    pub fn get_strict(&self, key: &str) -> ApiResult {
        self.db.get_item_sync(&self.namespace, Some(&self.key(key)), None)
    }

    pub fn delete(&self, key: &str) -> ApiResult {
        self.db.delete_item_sync(&self.namespace, &self.key(key))
    }

    pub fn move_batch(&self, source: &[String], dest: &[String]) -> Result<(), ServerError> {
        self.db.move_batch_sync(&self.namespace, source, dest)
    }

    pub fn delete_batch(&self, keys: &[String]) -> Result<Map<String, Value>, ServerError> {
        self.db.delete_batch_sync(&self.namespace, keys)
    }

    pub fn as_dict(&self) -> Map<String, Value> {
        self.db.get_namespace(&self.namespace, false).unwrap_or_default()
    }

    pub fn keys(&self) -> Vec<String> {
        self.db.get_namespace_keys(&self.namespace).unwrap_or_default()
    }

    pub fn values(&self) -> Vec<Value> {
        self.db.get_namespace_values(&self.namespace).unwrap_or_default()
    }

    pub fn items(&self) -> Vec<(String, Value)> {
        self.db.get_namespace_items(&self.namespace).unwrap_or_default()
    }

    pub fn pop(&self, key: &str, default: Option<Value>) -> ApiResult {
        match self.delete(key) {
            Ok(v) => Ok(v),
            Err(e) => match default {
                Some(d) => Ok(d),
                None => Err(e),
            },
        }
    }

    pub fn clear(&self) -> Result<(), ServerError> {
        self.db.clear_namespace(&self.namespace)
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<MoonrakerDatabase>, crate::confighelper::ConfigError> {
    MoonrakerDatabase::new(config)
}

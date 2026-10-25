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

mod api;
mod encoding;
mod endpoints;
mod namespace;
mod records;

pub use encoding::*;
pub use namespace::*;

// ***** Table definitions *****

pub trait SqlTableDefinition {
    fn name(&self) -> &str;
    fn version(&self) -> i64;
    fn prototype(&self) -> String;
    fn migrate(&self, last_version: i64, db: &MoonrakerDatabase) -> Result<(), ServerError>;
}

pub use crate::pycompat::inspect::cleandoc;

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
        self.with_commit(|c| c.execute(&format!("CREATE TABLE {}", proto), []))?;
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
        self.with_commit(|c| c.execute(&format!("CREATE TABLE {}", proto), []))?;
        self.tables.borrow_mut().insert(REGISTRATION_TABLE.into());
        Ok(())
    }

    fn save_registered_table(&self, name: &str, prototype: &str, version: i64) -> Result<(), ServerError> {
        self.with_commit(|c| {
            c.execute(
                &format!(
                    "INSERT INTO {} VALUES(?, ?, ?) ON CONFLICT(name) DO UPDATE SET prototype=excluded.prototype, version=excluded.version",
                    REGISTRATION_TABLE
                ),
                params![name, prototype, version],
            )
        })?;
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

    /// The connection, for Python components' SQL (plugins/sql.py)
    pub fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
        f(&self.conn.borrow())
    }

    /// Python's `with conn:` around statements, as Moonraker's provider
    /// writes: one transaction, committed at the end, rolled back on an
    /// error. A transaction a Python component left open is part of it
    fn with_commit<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> rusqlite::Result<T> {
        let conn = self.conn.borrow();
        if conn.is_autocommit() {
            conn.execute_batch("BEGIN")?;
        }
        match f(&conn) {
            Ok(v) => {
                conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    pub fn get_database_path(&self) -> String {
        self.sql_db.to_string_lossy().to_string()
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<MoonrakerDatabase>, crate::confighelper::ConfigError> {
    MoonrakerDatabase::new(config)
}

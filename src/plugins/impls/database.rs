// moonraker.components.database.MoonrakerDatabase for Python components, on
// Moonruster's database. Moonraker runs each operation on its SQLite thread
// and returns an asyncio Future; before that thread starts (component_init)
// it runs the operation in place, so an error raises from the call. Both are
// kept: in place before, a future completed on a later loop iteration after.
// NamespaceWrapper is Moonraker's own class (mirror_verbatim.txt).
use super::*;
use crate::common::ServerError;
use crate::components::database::{MoonrakerDatabase, SqlTableDefinition};
use crate::plugins::futures::to_exception;
use crate::server::server;

macro_rules! db {
    ($m:literal) => {
        concat!("moonraker.components.database.MoonrakerDatabase.", $m)
    };
}

macro_rules! provider {
    ($m:literal) => {
        concat!("moonraker.components.database.SqliteProvider.", $m)
    };
}

/// MoonrakerDatabase.db_provider: an object of its own (identity), the
/// database behind it
pub struct DbProvider;

fn this(a: &Args) -> PyResult<Rc<MoonrakerDatabase>> {
    a.this::<MoonrakerDatabase>()
}

fn db_err(e: ServerError) -> PyError {
    server_error(&e.message, e.status_code)
}

/// A value to store: encode_record takes int, float, bool, str, list, dict
/// and None (by exact type), their contents as JSON
fn record(v: &PyRef, top_level: bool) -> PyResult<Value> {
    let fail = || -> PyError {
        let t = v.getattr("__class__").and_then(|c| c.repr()).unwrap_or_default();
        server_error(&format!("Error encoding val: {}, type: {}", v.str().unwrap_or_default(), t), 400)
    };
    if top_level {
        let t = v.getattr("__class__")?;
        let ok = ["int", "float", "bool", "str", "list", "dict"]
            .iter()
            .any(|n| lookup("builtins", n).map(|c| c.as_ptr() == t.as_ptr()).unwrap_or(false))
            || v.is_none();
        if !ok {
            return Err(fail());
        }
    }
    from_py(v).map_err(|_| fail())
}

fn str_list(v: &PyRef) -> PyResult<Vec<String>> {
    match from_py(v)? {
        Value::Array(a) => Ok(a.iter().map(crate::pyfmt::py_str).collect()),
        other => Err(py_error("TypeError", format!("expected a list, got {}", crate::pyfmt::py_type_name(&other)))),
    }
}

/// The operation's result as Moonraker's execute_db_function returns it
fn execute(db: &MoonrakerDatabase, f: impl FnOnce() -> PyResult<PyRef>) -> PyResult<PyRef> {
    let l = crate::plugins::the_loop();
    let fut = l.loop_.call_method("create_future", &[])?;
    if !db.is_alive() {
        fut.call_method("set_result", &[&f()?])?;
        return Ok(fut);
    }
    let (method, value) = match f() {
        Ok(v) => ("set_result", v),
        Err(e) => ("set_exception", to_exception(&e)?),
    };
    // the SQLite thread's loop.call_soon_threadsafe(future.set_result, ret)
    l.loop_.call_method("call_soon", &[&fut.getattr(method)?, &value])?;
    l.wake.notify_one();
    Ok(fut)
}

/// An operation on the database: its arguments are read in it too (an
/// encoding error is the operation's, as encode_record's is there)
fn op(a: &Args, f: impl FnOnce(&MoonrakerDatabase) -> PyResult<Value>) -> PyResult<PyRef> {
    let db = this(a)?;
    execute(&db, || to_py(&f(&db)?))
}

fn records(v: &PyRef, name: &str) -> PyResult<serde_json::Map<String, Value>> {
    match record(v, true)? {
        Value::Object(m) => Ok(m),
        _ => Err(py_error("AttributeError", format!("'{}' has no attribute 'items'", name))),
    }
}

/// A cursor whose description is (key, value), for sqlite3.Row(cursor, data)
fn row_cursor() -> PyResult<PyRef> {
    thread_local! {
        static CURSOR: std::cell::RefCell<Option<PyRef>> = const { std::cell::RefCell::new(None) };
    }
    if let Some(c) = CURSOR.with(|c| c.borrow().clone()) {
        return Ok(c);
    }
    let conn = lookup("sqlite3", "connect")?.call(&[&py_str(":memory:")])?;
    let c = conn.call_method("execute", &[&py_str("SELECT NULL AS key, NULL AS value")])?;
    CURSOR.with(|x| *x.borrow_mut() = Some(c.clone()));
    Ok(c)
}

fn py_set(items: impl IntoIterator<Item = String>) -> PyResult<PyRef> {
    let set = lookup("builtins", "set")?.call(&[])?;
    for i in items {
        set.call_method("add", &[&py_str(&i)])?;
    }
    Ok(set)
}

fn namespace_wrapper(a: &Args, namespace: &str, parse_keys: bool) -> PyResult<PyRef> {
    let slf = a.slf.as_ref().expect("method without self");
    lookup("moonraker.components.database", "NamespaceWrapper")?.call(&[&py_str(namespace), slf, &py_bool(parse_keys)])
}

/// A Python component's SqlTableDefinition: its migrate() gets a
/// DBProviderWrapper; its exception is raised from register_table
struct PyTableDef {
    obj: PyRef,
    name: String,
    version: i64,
    prototype: String,
    error: std::cell::RefCell<Option<PyError>>,
}

impl SqlTableDefinition for PyTableDef {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> i64 {
        self.version
    }
    fn prototype(&self) -> String {
        self.prototype.clone()
    }
    fn migrate(&self, last_version: i64, _db: &MoonrakerDatabase) -> Result<(), ServerError> {
        let r = provider_object().and_then(|p| {
            let wrapper = lookup("moonraker.components.database", "DBProviderWrapper")?.call(&[&p])?;
            self.obj.call_method("migrate", &[&py_int(last_version), &wrapper])
        });
        r.map(|_| ()).map_err(|e| {
            let msg = e.message.clone();
            *self.error.borrow_mut() = Some(e);
            ServerError::exc(msg)
        })
    }
}

fn provider_object() -> PyResult<PyRef> {
    thread_local! {
        static PROVIDER: Rc<DbProvider> = Rc::new(DbProvider);
    }
    let p: Rc<dyn std::any::Any> = PROVIDER.with(|p| p.clone());
    objects::wrap_class(p, "moonraker.components.database", "SqliteProvider")
}

fn sync_conn() -> PyResult<PyRef> {
    super::sql::sql_module()?.getattr("sync_connection")
}

pub fn register(r: &mut Registry) {
    r.method(db!("insert_item"), |a| {
        let ns = arg(a, 0, "namespace")?.as_string()?;
        op(a, |db| {
            let key = from_py(arg(a, 1, "key")?)?;
            // a nested key stores the value inside a dict record
            let nested = matches!(key, Value::Array(ref k) if k.len() > 1) || key.as_str().is_some_and(|k| k.contains('.'));
            let value = record(arg(a, 2, "value")?, !nested)?;
            db.insert_item_sync(&ns, &key, value).map_err(db_err)?;
            Ok(Value::Null)
        })
    });
    r.method(db!("update_item"), |a| {
        let ns = arg(a, 0, "namespace")?.as_string()?;
        op(a, |db| {
            let key = from_py(arg(a, 1, "key")?)?;
            let value = record(arg(a, 2, "value")?, false)?;
            db.update_item_sync(&ns, &key, value).map_err(db_err)?;
            Ok(Value::Null)
        })
    });
    r.method(db!("delete_item"), |a| {
        let ns = arg(a, 0, "namespace")?.as_string()?;
        op(a, |db| db.delete_item_sync(&ns, &from_py(arg(a, 1, "key")?)?).map_err(db_err))
    });
    r.method(db!("get_item"), |a| {
        let ns = arg(a, 0, "namespace")?.as_string()?;
        let key = match a.get(1, "key") {
            Some(k) if !k.is_none() => Some(from_py(k)?),
            _ => None,
        };
        // the default is returned as given (the same object), not converted
        let default = match a.get(2, "default") {
            Some(d) if !is_missing(d)? => Some(d.clone()),
            _ => None,
        };
        let db = this(a)?;
        execute(&db, || match db.get_item_sync(&ns, key.as_ref(), None) {
            Ok(v) => to_py(&v),
            Err(e) => match &default {
                Some(d) => Ok(d.clone()),
                None => Err(db_err(e)),
            },
        })
    });
    r.method(db!("insert_batch"), |a| {
        let ns = arg(a, 0, "namespace")?.as_string()?;
        op(a, |db| {
            db.insert_batch_sync(&ns, &records(arg(a, 1, "records")?, "records")?).map_err(db_err)?;
            Ok(Value::Null)
        })
    });
    r.method(db!("update_namespace"), |a| {
        let ns = arg(a, 0, "namespace")?.as_string()?;
        op(a, |db| {
            db.insert_batch_sync(&ns, &records(arg(a, 1, "values")?, "values")?).map_err(db_err)?;
            Ok(Value::Null)
        })
    });
    r.method(db!("sync_namespace"), |a| {
        let ns = arg(a, 0, "namespace")?.as_string()?;
        op(a, |db| {
            db.sync_namespace_sync(&ns, &records(arg(a, 1, "values")?, "values")?).map_err(db_err)?;
            Ok(Value::Null)
        })
    });
    r.method(db!("move_batch"), |a| {
        let ns = arg(a, 0, "namespace")?.as_string()?;
        op(a, |db| {
            db.move_batch_sync(&ns, &str_list(arg(a, 1, "source_keys")?)?, &str_list(arg(a, 2, "dest_keys")?)?).map_err(db_err)?;
            Ok(Value::Null)
        })
    });
    r.method(db!("delete_batch"), |a| {
        let ns = arg(a, 0, "namespace")?.as_string()?;
        op(a, |db| db.delete_batch_sync(&ns, &str_list(arg(a, 1, "keys")?)?).map(Value::Object).map_err(db_err))
    });
    r.method(db!("get_batch"), |a| {
        let ns = arg(a, 0, "namespace")?.as_string()?;
        op(a, |db| db.get_batch_sync(&ns, &str_list(arg(a, 1, "keys")?)?).map(Value::Object).map_err(db_err))
    });
    r.method(db!("clear_namespace"), |a| {
        let ns = arg(a, 0, "namespace")?.as_string()?;
        op(a, |db| db.clear_namespace(&ns).map(|_| Value::Null).map_err(db_err))
    });
    r.method(db!("drop_empty_namespace"), |a| {
        let ns = arg(a, 0, "namespace")?.as_string()?;
        op(a, |db| db.drop_empty_namespace(&ns).map(|_| Value::Null).map_err(db_err))
    });
    r.method(db!("ns_length"), |a| {
        let ns = arg(a, 0, "namespace")?.as_string()?;
        op(a, |db| db.get_namespace_length(&ns).map(Value::from).map_err(db_err))
    });
    r.method(db!("ns_keys"), |a| {
        let ns = arg(a, 0, "namespace")?.as_string()?;
        op(a, |db| db.get_namespace_keys(&ns).map(Value::from).map_err(db_err))
    });
    r.method(db!("ns_values"), |a| {
        let ns = arg(a, 0, "namespace")?.as_string()?;
        op(a, |db| db.get_namespace_values(&ns).map(Value::from).map_err(db_err))
    });
    r.method(db!("ns_items"), |a| {
        let ns = arg(a, 0, "namespace")?.as_string()?;
        let db = this(a)?;
        // cursor.fetchall() of "SELECT key, value": sqlite3.Row objects
        execute(&db, || {
            let items = db.get_namespace_items(&ns).map_err(db_err)?;
            let list = lookup("builtins", "list")?.call(&[])?;
            if items.is_empty() {
                return Ok(list);
            }
            let row = lookup("sqlite3", "Row")?;
            let cursor = row_cursor()?;
            for (k, v) in items {
                list.call_method("append", &[&row.call(&[&cursor, &tuple(&[&py_str(&k), &to_py(&v)?])])?])?;
            }
            Ok(list)
        })
    });
    r.method(db!("ns_contains"), |a| {
        // Moonraker passes the provider's namespace_contains() no key, so
        // the call itself fails (database.py ns_contains, 9e676eba)
        let db = this(a)?;
        execute(&db, || {
            Err(py_error("TypeError", "SqliteProvider.namespace_contains() missing 1 required positional argument: 'key'".into()))
        })
    });
    r.method(db!("get_database_path"), |a| Ok(py_str(&this(a)?.get_database_path())));
    r.method(db!("database_path"), |a| lookup("pathlib", "Path")?.call(&[&py_str(&this(a)?.get_database_path())]));
    r.attr(db!("_sql_db"), |o| lookup("pathlib", "Path")?.call(&[&py_str(&objects::unwrap_as::<MoonrakerDatabase>(o)?.get_database_path())]), None);
    r.method(db!("get_backup_dir"), |a| {
        lookup("pathlib", "Path")?.call(&[&py_str(&this(a)?.get_backup_dir().to_string_lossy())])
    });
    r.method(db!("register_local_namespace"), |a| {
        let ns = arg(a, 0, "namespace")?.as_string()?;
        let forbidden = bool_or(a, 1, "forbidden", false)?;
        let parse_keys = bool_or(a, 2, "parse_keys", false)?;
        this(a)?.register_local_namespace(&ns, forbidden, parse_keys).map_err(db_err)?;
        namespace_wrapper(a, &ns, parse_keys)
    });
    r.method(db!("wrap_namespace"), |a| {
        let ns = arg(a, 0, "namespace")?.as_string()?;
        let parse_keys = bool_or(a, 1, "parse_keys", true)?;
        this(a)?.wrap_namespace(&ns, parse_keys).map_err(db_err)?;
        namespace_wrapper(a, &ns, parse_keys)
    });
    r.method(db!("unregister_local_namespace"), |a| {
        this(a)?.unregister_local_namespace(&arg(a, 0, "namespace")?.as_string()?);
        Ok(none())
    });
    r.attr(db!("server"), |_| objects::wrap(server()), None);
    r.attr(db!("eventloop"), |_| super::server::event_loop(), None);
    r.attr(db!("db_provider"), |_| provider_object(), None);
    // register_table(table_def) -> SqlTableWrapper
    r.method(db!("register_table"), |a| {
        let obj = arg(a, 0, "table_def")?.clone();
        let def = PyTableDef {
            name: obj.getattr("name")?.as_string()?,
            version: obj.getattr("version")?.as_i64()?,
            prototype: obj.getattr("prototype")?.as_string()?,
            obj: obj.clone(),
            error: Default::default(),
        };
        if let Err(e) = this(a)?.register_table(&def) {
            return Err(def.error.borrow_mut().take().unwrap_or_else(|| db_err(e)));
        }
        let slf = a.slf.as_ref().expect("method without self");
        lookup("moonraker.components.database", "SqlTableWrapper")?.call(&[slf, &obj])
    });
    // SqliteProvider: its connection is Moonruster's (plugins/sql.py), its
    // functions run on the event loop (execute() above). The methods that
    // change the namespace set are Moonruster's; the others are Moonraker's
    // code over the connection (mirror_verbatim.txt)
    r.attr(provider!("sync_conn"), |_| sync_conn(), None);
    r.attr(provider!("server"), |_| objects::wrap(server()), None);
    r.method(provider!("execute_db_function"), |a| {
        let func = a.args.first().ok_or_else(|| py_error("TypeError", "missing argument 'command_func'".into()))?.clone();
        // the thread's connection, or sync_conn before the thread starts
        let conn = if server().database().is_alive() { super::sql::sql_module()?.getattr("connection")? } else { sync_conn()? };
        let mut args = vec![conn];
        args.extend(a.args.iter().skip(1).cloned());
        execute(&server().database(), || func.call(&args.iter().collect::<Vec<_>>()))
    });
    r.method(provider!("insert_item"), |a| {
        let ns = arg(a, 1, "namespace")?.as_string()?;
        let key = from_py(arg(a, 2, "key")?)?;
        let nested = matches!(key, Value::Array(ref k) if k.len() > 1) || key.as_str().is_some_and(|k| k.contains('.'));
        let value = record(arg(a, 3, "value")?, !nested)?;
        server().database().insert_item_sync(&ns, &key, value).map_err(db_err)?;
        Ok(none())
    });
    r.method(provider!("insert_batch"), |a| {
        let ns = arg(a, 1, "namespace")?.as_string()?;
        server().database().insert_batch_sync(&ns, &records(arg(a, 2, "records")?, "records")?).map_err(db_err)?;
        Ok(none())
    });
    r.method(provider!("drop_empty_namespace"), |a| {
        server().database().drop_empty_namespace(&arg(a, 1, "namespace")?.as_string()?).map_err(db_err)?;
        Ok(none())
    });
    r.method(provider!("compact_database"), |_| to_py(&server().database().compact_database().map_err(db_err)?));
    r.method(provider!("backup_database"), |a| {
        let p = std::path::PathBuf::from(arg(a, 1, "bkp_path")?.str()?);
        server().database().backup_database(&p).map_err(db_err)?;
        Ok(none())
    });
    r.method(provider!("restore_database"), |a| {
        let p = std::path::PathBuf::from(arg(a, 1, "restore_path")?.str()?);
        to_py(&server().database().restore_database(&p).map_err(db_err)?)
    });
    r.attr(db!("registered_namespaces"), |o| py_set(objects::unwrap_as::<MoonrakerDatabase>(o)?.registered_namespaces()), None);
    r.attr(db!("protected_namespaces"), |o| py_set(objects::unwrap_as::<MoonrakerDatabase>(o)?.protected_namespaces()), None);
    r.attr(db!("forbidden_namespaces"), |o| py_set(objects::unwrap_as::<MoonrakerDatabase>(o)?.forbidden_namespaces()), None);
    r.method(provider!("is_alive"), |_| Ok(py_bool(server().database().is_alive())));
    r.method(provider!("namespaces"), |_| py_set(server().database().namespaces()));
    r.method(provider!("tables"), |_| py_set(server().database().tables()));
    r.attr(provider!("_namespaces"), |_| py_set(server().database().namespaces()), None);
    r.attr(provider!("_tables"), |_| py_set(server().database().tables()), None);
}

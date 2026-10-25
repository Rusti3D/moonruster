// The SQL of Python components on Moonruster's one SQLite connection
// (docs/PLUGINS.md "SQL"): the module `_mr_sql` (sql.py) and its natives
use super::*;
use crate::server::server;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{ffi, Connection};
use std::ffi::{CStr, CString};

thread_local! {
    static MODULE: std::cell::RefCell<Option<PyRef>> = const { std::cell::RefCell::new(None) };
}

/// `_mr_sql`, created on first use
pub fn sql_module() -> PyResult<PyRef> {
    if let Some(m) = MODULE.with(|m| m.borrow().clone()) {
        return Ok(m);
    }
    let m = exec_module("_mr_sql", include_str!("../sql.py"))?;
    let natives: [(&str, crate::plugins::native::Method); 6] = [
        ("_alive", |_| Ok(py_bool(server().database().is_alive()))),
        ("_prepare", prepare),
        ("_exec", exec),
        ("_script", script),
        ("_autocommit", |_| Ok(py_bool(with_conn(|c| c.is_autocommit())))),
        // SAFETY: the handle is the open connection, valid while with_conn borrows it
        ("_total_changes", |_| Ok(py_int(with_conn(|c| unsafe { ffi::sqlite3_total_changes(c.handle()) }) as i64))),
    ];
    for (name, f) in natives {
        m.setattr(name, &crate::plugins::native::bound_method(&none(), f)?)?;
    }
    MODULE.with(|c| *c.borrow_mut() = Some(m.clone()));
    Ok(m)
}

fn with_conn<T>(f: impl FnOnce(&Connection) -> T) -> T {
    server().database().with_conn(f)
}

/// the sqlite3 exception of an SQLite error (sql.py _error)
fn sqlite_error(code: i32, message: &str) -> PyError {
    match sql_module().and_then(|m| m.getattr("_error")?.call(&[&py_int(code as i64), &py_str(message)])) {
        Ok(exc) => PyError { type_name: "DatabaseError".into(), message: message.into(), traceback: String::new(), value: Some(exc) },
        Err(e) => e,
    }
}

fn programming_error(message: &str) -> PyError {
    match lookup("sqlite3", "ProgrammingError").and_then(|c| c.call(&[&py_str(message)])) {
        Ok(exc) => PyError { type_name: "ProgrammingError".into(), message: message.into(), traceback: String::new(), value: Some(exc) },
        Err(e) => e,
    }
}

fn db_error(e: rusqlite::Error) -> PyError {
    match e {
        rusqlite::Error::SqliteFailure(f, msg) => {
            // SAFETY: sqlite3_errstr returns a static NUL-terminated string for any code
            let msg = msg.unwrap_or_else(|| unsafe { CStr::from_ptr(ffi::sqlite3_errstr(f.extended_code)) }.to_string_lossy().into_owned());
            sqlite_error(f.extended_code, &msg)
        }
        other => match lookup("sqlite3", "DatabaseError").and_then(|c| c.call(&[&py_str(&other.to_string())])) {
            Ok(exc) => PyError { type_name: "DatabaseError".into(), message: other.to_string(), traceback: String::new(), value: Some(exc) },
            Err(e) => e,
        },
    }
}

fn sql_arg(a: &Args) -> PyResult<CString> {
    CString::new(arg(a, 0, "sql")?.as_string()?).map_err(|_| programming_error("the query contains a null character"))
}

/// CPython's lstrip_sql: whether anything but blanks and comments is left
fn has_statement(mut s: &[u8]) -> bool {
    loop {
        match s {
            [c, rest @ ..] if b" \t\x0c\n\r".contains(c) => s = rest,
            [b'-', b'-', rest @ ..] => match rest.iter().position(|&c| c == b'\n') {
                Some(i) => s = &rest[i + 1..],
                None => return false,
            },
            [b'/', b'*', rest @ ..] => match rest.windows(2).position(|w| w == b"*/") {
                Some(i) => s = &rest[i + 2..],
                None => return false,
            },
            [] => return false,
            _ => return true,
        }
    }
}

/// _prepare(sql) -> (parameter names (None: nameless), readonly): checked
/// as sqlite3 checks a statement before binding
fn prepare(a: &Args) -> PyResult<PyRef> {
    let sql = sql_arg(a)?;
    // SAFETY: db is the open connection, valid while with_conn borrows it; sql is a NUL-terminated CString that outlives stmt and tail (which points into it); stmt is finalized exactly once below, NULL included (a no-op)
    with_conn(|conn| unsafe {
        let db = conn.handle();
        let mut stmt = std::ptr::null_mut();
        let mut tail = std::ptr::null();
        let rc = ffi::sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, &mut tail);
        if rc != ffi::SQLITE_OK {
            let msg = CStr::from_ptr(ffi::sqlite3_errmsg(db)).to_string_lossy().into_owned();
            return Err(sqlite_error(ffi::sqlite3_extended_errcode(db), &msg));
        }
        let rest = if tail.is_null() { &[][..] } else { CStr::from_ptr(tail).to_bytes() };
        let result = (|| {
            if has_statement(rest) {
                return Err(programming_error("You can only execute one statement at a time."));
            }
            let names = lookup("builtins", "list")?.call(&[])?;
            let mut readonly = true;
            if !stmt.is_null() {
                for i in 1..=ffi::sqlite3_bind_parameter_count(stmt) {
                    let n = ffi::sqlite3_bind_parameter_name(stmt, i);
                    let v = if n.is_null() { none() } else { py_str(&CStr::from_ptr(n).to_string_lossy()) };
                    names.call_method("append", &[&v])?;
                }
                readonly = ffi::sqlite3_stmt_readonly(stmt) != 0;
            }
            Ok(tuple(&[&names, &py_bool(readonly)]))
        })();
        ffi::sqlite3_finalize(stmt);
        result
    })
}

/// a value sqlite3 bound (None, int, float, str, bytes)
fn sql_value(v: &PyRef) -> PyResult<SqlValue> {
    if v.is_none() {
        return Ok(SqlValue::Null);
    }
    Ok(match v.getattr("__class__")?.getattr("__name__")?.as_string()?.as_str() {
        "int" => SqlValue::Integer(v.as_i64()?),
        "float" => SqlValue::Real(v.as_f64()?),
        "str" => SqlValue::Text(v.as_string()?),
        _ => SqlValue::Blob(match from_py(&lookup("builtins", "list")?.call(&[v])?)? {
            Value::Array(a) => a.iter().map(|b| b.as_u64().unwrap_or(0) as u8).collect(),
            _ => Vec::new(),
        }),
    })
}

fn py_value(v: ValueRef) -> PyResult<PyRef> {
    Ok(match v {
        ValueRef::Null => none(),
        ValueRef::Integer(i) => py_int(i),
        ValueRef::Real(f) => py_float(f),
        ValueRef::Text(t) => match std::str::from_utf8(t) {
            Ok(s) => py_str(s),
            // not UTF-8: the converter's bytes, or sqlite3's decode error
            Err(_) => tuple(&[&py_bytes(t)?]),
        },
        ValueRef::Blob(b) => py_bytes(b)?,
    })
}

/// _exec(sql, values) -> (columns [(name, declared type)], rows, changes,
/// lastrowid)
fn exec(a: &Args) -> PyResult<PyRef> {
    let sql = arg(a, 0, "sql")?.as_string()?;
    let values: Vec<SqlValue> = {
        let list = arg(a, 1, "values")?;
        let n = list.call_method("__len__", &[])?.as_i64()?;
        (0..n).map(|i| sql_value(&list.call_method("__getitem__", &[&py_int(i)])?)).collect::<PyResult<_>>()?
    };
    with_conn(|conn| {
        let mut stmt = conn.prepare_cached(&sql).map_err(db_error)?;
        let cols = lookup("builtins", "list")?.call(&[])?;
        for c in stmt.columns() {
            let decl = c.decl_type().map(py_str).unwrap_or_else(none);
            cols.call_method("append", &[&tuple(&[&py_str(c.name()), &decl])])?;
        }
        let ncols = stmt.column_count();
        let rows = lookup("builtins", "list")?.call(&[])?;
        let mut q = stmt.query(rusqlite::params_from_iter(values.iter())).map_err(db_error)?;
        while let Some(row) = q.next().map_err(db_error)? {
            let vals = (0..ncols).map(|i| py_value(row.get_ref(i).map_err(db_error)?)).collect::<PyResult<Vec<_>>>()?;
            rows.call_method("append", &[&tuple(&vals.iter().collect::<Vec<_>>())])?;
        }
        drop(q);
        Ok(tuple(&[&cols, &rows, &py_int(conn.changes() as i64), &py_int(conn.last_insert_rowid())]))
    })
}

fn script(a: &Args) -> PyResult<PyRef> {
    let sql = arg(a, 0, "sql")?.as_string()?;
    with_conn(|conn| conn.execute_batch(&sql)).map_err(db_error)?;
    Ok(none())
}

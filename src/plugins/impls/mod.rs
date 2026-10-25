// What each mirrored Moonraker method does in Moonruster, by Moonraker
// qualified name (native.rs Registry). Grown component by component, each
// area with a parity case against Python Moonraker (docs/PLUGINS.md).
pub mod analysis;
pub mod announcements;
pub mod application;
pub mod authorization;
pub mod config;
pub mod database;
pub mod file_manager;
pub mod gpio;
pub mod history;
pub mod http_client;
pub mod klippy_apis;
pub mod klippy_connection;
pub mod machine;
pub mod mqtt;
pub mod server;
pub mod services;
pub mod shell_command;
pub mod source_info;
pub mod sql;
pub mod template;
pub mod transport;
pub mod update_manager;
pub mod webcam;
pub mod websockets;

pub(crate) use super::convert::{from_py, to_py};
pub(crate) use super::native::{py_error, Args, Registry};
pub(crate) use super::objects;
pub(crate) use super::py::*;
pub(crate) use serde_json::Value;
pub(crate) use std::rc::Rc;

pub fn register(r: &mut Registry) {
    analysis::register(r);
    announcements::register(r);
    application::register(r);
    authorization::register(r);
    config::register(r);
    database::register(r);
    file_manager::register(r);
    gpio::register(r);
    history::register(r);
    http_client::register(r);
    klippy_apis::register(r);
    klippy_connection::register(r);
    machine::register(r);
    mqtt::register(r);
    server::register(r);
    services::register(r);
    shell_command::register(r);
    source_info::register(r);
    template::register(r);
    update_manager::register(r);
    webcam::register(r);
    websockets::register(r);
}

#[cfg(test)]
mod tests {
    /// every handler names a method, attribute or constructor the mirror
    /// has (a typo would leave it NotImplementedError at run time)
    #[test]
    fn handlers_name_mirrored_ids() {
        let mut r = super::Registry::default();
        super::register(&mut r);
        let ids: std::collections::HashSet<&str> = crate::plugins::mirror_ids::IDS.iter().copied().collect();
        let unknown: Vec<&str> = r.names().filter(|n| !ids.contains(n)).collect();
        assert!(unknown.is_empty(), "handlers for names the mirror does not have: {:?}", unknown);
    }
}

/// Whether `v` is moonraker.utils.Sentinel.MISSING (a missing default)
pub fn is_missing(v: &PyRef) -> PyResult<bool> {
    let missing = lookup("moonraker.utils", "Sentinel")?.getattr("MISSING")?;
    Ok(v.as_ptr() == missing.as_ptr())
}

/// A ServerError raised as Moonraker's moonraker.utils.exceptions.ServerError
/// A required argument (positional index or keyword name)
pub fn arg<'a>(a: &'a Args, i: usize, n: &str) -> PyResult<&'a PyRef> {
    a.get(i, n).ok_or_else(|| py_error("TypeError", format!("missing argument '{}'", n)))
}

/// An optional argument; None counts as absent
pub fn opt<'a>(a: &'a Args, i: usize, n: &str) -> Option<&'a PyRef> {
    a.get(i, n).filter(|v| !v.is_none())
}

pub fn f64_or(a: &Args, i: usize, n: &str, d: f64) -> PyResult<f64> {
    opt(a, i, n).map(|v| v.as_f64()).unwrap_or(Ok(d))
}

/// A flag argument, by Python truth (None is false)
pub fn bool_or(a: &Args, i: usize, n: &str, d: bool) -> PyResult<bool> {
    a.get(i, n).map(|v| v.is_true()).unwrap_or(Ok(d))
}

/// The arguments given, positional or keyword, by their parameter names
pub fn named_args(a: &Args, names: &[&str]) -> PyResult<serde_json::Map<String, Value>> {
    let mut m = serde_json::Map::new();
    for (i, n) in names.iter().enumerate() {
        if let Some(v) = a.get(i, n) {
            m.insert(n.to_string(), from_py(v)?);
        }
    }
    Ok(m)
}

pub fn str_or(a: &Args, i: usize, n: &str, default: &str) -> PyResult<String> {
    match a.get(i, n) {
        Some(v) => v.as_string(),
        None => Ok(default.to_string()),
    }
}

pub fn server_error(message: &str, status_code: i64) -> PyError {
    match lookup("moonraker.utils.exceptions", "ServerError").and_then(|c| c.call(&[&py_str(message), &py_int(status_code)])) {
        Ok(exc) => PyError { type_name: "ServerError".into(), message: message.into(), traceback: String::new(), value: Some(exc) },
        Err(err) => err,
    }
}

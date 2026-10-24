// What each mirrored Moonraker method does in Moonruster, by Moonraker
// qualified name (native.rs Registry). Grown component by component, each
// area with a parity case against Python Moonraker (docs/PLUGINS.md).
pub mod config;
pub mod server;

pub(crate) use super::convert::{from_py, to_py};
pub(crate) use super::native::{py_error, Args, Registry};
pub(crate) use super::objects;
pub(crate) use super::py::*;
pub(crate) use serde_json::Value;
pub(crate) use std::rc::Rc;

pub fn register(r: &mut Registry) {
    config::register(r);
    server::register(r);
}

/// Whether `v` is moonraker.utils.Sentinel.MISSING (a missing default)
pub fn is_missing(v: &PyRef) -> PyResult<bool> {
    let missing = lookup("moonraker.utils", "Sentinel")?.getattr("MISSING")?;
    Ok(v.as_ptr() == missing.as_ptr())
}

/// A ServerError raised as Moonraker's moonraker.utils.exceptions.ServerError
pub fn server_error(message: &str, status_code: i64) -> PyError {
    match lookup("moonraker.utils.exceptions", "ServerError").and_then(|c| c.call(&[&py_str(message), &py_int(status_code)])) {
        Ok(exc) => PyError { type_name: "ServerError".into(), message: message.into(), traceback: String::new(), value: Some(exc) },
        Err(err) => err,
    }
}

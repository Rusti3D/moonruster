// What each mirrored Moonraker method does in Moonruster, by Moonraker
// qualified name (native.rs Registry). Grown component by component, each
// area with a parity case against Python Moonraker (docs/PLUGINS.md).
pub mod announcements;
pub mod application;
pub mod config;
pub mod database;
pub mod file_manager;
pub mod history;
pub mod http_client;
pub mod klippy_apis;
pub mod klippy_connection;
pub mod mqtt;
pub mod server;
pub mod services;
pub mod shell_command;
pub mod source_info;
pub mod template;
pub mod webcam;
pub mod websockets;

pub(crate) use super::convert::{from_py, to_py};
pub(crate) use super::native::{py_error, Args, Registry};
pub(crate) use super::objects;
pub(crate) use super::py::*;
pub(crate) use serde_json::Value;
pub(crate) use std::rc::Rc;

pub fn register(r: &mut Registry) {
    announcements::register(r);
    application::register(r);
    config::register(r);
    database::register(r);
    file_manager::register(r);
    history::register(r);
    http_client::register(r);
    klippy_apis::register(r);
    klippy_connection::register(r);
    mqtt::register(r);
    server::register(r);
    services::register(r);
    shell_command::register(r);
    source_info::register(r);
    template::register(r);
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
pub fn server_error(message: &str, status_code: i64) -> PyError {
    match lookup("moonraker.utils.exceptions", "ServerError").and_then(|c| c.call(&[&py_str(message), &py_int(status_code)])) {
        Ok(exc) => PyError { type_name: "ServerError".into(), message: message.into(), traceback: String::new(), value: Some(exc) },
        Err(err) => err,
    }
}

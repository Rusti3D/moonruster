// APITransport objects from Python components: Moonruster's own transports
// (a websocket connection, MQTT, KlippyAPI ...) and a component's own
// APITransport subclass, which Moonruster calls back (send_status ...)
use super::*;
use crate::common::{ApiTransport, TransportType, UserInfo};
use crate::components::application::{HttpRpcTransport, InternalTransport};
use crate::components::klippy_apis::KlippyApi;
use crate::components::mqtt::MqttClient;
use crate::components::simplyprint::SimplyPrint;
use crate::components::websockets::RemoteConnection;
use serde_json::Map;
use std::any::Any;

/// A Python component's APITransport
pub struct PyTransport {
    obj: PyRef,
    id: u64,
}

impl PyTransport {
    fn attr(&self, name: &str) -> Option<PyRef> {
        match self.obj.getattr(name) {
            Ok(v) if !v.is_none() => Some(v),
            Ok(_) => None,
            Err(e) => {
                log::error!("moonruster: transport.{}\n{}", name, e.traceback);
                None
            }
        }
    }
}

impl ApiTransport for PyTransport {
    fn transport_type(&self) -> TransportType {
        self.attr("transport_type")
            .and_then(|t| t.getattr("value").and_then(|v| v.as_i64()).ok())
            .map(|v| TransportType(v as u32))
            .unwrap_or(TransportType::INTERNAL)
    }
    fn user_info(&self) -> Option<UserInfo> {
        self.attr("user_info").and_then(|u| super::application::user_info_from_py(&u).ok())
    }
    fn ip_addr(&self) -> Option<std::net::IpAddr> {
        self.attr("ip_addr").and_then(|ip| ip.str().ok()?.parse().ok())
    }
    fn send_status(&self, status: &Map<String, Value>, eventtime: f64) {
        let r = to_py(&Value::Object(status.clone())).and_then(|st| self.obj.call_method("send_status", &[&st, &py_float(eventtime)]));
        if let Err(e) = r {
            log::error!("moonruster: transport send_status\n{}", e.traceback);
        }
    }
    fn transport_id(&self) -> u64 {
        self.id
    }
    // the component's own object (bound to this transport)
    fn py_object(self: Rc<Self>) -> Option<(Rc<dyn Any>, &'static str, &'static str)> {
        Some((self, "moonraker.common", "APITransport"))
    }
}

/// The transport a Python object is (None: none)
pub fn transport_from_py(obj: &PyRef) -> PyResult<Option<Rc<dyn ApiTransport>>> {
    if obj.is_none() {
        return Ok(None);
    }
    if obj.getattr("_mr_h").is_ok() {
        let rust = objects::unwrap(obj)?;
        macro_rules! as_transport {
            ($($t:ty),*) => {
                $(if let Ok(t) = rust.clone().downcast::<$t>() {
                    return Ok(Some(t));
                })*
            };
        }
        as_transport!(RemoteConnection, MqttClient, SimplyPrint, InternalTransport, HttpRpcTransport, PyTransport);
        if let Ok(api) = rust.downcast::<KlippyApi>() {
            return Ok(Some(api.host_transport()));
        }
        return Err(py_error("TypeError", format!("{} is not an APITransport", obj.repr()?)));
    }
    let id = lookup("builtins", "id")?.call(&[obj])?.as_i64()? as u64;
    let t = Rc::new(PyTransport { obj: obj.clone(), id });
    objects::bind(obj, t.clone())?;
    Ok(Some(t))
}

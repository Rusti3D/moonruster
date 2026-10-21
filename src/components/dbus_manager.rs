// DBus Connection Management (port of moonraker/components/dbus_manager.py)

use crate::common::ServerError;
use crate::confighelper::ConfigHelper;
use crate::server::{server, Component};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use zvariant::OwnedValue;

const DOC_URL: &str = "https://moonraker.readthedocs.io/en/latest/installation/#policykit-permissions";

pub struct DbusManager {
    conn: RefCell<Option<zbus::Connection>>,
    polkit_available: Cell<bool>,
    warned: Cell<bool>,
    start_ticks: Option<u64>,
}

#[async_trait(?Send)]
impl Component for DbusManager {
    fn has_init(&self) -> bool {
        true
    }
    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        let conn = match zbus::Connection::system().await {
            Ok(c) => c,
            Err(_) => {
                log::info!("Unable to Connect to D-Bus");
                return Ok(());
            }
        };
        *self.conn.borrow_mut() = Some(conn);
        match self.introspect("org.freedesktop.PolicyKit1", "/org/freedesktop/PolicyKit1/Authority").await {
            Ok(xml) if xml.contains("org.freedesktop.PolicyKit1.Authority") => self.polkit_available.set(true),
            Ok(_) => log::info!("Failed to get PolKit interface: interface not found"),
            Err(e) => log::info!("Failed to get PolKit interface: {}", e),
        }
        Ok(())
    }
    async fn close(self: Rc<Self>) {
        *self.conn.borrow_mut() = None;
    }
}

/// Converts a dbus value into json the way dbus_fast unpacks values
pub fn zv_to_json(v: &zvariant::Value) -> Value {
    use zvariant::Value as V;
    match v {
        V::U8(x) => json!(x),
        V::Bool(b) => json!(b),
        V::I16(x) => json!(x),
        V::U16(x) => json!(x),
        V::I32(x) => json!(x),
        V::U32(x) => json!(x),
        V::I64(x) => json!(x),
        V::U64(x) => json!(x),
        V::F64(x) => crate::utils::float_value(*x),
        V::Str(s) => json!(s.as_str()),
        V::Signature(s) => json!(s.to_string()),
        V::ObjectPath(p) => json!(p.as_str()),
        V::Value(inner) => zv_to_json(inner),
        V::Array(a) => Value::Array(a.iter().map(zv_to_json).collect()),
        V::Dict(d) => {
            let mut m = Map::new();
            for (k, val) in d.iter() {
                let key = match zv_to_json(k) {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                m.insert(key, zv_to_json(val));
            }
            Value::Object(m)
        }
        V::Structure(s) => Value::Array(s.fields().iter().map(zv_to_json).collect()),
        V::Fd(_) => Value::Null,
    }
}

impl DbusManager {
    pub fn new(_config: &ConfigHelper) -> Rc<Self> {
        let start_ticks = std::fs::read_to_string("/proc/self/stat")
            .ok()
            .and_then(|d| {
                // the comm field may contain spaces, split after the closing paren
                let rest = d.rsplit_once(')').map(|(_, r)| r.to_string())?;
                rest.split_whitespace().nth(19).and_then(|v| v.parse::<u64>().ok())
            });
        Rc::new(DbusManager { conn: RefCell::new(None), polkit_available: Cell::new(false), warned: Cell::new(false), start_ticks })
    }

    pub fn is_connected(&self) -> bool {
        self.conn.borrow().is_some()
    }

    pub fn connection(&self) -> Option<zbus::Connection> {
        self.conn.borrow().clone()
    }

    pub async fn introspect(&self, bus_name: &str, path: &str) -> Result<String, ServerError> {
        let conn = self.connection().ok_or_else(|| ServerError::bad("Bus not available"))?;
        let msg = conn
            .call_method(Some(bus_name), path, Some("org.freedesktop.DBus.Introspectable"), "Introspect", &())
            .await
            .map_err(|e| ServerError::bad(e.to_string()))?;
        msg.body().deserialize::<String>().map_err(|e| ServerError::bad(e.to_string()))
    }

    /// Equivalent of get_interface(): validates the interface exists on the object
    pub async fn has_interface(&self, bus_name: &str, path: &str, iface: &str) -> Result<(), ServerError> {
        let xml = self.introspect(bus_name, path).await?;
        if xml.contains(&format!("interface name=\"{}\"", iface)) {
            Ok(())
        } else {
            Err(ServerError::bad(format!("interface {} not found", iface)))
        }
    }

    pub async fn call<B, R>(&self, bus_name: &str, path: &str, iface: &str, method: &str, body: &B) -> Result<R, ServerError>
    where
        B: serde::Serialize + zvariant::DynamicType,
        R: for<'d> zvariant::DynamicDeserialize<'d>,
    {
        let conn = self.connection().ok_or_else(|| ServerError::new("DBus Not Connected, ", 503))?;
        let msg = conn
            .call_method(Some(bus_name), path, Some(iface), method, body)
            .await
            .map_err(|e| ServerError::new(e.to_string(), 500))?;
        msg.body().deserialize::<R>().map_err(|e| ServerError::new(e.to_string(), 500))
    }

    pub async fn get_property(&self, bus_name: &str, path: &str, iface: &str, prop: &str) -> Result<Value, ServerError> {
        let v: OwnedValue = self.call(bus_name, path, "org.freedesktop.DBus.Properties", "Get", &(iface, prop)).await?;
        Ok(zv_to_json(&v))
    }

    pub async fn get_all_properties(&self, bus_name: &str, path: &str, iface: &str) -> Result<HashMap<String, Value>, ServerError> {
        let v: HashMap<String, OwnedValue> = self.call(bus_name, path, "org.freedesktop.DBus.Properties", "GetAll", &(iface,)).await?;
        Ok(v.into_iter().map(|(k, v)| (k, zv_to_json(&v))).collect())
    }

    pub async fn check_permission(&self, action: &str, err_msg: &str) -> bool {
        let srv = server();
        if !self.polkit_available.get() {
            srv.add_warning(
                "Unable to find DBus PolKit Interface, this suggests PolKit is not installed on your OS.",
                Some("dbus_polkit"),
                true,
            );
            return false;
        }
        let mut subject: HashMap<&str, zvariant::Value> = HashMap::new();
        subject.insert("pid", zvariant::Value::U32(std::process::id()));
        subject.insert("start-time", zvariant::Value::U64(self.start_ticks.unwrap_or(0)));
        let details: HashMap<&str, &str> = HashMap::new();
        let body = (("unix-process", subject), action, details, 0u32, "");
        let res: Result<(bool, bool, HashMap<String, String>), ServerError> = self
            .call(
                "org.freedesktop.PolicyKit1",
                "/org/freedesktop/PolicyKit1/Authority",
                "org.freedesktop.PolicyKit1.Authority",
                "CheckAuthorization",
                &body,
            )
            .await;
        match res {
            Err(e) => {
                self.check_warned();
                srv.add_warning(
                    &format!(
                        "Error checking authorization for action [{}]: {}. This suggests that a dependency is not installed or up to date. {}.",
                        action, e, err_msg
                    ),
                    None,
                    true,
                );
                false
            }
            Ok((authorized, _, _)) => {
                if !authorized {
                    self.check_warned();
                    srv.add_warning(&format!("Moonraker not authorized for PolicyKit action: [{}], {}", action, err_msg), None, true);
                }
                authorized
            }
        }
    }

    fn check_warned(&self) {
        if !self.warned.get() {
            server().add_warning(&format!("PolKit warnings detected. See {} for instructions on how to resolve.", DOC_URL), None, true);
            self.warned.set(true);
        }
    }
}

pub fn load_component(config: &ConfigHelper) -> Rc<DbusManager> {
    DbusManager::new(config)
}

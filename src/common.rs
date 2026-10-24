// Common classes used throughout Moonruster (port of moonraker/common.py)

use crate::pyfmt::{py_str, py_type_name};
use futures::future::LocalBoxFuture;
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::rc::Rc;

pub type Args = Map<String, Value>;
pub type ApiResult = Result<Value, ServerError>;
pub type ApiCallback = Rc<dyn Fn(WebRequest) -> LocalBoxFuture<'static, ApiResult>>;

pub const ENDPOINT_PREFIXES: [&str; 6] = ["printer", "server", "machine", "access", "api", "debug"];

// ***** Errors *****

#[derive(Debug, Clone)]
pub struct ServerError {
    pub message: String,
    pub status_code: i64,
    /// Present for AgentError (status 424)
    pub error_data: Option<Value>,
    /// Raised as a tornado HTTPError (message is a log message)
    pub is_http: bool,
    /// A python exception that is not a ServerError (ValueError, KeyError,
    /// OSError...): http requests get a plain 500, json-rpc gets str(e)
    pub is_exc: bool,
    /// A python TypeError from an API callback (JSON-RPC: -32602 Invalid params)
    pub is_type_error: bool,
    /// The traceback of an exception from a Python component (the uncaught
    /// exception log of HTTP requests shows it)
    pub traceback: Option<String>,
}

impl ServerError {
    pub fn new(message: impl Into<String>, status_code: i64) -> Self {
        ServerError { message: message.into(), status_code, error_data: None, is_http: false, is_exc: false, is_type_error: false, traceback: None }
    }
    /// A non-ServerError python exception
    pub fn exc(message: impl Into<String>) -> Self {
        ServerError { message: message.into(), status_code: 500, error_data: None, is_http: false, is_exc: true, is_type_error: false, traceback: None }
    }
    /// tornado.web.HTTPError(code, log_message) raised from an API callback
    pub fn http(status_code: i64, log_message: impl Into<String>) -> Self {
        ServerError { message: log_message.into(), status_code, error_data: None, is_http: true, is_exc: false, is_type_error: false, traceback: None }
    }
    pub fn bad(message: impl Into<String>) -> Self {
        Self::new(message, 400)
    }
    /// A python TypeError raised by an API callback
    pub fn type_error(message: impl Into<String>) -> Self {
        ServerError { is_type_error: true, ..Self::exc(message) }
    }
    pub fn agent(message: impl Into<String>, data: Value) -> Self {
        ServerError { message: message.into(), status_code: 424, error_data: Some(data), is_http: false, is_exc: false, is_type_error: false, traceback: None }
    }
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ServerError {}

impl From<std::io::Error> for ServerError {
    fn from(e: std::io::Error) -> Self {
        ServerError::exc(e.to_string())
    }
}

impl From<rusqlite::Error> for ServerError {
    fn from(e: rusqlite::Error) -> Self {
        ServerError::exc(e.to_string())
    }
}

#[macro_export]
macro_rules! srv_err {
    ($code:expr, $($arg:tt)*) => {
        $crate::common::ServerError::new(format!($($arg)*), $code)
    };
}

// ***** Flags *****

macro_rules! ext_flag {
    ($name:ident { $($member:ident = $val:expr),* $(,)? }) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
        pub struct $name(pub u32);
        #[allow(non_upper_case_globals, dead_code)]
        impl $name {
            $(pub const $member: $name = $name($val);)*
            const MEMBERS: &'static [(&'static str, u32)] = &[$((stringify!($member), $val)),*];
            pub fn all() -> Self {
                $name(0 $(| $val)*)
            }
            pub fn empty() -> Self { $name(0) }
            pub fn contains(&self, other: Self) -> bool {
                other.0 != 0 && (self.0 & other.0) == other.0
            }
            pub fn from_string(name: &str) -> Result<Self, String> {
                let upper = name.to_uppercase();
                for (n, v) in Self::MEMBERS {
                    if *n == upper {
                        return Ok($name(*v));
                    }
                }
                Err(format!("No flag member named {}", name))
            }
            pub fn from_string_list<S: AsRef<str>>(names: &[S]) -> Result<Self, String> {
                let mut ret = $name(0);
                for n in names {
                    ret = ret | Self::from_string(n.as_ref())?;
                }
                Ok(ret)
            }
            /// Iterate over single bit members in definition order
            pub fn iter(&self) -> impl Iterator<Item = $name> + '_ {
                Self::MEMBERS.iter().filter(move |(_, v)| self.0 & v == *v).map(|(_, v)| $name(*v))
            }
            pub fn len(&self) -> usize {
                self.0.count_ones() as usize
            }
            /// The member name, only valid for single bit values
            pub fn name(&self) -> Option<&'static str> {
                Self::MEMBERS.iter().find(|(_, v)| *v == self.0).map(|(n, _)| *n)
            }
            /// Matches python's `name` attribute for composite flags (members joined by '|')
            pub fn py_name(&self) -> String {
                match self.name() {
                    Some(n) => n.to_string(),
                    None => self.iter().filter_map(|m| m.name()).collect::<Vec<_>>().join("|"),
                }
            }
        }
        impl std::ops::BitOr for $name {
            type Output = $name;
            fn bitor(self, rhs: $name) -> $name { $name(self.0 | rhs.0) }
        }
        impl std::ops::BitAnd for $name {
            type Output = $name;
            fn bitand(self, rhs: $name) -> $name { $name(self.0 & rhs.0) }
        }
        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}.{}", stringify!($name), self.py_name())
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}.{}", stringify!($name), self.py_name())
            }
        }
    };
}

ext_flag!(RequestType { GET = 1, POST = 2, DELETE = 4 });
ext_flag!(TransportType { HTTP = 1, WEBSOCKET = 2, MQTT = 4, INTERNAL = 8 });

// ***** Enums *****

#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord, Hash)]
pub enum JobEvent {
    Standby = 1,
    Started = 2,
    Paused = 3,
    Resumed = 4,
    Complete = 5,
    Error = 6,
    Cancelled = 7,
}

impl JobEvent {
    pub fn aborted(&self) -> bool {
        (*self as i32) >= 6
    }
    pub fn is_printing(&self) -> bool {
        matches!(self, JobEvent::Started | JobEvent::Resumed)
    }
    pub fn from_string(name: &str) -> Result<Self, String> {
        Ok(match name.to_uppercase().as_str() {
            "STANDBY" => JobEvent::Standby,
            "STARTED" => JobEvent::Started,
            "PAUSED" => JobEvent::Paused,
            "RESUMED" => JobEvent::Resumed,
            "COMPLETE" => JobEvent::Complete,
            "ERROR" => JobEvent::Error,
            "CANCELLED" => JobEvent::Cancelled,
            _ => return Err(format!("No enum member named {}", name)),
        })
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            JobEvent::Standby => "standby",
            JobEvent::Started => "started",
            JobEvent::Paused => "paused",
            JobEvent::Resumed => "resumed",
            JobEvent::Complete => "complete",
            JobEvent::Error => "error",
            JobEvent::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for JobEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KlippyStateKind {
    Disconnected = 1,
    Startup = 2,
    Ready = 3,
    Error = 4,
    Shutdown = 5,
}

/// Python's KlippyState enum members are singletons with a mutable message
/// attribute.  The message is therefore shared per member; we emulate this
/// with a per-member message table owned by the klippy connection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KlippyState(pub KlippyStateKind);

impl KlippyState {
    pub const DISCONNECTED: KlippyState = KlippyState(KlippyStateKind::Disconnected);
    pub const STARTUP: KlippyState = KlippyState(KlippyStateKind::Startup);
    pub const READY: KlippyState = KlippyState(KlippyStateKind::Ready);
    pub const ERROR: KlippyState = KlippyState(KlippyStateKind::Error);
    pub const SHUTDOWN: KlippyState = KlippyState(KlippyStateKind::Shutdown);

    pub fn from_string(name: &str) -> Result<Self, String> {
        Ok(match name.to_uppercase().as_str() {
            "DISCONNECTED" => Self::DISCONNECTED,
            "STARTUP" => Self::STARTUP,
            "READY" => Self::READY,
            "ERROR" => Self::ERROR,
            "SHUTDOWN" => Self::SHUTDOWN,
            _ => return Err(format!("No enum member named {}", name)),
        })
    }
    pub fn startup_complete(&self) -> bool {
        (self.0 as i32) > 2
    }
    pub fn as_str(&self) -> &'static str {
        match self.0 {
            KlippyStateKind::Disconnected => "disconnected",
            KlippyStateKind::Startup => "startup",
            KlippyStateKind::Ready => "ready",
            KlippyStateKind::Error => "error",
            KlippyStateKind::Shutdown => "shutdown",
        }
    }
    pub fn index(&self) -> usize {
        self.0 as usize - 1
    }
}

impl fmt::Display for KlippyState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ***** User Info *****

#[derive(Clone, Debug, PartialEq)]
pub struct UserInfo {
    pub username: String,
    pub password: String,
    pub created_on: f64,
    pub salt: String,
    pub source: String,
    pub jwt_secret: Option<String>,
    pub jwk_id: Option<String>,
    pub groups: Vec<String>,
}

impl UserInfo {
    pub fn new(username: &str, password: &str) -> Self {
        UserInfo {
            username: username.to_string(),
            password: password.to_string(),
            created_on: crate::utils::time_now(),
            salt: String::new(),
            source: "moonraker".into(),
            jwt_secret: None,
            jwk_id: None,
            groups: vec!["admin".into()],
        }
    }
    pub fn as_dict(&self) -> Value {
        json!({
            "username": self.username,
            "password": self.password,
            "created_on": self.created_on,
            "salt": self.salt,
            "source": self.source,
            "jwt_secret": self.jwt_secret,
            "jwk_id": self.jwk_id,
            "groups": self.groups,
        })
    }
    pub fn from_dict(v: &Value) -> Option<Self> {
        let o = v.as_object()?;
        Some(UserInfo {
            username: o.get("username")?.as_str()?.to_string(),
            password: o.get("password").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            created_on: o.get("created_on").and_then(|v| v.as_f64()).unwrap_or_else(crate::utils::time_now),
            salt: o.get("salt").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            source: o.get("source").and_then(|v| v.as_str()).unwrap_or("moonraker").to_string(),
            jwt_secret: o.get("jwt_secret").and_then(|v| v.as_str()).map(String::from),
            jwk_id: o.get("jwk_id").and_then(|v| v.as_str()).map(String::from),
            groups: o
                .get("groups")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|g| g.as_str().map(String::from)).collect())
                .unwrap_or_else(|| vec!["admin".into()]),
        })
    }
}

// ***** API Definition *****

pub struct ApiDefinition {
    pub endpoint: String,
    pub http_path: String,
    pub rpc_methods: Vec<String>,
    pub request_types: RequestType,
    pub transports: TransportType,
    pub callback: ApiCallback,
    pub auth_required: bool,
}

impl fmt::Display for ApiDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tprt: Vec<&str> = self.transports.iter().filter_map(|t| t.name()).collect();
        write!(f, "(Transports: {})", tprt.join("|"))?;
        if self.transports.contains(TransportType::HTTP) {
            let rts: Vec<&str> = self.request_types.iter().filter_map(|t| t.name()).collect();
            write!(f, " (HTTP Request: {} {})", rts.join("|"), self.http_path)?;
        }
        if !self.rpc_methods.is_empty() {
            write!(f, " (RPC Methods: {})", self.rpc_methods.join(" "))?;
        }
        write!(f, " (Auth Required: {})", if self.auth_required { "True" } else { "False" })
    }
}

impl ApiDefinition {
    pub fn need_object_parser(&self) -> bool {
        self.endpoint.starts_with("objects/")
    }

    pub fn rpc_items(&self) -> Vec<(RequestType, String)> {
        self.request_types.iter().zip(self.rpc_methods.iter().cloned()).collect()
    }

    pub async fn request(
        &self,
        args: Args,
        request_type: RequestType,
        transport: Option<Rc<dyn ApiTransport>>,
        ip_addr: Option<IpAddr>,
        user: Option<UserInfo>,
    ) -> ApiResult {
        let req = WebRequest::new(&self.endpoint, args, request_type, transport, ip_addr, user);
        (self.callback)(req).await
    }

    pub fn create(
        cache: &RefCell<HashMap<String, Rc<ApiDefinition>>>,
        endpoint: &str,
        request_types: RequestType,
        callback: ApiCallback,
        transports: TransportType,
        auth_required: bool,
        is_remote: bool,
    ) -> Result<Rc<ApiDefinition>, ServerError> {
        if let Some(d) = cache.borrow().get(endpoint) {
            return Ok(d.clone());
        }
        let http_path = if is_remote {
            format!("/printer/{}", endpoint.trim_matches('/'))
        } else {
            endpoint.to_string()
        };
        if transports.contains(TransportType::HTTP) {
            // r"^/([^/]+)"
            let ok = http_path
                .strip_prefix('/')
                .and_then(|p| p.split('/').next())
                .map(|seg| !seg.is_empty() && ENDPOINT_PREFIXES.contains(&seg))
                .unwrap_or(false);
            if !ok {
                let prefixes: Vec<String> = ENDPOINT_PREFIXES.iter().map(|p| format!("'/{} '", p)).collect();
                return Err(ServerError::bad(format!(
                    "Invalid endpoint name '{}', must start with one of the following: [{}]",
                    endpoint,
                    prefixes.join(", ")
                )));
            }
        }
        let mut rpc_methods = Vec::new();
        let mut request_types = request_types;
        if is_remote {
            request_types = RequestType::GET | RequestType::POST;
            rpc_methods.push(http_path[1..].replace('/', "."));
        } else if transports != TransportType::HTTP {
            let name_parts: Vec<&str> = http_path[1..].split('/').collect();
            if request_types.len() > 1 {
                for rtype in request_types.iter() {
                    let func_name = format!("{}_{}", rtype.name().unwrap().to_lowercase(), name_parts.last().unwrap());
                    let mut parts: Vec<String> = name_parts[..name_parts.len() - 1].iter().map(|s| s.to_string()).collect();
                    parts.push(func_name);
                    rpc_methods.push(parts.join("."));
                }
            } else {
                rpc_methods.push(name_parts.join("."));
            }
            if request_types.len() != rpc_methods.len() {
                return Err(ServerError::bad(
                    "Invalid API definition.  Number of websocket methods must match the number of request methods",
                ));
            }
        }
        let api_def = Rc::new(ApiDefinition {
            endpoint: endpoint.to_string(),
            http_path,
            rpc_methods,
            request_types,
            transports,
            callback,
            auth_required,
        });
        cache.borrow_mut().insert(endpoint.to_string(), api_def.clone());
        Ok(api_def)
    }
}

// ***** Transports *****

pub trait ApiTransport {
    fn transport_type(&self) -> TransportType {
        TransportType::INTERNAL
    }
    fn user_info(&self) -> Option<UserInfo> {
        None
    }
    fn ip_addr(&self) -> Option<IpAddr> {
        None
    }
    fn screen_rpc_request(&self, _api_def: &ApiDefinition, _req_type: RequestType, _args: &mut Args) -> Result<(), ServerError> {
        Ok(())
    }
    fn send_status(&self, _status: &Map<String, Value>, _eventtime: f64) {}
    /// Unique identifier used to key subscriptions
    fn transport_id(&self) -> u64;
    /// Returns the remote connection if this transport is a websocket/unix connection
    fn as_remote(self: Rc<Self>) -> Option<Rc<crate::components::websockets::RemoteConnection>> {
        None
    }
    /// The Moonraker object this transport is, for Python components
    /// (WebRequest.transport): the Rust object, its module and class
    fn py_object(self: Rc<Self>) -> Option<(Rc<dyn std::any::Any>, &'static str, &'static str)> {
        None
    }
}

// ***** Web Request *****

#[derive(Clone)]
pub struct WebRequest {
    pub endpoint: String,
    pub args: Args,
    pub request_type: RequestType,
    pub transport: Option<Rc<dyn ApiTransport>>,
    pub ip_addr: Option<IpAddr>,
    pub current_user: Option<UserInfo>,
}

fn missing(key: &str) -> ServerError {
    ServerError::bad(format!("No data for argument: {}", key))
}

fn conv_err(key: &str, dtype: &str, val: &Value) -> ServerError {
    ServerError::bad(format!(
        "Unable to convert argument [{}] to <class '{}'>: value received: {}",
        key,
        dtype,
        py_str(val)
    ))
}

pub fn value_to_int(val: &Value) -> Option<i64> {
    match val {
        Value::Bool(b) => Some(*b as i64),
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::String(s) => crate::pyfmt::py_int(s),
        _ => None,
    }
}

pub fn value_to_float(val: &Value) -> Option<f64> {
    match val {
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Number(n) => n.as_f64(),
        Value::String(s) => crate::pyfmt::py_float(s),
        _ => None,
    }
}

impl WebRequest {
    pub fn new(
        endpoint: &str,
        args: Args,
        request_type: RequestType,
        transport: Option<Rc<dyn ApiTransport>>,
        ip_addr: Option<IpAddr>,
        user: Option<UserInfo>,
    ) -> Self {
        WebRequest { endpoint: endpoint.to_string(), args, request_type, transport, ip_addr, current_user: user }
    }

    pub fn get_endpoint(&self) -> &str {
        &self.endpoint
    }
    pub fn get_request_type(&self) -> RequestType {
        self.request_type
    }
    pub fn get_args(&self) -> &Args {
        &self.args
    }
    pub fn get_subscribable(&self) -> Option<Rc<dyn ApiTransport>> {
        self.transport.clone()
    }
    pub fn get_client_connection(&self) -> Option<Rc<crate::components::websockets::RemoteConnection>> {
        self.transport.clone().and_then(|t| t.as_remote())
    }
    pub fn get_ip_address(&self) -> Option<IpAddr> {
        self.ip_addr
    }
    pub fn get_current_user(&self) -> Option<&UserInfo> {
        self.current_user.as_ref()
    }

    pub fn has(&self, key: &str) -> bool {
        self.args.contains_key(key)
    }

    /// Raw argument access.  Returns an error if missing.
    pub fn get(&self, key: &str) -> Result<Value, ServerError> {
        self.args.get(key).cloned().ok_or_else(|| missing(key))
    }

    pub fn get_opt(&self, key: &str) -> Option<Value> {
        self.args.get(key).cloned()
    }

    pub fn get_str(&self, key: &str) -> Result<String, ServerError> {
        match self.args.get(key) {
            None => Err(missing(key)),
            Some(v) => Ok(py_str(v)),
        }
    }

    pub fn get_str_or(&self, key: &str, default: &str) -> String {
        self.args.get(key).map(py_str).unwrap_or_else(|| default.to_string())
    }

    pub fn get_str_opt(&self, key: &str) -> Option<String> {
        self.args.get(key).map(py_str)
    }

    pub fn get_int(&self, key: &str) -> Result<i64, ServerError> {
        match self.args.get(key) {
            None => Err(missing(key)),
            Some(v) => value_to_int(v).ok_or_else(|| conv_err(key, "int", v)),
        }
    }

    pub fn get_int_opt(&self, key: &str) -> Result<Option<i64>, ServerError> {
        match self.args.get(key) {
            None => Ok(None),
            Some(v) => value_to_int(v).map(Some).ok_or_else(|| conv_err(key, "int", v)),
        }
    }

    pub fn get_int_or(&self, key: &str, default: i64) -> Result<i64, ServerError> {
        Ok(self.get_int_opt(key)?.unwrap_or(default))
    }

    pub fn get_float(&self, key: &str) -> Result<f64, ServerError> {
        match self.args.get(key) {
            None => Err(missing(key)),
            Some(v) => value_to_float(v).ok_or_else(|| conv_err(key, "float", v)),
        }
    }

    pub fn get_float_opt(&self, key: &str) -> Result<Option<f64>, ServerError> {
        match self.args.get(key) {
            None => Ok(None),
            Some(v) => value_to_float(v).map(Some).ok_or_else(|| conv_err(key, "float", v)),
        }
    }

    pub fn get_float_or(&self, key: &str, default: f64) -> Result<f64, ServerError> {
        Ok(self.get_float_opt(key)?.unwrap_or(default))
    }

    fn conv_bool(key: &str, v: &Value) -> Result<bool, ServerError> {
        match v {
            Value::Bool(b) => Ok(*b),
            Value::String(s) => match s.to_lowercase().as_str() {
                "true" => Ok(true),
                "false" => Ok(false),
                _ => Err(conv_err(key, "bool", v)),
            },
            _ => Err(conv_err(key, "bool", v)),
        }
    }

    pub fn get_boolean(&self, key: &str) -> Result<bool, ServerError> {
        match self.args.get(key) {
            None => Err(missing(key)),
            Some(v) => Self::conv_bool(key, v),
        }
    }

    pub fn get_boolean_opt(&self, key: &str) -> Result<Option<bool>, ServerError> {
        match self.args.get(key) {
            None => Ok(None),
            Some(v) => Self::conv_bool(key, v).map(Some),
        }
    }

    pub fn get_boolean_or(&self, key: &str, default: bool) -> Result<bool, ServerError> {
        Ok(self.get_boolean_opt(key)?.unwrap_or(default))
    }

    pub fn get_list_opt(&self, key: &str, sep: &str, count: Option<usize>) -> Result<Option<Vec<String>>, ServerError> {
        let value = match self.args.get(key) {
            None => return Ok(None),
            Some(v) => v,
        };
        let ret: Vec<String> = match value {
            Value::String(s) => s.split(sep).map(|v| v.trim()).filter(|v| !v.is_empty()).map(String::from).collect(),
            Value::Array(items) => {
                let mut out = Vec::new();
                for v in items {
                    match v {
                        Value::String(s) => out.push(s.clone()),
                        _ => {
                            return Err(ServerError::bad(format!(
                                "Invalid list format for argument '{}', expected all values to be of type str.",
                                key
                            )))
                        }
                    }
                }
                out
            }
            other => {
                return Err(ServerError::bad(format!(
                    "Invalid value received for argument '{}'.  Expected List type, received {}",
                    key,
                    py_type_name(other)
                )))
            }
        };
        if let Some(c) = count {
            if ret.len() != c {
                return Err(ServerError::bad(format!(
                    "Invalid list received for argument '{}', count mismatch. Expected {} items, got {}.",
                    key,
                    c,
                    ret.len()
                )));
            }
        }
        Ok(Some(ret))
    }

    pub fn get_list(&self, key: &str) -> Result<Vec<String>, ServerError> {
        self.get_list_opt(key, ",", None)?.ok_or_else(|| missing(key))
    }
}

// ***** JSON-RPC *****

pub struct JsonRpc {
    pub methods: RefCell<HashMap<String, (RequestType, Rc<ApiDefinition>)>>,
    sanitize_response: std::cell::Cell<bool>,
    verbose: bool,
}

impl JsonRpc {
    pub fn new(verbose: bool) -> Self {
        JsonRpc { methods: RefCell::new(HashMap::new()), sanitize_response: std::cell::Cell::new(false), verbose }
    }

    fn log_request(&self, rpc_obj: &Value, trtype: TransportType) {
        if !self.verbose {
            return;
        }
        self.sanitize_response.set(false);
        let mut output = rpc_obj.clone();
        if let Some(method) = rpc_obj.get("method").and_then(|m| m.as_str()) {
            let params = rpc_obj.get("params");
            if method.starts_with("access.") || method == "machine.sudo.password" {
                self.sanitize_response.set(true);
                if let Some(Value::Object(p)) = params {
                    if !p.is_empty() {
                        let sanitized: Map<String, Value> =
                            p.keys().map(|k| (k.clone(), Value::String("<sanitized>".into()))).collect();
                        output["params"] = Value::Object(sanitized);
                    }
                }
            } else if method == "server.connection.identify" {
                if let Some(Value::Object(p)) = output.get_mut("params") {
                    for field in ["access_token", "api_key"] {
                        if p.contains_key(field) {
                            p.insert(field.into(), Value::String("<sanitized>".into()));
                        }
                    }
                }
            }
        }
        log::debug!("{} Received::{}", trtype, output);
    }

    fn log_response(&self, resp: &Value, trtype: TransportType) {
        if !self.verbose {
            return;
        }
        let mut output = resp.clone();
        if self.sanitize_response.get() && resp.get("result").is_some() {
            output["result"] = Value::String("<sanitized>".into());
        }
        self.sanitize_response.set(false);
        log::debug!("{} Response::{}", trtype, output);
    }

    pub fn register_method(&self, name: &str, request_type: RequestType, api_def: Rc<ApiDefinition>) {
        self.methods.borrow_mut().insert(name.to_string(), (request_type, api_def));
    }

    pub fn get_method(&self, name: &str) -> Option<(RequestType, Rc<ApiDefinition>)> {
        self.methods.borrow().get(name).cloned()
    }

    pub async fn dispatch(&self, data: &[u8], transport: Rc<dyn ApiTransport>) -> Option<String> {
        let ttype = transport.transport_type();
        let obj: Value = match serde_json::from_slice(data) {
            Ok(v) => v,
            Err(_) => {
                log::error!("{} data not valid json: {}", ttype, String::from_utf8_lossy(data));
                let err = self.build_error(-32700, "Parse error", Value::Null, None, "");
                return Some(err.to_string());
            }
        };
        match obj {
            Value::Array(items) => {
                let mut responses = Vec::new();
                for item in items {
                    self.log_request(&item, ttype);
                    if let Some(resp) = self.process_object(item, transport.clone()).await {
                        self.log_response(&resp, ttype);
                        responses.push(resp);
                    }
                }
                if !responses.is_empty() {
                    return Some(Value::Array(responses).to_string());
                }
                None
            }
            obj => {
                self.log_request(&obj, ttype);
                let resp = self.process_object(obj, transport).await?;
                self.log_response(&resp, ttype);
                Some(resp.to_string())
            }
        }
    }

    pub async fn process_object(&self, obj: Value, transport: Rc<dyn ApiTransport>) -> Option<Value> {
        let obj = match obj {
            Value::Object(o) => o,
            // Python raises AttributeError here which is logged and swallowed
            other => {
                log::error!("Websocket Command Error: invalid object {}", other);
                return None;
            }
        };
        let req_id = obj.get("id").cloned().unwrap_or(Value::Null);
        if obj.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
            return Some(self.build_error(-32600, "Invalid Request", req_id, None, ""));
        }
        let method_name = match obj.get("method") {
            None => {
                self.process_response(&obj, transport);
                return None;
            }
            Some(Value::String(s)) => s.clone(),
            Some(other) => {
                return Some(self.build_error(-32600, "Invalid Request", req_id, None, &py_str(other)));
            }
        };
        let method_info = self.get_method(&method_name);
        let (request_type, api_def) = match method_info {
            None => return Some(self.build_error(-32601, "Method not found", req_id, None, &method_name)),
            Some(m) => m,
        };
        let ttype = transport.transport_type();
        if !api_def.transports.contains(ttype) {
            return Some(self.build_error(
                -32601,
                &format!("Method not found for transport {}", ttype.py_name()),
                req_id,
                None,
                &method_name,
            ));
        }
        let mut params = Map::new();
        if let Some(p) = obj.get("params") {
            match p {
                Value::Object(m) => params = m.clone(),
                _ => return Some(self.build_error(-32602, "Invalid params:", req_id, None, &method_name)),
            }
        }
        self.execute_method(&method_name, request_type, &api_def, req_id, transport, params).await
    }

    fn process_response(&self, obj: &Map<String, Value>, conn: Rc<dyn ApiTransport>) {
        let conn = match conn.as_remote() {
            Some(c) => c,
            None => {
                log::debug!("RPC Response to non-socket request: {}", Value::Object(obj.clone()));
                return;
            }
        };
        let response_id = match obj.get("id") {
            None | Some(Value::Null) => {
                log::debug!("RPC Response with null ID: {}", Value::Object(obj.clone()));
                return;
            }
            Some(id) => id.clone(),
        };
        let result = obj.get("result").cloned().unwrap_or(Value::Null);
        let ret = if result.is_null() {
            let name = conn.client_data().get("name").cloned().unwrap_or_default();
            Err(ServerError::agent(format!("Agent {} RPC error", name), obj.get("error").cloned().unwrap_or(Value::Null)))
        } else {
            Ok(result)
        };
        conn.resolve_pending_response(&response_id, ret);
    }

    pub async fn execute_method(
        &self,
        method_name: &str,
        request_type: RequestType,
        api_def: &ApiDefinition,
        req_id: Value,
        transport: Rc<dyn ApiTransport>,
        mut params: Args,
    ) -> Option<Value> {
        let result = match transport.screen_rpc_request(api_def, request_type, &mut params) {
            Err(e) => Err(e),
            Ok(()) => {
                api_def
                    .request(params, request_type, Some(transport.clone()), transport.ip_addr(), transport.user_info())
                    .await
            }
        };
        match result {
            // common.py JsonRPC.execute_method: except TypeError
            Err(e) if e.is_type_error => {
                Some(self.build_error(-32602, &format!("Invalid params:\n{}", e.message), req_id, Some(&e), method_name))
            }
            Err(e) if e.is_http => {
                let reason = crate::components::application::standard_reason(e.status_code as u16);
                let msg = format!("HTTP {}: {} ({})", e.status_code, reason, e.message);
                Some(self.build_error(500, &msg, req_id, Some(&e), method_name))
            }
            Err(e) => {
                let mut code = e.status_code;
                if code == 404 {
                    code = -32601;
                } else if code == 401 {
                    code = -32602;
                }
                Some(self.build_error(code, &e.message, req_id, Some(&e), method_name))
            }
            Ok(result) => {
                if req_id.is_null() {
                    None
                } else {
                    Some(self.build_result(result, req_id))
                }
            }
        }
    }

    pub fn build_result(&self, result: Value, req_id: Value) -> Value {
        json!({"jsonrpc": "2.0", "result": result, "id": req_id})
    }

    pub fn build_error(&self, code: i64, msg: &str, req_id: Value, exc: Option<&ServerError>, method_name: &str) -> Value {
        let method_part = if method_name.is_empty() { String::new() } else { format!("Requested Method: {}, ", method_name) };
        let mut log_msg = format!("JSON-RPC Request Error - {}Code: {}, Message: {}", method_part, code, msg);
        let mut err = Map::new();
        err.insert("code".into(), json!(code));
        err.insert("message".into(), json!(msg));
        if let Some(e) = exc {
            if let Some(data) = &e.error_data {
                err.insert("data".into(), data.clone());
                if self.verbose {
                    log_msg.push_str(&format!("\nExtra data: {}", data));
                }
            }
        }
        log::info!("{}", log_msg);
        json!({"jsonrpc": "2.0", "error": Value::Object(err), "id": req_id})
    }
}

/// Helper to build an ApiCallback from an async closure
pub fn callback<F, Fut>(f: F) -> ApiCallback
where
    F: Fn(WebRequest) -> Fut + 'static,
    Fut: std::future::Future<Output = ApiResult> + 'static,
{
    Rc::new(move |req| Box::pin(f(req)))
}

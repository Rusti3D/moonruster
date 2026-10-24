// moonraker.components.application.MoonrakerApp.register_endpoint for
// Python components: the component's coroutine answers the endpoint on
// every transport, with Moonraker's own WebRequest (verbatim class) built
// from Moonruster's request, and Moonraker's error mapping
use super::*;
use crate::common::{callback, RequestType, ServerError, TransportType, UserInfo, WebRequest};
use crate::plugins::futures::await_py;
use crate::server::server;

macro_rules! app {
    ($m:literal) => {
        concat!("moonraker.components.application.MoonrakerApp.", $m)
    };
}

macro_rules! srv {
    ($m:literal) => {
        concat!("moonraker.server.Server.", $m)
    };
}

/// Moonraker's UserInfo (dataclass) of a Rust UserInfo
pub fn user_info(u: &UserInfo) -> PyResult<PyRef> {
    let cls = lookup("moonraker.common", "UserInfo")?;
    let groups = to_py(&Value::from(u.groups.clone()))?;
    let opt = |v: &Option<String>| v.as_deref().map(py_str).unwrap_or_else(none);
    cls.call(&[
        &py_str(&u.username),
        &py_str(&u.password),
        &py_float(u.created_on),
        &py_str(&u.salt),
        &py_str(&u.source),
        &opt(&u.jwt_secret),
        &opt(&u.jwk_id),
        &groups,
    ])
}

/// Moonraker's WebRequest of a Rust request: endpoint, args, request type,
/// the transport's Moonraker object, the client address
/// (utils.parse_ip_address) and the user
pub fn py_web_request(req: &WebRequest) -> PyResult<PyRef> {
    let common = import("moonraker.common")?;
    let args = to_py(&Value::Object(req.args.clone()))?;
    let rt = common.getattr("RequestType")?.call(&[&py_int(req.request_type.0 as i64)])?;
    let transport = match req.transport.clone().and_then(|t| t.py_object()) {
        Some((rust, module, class)) => objects::wrap_class(rust, module, class)?,
        None => none(),
    };
    let ip = match req.ip_addr {
        Some(ip) => lookup("moonraker.utils", "parse_ip_address")?.call(&[&py_str(&ip.to_string())])?,
        None => none(),
    };
    let user = match &req.current_user {
        Some(u) => user_info(u)?,
        None => none(),
    };
    common.getattr("WebRequest")?.call(&[&py_str(&req.endpoint), &args, &rt, &transport, &ip, &user])
}

/// A Python exception from an API callback as Moonruster's ServerError:
/// Moonraker's ServerError / AgentError keep their status and data, a
/// TypeError is JSON-RPC's "Invalid params", anything else is an uncaught
/// exception (HTTP 500, JSON-RPC 500 with its message). Moonraker logs none
/// of them beyond its request / JSON-RPC error line
pub fn to_server_error(e: &PyError) -> ServerError {
    let is = |module: &str, class: &str| -> bool {
        match (&e.value, lookup(module, class)) {
            (Some(v), Ok(c)) => v.is_instance(&c).unwrap_or(false),
            _ => false,
        }
    };
    if is("moonraker.utils.exceptions", "ServerError") {
        let v = e.value.as_ref().unwrap();
        let code = v.getattr("status_code").and_then(|c| c.as_i64()).unwrap_or(400);
        if is("moonraker.utils.exceptions", "AgentError") {
            let data = v.getattr("error_data").and_then(|d| from_py(&d)).unwrap_or(Value::Null);
            return ServerError::agent(e.message.clone(), data);
        }
        return ServerError::new(e.message.clone(), code);
    }
    let mut err = if is("builtins", "TypeError") {
        ServerError::type_error(e.message.clone())
    } else {
        ServerError::exc(e.message.clone())
    };
    // tornado logs the traceback of an uncaught exception of an HTTP request
    err.traceback = Some(e.traceback.clone());
    err
}

/// A request type / transport argument: a flag, or a list of names
/// (Flag.from_string_list), as its integer value
fn flag_bits(v: &PyRef, class: &str) -> PyResult<u32> {
    let flag = if v.is_instance(&lookup("builtins", "list")?)? {
        lookup("moonraker.common", class)?.call_method("from_string_list", &[v])?
    } else {
        v.clone()
    };
    Ok(flag.getattr("value")?.as_i64()? as u32)
}

/// Answer a request with the component's callback: await what it returns
async fn call_endpoint(cb: PyRef, req: WebRequest) -> Result<Value, ServerError> {
    let r: PyResult<PyRef> = async {
        let wr = py_web_request(&req)?;
        let ret = cb.call(&[&wr])?;
        // Moonraker awaits the callback's result (`await api_definition.request(...)`)
        await_py(ret).await
    }
    .await;
    match r {
        Ok(v) => from_py(&v).map_err(|e| to_server_error(&e)),
        Err(e) => Err(to_server_error(&e)),
    }
}

fn arg<'a>(a: &'a Args, i: usize, n: &str) -> PyResult<&'a PyRef> {
    a.get(i, n).ok_or_else(|| py_error("TypeError", format!("missing argument '{}'", n)))
}

pub fn register(r: &mut Registry) {
    // Server.__init__: self.register_endpoint = app.register_endpoint
    r.attr(srv!("register_endpoint"), |_| objects::wrap(server().app())?.getattr("register_endpoint"), None);
    r.attr(srv!("register_debug_endpoint"), |_| objects::wrap(server().app())?.getattr("register_debug_endpoint"), None);
    r.method(app!("register_endpoint"), |a| {
        let endpoint = arg(a, 0, "endpoint")?.as_string()?;
        let request_types = RequestType(flag_bits(arg(a, 1, "request_types")?, "RequestType")?);
        let cb = arg(a, 2, "callback")?.clone();
        let transports = match a.get(3, "transports") {
            Some(t) => TransportType(flag_bits(t, "TransportType")?),
            None => TransportType::all(),
        };
        let wrap_result = match a.get(4, "wrap_result") {
            Some(v) => v.is_true()?,
            None => true,
        };
        let content_type = match a.get(5, "content_type") {
            Some(v) if !v.is_none() => Some(v.as_string()?),
            _ => None,
        };
        let auth_required = match a.get(6, "auth_required") {
            Some(v) => v.is_true()?,
            None => true,
        };
        let is_remote = match a.get(7, "is_remote") {
            Some(v) => v.is_true()?,
            None => false,
        };
        let api_cb = callback(move |req: WebRequest| call_endpoint(cb.clone(), req));
        server()
            .app()
            .register_endpoint(&endpoint, request_types, api_cb, transports, wrap_result, content_type.as_deref(), auth_required, is_remote)
            .map_err(|e| server_error(&e.message, e.status_code))?;
        Ok(none())
    });
    r.method(app!("register_debug_endpoint"), |a| {
        let endpoint = arg(a, 0, "endpoint")?.as_string()?;
        let request_types = RequestType(flag_bits(arg(a, 1, "request_types")?, "RequestType")?);
        let cb = arg(a, 2, "callback")?.clone();
        let transports = match a.get(3, "transports") {
            Some(t) => TransportType(flag_bits(t, "TransportType")?),
            None => TransportType::all(),
        };
        let wrap_result = match a.get(4, "wrap_result") {
            Some(v) => v.is_true()?,
            None => true,
        };
        let api_cb = callback(move |req: WebRequest| call_endpoint(cb.clone(), req));
        server()
            .app()
            .register_debug_endpoint(&endpoint, request_types, api_cb, transports, wrap_result)
            .map_err(|e| server_error(&e.message, e.status_code))?;
        Ok(none())
    });
}

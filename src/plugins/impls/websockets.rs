// moonraker.components.websockets.WebsocketManager and the client
// connections (BaseRemoteConnection / WebSocket) for Python components
use super::*;
use crate::components::websockets::{RemoteConnection, WebsocketManager};
use crate::plugins::futures::rust_future;

macro_rules! wsm {
    ($m:literal) => {
        concat!("moonraker.components.websockets.WebsocketManager.", $m)
    };
}

macro_rules! conn {
    ($m:literal) => {
        concat!("moonraker.common.BaseRemoteConnection.", $m)
    };
}

/// a connection's Python object (the same one events pass)
fn conn_object(c: Rc<RemoteConnection>) -> PyResult<PyRef> {
    let t: Rc<dyn crate::common::ApiTransport> = c;
    match t.py_object() {
        Some((rust, module, class)) => objects::wrap_class(rust, module, class),
        None => Ok(none()),
    }
}

fn conn_list(v: Vec<Rc<RemoteConnection>>) -> PyResult<PyRef> {
    let list = lookup("builtins", "list")?.call(&[])?;
    for c in v {
        list.call_method("append", &[&conn_object(c)?])?;
    }
    Ok(list)
}

fn this(a: &Args) -> PyResult<Rc<RemoteConnection>> {
    a.this::<RemoteConnection>()
}

fn params(a: &Args, i: usize) -> PyResult<Option<Value>> {
    match a.get(i, "params") {
        Some(p) if !p.is_none() => Ok(Some(from_py(p)?)),
        _ => Ok(None),
    }
}

pub fn register(r: &mut Registry) {
    r.method(wsm!("register_notification"), |a| {
        let event = arg(a, 0, "event_name")?.as_string()?;
        let opt = |i, n| -> PyResult<Option<String>> {
            match a.get(i, n) {
                Some(v) if !v.is_none() => Ok(Some(v.as_string()?)),
                _ => Ok(None),
            }
        };
        let (notify, etype) = (opt(1, "notify_name")?, opt(2, "event_type")?);
        a.this::<WebsocketManager>()?.register_notification(&event, notify.as_deref(), etype.as_deref());
        Ok(none())
    });
    r.method(wsm!("notify_clients"), |a| {
        let name = arg(a, 0, "name")?.as_string()?;
        let data = match a.get(1, "data") {
            Some(d) => match from_py(d)? {
                Value::Array(v) => v,
                Value::Null => vec![],
                other => vec![other],
            },
            None => vec![],
        };
        let mask: Vec<u64> = match a.get(2, "mask") {
            Some(m) => match from_py(m)? {
                Value::Array(v) => v.iter().filter_map(|x| x.as_u64()).collect(),
                _ => vec![],
            },
            None => vec![],
        };
        a.this::<WebsocketManager>()?.notify_clients(&name, data, &mask);
        Ok(none())
    });
    r.method(wsm!("get_count"), |a| Ok(py_int(a.this::<WebsocketManager>()?.get_count() as i64)));
    r.method(wsm!("has_socket"), |a| Ok(py_bool(a.this::<WebsocketManager>()?.has_socket(arg(a, 0, "ws_id")?.as_i64()? as u64))));
    r.method(wsm!("get_client"), |a| match a.this::<WebsocketManager>()?.get_client(arg(a, 0, "uid")?.as_i64()? as u64) {
        Some(c) => conn_object(c),
        None => Ok(none()),
    });
    r.method(wsm!("get_client_ws"), |a| match a.this::<WebsocketManager>()?.get_client_ws(arg(a, 0, "ws_id")?.as_i64()? as u64) {
        Some(c) => conn_object(c),
        None => Ok(none()),
    });
    r.method(wsm!("get_clients_by_type"), |a| conn_list(a.this::<WebsocketManager>()?.get_clients_by_type(&arg(a, 0, "client_type")?.as_string()?)));
    r.method(wsm!("get_clients_by_name"), |a| conn_list(a.this::<WebsocketManager>()?.get_clients_by_name(&arg(a, 0, "name")?.as_string()?)));
    r.method(wsm!("get_unidentified_clients"), |a| conn_list(a.this::<WebsocketManager>()?.get_unidentified_clients()));

    // a connection
    r.method(conn!("uid"), |a| Ok(py_int(this(a)?.uid() as i64)));
    r.method(conn!("hostname"), |a| Ok(py_str(&this(a)?.hostname())));
    r.method(conn!("start_time"), |a| Ok(py_float(this(a)?.connected_time())));
    r.method(conn!("identified"), |a| Ok(py_bool(this(a)?.identified())));
    r.method(conn!("need_auth"), |a| Ok(py_bool(this(a)?.need_auth())));
    r.method(conn!("client_data"), |a| {
        let cd: serde_json::Map<String, Value> = this(a)?.client_data().into_iter().map(|(k, v)| (k, Value::String(v))).collect();
        to_py(&Value::Object(cd))
    });
    r.method(conn!("user_info"), |a| match this(a)?.get_user_info() {
        Some(u) => super::application::user_info(&u),
        None => Ok(none()),
    });
    r.method(conn!("transport_type"), |a| {
        let t = lookup("moonraker.common", "TransportType")?;
        let name = if this(a)?.kind == crate::components::websockets::ConnKind::UnixSocket { "INTERNAL" } else { "WEBSOCKET" };
        t.getattr(name)
    });
    r.method("moonraker.components.websockets.WebSocket.ip_addr", |a| match this(a)?.ip() {
        Some(ip) => lookup("moonraker.utils", "parse_ip_address")?.call(&[&py_str(&ip.to_string())]),
        None => Ok(none()),
    });
    r.method("moonraker.components.websockets.WebSocket.hostname", |a| Ok(py_str(&this(a)?.hostname())));
    r.method(conn!("queue_message"), |a| {
        let msg = arg(a, 0, "message")?;
        let c = this(a)?;
        if msg.is_instance(&lookup("builtins", "dict")?)? {
            c.queue_message(&from_py(msg)?);
        } else if msg.is_instance(&lookup("builtins", "bytes")?)? {
            c.queue_message_str(msg.call_method("decode", &[])?.as_string()?);
        } else {
            c.queue_message_str(msg.as_string()?);
        }
        Ok(none())
    });
    r.method(conn!("send_notification"), |a| {
        let name = arg(a, 0, "name")?.as_string()?;
        let data = match from_py(arg(a, 1, "data")?)? {
            Value::Array(v) => v,
            other => vec![other],
        };
        this(a)?.send_notification(&name, data);
        Ok(none())
    });
    r.method(conn!("call_method"), |a| {
        let method = arg(a, 0, "method")?.as_string()?;
        this(a)?.call_method(&method, params(a, 1)?);
        Ok(none())
    });
    r.method(conn!("call_method_with_response"), |a| {
        let method = arg(a, 0, "method")?.as_string()?;
        let p = params(a, 1)?;
        let c = this(a)?;
        rust_future(async move {
            c.call_method_with_response(&method, p).await.and_then(|v| Ok(v)).map_err(|e| server_error(&e.message, e.status_code)).and_then(|v| to_py(&v))
        })
    });
    r.method(conn!("close_socket"), |a| {
        let code = arg(a, 0, "code")?.as_i64()? as u16;
        let reason = arg(a, 1, "reason")?.as_string()?;
        this(a)?.close_socket(code, &reason);
        Ok(none())
    });
}

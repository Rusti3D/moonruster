// moonraker.components.klippy_apis.KlippyAPI for Python components. Its
// request methods are Moonraker's own code (mirror_verbatim.txt: run_gcode,
// start_print, pause_print, query_objects, ...) over the primitive here,
// _send_klippy_request, so their logging, events and result checks are
// Moonraker's. subscribe_objects shares Moonruster's host subscription and
// status callbacks.
use super::*;
use crate::common::ServerError;
use crate::components::klippy_apis::{KlippyApi, SubCallback};
use crate::components::klippy_connection::KlippyConnection;
use crate::plugins::futures::{await_py, is_awaitable, rust_future};
use crate::server::server;
use serde_json::Map;

macro_rules! api {
    ($m:literal) => {
        concat!("moonraker.components.klippy_apis.KlippyAPI.", $m)
    };
}

fn arg<'a>(a: &'a Args, i: usize, n: &str) -> PyResult<&'a PyRef> {
    a.get(i, n).ok_or_else(|| py_error("TypeError", format!("missing argument '{}'", n)))
}

/// An optional `default` argument (Sentinel.MISSING: none)
fn default_arg(a: &Args, i: usize) -> PyResult<Option<PyRef>> {
    match a.get(i, "default") {
        Some(d) if !is_missing(d)? => Ok(Some(d.clone())),
        _ => Ok(None),
    }
}

fn to_err(e: ServerError) -> PyError {
    server_error(&e.message, e.status_code)
}

/// EventLoop.register_callback's wrapper for a callback Moonruster calls:
/// call it, await what is awaitable, log an exception as "Error Running
/// Callback" (eventloop.py _wrapper)
fn flex_callback(cb: &PyRef, args: &[&PyRef]) -> Option<futures::future::LocalBoxFuture<'static, ()>> {
    let log_err = |e: &PyError| {
        let msg = format!("Error Running Callback\n{}", e.traceback);
        crate::loghelper::emit(log::Level::Error, "eventloop.py", "_wrapper", 100, &msg);
    };
    let ret = match cb.call(args) {
        Ok(r) => r,
        Err(e) => {
            log_err(&e);
            return None;
        }
    };
    match is_awaitable(&ret) {
        Ok(true) => Some(Box::pin(async move {
            if let Err(e) = await_py(ret).await {
                if e.type_name != "CancelledError" {
                    log_err(&e);
                }
            }
        })),
        _ => None,
    }
}

pub fn register(r: &mut Registry) {
    r.method(api!("_send_klippy_request"), |a| {
        let method = arg(a, 0, "method")?.as_string()?;
        let params = from_py(arg(a, 1, "params")?)?;
        let default = default_arg(a, 2)?;
        if a.get(3, "transport").is_some_and(|t| !t.is_none()) {
            return Err(py_error("NotImplementedError", "moonruster: _send_klippy_request with a transport".into()));
        }
        let api = a.this::<KlippyApi>()?;
        rust_future(async move {
            match api.send_klippy_request(&method, params, None).await {
                Ok(v) => to_py(&v),
                Err(_) if default.is_some() => Ok(default.unwrap()),
                Err(e) => Err(to_err(e)),
            }
        })
    });
    r.method(api!("subscribe_objects"), |a| {
        let objects = match from_py(arg(a, 0, "objects")?)? {
            Value::Object(m) => m,
            _ => return Err(py_error("AttributeError", "'objects' has no attribute 'items'".into())),
        };
        let cb: Option<SubCallback> = match a.get(1, "callback") {
            Some(c) if !c.is_none() => {
                let c = c.clone();
                Some(Rc::new(move |status: Map<String, Value>, eventtime: f64| match to_py(&Value::Object(status)) {
                    Ok(st) => flex_callback(&c, &[&st, &py_float(eventtime)]),
                    Err(e) => {
                        log::error!("moonruster: status callback\n{}", e.traceback);
                        None
                    }
                }))
            }
            _ => None,
        };
        let default = default_arg(a, 2)?;
        let api = a.this::<KlippyApi>()?;
        rust_future(async move {
            match api.subscribe_objects(&objects, cb).await {
                Ok(v) => to_py(&Value::Object(v)),
                Err(_) if default.is_some() => Ok(default.unwrap()),
                Err(e) => Err(to_err(e)),
            }
        })
    });
    r.attr(api!("version"), |o| to_py(&objects::unwrap_as::<KlippyApi>(o)?.version), None);
    r.attr(api!("klippy"), |_| objects::wrap(server().klippy()), None);
    r.attr(api!("server"), |_| objects::wrap(server()), None);
    r.attr(api!("eventloop"), |_| super::server::event_loop(), None);
    r.method("moonraker.components.klippy_connection.KlippyConnection.wait_started", |a| {
        let timeout = match a.get(0, "timeout") {
            Some(t) => t.as_f64()?,
            None => 20.0,
        };
        let kc = a.this::<KlippyConnection>()?;
        rust_future(async move { Ok(py_bool(kc.wait_started(timeout).await)) })
    });
}

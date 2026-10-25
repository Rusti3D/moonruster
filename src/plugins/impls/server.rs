// moonraker.server.Server for Python components
use super::*;
use crate::plugins::component::PyComponent;
use crate::plugins::native;
use crate::plugins::mirror_ids::COMPONENT_CLASSES;
use crate::plugins::futures::{await_py, rust_future};
use crate::server::{server, EventArgs, EventObject, LifecycleDyn};
use futures::future::LocalBoxFuture;
use std::any::Any;

macro_rules! name {
    ($m:literal) => {
        concat!("moonraker.server.Server.", $m)
    };
}

/// A component as Python sees it: a Python component's own object, a
/// built-in one as an object of the class Moonraker's load_component returns
pub fn component_object(name: &str, comp: Rc<dyn Any>) -> PyResult<PyRef> {
    if let Ok(py) = comp.clone().downcast::<PyComponent>() {
        return Ok(py.obj.clone());
    }
    if let Some(py) = objects::existing(&comp) {
        return Ok(py);
    }
    match COMPONENT_CLASSES.iter().find(|(n, _, _)| *n == name) {
        Some((_, module, class)) => objects::wrap_class(comp, module, class),
        None => Err(py_error("TypeError", format!("moonruster: component {} has no Moonraker class yet", name))),
    }
}

/// The singleton EventLoop components get from get_event_loop(): Moonraker's
/// own class (verbatim mirror) on the asyncio loop
pub(crate) fn event_loop() -> PyResult<PyRef> {
    thread_local! {
        static EL: std::cell::RefCell<Option<PyRef>> = const { std::cell::RefCell::new(None) };
    }
    if let Some(el) = EL.with(|e| e.borrow().clone()) {
        return Ok(el);
    }
    let el = lookup("moonraker.eventloop", "EventLoop")?.call(&[])?;
    EL.with(|e| *e.borrow_mut() = Some(el.clone()));
    Ok(el)
}

/// Moonraker's object of an event's object argument
fn event_object(o: &EventObject) -> PyResult<PyRef> {
    match o {
        EventObject::Transport(t) => match t.clone().py_object() {
            Some((rust, module, class)) => objects::wrap_class(rust, module, class),
            None => Ok(none()),
        },
        EventObject::KlippyState(state, msg) => {
            lookup("moonraker.common", "KlippyState")?.call_method("from_string", &[&py_str(state), &py_str(msg)])
        }
        EventObject::JobEvent(name) => lookup("moonraker.common", "JobEvent")?.call_method("from_string", &[&py_str(name)]),
        EventObject::User(Some(u)) => application::user_info(u),
        EventObject::User(None) => Ok(none()),
    }
}

/// An event's arguments for a Python handler: as a Python component sent
/// them, else Moonruster's values with Moonraker's object in front
fn event_py_args(a: &EventArgs) -> PyResult<Vec<PyRef>> {
    if let Some(t) = &a.py {
        let n = t.call_method("__len__", &[])?.as_i64()?;
        return (0..n).map(|i| t.call_method("__getitem__", &[&py_int(i)])).collect();
    }
    let mut out = a.values.iter().map(to_py).collect::<PyResult<Vec<_>>>()?;
    if let (Some(o), Some(first)) = (&a.object, out.first_mut()) {
        *first = event_object(o)?;
    }
    Ok(out)
}

/// Server._process_event for a Python handler: call it, log what it raises,
/// and await what it returns; an exception of the awaitable is logged with
/// its traceback at info level, as Moonraker's gather does
fn call_event_handler(cb: &PyRef, event: &str, args: &EventArgs) -> Option<LocalBoxFuture<'static, ()>> {
    let ret = event_py_args(args).and_then(|v| cb.call(&v.iter().collect::<Vec<_>>()));
    let ret = match ret {
        Ok(r) if r.is_none() => return None,
        Ok(r) => r,
        Err(e) => {
            let msg = format!("Error processing callback in event {}\n{}", event, e.traceback);
            crate::loghelper::emit(log::Level::Error, "server.py", "_process_event", 408, &msg);
            return None;
        }
    };
    let event = event.to_string();
    Some(Box::pin(async move {
        if let Err(e) = await_py(ret).await {
            // gather(return_exceptions=True) logs Exceptions, not CancelledError
            let is_exc = match (&e.value, lookup("builtins", "Exception")) {
                (Some(v), Ok(c)) => v.is_instance(&c).unwrap_or(false),
                _ => true,
            };
            if is_exc {
                let msg = format!("\nError processing callback in event {}\n{}\n", event, e.traceback);
                crate::loghelper::emit(log::Level::Info, "server.py", "_process_event", 422, &msg);
            }
        }
    }))
}

thread_local! {
    // Server.pip_recovery_attempted (used by Moonraker's own try_pip_recovery)
    static PIP_RECOVERY_ATTEMPTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn register(r: &mut Registry) {
    r.attr(
        name!("pip_recovery_attempted"),
        |_| Ok(py_bool(PIP_RECOVERY_ATTEMPTED.with(|c| c.get()))),
        Some(|_, v| {
            let b = v.is_true()?;
            PIP_RECOVERY_ATTEMPTED.with(|c| c.set(b));
            Ok(())
        }),
    );
    r.method(name!("get_app_args"), |_| to_py(&Value::Object(server().get_app_args())));
    r.method(name!("get_host_info"), |_| to_py(&server().get_host_info()));
    r.method(name!("is_running"), |_| Ok(py_bool(server().is_running())));
    r.method(name!("get_event_loop"), |_| event_loop());
    r.attr(name!("event_loop"), |_| event_loop(), None);
    r.method(name!("lookup_component"), |a| {
        let name = arg(a, 0, "component_name")?.as_string()?;
        match server().component_any(&name) {
            Some(c) => component_object(&name, c),
            None => match a.get(1, "default") {
                Some(d) if !is_missing(d)? => Ok(d.clone()),
                _ => Err(server_error(&format!("Component ({}) not found", name), 400)),
            },
        }
    });
    // SpoolManager's attributes components read (mmu_server, additional_pre_print_checks)
    r.attr("moonraker.components.spoolman.SpoolManager.spoolman_url", |o| {
        Ok(py_str(&objects::unwrap_as::<crate::components::spoolman::SpoolManager>(o)?.spoolman_url))
    }, None);
    r.attr("moonraker.components.spoolman.SpoolManager.database", |_| component_object("database", server().component_any("database").expect("database is core")), None);
    r.attr("moonraker.components.spoolman.SpoolManager.http_client", |_| component_object("http_client", server().component_any("http_client").expect("http_client is core")), None);
    r.attr("moonraker.components.spoolman.SpoolManager.server", |_| objects::wrap(server()), None);
    r.attr("moonraker.components.spoolman.SpoolManager.eventloop", |_| event_loop(), None);
    r.method(name!("register_notification"), |a| {
        let event = arg(a, 0, "event_name")?.as_string()?;
        let notify = match a.get(1, "notify_name") {
            Some(n) if !n.is_none() => Some(n.as_string()?),
            _ => None,
        };
        server().register_notification(&event, notify.as_deref());
        Ok(none())
    });
    r.method(name!("register_event_handler"), |a| {
        let event = arg(a, 0, "event")?.as_string()?;
        let cb = arg(a, 1, "callback")?.clone();
        let ev = event.clone();
        server().register_event_handler_args(&event, Rc::new(move |args: &EventArgs| call_event_handler(&cb, &ev, args)));
        Ok(none())
    });
    // send_event is Moonraker's own (mirror_verbatim.txt): it schedules this
    // with event_loop.register_callback, so the handlers run in a task of
    // their own after the sender's step, as there
    r.method(name!("_process_event"), |a| {
        let fut = arg(a, 0, "fut")?.clone();
        let event = arg(a, 1, "event")?.as_string()?;
        let rest = a.args.get(2..).unwrap_or_default();
        // Rust handlers get the values (an object JSON has no form for is null)
        let values = rest.iter().map(|v| from_py(v).unwrap_or(Value::Null)).collect();
        let py = tuple(&rest.iter().collect::<Vec<_>>());
        let done = server().process_event(&event, &EventArgs { values, object: None, py: Some(py) });
        rust_future(async move {
            done.await;
            if !fut.call_method("done", &[])?.is_true()? {
                fut.call_method("set_result", &[&none()])?;
            }
            Ok(none())
        })
    });
    r.method(name!("register_remote_method"), |a| {
        let method = arg(a, 0, "method_name")?.as_string()?;
        let remote = python_remote_method(arg(a, 1, "cb")?.clone());
        server().register_remote_method(&method, remote).map_err(|e| server_error(&e.message, e.status_code))?;
        Ok(none())
    });
    r.method(name!("register_component"), |a| {
        let name = arg(a, 0, "component_name")?.as_string()?;
        register_object(&name, arg(a, 1, "component")?)
    });
    r.method(name!("is_configured"), |_| Ok(py_bool(server().is_configured())));
    // Moonraker's app_args, with its get_software_info() keys the startup
    // read from the Moonraker tree (unofficial_components: pip recovery)
    r.attr(
        name!("app_args"),
        |_| {
            let args = to_py(&Value::Object(server().get_app_args()))?;
            if !args.call_method("__contains__", &[&py_str("unofficial_components")])?.is_true()? {
                let uc = lookup("_mr", "_unofficial")?;
                let uc = if uc.is_none() { lookup("builtins", "list")?.call(&[])? } else { uc };
                args.call_method("__setitem__", &[&py_str("unofficial_components"), &uc])?;
            }
            Ok(args)
        },
        None,
    );
    // views of Moonruster's component table and failed list, as Moonraker's
    // own load_component uses them (prelude TableView)
    r.attr(
        name!("components"),
        |o| {
            lookup("_mr", "TableView")?.call(&[
                &native::bound_method(o, |a| Ok(py_bool(server().has_component(&arg(a, 0, "name")?.as_string()?))))?,
                &native::bound_method(o, |a| {
                    let name = arg(a, 0, "name")?.as_string()?;
                    match server().component_any(&name) {
                        Some(c) => component_object(&name, c),
                        None => Err(py_error("KeyError", name)),
                    }
                })?,
                &native::bound_method(o, |a| {
                    let name = arg(a, 0, "name")?.as_string()?;
                    let obj = arg(a, 1, "value")?;
                    // a built-in the module's load_component registered already
                    if let Some(c) = server().component_any(&name) {
                        if component_object(&name, c)?.as_ptr() == obj.as_ptr() {
                            return Ok(none());
                        }
                    }
                    register_object(&name, obj)
                })?,
                &native::bound_method(o, |_| to_py(&Value::from(server().component_names())))?,
            ])
        },
        None,
    );
    r.attr(
        name!("failed_components"),
        |o| {
            lookup("_mr", "TableView")?.call(&[
                &native::bound_method(o, |a| {
                    let name = arg(a, 0, "name")?.as_string()?;
                    Ok(py_bool(server().failed_components().contains(&name)))
                })?,
                &native::bound_method(o, |a| Ok(arg(a, 0, "name")?.clone()))?,
                &native::bound_method(o, |a| {
                    server().set_failed_component(&arg(a, 0, "name")?.as_string()?);
                    Ok(none())
                })?,
                &native::bound_method(o, |_| to_py(&Value::from(server().failed_components())))?,
            ])
        },
        None,
    );
    // load_component(config) of each built-in component's module, as
    // Moonraker's load_component calls it (for a built-in a component loads:
    // `server.load_component(config, "spoolman", None)`)
    for name in crate::components::COMPONENT_NAMES {
        for qual in [format!("moonraker.components.{}.load_component", name), format!("moonraker.components.{}.{}.load_component", name, name)] {
            if crate::plugins::mirror_ids::IDS.contains(&qual.as_str()) {
                r.method(Box::leak(qual.into_boxed_str()), |a| {
                    let config = objects::unwrap_as::<crate::confighelper::ConfigHelper>(arg(a, 0, "config")?)?;
                    let name = a.qualname.trim_end_matches(".load_component").rsplit('.').next().unwrap_or("");
                    let comp = server().construct_component(&config, name).map_err(super::config::config_error)?;
                    component_object(name, comp)
                });
            }
        }
    }
}

/// A component's remote method: Klippy's call runs as Moonraker runs it,
/// event_loop.register_callback(cb, **params)
pub fn python_remote_method(cb: PyRef) -> crate::components::klippy_connection::RemoteMethod {
    Rc::new(move |params| {
        let r = to_py(&Value::Object(params)).and_then(|kw| event_loop()?.getattr("register_callback")?.call_kw(&[&cb], &kw));
        if let Err(e) = r {
            log::error!("moonruster: remote method callback failed\n{}", e.traceback);
        }
        None
    })
}

/// Server.register_component / components[name] = component: a built-in's
/// Python object registers the Rust component; a Python object is a
/// component of its own, with its lifecycle (Moonraker calls component_init
/// / close on every registered name)
fn register_object(name: &str, obj: &PyRef) -> PyResult<PyRef> {
    let (comp, lc): (Rc<dyn Any>, Option<Rc<dyn LifecycleDyn>>) = match objects::unwrap(obj) {
        Ok(rust) => (rust, None),
        Err(_) => {
            let py = PyComponent::new(obj.clone());
            (py.clone() as Rc<dyn Any>, Some(py as Rc<dyn LifecycleDyn>))
        }
    };
    server().register_component_any(name, comp, lc).map_err(|e| server_error(&e.message, e.status_code))?;
    Ok(none())
}

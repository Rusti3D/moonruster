// moonraker.server.Server for Python components
use super::*;
use crate::plugins::component::PyComponent;
use crate::plugins::mirror_ids::COMPONENT_CLASSES;
use crate::server::{server, LifecycleDyn};
use std::any::Any;

macro_rules! name {
    ($m:literal) => {
        concat!("moonraker.server.Server.", $m)
    };
}

fn arg<'a>(a: &'a Args, i: usize, n: &str) -> PyResult<&'a PyRef> {
    a.get(i, n).ok_or_else(|| py_error("TypeError", format!("missing argument '{}'", n)))
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
fn event_loop() -> PyResult<PyRef> {
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
    r.method(name!("get_event_loop"), |_| event_loop());
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
    r.method(name!("register_component"), |a| {
        let name = arg(a, 0, "component_name")?.as_string()?;
        let obj = arg(a, 1, "component")?.clone();
        // a built-in component's Python object registers the Rust component;
        // a Python object is a component of its own, with its lifecycle
        // (Moonraker calls component_init / close on every registered name)
        let (comp, lc): (Rc<dyn Any>, Option<Rc<dyn LifecycleDyn>>) = match objects::unwrap(&obj) {
            Ok(rust) => (rust, None),
            Err(_) => {
                let py = PyComponent::new(obj);
                (py.clone() as Rc<dyn Any>, Some(py as Rc<dyn LifecycleDyn>))
            }
        };
        server()
            .register_component_any(&name, comp, lc)
            .map_err(|e| server_error(&e.message, e.status_code))?;
        Ok(none())
    });
}

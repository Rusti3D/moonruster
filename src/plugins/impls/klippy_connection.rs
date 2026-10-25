// moonraker.components.klippy_connection.KlippyConnection for Python
// components: its state and the calls components make
// (register_remote_method, wait_connected, is_ready ...)
use super::*;
use crate::components::klippy_connection::KlippyConnection;
use crate::plugins::futures::rust_future;
use crate::server::server;

macro_rules! kc {
    ($m:literal) => {
        concat!("moonraker.components.klippy_connection.KlippyConnection.", $m)
    };
}

fn this(a: &Args) -> PyResult<Rc<KlippyConnection>> {
    a.this::<KlippyConnection>()
}

fn path(p: &std::path::Path) -> PyResult<PyRef> {
    lookup("pathlib", "Path")?.call(&[&py_str(&p.to_string_lossy())])
}

pub fn register(r: &mut Registry) {
    // properties (the mirror calls them as methods)
    r.method(kc!("klippy_apis"), |_| super::server::component_object("klippy_apis", server().component_any("klippy_apis").expect("klippy_apis is core")));
    r.method(kc!("state"), |a| {
        let kc = this(a)?;
        let state = kc.state();
        lookup("moonraker.common", "KlippyState")?.call_method("from_string", &[&py_str(state.as_str()), &py_str(&kc.state_message_for(state))])
    });
    r.method(kc!("state_message"), |a| Ok(py_str(&this(a)?.state_message())));
    r.method(kc!("klippy_info"), |a| to_py(&this(a)?.klippy_info()));
    r.method(kc!("missing_requirements"), |a| to_py(&Value::from(this(a)?.missing_requirements())));
    r.method(kc!("peer_credentials"), |a| to_py(&Value::Object(this(a)?.peer_credentials())));
    r.method(kc!("service_info"), |a| to_py(&this(a)?.service_info()));
    r.method(kc!("unit_name"), |a| Ok(py_str(&this(a)?.unit_name())));
    r.method(kc!("path"), |a| path(&this(a)?.path()));
    r.method(kc!("executable"), |a| path(&this(a)?.executable()));
    // request(web_request): a component's own WebRequest to Klippy
    r.method(kc!("request"), |a| {
        let req = super::application::web_request_from_py(arg(a, 0, "web_request")?)?;
        let kc = this(a)?;
        rust_future(async move { to_py(&kc.request(req).await.map_err(|e| server_error(&e.message, e.status_code))?) })
    });
    r.method(kc!("remove_subscription"), |a| {
        if let Some(t) = super::transport::transport_from_py(arg(a, 0, "conn")?)? {
            this(a)?.remove_subscription(t.transport_id());
        }
        Ok(none())
    });
    r.method(kc!("rollover_log"), |a| {
        let kc = this(a)?;
        rust_future(async move { kc.rollover_log().await.map(|_| none()).map_err(|e| server_error(&e.message, e.status_code)) })
    });
    r.method(kc!("is_connected"), |a| Ok(py_bool(this(a)?.is_connected())));
    r.method(kc!("is_ready"), |a| Ok(py_bool(this(a)?.is_ready())));
    r.method(kc!("is_printing"), |a| Ok(py_bool(this(a)?.is_printing())));
    r.method(kc!("get_subscription_cache"), |a| to_py(&Value::Object(this(a)?.get_subscription_cache())));
    r.method(kc!("wait_connected"), |a| {
        let kc = this(a)?;
        rust_future(async move { Ok(py_bool(kc.wait_connected().await)) })
    });
    r.method(kc!("register_remote_method"), |a| {
        let method = a.get(0, "method_name").ok_or_else(|| py_error("TypeError", "missing argument 'method_name'".into()))?.as_string()?;
        let cb = a.get(1, "cb").ok_or_else(|| py_error("TypeError", "missing argument 'cb'".into()))?.clone();
        let need_reg = match a.get(2, "need_klippy_reg") {
            Some(v) => v.is_true()?,
            None => true,
        };
        this(a)?
            .register_remote_method(&method, super::server::python_remote_method(cb), need_reg)
            .map_err(|e| server_error(&e.message, e.status_code))?;
        Ok(none())
    });
    r.method(kc!("unregister_method"), |a| {
        let method = a.get(0, "method_name").ok_or_else(|| py_error("TypeError", "missing argument 'method_name'".into()))?.as_string()?;
        this(a)?.unregister_method(&method);
        Ok(none())
    });
}

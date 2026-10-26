// moonraker.components.machine.Machine for Python components
use super::*;
use crate::components::machine::{Machine, SudoCallback};
use crate::plugins::futures::{await_py, is_awaitable, rust_future};

macro_rules! mc {
    ($m:literal) => {
        concat!("moonraker.components.machine.Machine.", $m)
    };
}

fn this(a: &Args) -> PyResult<Rc<Machine>> {
    a.this::<Machine>()
}

fn err(e: crate::common::ServerError) -> PyError {
    server_error(&e.message, e.status_code)
}

pub fn register(r: &mut Registry) {
    r.method(mc!("validate_installation"), |a| {
        let m = a.this::<crate::components::machine::Machine>()?;
        crate::plugins::futures::rust_future(async move { Ok(py_bool(m.validate_installation().await)) })
    });
    r.method(mc!("sudo_password"), |a| Ok(a.this::<crate::components::machine::Machine>()?.sudo_password().map(|p| py_str(&p)).unwrap_or_else(none)));
    r.method(mc!("sudo_password.setter"), |a| {
        let pw = match opt(a, 0, "pwd") {
            Some(p) => Some(p.as_string()?),
            None => None,
        };
        a.this::<crate::components::machine::Machine>()?.set_sudo_password_value(pw);
        Ok(none())
    });
    r.method(mc!("log_service_info"), |a| {
        a.this::<crate::components::machine::Machine>()?.log_service_info(&from_py(arg(a, 0, "svc_info")?)?);
        Ok(none())
    });
    r.method(mc!("update_usb_ids"), |a| {
        let force = bool_or(a, 0, "force", false)?;
        let m = a.this::<crate::components::machine::Machine>()?;
        crate::plugins::futures::rust_future(async move {
            m.update_usb_ids(force).await;
            Ok(none())
        })
    });
    // the system's libcamera python bindings' cameras (none without them)
    r.method(mc!("get_libcamera_devices"), |a| {
        let libcam = a.this::<crate::components::machine::Machine>()?.has_libcamera();
        to_py(&crate::components::machine::Machine::libcamera_devices(libcam))
    });
    // properties
    r.method(mc!("public_ip"), |a| Ok(py_str(&this(a)?.public_ip().unwrap_or_default())));
    r.method(mc!("unit_name"), |a| Ok(py_str(&this(a)?.unit_name())));
    r.method(mc!("sudo_requested"), |a| Ok(py_bool(this(a)?.sudo_requested())));
    r.method(mc!("linux_user"), |a| Ok(py_str(&this(a)?.linux_user())));
    r.method(mc!("sudo_request_messages"), |a| to_py(&Value::from(this(a)?.sudo_request_messages())));
    r.method(mc!("is_service_allowed"), |a| Ok(py_bool(this(a)?.is_service_allowed(&arg(a, 0, "service")?.as_string()?))));
    r.method(mc!("validation_enabled"), |a| Ok(py_bool(this(a)?.validation_enabled())));
    r.method(mc!("is_inside_container"), |a| Ok(py_bool(this(a)?.is_inside_container())));
    r.method(mc!("get_provider_type"), |a| Ok(py_str(&this(a)?.get_provider_type())));
    r.method(mc!("get_moonraker_service_info"), |a| to_py(&this(a)?.get_moonraker_service_info()));
    r.method(mc!("get_system_info"), |a| to_py(&Value::Object(this(a)?.get_system_info())));
    r.method(mc!("wait_for_init"), |a| {
        let timeout = match a.get(0, "timeout") {
            Some(t) if !t.is_none() => Some(t.as_f64()?),
            _ => None,
        };
        let m = this(a)?;
        rust_future(async move {
            m.wait_for_init(timeout).await;
            Ok(none())
        })
    });
    r.method(mc!("do_service_action"), |a| {
        let action = arg(a, 0, "action")?.as_string()?;
        let name = arg(a, 1, "service_name")?.as_string()?;
        let m = this(a)?;
        rust_future(async move { m.do_service_action(&action, &name).await.map(|_| none()).map_err(err) })
    });
    r.method(mc!("restart_moonraker_service"), |a| {
        this(a)?.restart_moonraker_service();
        Ok(none())
    });
    r.method(mc!("register_sudo_request"), |a| {
        let cb = arg(a, 0, "callback")?.clone();
        let message = arg(a, 1, "message")?.as_string()?;
        // the callback returns (message, success) or an awaitable of it
        let sudo_cb: SudoCallback = Rc::new(move || {
            let cb = cb.clone();
            Box::pin(async move {
                let r: PyResult<(String, bool)> = async {
                    let mut ret = cb.call(&[])?;
                    if is_awaitable(&ret)? {
                        ret = await_py(ret).await?;
                    }
                    let msg = ret.call_method("__getitem__", &[&py_int(0)])?.as_string()?;
                    let ok = ret.call_method("__getitem__", &[&py_int(1)])?.is_true()?;
                    Ok((msg, ok))
                }
                .await;
                r.map_err(|e| crate::common::ServerError::new(e.message, 500))
            })
        });
        this(a)?.register_sudo_request(sudo_cb, &message);
        Ok(none())
    });
    r.method(mc!("check_sudo_access"), |a| {
        let cmds: Vec<String> = match a.get(0, "cmds") {
            Some(c) => match from_py(c)? {
                Value::Array(v) => v.iter().map(crate::pyfmt::py_str).collect(),
                _ => vec![],
            },
            None => vec![],
        };
        let m = this(a)?;
        rust_future(async move {
            let refs: Vec<&str> = cmds.iter().map(|s| s.as_str()).collect();
            Ok(py_bool(m.check_sudo_access(&refs).await))
        })
    });
    r.method(mc!("exec_sudo_command"), |a| {
        let command = arg(a, 0, "command")?.as_string()?;
        let tries = match a.get(1, "tries") {
            Some(t) => t.as_i64()? as usize,
            None => 1,
        };
        let timeout = match a.get(2, "timeout") {
            Some(t) => t.as_f64()?,
            None => 2.0,
        };
        let m = this(a)?;
        rust_future(async move { m.exec_sudo_command(&command, tries, timeout).await.map(|s| py_str(&s)).map_err(err) })
    });
    r.method(mc!("get_public_network"), |a| {
        let m = this(a)?;
        rust_future(async move { to_py(&m.get_public_network().await) })
    });
    r.method(mc!("detect_serial_devices"), |a| {
        let m = this(a)?;
        rust_future(async move { to_py(&m.detect_serial_devices().await) })
    });
    r.method(mc!("detect_usb_devices"), |a| {
        let m = this(a)?;
        rust_future(async move { to_py(&m.detect_usb_devices().await) })
    });
    r.method(mc!("detect_video_devices"), |a| {
        let m = this(a)?;
        rust_future(async move { to_py(&m.detect_video_devices().await) })
    });
    r.method(mc!("query_can_uuids"), |a| {
        let iface = arg(a, 0, "interface")?.as_string()?;
        let m = this(a)?;
        rust_future(async move { m.query_can_uuids(&iface).await.map_err(err).and_then(|v| to_py(&v)) })
    });
}

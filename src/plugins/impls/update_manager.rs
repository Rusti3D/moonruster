// moonraker.components.update_manager: UpdateManager's updater API and the
// updaters themselves (BaseDeploy and its subclasses) for Python components
use super::*;
use crate::components::update_manager::{DeployKind, Deployer, RegisterError, UpdateManager};
use crate::plugins::futures::rust_future;
use std::collections::HashMap;
use std::rc::Weak;

macro_rules! um {
    ($m:literal) => {
        concat!("moonraker.components.update_manager.update_manager.UpdateManager.", $m)
    };
}

/// An updater's Python object
struct Updater(Rc<dyn Deployer>);

thread_local! {
    // one Python object per updater while Python holds it (identity)
    static UPDATERS: std::cell::RefCell<HashMap<usize, Weak<Updater>>> = Default::default();
}

fn err(e: crate::common::ServerError) -> PyError {
    server_error(&e.message, e.status_code)
}

fn updater_object(d: &Rc<dyn Deployer>) -> PyResult<PyRef> {
    let key = Rc::as_ptr(d) as *const () as usize;
    let u = UPDATERS.with(|m| {
        let mut m = m.borrow_mut();
        m.retain(|_, w| w.strong_count() > 0);
        match m.get(&key).and_then(Weak::upgrade) {
            Some(u) => u,
            None => {
                let u = Rc::new(Updater(d.clone()));
                m.insert(key, Rc::downgrade(&u));
                u
            }
        }
    });
    let (module, class) = match d.kind() {
        DeployKind::Base => ("base_deploy", "BaseDeploy"),
        DeployKind::Git => ("git_deploy", "GitDeploy"),
        DeployKind::Net => ("net_deploy", "NetDeploy"),
        DeployKind::Python => ("python_deploy", "PythonDeploy"),
        DeployKind::System => ("system_deploy", "PackageDeploy"),
    };
    objects::wrap_class(u, &format!("moonraker.components.update_manager.{}", module), class)
}

fn this(a: &Args) -> PyResult<Rc<dyn Deployer>> {
    Ok(a.this::<Updater>()?.0.clone())
}

fn text(a: &Args) -> PyResult<String> {
    arg(a, 0, "msg")?.as_string()
}

/// BaseDeploy's public methods, by name
fn deploy_method(name: &str) -> Option<crate::plugins::native::Method> {
    let m: crate::plugins::native::Method = match name {
        "initialize" => |a| {
            let d = this(a)?;
            rust_future(async move { to_py(&Value::Object(d.initialize().await)) })
        },
        "needs_refresh" => |a| Ok(py_bool(this(a)?.base().needs_refresh(bool_or(a, 0, "log_remaining_time", false)?))),
        "get_last_refresh_time" => |a| Ok(py_float(this(a)?.base().last_refresh_time.get())),
        "refresh" => |a| {
            let d = this(a)?;
            rust_future(async move { d.refresh().await.map(|_| none()).map_err(err) })
        },
        "update" => |a| {
            let d = this(a)?;
            rust_future(async move { d.update().await.map(py_bool).map_err(err) })
        },
        "rollback" => |a| {
            let d = this(a)?;
            rust_future(async move { d.rollback().await.map(py_bool).map_err(err) })
        },
        "recover" => |a| {
            let (d, hard, deps) = (this(a)?, bool_or(a, 0, "hard", false)?, bool_or(a, 1, "force_dep_update", false)?);
            rust_future(async move { d.recover(hard, deps).await.map(|_| none()).map_err(err) })
        },
        "get_update_status" => |a| to_py(&this(a)?.get_update_status()),
        "get_persistent_data" => |a| to_py(&Value::Object(this(a)?.get_persistent_data())),
        // log_exc returns the error (the caller raises it)
        "log_exc" => |a| {
            let e = this(a)?.base().log_exc(&text(a)?, bool_or(a, 1, "traceback", true)?);
            Ok(err(e).value.unwrap_or_else(none))
        },
        "log_info" => |a| {
            this(a)?.base().log_info(&text(a)?);
            Ok(none())
        },
        "log_debug" => |a| {
            this(a)?.base().log_debug(&text(a)?);
            Ok(none())
        },
        "notify_status" => |a| {
            this(a)?.base().notify_status(&text(a)?, bool_or(a, 1, "is_complete", false)?);
            Ok(none())
        },
        "close" => |a| {
            let d = this(a)?;
            rust_future(async move {
                d.close().await;
                Ok(none())
            })
        },
        _ => return None,
    };
    Some(m)
}

const DEPLOY_CLASSES: [&str; 6] = [
    "base_deploy.BaseDeploy.",
    "app_deploy.AppDeploy.",
    "git_deploy.GitDeploy.",
    "net_deploy.NetDeploy.",
    "python_deploy.PythonDeploy.",
    "system_deploy.PackageDeploy.",
];

pub fn register(r: &mut Registry) {
    r.method(um!("get_updaters"), |a| {
        let d = lookup("builtins", "dict")?.call(&[])?;
        for (name, u) in a.this::<UpdateManager>()?.get_updaters() {
            d.call_method("__setitem__", &[&py_str(&name), &updater_object(&u)?])?;
        }
        Ok(d)
    });
    r.method(um!("register_updater"), |a| {
        let config = from_py(arg(a, 1, "config")?)?;
        match a.this::<UpdateManager>()?.register_updater(&arg(a, 0, "name")?.as_string()?, config) {
            Ok(()) => Ok(none()),
            Err(RegisterError::Server(e)) => Err(err(e)),
            Err(RegisterError::Config(e)) => Err(super::config::config_error(e)),
            Err(RegisterError::UnknownType(m)) => Err(py_error("ValueError", m)),
        }
    });
    r.method(um!("refresh_updater"), |a| {
        let (u, name, force) = (a.this::<UpdateManager>()?, arg(a, 0, "updater_name")?.as_string()?, bool_or(a, 1, "force", false)?);
        rust_future(async move { u.refresh_updater(&name, force).await.map(|_| none()).map_err(err) })
    });
    // each deploy class's own definition of a method (an override in the
    // mirror is its own name)
    for id in crate::plugins::mirror_ids::IDS {
        let Some(rest) = id.strip_prefix("moonraker.components.update_manager.") else { continue };
        for cls in DEPLOY_CLASSES {
            match rest.strip_prefix(cls) {
                Some("name") => r.attr(id, |o| Ok(py_str(&objects::unwrap_as::<Updater>(o)?.0.base().name)), None),
                Some(m) => {
                    if let Some(m) = deploy_method(m) {
                        r.method(id, m);
                    }
                }
                None => {}
            }
        }
    }
}

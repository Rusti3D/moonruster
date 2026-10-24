// moonraker.confighelper.ConfigHelper for Python components, on
// Moonruster's ConfigHelper (get_py: Moonraker's parsing, errors, bounds and
// recording of defaults)
use super::*;
use crate::confighelper::{Bounds, ConfigError, ConfigHelper, OptKind};

macro_rules! name {
    ($m:literal) => {
        concat!("moonraker.confighelper.ConfigHelper.", $m)
    };
}

/// A ConfigError raised as Moonraker's moonraker.confighelper.ConfigError
pub fn config_error(e: ConfigError) -> PyError {
    match lookup("moonraker.confighelper", "ConfigError").and_then(|c| c.call(&[&py_str(&e.0)])) {
        Ok(exc) => PyError { type_name: "ConfigError".into(), message: e.0, traceback: String::new(), value: Some(exc) },
        Err(err) => err,
    }
}

fn this(a: &Args) -> PyResult<Rc<ConfigHelper>> {
    a.this::<ConfigHelper>()
}

/// A Python default: None is Sentinel.MISSING (required option)
fn default(v: Option<&PyRef>) -> PyResult<Option<Value>> {
    match v {
        None => Ok(None),
        Some(v) if is_missing(v)? => Ok(None),
        Some(v) => Ok(Some(from_py(v)?)),
    }
}

fn opt_f64(v: Option<&PyRef>) -> PyResult<Option<f64>> {
    match v {
        Some(v) if !v.is_none() => Ok(Some(v.as_f64()?)),
        _ => Ok(None),
    }
}

fn get_kind(a: &Args, kind: OptKind, numeric: bool) -> PyResult<PyRef> {
    let cfg = this(a)?;
    let option = a.get(0, "option").ok_or_else(|| py_error("TypeError", "missing option".into()))?.as_string()?;
    let def = default(a.get(1, "default"))?;
    let (bounds, dep_idx) = if numeric {
        (
            Bounds {
                above: opt_f64(a.get(2, "above"))?,
                below: opt_f64(a.get(3, "below"))?,
                minval: opt_f64(a.get(4, "minval"))?,
                maxval: opt_f64(a.get(5, "maxval"))?,
            },
            6,
        )
    } else {
        (Bounds::none(), 2)
    };
    let deprecate = match a.get(dep_idx, "deprecate") {
        Some(d) => d.is_true()?,
        None => false,
    };
    let v = cfg.get_py(kind, &option, def, bounds, deprecate).map_err(config_error)?;
    to_py(&v)
}

pub fn register(r: &mut Registry) {
    r.method(name!("get_server"), |_| objects::wrap(crate::server::server()));
    r.method(name!("get_name"), |a| Ok(py_str(this(a)?.get_name())));
    r.method(name!("get"), |a| get_kind(a, OptKind::Str, false));
    r.method(name!("getint"), |a| get_kind(a, OptKind::Int, true));
    r.method(name!("getfloat"), |a| get_kind(a, OptKind::Float, true));
    r.method(name!("getboolean"), |a| get_kind(a, OptKind::Bool, false));
    r.method(name!("has_option"), |a| {
        let option = a.get(0, "option").ok_or_else(|| py_error("TypeError", "missing option".into()))?.as_string()?;
        Ok(py_bool(this(a)?.has_option(&option)))
    });
    // instance attributes holding configparser's methods in Moonraker
    // (self.has_section = self.config.has_section)
    r.attr(
        name!("has_section"),
        |o| {
            crate::plugins::native::bound_method(o, |a| {
                let section = a.get(0, "section").ok_or_else(|| py_error("TypeError", "missing section".into()))?.as_string()?;
                Ok(py_bool(this(a)?.has_section(&section)))
            })
        },
        None,
    );
    r.attr(
        name!("sections"),
        |o| crate::plugins::native::bound_method(o, |a| to_py(&Value::from(this(a)?.sections()))),
        None,
    );
    r.method(name!("get_options"), |a| {
        let opts = this(a)?.get_options();
        to_py(&Value::Object(opts.into_iter().map(|(k, v)| (k, Value::String(v))).collect()))
    });
    r.method(name!("get_prefix_sections"), |a| {
        let prefix = a.get(0, "prefix").ok_or_else(|| py_error("TypeError", "missing prefix".into()))?.as_string()?;
        to_py(&Value::Array(this(a)?.get_prefix_sections(&prefix).into_iter().map(Value::String).collect()))
    });
    r.method(name!("getsection"), |a| {
        let section = a.get(0, "section").ok_or_else(|| py_error("TypeError", "missing section".into()))?.as_string()?;
        let fallback = match a.get(1, "fallback") {
            Some(f) if !f.is_none() => Some(f.as_string()?),
            _ => None,
        };
        objects::wrap(Rc::new(this(a)?.getsection_fallback(&section, fallback.as_deref())))
    });
}

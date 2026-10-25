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

/// A value as Moonraker records it in `parsed`: JSON types as they are,
/// anything else as str(value)
fn parsed_value(v: &PyRef) -> Value {
    let json = ["NoneType", "int", "float", "bool", "str", "dict", "list"]
        .iter()
        .any(|t| v.getattr("__class__").and_then(|c| c.getattr("__name__")).and_then(|n| n.as_string()).map(|n| n == *t).unwrap_or(false));
    if json {
        if let Ok(j) = from_py(v) {
            return j;
        }
    }
    Value::String(v.str().unwrap_or_default())
}

/// configparser's missing option / section exceptions
fn is_missing_option(e: &PyError) -> bool {
    let is = |n: &str| match (&e.value, lookup("configparser", n)) {
        (Some(v), Ok(c)) => v.is_instance(&c).unwrap_or(false),
        _ => false,
    };
    is("NoOptionError") || is("NoSectionError")
}

pub fn register(r: &mut Registry) {
    // ConfigHelper._get_option(func, option, default, above, below, minval,
    // maxval, deprecate): Moonraker's getlists / getdict / gettemplate ...
    // call it with their parse function
    r.method(name!("_get_option"), |a| {
        let cfg = this(a)?;
        let func = a.get(0, "func").ok_or_else(|| py_error("TypeError", "missing func".into()))?.clone();
        let option = a.get(1, "option").ok_or_else(|| py_error("TypeError", "missing option".into()))?.as_string()?;
        let default = match a.get(2, "default") {
            Some(d) if !is_missing(d)? => Some(d.clone()),
            _ => None,
        };
        let bounds = Bounds {
            above: opt_f64(a.get(3, "above"))?,
            below: opt_f64(a.get(4, "below"))?,
            minval: opt_f64(a.get(5, "minval"))?,
            maxval: opt_f64(a.get(6, "maxval"))?,
        };
        let deprecate = match a.get(7, "deprecate") {
            Some(d) => d.is_true()?,
            None => false,
        };
        let fetch = |section: &str, opt: &str| -> Result<PyRef, Result<String, String>> {
            func.call(&[&py_str(section), &py_str(opt)]).map_err(|e| {
                let msg = e.value.as_ref().and_then(|v| v.str().ok()).unwrap_or(e.message.clone());
                if is_missing_option(&e) {
                    Ok(msg)
                } else {
                    Err(msg)
                }
            })
        };
        // isinstance(val, (int, float)): bounds apply (bool is an int)
        let numeric = |v: &PyRef| -> Option<(f64, bool)> {
            let int = v.is_instance(&lookup("builtins", "int").ok()?).ok()?;
            let float = v.is_instance(&lookup("builtins", "float").ok()?).ok()?;
            (int || float).then(|| (v.as_f64().unwrap_or(0.0), int))
        };
        cfg.get_option_with(&fetch, &option, default, Some((&numeric, bounds)), deprecate, &parsed_value).map_err(config_error)
    });
    r.attr(
        name!("config"),
        |o| {
            lookup("_mr", "ConfigView")?.call(&[
                &crate::plugins::native::bound_method(o, |a| {
                    let section = a.get(0, "section").ok_or_else(|| py_error("TypeError", "missing section".into()))?.as_string()?;
                    let option = a.get(1, "option").ok_or_else(|| py_error("TypeError", "missing option".into()))?.as_string()?;
                    Ok(match this(a)?.raw_get(&section, &option) {
                        Ok(v) => tuple(&[&py_int(0), &py_str(&v)]),
                        Err(crate::confighelper::RawMissing::Section) => tuple(&[&py_int(1), &none()]),
                        Err(crate::confighelper::RawMissing::Option) => tuple(&[&py_int(2), &none()]),
                    })
                })?,
                &crate::plugins::native::bound_method(o, |a| to_py(&Value::from(this(a)?.sections())))?,
                &crate::plugins::native::bound_method(o, |a| {
                    let section = a.get(0, "section").ok_or_else(|| py_error("TypeError", "missing section".into()))?.as_string()?;
                    Ok(py_bool(this(a)?.sections().contains(&section)))
                })?,
            ])
        },
        None,
    );
    r.attr(name!("section"), |o| Ok(py_str(objects::unwrap_as::<ConfigHelper>(o)?.get_name())), None);
    r.attr(
        name!("fallback_section"),
        |o| Ok(objects::unwrap_as::<ConfigHelper>(o)?.fallback_section.as_deref().map(py_str).unwrap_or_else(none)),
        None,
    );
    r.attr(name!("server"), |_| objects::wrap(crate::server::server()), None);
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
    r.method(name!("set_option"), |a| {
        this(a)?.set_option(&arg(a, 0, "option")?.str()?, &arg(a, 1, "value")?.str()?).map_err(config_error)?;
        Ok(none())
    });
    r.method(name!("get_file"), |a| {
        let c = this(a)?;
        match c.source.find_config_file(c.get_name()) {
            Some(p) => lookup("pathlib", "Path")?.call(&[&py_str(&p.to_string_lossy())]),
            None => Ok(none()),
        }
    });
    // get_hash(): hashlib.sha256 over the section's options and values
    r.method(name!("get_hash"), |a| {
        let h = lookup("hashlib", "sha256")?.call(&[])?;
        for (option, val) in this(a)?.get_options() {
            h.call_method("update", &[&py_bytes(option.as_bytes())?])?;
            h.call_method("update", &[&py_bytes(val.as_bytes())?])?;
        }
        Ok(h)
    });
    r.method(name!("read_supplemental_dict"), |a| {
        let obj = from_py(arg(a, 0, "obj")?)?;
        objects::wrap(Rc::new(this(a)?.read_supplemental_dict(&obj).map_err(config_error)?))
    });
    r.method(name!("read_supplemental_config"), |a| {
        match this(a)?.read_supplemental_config(&arg(a, 0, "file_name")?.str()?).map_err(config_error)? {
            Some(c) => objects::wrap(Rc::new(c)),
            None => Err(py_error("IndexError", "list index out of range".into())),
        }
    });
    r.method(name!("write_config"), |a| {
        arg(a, 0, "file_obj")?.call_method("write", &[&py_str(&this(a)?.write_config())])?;
        Ok(none())
    });
    r.method(name!("get_parsed_config"), |a| to_py(&this(a)?.get_parsed_config()));
    r.method(name!("get_orig_config"), |a| to_py(&this(a)?.get_orig_config()));
    r.method(name!("get_file_sections"), |a| {
        let m: serde_json::Map<String, Value> = this(a)?.get_file_sections().into_iter().map(|(k, v)| (k, Value::from(v))).collect();
        to_py(&Value::Object(m))
    });
    r.method(name!("get_config_files"), |a| to_py(&Value::from(this(a)?.get_config_files())));
    r.method(name!("validate_config"), |a| {
        this(a)?.validate_config(&crate::server::server());
        Ok(none())
    });
    r.method(name!("create_backup"), |a| {
        this(a)?.create_backup(&crate::server::server().app_arg_str("config_file"));
        Ok(none())
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

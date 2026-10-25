// moonraker.components.authorization.Authorization for Python components
use super::*;
use crate::components::authorization::{Authorization, JwtError};
use crate::plugins::futures::rust_future;

macro_rules! au {
    ($m:literal) => {
        concat!("moonraker.components.authorization.Authorization.", $m)
    };
}

fn err(e: crate::common::ServerError) -> PyError {
    server_error(&e.message, e.status_code)
}

/// decode_jwt's error as the Python exception Moonraker's code raises
fn jwt_err(e: JwtError) -> PyError {
    let raised = match e {
        JwtError::Server(e) => return err(e),
        JwtError::Exc(class, arg) => {
            let (module, name) = class.rsplit_once('.').unwrap_or(("builtins", class));
            match lookup(module, name).and_then(|c| c.call(&[&py_str(&arg)])) {
                Ok(exc) => return PyError { type_name: name.into(), message: arg, traceback: String::new(), value: Some(exc) },
                Err(e) => return e,
            }
        }
        // Python's own errors (their text varies with the Python version):
        // `a, b = parts`, jsonw.loads(data), `kid in self.public_jwks`,
        // self.users.get(name)
        JwtError::Unpack(n) => lookup("builtins", "exec")
            .and_then(|f| f.call(&[&py_str(&format!("a, b = [0] * {}", n))])),
        JwtError::Json(data, msg) => {
            match lookup("moonraker.utils.json_wrapper", "loads").and_then(|f| f.call(&[&py_bytes(&data)?])) {
                // input serde_json rejects and Python takes (an out of range number)
                Ok(_) => return py_error("ValueError", msg),
                r => r,
            }
        }
        JwtError::Unhashable(v, "get") => to_py(&v).and_then(|v| lookup("builtins", "dict")?.call(&[])?.getattr("get")?.call(&[&v])),
        JwtError::Unhashable(v, _) => to_py(&v).and_then(|v| lookup("operator", "contains")?.call(&[&lookup("builtins", "dict")?.call(&[])?, &v])),
    };
    match raised {
        Err(e) => e,
        Ok(_) => py_error("RuntimeError", "decode_jwt: expected an exception".into()),
    }
}

/// an ip_address object (or its text) as a Rust address
fn ip(v: &PyRef) -> PyResult<Option<std::net::IpAddr>> {
    if v.is_none() {
        return Ok(None);
    }
    Ok(v.str()?.parse().ok())
}

pub fn register(r: &mut Registry) {
    r.method(au!("decode_jwt"), |a| {
        let token = arg(a, 0, "token")?.as_string()?;
        let ttype = match a.get(1, "token_type") {
            Some(t) => t.as_string()?,
            None => "access".to_string(),
        };
        let check_exp = match a.get(2, "check_exp") {
            Some(c) => c.is_true()?,
            None => true,
        };
        let u = a.this::<Authorization>()?.decode_jwt_exc(&token, &ttype, check_exp).map_err(jwt_err)?;
        super::application::user_info(&u)
    });
    r.method(au!("validate_jwt"), |a| {
        let u = a.this::<Authorization>()?.validate_jwt(&arg(a, 0, "token")?.as_string()?).map_err(err)?;
        super::application::user_info(&u)
    });
    r.method(au!("validate_api_key"), |a| match a.this::<Authorization>()?.validate_api_key(&arg(a, 0, "api_key")?.as_string()?).map_err(err)? {
        Some(u) => super::application::user_info(&u),
        None => Ok(none()),
    });
    r.method(au!("is_api_key_enabled"), |a| Ok(py_bool(a.this::<Authorization>()?.is_api_key_enabled())));
    r.method(au!("cors_enabled"), |a| Ok(py_bool(a.this::<Authorization>()?.cors_enabled())));
    r.method(au!("get_api_key"), |a| Ok(a.this::<Authorization>()?.get_api_key().map(|k| py_str(&k)).unwrap_or_else(none)));
    r.method(au!("get_oneshot_token"), |a| {
        let addr = ip(arg(a, 0, "ip_addr")?)?;
        let user = match a.get(1, "user") {
            Some(u) if !u.is_none() => Some(super::application::user_info_from_py(u)?),
            _ => None,
        };
        Ok(py_str(&a.this::<Authorization>()?.get_oneshot_token(addr, user)))
    });
    r.method(au!("check_logins_maxed"), |a| match ip(arg(a, 0, "ip_addr")?)? {
        Some(addr) => Ok(py_bool(a.this::<Authorization>()?.check_logins_maxed(addr))),
        None => Ok(py_bool(false)),
    });
    r.method(au!("check_cors"), |a| {
        let origin = match arg(a, 0, "origin")? {
            o if o.is_none() => None,
            o => Some(o.as_string()?),
        };
        let au = a.this::<Authorization>()?;
        rust_future(async move { Ok(py_bool(au.check_cors(origin.as_deref()).await)) })
    });
}

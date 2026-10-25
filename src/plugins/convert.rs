// Python objects <-> serde_json::Value, for data that crosses as JSON in
// Moonruster (request arguments, config defaults, event payloads, status):
// None, bool, int, float, str, list/tuple, dict with str keys.
use super::capi::*;
use super::native::py_error;
use super::py::*;
use serde_json::{Map, Number, Value};

pub fn to_py(v: &Value) -> PyResult<PyRef> {
    Ok(match v {
        Value::Null => none(),
        Value::Bool(b) => py_bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                py_int(i)
            } else if let Some(u) = n.as_u64() {
                // beyond i64: Python's int from its decimal text
                lookup("builtins", "int")?.call(&[&py_str(&u.to_string())])?
            } else {
                py_float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        Value::String(s) => py_str(s),
        Value::Array(a) => {
            // SAFETY: GIL held (py.rs); returns a new empty list or NULL with an exception
            let list = PyRef::owned(unsafe { PyList_New(0) })?;
            for item in a {
                let x = to_py(item)?;
                // SAFETY: GIL held; list and x are valid objects (Append takes its own reference to x)
                if unsafe { PyList_Append(list.as_ptr(), x.as_ptr()) } != 0 {
                    return Err(take_error());
                }
            }
            list
        }
        Value::Object(o) => {
            // SAFETY: GIL held; returns a new empty dict or NULL with an exception
            let d = PyRef::owned(unsafe { PyDict_New() })?;
            for (k, item) in o {
                let x = to_py(item)?;
                // SAFETY: GIL held; d, the key (a temporary alive to the end of the statement) and x are valid objects; SetItem takes its own references
                if unsafe { PyDict_SetItem(d.as_ptr(), py_str(k).as_ptr(), x.as_ptr()) } != 0 {
                    return Err(take_error());
                }
            }
            d
        }
    })
}

/// The JSON value of a Python object; TypeError for anything else (as
/// json.dumps would raise)
pub fn from_py(o: &PyRef) -> PyResult<Value> {
    if o.is_none() {
        return Ok(Value::Null);
    }
    let builtins = import("builtins")?;
    let is = |name: &str| -> PyResult<bool> { o.is_instance(&builtins.getattr(name)?) };
    // bool before int: bool is an int subclass
    if is("bool")? {
        return Ok(Value::Bool(o.is_true()?));
    }
    if is("int")? {
        return match o.as_i64() {
            Ok(i) => Ok(Value::from(i)),
            Err(_) => {
                // beyond i64: keep it exact through its decimal text
                let text = o.str()?;
                Ok(serde_json::from_str::<Number>(&text).map(Value::Number).unwrap_or(Value::String(text)))
            }
        };
    }
    if is("float")? {
        return Ok(Number::from_f64(o.as_f64()?).map(Value::Number).unwrap_or(Value::Null));
    }
    if is("str")? {
        return Ok(Value::String(o.as_string()?));
    }
    if is("dict")? {
        let mut m = Map::new();
        let items = o.call_method("items", &[])?;
        // SAFETY: GIL held; items is a valid object
        let it = PyRef::owned(unsafe { PyObject_GetIter(items.as_ptr()) })?;
        loop {
            // SAFETY: GIL held; it is a valid iterator; the result is a new reference or NULL
            let p = unsafe { PyIter_Next(it.as_ptr()) };
            if p.is_null() {
                // SAFETY: GIL held; PyErr_Occurred only reads the thread's error state
                if unsafe { !PyErr_Occurred().is_null() } {
                    return Err(take_error());
                }
                break;
            }
            let pair = PyRef::owned(p)?;
            // SAFETY: GIL held; pair is a (key, value) tuple from dict.items(), so index 0 exists; borrowed() takes a reference before pair is dropped
            let k = PyRef::borrowed(unsafe { PyTuple_GetItem(pair.as_ptr(), 0) });
            // SAFETY: GIL held; pair is a (key, value) tuple from dict.items(), so index 1 exists; borrowed() takes a reference before pair is dropped
            let v = PyRef::borrowed(unsafe { PyTuple_GetItem(pair.as_ptr(), 1) });
            // json.dumps' key conversion: None -> "null", bools lowercase
            let key = if k.is_none() {
                "null".to_string()
            } else if k.is_instance(&builtins.getattr("bool")?)? {
                if k.is_true()? { "true" } else { "false" }.to_string()
            } else {
                k.str()?
            };
            m.insert(key, from_py(&v)?);
        }
        return Ok(Value::Object(m));
    }
    if is("list")? || is("tuple")? {
        // SAFETY: GIL held; o is a valid object
        let it = PyRef::owned(unsafe { PyObject_GetIter(o.as_ptr()) })?;
        let mut out = vec![];
        loop {
            // SAFETY: GIL held; it is a valid iterator; the result is a new reference or NULL
            let p = unsafe { PyIter_Next(it.as_ptr()) };
            if p.is_null() {
                // SAFETY: GIL held; PyErr_Occurred only reads the thread's error state
                if unsafe { !PyErr_Occurred().is_null() } {
                    return Err(take_error());
                }
                break;
            }
            out.push(from_py(&PyRef::owned(p)?)?);
        }
        return Ok(Value::Array(out));
    }
    let tname = o.getattr("__class__")?.getattr("__name__")?.as_string()?;
    Err(py_error("TypeError", format!("Object of type {} is not JSON serializable", tname)))
}

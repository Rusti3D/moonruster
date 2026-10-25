// Record encoding and Python sqlite3's conversions

use super::*;

pub fn encode_record(value: &Value) -> Result<Vec<u8>, ServerError> {
    match value {
        Value::Null => Ok(vec![0u8]),
        Value::Bool(b) => Ok(vec![b'?', *b as u8]),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                let mut v = vec![b'q'];
                v.extend_from_slice(&i.to_ne_bytes());
                Ok(v)
            } else if n.is_u64() {
                Err(ServerError::bad(format!("Error encoding val: {}, type: <class 'int'>", n)))
            } else {
                let mut v = vec![b'd'];
                v.extend_from_slice(&n.as_f64().unwrap_or(0.0).to_ne_bytes());
                Ok(v)
            }
        }
        Value::String(s) => {
            let mut v = vec![b's'];
            v.extend_from_slice(s.as_bytes());
            Ok(v)
        }
        Value::Array(_) | Value::Object(_) => Ok(value.to_string().into_bytes()),
    }
}

pub fn decode_record(b: &[u8]) -> Result<Value, ServerError> {
    let fmt = *b.first().ok_or_else(|| ServerError::bad("Error decoding empty value"))?;
    let err = || ServerError::bad(format!("Error decoding value {}, format: {}", String::from_utf8_lossy(b), fmt as char));
    match fmt {
        b'q' => {
            let arr: [u8; 8] = b.get(1..9).ok_or_else(err)?.try_into().map_err(|_| err())?;
            Ok(json!(i64::from_ne_bytes(arr)))
        }
        b'd' => {
            let arr: [u8; 8] = b.get(1..9).ok_or_else(err)?.try_into().map_err(|_| err())?;
            Ok(crate::utils::float_value(f64::from_ne_bytes(arr)))
        }
        b'?' => Ok(Value::Bool(b.get(1).map(|x| *x != 0).ok_or_else(err)?)),
        b's' => Ok(Value::String(String::from_utf8(b[1..].to_vec()).map_err(|_| err())?)),
        b'[' | b'{' => serde_json::from_slice(b).map_err(|_| err()),
        0 => Ok(Value::Null),
        _ => Err(err()),
    }
}

pub(super) fn value_ref_bytes(v: ValueRef) -> Vec<u8> {
    match v {
        ValueRef::Blob(b) => b.to_vec(),
        ValueRef::Text(t) => t.to_vec(),
        ValueRef::Integer(i) => i.to_string().into_bytes(),
        ValueRef::Real(r) => r.to_string().into_bytes(),
        ValueRef::Null => vec![0],
    }
}

/// Converts a sql column value into json, emulating python's sqlite3
/// converters for declared column types.
pub fn column_to_json(v: ValueRef, decl_type: Option<&str>) -> Value {
    let dt = decl_type.map(|d| d.to_lowercase());
    match dt.as_deref() {
        Some("record") => decode_record(&value_ref_bytes(v)).unwrap_or(Value::Null),
        Some("pyjson") => match v {
            ValueRef::Null => Value::Null,
            other => serde_json::from_slice(&value_ref_bytes(other)).unwrap_or(Value::Null),
        },
        Some("pybool") => match v {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(i) => Value::Bool(i != 0),
            other => Value::Bool(!value_ref_bytes(other).is_empty()),
        },
        _ => match v {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(i) => json!(i),
            ValueRef::Real(r) => crate::utils::float_value(r),
            ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).to_string()),
            ValueRef::Blob(b) => Value::Array(b.iter().map(|x| json!(x)).collect()),
        },
    }
}

/// Converts a json value into a sql parameter (python adapters: list/dict -> json)
pub fn json_to_sql(v: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as SV;
    match v {
        Value::Null => SV::Null,
        Value::Bool(b) => SV::Integer(*b as i64),
        Value::Number(n) => match n.as_i64() {
            Some(i) => SV::Integer(i),
            None => SV::Real(n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => SV::Text(s.clone()),
        Value::Array(_) | Value::Object(_) => SV::Blob(v.to_string().into_bytes()),
    }
}

pub fn parse_namespace_key(key: &Value) -> Result<Vec<String>, ServerError> {
    let key_list: Vec<String> = match key {
        Value::Array(a) => a.iter().map(crate::pyfmt::py_str).collect(),
        Value::String(s) => s.split('.').map(String::from).collect(),
        _ => Vec::new(),
    };
    if key_list.is_empty() || key_list.iter().any(|k| k.is_empty()) {
        return Err(ServerError::bad(format!("Invalid Key Format: '{}'", crate::pyfmt::py_str(key))));
    }
    Ok(key_list)
}

pub(super) fn getitem<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(o) => o.get(key),
        _ => None,
    }
}

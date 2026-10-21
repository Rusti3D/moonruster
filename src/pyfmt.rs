// Python compatible string formatting and conversion helpers.  Moonraker
// frequently embeds str()/repr() output of python objects in messages and
// responses, these helpers reproduce that output.

use serde_json::Value;

/// Python's repr() for a float
pub fn py_float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    // Rust's {:e} produces the shortest round trip digits
    let sci = format!("{:e}", f);
    let (mantissa, exp) = sci.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let neg = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let sign = if neg { "-" } else { "" };
    if (-4..16).contains(&exp) {
        // fixed notation
        let ndigits = digits.len() as i32;
        let s = if exp >= 0 {
            let int_len = exp + 1;
            if ndigits <= int_len {
                format!("{}{}.0", digits, "0".repeat((int_len - ndigits) as usize))
            } else {
                format!("{}.{}", &digits[..int_len as usize], &digits[int_len as usize..])
            }
        } else {
            format!("0.{}{}", "0".repeat((-exp - 1) as usize), digits)
        };
        format!("{}{}", sign, s)
    } else {
        let m = if digits.len() > 1 { format!("{}.{}", &digits[..1], &digits[1..]) } else { digits.clone() };
        let esign = if exp < 0 { "-" } else { "+" };
        format!("{}{}e{}{:02}", sign, m, esign, exp.abs())
    }
}

fn py_str_repr(s: &str) -> String {
    // Python prefers single quotes unless the string contains single quotes
    // and no double quotes
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python's repr() for a json value
pub fn py_repr(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True".into() } else { "False".into() },
        Value::Number(n) => {
            if n.is_f64() {
                py_float_repr(n.as_f64().unwrap())
            } else {
                n.to_string()
            }
        }
        Value::String(s) => py_str_repr(s),
        Value::Array(a) => format!("[{}]", a.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        Value::Object(o) => format!(
            "{{{}}}",
            o.iter().map(|(k, v)| format!("{}: {}", py_str_repr(k), py_repr(v))).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// Python's str() for a json value
pub fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => py_repr(other),
    }
}

pub fn py_bool(b: bool) -> &'static str {
    if b { "True" } else { "False" }
}

/// Python type name of a json value
pub fn py_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) => if n.is_f64() { "float" } else { "int" },
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// Python's round(x, ndigits) (correctly rounded, ties to even)
pub fn py_round(x: f64, ndigits: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{:.*}", ndigits, x).parse().unwrap_or(x)
}

/// round() applied to a json number, ints are returned unchanged
pub fn round_value(v: &Value, ndigits: usize) -> Value {
    match v {
        Value::Number(n) if n.is_f64() => crate::utils::float_value(py_round(n.as_f64().unwrap(), ndigits)),
        other => other.clone(),
    }
}

/// Python's int(str) conversion
pub fn py_int(s: &str) -> Option<i64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let (sign, body) = match t.as_bytes()[0] {
        b'+' => (1, &t[1..]),
        b'-' => (-1, &t[1..]),
        _ => (1, t),
    };
    if body.is_empty() || body.starts_with('_') || body.ends_with('_') || body.contains("__") {
        return None;
    }
    let clean: String = body.chars().filter(|c| *c != '_').collect();
    if !clean.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    clean.parse::<i64>().ok().map(|v| v * sign)
}

/// Python's float(str) conversion
pub fn py_float(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_lowercase();
    let (sign, body) = match lower.as_bytes()[0] {
        b'+' => (1.0, &lower[1..]),
        b'-' => (-1.0, &lower[1..]),
        _ => (1.0, lower.as_str()),
    };
    match body {
        "inf" | "infinity" => return Some(sign * f64::INFINITY),
        "nan" => return Some(f64::NAN),
        _ => {}
    }
    if body.contains("__") || body.starts_with('_') || body.ends_with('_') {
        return None;
    }
    let clean: String = body.chars().filter(|c| *c != '_').collect();
    if !clean.chars().all(|c| c.is_ascii_digit() || c == '.' || c == 'e' || c == '+' || c == '-') {
        return None;
    }
    clean.parse::<f64>().ok().map(|v| v * sign)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn float_repr() {
        assert_eq!(py_float_repr(1.0), "1.0");
        assert_eq!(py_float_repr(0.1), "0.1");
        assert_eq!(py_float_repr(1e-5), "1e-05");
        assert_eq!(py_float_repr(1e16), "1e+16");
        assert_eq!(py_float_repr(123456.789), "123456.789");
        assert_eq!(py_float_repr(-0.0001), "-0.0001");
        assert_eq!(py_float_repr(1.5e300), "1.5e+300");
    }

    #[test]
    fn repr() {
        assert_eq!(py_repr(&json!({"a": [1, 2.5, null, true], "b": "it's"})), "{'a': [1, 2.5, None, True], 'b': \"it's\"}");
        assert_eq!(py_int(" 1_000 "), Some(1000));
        assert_eq!(py_int("1.5"), None);
        assert_eq!(py_float("1e3"), Some(1000.0));
    }
}

/// Python bool(value)
pub fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

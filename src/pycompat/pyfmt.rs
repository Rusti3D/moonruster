// Python compatible string formatting and conversion helpers.  Moonraker
// frequently embeds str()/repr() output of python objects in messages and
// responses, these helpers reproduce that output.

use crate::pycompat::pyfmt_tables::{DIGIT_ZEROS, NONPRINTABLE, WHITESPACE};
use serde_json::Value;

fn in_ranges(table: &[(u32, u32)], cp: u32) -> bool {
    table
        .binary_search_by(|&(a, b)| {
            if b < cp {
                std::cmp::Ordering::Less
            } else if a > cp {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// str.isspace()
fn py_isspace(c: char) -> bool {
    in_ranges(WHITESPACE, c as u32)
}

/// The value of a Unicode decimal digit (unicodedata.decimal)
fn py_decimal(c: char) -> Option<u32> {
    let cp = c as u32;
    let i = DIGIT_ZEROS.partition_point(|&z| z <= cp);
    let zero = *DIGIT_ZEROS.get(i.checked_sub(1)?)?;
    (cp - zero < 10).then_some(cp - zero)
}

/// str.strip() with no argument
fn py_strip(s: &str) -> &str {
    s.trim_matches(py_isspace)
}

/// Python's digitpart (digit (["_"] digit)*) as ASCII digits, None if it
/// is not one
fn py_digitpart(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut prev_digit = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '_' {
            // only between digits
            if !prev_digit || !chars.peek().is_some_and(|&n| py_decimal(n).is_some()) {
                return None;
            }
            prev_digit = false;
            continue;
        }
        out.push(char::from_digit(py_decimal(c)?, 10)?);
        prev_digit = true;
    }
    (!out.is_empty()).then_some(out)
}

/// A float as JSON (NaN and infinities have no JSON number: null)
pub fn float_value(f: f64) -> Value {
    serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
}

/// Python's repr() for a float
pub fn py_float_repr(f: f64) -> String {
    let mut out = String::new();
    py_float_repr_into(&mut out, f);
    out
}

/// A fixed buffer for a formatted number (no heap allocation)
struct NumBuf {
    buf: [u8; 32],
    len: usize,
}

impl std::fmt::Write for NumBuf {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let b = s.as_bytes();
        let end = self.len + b.len();
        if end > self.buf.len() {
            return Err(std::fmt::Error);
        }
        self.buf[self.len..end].copy_from_slice(b);
        self.len = end;
        Ok(())
    }
}

/// Python's repr() for a float, appended to `out`
pub fn py_float_repr_into(out: &mut String, f: f64) {
    use std::fmt::Write;
    if f.is_nan() {
        return out.push_str("nan");
    }
    if f.is_infinite() {
        return out.push_str(if f > 0.0 { "inf" } else { "-inf" });
    }
    if f == 0.0 {
        return out.push_str(if f.is_sign_negative() { "-0.0" } else { "0.0" });
    }
    // Rust's {:e} produces the shortest round trip digits: "-d.ddde-7"
    let mut sci = NumBuf { buf: [0; 32], len: 0 };
    write!(sci, "{:e}", f).expect("a float fits 32 bytes");
    let sci = &sci.buf[..sci.len];
    let e = sci.iter().position(|&c| c == b'e').expect("exponent");
    let (mantissa, exp) = (&sci[..e], &sci[e + 1..]);
    let exp: i32 = std::str::from_utf8(exp).expect("ascii").parse().expect("exponent");
    let neg = mantissa[0] == b'-';
    let mut digits = [0u8; 24];
    let mut nd = 0;
    for &c in mantissa {
        if c.is_ascii_digit() {
            digits[nd] = c;
            nd += 1;
        }
    }
    let digits = std::str::from_utf8(&digits[..nd]).expect("ascii");
    if neg {
        out.push('-');
    }
    let zeros = |out: &mut String, n: i32| {
        for _ in 0..n {
            out.push('0');
        }
    };
    if (-4..16).contains(&exp) {
        // fixed notation
        let ndigits = nd as i32;
        if exp >= 0 {
            let int_len = exp + 1;
            if ndigits <= int_len {
                out.push_str(digits);
                zeros(out, int_len - ndigits);
                out.push_str(".0");
            } else {
                out.push_str(&digits[..int_len as usize]);
                out.push('.');
                out.push_str(&digits[int_len as usize..]);
            }
        } else {
            out.push_str("0.");
            zeros(out, -exp - 1);
            out.push_str(digits);
        }
    } else {
        out.push_str(&digits[..1]);
        if nd > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if exp < 0 { '-' } else { '+' });
        let _ = write!(out, "{:02}", exp.abs());
    }
}

pub fn py_str_repr(s: &str) -> String {
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
            // what str.isprintable() rejects, escaped as repr() does
            c if in_ranges(NONPRINTABLE, c as u32) => {
                let cp = c as u32;
                if cp < 0x100 {
                    out.push_str(&format!("\\x{:02x}", cp));
                } else if cp < 0x10000 {
                    out.push_str(&format!("\\u{:04x}", cp));
                } else {
                    out.push_str(&format!("\\U{:08x}", cp));
                }
            }
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
        Value::Number(n) if n.is_f64() => float_value(py_round(n.as_f64().unwrap(), ndigits)),
        other => other.clone(),
    }
}

/// Python's int(str) conversion (base 10; values beyond i64 are None)
pub fn py_int(s: &str) -> Option<i64> {
    let t = py_strip(s);
    let (neg, body) = match t.as_bytes().first()? {
        b'+' => (false, &t[1..]),
        b'-' => (true, &t[1..]),
        _ => (false, t),
    };
    let digits = py_digitpart(body)?;
    format!("{}{}", if neg { "-" } else { "" }, digits).parse().ok()
}

/// Python's float(str) conversion: [sign] (inf | infinity | nan |
/// number), number = digitpart ["." [digitpart]] | "." digitpart, then
/// an optional exponent "e" [sign] digitpart
pub fn py_float(s: &str) -> Option<f64> {
    let t = py_strip(s);
    let (sign, body) = match t.as_bytes().first()? {
        b'+' => (1.0, &t[1..]),
        b'-' => (-1.0, &t[1..]),
        _ => (1.0, t),
    };
    match body.to_ascii_lowercase().as_str() {
        "inf" | "infinity" => return Some(sign * f64::INFINITY),
        "nan" => return Some(f64::NAN),
        _ => {}
    }
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    let mut ascii = match mantissa.split_once('.') {
        Some((int, frac)) => {
            let int = if int.is_empty() { None } else { Some(py_digitpart(int)?) };
            let frac = if frac.is_empty() { None } else { Some(py_digitpart(frac)?) };
            if int.is_none() && frac.is_none() {
                return None;
            }
            format!("{}.{}", int.unwrap_or_else(|| "0".into()), frac.unwrap_or_else(|| "0".into()))
        }
        None => py_digitpart(mantissa)?,
    };
    if let Some(e) = exponent {
        let (esign, edigits) = match e.as_bytes().first()? {
            b'+' => ("", &e[1..]),
            b'-' => ("-", &e[1..]),
            _ => ("", e),
        };
        ascii = format!("{}e{}{}", ascii, esign, py_digitpart(edigits)?);
    }
    ascii.parse::<f64>().ok().map(|v| sign * v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // the allocating py_float_repr the in-place one replaced (rustify): the
    // oracle the property test compares with
    fn old_py_float_repr(f: f64) -> String {
        if f.is_nan() {
            return "nan".into();
        }
        if f.is_infinite() {
            return if f > 0.0 { "inf".into() } else { "-inf".into() };
        }
        if f == 0.0 {
            return if f.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
        }
        let sci = format!("{:e}", f);
        let (mantissa, exp) = sci.split_once('e').unwrap();
        let exp: i32 = exp.parse().unwrap();
        let neg = mantissa.starts_with('-');
        let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
        let sign = if neg { "-" } else { "" };
        if (-4..16).contains(&exp) {
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

    /// a deterministic stream of f64 bit patterns (xorshift64*): every
    /// exponent range, subnormals, NaN payloads
    fn bit_patterns(n: usize) -> impl Iterator<Item = f64> {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        (0..n).map(move |_| {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            f64::from_bits(x.wrapping_mul(0x2545_F491_4F6C_DD1D))
        })
    }

    #[test]
    fn float_repr_matches_the_allocating_one() {
        let special = [
            0.0, -0.0, 1.0, -1.0, 0.1, 0.5, 1e-4, 9.999e-5, 1e15, 9.99e15, 1e16, 1e17, 123456789012345680.0,
            f64::MIN_POSITIVE, f64::MAX, f64::MIN, f64::EPSILON, 5e-324, 1e300, 1e-300, 2.5, 100.0, 1e22, 0.001,
            f64::NAN, f64::INFINITY, f64::NEG_INFINITY,
        ];
        for f in special.into_iter().chain(bit_patterns(1_000_000)) {
            assert_eq!(py_float_repr(f), old_py_float_repr(f), "bits {:#x}", f.to_bits());
        }
        // values a printer reports: short decimals, temperatures, positions
        for i in -200_000..200_000i64 {
            let f = i as f64 / 1000.0;
            assert_eq!(py_float_repr(f), old_py_float_repr(f));
        }
    }


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

    /// CPython's own results (tools/gen_pyfmt_vectors.py): every mismatch is
    /// listed, not only the first
    #[test]
    fn vectors() {
        let v: Value = serde_json::from_str(include_str!("../../tests/data/pyfmt_vectors.json")).unwrap();
        let mut bad: Vec<String> = Vec::new();
        for e in v["float_repr"].as_array().unwrap() {
            let f = f64::from_bits(e[0].as_u64().unwrap());
            let got = py_float_repr(f);
            if got != e[1].as_str().unwrap() {
                bad.push(format!("float_repr {:e}: {} != {}", f, got, e[1]));
            }
        }
        for e in v["str_repr"].as_array().unwrap() {
            let got = py_str_repr(e[0].as_str().unwrap());
            if got != e[1].as_str().unwrap() {
                bad.push(format!("str_repr {:?}: {} != {}", e[0], got, e[1]));
            }
        }
        for e in v["int"].as_array().unwrap() {
            let got = py_int(e[0].as_str().unwrap()).map(Value::from).unwrap_or(Value::Null);
            // python ints beyond i64 are out of range here
            if got != e[1] && !(e[1].is_number() && e[1].as_i64().is_none()) {
                bad.push(format!("int {:?}: {} != {}", e[0], got, e[1]));
            }
        }
        for e in v["float"].as_array().unwrap() {
            let got = match py_float(e[0].as_str().unwrap()) {
                None => Value::Null,
                Some(f) if !f.is_finite() => Value::from(py_float_repr(f)),
                Some(f) => Value::from(f),
            };
            let want = match &e[1] {
                Value::Number(n) => Value::from(n.as_f64().unwrap()),
                other => other.clone(),
            };
            if got != want {
                bad.push(format!("float {:?}: {} != {}", e[0], got, want));
            }
        }
        for e in v["round"].as_array().unwrap() {
            let f = f64::from_bits(e[0].as_u64().unwrap());
            let got = py_float_repr(py_round(f, e[1].as_u64().unwrap() as usize));
            if got != e[2].as_str().unwrap() {
                bad.push(format!("round({:e}, {}): {} != {}", f, e[1], got, e[2]));
            }
        }
        assert!(bad.is_empty(), "{} mismatches:
{}", bad.len(), bad.join("
"));
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

// JWT encoding and decoding, password hashing

use super::*;

pub fn base64url_encode(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// An error of decode_jwt, as Moonraker's code raises it
pub enum JwtError {
    /// self.server.error(...)
    Server(ServerError),
    /// a Python exception: its class ("module.Name") and argument
    Exc(&'static str, String),
    /// jsonw.loads(data) failed (the plugin layer raises Python's own error)
    Json(Vec<u8>, String),
    /// a dict lookup of an unhashable value: `in` ("contains") or .get()
    Unhashable(Value, &'static str),
    /// unpacking this many token parts into two names
    Unpack(usize),
}

impl From<ServerError> for JwtError {
    fn from(e: ServerError) -> Self {
        JwtError::Server(e)
    }
}

impl From<JwtError> for ServerError {
    fn from(e: JwtError) -> Self {
        match e {
            JwtError::Server(e) => e,
            JwtError::Exc("builtins.KeyError", key) => ServerError::exc(format!("'{}'", key)),
            JwtError::Exc(_, msg) | JwtError::Json(_, msg) => ServerError::exc(msg),
            JwtError::Unhashable(v, _) => ServerError::exc(format!("unhashable type: '{}'", crate::pyfmt::py_type_name(&v))),
            JwtError::Unpack(n) if n < 2 => ServerError::exc(format!("not enough values to unpack (expected 2, got {})", n)),
            JwtError::Unpack(_) => ServerError::exc("too many values to unpack (expected 2)"),
        }
    }
}

/// Moonraker's base64url_decode: padded, then base64.urlsafe_b64decode
/// (binascii.a2b_base64, non-strict: characters outside the alphabet are
/// skipped and decoding stops at the padding)
pub fn base64url_decode(data: &str) -> Result<Vec<u8>, JwtError> {
    if !data.is_ascii() {
        return Err(JwtError::Exc("builtins.ValueError", "string argument should contain only ASCII characters".into()));
    }
    let pad = data.len() % 4;
    let padded = data.bytes().chain(std::iter::repeat_n(b'=', if pad != 0 { 4 - pad } else { 0 }));
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    let (mut quad_pos, mut pads, mut left) = (0u8, 0u8, 0u8);
    for c in padded {
        if c == b'=' {
            if quad_pos >= 2 {
                pads += 1;
                if quad_pos + pads >= 4 {
                    return Ok(out);
                }
            }
            continue;
        }
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => continue,
        };
        pads = 0;
        match quad_pos {
            0 => left = v,
            1 => {
                out.push((left << 2) | (v >> 4));
                left = v & 0x0f;
            }
            2 => {
                out.push((left << 4) | (v >> 2));
                left = v & 0x03;
            }
            _ => out.push((left << 6) | v),
        }
        quad_pos = (quad_pos + 1) % 4;
    }
    match quad_pos {
        0 => Ok(out),
        1 => Err(JwtError::Exc(
            "binascii.Error",
            format!("Invalid base64-encoded string: number of data characters ({}) cannot be 1 more than a multiple of 4", out.len() / 3 * 4 + 1),
        )),
        _ => Err(JwtError::Exc("binascii.Error", "Incorrect padding".into())),
    }
}

/// jsonw.loads of decoded JWT data
pub(super) fn jwt_json(data: Vec<u8>) -> Result<Value, JwtError> {
    serde_json::from_slice(&data).map_err(|e| {
        let msg = e.to_string();
        JwtError::Json(data, msg)
    })
}

/// payload[key] of a decoded JWT payload
pub(super) fn jwt_item<'a>(payload: &'a Value, key: &str) -> Result<&'a Value, JwtError> {
    let name = crate::pyfmt::py_type_name(payload);
    match payload {
        Value::Object(m) => m.get(key).ok_or_else(|| JwtError::Exc("builtins.KeyError", key.to_string())),
        Value::Array(_) => Err(JwtError::Exc("builtins.TypeError", "list indices must be integers or slices, not str".into())),
        Value::String(_) => Err(JwtError::Exc("builtins.TypeError", "string indices must be integers, not 'str'".into())),
        _ => Err(JwtError::Exc("builtins.TypeError", format!("'{}' object is not subscriptable", name))),
    }
}

pub(super) fn pbkdf2_hex(password: &str, salt: &[u8]) -> String {
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), salt, HASH_ITER, &mut out);
    hex::encode(out)
}

pub(super) fn compare_digest(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

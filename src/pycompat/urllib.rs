// Python's urllib.parse quoting, ported from CPython's source

/// urllib.parse.quote(s, safe): the UTF-8 bytes, letters, digits and
/// "_.-~" and `safe` kept, the rest as %XX (upper case)
pub fn quote(s: &str, safe: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || b"_.-~".contains(&b) || (b.is_ascii() && safe.as_bytes().contains(&b)) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// urllib.parse.quote_plus(s): quote(s, safe="") with spaces as "+"
pub fn quote_plus(s: &str) -> String {
    quote(s, " ").replace(' ', "+")
}

/// urllib.parse.unquote(s): %XX sequences decoded as UTF-8 (errors
/// replaced), anything else kept
pub fn unquote(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("00");
            out.push(u8::from_str_radix(hex, 16).unwrap_or(0));
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// urllib.parse.urlencode(pairs): quote_plus'ed key=value joined by "&"
pub fn urlencode<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", quote_plus(k.as_ref()), quote_plus(v.as_ref()))).collect::<Vec<_>>().join("&")
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    /// CPython's urllib.parse (tools/gen_path_vectors.py)
    #[test]
    fn vectors() {
        let v: Value = serde_json::from_str(include_str!("../../tests/data/path_vectors.json")).unwrap();
        let mut bad: Vec<String> = Vec::new();
        for (key, safe) in [("quote", "/"), ("quote_nosafe", "")] {
            for e in v[key].as_array().unwrap() {
                let got = super::quote(e[0].as_str().unwrap(), safe);
                if got != e[1].as_str().unwrap() {
                    bad.push(format!("{} {}: {} != {}", key, e[0], got, e[1]));
                }
            }
        }
        for e in v["unquote"].as_array().unwrap() {
            let got = super::unquote(e[0].as_str().unwrap());
            if got != e[1].as_str().unwrap() {
                bad.push(format!("unquote {}: {:?} != {}", e[0], got, e[1]));
            }
        }
        for e in v["urlencode"].as_array().unwrap() {
            let pairs: Vec<(String, String)> = e[0]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| (p[0].as_str().unwrap().to_string(), p[1].as_str().unwrap().to_string()))
                .collect();
            let got = super::urlencode(&pairs);
            if got != e[1].as_str().unwrap() {
                bad.push(format!("urlencode: {} != {}", got, e[1]));
            }
        }
        assert!(bad.is_empty(), "{} mismatches:\n{}", bad.len(), bad.join("\n"));
    }
}

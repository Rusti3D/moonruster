// The request: its info and its arguments (tornado.httputil)

use super::*;

/// Information about an incoming HTTP request, the equivalent of tornado's HTTPServerRequest
#[derive(Clone)]
pub struct HttpRequest {
    pub method: String,
    pub uri: String,
    pub path: String,
    pub headers: HeaderMap,
    pub remote_ip: String,
    pub orig_remote_ip: String,
    pub protocol: String,
    pub host: String,
    pub query_arguments: IndexMap<String, Vec<String>>,
    pub arguments: IndexMap<String, Vec<String>>,
    pub body: Bytes,
    pub start_time: f64,
}

impl HttpRequest {
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers.get(name).map(|v| String::from_utf8_lossy(v.as_bytes()).to_string())
    }

    pub fn has_header(&self, name: &str) -> bool {
        self.headers.contains_key(name)
    }

    /// tornado's get_argument(): last value, control chars replaced, stripped
    pub fn get_argument(&self, name: &str) -> Option<String> {
        self.arguments.get(name).and_then(|v| v.last()).map(|s| clean_argument(s))
    }

    pub fn host_name(&self) -> String {
        split_host_and_port(&self.host.to_lowercase()).0
    }

    pub fn is_proxied(&self) -> bool {
        self.has_header("X-Forwarded-For") || self.has_header("X-Real-Ip")
    }

    pub fn request_time(&self) -> f64 {
        utils::time_now() - self.start_time
    }
}

pub(super) fn clean_argument(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if matches!(c as u32, 0x00..=0x08 | 0x0e..=0x1f) { ' ' } else { c })
        .collect();
    cleaned.trim().to_string()
}

pub fn split_host_and_port(netloc: &str) -> (String, Option<u16>) {
    // r"^(.+):(\d+)$"
    if let Some((host, port)) = netloc.rsplit_once(':') {
        if !host.is_empty() && !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) {
            return (host.to_string(), port.parse().ok());
        }
    }
    (netloc.to_string(), None)
}

/// python's urllib.parse.parse_qs_bytes with keep_blank_values=True
pub fn parse_qs(qs: &str, out: &mut IndexMap<String, Vec<String>>) {
    for part in qs.split('&') {
        if part.is_empty() {
            continue;
        }
        let (k, v) = match part.split_once('=') {
            Some((k, v)) => (k, v),
            None => (part, ""),
        };
        let k = form_unquote(k);
        let v = form_unquote(v);
        out.entry(k).or_default().push(v);
    }
}

pub(super) fn form_unquote(s: &str) -> String {
    let replaced = s.replace('+', " ");
    percent_encoding::percent_decode_str(&replaced).decode_utf8_lossy().to_string()
}

pub(super) fn is_valid_ip(ip: &str) -> bool {
    utils::parse_ip_address(ip).is_some()
}

/// tornado.httputil.parse_body_arguments (urlencoded and multipart fields)
pub fn parse_body_arguments(content_type: &str, body: &[u8], arguments: &mut IndexMap<String, Vec<String>>) {
    if content_type.starts_with("application/x-www-form-urlencoded") {
        let s = String::from_utf8_lossy(body);
        parse_qs(&s, arguments);
    } else if content_type.starts_with("multipart/form-data") {
        let boundary = content_type
            .split(';')
            .map(|p| p.trim())
            .find_map(|p| p.strip_prefix("boundary="))
            .map(|b| b.trim_matches('"').to_string());
        if let Some(b) = boundary {
            parse_multipart_fields(&b, body, arguments);
        }
    }
}

pub(super) fn parse_multipart_fields(boundary: &str, data: &[u8], arguments: &mut IndexMap<String, Vec<String>>) {
    let delim = format!("--{}", boundary);
    let text = String::from_utf8_lossy(data);
    for part in text.split(&delim) {
        let part = part.strip_prefix("\r\n").unwrap_or(part);
        if part.starts_with("--") || part.trim().is_empty() {
            continue;
        }
        let (headers, value) = match part.split_once("\r\n\r\n") {
            Some(x) => x,
            None => continue,
        };
        let value = value.strip_suffix("\r\n").unwrap_or(value);
        let disp = headers.lines().find(|l| l.to_lowercase().starts_with("content-disposition"));
        if let Some(d) = disp {
            if d.contains("filename=") {
                continue;
            }
            if let Some(pos) = d.find("name=\"") {
                let rest = &d[pos + 6..];
                if let Some(end) = rest.find('"') {
                    arguments.entry(rest[..end].to_string()).or_default().push(value.to_string());
                }
            }
        }
    }
}

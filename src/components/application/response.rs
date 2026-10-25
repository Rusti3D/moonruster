// Responses and errors (tornado.web)

use super::*;

pub struct HttpResponse {
    pub status: u16,
    pub reason: Option<String>,
    pub headers: Vec<(String, String)>,
    pub body: Body,
    pub body_bytes: Option<Bytes>,
}

impl HttpResponse {
    pub fn new(status: u16) -> Self {
        HttpResponse { status, reason: None, headers: Vec::new(), body: empty_body(), body_bytes: Some(Bytes::new()) }
    }

    pub fn set_header(&mut self, name: &str, value: impl Into<String>) {
        let value = value.into();
        if let Some(h) = self.headers.iter_mut().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
            h.1 = value;
        } else {
            self.headers.push((name.to_string(), value));
        }
    }

    pub fn remove_header(&mut self, name: &str) {
        self.headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
    }

    pub fn has_header(&self, name: &str) -> bool {
        self.headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
    }

    pub fn set_body(&mut self, data: impl Into<Bytes>) {
        let b: Bytes = data.into();
        self.body_bytes = Some(b.clone());
        self.body = full_body(b);
    }

    pub fn set_stream(&mut self, body: Body) {
        self.body_bytes = None;
        self.body = body;
    }

    pub fn json(status: u16, val: &Value) -> Self {
        let mut r = HttpResponse::new(status);
        r.set_header("Content-Type", "application/json; charset=UTF-8");
        r.set_body(val.to_string());
        r
    }
}

/// Tornado's reason phrase validation
pub(super) fn valid_reason(reason: &str) -> bool {
    !reason.contains('<') && reason.chars().all(|c| c == '\t' || c == ' ' || (c as u32 >= 0x21 && c as u32 != 0x7f))
}

pub fn standard_reason(code: u16) -> &'static str {
    // http.client.responses (python 3.12)
    match code {
        100 => "Continue",
        101 => "Switching Protocols",
        102 => "Processing",
        103 => "Early Hints",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        207 => "Multi-Status",
        208 => "Already Reported",
        226 => "IM Used",
        300 => "Multiple Choices",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        305 => "Use Proxy",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Request Entity Too Large",
        414 => "Request-URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Requested Range Not Satisfiable",
        417 => "Expectation Failed",
        418 => "I'm a Teapot",
        421 => "Misdirected Request",
        422 => "Unprocessable Entity",
        423 => "Locked",
        424 => "Failed Dependency",
        425 => "Too Early",
        426 => "Upgrade Required",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        451 => "Unavailable For Legal Reasons",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        506 => "Variant Also Negotiates",
        507 => "Insufficient Storage",
        508 => "Loop Detected",
        510 => "Not Extended",
        511 => "Network Authentication Required",
        _ => "Unknown",
    }
}

/// An error raised during request processing (tornado.web.HTTPError)
#[derive(Debug, Clone)]
pub struct HttpError {
    pub status: u16,
    pub reason: Option<String>,
}

impl HttpError {
    pub fn new(status: u16) -> Self {
        HttpError { status, reason: None }
    }
    pub fn with_reason(status: u16, reason: impl Into<String>) -> Self {
        HttpError { status, reason: Some(reason.into()) }
    }
    /// Python ServerErrors converted via HTTPError(e.status_code, reason=str(e))
    pub fn from_server_error(e: &ServerError) -> Self {
        if e.is_exc {
            return Self::internal();
        }
        if e.is_http {
            return Self::std_from(e);
        }
        let code = if (100..=599).contains(&e.status_code) { e.status_code as u16 } else { 500 };
        HttpError::with_reason(code, e.message.clone())
    }
    /// HTTPError(e.status_code, str(e)): the message is a log message, the
    /// reason phrase is the standard one
    pub fn std_from(e: &ServerError) -> Self {
        let code = if (100..=599).contains(&e.status_code) { e.status_code as u16 } else { 500 };
        log::debug!("HTTP {}: {}", code, e.message);
        HttpError::new(code)
    }
    /// Non-HTTPError exceptions become a 500 in tornado
    pub fn internal() -> Self {
        HttpError::new(500)
    }
    pub fn reason_phrase(&self) -> String {
        match &self.reason {
            Some(r) if valid_reason(r) => r.clone(),
            Some(_) => "Unknown".into(),
            None => standard_reason(self.status).to_string(),
        }
    }
}

impl From<ServerError> for HttpError {
    fn from(e: ServerError) -> Self {
        HttpError::from_server_error(&e)
    }
}

/// tornado's RequestHandler.log_exception (tornado 6.5.8 web.py:1972) for an
/// exception that is not an HTTPError: "Uncaught exception <summary>",
/// repr(request), the traceback (the Python component's own, else synthetic)
pub(super) fn log_uncaught_exception(req: &HttpRequest, e: &ServerError, err: &HttpError) {
    let r = crate::pyfmt::py_str_repr;
    let msg = format!(
        "Uncaught exception {} {} ({})\nHTTPServerRequest(protocol={}, host={}, method={}, uri={}, version={}, remote_ip={})\n{}",
        req.method,
        req.uri,
        req.remote_ip,
        r(&req.protocol),
        r(&req.host),
        r(&req.method),
        r(&req.uri),
        r("HTTP/1.1"),
        r(&req.remote_ip),
        e.traceback().map(str::to_string).unwrap_or_else(|| synthetic_traceback(err).trim_end().to_string())
    );
    crate::loghelper::emit(log::Level::Error, "web.py", "log_exception", 1972, &msg);
}

pub(super) fn synthetic_traceback(err: &HttpError) -> String {
    format!(
        "Traceback (most recent call last):\n\n  File \"moonruster\", in _execute\n\ntornado.web.HTTPError: HTTP {}: {}\n",
        err.status,
        err.reason_phrase()
    )
}

/// Moonraker's write_error() JSON response
pub fn error_response(err: &HttpError, cors: Option<&HttpRequest>) -> HttpResponse {
    let reason = err.reason_phrase();
    let mut resp = HttpResponse::new(err.status);
    resp.reason = Some(reason.clone());
    if let Some(req) = cors {
        set_cors_headers(req, &mut resp);
    }
    if err.status == 304 {
        return resp;
    }
    let body = json!({"error": {"code": err.status, "message": reason, "traceback": synthetic_traceback(err)}});
    resp.set_header("Content-Type", "application/json; charset=UTF-8");
    resp.set_body(body.to_string());
    resp
}

pub fn set_cors_headers(req: &HttpRequest, resp: &mut HttpResponse) {
    let origin = match req.header("Origin") {
        Some(o) => o,
        None => return,
    };
    resp.set_header("Access-Control-Allow-Origin", origin);
    if req.method == "OPTIONS" {
        resp.set_header("Access-Control-Allow-Methods", "GET, POST, PUT, DELETE, OPTIONS");
        resp.set_header(
            "Access-Control-Allow-Headers",
            "Origin, Accept, Content-Type, X-Requested-With, X-CRSF-Token, Authorization, X-Access-Token, X-Api-Key",
        );
        if req.header("Access-Control-Request-Private-Network").as_deref() == Some("true") {
            resp.set_header("Access-Control-Allow-Private-Network", "true");
        }
    }
}

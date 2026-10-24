// Klipper Web Server Rest API (port of moonraker/components/application.py)
//
// Tornado's request handling semantics are reproduced here: ordered regex
// routing, argument parsing, error responses, automatic Etags, CORS.

use crate::common::*;
use crate::confighelper::ConfigHelper;
use crate::server::{server, spawn, Component};
use crate::utils;
use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode};
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{json, Map, Value};
use sha1::Digest;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub const MAX_BODY_SIZE: u64 = 50 * 1024 * 1024;
pub const MAX_WS_CONNS_DEFAULT: i64 = 50;
pub const EXCLUDED_ARGS: [&str; 4] = ["_", "token", "access_token", "connection_id"];
pub const AUTHORIZED_EXTS: [&str; 2] = [".png", ".jpg"];
pub const DEFAULT_KLIPPY_LOG_PATH: &str = "/tmp/klippy.log";

pub type Body = UnsyncBoxBody<Bytes, std::io::Error>;

pub fn full_body(data: impl Into<Bytes>) -> Body {
    Full::new(data.into()).map_err(|never| match never {}).boxed_unsync()
}

pub fn empty_body() -> Body {
    full_body(Bytes::new())
}

// ***** Request info *****

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

fn clean_argument(s: &str) -> String {
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
    for part in qs.split(|c| c == '&') {
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

fn form_unquote(s: &str) -> String {
    let replaced = s.replace('+', " ");
    percent_encoding::percent_decode_str(&replaced).decode_utf8_lossy().to_string()
}

fn is_valid_ip(ip: &str) -> bool {
    utils::parse_ip_address(ip).is_some()
}

// ***** Responses *****

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
fn valid_reason(reason: &str) -> bool {
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
fn log_uncaught_exception(req: &HttpRequest, e: &ServerError, err: &HttpError) {
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
        e.traceback.clone().unwrap_or_else(|| synthetic_traceback(err).trim_end().to_string())
    );
    crate::loghelper::emit(log::Level::Error, "web.py", "log_exception", 1972, &msg);
}

fn synthetic_traceback(err: &HttpError) -> String {
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

// ***** Routing *****

#[derive(Clone)]
pub enum HandlerKind {
    Welcome,
    Redirect,
    Rpc,
    Dynamic { api_def: Rc<ApiDefinition>, wrap_result: bool, content_type: Option<String> },
    StaticFile { path: String },
    Upload { location_prefix: String, max_upload_size: u64 },
    WebSocket,
    BridgeSocket,
}

struct Rule {
    pattern: String,
    // None for plain paths (most routes), matched by string comparison
    regex: Option<Regex>,
    kind: HandlerKind,
}

pub struct RouteMatch {
    pub kind: HandlerKind,
    pub path_args: Vec<Option<String>>,
    pub path_kwargs: Vec<(String, String)>,
}

// ***** Internal Transport *****

pub struct InternalTransport;

impl ApiTransport for InternalTransport {
    fn transport_id(&self) -> u64 {
        1
    }
    fn py_object(self: Rc<Self>) -> Option<(Rc<dyn std::any::Any>, &'static str, &'static str)> {
        Some((self, "moonraker.components.application", "InternalTransport"))
    }
}

impl Component for InternalTransport {}

impl InternalTransport {
    pub async fn call_method(&self, method_name: &str, args: Args) -> ApiResult {
        let app = server().app();
        let (req_type, api_def) = app
            .json_rpc
            .get_method(method_name)
            .ok_or_else(|| ServerError::bad(format!("No method {} available", method_name)))?;
        if !api_def.transports.contains(TransportType::INTERNAL) {
            return Err(ServerError::bad(format!("No method {} available", method_name)));
        }
        let transport: Rc<dyn ApiTransport> = Rc::new(InternalTransport);
        api_def.request(args, req_type, Some(transport), None, None).await
    }
}

// ***** Moonraker App *****

pub struct MoonrakerApp {
    pub json_rpc: Rc<JsonRpc>,
    pub api_cache: RefCell<HashMap<String, Rc<ApiDefinition>>>,
    registered_base_handlers: RefCell<Vec<String>>,
    rules: RefCell<Vec<Rule>>,
    pub use_xheaders: bool,
    pub max_upload_size: u64,
    pub max_ws_conns: i64,
    cert_path: PathBuf,
    key_path: PathBuf,
    route_prefix: String,
    template_cache: RefCell<HashMap<String, Rc<crate::components::template::JinjaTemplate>>>,
    listeners: RefCell<Vec<tokio::task::JoinHandle<()>>>,
    pub verbose: bool,
    pub http_running: Cell<bool>,
}

impl Component for JsonRpc {}

#[async_trait(?Send)]
impl Component for MoonrakerApp {}

impl MoonrakerApp {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, crate::confighelper::ConfigError> {
        let srv = server();
        let json_rpc = Rc::new(JsonRpc::new(srv.is_verbose_enabled()));
        let use_xheaders = config.getboolean_or("use_xheaders", true)?;
        let max_upload_size = config.getint_or("max_upload_size", 1024)? as u64 * 1024 * 1024;
        let cert_path = Self::get_path_option(config, "ssl_certificate_path")?;
        let key_path = Self::get_path_option(config, "ssl_key_path")?;
        let mut home_pattern = "/".to_string();
        let mut route_prefix = String::new();
        if let Some(rp) = config.get_opt("route_prefix")? {
            let parts: Vec<String> =
                rp.trim_matches('/').split('/').filter(|p| !p.is_empty()).map(utils::url_escape).collect();
            let joined = parts.join("/");
            if joined.is_empty() {
                return Err(ConfigHelper::error(format!("Invalid value for option 'route_prefix': {}", rp)));
            }
            route_prefix = format!("/{}", joined);
            home_pattern = format!("{}/?", route_prefix);
        }
        let max_ws_conns = config.getint_or("max_websocket_connections", MAX_WS_CONNS_DEFAULT)?;
        let app = Rc::new(MoonrakerApp {
            json_rpc: json_rpc.clone(),
            api_cache: RefCell::new(HashMap::new()),
            registered_base_handlers: RefCell::new(vec!["/server/redirect".into(), "/server/jsonrpc".into()]),
            rules: RefCell::new(Vec::new()),
            use_xheaders,
            max_upload_size,
            max_ws_conns,
            cert_path,
            key_path,
            route_prefix: route_prefix.clone(),
            template_cache: RefCell::new(HashMap::new()),
            listeners: RefCell::new(Vec::new()),
            verbose: srv.is_verbose_enabled(),
            http_running: Cell::new(false),
        });
        app.add_handler(&home_pattern, HandlerKind::Welcome);
        app.add_handler(&format!("{}/server/redirect", route_prefix), HandlerKind::Redirect);
        app.add_handler(&format!("{}/server/jsonrpc", route_prefix), HandlerKind::Rpc);
        srv.register_component("jsonrpc", json_rpc).map_err(crate::confighelper::ConfigError::from)?;
        srv.register_component("internal_transport", Rc::new(InternalTransport))
            .map_err(crate::confighelper::ConfigError::from)?;
        // The application component is registered by the server after
        // construction, but the handlers below need it via lookup.
        Ok(app)
    }

    /// Called by the component loader after the app has been registered
    pub fn post_register(&self) {
        let logfile = server().app_arg_str("log_file");
        if !logfile.is_empty() {
            self.register_static_file_handler("moonraker.log", &logfile, true);
        }
        self.register_static_file_handler("klippy.log", DEFAULT_KLIPPY_LOG_PATH, true);
        self.register_upload_handler("/server/files/upload", "server/files");
    }

    fn get_path_option(config: &ConfigHelper, option: &str) -> Result<PathBuf, crate::confighelper::ConfigError> {
        let path = config.get_opt_dep(option, true)?;
        let data_path = server().data_path();
        let certs_path = PathBuf::from(&data_path).join("certs");
        if !certs_path.exists() {
            let _ = std::fs::create_dir(&certs_path);
        }
        let ext = if option.contains("key") { "key" } else { "cert" };
        let item = certs_path.join(format!("moonraker.{}", ext));
        if item.exists() || path.is_none() {
            return Ok(item);
        }
        let p = path.unwrap();
        let item = utils::resolve_path(&utils::expand_user(&p));
        if !item.exists() {
            return Err(ConfigHelper::error(format!("Invalid path for option '{}', {} does not exist", option, p)));
        }
        Ok(item)
    }

    pub fn route_prefix(&self) -> &str {
        &self.route_prefix
    }

    pub fn parse_endpoint(&self, http_path: &str) -> String {
        if self.route_prefix.is_empty() || !http_path.starts_with(&self.route_prefix) {
            return http_path.to_string();
        }
        http_path[self.route_prefix.len()..].to_string()
    }

    pub fn https_enabled(&self) -> bool {
        self.cert_path.exists() && self.key_path.exists()
    }

    fn add_handler(&self, pattern: &str, kind: HandlerKind) {
        self.remove_handler(pattern);
        if !pattern.contains(|c: char| "\\.+*?()[]{}|^$".contains(c)) {
            self.rules.borrow_mut().push(Rule { pattern: pattern.to_string(), regex: None, kind });
            return;
        }
        let mut re_src = pattern.to_string();
        if !re_src.ends_with('$') {
            re_src.push('$');
        }
        let regex = match Regex::new(&format!("^(?:{})", re_src)) {
            Ok(r) => r,
            Err(e) => {
                log::error!("Invalid route pattern {}: {}", pattern, e);
                return;
            }
        };
        self.rules.borrow_mut().push(Rule { pattern: pattern.to_string(), regex: Some(regex), kind });
    }

    fn remove_handler(&self, pattern: &str) {
        self.rules.borrow_mut().retain(|r| r.pattern != pattern);
    }

    fn find_handler(&self, path: &str) -> Option<RouteMatch> {
        let rules = self.rules.borrow();
        for rule in rules.iter() {
            let regex = match &rule.regex {
                Some(r) => r,
                None if rule.pattern == path => {
                    return Some(RouteMatch { kind: rule.kind.clone(), path_args: vec![], path_kwargs: vec![] });
                }
                None => continue,
            };
            if let Some(caps) = regex.captures(path) {
                let mut path_args = Vec::new();
                let mut path_kwargs = Vec::new();
                let names: Vec<Option<&str>> = regex.capture_names().collect();
                let has_named = names.iter().any(|n| n.is_some());
                for (i, name) in names.iter().enumerate().skip(1) {
                    let val = caps.get(i).map(|m| utils::url_unescape(m.as_str()));
                    match name {
                        Some(n) => {
                            if let Some(v) = val {
                                path_kwargs.push((n.to_string(), v));
                            }
                        }
                        None => {
                            if !has_named {
                                path_args.push(val);
                            }
                        }
                    }
                }
                return Some(RouteMatch { kind: rule.kind.clone(), path_args, path_kwargs });
            }
        }
        None
    }

    pub async fn listen(self: &Rc<Self>, host: &str, port: i64, ssl_port: i64) {
        let host = if host.to_lowercase() == "all" { "" } else { host };
        if let Err(e) = self.create_http_server(port, host, None).await {
            server().add_warning(
                &format!("Failed to start HTTP server: {}.  See moonraker.log for more details.", e),
                None,
                true,
            );
        }
        if self.https_enabled() {
            if port == ssl_port {
                server().add_warning(
                    &format!(
                        "Failed to start HTTPS server.  Server options 'port' and 'ssl_port' match, both set to {}.  Modify the configuration to use different ports.",
                        port
                    ),
                    None,
                    true,
                );
                return;
            }
            log::info!("Starting secure server on port {}", ssl_port);
            match self.load_tls() {
                Ok(acceptor) => {
                    if let Err(e) = self.create_http_server(ssl_port, host, Some(acceptor)).await {
                        server().add_warning(
                            &format!("Failed to start HTTPS server: {}.  See moonraker.log for more details.", e),
                            None,
                            true,
                        );
                    }
                }
                Err(e) => {
                    server().add_warning(
                        &format!("Failed to start HTTPS server: {}.  See moonraker.log for more details.", e),
                        None,
                        true,
                    );
                }
            }
        } else {
            log::info!("SSL Certificate/Key not configured, aborting HTTPS Server startup");
        }
    }

    fn load_tls(&self) -> Result<tokio_rustls::TlsAcceptor, String> {
        let certs: Vec<_> = {
            let data = std::fs::read(&self.cert_path).map_err(|e| e.to_string())?;
            rustls_pemfile::certs(&mut &data[..]).filter_map(|c| c.ok()).collect()
        };
        let key = {
            let data = std::fs::read(&self.key_path).map_err(|e| e.to_string())?;
            rustls_pemfile::private_key(&mut &data[..]).map_err(|e| e.to_string())?.ok_or("No private key found")?
        };
        let provider = std::sync::Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
        let cfg = tokio_rustls::rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| e.to_string())?;
        Ok(tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(cfg)))
    }

    async fn create_http_server(
        self: &Rc<Self>,
        port: i64,
        address: &str,
        tls: Option<tokio_rustls::TlsAcceptor>,
    ) -> std::io::Result<()> {
        let mut listeners = Vec::new();
        if address.is_empty() || address == "::" {
            // Bind both ipv6 and ipv4 like tornado's bind_sockets
            if let Ok(l) = bind_socket(&format!("[::]:{}", port), true) {
                listeners.push(l);
            }
            match bind_socket(&format!("0.0.0.0:{}", port), false) {
                Ok(l) => listeners.push(l),
                Err(e) if listeners.is_empty() => return Err(e),
                Err(_) => {}
            }
        } else if address.contains(':') && !address.starts_with('[') {
            listeners.push(bind_socket(&format!("[{}]:{}", address, port), true)?);
        } else {
            listeners.push(bind_socket(&format!("{}:{}", address, port), false)?);
        }
        for listener in listeners {
            let app = self.clone();
            let tls = tls.clone();
            let handle = spawn(async move {
                loop {
                    let (stream, peer) = match listener.accept().await {
                        Ok(s) => s,
                        Err(e) => {
                            log::debug!("Accept error: {}", e);
                            continue;
                        }
                    };
                    let _ = stream.set_nodelay(true);
                    let app = app.clone();
                    match tls.clone() {
                        None => {
                            spawn(async move { app.serve_connection(stream, peer, "http").await });
                        }
                        Some(acc) => {
                            spawn(async move {
                                match acc.accept(stream).await {
                                    Ok(s) => app.serve_connection(s, peer, "https").await,
                                    Err(e) => log::debug!("TLS handshake failed: {}", e),
                                }
                            });
                        }
                    }
                }
            });
            self.listeners.borrow_mut().push(handle);
        }
        self.http_running.set(true);
        Ok(())
    }

    async fn serve_connection<S>(self: Rc<Self>, stream: S, peer: SocketAddr, protocol: &'static str)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let io = hyper_util::rt::TokioIo::new(stream);
        let app = self.clone();
        let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
            let app = app.clone();
            async move { Ok::<_, std::convert::Infallible>(app.handle_request(req, peer, protocol).await) }
        });
        let conn = hyper::server::conn::http1::Builder::new()
            .title_case_headers(true)
            .keep_alive(true)
            .serve_connection(io, svc)
            .with_upgrades();
        if let Err(e) = conn.await {
            log::debug!("HTTP connection error: {}", e);
        }
    }

    pub async fn close(&self) {
        for h in self.listeners.borrow_mut().drain(..) {
            h.abort();
        }
        self.api_cache.borrow_mut().clear();
    }

    // ***** Registration *****

    #[allow(clippy::too_many_arguments)]
    pub fn register_endpoint(
        &self,
        endpoint: &str,
        request_types: RequestType,
        cb: ApiCallback,
        transports: TransportType,
        wrap_result: bool,
        content_type: Option<&str>,
        auth_required: bool,
        is_remote: bool,
    ) -> Result<(), ServerError> {
        let api_def = ApiDefinition::create(&self.api_cache, endpoint, request_types, cb, transports, auth_required, is_remote)?;
        let http_path = api_def.http_path.clone();
        if self.registered_base_handlers.borrow().contains(&http_path) {
            if !is_remote {
                return Err(ServerError::bad(format!("Local endpoint '{}' already registered", endpoint)));
            }
            return Ok(());
        }
        log::debug!("Registering API: {}", api_def);
        if transports.contains(TransportType::HTTP) {
            self.add_handler(
                &format!("{}{}", self.route_prefix, http_path),
                HandlerKind::Dynamic { api_def: api_def.clone(), wrap_result, content_type: content_type.map(String::from) },
            );
        }
        self.registered_base_handlers.borrow_mut().push(http_path);
        for (rt, method) in api_def.rpc_items() {
            self.json_rpc.register_method(&method, rt, api_def.clone());
        }
        Ok(())
    }

    pub fn register_debug_endpoint(
        &self,
        endpoint: &str,
        request_types: RequestType,
        cb: ApiCallback,
        transports: TransportType,
        wrap_result: bool,
    ) -> Result<(), ServerError> {
        if !server().is_debug_enabled() {
            return Ok(());
        }
        if !endpoint.starts_with("/debug") {
            return Err(ServerError::bad("Debug Endpoints must be registered in the '/debug' path"));
        }
        self.register_endpoint(endpoint, request_types, cb, transports, wrap_result, None, true, false)
    }

    pub fn register_remote_endpoint(&self, endpoint: &str, cb: ApiCallback) -> Result<(), ServerError> {
        self.register_endpoint(
            endpoint,
            RequestType::GET | RequestType::POST,
            cb,
            TransportType::all(),
            true,
            None,
            true,
            true,
        )
    }

    pub fn register_static_file_handler(&self, pattern: &str, file_path: &str, force: bool) {
        let mut pattern = pattern.to_string();
        if !pattern.starts_with('/') {
            pattern = format!("/server/files/{}", pattern);
        }
        let p = Path::new(file_path);
        if p.is_file() || force {
            pattern.push_str("()");
        } else if p.is_dir() {
            if !pattern.ends_with('/') {
                pattern.push('/');
            }
            pattern.push_str("(.*)");
        } else {
            log::info!("Invalid file path: {}", file_path);
            return;
        }
        log::debug!("Registering static file: ({}) {}", pattern, file_path);
        self.add_handler(
            &format!("{}{}", self.route_prefix, pattern),
            HandlerKind::StaticFile { path: file_path.to_string() },
        );
    }

    pub fn register_upload_handler(&self, pattern: &str, location_prefix: &str) {
        let mut lp = location_prefix.trim_matches('/').to_string();
        if !self.route_prefix.is_empty() {
            lp = format!("{}/{}", self.route_prefix.trim_matches('/'), lp);
        }
        self.add_handler(
            &format!("{}{}", self.route_prefix, pattern),
            HandlerKind::Upload { location_prefix: lp, max_upload_size: self.max_upload_size },
        );
    }

    pub fn register_websocket_handler(&self, pattern: &str, bridge: bool) {
        let kind = if bridge { HandlerKind::BridgeSocket } else { HandlerKind::WebSocket };
        self.add_handler(&format!("{}{}", self.route_prefix, pattern), kind);
    }

    pub async fn load_template(&self, asset_name: &str) -> Result<Rc<crate::components::template::JinjaTemplate>, HttpError> {
        if let Some(t) = self.template_cache.borrow().get(asset_name) {
            return Ok(t.clone());
        }
        let asset = crate::assets::read_asset(asset_name).ok_or_else(|| HttpError::new(404))?;
        let factory: Rc<crate::components::template::TemplateFactory> =
            server().lookup_component("template").ok_or_else(HttpError::internal)?;
        let t = Rc::new(factory.create_ui_template(&asset).map_err(|_| HttpError::internal())?);
        self.template_cache.borrow_mut().insert(asset_name.to_string(), t.clone());
        Ok(t)
    }

    // ***** Request handling *****

    fn build_request(&self, parts: &hyper::http::request::Parts, peer: SocketAddr, protocol: &str) -> HttpRequest {
        let uri = parts.uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
        let (path, query) = match uri.split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (uri.clone(), String::new()),
        };
        let headers = parts.headers.clone();
        let mut remote_ip = peer.ip().to_string();
        if let std::net::IpAddr::V6(v6) = peer.ip() {
            if let Some(v4) = v6.to_ipv4_mapped() {
                remote_ip = v4.to_string();
            }
        }
        let orig_remote_ip = remote_ip.clone();
        let mut protocol = protocol.to_string();
        let hv = |n: &str| headers.get(n).map(|v| String::from_utf8_lossy(v.as_bytes()).to_string());
        if self.use_xheaders {
            let mut ip = hv("X-Forwarded-For").unwrap_or_else(|| remote_ip.clone());
            ip = ip.rsplit(',').next().unwrap_or("").trim().to_string();
            if let Some(real) = hv("X-Real-Ip") {
                ip = real;
            }
            if is_valid_ip(&ip) {
                remote_ip = ip;
            }
            let proto = hv("X-Scheme").or_else(|| hv("X-Forwarded-Proto"));
            if let Some(p) = proto {
                let p = p.rsplit(',').next().unwrap_or("").trim().to_lowercase();
                if p == "http" || p == "https" {
                    protocol = p;
                }
            }
        }
        let host = hv("Host").unwrap_or_else(|| "127.0.0.1".into());
        let mut query_arguments = IndexMap::new();
        parse_qs(&query, &mut query_arguments);
        let arguments = query_arguments.clone();
        HttpRequest {
            method: parts.method.as_str().to_string(),
            uri,
            path,
            headers,
            remote_ip,
            orig_remote_ip,
            protocol,
            host,
            query_arguments,
            arguments,
            body: Bytes::new(),
            start_time: utils::time_now(),
        }
    }

    async fn read_body(info: &mut HttpRequest, body: Incoming, max_body: u64) -> Result<(), ()> {
        if let Some(cl) = info.header("Content-Length").and_then(|v| v.trim().parse::<u64>().ok()) {
            if cl > max_body {
                return Err(());
            }
        }
        let limited = http_body_util::Limited::new(body, max_body as usize);
        info.body = match limited.collect().await {
            Ok(c) => c.to_bytes(),
            Err(_) => return Err(()),
        };
        let ctype = info.header("Content-Type").unwrap_or_default();
        let body = info.body.clone();
        parse_body_arguments(&ctype, &body, &mut info.arguments);
        Ok(())
    }

    pub async fn handle_request(self: &Rc<Self>, req: Request<Incoming>, peer: SocketAddr, protocol: &str) -> Response<Body> {
        let (parts, body) = req.into_parts();
        let route = self.find_handler(parts.uri.path());
        let kind = route.as_ref().map(|r| r.kind.clone());
        let streaming = matches!(kind, Some(HandlerKind::Upload { .. }));
        let is_ws = matches!(kind, Some(HandlerKind::WebSocket) | Some(HandlerKind::BridgeSocket));
        let mut info = self.build_request(&parts, peer, protocol);
        let mut raw_req: Option<Request<Incoming>> = None;
        if streaming || is_ws {
            raw_req = Some(Request::from_parts(parts, body));
        } else if Self::read_body(&mut info, body, MAX_BODY_SIZE).await.is_err() {
            let mut r = HttpResponse::new(400);
            r.set_header("Connection", "close");
            return self.finish(None, r, None);
        }
        const SUPPORTED: [&str; 7] = ["GET", "HEAD", "POST", "DELETE", "PATCH", "PUT", "OPTIONS"];
        let (resp, user) = if !SUPPORTED.contains(&info.method.as_str()) {
            (error_response(&HttpError::new(405), None), None)
        } else {
            match route {
                None => self.handle_not_found(&info).await,
                Some(m) => match m.kind.clone() {
                    HandlerKind::Welcome => (self.handle_welcome(&info).await, None),
                    HandlerKind::Redirect => self.handle_redirect(&info).await,
                    HandlerKind::Rpc => self.handle_rpc(&info).await,
                    HandlerKind::Dynamic { api_def, wrap_result, content_type } => {
                        self.handle_dynamic(&info, &m, api_def, wrap_result, content_type).await
                    }
                    HandlerKind::StaticFile { path } => self.handle_static(&info, &m, &path).await,
                    HandlerKind::Upload { location_prefix, max_upload_size } => {
                        self.handle_upload(raw_req.take().unwrap(), &info, &location_prefix, max_upload_size).await
                    }
                    HandlerKind::WebSocket => {
                        let (resp, user) = crate::components::websockets::serve_websocket(raw_req.take().unwrap(), &info, false).await;
                        return self.finish_ws(&info, resp, user);
                    }
                    HandlerKind::BridgeSocket => {
                        let (resp, user) = crate::components::websockets::serve_websocket(raw_req.take().unwrap(), &info, true).await;
                        return self.finish_ws(&info, resp, user);
                    }
                },
            }
        };
        self.finish(Some(&info), resp, user)
    }

    /// tornado's finish(): etag handling, content-length, logging
    fn finish(&self, info: Option<&HttpRequest>, mut resp: HttpResponse, user: Option<UserInfo>) -> Response<Body> {
        if let Some(req) = info {
            if resp.status == 200 && (req.method == "GET" || req.method == "HEAD") && !resp.has_header("Etag") {
                if let Some(b) = &resp.body_bytes {
                    let mut hasher = sha1::Sha1::new();
                    hasher.update(b);
                    let etag = format!("\"{}\"", hex::encode(hasher.finalize()));
                    let matched = check_etag(req.header("If-None-Match").as_deref(), &etag);
                    resp.set_header("Etag", etag);
                    if matched {
                        resp.status = 304;
                        resp.set_body(Bytes::new());
                    }
                }
            }
            if req.method == "HEAD" && resp.body_bytes.is_some() {
                // tornado computes content-length from the buffer, then the
                // connection drops the body for HEAD requests
                if !resp.has_header("Content-Length") {
                    let len = resp.body_bytes.as_ref().map(|b| b.len()).unwrap_or(0);
                    resp.set_header("Content-Length", len.to_string());
                }
                resp.set_body(Bytes::new());
            }
        }
        if resp.status == 204 || resp.status == 304 || (100..200).contains(&resp.status) {
            resp.remove_header("Content-Type");
            resp.remove_header("Content-Encoding");
            resp.remove_header("Content-Language");
        } else if !resp.has_header("Content-Length") {
            if let Some(b) = &resp.body_bytes {
                resp.set_header("Content-Length", b.len().to_string());
            }
        }
        if !resp.has_header("Content-Type") && !(resp.status == 204 || resp.status == 304) {
            resp.set_header("Content-Type", "text/html; charset=UTF-8");
        }
        if let Some(req) = info {
            self.log_request(req, resp.status, user.as_ref());
        }
        let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut builder = Response::builder().status(status);
        // tornado's header order: Server, Content-Type, Date, then the rest
        builder = builder.header("Server", concat!("Moonruster/", env!("CARGO_PKG_VERSION")));
        let ctype = resp.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("Content-Type")).map(|(_, v)| v.clone());
        if let Some(Ok(val)) = ctype.as_deref().map(HeaderValue::from_str) {
            builder = builder.header("Content-Type", val);
        }
        builder = builder.header("Date", httpdate::fmt_http_date(std::time::SystemTime::now()));
        for (n, v) in &resp.headers {
            if n.eq_ignore_ascii_case("Content-Type") {
                continue;
            }
            if let (Ok(name), Ok(val)) = (HeaderName::from_bytes(n.as_bytes()), HeaderValue::from_str(v)) {
                builder = builder.header(name, val);
            }
        }
        let mut response = builder.body(resp.body).unwrap_or_else(|_| Response::new(empty_body()));
        // tornado uses python's reason phrases, which differ from hyper's
        // canonical ones for some codes (413, 414, 416...)
        let reason = resp.reason.unwrap_or_else(|| standard_reason(resp.status).to_string());
        if let Ok(rp) = hyper::ext::ReasonPhrase::try_from(reason.into_bytes()) {
            response.extensions_mut().insert(rp);
        }
        response
    }

    fn finish_ws(&self, info: &HttpRequest, resp: Result<Response<Body>, HttpResponse>, user: Option<UserInfo>) -> Response<Body> {
        match resp {
            Ok(r) => {
                self.log_request(info, 101, user.as_ref());
                r
            }
            Err(e) => self.finish(Some(info), e, user),
        }
    }

    fn log_request(&self, req: &HttpRequest, status: u16, user: Option<&UserInfo>) {
        if !self.verbose && [200, 204, 206, 304].contains(&status) {
            return;
        }
        let request_time = 1000.0 * req.request_time();
        let username = user.map(|u| u.username.clone()).unwrap_or_else(|| "No User".into());
        let summary = if req.orig_remote_ip != req.remote_ip && req.is_proxied() {
            format!("{} {} ({} via {})", req.method, req.uri, req.remote_ip, req.orig_remote_ip)
        } else {
            format!("{} {} ({})", req.method, req.uri, req.remote_ip)
        };
        let msg = format!("{} {} [{}] {:.2}ms", status, summary, username, request_time);
        if status < 400 {
            log::info!("{}", msg);
        } else if status < 500 {
            log::warn!("{}", msg);
        } else {
            log::error!("{}", msg);
        }
    }

    /// AuthorizedRequestHandler.prepare(): returns (cors_enabled, user)
    pub async fn prepare_auth(&self, req: &HttpRequest, auth_required: bool) -> (bool, Result<Option<UserInfo>, HttpError>) {
        let auth: Option<Rc<crate::components::authorization::Authorization>> = server().lookup_component("authorization");
        match auth {
            None => (false, Ok(None)),
            Some(auth) => {
                let origin = req.header("Origin");
                let cors = auth.check_cors(origin.as_deref()).await;
                let user = auth.authenticate_request(req, auth_required).await;
                (cors, user)
            }
        }
    }

    async fn handle_not_found(&self, req: &HttpRequest) -> (HttpResponse, Option<UserInfo>) {
        let (cors, user) = self.prepare_auth(req, true).await;
        let cors_req = if cors { Some(req) } else { None };
        match user {
            Err(e) => (error_response(&e, cors_req), None),
            Ok(u) => (error_response(&HttpError::new(404), cors_req), u),
        }
    }

    fn options_response(&self, req: &HttpRequest, cors: bool) -> HttpResponse {
        if cors {
            let mut r = HttpResponse::new(204);
            set_cors_headers(req, &mut r);
            r
        } else {
            error_response(&HttpError::new(405), None)
        }
    }

    async fn handle_dynamic(
        &self,
        req: &HttpRequest,
        m: &RouteMatch,
        api_def: Rc<ApiDefinition>,
        wrap_result: bool,
        content_type: Option<String>,
    ) -> (HttpResponse, Option<UserInfo>) {
        let (cors, user) = self.prepare_auth(req, api_def.auth_required).await;
        let cors_req = if cors { Some(req) } else { None };
        let user = match user {
            Err(e) => return (error_response(&e, cors_req), None),
            Ok(u) => u,
        };
        let req_type = match req.method.as_str() {
            "GET" => RequestType::GET,
            "POST" => RequestType::POST,
            "DELETE" => RequestType::DELETE,
            "OPTIONS" => return (self.options_response(req, cors), user),
            _ => return (error_response(&HttpError::new(405), cors_req), user),
        };
        if !api_def.request_types.contains(req_type) {
            return (error_response(&HttpError::new(405), cors_req), user);
        }
        let args = match parse_dynamic_args(req, &api_def, m) {
            Ok(a) => a,
            Err(e) => return (error_response(&e, cors_req), user),
        };
        let transport = self.get_associated_websocket(req);
        let req_str = format!("{} {}", req.method, req.path);
        self.log_debug(&api_def, &format!("HTTP Request::{}", req_str), &Value::Object(args.clone()));
        let ip = utils::parse_ip_address(&req.remote_ip);
        let result = api_def.request(args, req_type, transport, ip, user.clone()).await;
        let result = match result {
            Ok(r) => r,
            Err(e) => {
                if self.verbose {
                    log::error!("API Request Failure: {}", e);
                }
                let err = HttpError::from_server_error(&e);
                if e.is_exc {
                    log_uncaught_exception(req, &e, &err);
                }
                return (error_response(&err, cors_req), user);
            }
        };
        let result = if wrap_result { json!({"result": result}) } else { result };
        self.log_debug(&api_def, &format!("HTTP Response::{}", req_str), &result);
        let mut resp = HttpResponse::new(200);
        if cors {
            set_cors_headers(req, &mut resp);
        }
        match result {
            Value::Null => {
                resp.status = 204;
            }
            Value::Object(_) => {
                resp.set_header("Content-Type", "application/json; charset=UTF-8");
                resp.set_body(result.to_string());
            }
            Value::String(s) => {
                if let Some(ct) = content_type {
                    resp.set_header("Content-Type", ct);
                }
                resp.set_body(s);
            }
            _ => {
                // tornado's write() rejects other types
                return (error_response(&HttpError::internal(), cors_req), user);
            }
        }
        (resp, user)
    }

    fn log_debug(&self, api_def: &ApiDefinition, header: &str, args: &Value) {
        if !self.verbose {
            return;
        }
        let ep = &api_def.endpoint;
        let out = match args {
            Value::Object(o) if ep.starts_with("/access") || ep.starts_with("/machine/sudo/password") => {
                Value::Object(o.keys().map(|k| (k.clone(), Value::String("<sanitized>".into()))).collect())
            }
            Value::String(s) if s.starts_with("<html>") => Value::String("<html>".into()),
            other => other.clone(),
        };
        log::debug!("{}::{}", header, crate::pyfmt::py_str(&out));
    }

    fn get_associated_websocket(&self, req: &HttpRequest) -> Option<Rc<dyn ApiTransport>> {
        let conn_id = req.get_argument("connection_id")?;
        let id: u64 = conn_id.trim().parse().ok()?;
        let wsm = server().wsm();
        wsm.get_client_ws(id).map(|c| c as Rc<dyn ApiTransport>)
    }

    async fn handle_rpc(&self, req: &HttpRequest) -> (HttpResponse, Option<UserInfo>) {
        let (cors, user) = self.prepare_auth(req, false).await;
        let cors_req = if cors { Some(req) } else { None };
        let user = match user {
            Err(e) => return (error_response(&e, cors_req), None),
            Ok(u) => u,
        };
        match req.method.as_str() {
            "POST" => {}
            "OPTIONS" => return (self.options_response(req, cors), user),
            _ => return (error_response(&HttpError::new(405), cors_req), user),
        }
        let ctype = req.header("Content-Type").unwrap_or_default();
        if !ctype.trim().starts_with("application/json") {
            return (
                error_response(&HttpError::new(400), cors_req),
                user,
            );
        }
        let transport: Rc<dyn ApiTransport> = Rc::new(HttpRpcTransport {
            user: user.clone(),
            ip: utils::parse_ip_address(&req.remote_ip),
        });
        let result = self.json_rpc.dispatch(&req.body, transport).await;
        let mut resp = HttpResponse::new(200);
        if cors {
            set_cors_headers(req, &mut resp);
        }
        if let Some(r) = result {
            resp.set_header("Content-Type", "application/json; charset=UTF-8");
            resp.set_body(r);
        }
        (resp, user)
    }

    async fn handle_redirect(&self, req: &HttpRequest) -> (HttpResponse, Option<UserInfo>) {
        let (cors, user) = self.prepare_auth(req, false).await;
        let cors_req = if cors { Some(req) } else { None };
        let user = match user {
            Err(e) => return (error_response(&e, cors_req), None),
            Ok(u) => u,
        };
        match req.method.as_str() {
            "GET" => {}
            "OPTIONS" => return (self.options_response(req, cors), user),
            _ => return (error_response(&HttpError::new(405), cors_req), user),
        }
        let url = match req.get_argument("url") {
            Some(u) => u,
            None => {
                let body_args: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
                match body_args.get("url") {
                    Some(u) => crate::pyfmt::py_str(u),
                    None => return (error_response(&HttpError::new(400), cors_req), user),
                }
            }
        };
        let auth: Option<Rc<crate::components::authorization::Authorization>> = server().lookup_component("authorization");
        let allowed = match auth {
            None => false,
            Some(a) => a.check_cors(Some(url.trim_end_matches('/'))).await,
        };
        if !allowed {
            return (error_response(&HttpError::new(400), cors_req), user);
        }
        let mut resp = HttpResponse::new(302);
        if cors {
            set_cors_headers(req, &mut resp);
        }
        resp.set_header("Location", url);
        (resp, user)
    }

    async fn handle_welcome(&self, req: &HttpRequest) -> HttpResponse {
        if req.method != "GET" {
            return error_response(&HttpError::new(405), None);
        }
        let srv = server();
        let mut summary: Vec<String> = Vec::new();
        let auth: Option<Rc<crate::components::authorization::Authorization>> = srv.lookup_component("authorization");
        let (authorized, cors_enabled) = match &auth {
            Some(a) => {
                let authorized = a.authenticate_request(req, true).await.is_ok();
                if authorized {
                    summary.push("Your device is authorized to access Moonraker's API.".into());
                } else {
                    summary.push("Your device is not authorized to access Moonraker's API. This is normal if you intend to use API Key authentication or log in as an authenticated user.  Otherwise you need to add your IP address to the 'trusted_clients' option in the [authorization] section of moonraker.conf.".into());
                }
                let ce = a.cors_enabled();
                if ce {
                    summary.push("CORS is enabled.  Cross origin requests will be allowed for origins that match one of the patterns specified in the 'cors_domain' option of the [authorization] section.".into());
                } else {
                    summary.push("All cross origin requests will be blocked by the browser. The 'cors_domains' option in [authorization] must be  configured to enable CORS.".into());
                }
                (authorized, ce)
            }
            None => {
                summary.push("The [authorization] component is not enabled in moonraker.conf.  All connections will be considered trusted.".into());
                summary.push("All cross origin requests will be blocked by the browser.  The [authorization] section in moonraker.conf must be configured to enable CORS.".into());
                (true, false)
            }
        };
        let kconn = srv.klippy();
        let kstate = kconn.state();
        if kstate != KlippyState::DISCONNECTED {
            summary.push(format!("Klipper reports {}", kconn.state_message_for(kstate).to_lowercase()));
        } else {
            summary.push("Moonraker is not currently connected to Klipper.  Make sure that the klipper service has successfully started and that its unix is enabled.".into());
        }
        let announcements = match srv.lookup_component::<crate::components::announcements::Announcements>("announcements") {
            Some(a) => a.get_announcements(false).await,
            None => Value::Array(vec![]),
        };
        let (sudo_requested, sudo_msg, linux_user, local_ip, service_name) =
            match srv.lookup_component::<crate::components::machine::Machine>("machine") {
                Some(m) => {
                    let svc = m.get_moonraker_service_info();
                    (
                        m.sudo_requested(),
                        m.sudo_request_messages().join("<br/>"),
                        m.linux_user(),
                        m.public_ip().unwrap_or_else(|| "unknown".into()),
                        svc.get("unit_name").and_then(|v| v.as_str()).unwrap_or("unknown").to_string(),
                    )
                }
                None => (false, String::new(), utils::get_user(), "unknown".into(), "unknown".into()),
            };
        let context = json!({
            "remote_ip": req.remote_ip,
            "authorized": crate::pyfmt::py_bool(authorized),
            "cors_enabled": crate::pyfmt::py_bool(cors_enabled),
            "version": srv.get_app_arg("software_version").unwrap_or(Value::Null),
            "ws_count": srv.wsm().get_count(),
            "klippy_state": kstate.as_str(),
            "warnings": srv.get_warnings(),
            "summary": summary,
            "announcements": announcements,
            "sudo_requested": sudo_requested,
            "sudo_request_message": sudo_msg,
            "linux_user": linux_user,
            "local_ip": local_ip,
            "service_name": service_name,
            "hostname": utils::hostname(),
        });
        let tmpl = match self.load_template("welcome.html").await {
            Ok(t) => t,
            Err(e) => return error_response(&e, None),
        };
        match tmpl.render_async(&context).await {
            Ok(html) => {
                let mut r = HttpResponse::new(200);
                r.set_body(html);
                r
            }
            Err(_) => error_response(&HttpError::internal(), None),
        }
    }

    async fn handle_static(&self, req: &HttpRequest, m: &RouteMatch, root: &str) -> (HttpResponse, Option<UserInfo>) {
        // AuthorizedFileHandler: images are served without auth for GET
        let need_auth = if req.method != "GET" {
            true
        } else {
            !AUTHORIZED_EXTS.contains(&utils::splitext_ext(&req.path).to_lowercase().as_str())
        };
        let (cors, user) = self.prepare_auth(req, need_auth).await;
        let cors_req = if cors { Some(req) } else { None };
        let user = match user {
            Err(e) => return (error_response(&e, cors_req), None),
            Ok(u) => u,
        };
        let path_arg = m.path_args.first().cloned().flatten().unwrap_or_default();
        let resp = match req.method.as_str() {
            "GET" => self.static_get(req, root, &path_arg, true).await,
            "HEAD" => self.static_get(req, root, &path_arg, false).await,
            "DELETE" => self.static_delete(req).await,
            "OPTIONS" => Ok(self.options_response(req, cors)),
            _ => Err(HttpError::new(405)),
        };
        match resp {
            Ok(mut r) => {
                if cors {
                    set_cors_headers(req, &mut r);
                }
                (r, user)
            }
            Err(e) => (error_response(&e, cors_req), user),
        }
    }

    async fn static_delete(&self, req: &HttpRequest) -> Result<HttpResponse, HttpError> {
        let endpoint = self.parse_endpoint(&req.path);
        let trimmed = endpoint.trim_start_matches('/');
        let path = trimmed.splitn(3, '/').last().unwrap_or("").to_string();
        let path = utils::url_unescape(&path);
        let fm: Rc<crate::components::file_manager::FileManager> =
            server().lookup_component("file_manager").ok_or_else(HttpError::internal)?;
        let filename = fm.delete_file(&path).await.map_err(|e| HttpError::std_from(&e))?;
        Ok(HttpResponse::json(200, &json!({"result": filename})))
    }

    async fn static_get(&self, req: &HttpRequest, root: &str, path: &str, include_body: bool) -> Result<HttpResponse, HttpError> {
        // StaticFileHandler.get_absolute_path / validate_absolute_path
        let abs_root = utils::normpath(&utils::absolute(Path::new(root)));
        let joined = if path.is_empty() { abs_root.clone() } else { abs_root.join(path) };
        let absolute_path = utils::normpath(&joined);
        let mut root_str = abs_root.to_string_lossy().to_string();
        if !root_str.ends_with('/') {
            root_str.push('/');
        }
        let abs_str = format!("{}/", absolute_path.to_string_lossy());
        if !abs_str.starts_with(&root_str) {
            return Err(HttpError::new(403));
        }
        if !absolute_path.exists() {
            return Err(HttpError::new(404));
        }
        if !absolute_path.is_file() {
            return Err(HttpError::new(403));
        }
        let fm: Option<Rc<crate::components::file_manager::FileManager>> = server().lookup_component("file_manager");
        if let Some(fm) = fm {
            fm.check_reserved_path(&absolute_path.to_string_lossy(), false)
                .map_err(|e| HttpError::std_from(&e))?;
        }
        let meta = std::fs::metadata(&absolute_path).map_err(|_| HttpError::new(404))?;
        let size = meta.len();
        let mtime_secs = {
            use std::os::unix::fs::MetadataExt;
            meta.mtime()
        };
        let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime_secs.max(0) as u64);
        let mut resp = HttpResponse::new(200);
        resp.set_header("Accept-Ranges", "bytes");
        resp.set_header("Last-Modified", httpdate::fmt_http_date(modified));
        resp.set_header("Content-Type", guess_content_type(&absolute_path.to_string_lossy()));
        if req.arguments.contains_key("v") {
            let cache_time = 86400u64 * 365 * 10;
            resp.set_header("Expires", httpdate::fmt_http_date(std::time::SystemTime::now() + std::time::Duration::from_secs(cache_time)));
            resp.set_header("Cache-Control", format!("max-age={}", cache_time));
        }
        let basename = absolute_path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let ascii: String = basename.chars().map(|c| if c.is_ascii() { c } else { '?' }).collect::<String>().replace('"', "\\\"");
        let utf8 = utils::url_quote(&basename);
        resp.set_header("Content-Disposition", format!("attachment; filename=\"{}\"; filename*=UTF-8''{}", ascii, utf8));
        // should_return_304 (If-None-Match is ignored by moonraker)
        if let Some(ims) = req.header("If-Modified-Since") {
            if let Ok(since) = httpdate::parse_http_date(&ims) {
                if since >= modified {
                    resp.status = 304;
                    return Ok(resp);
                }
            }
        }
        let mut start: Option<i64> = None;
        let mut end: Option<i64> = None;
        let size_i = size as i64;
        if let Some(range) = req.header("Range") {
            if let Some((s, e)) = parse_request_range(&range) {
                start = s;
                end = e;
                if let Some(st) = start {
                    if st < 0 {
                        start = Some((st + size_i).max(0));
                    }
                }
                if (start.is_some() && (start.unwrap() >= size_i || (end.is_some() && start.unwrap() >= end.unwrap())))
                    || end == Some(0)
                {
                    // headers from set_headers() are kept
                    resp.status = 416;
                    resp.set_header("Content-Type", "text/plain");
                    resp.set_header("Content-Range", format!("bytes */{}", size));
                    return Ok(resp);
                }
                if let Some(e) = end {
                    if e > size_i {
                        end = Some(size_i);
                    }
                }
                if size_i != end.unwrap_or(size_i) - start.unwrap_or(0) {
                    resp.status = 206;
                    let s = start.unwrap_or(0);
                    let e = end.unwrap_or(size_i);
                    resp.set_header("Content-Range", format!("bytes {}-{}/{}", s, e - 1, size));
                }
            }
        }
        let content_length = match (start, end) {
            (Some(s), Some(e)) => e - s,
            (None, Some(e)) => e,
            (Some(s), None) => size_i - s,
            (None, None) => size_i,
        };
        resp.set_header("Content-Length", content_length.to_string());
        if include_body {
            let file = tokio::fs::File::open(&absolute_path).await.map_err(|_| HttpError::new(404))?;
            let body = file_stream_body(file, start.unwrap_or(0) as u64, content_length.max(0) as u64).await;
            resp.set_stream(body);
        } else {
            resp.set_stream(empty_body());
        }
        Ok(resp)
    }

    async fn handle_upload(
        &self,
        req: Request<Incoming>,
        info: &HttpRequest,
        location_prefix: &str,
        max_upload_size: u64,
    ) -> (HttpResponse, Option<UserInfo>) {
        let (cors, user) = self.prepare_auth(info, true).await;
        let cors_req = if cors { Some(info) } else { None };
        let user = match user {
            Err(e) => return (error_response(&e, cors_req), None),
            Ok(u) => u,
        };
        let ctype = info.header("Content-Type").unwrap_or_default();
        log::info!("Upload Request Received from {}\nContent-Type: {}", info.remote_ip, ctype);
        let fm: Rc<crate::components::file_manager::FileManager> = match server().lookup_component("file_manager") {
            Some(f) => f,
            None => return (error_response(&HttpError::internal(), cors_req), user),
        };
        // check_write_enabled raises a ServerError (not an HTTPError) in prepare -> 500
        if fm.check_write_enabled().is_err() {
            return (error_response(&HttpError::internal(), cors_req), user);
        }
        match info.method.as_str() {
            "POST" => {}
            "OPTIONS" => return (self.options_response(info, cors), user),
            _ => return (error_response(&HttpError::new(405), cors_req), user),
        }
        if let Some(cl) = info.header("Content-Length").and_then(|v| v.trim().parse::<u64>().ok()) {
            if cl > max_upload_size {
                let mut r = HttpResponse::new(400);
                r.set_header("Connection", "close");
                return (r, user);
            }
        }
        let result = crate::components::file_manager::upload::process_upload(req, info, &fm, max_upload_size, user.clone()).await;
        let result = match result {
            Ok(r) => r,
            Err(e) => return (error_response(&e, cors_req), user),
        };
        let mut resp = HttpResponse::new(201);
        if cors {
            set_cors_headers(info, &mut resp);
        }
        let item = result.get("item").cloned().unwrap_or(Value::Null);
        if let (Some(root), Some(fpath)) = (item.get("root").and_then(|v| v.as_str()), item.get("path").and_then(|v| v.as_str())) {
            let fpath: Vec<String> = fpath.split('/').map(utils::url_escape).collect();
            let location = format!("{}://{}/{}/{}/{}", info.protocol, info.host, location_prefix, root, fpath.join("/"));
            log::debug!("Upload Location header set: {}", location);
            resp.set_header("Location", location);
        }
        resp.set_header("Content-Type", "application/json; charset=UTF-8");
        resp.set_body(result.to_string());
        (resp, user)
    }
}

fn bind_socket(addr: &str, v6only: bool) -> std::io::Result<tokio::net::TcpListener> {
    let sockaddr: SocketAddr = addr.parse().map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{}", e)))?;
    let socket = if sockaddr.is_ipv6() { tokio::net::TcpSocket::new_v6()? } else { tokio::net::TcpSocket::new_v4()? };
    socket.set_reuseaddr(true)?;
    if v6only {
        let fd = std::os::fd::AsRawFd::as_raw_fd(&socket);
        let one: libc::c_int = 1;
        unsafe {
            libc::setsockopt(fd, libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, &one as *const _ as *const libc::c_void, 4);
        }
    }
    socket.bind(sockaddr)?;
    socket.listen(128)
}

fn check_etag(if_none_match: Option<&str>, etag: &str) -> bool {
    let header = match if_none_match {
        Some(h) => h,
        None => return false,
    };
    if header.trim() == "*" {
        return true;
    }
    let strip = |s: &str| s.trim().trim_start_matches("W/").to_string();
    let target = strip(etag);
    header.split(',').any(|t| strip(t) == target)
}

/// tornado.httputil._parse_request_range
fn parse_request_range(range_header: &str) -> Option<(Option<i64>, Option<i64>)> {
    let (unit, value) = range_header.split_once('=')?;
    let (unit, value) = (unit.trim(), value.trim());
    if unit != "bytes" {
        return None;
    }
    let (start_b, end_b) = value.split_once('-')?;
    let parse = |s: &str| -> Result<Option<i64>, ()> {
        let s = s.trim();
        if s.is_empty() {
            Ok(None)
        } else {
            s.parse::<i64>().map(Some).map_err(|_| ())
        }
    };
    let mut start = parse(start_b).ok()?;
    let mut end = parse(end_b).ok()?;
    if let Some(e) = end {
        if start.is_none() {
            if e != 0 {
                start = Some(-e);
                end = None;
            }
        } else {
            end = Some(e + 1);
        }
    }
    Some((start, end))
}

pub fn guess_content_type(path: &str) -> String {
    let lower = path.to_lowercase();
    let ext = utils::splitext_ext(&lower);
    match ext.as_str() {
        ".gz" | ".tgz" => return "application/gzip".into(),
        ".bz2" | ".xz" | ".br" | ".z" => return "application/octet-stream".into(),
        ".log" | ".gcode" | ".cfg" | ".txt" | ".conf" => return "text/plain".into(),
        _ => {}
    }
    match mime_guess::from_path(path).first() {
        Some(m) => m.essence_str().to_string(),
        None => "application/octet-stream".into(),
    }
}

async fn file_stream_body(mut file: tokio::fs::File, start: u64, len: u64) -> Body {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    if start > 0 {
        let _ = file.seek(std::io::SeekFrom::Start(start)).await;
    }
    let reader = file.take(len);
    let stream = tokio_util::io::ReaderStream::with_capacity(reader, 64 * 1024);
    let body = StreamBody::new(stream.map(|r| r.map(Frame::data)));
    body.boxed_unsync()
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

fn parse_multipart_fields(boundary: &str, data: &[u8], arguments: &mut IndexMap<String, Vec<String>>) {
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

/// DynamicRequestHandler.parse_args
fn parse_dynamic_args(req: &HttpRequest, api_def: &ApiDefinition, m: &RouteMatch) -> Result<Args, HttpError> {
    let mut args = Map::new();
    if api_def.need_object_parser() {
        let mut objs = Map::new();
        for key in req.arguments.keys() {
            if EXCLUDED_ARGS.contains(&key.as_str()) {
                continue;
            }
            let val = req.get_argument(key).unwrap_or_default();
            if val.is_empty() {
                objs.insert(key.clone(), Value::Null);
            } else {
                objs.insert(key.clone(), Value::Array(val.split(',').map(|s| Value::String(s.to_string())).collect()));
            }
        }
        log::debug!("Parsed Arguments: {}", crate::pyfmt::py_repr(&Value::Object(objs.clone())));
        args.insert("objects".into(), Value::Object(objs));
    } else {
        for key in req.arguments.keys() {
            if EXCLUDED_ARGS.contains(&key.as_str()) {
                continue;
            }
            let val = req.get_argument(key).unwrap_or_default();
            match key.rsplit_once(':') {
                None => {
                    args.insert(key.clone(), Value::String(val));
                }
                Some((name, hint)) => {
                    args.insert(name.to_string(), convert_type(&val, hint));
                }
            }
        }
    }
    let ctype = req.header("Content-Type").unwrap_or_default();
    if ctype.trim().starts_with("application/json") {
        if let Ok(v) = serde_json::from_slice::<Value>(&req.body) {
            match v {
                Value::Object(o) => {
                    for (k, v) in o {
                        args.insert(k, v);
                    }
                }
                // dict.update() with a non mapping raises an uncaught exception
                _ => return Err(HttpError::internal()),
            }
        }
    }
    for (k, v) in &m.path_kwargs {
        args.insert(k.clone(), Value::String(v.clone()));
    }
    Ok(args)
}

fn convert_type(value: &str, hint: &str) -> Value {
    match hint {
        "int" => match crate::pyfmt::py_int(value) {
            Some(i) => Value::from(i),
            None => {
                log::error!("Argument conversion error: Hint: {}, Arg: {}", hint, value);
                Value::String(value.to_string())
            }
        },
        "float" => match crate::pyfmt::py_float(value) {
            Some(f) => utils::float_value(f),
            None => {
                log::error!("Argument conversion error: Hint: {}, Arg: {}", hint, value);
                Value::String(value.to_string())
            }
        },
        "bool" => Value::Bool(value.to_lowercase() == "true"),
        "json" => match serde_json::from_str(value) {
            Ok(v) => v,
            Err(_) => {
                log::error!("Argument conversion error: Hint: {}, Arg: {}", hint, value);
                Value::String(value.to_string())
            }
        },
        _ => {
            log::info!("No conversion method for type hint {}", hint);
            Value::String(value.to_string())
        }
    }
}

/// RPCHandler acting as an APITransport
pub struct HttpRpcTransport {
    user: Option<UserInfo>,
    ip: Option<std::net::IpAddr>,
}

impl ApiTransport for HttpRpcTransport {
    fn transport_type(&self) -> TransportType {
        TransportType::HTTP
    }
    fn py_object(self: Rc<Self>) -> Option<(Rc<dyn std::any::Any>, &'static str, &'static str)> {
        Some((self, "moonraker.components.application", "RPCHandler"))
    }
    fn user_info(&self) -> Option<UserInfo> {
        self.user.clone()
    }
    fn ip_addr(&self) -> Option<std::net::IpAddr> {
        self.ip
    }
    fn screen_rpc_request(&self, api_def: &ApiDefinition, _rt: RequestType, _args: &mut Args) -> Result<(), ServerError> {
        if self.user.is_none() && api_def.auth_required {
            return Err(ServerError::new("Unauthorized", 401));
        }
        if api_def.endpoint == "objects/subscribe" {
            return Err(ServerError::new("Subscriptions not available for HTTP transport", 404));
        }
        Ok(())
    }
    fn transport_id(&self) -> u64 {
        self as *const _ as u64
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<MoonrakerApp>, crate::confighelper::ConfigError> {
    MoonrakerApp::new(config)
}

#[cfg(test)]
mod tests {
    use super::split_host_and_port as s;

    #[test]
    fn host_and_port() {
        assert_eq!(s("printer.local:7125"), ("printer.local".into(), Some(7125)));
        assert_eq!(s("printer.local"), ("printer.local".into(), None));
        assert_eq!(s("[::1]:80"), ("[::1]".into(), Some(80)));
        assert_eq!(s("[::1]"), ("[::1]".into(), None));
        assert_eq!(s("::1"), (":".into(), Some(1)));
        assert_eq!(s("host:99999"), ("host".into(), None));
        assert_eq!(s(":80"), (":80".into(), None));
    }
}

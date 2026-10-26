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

mod handlers;
mod request;
mod response;

pub use request::*;
pub use response::*;

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
    matcher: Matcher,
    kind: HandlerKind,
}

/// How a route pattern (tornado's regex, anchored at both ends) matches
enum Matcher {
    /// a plain path (most routes): string comparison
    Exact,
    /// `<literal>(.*)` (the file roots): the literal as a prefix, the rest
    /// of the path (no newline, as `.`) the one argument; no regex engine
    Prefix(String),
    Regex(Regex),
}

const REGEX_CHARS: &str = "\\.+*?()[]{}|^$";

/// What `^(?:<literal>(.*))$` captures in `path`
fn prefix_rest<'a>(literal: &str, path: &'a str) -> Option<&'a str> {
    path.strip_prefix(literal).filter(|rest| !rest.contains('\n'))
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
    listeners: crate::server::TaskSet,
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
            listeners: Default::default(),
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
        if !pattern.contains(|c: char| REGEX_CHARS.contains(c)) {
            self.rules.borrow_mut().push(Rule { pattern: pattern.to_string(), matcher: Matcher::Exact, kind });
            return;
        }
        if let Some(literal) = pattern.strip_suffix("(.*)").filter(|l| !l.contains(|c: char| REGEX_CHARS.contains(c))) {
            let matcher = Matcher::Prefix(literal.to_string());
            self.rules.borrow_mut().push(Rule { pattern: pattern.to_string(), matcher, kind });
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
        self.rules.borrow_mut().push(Rule { pattern: pattern.to_string(), matcher: Matcher::Regex(regex), kind });
    }

    fn remove_handler(&self, pattern: &str) {
        self.rules.borrow_mut().retain(|r| r.pattern != pattern);
    }

    fn find_handler(&self, path: &str) -> Option<RouteMatch> {
        let rules = self.rules.borrow();
        for rule in rules.iter() {
            let regex = match &rule.matcher {
                Matcher::Regex(r) => r,
                Matcher::Exact if rule.pattern == path => {
                    return Some(RouteMatch { kind: rule.kind.clone(), path_args: vec![], path_kwargs: vec![] });
                }
                Matcher::Prefix(literal) => match prefix_rest(literal, path) {
                    Some(rest) => {
                        let path_args = vec![Some(utils::url_unescape(rest))];
                        return Some(RouteMatch { kind: rule.kind.clone(), path_args, path_kwargs: vec![] });
                    }
                    None => continue,
                },
                Matcher::Exact => continue,
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
            self.listeners.push(handle);
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
        self.listeners.cancel_all();
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

    /// MoonrakerApp.remove_endpoint: the definition, its HTTP route and its
    /// JSON-RPC methods go
    pub fn remove_endpoint(&self, endpoint: &str) {
        let api_def = self.api_cache.borrow_mut().remove(endpoint);
        if let Some(api_def) = api_def {
            log::debug!("Removing Endpoint: {}", endpoint);
            self.registered_base_handlers.borrow_mut().retain(|p| *p != api_def.http_path);
            self.remove_handler(&format!("{}{}", self.route_prefix, api_def.http_path));
            for method in &api_def.rpc_methods {
                self.json_rpc.remove_method(method);
            }
        }
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
            server().get::<crate::components::template::TemplateFactory>().ok_or_else(HttpError::internal)?;
        let t = Rc::new(factory.create_ui_template(&asset).map_err(|_| HttpError::internal())?);
        self.template_cache.borrow_mut().insert(asset_name.to_string(), t.clone());
        Ok(t)
    }

}

fn bind_socket(addr: &str, v6only: bool) -> std::io::Result<tokio::net::TcpListener> {
    let sockaddr: SocketAddr = addr.parse().map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{}", e)))?;
    let socket = if sockaddr.is_ipv6() { tokio::net::TcpSocket::new_v6()? } else { tokio::net::TcpSocket::new_v4()? };
    socket.set_reuseaddr(true)?;
    if v6only {
        let fd = std::os::fd::AsRawFd::as_raw_fd(&socket);
        let one: libc::c_int = 1;
        // SAFETY: fd is the socket's open descriptor; one is a c_int that outlives the call and 4 is its size
        unsafe {
            libc::setsockopt(fd, libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, &one as *const _ as *const libc::c_void, 4);
        }
    }
    socket.bind(sockaddr)?;
    socket.listen(128)
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

#[cfg(test)]
mod route_tests {
    // a file root's prefix matcher captures what its regex captures
    #[test]
    fn prefix_matcher_as_regex() {
        let literal = "/server/files/gcodes/";
        let re = regex::Regex::new(&format!("^(?:{}(.*))$", literal)).unwrap();
        for path in [
            "/server/files/gcodes/",
            "/server/files/gcodes/a%20b.gcode",
            "/server/files/gcodes/sub/dir/x.gcode",
            "/server/files/gcodes",
            "/server/files/gcodesx/y",
            "/server/files/config/x",
            "/server/files/gcodes/a\nb",
            "/server/files/gcodes/a\rb",
            "",
        ] {
            let by_regex = re.captures(path).map(|c| c.get(1).unwrap().as_str());
            assert_eq!(super::prefix_rest(literal, path), by_regex, "{path:?}");
        }
    }
}

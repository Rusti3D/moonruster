// API Key Based Authorization (port of moonraker/components/authorization.py)

use crate::common::*;
use crate::components::application::{HttpError, HttpRequest};
use crate::components::database::{MoonrakerDatabase, SqlTableDefinition};
use crate::confighelper::{Bounds, ConfigHelper};
use crate::server::{server, spawn, Component};
use async_trait::async_trait;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::net::IpAddr;
use std::rc::Rc;

const ONESHOT_TIMEOUT: f64 = 5.0;
const TRUSTED_CONNECTION_TIMEOUT: f64 = 3600.0;
const PROXY_CLEAR_INTERVAL: f64 = 7200.0;
const FQDN_CACHE_TIMEOUT: f64 = 84000.0;
const PRUNE_CHECK_TIME: f64 = 300.0;
const USER_TABLE: &str = "authorized_users";
const AUTH_SOURCES: [&str; 2] = ["moonraker", "ldap"];
const HASH_ITER: u32 = 100000;
pub const API_USER: &str = "_API_KEY_USER_";
pub const TRUSTED_USER: &str = "_TRUSTED_USER_";
const RESERVED_USERS: [&str; 2] = [API_USER, TRUSTED_USER];
const JWT_EXP_TIME: i64 = 3600;

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
fn jwt_json(data: Vec<u8>) -> Result<Value, JwtError> {
    serde_json::from_slice(&data).map_err(|e| {
        let msg = e.to_string();
        JwtError::Json(data, msg)
    })
}

/// payload[key] of a decoded JWT payload
fn jwt_item<'a>(payload: &'a Value, key: &str) -> Result<&'a Value, JwtError> {
    let name = crate::pyfmt::py_type_name(payload);
    match payload {
        Value::Object(m) => m.get(key).ok_or_else(|| JwtError::Exc("builtins.KeyError", key.to_string())),
        Value::Array(_) => Err(JwtError::Exc("builtins.TypeError", "list indices must be integers or slices, not str".into())),
        Value::String(_) => Err(JwtError::Exc("builtins.TypeError", "string indices must be integers, not 'str'".into())),
        _ => Err(JwtError::Exc("builtins.TypeError", format!("'{}' object is not subscriptable", name))),
    }
}

fn pbkdf2_hex(password: &str, salt: &[u8]) -> String {
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), salt, HASH_ITER, &mut out);
    hex::encode(out)
}

fn compare_digest(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

struct UserSqlDefinition;

impl SqlTableDefinition for UserSqlDefinition {
    fn name(&self) -> &str {
        USER_TABLE
    }
    fn version(&self) -> i64 {
        1
    }
    fn prototype(&self) -> String {
        format!(
            "{} (\n    username TEXT PRIMARY KEY NOT NULL,\n    password TEXT NOT NULL,\n    created_on REAL NOT NULL,\n    salt TEXT NOT NULL,\n    source TEXT NOT NULL,\n    jwt_secret TEXT,\n    jwk_id TEXT,\n    groups pyjson\n)",
            USER_TABLE
        )
    }
    fn migrate(&self, last_version: i64, db: &MoonrakerDatabase) -> Result<(), ServerError> {
        if last_version != 0 {
            return Ok(());
        }
        let mut users = db.get_namespace("authorized_users", false)?;
        let api_user = users.shift_remove(API_USER).filter(|v| v.is_object()).unwrap_or(json!({}));
        let mut vals: Vec<UserInfo> = vec![{
            let mut u = UserInfo::new(
                API_USER,
                api_user.get("api_key").and_then(|v| v.as_str()).map(String::from).unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string()).as_str(),
            );
            if let Some(c) = api_user.get("created_on").and_then(|v| v.as_f64()) {
                u.created_on = c;
            }
            u
        }];
        for (key, user) in users {
            match UserInfo::from_dict(&user) {
                Some(u) if user.is_object() => vals.push(u),
                _ => log::info!("Auth migration, skipping invalid value: {} {}", key, crate::pyfmt::py_str(&user)),
            }
        }
        for u in vals {
            db.sql_execute(
                &format!("INSERT OR IGNORE INTO {} VALUES(?,?,?,?,?,?,?,?)", USER_TABLE),
                &user_params(&u),
            )?;
        }
        db.wipe_local_namespace("authorized_users")?;
        Ok(())
    }
}

fn user_params(u: &UserInfo) -> Vec<rusqlite::types::Value> {
    use rusqlite::types::Value as SV;
    vec![
        SV::Text(u.username.clone()),
        SV::Text(u.password.clone()),
        SV::Real(u.created_on),
        SV::Text(u.salt.clone()),
        SV::Text(u.source.clone()),
        u.jwt_secret.clone().map(SV::Text).unwrap_or(SV::Null),
        u.jwk_id.clone().map(SV::Text).unwrap_or(SV::Null),
        SV::Blob(serde_json::to_vec(&u.groups).unwrap_or_default()),
    ]
}

struct TrustedUser {
    user: UserInfo,
    expires_at: f64,
}

struct FqdnInfo {
    domain: String,
    expires_at: f64,
}

pub struct Authorization {
    login_timeout: i64,
    force_logins: bool,
    default_source: String,
    enable_api_key: bool,
    max_logins: Option<i64>,
    failed_logins: RefCell<HashMap<IpAddr, i64>>,
    fqdn_cache: RefCell<HashMap<IpAddr, FqdnInfo>>,
    ldap_enabled: bool,
    users: RefCell<IndexMap<String, UserInfo>>,
    api_key: RefCell<String>,
    issuer: String,
    public_jwks: RefCell<HashMap<String, Value>>,
    trusted_users: RefCell<HashMap<IpAddr, TrustedUser>>,
    trusted_proxies: RefCell<Vec<IpAddr>>,
    last_proxy_clear_time: Cell<f64>,
    oneshot_tokens: RefCell<HashMap<String, (Option<IpAddr>, Option<UserInfo>, tokio::task::JoinHandle<()>)>>,
    cors_domains: Vec<Regex>,
    cors_patterns: Vec<String>,
    trusted_ips: Vec<IpAddr>,
    trusted_ranges: Vec<ipnet::IpNet>,
    trusted_domains: Vec<String>,
    prune_task: RefCell<Option<tokio::task::JoinHandle<()>>>,
}

#[async_trait(?Send)]
impl Component for Authorization {
    fn has_init(&self) -> bool {
        true
    }
    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        let db = server().database();
        let rows = db.sql_query(&format!("SELECT * FROM {}", USER_TABLE), &[])?;
        {
            let mut users = self.users.borrow_mut();
            for r in rows {
                if let Some(u) = UserInfo::from_dict(&Value::Object(r)) {
                    users.insert(u.username.clone(), u);
                }
            }
        }
        if self.initialize_users() {
            self.sync_user_table()?;
        }
        let a = Rc::downgrade(&self);
        *self.prune_task.borrow_mut() = Some(spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs_f64(PRUNE_CHECK_TIME)).await;
                match a.upgrade() {
                    Some(a) => a.prune_conn_handler(),
                    None => return,
                }
            }
        }));
        Ok(())
    }
    async fn close(self: Rc<Self>) {
        if let Some(t) = self.prune_task.borrow_mut().take() {
            t.abort();
        }
    }
}

macro_rules! auth_endpoint {
    ($srv:expr, $auth:expr, $path:expr, $rt:expr, $method:ident, $auth_req:expr) => {{
        let a = $auth.clone();
        let _ = $srv.register_endpoint_ext(
            $path,
            $rt,
            callback(move |req: WebRequest| {
                let a = a.clone();
                async move { a.$method(req).await }
            }),
            TransportType::HTTP | TransportType::WEBSOCKET,
            true,
            None,
            $auth_req,
        );
    }};
}

impl Authorization {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, crate::confighelper::ConfigError> {
        let srv = server();
        let login_timeout = config.getint_or("login_timeout", 90)?;
        let force_logins = config.getboolean_or("force_logins", false)?;
        let mut default_source = config.get_or("default_source", "moonraker")?.to_lowercase();
        let enable_api_key = config.getboolean_or("enable_api_key", true)?;
        let max_logins = config.getint_opt("max_login_attempts", Bounds::above(0.0))?;
        if !AUTH_SOURCES.contains(&default_source.as_str()) {
            srv.add_warning(
                &format!(
                    "[authorization]: option 'default_source' - Invalid value '{}', falling back to 'moonraker'.",
                    default_source
                ),
                None,
                true,
            );
            default_source = "moonraker".into();
        }
        let mut ldap_enabled = false;
        if config.has_section("ldap") {
            ldap_enabled = srv.load_component_optional(config, "ldap");
        }
        if default_source == "ldap" && !ldap_enabled {
            srv.add_warning(
                "[authorization]: Option 'default_source' set to 'ldap', however [ldap] section failed to load or not configured",
                None,
                true,
            );
        }
        let db = srv.database();
        db.register_table(&UserSqlDefinition).map_err(crate::confighelper::ConfigError::from)?;
        let hi = srv.get_host_info();
        let issuer = format!("http://{}:{}", hi["hostname"].as_str().unwrap_or(""), hi["port"]);
        let mut cors_domains = Vec::new();
        let mut cors_patterns = Vec::new();
        let bad_re = Regex::new(r"^.+\.[^:]*\*").unwrap();
        for domain in config.getlist_opt("cors_domains", Some(vec![]), Some("\n"))?.unwrap_or_default() {
            if bad_re.is_match(&domain) {
                srv.add_warning(
                    &format!(
                        "[authorization]: Unsafe domain '{}' in option 'cors_domains'. Wildcards are not permitted in the top level domain.",
                        domain
                    ),
                    None,
                    true,
                );
                continue;
            }
            if domain.ends_with('/') {
                srv.add_warning(
                    &format!(
                        "[authorization]: Invalid domain '{}' in option 'cors_domains'.  Domain's cannot contain a trailing slash.",
                        domain
                    ),
                    None,
                    true,
                );
            } else {
                let pat = domain.replace('.', "\\.").replace('*', ".*");
                if let Ok(re) = Regex::new(&format!("^(?:{})", pat)) {
                    cors_domains.push(re);
                    cors_patterns.push(pat);
                }
            }
        }
        let mut trusted_ips = Vec::new();
        let mut trusted_ranges = Vec::new();
        let mut trusted_domains = Vec::new();
        let host_re = Regex::new(r"^([a-z0-9]+(-[a-z0-9]+)*\.?)+[a-z]{2,}$").unwrap();
        for val in config.getlist_opt("trusted_clients", Some(vec![]), Some("\n"))?.unwrap_or_default() {
            if let Ok(ip) = val.parse::<IpAddr>() {
                trusted_ips.push(ip);
                continue;
            }
            match val.parse::<ipnet::IpNet>() {
                Ok(net) => {
                    if net.trunc() != net {
                        srv.add_warning(
                            &format!("[authorization]: Invalid CIDR expression '{}' in option 'trusted_clients'", val),
                            None,
                            true,
                        );
                        continue;
                    }
                    trusted_ranges.push(net);
                    continue;
                }
                Err(_) => {
                    // python's ip_network also accepts a bare address without prefix
                }
            }
            if host_re.is_match(&val) {
                trusted_domains.push(val.to_lowercase());
            } else {
                srv.add_warning(
                    &format!("[authorization]: Invalid domain name '{}' in option 'trusted_clients'", val),
                    None,
                    true,
                );
            }
        }
        let mut tc: Vec<String> = trusted_ips.iter().map(|i| i.to_string()).collect();
        tc.extend(trusted_ranges.iter().map(|r| r.to_string()));
        tc.extend(trusted_domains.iter().cloned());
        log::info!(
            "Authorization Configuration Loaded\nTrusted Clients:\n{}\nCORS Domains:\n{}",
            tc.join("\n"),
            cors_patterns.join("\n")
        );
        let auth = Rc::new(Authorization {
            login_timeout,
            force_logins,
            default_source,
            enable_api_key,
            max_logins,
            failed_logins: RefCell::new(HashMap::new()),
            fqdn_cache: RefCell::new(HashMap::new()),
            ldap_enabled,
            users: RefCell::new(IndexMap::new()),
            api_key: RefCell::new(uuid::Uuid::new_v4().simple().to_string()),
            issuer,
            public_jwks: RefCell::new(HashMap::new()),
            trusted_users: RefCell::new(HashMap::new()),
            trusted_proxies: RefCell::new(Vec::new()),
            last_proxy_clear_time: Cell::new(crate::utils::time_now()),
            oneshot_tokens: RefCell::new(HashMap::new()),
            cors_domains,
            cors_patterns,
            trusted_ips,
            trusted_ranges,
            trusted_domains,
            prune_task: RefCell::new(None),
        });
        auth_endpoint!(srv, auth, "/access/login", RequestType::POST, handle_login, false);
        auth_endpoint!(srv, auth, "/access/logout", RequestType::POST, handle_logout, true);
        auth_endpoint!(srv, auth, "/access/refresh_jwt", RequestType::POST, handle_refresh_jwt, false);
        auth_endpoint!(srv, auth, "/access/user", RequestType::all(), handle_user_request, true);
        auth_endpoint!(srv, auth, "/access/users/list", RequestType::GET, handle_list_request, true);
        auth_endpoint!(srv, auth, "/access/user/password", RequestType::POST, handle_password_reset, true);
        auth_endpoint!(srv, auth, "/access/api_key", RequestType::GET | RequestType::POST, handle_apikey_request, true);
        auth_endpoint!(srv, auth, "/access/oneshot_token", RequestType::GET, handle_oneshot_request, true);
        auth_endpoint!(srv, auth, "/access/info", RequestType::GET, handle_info_request, false);
        let wsm = srv.wsm();
        wsm.register_notification("authorization:user_created", None, None);
        wsm.register_notification("authorization:user_deleted", None, Some("logout"));
        wsm.register_notification("authorization:user_logged_out", None, Some("logout"));
        Ok(auth)
    }

    fn sync_user(&self, username: &str) -> Result<(), ServerError> {
        let user = self.users.borrow().get(username).cloned().ok_or_else(|| ServerError::bad("Unknown user"))?;
        server().database().sql_execute(&format!("REPLACE INTO {} VALUES(?,?,?,?,?,?,?,?)", USER_TABLE), &user_params(&user))?;
        Ok(())
    }

    fn sync_user_table(&self) -> Result<(), ServerError> {
        let db = server().database();
        db.sql_execute(&format!("DELETE FROM {}", USER_TABLE), &[])?;
        let params: Vec<Vec<rusqlite::types::Value>> = self.users.borrow().values().map(user_params).collect();
        if params.is_empty() {
            return Ok(());
        }
        db.sql_executemany(&format!("INSERT INTO {} VALUES(?,?,?,?,?,?,?,?)", USER_TABLE), &params)?;
        Ok(())
    }

    fn initialize_users(&self) -> bool {
        let mut need_sync = false;
        let api_user = self.users.borrow().get(API_USER).cloned();
        match api_user {
            None => {
                need_sync = true;
                let key = self.api_key.borrow().clone();
                self.users.borrow_mut().insert(API_USER.into(), UserInfo::new(API_USER, &key));
            }
            Some(u) => *self.api_key.borrow_mut() = u.password.clone(),
        }
        let names: Vec<String> = self.users.borrow().keys().cloned().collect();
        for username in names {
            if username == API_USER {
                continue;
            }
            let user = self.users.borrow().get(&username).cloned().unwrap();
            if let Some(secret) = &user.jwt_secret {
                match (Self::load_private_key(secret), &user.jwk_id) {
                    (Ok(key), Some(jwk_id)) => {
                        self.public_jwks.borrow_mut().insert(jwk_id.clone(), Self::generate_public_jwk(&key));
                    }
                    _ => {
                        log::info!("Invalid jwk found for user, removing");
                        let mut u = user.clone();
                        u.jwt_secret = None;
                        u.jwk_id = None;
                        self.users.borrow_mut().insert(username.clone(), u);
                        need_sync = true;
                    }
                }
            }
        }
        need_sync
    }

    async fn handle_apikey_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        if req.get_request_type() == RequestType::POST {
            let key = uuid::Uuid::new_v4().simple().to_string();
            *self.api_key.borrow_mut() = key.clone();
            if let Some(u) = self.users.borrow_mut().get_mut(API_USER) {
                u.password = key;
            }
            self.sync_user(API_USER)?;
        }
        Ok(json!(self.api_key.borrow().clone()))
    }

    async fn handle_oneshot_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let ip = req.get_ip_address();
        let user = req.get_current_user().cloned();
        Ok(json!(self.get_oneshot_token(ip, user)))
    }

    async fn handle_login(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let ip = req.get_ip_address();
        if let Some(ip) = ip {
            if self.check_logins_maxed(ip) {
                return Err(ServerError::http(401, "Unauthorized, Maximum Login Attempts Reached"));
            }
        }
        match self.login_jwt_user(&req, false).await {
            Ok(r) => {
                if let Some(ip) = ip {
                    self.failed_logins.borrow_mut().remove(&ip);
                }
                Ok(r)
            }
            Err(e) => {
                if let Some(ip) = ip {
                    *self.failed_logins.borrow_mut().entry(ip).or_insert(0) += 1;
                }
                Err(e)
            }
        }
    }

    fn delayed_event(event: &'static str, username: String) {
        spawn(async move {
            crate::utils::sleep_ordered(std::time::Duration::from_millis(5)).await;
            let _ = server().send_event(event, vec![json!({"username": username})]);
        });
    }

    async fn handle_logout(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let user = req.get_current_user().cloned().ok_or_else(|| ServerError::bad("No user logged in"))?;
        let username = user.username.clone();
        if RESERVED_USERS.contains(&username.as_str()) {
            return Err(ServerError::bad(format!("Invalid log out request for user {}", username)));
        }
        let jwk_id = {
            let mut users = self.users.borrow_mut();
            let u = users.get_mut(&username).ok_or_else(|| ServerError::exc(format!("'{}'", username)))?;
            let id = u.jwk_id.take();
            u.jwt_secret = None;
            id
        };
        if let Some(id) = jwk_id {
            self.public_jwks.borrow_mut().remove(&id);
        }
        self.sync_user(&username)?;
        Self::delayed_event("authorization:user_logged_out", username.clone());
        Ok(json!({"username": username, "action": "user_logged_out"}))
    }

    async fn handle_info_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let mut sources = vec!["moonraker"];
        if self.ldap_enabled {
            sources.push("ldap");
        }
        let login_req = self.force_logins && self.users.borrow().len() > 1;
        let user = req.get_current_user();
        let trusted = if user.map(|u| u.username == TRUSTED_USER).unwrap_or(false) {
            Value::Bool(true)
        } else if let Some(ip) = req.get_ip_address() {
            Value::Bool(self.check_authorized_ip(ip).await)
        } else {
            Value::Null
        };
        Ok(json!({
            "default_source": self.default_source,
            "available_sources": sources,
            "login_required": login_req,
            "trusted": trusted,
        }))
    }

    async fn handle_refresh_jwt(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let refresh_token = req.get_str("refresh_token")?;
        let user = self.decode_jwt(&refresh_token, "refresh", true).map_err(|_| ServerError::new("Invalid Refresh Token", 401))?;
        let (secret, jwk_id) = match (&user.jwt_secret, &user.jwk_id) {
            (Some(s), Some(j)) => (s.clone(), j.clone()),
            _ => return Err(ServerError::new("User not logged in", 401)),
        };
        let key = Self::load_private_key(&secret)?;
        let token = self.generate_jwt(&user.username, &jwk_id, &key, "access", JWT_EXP_TIME);
        Ok(json!({"username": user.username, "token": token, "source": user.source, "action": "user_jwt_refresh"}))
    }

    async fn handle_user_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let rt = req.get_request_type();
        if rt == RequestType::GET {
            return Ok(match req.get_current_user() {
                None => json!({"username": null, "source": null, "created_on": null}),
                Some(u) => json!({"username": u.username, "source": u.source, "created_on": u.created_on}),
            });
        } else if rt == RequestType::POST {
            return self.login_jwt_user(&req, true).await;
        } else if rt == RequestType::DELETE {
            return self.delete_jwt_user(&req).await;
        }
        Err(ServerError::bad("Invalid Request Method"))
    }

    async fn handle_list_request(self: Rc<Self>, _req: WebRequest) -> ApiResult {
        let list: Vec<Value> = self
            .users
            .borrow()
            .values()
            .filter(|u| u.username != API_USER)
            .map(|u| json!({"username": u.username, "source": u.source, "created_on": u.created_on}))
            .collect();
        Ok(json!({"users": list}))
    }

    async fn handle_password_reset(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let password = req.get_str("password")?;
        let new_pass = req.get_str("new_password")?;
        let user = req.get_current_user().cloned().ok_or_else(|| ServerError::bad("No Current User"))?;
        let username = user.username.clone();
        if user.source == "ldap" {
            return Err(ServerError::bad(format!("Can´t Reset password for ldap user {}", username)));
        }
        if RESERVED_USERS.contains(&username.as_str()) {
            return Err(ServerError::bad(format!("Invalid Reset Request for user {}", username)));
        }
        let salt = hex::decode(&user.salt).map_err(|e| ServerError::exc(e.to_string()))?;
        if !compare_digest(&pbkdf2_hex(&password, &salt), &user.password) {
            return Err(ServerError::bad("Invalid Password"));
        }
        let new_hashed = pbkdf2_hex(&new_pass, &salt);
        if let Some(u) = self.users.borrow_mut().get_mut(&username) {
            u.password = new_hashed;
        }
        self.sync_user(&username)?;
        Ok(json!({"username": username, "action": "user_password_reset"}))
    }

    async fn login_jwt_user(&self, req: &WebRequest, new_user_request: bool) -> ApiResult {
        let username = req.get_str("username")?;
        let password = req.get_str("password")?;
        let source = req.get_str_or("source", &self.default_source).to_lowercase();
        if !AUTH_SOURCES.contains(&source.as_str()) {
            return Err(ServerError::bad(format!("Invalid 'source': {}", source)));
        }
        if RESERVED_USERS.contains(&username.as_str()) {
            return Err(ServerError::bad(format!("Invalid Request for user {}", username)));
        }
        let is_local_user = source == "moonraker";
        let mut need_create = new_user_request;
        if source == "ldap" {
            if new_user_request {
                return Err(ServerError::bad("Invalid Request to create new LDAP User"));
            }
            let ldap: Rc<crate::components::ldap::MoonrakerLdap> = server()
                .lookup_component("ldap")
                .ok_or_else(|| ServerError::new("LDAP authentication not available", 401))?;
            ldap.authenticate_ldap_user(&username, &password).await?;
            need_create = !self.users.borrow().contains_key(&username);
        }
        let mut user_info;
        if need_create {
            if self.users.borrow().contains_key(&username) {
                return Err(ServerError::bad(format!("User {} already exists", username)));
            }
            let (salt, hashed) = if is_local_user {
                let salt: [u8; 32] = rand::random();
                (salt.to_vec(), pbkdf2_hex(&password, &salt))
            } else {
                (Vec::new(), String::new())
            };
            user_info = UserInfo::new(&username, &hashed);
            user_info.salt = hex::encode(&salt);
            user_info.source = source.clone();
            self.users.borrow_mut().insert(username.clone(), user_info.clone());
            self.sync_user(&username)?;
        } else {
            user_info = self
                .users
                .borrow()
                .get(&username)
                .cloned()
                .ok_or_else(|| ServerError::bad(format!("Unregistered User: {}", username)))?;
            if user_info.source != source {
                return Err(ServerError::new(
                    format!("Moonraker cannot authenticate user '{}', must specify source '{}'", username, user_info.source),
                    401,
                ));
            }
            if is_local_user {
                let salt = hex::decode(&user_info.salt).unwrap_or_default();
                if !compare_digest(&pbkdf2_hex(&password, &salt), &user_info.password) {
                    return Err(ServerError::bad("Invalid Password"));
                }
            }
        }
        let (private_key, jwk_id) = match user_info.jwt_secret.clone() {
            None => {
                let seed: [u8; 32] = rand::random();
                let key = SigningKey::from_bytes(&seed);
                let tok: [u8; 32] = rand::random();
                let jwk_id = base64url_encode(&tok);
                user_info.jwt_secret = Some(hex::encode(seed));
                user_info.jwk_id = Some(jwk_id.clone());
                self.users.borrow_mut().insert(username.clone(), user_info.clone());
                self.sync_user(&username)?;
                self.public_jwks.borrow_mut().insert(jwk_id.clone(), Self::generate_public_jwk(&key));
                (key, jwk_id)
            }
            Some(secret) => {
                let key = Self::load_private_key(&secret)?;
                if user_info.jwk_id.is_none() {
                    let tok: [u8; 32] = rand::random();
                    user_info.jwk_id = Some(base64url_encode(&tok));
                }
                (key, user_info.jwk_id.clone().unwrap())
            }
        };
        let token = self.generate_jwt(&username, &jwk_id, &private_key, "access", JWT_EXP_TIME);
        let refresh_token = self.generate_jwt(&username, &jwk_id, &private_key, "refresh", self.login_timeout * 86400);
        let conn = req.get_client_connection();
        if new_user_request {
            Self::delayed_event("authorization:user_created", username.clone());
        } else if let Some(c) = conn {
            c.set_user_info(user_info.clone());
        }
        Ok(json!({
            "username": username,
            "token": token,
            "source": user_info.source,
            "refresh_token": refresh_token,
            "action": if new_user_request { "user_created" } else { "user_logged_in" },
        }))
    }

    async fn delete_jwt_user(&self, req: &WebRequest) -> ApiResult {
        let username = req.get_str("username")?;
        if let Some(cur) = req.get_current_user() {
            if cur.username == username {
                return Err(ServerError::bad(format!("Cannot delete logged in user {}", cur.username)));
            }
        }
        if RESERVED_USERS.contains(&username.as_str()) {
            return Err(ServerError::bad(format!("Invalid Request for reserved user {}", username)));
        }
        let user = self
            .users
            .borrow()
            .get(&username)
            .cloned()
            .ok_or_else(|| ServerError::bad(format!("No registered user: {}", username)))?;
        if let Some(id) = &user.jwk_id {
            self.public_jwks.borrow_mut().remove(id);
        }
        self.users.borrow_mut().shift_remove(&username);
        server()
            .database()
            .sql_execute(&format!("DELETE FROM {} WHERE username = ?", USER_TABLE), &[rusqlite::types::Value::Text(username.clone())])?;
        Self::delayed_event("authorization:user_deleted", username.clone());
        Ok(json!({"username": username, "action": "user_deleted"}))
    }

    fn generate_jwt(&self, username: &str, jwk_id: &str, key: &SigningKey, token_type: &str, exp_time: i64) -> String {
        let curtime = crate::utils::time_now() as i64;
        let payload = json!({
            "iss": self.issuer,
            "aud": "Moonraker",
            "iat": curtime,
            "exp": curtime + exp_time,
            "username": username,
            "token_type": token_type,
        });
        let header = json!({"kid": jwk_id, "alg": "EdDSA", "typ": "JWT"});
        let msg = format!("{}.{}", base64url_encode(header.to_string().as_bytes()), base64url_encode(payload.to_string().as_bytes()));
        let sig = key.sign(msg.as_bytes());
        format!("{}.{}", msg, base64url_encode(&sig.to_bytes()))
    }

    pub fn decode_jwt(&self, token: &str, token_type: &str, check_exp: bool) -> Result<UserInfo, ServerError> {
        self.decode_jwt_exc(token, token_type, check_exp).map_err(ServerError::from)
    }

    /// decode_jwt with the Python exception each step raises
    pub fn decode_jwt_exc(&self, token: &str, token_type: &str, check_exp: bool) -> Result<UserInfo, JwtError> {
        let (message, sig) = token.rsplit_once('.').ok_or(JwtError::Unpack(1))?;
        let parts: Vec<&str> = message.split('.').collect();
        if parts.len() != 2 {
            return Err(JwtError::Unpack(parts.len()));
        }
        let header = jwt_json(base64url_decode(parts[0])?)?;
        let sig_bytes = base64url_decode(sig)?;
        let Value::Object(header) = header else {
            return Err(JwtError::Exc("builtins.AttributeError", format!("'{}' object has no attribute 'get'", crate::pyfmt::py_type_name(&header))));
        };
        if header.get("typ").and_then(|v| v.as_str()) != Some("JWT") || header.get("alg").and_then(|v| v.as_str()) != Some("EdDSA") {
            return Err(ServerError::bad("Invalid JWT header").into());
        }
        let jwk = match header.get("kid") {
            Some(k @ (Value::Array(_) | Value::Object(_))) => return Err(JwtError::Unhashable(k.clone(), "contains")),
            Some(Value::String(k)) => self.public_jwks.borrow().get(k).cloned(),
            _ => None,
        };
        let jwk = jwk.ok_or_else(|| ServerError::bad("Invalid key ID"))?;
        let public_key = Self::public_key_from_jwk(&jwk)?;
        // libnacl's crypto_sign_open(sig_bytes + message)
        let signature = ed25519_dalek::Signature::from_slice(&sig_bytes);
        if signature.and_then(|s| public_key.verify_strict(message.as_bytes(), &s)).is_err() {
            return Err(JwtError::Exc("builtins.ValueError", "Failed to validate message".into()));
        }
        let payload = jwt_json(base64url_decode(parts[1])?)?;
        let ptype = jwt_item(&payload, "token_type")?;
        if ptype.as_str() != Some(token_type) {
            return Err(ServerError::new(
                format!("JWT Token type mismatch: Expected {}, Recd: {}", token_type, crate::pyfmt::py_str(ptype)),
                401,
            )
            .into());
        }
        if jwt_item(&payload, "iss")?.as_str() != Some(self.issuer.as_str()) {
            return Err(ServerError::new("Invalid JWT Issuer", 401).into());
        }
        if jwt_item(&payload, "aud")?.as_str() != Some("Moonraker") {
            return Err(ServerError::new("Invalid JWT Audience", 401).into());
        }
        if check_exp {
            let exp = jwt_item(&payload, "exp")?;
            let exp = match exp {
                Value::Number(n) => n.as_f64().unwrap_or(0.0),
                Value::Bool(b) => *b as i64 as f64,
                other => {
                    return Err(JwtError::Exc(
                        "builtins.TypeError",
                        format!("'<' not supported between instances of '{}' and 'int'", crate::pyfmt::py_type_name(other)),
                    ))
                }
            };
            if exp < crate::utils::time_now().trunc() {
                return Err(ServerError::new("JWT Expired", 401).into());
            }
        }
        // payload.get('username', "") (the payload is a dict here)
        let user = match payload.get("username") {
            None => self.users.borrow().get("").cloned(),
            Some(k @ (Value::Array(_) | Value::Object(_))) => return Err(JwtError::Unhashable(k.clone(), "get")),
            Some(Value::String(k)) => self.users.borrow().get(k).cloned(),
            Some(_) => None,
        };
        user.ok_or_else(|| ServerError::new("Unknown user", 401).into())
    }

    pub fn validate_jwt(&self, token: &str) -> Result<UserInfo, ServerError> {
        self.decode_jwt(token, "access", true).map_err(|e| {
            if e.is_exc {
                ServerError::new(format!("Failed to decode JWT: {}", e.message), 401)
            } else {
                e
            }
        })
    }

    pub fn validate_api_key(&self, api_key: &str) -> Result<Option<UserInfo>, ServerError> {
        if !self.enable_api_key {
            log::debug!("API Key received when API Key authentication is disabled");
            return Ok(None);
        }
        if api_key.trim().is_empty() {
            return Ok(None);
        }
        if compare_digest(api_key, &self.api_key.borrow()) {
            return Ok(self.users.borrow().get(API_USER).cloned());
        }
        Err(ServerError::new("Invalid API Key", 401))
    }

    fn load_private_key(secret: &str) -> Result<SigningKey, ServerError> {
        let err = || ServerError::new("Error decoding private key, user data may be corrupt", 500);
        let bytes = hex::decode(secret).map_err(|_| err())?;
        let seed: [u8; 32] = bytes.as_slice().try_into().map_err(|_| err())?;
        Ok(SigningKey::from_bytes(&seed))
    }

    fn generate_public_jwk(key: &SigningKey) -> Value {
        json!({
            "x": base64url_encode(key.verifying_key().as_bytes()),
            "kty": "OKP",
            "crv": "Ed25519",
            "use": "sig",
        })
    }

    fn public_key_from_jwk(jwk: &Value) -> Result<VerifyingKey, ServerError> {
        if jwk.get("kty").and_then(|v| v.as_str()) != Some("OKP") {
            return Err(ServerError::bad("Not an Octet Key Pair"));
        }
        if jwk.get("crv").and_then(|v| v.as_str()) != Some("Ed25519") {
            return Err(ServerError::bad("Invalid Curve"));
        }
        let x = jwk.get("x").and_then(|v| v.as_str()).ok_or_else(|| ServerError::bad("No 'x' argument in jwk"))?;
        let key = base64url_decode(x)?;
        let arr: [u8; 32] = key.as_slice().try_into().map_err(|_| ServerError::exc("Invalid public key"))?;
        VerifyingKey::from_bytes(&arr).map_err(|e| ServerError::exc(e.to_string()))
    }

    fn prune_conn_handler(&self) {
        let cur_time = crate::utils::time_now();
        self.trusted_users.borrow_mut().retain(|ip, u| {
            if cur_time >= u.expires_at {
                log::info!("Trusted Connection Expired, IP: {}", ip);
                false
            } else {
                true
            }
        });
        self.fqdn_cache.borrow_mut().retain(|ip, f| {
            if cur_time >= f.expires_at {
                log::info!("Cached FQDN Expired, IP: {}, domain: {}", ip, f.domain);
                false
            } else {
                true
            }
        });
        if cur_time - self.last_proxy_clear_time.get() >= PROXY_CLEAR_INTERVAL {
            self.last_proxy_clear_time.set(cur_time);
            self.trusted_proxies.borrow_mut().clear();
        }
    }

    pub fn get_oneshot_token(self: &Rc<Self>, ip: Option<IpAddr>, user: Option<UserInfo>) -> String {
        let bytes: [u8; 20] = rand::random();
        let token = base32_encode(&bytes);
        let a = Rc::downgrade(self);
        let t = token.clone();
        let hdl = spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs_f64(ONESHOT_TIMEOUT)).await;
            if let Some(a) = a.upgrade() {
                a.oneshot_tokens.borrow_mut().remove(&t);
            }
        });
        self.oneshot_tokens.borrow_mut().insert(token.clone(), (ip, user, hdl));
        token
    }

    fn check_json_web_token(&self, req: &HttpRequest, required: bool) -> Result<Option<UserInfo>, HttpError> {
        let auth_token = match req.header("Authorization") {
            None => match req.header("X-Access-Token") {
                Some(t) => Some(t),
                None => req.query_arguments.get("access_token").and_then(|v| v.last().cloned()),
            },
            Some(t) => match t.strip_prefix("Bearer ") {
                Some(rest) => Some(rest.to_string()),
                None => return Ok(None),
            },
        };
        match auth_token {
            Some(t) if !t.is_empty() => match self.decode_jwt(&t, "access", required) {
                Ok(u) => Ok(Some(u)),
                Err(e) => {
                    log::error!("JWT Decode Error {}\n{}", t, e);
                    Err(HttpError::new(401))
                }
            },
            _ => Ok(None),
        }
    }

    pub async fn check_authorized_ip(&self, ip: IpAddr) -> bool {
        if self.trusted_ips.contains(&ip) {
            return true;
        }
        if self.trusted_ranges.iter().any(|r| r.contains(&ip)) {
            return true;
        }
        if !self.trusted_domains.is_empty() {
            let cached = self.fqdn_cache.borrow().get(&ip).map(|f| f.domain.clone());
            let fqdn = match cached {
                Some(f) => f,
                None => {
                    let lookup = tokio::task::spawn_blocking(move || getfqdn(ip));
                    match tokio::time::timeout(std::time::Duration::from_secs(5), lookup).await {
                        Ok(Ok(f)) => {
                            let f = f.to_lowercase();
                            self.fqdn_cache.borrow_mut().insert(
                                ip,
                                FqdnInfo { domain: f.clone(), expires_at: crate::utils::time_now() + FQDN_CACHE_TIMEOUT },
                            );
                            f
                        }
                        _ => {
                            log::info!("Call to socket.getfqdn() timed out");
                            return false;
                        }
                    }
                }
            };
            return self.trusted_domains.contains(&fqdn);
        }
        false
    }

    async fn check_trusted_connection(&self, ip: Option<IpAddr>) -> Option<UserInfo> {
        let ip = ip?;
        let curtime = crate::utils::time_now();
        let exp_time = curtime + TRUSTED_CONNECTION_TIMEOUT;
        if let Some(tu) = self.trusted_users.borrow_mut().get_mut(&ip) {
            tu.expires_at = exp_time;
            return Some(tu.user.clone());
        }
        if self.check_authorized_ip(ip).await {
            log::info!("Trusted Connection Detected, IP: {}", ip);
            let mut user = UserInfo::new(TRUSTED_USER, "");
            user.created_on = curtime;
            self.trusted_users.borrow_mut().insert(ip, TrustedUser { user: user.clone(), expires_at: exp_time });
            return Some(user);
        }
        None
    }

    fn check_oneshot_token(&self, token: &str, cur_ip: Option<IpAddr>) -> Option<UserInfo> {
        let entry = self.oneshot_tokens.borrow_mut().remove(token);
        match entry {
            Some((ip, user, hdl)) => {
                hdl.abort();
                if cur_ip != ip {
                    log::info!(
                        "Oneshot Token IP Mismatch: expected{}, Recd: {}",
                        ip.map(|i| i.to_string()).unwrap_or_else(|| "None".into()),
                        cur_ip.map(|i| i.to_string()).unwrap_or_else(|| "None".into())
                    );
                    return None;
                }
                user
            }
            None => None,
        }
    }

    pub fn check_logins_maxed(&self, ip: IpAddr) -> bool {
        match self.max_logins {
            None => false,
            Some(m) => self.failed_logins.borrow().get(&ip).cloned().unwrap_or(0) >= m,
        }
    }

    async fn check_trusted_proxy(&self, ip: IpAddr) -> Result<(), HttpError> {
        if self.trusted_proxies.borrow().contains(&ip) {
            return Ok(());
        }
        if self.check_authorized_ip(ip).await {
            self.trusted_proxies.borrow_mut().push(ip);
            return Ok(());
        }
        Err(HttpError::new(401))
    }

    pub async fn authenticate_request(&self, req: &HttpRequest, auth_required: bool) -> Result<Option<UserInfo>, HttpError> {
        if req.method == "OPTIONS" {
            return Ok(None);
        }
        if let Some(u) = self.check_json_web_token(req, auth_required)? {
            return Ok(Some(u));
        }
        let ip = crate::utils::parse_ip_address(&req.remote_ip);
        if let Some(ost) = req.arguments.get("token").and_then(|v| v.last()) {
            if let Some(u) = self.check_oneshot_token(ost, ip) {
                return Ok(Some(u));
            }
        }
        if let Some(key) = req.header("X-Api-Key") {
            if !key.is_empty() {
                match self.validate_api_key(&key) {
                    Err(_) => return Err(HttpError::new(401)),
                    Ok(Some(u)) => return Ok(Some(u)),
                    Ok(None) => {}
                }
            }
        }
        if self.force_logins && self.users.borrow().len() > 1 {
            if !auth_required {
                return Ok(None);
            }
            return Err(HttpError::new(401));
        }
        if let Some(tu) = self.check_trusted_connection(ip).await {
            let mut orig_ip = None;
            if req.orig_remote_ip != req.remote_ip {
                orig_ip = crate::utils::parse_ip_address(&req.orig_remote_ip);
            }
            if auth_required {
                if let Some(o) = orig_ip {
                    self.check_trusted_proxy(o).await?;
                }
            }
            return Ok(Some(tu));
        }
        if !auth_required {
            return Ok(None);
        }
        Err(HttpError::new(401))
    }

    pub async fn check_cors(&self, origin: Option<&str>) -> bool {
        let origin = match origin {
            Some(o) if !self.cors_domains.is_empty() => o,
            _ => return false,
        };
        for (re, pat) in self.cors_domains.iter().zip(self.cors_patterns.iter()) {
            if let Some(m) = re.find(origin) {
                if m.as_str() == origin {
                    log::debug!("CORS Pattern Matched, origin: {}  | pattern: {}", origin, pat);
                    return true;
                } else {
                    log::debug!("Partial Cors Match: {}", m.as_str());
                }
            }
        }
        static IP_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"^https?://([^/:]+)").unwrap());
        let ip_re = &*IP_RE;
        if let Some(c) = ip_re.captures(origin) {
            let ip = c.get(1).unwrap().as_str();
            if let Ok(addr) = ip.parse::<IpAddr>() {
                if self.check_authorized_ip(addr).await {
                    log::debug!("Cors request matched trusted IP: {}", ip);
                    return true;
                }
            }
        }
        log::debug!("No CORS match for origin: {}\nPatterns: {:?}", origin, self.cors_patterns);
        false
    }

    pub fn is_api_key_enabled(&self) -> bool {
        self.enable_api_key
    }

    pub fn cors_enabled(&self) -> bool {
        // moonraker checks `cors_domains is not None`, which is always true
        true
    }

    pub fn get_api_key(&self) -> Option<String> {
        if !self.enable_api_key {
            return None;
        }
        Some(self.api_key.borrow().clone())
    }
}

fn base32_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let mut bits: u32 = 0;
    let mut value: u32 = 0;
    for &b in data {
        value = (value << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(ALPHABET[((value >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((value << (5 - bits)) & 31) as usize] as char);
    }
    while out.len() % 8 != 0 {
        out.push('=');
    }
    out
}

/// python's socket.getfqdn() for an ip address
fn getfqdn(ip: IpAddr) -> String {
    use std::ffi::CStr;
    let sockaddr: std::net::SocketAddr = (ip, 0).into();
    let (storage, len) = socket_addr_to_raw(&sockaddr);
    let mut host = [0 as libc::c_char; 1025];
    let ret = unsafe {
        libc::getnameinfo(
            &storage as *const _ as *const libc::sockaddr,
            len,
            host.as_mut_ptr(),
            host.len() as libc::socklen_t,
            std::ptr::null_mut(),
            0,
            libc::NI_NAMEREQD,
        )
    };
    if ret != 0 {
        return ip.to_string();
    }
    unsafe { CStr::from_ptr(host.as_ptr()) }.to_string_lossy().to_string()
}

fn socket_addr_to_raw(addr: &std::net::SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    match addr {
        std::net::SocketAddr::V4(a) => {
            let sin = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: a.port().to_be(),
                sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(a.ip().octets()) },
                sin_zero: [0; 8],
            };
            unsafe { std::ptr::write(&mut storage as *mut _ as *mut libc::sockaddr_in, sin) };
            (storage, std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t)
        }
        std::net::SocketAddr::V6(a) => {
            let sin6 = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: a.port().to_be(),
                sin6_flowinfo: 0,
                sin6_addr: libc::in6_addr { s6_addr: a.ip().octets() },
                sin6_scope_id: 0,
            };
            unsafe { std::ptr::write(&mut storage as *mut _ as *mut libc::sockaddr_in6, sin6) };
            (storage, std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t)
        }
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<Authorization>, crate::confighelper::ConfigError> {
    Authorization::new(config)
}

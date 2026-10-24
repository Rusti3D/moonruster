// HTTP Client with a "requests-like" interface (port of moonraker/components/http_client.py)

use crate::common::ServerError;
use crate::confighelper::ConfigHelper;
use crate::server::Component;
use futures::StreamExt;
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use tokio::io::AsyncWriteExt;

const MAX_BODY_SIZE: usize = 512 * 1024 * 1024;
const GITHUB_PREFIX: &str = "https://api.github.com/";

#[derive(Clone, Debug)]
pub enum HttpErrorKind {
    /// tornado HTTPClientError (has a response code)
    Http { code: u16, message: String },
    /// any other exception
    Other(String),
}

#[derive(Clone, Debug)]
pub struct HttpResponse {
    pub url: String,
    pub final_url: String,
    pub status_code: u16,
    pub content: Vec<u8>,
    pub headers: IndexMap<String, String>,
    pub error: Option<HttpErrorKind>,
}

impl HttpResponse {
    pub fn json(&self) -> Result<Value, ServerError> {
        serde_json::from_slice(&self.content).map_err(|e| ServerError::exc(e.to_string()))
    }
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone())
    }
    pub fn etag(&self) -> Option<String> {
        self.header("etag")
    }
    pub fn last_modified(&self) -> Option<String> {
        self.header("last-modified")
    }
    pub fn is_cachable(&self) -> bool {
        self.etag().is_some() || self.last_modified().is_some()
    }
    pub fn has_error(&self) -> bool {
        self.error.is_some()
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.content).to_string()
    }
    pub fn error_string(&self) -> String {
        match &self.error {
            None => String::new(),
            Some(HttpErrorKind::Http { code, message }) => format!("HTTP {}: {}", code, message),
            Some(HttpErrorKind::Other(m)) => m.clone(),
        }
    }
    pub fn raise_for_status(&self, message: Option<&str>) -> Result<(), ServerError> {
        match &self.error {
            None => Ok(()),
            Some(e) => {
                let (code, mut msg) = match e {
                    HttpErrorKind::Http { code, message } => (*code as i64, message.clone()),
                    HttpErrorKind::Other(_) => (500, format!("HTTP Request Error: {}", self.url)),
                };
                if let Some(m) = message {
                    msg = m.to_string();
                }
                Err(ServerError::new(msg, code))
            }
        }
    }
}

#[derive(Clone)]
pub enum Body {
    None,
    Text(String),
    Json(Value),
}

#[derive(Clone)]
pub struct RequestOpts {
    pub connect_timeout: f64,
    pub request_timeout: f64,
    pub attempts: usize,
    pub retry_pause_time: f64,
    pub enable_cache: bool,
    pub send_etag: bool,
    pub send_if_modified_since: bool,
    pub basic_auth: Option<(String, String)>,
}

impl Default for RequestOpts {
    fn default() -> Self {
        RequestOpts {
            connect_timeout: 5.0,
            request_timeout: 10.0,
            attempts: 1,
            retry_pause_time: 0.1,
            enable_cache: false,
            send_etag: true,
            send_if_modified_since: true,
            basic_auth: None,
        }
    }
}

pub struct HttpClient {
    client: reqwest::Client,
    response_cache: RefCell<HashMap<String, HttpResponse>>,
    gh_rate_limit: Cell<Option<i64>>,
    gh_limit_remaining: Cell<Option<i64>>,
    gh_limit_reset_time: Cell<Option<f64>>,
}

impl Component for HttpClient {}

pub type ProgressCallback = Rc<dyn Fn(i64, i64, i64)>;

impl HttpClient {
    pub fn new(_config: &ConfigHelper) -> Rc<Self> {
        let client = reqwest::Client::builder()
            .user_agent("Moonraker")
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("http client");
        Rc::new(HttpClient {
            client,
            response_cache: RefCell::new(HashMap::new()),
            gh_rate_limit: Cell::new(None),
            gh_limit_remaining: Cell::new(None),
            gh_limit_reset_time: Cell::new(None),
        })
    }

    /// The cached response's ETag and Last-Modified for a URL (the request
    /// headers a cached request sends)
    pub fn cache_validators(&self, url: &str) -> Option<(Option<String>, Option<String>)> {
        let key = url.split('?').next().unwrap_or(url);
        self.response_cache.borrow().get(key).map(|c| (c.etag(), c.last_modified()))
    }

    pub async fn request(&self, method: &str, url: &str, body: Body, headers: &[(String, String)], opts: RequestOpts) -> HttpResponse {
        let cache_key = url.split('?').next().unwrap_or(url).to_string();
        let method = method.to_uppercase();
        let mut req_headers: Vec<(String, String)> = Vec::new();
        let body_bytes: Option<Vec<u8>> = match body {
            Body::None => None,
            Body::Text(t) => Some(t.into_bytes()),
            Body::Json(v) => {
                req_headers.push(("Content-Type".into(), "application/json".into()));
                Some(v.to_string().into_bytes())
            }
        };
        let mut cached = None;
        if opts.enable_cache {
            cached = self.response_cache.borrow().get(&cache_key).cloned();
            if let Some(c) = &cached {
                if opts.send_etag {
                    if let Some(e) = c.etag() {
                        req_headers.push(("If-None-Match".into(), e));
                    }
                    if let (Some(l), true) = (c.last_modified(), opts.send_if_modified_since) {
                        req_headers.push(("If-Modified-Since".into(), l));
                    }
                }
            }
        }
        let mut all_headers: Vec<(String, String)> = headers.to_vec();
        for (k, v) in req_headers {
            all_headers.retain(|(hk, _)| !hk.eq_ignore_ascii_case(&k));
            all_headers.push((k, v));
        }
        let timeout = 1.0 + opts.connect_timeout + opts.request_timeout;
        let mut err: Option<HttpErrorKind> = None;
        let mut result: Option<HttpResponse> = None;
        for i in 0..opts.attempts.max(1) {
            if i > 0 {
                tokio::time::sleep(std::time::Duration::from_secs_f64(opts.retry_pause_time)).await;
            }
            let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
            let mut rb = self.client.request(m, url).timeout(std::time::Duration::from_secs_f64(opts.request_timeout));
            for (k, v) in &all_headers {
                rb = rb.header(k, v);
            }
            if let Some(b) = &body_bytes {
                rb = rb.body(b.clone());
            }
            if let Some((u, p)) = &opts.basic_auth {
                rb = rb.basic_auth(u, Some(p));
            }
            let fut = async {
                let resp = rb.send().await?;
                let code = resp.status().as_u16();
                let final_url = resp.url().to_string();
                let mut hdrs = IndexMap::new();
                for (k, v) in resp.headers() {
                    hdrs.insert(title_case(k.as_str()), String::from_utf8_lossy(v.as_bytes()).to_string());
                }
                let mut content = Vec::new();
                let mut stream = resp.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk?;
                    if content.len() + chunk.len() > MAX_BODY_SIZE {
                        break;
                    }
                    content.extend_from_slice(&chunk);
                }
                Ok::<_, reqwest::Error>((code, final_url, hdrs, content))
            };
            match tokio::time::timeout(std::time::Duration::from_secs_f64(timeout), fut).await {
                Err(_) => {
                    err = Some(HttpErrorKind::Other("TimeoutError()".into()));
                }
                Ok(Err(e)) => {
                    if e.is_timeout() {
                        err = Some(HttpErrorKind::Http { code: 599, message: "Timeout during request".into() });
                        if opts.attempts - i == 1 {
                            result = Some(HttpResponse {
                                url: url.into(),
                                final_url: url.into(),
                                status_code: 599,
                                content: Vec::new(),
                                headers: IndexMap::new(),
                                error: err.clone(),
                            });
                            break;
                        }
                        continue;
                    }
                    err = Some(HttpErrorKind::Other(e.to_string()));
                }
                Ok(Ok((code, final_url, hdrs, content))) => {
                    let resp_err = if (200..300).contains(&code) {
                        None
                    } else {
                        Some(HttpErrorKind::Http { code, message: crate::components::application::standard_reason(code).to_string() })
                    };
                    let body;
                    if code == 304 {
                        err = None;
                        match &cached {
                            None => {
                                if opts.enable_cache {
                                    log::info!("Request returned 304, however no cached item was found");
                                }
                                body = Vec::new();
                            }
                            Some(c) => {
                                log::debug!("Request returned from cache: {}", url);
                                body = c.content.clone();
                            }
                        }
                    } else if resp_err.is_some() && opts.attempts - i != 1 {
                        err = resp_err;
                        continue;
                    } else {
                        err = resp_err;
                        body = content;
                    }
                    result = Some(HttpResponse { url: url.into(), final_url, status_code: code, content: body, headers: hdrs, error: err.clone() });
                    break;
                }
            }
        }
        let ret = result.unwrap_or(HttpResponse {
            url: url.into(),
            final_url: url.into(),
            status_code: 500,
            content: Vec::new(),
            headers: IndexMap::new(),
            error: err,
        });
        if opts.enable_cache && ret.is_cachable() {
            log::debug!("Caching HTTP Response: {}", url);
            self.response_cache.borrow_mut().insert(cache_key, ret.clone());
        } else {
            self.response_cache.borrow_mut().remove(&cache_key);
        }
        ret
    }

    /// GET with caching enabled by default (python's get())
    pub async fn get(&self, url: &str, headers: &[(String, String)], enable_cache: bool) -> HttpResponse {
        self.request("GET", url, Body::None, headers, RequestOpts { enable_cache, ..Default::default() }).await
    }

    pub async fn get_opts(&self, url: &str, headers: &[(String, String)], opts: RequestOpts) -> HttpResponse {
        self.request("GET", url, Body::None, headers, opts).await
    }

    pub async fn post(&self, url: &str, body: Body, headers: &[(String, String)], opts: RequestOpts) -> HttpResponse {
        self.request("POST", url, body, headers, opts).await
    }

    pub async fn github_api_request(&self, resource: &str, attempts: usize, retry_pause_time: f64) -> Result<HttpResponse, ServerError> {
        let url = format!("{}{}", GITHUB_PREFIX, resource.trim_matches('/'));
        if let (Some(reset), Some(0)) = (self.gh_limit_reset_time.get(), self.gh_limit_remaining.get()) {
            let now = crate::utils::time_now();
            if now < reset {
                let reset_time = chrono::DateTime::from_timestamp(reset as i64, 0)
                    .map(|d| d.with_timezone(&chrono::Local).format("%a %b %e %H:%M:%S %Y").to_string())
                    .unwrap_or_default();
                return Err(ServerError::bad(format!("GitHub Rate Limit Reached\nRequest: {}\nLimit Reset Time: {}", url, reset_time)));
            }
        }
        let headers = vec![("Accept".to_string(), "application/vnd.github.v3+json".to_string())];
        let resp = self
            .get_opts(&url, &headers, RequestOpts { enable_cache: true, attempts, retry_pause_time, ..Default::default() })
            .await;
        if let Some(limit) = resp.header("X-Ratelimit-Limit") {
            self.gh_rate_limit.set(limit.parse().ok());
            self.gh_limit_remaining.set(resp.header("X-Ratelimit-Remaining").and_then(|v| v.parse().ok()));
            self.gh_limit_reset_time.set(resp.header("X-Ratelimit-Reset").and_then(|v| v.parse().ok()));
        }
        Ok(resp)
    }

    pub fn github_api_stats(&self) -> Value {
        json!({
            "github_rate_limit": self.gh_rate_limit.get(),
            "github_requests_remaining": self.gh_limit_remaining.get(),
            "github_limit_reset_time": self.gh_limit_reset_time.get(),
        })
    }

    pub async fn download_file(
        &self,
        url: &str,
        content_type: &str,
        destination: Option<PathBuf>,
        download_size: i64,
        progress: Option<ProgressCallback>,
        request_timeout: f64,
        attempts: usize,
        retry_pause_time: f64,
    ) -> Result<PathBuf, ServerError> {
        for i in 0..attempts.max(1) {
            let need_cd = destination.is_none();
            let mut dest = destination.clone().unwrap_or_else(|| {
                std::env::temp_dir().join(format!("moonraker.download-{}.mrd", crate::utils::monotonic() as i64))
            });
            let res: Result<(u16, PathBuf), String> = async {
                let resp = self
                    .client
                    .get(url)
                    .header("Accept", content_type)
                    .timeout(std::time::Duration::from_secs_f64(request_timeout))
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                let code = resp.status().as_u16();
                let mut size = download_size;
                if code == 200 {
                    if let Some(cl) = resp.content_length() {
                        size = cl as i64;
                        log::debug!("Content-Length header received: size = {}", size);
                    }
                    if need_cd {
                        if let Some(cd) = resp.headers().get("content-disposition") {
                            let hval = String::from_utf8_lossy(cd.as_bytes()).to_string();
                            if let Some(fname) = parse_content_disposition(&hval) {
                                dest = dest.parent().unwrap_or(std::path::Path::new("/tmp")).join(&fname);
                            }
                        }
                    }
                }
                let mut file: Option<tokio::fs::File> = None;
                let mut total: i64 = 0;
                let mut pct_done: i64 = 0;
                let mut stream = resp.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.map_err(|e| e.to_string())?;
                    if chunk.is_empty() {
                        continue;
                    }
                    if file.is_none() {
                        file = Some(tokio::fs::File::create(&dest).await.map_err(|e| e.to_string())?);
                    }
                    file.as_mut().unwrap().write_all(&chunk).await.map_err(|e| e.to_string())?;
                    total += chunk.len() as i64;
                    if size > 0 {
                        if let Some(cb) = &progress {
                            let pct = ((total as f64 / size as f64 * 100.0 + 0.5) as i64).min(100);
                            if pct != pct_done {
                                pct_done = pct;
                                cb(pct, size, total);
                            }
                        }
                    }
                }
                if let Some(mut f) = file {
                    let _ = f.flush().await;
                }
                Ok((code, dest.clone()))
            }
            .await;
            match res {
                Err(e) => {
                    if i + 1 == attempts.max(1) {
                        return Err(ServerError::new(e, 500));
                    }
                    tokio::time::sleep(std::time::Duration::from_secs_f64(retry_pause_time)).await;
                    continue;
                }
                Ok((code, path)) => {
                    if code < 400 {
                        return Ok(path);
                    }
                }
            }
        }
        Err(ServerError::bad(format!("Retries exceeded for request: {}", url)))
    }
}

fn parse_content_disposition(hval: &str) -> Option<String> {
    let re = Regex::new(r#"filename[^;\n=]*=(['"])?(utf-8'')?([^\n;'"]*)['"]?"#).ok()?;
    let mut name = None;
    for c in re.captures_iter(hval) {
        let enc = c.get(2).map(|m| m.as_str()).unwrap_or("");
        let fname = c.get(3).map(|m| m.as_str()).unwrap_or("").to_string();
        if enc.starts_with("utf-8") {
            return Some(crate::utils::url_unescape(&fname));
        }
        name = Some(fname);
    }
    name
}

fn title_case(h: &str) -> String {
    h.split('-')
        .map(|p| {
            let mut c = p.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

pub fn load_component(config: &ConfigHelper) -> Rc<HttpClient> {
    HttpClient::new(config)
}

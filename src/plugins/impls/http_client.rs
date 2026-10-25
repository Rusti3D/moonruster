// moonraker.components.http_client.HttpClient / HttpResponse for Python
// components. get / post / delete are Moonraker's own code over request
// (mirror_verbatim.txt); request runs on Moonruster's client. A JSON body is
// encoded by Moonraker's json_wrapper, so the bytes sent are Moonraker's.
use super::*;
use crate::components::http_client::{Body, HttpClient, HttpErrorKind, HttpResponse, RequestOpts};
use crate::plugins::futures::rust_future;
use std::cell::RefCell;

macro_rules! client {
    ($m:literal) => {
        concat!("moonraker.components.http_client.HttpClient.", $m)
    };
}

macro_rules! resp {
    ($m:literal) => {
        concat!("moonraker.components.http_client.HttpResponse.", $m)
    };
}

/// A response with its text encoding (HttpResponse.encoding, settable)
pub struct PyResponse {
    resp: HttpResponse,
    encoding: RefCell<String>,
}

fn bytes(b: &[u8]) -> PyResult<PyRef> {
    py_bytes(b)
}

/// HttpResponse.error: tornado's HTTPClientError for an HTTP status, else
/// the exception's message as an Exception (tornado imported only here)
fn error_object(e: &HttpErrorKind) -> PyResult<PyRef> {
    match e {
        HttpErrorKind::Http { code, message } => {
            lookup("tornado.httpclient", "HTTPClientError")?.call(&[&py_int(*code as i64), &py_str(message)])
        }
        HttpErrorKind::Other(m) => lookup("builtins", "Exception")?.call(&[&py_str(m)]),
    }
}

fn response_object(resp: HttpResponse) -> PyResult<PyRef> {
    objects::wrap_class(
        Rc::new(PyResponse { resp, encoding: RefCell::new("utf-8".into()) }),
        "moonraker.components.http_client",
        "HttpResponse",
    )
}

pub fn register(r: &mut Registry) {
    r.method(client!("register_cached_url"), |a| {
        let url = arg(a, 0, "url")?.as_string()?;
        let s = |i, n| -> PyResult<Option<String>> {
            match opt(a, i, n) {
                Some(v) => Ok(Some(v.as_string()?)),
                None => Ok(None),
            }
        };
        let (etag, lm) = (s(1, "etag")?, s(2, "last_modified")?);
        a.this::<HttpClient>()?.register_cached_url(&url, etag, lm).map_err(|e| server_error(&e.message, e.status_code))?;
        Ok(none())
    });
    r.method(client!("github_api_stats"), |a| to_py(&a.this::<HttpClient>()?.github_api_stats()));
    r.method(client!("github_api_request"), |a| {
        let resource = arg(a, 0, "resource")?.as_string()?;
        let attempts = opt(a, 1, "attempts").map(|v| v.as_i64()).unwrap_or(Ok(1))?.max(1) as usize;
        let pause = f64_or(a, 2, "retry_pause_time", 0.1)?;
        let c = a.this::<HttpClient>()?;
        rust_future(async move {
            let resp = c.github_api_request(&resource, attempts, pause).await.map_err(|e| server_error(&e.message, e.status_code))?;
            response_object(resp)
        })
    });
    // download_file(url, content_type, destination_path, download_size,
    // progress_callback(percent, size, recd), connect_timeout,
    // request_timeout, attempts, retry_pause_time) -> pathlib.Path
    r.method(client!("download_file"), |a| {
        let url = arg(a, 0, "url")?.as_string()?;
        let ctype = arg(a, 1, "content_type")?.as_string()?;
        let dest = match opt(a, 2, "destination_path") {
            Some(p) => Some(std::path::PathBuf::from(p.str()?)),
            None => None,
        };
        let size = opt(a, 3, "download_size").map(|v| v.as_i64()).unwrap_or(Ok(-1))?;
        let progress: Option<crate::components::http_client::ProgressCallback> = opt(a, 4, "progress_callback").map(|cb| {
            let cb = cb.clone();
            Rc::new(move |pct: i64, size: i64, recd: i64| {
                if let Err(e) = cb.call(&[&py_int(pct), &py_int(size), &py_int(recd)]) {
                    log::error!("download progress callback failed\n{}", e.traceback);
                }
            }) as crate::components::http_client::ProgressCallback
        });
        let request_timeout = f64_or(a, 6, "request_timeout", 180.0)?;
        let attempts = opt(a, 7, "attempts").map(|v| v.as_i64()).unwrap_or(Ok(1))?.max(1) as usize;
        let pause = f64_or(a, 8, "retry_pause_time", 1.0)?;
        let c = a.this::<HttpClient>()?;
        rust_future(async move {
            let p = c
                .download_file(&url, &ctype, dest, size, progress, request_timeout, attempts, pause)
                .await
                .map_err(|e| server_error(&e.message, e.status_code))?;
            lookup("pathlib", "Path")?.call(&[&py_str(&p.to_string_lossy())])
        })
    });
    r.method(client!("request"), |a| {
        let method = arg(a, 0, "method")?.as_string()?.to_uppercase();
        let url = arg(a, 1, "url")?.as_string()?;
        let headers_py = opt(a, 3, "headers").cloned();
        let mut opts = RequestOpts {
            connect_timeout: f64_or(a, 4, "connect_timeout", 5.0)?,
            request_timeout: f64_or(a, 5, "request_timeout", 10.0)?,
            attempts: opt(a, 6, "attempts").map(|v| v.as_i64()).unwrap_or(Ok(1))?.max(0) as usize,
            retry_pause_time: f64_or(a, 7, "retry_pause_time", 0.1)?,
            enable_cache: bool_or(a, 8, "enable_cache", false)?,
            send_etag: bool_or(a, 9, "send_etag", true)?,
            send_if_modified_since: bool_or(a, 10, "send_if_modified_since", true)?,
            basic_auth: None,
        };
        if let Some(u) = opt(a, 11, "basic_auth_user") {
            let p = opt(a, 12, "basic_auth_pass").ok_or_else(|| py_error("AssertionError", String::new()))?;
            opts.basic_auth = Some((u.as_string()?, p.as_string()?));
        }
        // the body and the request headers Moonraker adds (and writes into
        // the caller's headers dict, as headers.update(req_headers) does)
        let mut req_headers: Vec<(String, String)> = vec![];
        let body = match opt(a, 2, "body") {
            None => Body::None,
            Some(b) if b.is_instance(&lookup("builtins", "list")?)? || b.is_instance(&lookup("builtins", "dict")?)? => {
                req_headers.push(("Content-Type".into(), "application/json".into()));
                let enc = lookup("moonraker.utils.json_wrapper", "dumps")?.call(&[b])?;
                Body::Text(if enc.is_instance(&lookup("builtins", "bytes")?)? { enc.call_method("decode", &[])?.as_string()? } else { enc.as_string()? })
            }
            Some(b) if b.is_instance(&lookup("builtins", "bytes")?)? => Body::Text(b.call_method("decode", &[])?.as_string()?),
            Some(b) => Body::Text(b.as_string()?),
        };
        let client = a.this::<HttpClient>()?;
        if opts.enable_cache && opts.send_etag {
            if let Some((etag, lm)) = client.cache_validators(&url) {
                if let Some(e) = etag {
                    req_headers.push(("If-None-Match".into(), e));
                }
                if let (Some(l), true) = (lm, opts.send_if_modified_since) {
                    req_headers.push(("If-Modified-Since".into(), l));
                }
            }
        }
        let mut headers: Vec<(String, String)> = req_headers.clone();
        if let Some(h) = &headers_py {
            for (k, v) in &req_headers {
                h.call_method("__setitem__", &[&py_str(k), &py_str(v)])?;
            }
            if let Value::Object(m) = from_py(h)? {
                headers = m.iter().map(|(k, v)| (k.clone(), crate::pyfmt::py_str(v))).collect();
            }
        }
        rust_future(async move {
            let resp = client.request(&method, &url, body, &headers, opts).await;
            response_object(resp)
        })
    });

    r.method(resp!("json"), |a| {
        let r = a.this::<PyResponse>()?;
        lookup("moonraker.utils.json_wrapper", "loads")?.call(&[&bytes(&r.resp.content)?])
    });
    r.method(resp!("is_cachable"), |a| Ok(py_bool(a.this::<PyResponse>()?.resp.is_cachable())));
    r.method(resp!("has_error"), |a| Ok(py_bool(a.this::<PyResponse>()?.resp.has_error())));
    r.method(resp!("raise_for_status"), |a| {
        let r = a.this::<PyResponse>()?;
        let message = match opt(a, 0, "message") {
            Some(m) => Some(m.as_string()?),
            None => None,
        };
        match (&r.resp.error, r.resp.raise_for_status(message.as_deref())) {
            (Some(e), Err(se)) => {
                // raise ServerError(msg, code) from self._error
                let err = server_error(&se.message, se.status_code);
                if let Some(v) = &err.value {
                    v.setattr("__cause__", &error_object(e)?)?;
                }
                Err(err)
            }
            _ => Ok(none()),
        }
    });
    r.method(resp!("encoding"), |a| Ok(py_str(&a.this::<PyResponse>()?.encoding.borrow())));
    r.method(resp!("encoding.setter"), |a| {
        *a.this::<PyResponse>()?.encoding.borrow_mut() = arg(a, 0, "new_enc")?.as_string()?;
        Ok(none())
    });
    r.method(resp!("text"), |a| {
        let r = a.this::<PyResponse>()?;
        let enc = r.encoding.borrow().clone();
        bytes(&r.resp.content)?.call_method("decode", &[&py_str(&enc)])
    });
    r.method(resp!("content"), |a| bytes(&a.this::<PyResponse>()?.resp.content));
    r.method(resp!("url"), |a| Ok(py_str(&a.this::<PyResponse>()?.resp.url)));
    r.method(resp!("final_url"), |a| Ok(py_str(&a.this::<PyResponse>()?.resp.final_url)));
    r.method(resp!("status_code"), |a| Ok(py_int(a.this::<PyResponse>()?.resp.status_code as i64)));
    r.method(resp!("headers"), |a| {
        let r = a.this::<PyResponse>()?;
        let h = lookup("tornado.httputil", "HTTPHeaders")?.call(&[])?;
        for (k, v) in &r.resp.headers {
            h.call_method("add", &[&py_str(k), &py_str(v)])?;
        }
        Ok(h)
    });
    r.method(resp!("last_modified"), |a| Ok(a.this::<PyResponse>()?.resp.last_modified().map(|s| py_str(&s)).unwrap_or_else(none)));
    r.method(resp!("etag"), |a| Ok(a.this::<PyResponse>()?.resp.etag().map(|s| py_str(&s)).unwrap_or_else(none)));
    r.method(resp!("error"), |a| match &a.this::<PyResponse>()?.resp.error {
        Some(e) => error_object(e),
        None => Ok(none()),
    });
}

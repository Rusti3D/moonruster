// The request and response objects of the generic HTTP device's templates

use super::*;

#[derive(Debug, Default)]
pub(super) struct ReqState {
    pub(super) method: String,
    pub(super) url: String,
    pub(super) body: Value,
    pub(super) headers: Vec<(String, String)>,
    pub(super) last_response: Option<Value>,
}

/// `http_request` object available to GenericHTTP templates
#[derive(Debug)]
pub(super) struct HttpRequestObj {
    pub(super) default_url: String,
    pub(super) st: Mutex<ReqState>,
    pub(super) caller: Mutex<Option<Caller>>,
}

impl HttpRequestObj {
    pub(super) fn new(url: &str) -> Arc<Self> {
        Arc::new(HttpRequestObj {
            default_url: url.to_string(),
            st: Mutex::new(ReqState { method: "GET".into(), url: url.into(), ..Default::default() }),
            caller: Mutex::new(None),
        })
    }
    pub(super) fn state_json(&self) -> Value {
        let st = self.st.lock().unwrap();
        let headers: Map<String, Value> = st.headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        json!({"method": st.method, "url": st.url, "body": st.body, "headers": headers})
    }
}

impl std::fmt::Debug for Caller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Caller")
    }
}

impl minijinja::value::Object for HttpRequestObj {
    fn call_method(
        self: &Arc<Self>,
        _state: &minijinja::State,
        method: &str,
        args: &[minijinja::Value],
    ) -> Result<minijinja::Value, minijinja::Error> {
        let to_json = |v: &minijinja::Value| serde_json::to_value(v).unwrap_or(Value::Null);
        let s = |i: usize| args.get(i).map(|a| to_json(a)).map(|v| crate::pyfmt::py_str(&v)).unwrap_or_default();
        let none = minijinja::Value::from(());
        match method {
            "send" => {
                let mut req = self.state_json();
                if let Some(kw) = args.last().and_then(|a| minijinja::value::Kwargs::try_from(a.clone()).ok()) {
                    req["kwargs"] = Value::Object(crate::components::template::kwargs_to_map(&kw));
                }
                let caller = self.caller.lock().unwrap().clone();
                let caller = caller.ok_or_else(|| minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, "no event loop"))?;
                let resp = caller.call("http_send", vec![req], Map::new())?;
                let rj = to_json(&resp);
                self.st.lock().unwrap().last_response = Some(rj.clone());
                Ok(minijinja::Value::from_object(ResponseObj(rj)))
            }
            "set_method" => {
                self.st.lock().unwrap().method = s(0);
                Ok(none)
            }
            "set_url" => {
                self.st.lock().unwrap().url = s(0);
                Ok(none)
            }
            "set_body" => {
                self.st.lock().unwrap().body = args.first().map(to_json).unwrap_or(Value::Null);
                Ok(none)
            }
            "add_header" => {
                let (k, v) = (s(0), s(1));
                let mut st = self.st.lock().unwrap();
                st.headers.retain(|(hk, _)| *hk != k);
                st.headers.push((k, v));
                Ok(none)
            }
            "set_headers" => {
                let h = args.first().map(to_json).unwrap_or(Value::Null);
                self.st.lock().unwrap().headers = h
                    .as_object()
                    .map(|m| m.iter().map(|(k, v)| (k.clone(), crate::pyfmt::py_str(v))).collect())
                    .unwrap_or_default();
                Ok(none)
            }
            "reset" => {
                let mut st = self.st.lock().unwrap();
                st.method = "GET".into();
                st.url = self.default_url.clone();
                st.body = Value::Null;
                st.headers.clear();
                Ok(none)
            }
            "last_response" => Ok(match self.st.lock().unwrap().last_response.clone() {
                Some(r) => minijinja::Value::from_object(ResponseObj(r)),
                None => none,
            }),
            _ => Err(minijinja::Error::new(minijinja::ErrorKind::UnknownMethod, format!("no method {}", method))),
        }
    }
}

/// HttpResponse as seen by templates
#[derive(Debug)]
pub(super) struct ResponseObj(Value);

impl minijinja::value::Object for ResponseObj {
    fn get_value(self: &Arc<Self>, key: &minijinja::Value) -> Option<minijinja::Value> {
        let k = key.as_str()?;
        match k {
            "status_code" | "url" | "final_url" | "headers" | "content" => self.0.get(k).map(minijinja::Value::from_serialize),
            "text" => self.0.get("content").map(minijinja::Value::from_serialize),
            "encoding" => Some(minijinja::Value::from("utf-8")),
            _ => None,
        }
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &minijinja::State,
        method: &str,
        args: &[minijinja::Value],
    ) -> Result<minijinja::Value, minijinja::Error> {
        let err = |m: String| minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, m);
        match method {
            "json" => {
                let text = self.0.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let v: Value = serde_json::from_str(text).map_err(|e| err(e.to_string()))?;
                Ok(minijinja::Value::from_serialize(&v))
            }
            "has_error" => Ok(minijinja::Value::from(!self.0["error"].is_null())),
            "is_cachable" => Ok(minijinja::Value::from(false)),
            "raise_for_status" => {
                if self.0["error"].is_null() {
                    return Ok(minijinja::Value::from(()));
                }
                let msg = args.first().and_then(|a| a.as_str()).map(String::from).unwrap_or_else(|| crate::pyfmt::py_str(&self.0["error"]));
                Err(err(msg))
            }
            _ => Err(minijinja::Error::new(minijinja::ErrorKind::UnknownMethod, format!("no method {}", method))),
        }
    }
}

pub(super) fn response_json(r: &HttpResponse) -> Value {
    let headers: Map<String, Value> = r.headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    json!({
        "url": r.url,
        "final_url": r.final_url,
        "status_code": r.status_code,
        "content": r.text(),
        "headers": headers,
        "error": if r.has_error() { json!(r.error_string()) } else { Value::Null },
    })
}

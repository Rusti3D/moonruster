// The HTTP device types: tasmota, shelly, homeseer, homeassistant, loxonev1,
// smartthings, hue, http

use super::*;

impl PowerDevice {
    pub(super) async fn send_http_command(&self, d: &HttpDev, url: &str, command: &str, retries: usize) -> Result<Value, ServerError> {
        let opts = RequestOpts {
            request_timeout: 20.0,
            attempts: retries,
            retry_pause_time: 1.0,
            enable_cache: false,
            basic_auth: if d.has_basic_auth { Some((d.user.clone(), d.password.clone())) } else { None },
            ..Default::default()
        };
        let resp = d.client.get_opts(url, &[], opts).await;
        resp.raise_for_status(Some(&format!("Error sending '{}' command: {}", self.type_, command)))?;
        resp.json()
    }

    pub(super) async fn http_command(&self, d: &HttpDev, command: &str) -> Result<Value, ServerError> {
        match &d.kind {
            HttpKind::Tasmota { output_id, timer } => {
                let out_cmd = match command {
                    "on" | "off" => {
                        let mut c = format!("Power{} {}", output_id, command);
                        if !timer.is_empty() && command == "off" {
                            c = format!("Backlog Delay {}0; {}", timer, c);
                        }
                        c
                    }
                    "info" => format!("Power{}", output_id),
                    _ => return Err(err(format!("Invalid tasmota command: {}", command))),
                };
                let query = crate::utils::urlencode(&[("user", d.user.clone()), ("password", d.password.clone()), ("cmnd", out_cmd)]);
                let url = format!("{}://{}/cm?{}", d.protocol, d.addr, query);
                self.send_http_command(d, &url, command, 3).await
            }
            HttpKind::Shelly { output_id, timer } => {
                let mut q: Vec<(&str, String)> = Vec::new();
                match command {
                    "on" | "off" => {
                        q.push(("turn", command.into()));
                        if command == "off" && !timer.is_empty() {
                            q[0].1 = "on".into();
                            q.push(("timer", timer.clone()));
                        }
                    }
                    "info" => {}
                    _ => return Err(err(format!("Invalid shelly command: {}", command))),
                }
                let url = format!("{}://{}/relay/{}?{}", d.protocol, d.addr, output_id, crate::utils::urlencode(&q));
                self.send_http_command(d, &url, command, 3).await
            }
            HttpKind::HomeSeer { device } => {
                let mut q = vec![("user", d.user.clone()), ("pass", d.password.clone()), ("request", command.to_string()), ("ref", device.to_string())];
                if let Some(label) = HOMESEER_LABEL.with(|l| l.borrow_mut().take()) {
                    q.push(("label", label));
                }
                let url = format!("{}://{}:{}/JSON?{}", d.protocol, d.addr, d.port, crate::utils::urlencode(&q));
                self.send_http_command(d, &url, command, 3).await
            }
            HttpKind::Loxone { output_id } => {
                let out_cmd = match command {
                    "on" | "off" => format!("jdev/sps/io/{}/{}", quote(output_id), command),
                    "info" => format!("jdev/sps/io/{}", quote(output_id)),
                    _ => return Err(err(format!("Invalid loxonev1 command: {}", command))),
                };
                let url = format!("http://{}/{}", d.addr, out_cmd);
                self.send_http_command(d, &url, command, 3).await
            }
            HttpKind::SmartThings { device, token } => {
                let (method, url, body) = match command {
                    "on" | "off" => (
                        "POST",
                        format!("{}://{}/v1/devices/{}/commands", d.protocol, d.addr, quote(device)),
                        Body::Json(json!([{"component": "main", "capability": "switch", "command": command}])),
                    ),
                    "info" => (
                        "GET",
                        format!("{}://{}/v1/devices/{}/components/main/capabilities/switch/status", d.protocol, d.addr, quote(device)),
                        Body::None,
                    ),
                    _ => return Err(err(format!("Invalid SmartThings command: {}", command))),
                };
                let headers = vec![("Authorization".to_string(), format!("Bearer {}", token))];
                let resp = d.client.request(method, &url, body, &headers, RequestOpts { attempts: 3, ..Default::default() }).await;
                resp.raise_for_status(Some(&format!("Error sending SmartThings command: {}", command)))?;
                resp.json()
            }
            HttpKind::HomeAssistant { device, token, domain, .. } => {
                let (method, out_cmd, body) = match command {
                    "on" | "off" => ("POST", format!("api/services/{}/turn_{}", quote(domain), command), Body::Json(json!({"entity_id": device}))),
                    "info" => ("GET", format!("api/states/{}", quote(device)), Body::None),
                    _ => return Err(err(format!("Invalid homeassistant command: {}", command))),
                };
                let url = format!("{}://{}:{}/{}", d.protocol, d.addr, d.port, out_cmd);
                let headers = vec![("Authorization".to_string(), format!("Bearer {}", token))];
                let resp = d.client.request(method, &url, body, &headers, RequestOpts { attempts: 3, ..Default::default() }).await;
                resp.raise_for_status(Some(&format!("Error sending homeassistant command: {}", command)))?;
                if method == "GET" {
                    return resp.json();
                }
                Ok(json!({}))
            }
            _ => Err(err("unsupported")),
        }
    }

    pub(super) fn key_err(k: &str) -> ServerError {
        ServerError::new(format!("KeyError: '{}'", k), 500)
    }

    pub(super) async fn http_status_request(&self) -> Result<String, ServerError> {
        let d = match &self.kind {
            Kind::Http(d) => d,
            _ => return Err(err("not http")),
        };
        match &d.kind {
            HttpKind::Tasmota { output_id, .. } => {
                let res = self.http_command(d, "info").await?;
                tasmota_state(&res, *output_id)
            }
            HttpKind::Shelly { timer, .. } => {
                let res = self.http_command(d, "info").await?;
                Ok(shelly_state(&res, timer))
            }
            HttpKind::SmartThings { .. } => {
                let res = self.http_command(d, "info").await?;
                res["switch"]["value"].as_str().map(|s| s.to_lowercase()).ok_or_else(|| Self::key_err("switch"))
            }
            HttpKind::HomeSeer { .. } => {
                let res = self.http_command(d, "getstatus").await?;
                res["Devices"][0]["status"].as_str().map(|s| s.to_lowercase()).ok_or_else(|| Self::key_err("Devices"))
            }
            HttpKind::HomeAssistant { .. } => {
                let res = self.http_command(d, "info").await?;
                res.get("state").map(crate::pyfmt::py_str).ok_or_else(|| Self::key_err("state"))
            }
            HttpKind::Loxone { .. } => {
                let res = self.http_command(d, "info").await?;
                loxone_state(&res)
            }
            HttpKind::Hue { device_id, device_type, on_state, .. } => {
                let url = format!("{}://{}:{}/api/{}/{}s/{}", d.protocol, d.addr, d.port, quote(&d.user), device_type, quote(device_id));
                let ret = d.client.request("GET", &url, Body::None, &[], RequestOpts::default()).await;
                let resp = ret.json()?;
                Ok(if crate::pyfmt::py_truthy(&resp["state"][on_state.as_str()]) { "on".into() } else { "off".into() })
            }
            HttpKind::Generic { .. } => self.generic_request(d, "status").await,
        }
    }

    pub(super) async fn http_power_request(&self, state: &str) -> Result<String, ServerError> {
        let d = match &self.kind {
            Kind::Http(d) => d,
            _ => return Err(err("not http")),
        };
        match &d.kind {
            HttpKind::Tasmota { output_id, timer } => {
                let res = self.http_command(d, state).await?;
                if timer.is_empty() || state != "off" {
                    return tasmota_state(&res, *output_id);
                }
                Ok(state.to_string())
            }
            HttpKind::Shelly { timer, .. } => {
                let res = self.http_command(d, state).await?;
                Ok(shelly_state(&res, timer))
            }
            HttpKind::SmartThings { .. } => {
                let res = self.http_command(d, state).await?;
                let ack = res["results"][0]["status"].as_str().map(|s| s.to_lowercase()).ok_or_else(|| Self::key_err("results"))?;
                Ok(if ack == "accepted" { state.to_string() } else { "error".into() })
            }
            HttpKind::HomeSeer { .. } => {
                let mut label = state.to_string();
                if let Some(f) = label.get_mut(0..1) {
                    f.make_ascii_uppercase();
                }
                HOMESEER_LABEL.with(|l| *l.borrow_mut() = Some(label));
                self.http_command(d, "controldevicebylabel").await?;
                Ok(state.to_string())
            }
            HttpKind::HomeAssistant { status_delay, .. } => {
                self.http_command(d, state).await?;
                tokio::time::sleep(Duration::from_secs_f64(*status_delay)).await;
                self.http_status_request().await
            }
            HttpKind::Loxone { .. } => {
                let res = self.http_command(d, state).await?;
                loxone_state(&res)
            }
            HttpKind::Hue { device_id, device_type, state_key, .. } => {
                let url = format!(
                    "{}://{}:{}/api/{}/{}s/{}/{}",
                    d.protocol,
                    d.addr,
                    d.port,
                    quote(&d.user),
                    device_type,
                    quote(device_id),
                    quote(state_key)
                );
                let ret = d.client.request("PUT", &url, Body::Json(json!({"on": state == "on"})), &[], RequestOpts::default()).await;
                let resp = ret.json()?;
                let state_url = format!("/{}s/{}/{}/on", device_type, device_id, state_key);
                let v = resp[0]["success"].get(&state_url).cloned().ok_or_else(|| Self::key_err(&state_url))?;
                Ok(if crate::pyfmt::py_truthy(&v) { "on".into() } else { "off".into() })
            }
            HttpKind::Generic { .. } => self.generic_request(d, state).await,
        }
    }

    pub(super) async fn generic_request(&self, d: &HttpDev, command: &str) -> Result<String, ServerError> {
        let (urls, request_template, response_template) = match &d.kind {
            HttpKind::Generic { urls, request_template, response_template } => (urls, request_template, response_template),
            _ => return Err(err("not generic")),
        };
        let url = urls.get(command).cloned().ok_or_else(|| Self::key_err(command))?;
        let req_obj = HttpRequestObj::new(&url);
        let client = d.client.clone();
        let basic = if d.has_basic_auth { Some((d.user.clone(), d.password.clone())) } else { None };
        let send = {
            let client = client.clone();
            let basic = basic.clone();
            move |req: Value| {
                let client = client.clone();
                let basic = basic.clone();
                async move {
                    let kw = req.get("kwargs").cloned().unwrap_or(json!({}));
                    let f = |k: &str, d: f64| kw.get(k).and_then(|v| v.as_f64()).unwrap_or(d);
                    let opts = RequestOpts {
                        request_timeout: f("request_timeout", 20.0),
                        connect_timeout: f("connect_timeout", 5.0),
                        attempts: f("attempts", 3.0) as usize,
                        retry_pause_time: f("retry_pause_time", 1.0),
                        enable_cache: false,
                        basic_auth: basic,
                        ..Default::default()
                    };
                    let body = match &req["body"] {
                        Value::Null => Body::None,
                        Value::String(s) => Body::Text(s.clone()),
                        v => Body::Json(v.clone()),
                    };
                    let headers: Vec<(String, String)> = req["headers"]
                        .as_object()
                        .map(|m| m.iter().map(|(k, v)| (k.clone(), crate::pyfmt::py_str(v))).collect())
                        .unwrap_or_default();
                    let method = req["method"].as_str().unwrap_or("GET").to_string();
                    let url = req["url"].as_str().unwrap_or("").to_string();
                    response_json(&client.request(&method, &url, body, &headers, opts).await)
                }
            }
        };
        let send_rc = Rc::new(send);
        let s2 = send_rc.clone();
        let http_send: AsyncFn = Rc::new(move |args: Vec<Value>, _kw| {
            let s = s2.clone();
            Box::pin(async move { Ok((s)(args.into_iter().next().unwrap_or(Value::Null)).await) })
        });
        let async_sleep: AsyncFn = Rc::new(|args: Vec<Value>, _kw| {
            Box::pin(async move {
                let t = args.first().and_then(|v| v.as_f64()).unwrap_or(0.0);
                tokio::time::sleep(Duration::from_secs_f64(t.max(0.0))).await;
                Ok(Value::Null)
            })
        });
        let ctx = json!({"command": command, "urls": urls});
        let builder = |o: Arc<HttpRequestObj>| -> ObjBuilder {
            Box::new(move |caller: Caller| {
                *o.caller.lock().unwrap() = Some(caller);
                minijinja::Value::from_dyn_object(o)
            })
        };
        let response = match request_template {
            Some(t) => {
                t.render_async_ext(
                    &ctx,
                    vec![("http_send", http_send.clone()), ("async_sleep", async_sleep.clone())],
                    vec![("http_request", builder(req_obj.clone()))],
                )
                .await?;
                req_obj.st.lock().unwrap().last_response.clone().ok_or_else(|| err("Failed to receive a response"))?
            }
            None => {
                let r = (send_rc)(req_obj.state_json()).await;
                req_obj.st.lock().unwrap().last_response = Some(r.clone());
                r
            }
        };
        if !response["error"].is_null() {
            let code = response["status_code"].as_i64().unwrap_or(500);
            return Err(ServerError::new(crate::pyfmt::py_str(&response["error"]), code));
        }
        let result = response_template
            .render_async_ext(&ctx, vec![("http_send", http_send), ("async_sleep", async_sleep)], vec![("http_request", builder(req_obj))])
            .await?
            .to_lowercase();
        if result != "on" && result != "off" {
            return Err(err(format!("Invalid result: {}", result)));
        }
        Ok(result)
    }
}

pub(super) fn tasmota_state(res: &Value, output_id: i64) -> Result<String, ServerError> {
    match res.get(format!("POWER{}", output_id)) {
        Some(v) => Ok(crate::pyfmt::py_str(v).to_lowercase()),
        None if output_id == 1 => res.get("POWER").map(|v| crate::pyfmt::py_str(v).to_lowercase()).ok_or_else(|| PowerDevice::key_err("POWER")),
        None => Err(PowerDevice::key_err(&format!("POWER{}", output_id))),
    }
}

pub(super) fn shelly_state(res: &Value, timer: &str) -> String {
    let ison = crate::pyfmt::py_truthy(&res["ison"]);
    let remaining = if timer.is_empty() { 0.0 } else { res["timer_remaining"].as_f64().unwrap_or(0.0) };
    if ison && remaining == 0.0 {
        "on".into()
    } else {
        "off".into()
    }
}

pub(super) fn loxone_state(res: &Value) -> Result<String, ServerError> {
    let v = &res["LL"]["value"];
    let n: i64 = match v {
        Value::String(s) => s.trim().parse().map_err(|_| ServerError::exc(format!("invalid literal for int() with base 10: '{}'", s)))?,
        Value::Number(n) => n.as_f64().unwrap_or(0.0) as i64,
        _ => return Err(PowerDevice::key_err("LL")),
    };
    Ok(if n == 1 { "on".into() } else { "off".into() })
}

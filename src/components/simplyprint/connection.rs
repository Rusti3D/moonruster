// The connection to SimplyPrint: websocket, its messages and the demands it sends

use super::*;

impl SimplyPrint {
    pub(super) async fn connect(self: Rc<Self>) {
        let mut log_connect = true;
        while !self.is_closing.get() {
            let mut url = self.connect_url.borrow().clone();
            if let Some(t) = self.reconnect_token.borrow().as_ref() {
                url = format!("{}/{}", url, t);
            }
            if log_connect {
                log::info!("Connecting To SimplyPrint: {}", url);
                log_connect = false;
            }
            match tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(url.as_str())).await {
                Ok(Ok((ws, _))) => {
                    self.last_ping_received.set(now());
                    log::info!("Connected to SimplyPrint Cloud");
                    self.clone().read_messages(ws).await;
                    log_connect = true;
                }
                res => {
                    let t = now();
                    if t - self.last_err_log_time.get() > CONNECTION_ERROR_LOG_TIME {
                        self.last_err_log_time.set(t);
                        let e = match res {
                            Ok(Err(e)) => e.to_string(),
                            _ => "Timeout while connecting".into(),
                        };
                        log::error!("Failed to connect to SimplyPrint\n{}", e);
                    }
                }
            }
            if !self.is_closing.get() {
                tokio::time::sleep(Duration::from_secs_f64(self.reconnect_delay.get())).await;
            }
        }
    }

    pub(super) async fn read_messages(
        self: Rc<Self>,
        ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    ) {
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
        *self.ws_tx.borrow_mut() = Some(tx);
        let (code, reason) = loop {
            tokio::select! {
                msg = stream.next() => match msg {
                    Some(Ok(Message::Text(t))) => self.process_message(t.as_str()),
                    Some(Ok(Message::Ping(_))) => self.last_ping_received.set(now()),
                    Some(Ok(Message::Close(f))) => break f.map(|f| (Some(u16::from(f.code)), Some(f.reason.to_string()))).unwrap_or((None, None)),
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break (None, None),
                },
                out = rx.recv() => {
                    if let Some(m) = out {
                        let _ = sink.send(m).await;
                    }
                }
            }
        };
        *self.ws_tx.borrow_mut() = None;
        self.ping_sp_timer.stop();
        let ping_time = now() - self.last_ping_received.get();
        log::info!(
            "SimplyPrint Disconnected - Code: {}, Reason: {}, Server Ping Time Elapsed: {}",
            code.map(|c| c.to_string()).unwrap_or("None".into()),
            reason.unwrap_or("None".into()),
            crate::pyfmt::py_float_repr(ping_time)
        );
        self.connected.set(false);
    }

    pub(super) fn process_message(self: &Rc<Self>, msg: &str) {
        self.logger.info(&format!("received: {}", msg));
        let packet: Value = match serde_json::from_str(msg) {
            Ok(p) => p,
            Err(_) => {
                log::debug!("Invalid message, not JSON: {}", msg);
                return;
            }
        };
        let event = packet.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let data = packet.get("data").cloned().filter(|d| !d.is_null());
        match event.as_str() {
            "connected" => {
                log::info!("SimplyPrint Reports Connection Success");
                self.connected.set(true);
                *self.reconnect_token.borrow_mut() = None;
                if let Some(d) = &data {
                    if d.get("in_setup").and_then(|v| v.as_i64()).unwrap_or(0) == 1 {
                        self.is_set_up.set(false);
                        self.save_item("printer_id", Value::Null);
                        self.save_item("printer_name", Value::Null);
                        if let Some(sid) = d.get("short_id") {
                            let s = self.clone();
                            let sid = crate::pyfmt::py_str(sid);
                            spawn(async move { s.announce_setup(&sid).await });
                        }
                    }
                    if let Some(Value::Object(iv)) = d.get("interval") {
                        self.update_intervals(iv);
                    }
                    *self.reconnect_token.borrow_mut() = d.get("reconnect_token").and_then(|v| v.as_str()).map(String::from);
                    if let Some(name) = d.get("name").filter(|n| !n.is_null()) {
                        self.save_item("printer_name", name.clone());
                    }
                }
                self.reconnect_delay.set(1.0);
                self.push_initial_state();
                let w = Rc::downgrade(self);
                self.ping_sp_timer.start(
                    0.0,
                    Rc::new(move || match w.upgrade() {
                        Some(s) => {
                            s.last_sp_ping.set(now());
                            s.send_sp("ping", Value::Null);
                            s.interval("ping")
                        }
                        None => 3600.0,
                    }),
                );
            }
            "error" => {
                log::info!("SimplyPrint Connection Error: {}", py_repr(&data.clone().unwrap_or(Value::Null)));
                self.reconnect_delay.set(30.0);
                *self.reconnect_token.borrow_mut() = None;
            }
            "new_token" => {
                let d = match data {
                    None => {
                        log::debug!("Invalid message, no data");
                        return;
                    }
                    Some(d) => d,
                };
                if d.get("no_exist").and_then(|v| v.as_bool()) == Some(true) && self.is_set_up.get() {
                    self.is_set_up.set(false);
                    self.save_item("printer_id", Value::Null);
                }
                let token = match d.get("token") {
                    Some(Value::String(t)) => {
                        log::info!("SimplyPrint Token Received");
                        Value::String(t.clone())
                    }
                    other => {
                        log::debug!("Invalid token received: {}", py_repr(&other.cloned().unwrap_or(Value::Null)));
                        Value::Null
                    }
                };
                self.save_item("printer_token", token);
                self.set_ws_url();
                if let Some(sid) = d.get("short_id") {
                    match sid {
                        Value::String(s) => {
                            let sp = self.clone();
                            let s = s.clone();
                            spawn(async move { sp.announce_setup(&s).await });
                        }
                        other => self.logger.debug(&format!("Invalid short_id received: {}", py_repr(other))),
                    }
                }
            }
            "complete_setup" => {
                let d = match data {
                    None => {
                        log::debug!("Invalid message, no data");
                        return;
                    }
                    Some(d) => d,
                };
                let pid = d.get("printer_id").cloned().unwrap_or(Value::Null);
                if pid.is_null() {
                    log::debug!("Invalid printer id, received null (None) value");
                }
                self.save_item("printer_id", json!(crate::pyfmt::py_str(&pid)));
                self.set_ws_url();
                self.save_item("temp_short_setup_id", Value::Null);
                let s = self.clone();
                spawn(async move { s.remove_setup_announcement().await });
            }
            "demand" => {
                let mut d = match data {
                    Some(Value::Object(d)) => d,
                    _ => {
                        log::debug!("Invalid message, no data");
                        return;
                    }
                };
                let demand = d.shift_remove("demand").map(|v| crate::pyfmt::py_str(&v)).unwrap_or("unknown".into());
                self.process_demand(&demand, d);
            }
            "interval_change" => {
                if let Some(Value::Object(iv)) = &data {
                    self.update_intervals(iv);
                }
            }
            "pong" => {
                let diff = now() - self.last_sp_ping.get();
                self.send_sp("latency", json!({"ms": (diff * 1000.0 + 0.5) as i64}));
            }
            _ => log::debug!("Unknown event: {}", msg),
        }
    }

    pub(super) fn process_demand(self: &Rc<Self>, demand: &str, args: Map<String, Value>) {
        let kconn = server().klippy();
        match demand {
            "pause" | "resume" | "cancel" => {
                if !kconn.is_connected() {
                    return;
                }
                let s = self.clone();
                let d = demand.to_string();
                spawn(async move { s.request_print_action(&d).await });
            }
            "terminal" => {
                if let Some(e) = args.get("enabled") {
                    self.gcode_terminal_enabled.set(crate::pyfmt::py_truthy(e));
                }
            }
            "gcode" => {
                if !kconn.is_connected() {
                    return;
                }
                let list: Vec<String> = args.get("list").and_then(|v| v.as_array()).map(|a| a.iter().map(crate::pyfmt::py_str).collect()).unwrap_or_default();
                let ident = args.get("identifier").filter(|v| !v.is_null()).map(crate::pyfmt::py_str);
                if !list.is_empty() {
                    let script = list.join("\n");
                    let s = self.clone();
                    spawn(async move {
                        let (success, msg) = match server().klippy_apis().run_gcode(&script).await {
                            Ok(_) => (true, Value::Null),
                            Err(e) => (false, json!(e.message)),
                        };
                        if let Some(id) = ident {
                            s.send_sp("gcode_executed", json!({"identifier": id, "success": success, "message": msg}));
                        }
                    });
                }
            }
            "webcam_snapshot" => {
                let s = self.clone();
                spawn(async move { s.post_image(args).await });
            }
            "file" => {
                let url = match args.get("url") {
                    Some(Value::String(u)) => u.clone(),
                    _ => {
                        log::debug!("Invalid url in message");
                        return;
                    }
                };
                let start = args.get("auto_start").map(crate::pyfmt::py_truthy).unwrap_or(false);
                let jid = args.get("job_id").and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0);
                self.current_job_id.set(if jid != 0 { Some(jid) } else { None });
                let s = self.clone();
                let h = spawn(async move { s.download_sp_file(&url, start).await });
                self.download_task.set(h);
            }
            "start_print" => {
                if kconn.is_connected() && self.cache.borrow().state == "operational" {
                    let s = self.clone();
                    spawn(async move { s.start_print().await });
                } else {
                    log::debug!("Failed to start print");
                }
            }
            "system_restart" => {
                spawn(async {
                    Self::call_internal_api("machine.reboot", Map::new()).await;
                });
            }
            "system_shutdown" => {
                spawn(async {
                    Self::call_internal_api("machine.shutdown", Map::new()).await;
                });
            }
            "api_restart" | "api_shutdown" => {
                let action = if demand == "api_restart" { "restart" } else { "shutdown" };
                spawn(async move {
                    if let Some(m) = server().get::<crate::components::machine::Machine>() {
                        let _ = m.do_service_action(action, "moonraker").await;
                    }
                });
            }
            "psu_on" | "psu_off" => {
                if !self.power_id.is_empty() {
                    if let Some(p) = server().get::<crate::components::power::PrinterPower>() {
                        p.set_device_power(&self.power_id, if demand == "psu_on" { "on" } else { "off" }, false);
                    }
                }
            }
            "test_webcam" => {
                let s = self.clone();
                spawn(async move {
                    s.test_connection().await;
                    s.send_sp("webcam_status", json!({"connected": s.webcam_connected.get()}));
                });
            }
            _ => log::debug!("Unknown demand: {}", demand),
        }
    }

    pub(super) fn save_item(&self, name: &str, data: Value) {
        if data.is_null() {
            self.sp_info.borrow_mut().shift_remove(name);
            let _ = self.spdb.pop(name, Some(Value::Null));
        } else {
            self.sp_info.borrow_mut().insert(name.into(), data.clone());
            let _ = self.spdb.insert(name, data);
        }
    }

    pub(super) async fn call_internal_api(method: &str, args: Map<String, Value>) -> Option<Value> {
        let it = crate::components::application::InternalTransport;
        it.call_method(method, args.into_iter().collect()).await.ok()
    }

    pub(super) fn set_ws_url(&self) {
        let info = self.sp_info.borrow();
        let token = info.get("printer_token").filter(|v| !v.is_null()).map(crate::pyfmt::py_str);
        let printer_id = info.get("printer_id").filter(|v| !v.is_null()).map(crate::pyfmt::py_str);
        let host = if self.test { "testws3.simplyprint.io" } else { "ws.simplyprint.io" };
        let ep = format!("wss://{}/{}/p", host, SP_VERSION);
        let mut url = format!("{}/0/0", ep);
        if let Some(t) = token {
            match printer_id {
                None => url = format!("{}/0/{}", ep, t),
                Some(p) => {
                    self.is_set_up.set(true);
                    url = format!("{}/{}/{}", ep, p, t);
                }
            }
        }
        *self.connect_url.borrow_mut() = url;
    }

    pub(super) fn update_intervals(&self, iv: &Map<String, Value>) {
        let mut i = self.intervals.borrow_mut();
        for (k, v) in iv {
            i.insert(k.clone(), v.as_f64().unwrap_or(0.0) / 1000.0);
        }
        let dbg: Map<String, Value> = i.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        log::debug!("Intervals Updated: {}", py_repr(&Value::Object(dbg)));
    }

    pub(super) async fn announce_setup(&self, short_id: &str) {
        let _g = self.announce_mutex.lock().await;
        let eid = self.sp_info.borrow().get("announcement_id").filter(|v| !v.is_null()).map(crate::pyfmt::py_str);
        if eid.is_some() && self.sp_info.borrow().get("temp_short_setup_id").and_then(|v| v.as_str()) == Some(short_id) {
            return;
        }
        let ann = match server().get::<crate::components::announcements::Announcements>() {
            Some(a) => a,
            None => return,
        };
        if let Some(e) = &eid {
            let _ = ann.remove_announcement(e).await;
        }
        self.save_item("temp_short_setup_id", json!(short_id));
        let entry = ann.add_internal_announcement(
            "SimplyPrint Setup Request",
            &format!(
                "SimplyPrint is ready to complete setup for your printer. Please log in to your account and enter the following setup code:\n\n{}\n\n",
                short_id
            ),
            "https://simplyprint.io",
            "high",
            "simplyprint",
        );
        self.save_item("announcement_id", entry.get("entry_id").cloned().unwrap_or(Value::Null));
    }

    pub(super) async fn remove_setup_announcement(&self) {
        let _g = self.announce_mutex.lock().await;
        let eid = match self.sp_info.borrow().get("announcement_id").filter(|v| !v.is_null()).map(crate::pyfmt::py_str) {
            None => return,
            Some(e) => e,
        };
        self.save_item("announcement_id", Value::Null);
        if let Some(ann) = server().get::<crate::components::announcements::Announcements>() {
            let _ = ann.remove_announcement(&eid).await;
        }
    }

    pub(super) async fn request_print_action(&self, action: &str) {
        let cur_state = self.cache.borrow().state.clone();
        let kapis = server().klippy_apis();
        self.print_request_event.clear();
        let mut requested = false;
        let ret = match action {
            "pause" if cur_state == "printing" => {
                self.update_state("pausing");
                requested = true;
                kapis.pause_print().await
            }
            "resume" if cur_state == "paused" => {
                self.update_state("resuming");
                requested = true;
                kapis.resume_print().await
            }
            "cancel" if cur_state == "printing" || cur_state == "paused" => {
                self.update_state("cancelling");
                requested = true;
                kapis.cancel_print().await
            }
            _ => Ok(json!("")),
        };
        // python passes default=None so request errors resolve to None
        if requested && ret.is_err() {
            let _ = tokio::time::timeout(Duration::from_secs(1), self.print_request_event.wait()).await;
            self.update_state_from_klippy();
        }
    }
}

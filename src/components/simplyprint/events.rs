// Klippy and Moonraker events, and the state reported to SimplyPrint

use super::*;

impl SimplyPrint {
    pub(super) async fn on_klippy_ready(self: Rc<Self>) {
        let js: Option<Rc<crate::components::job_state::JobState>> = server().lookup_component("job_state");
        let last_stats = js.map(|j| j.get_last_stats()).unwrap_or_default();
        match last_stats.get("state").and_then(|v| v.as_str()) {
            None => {
                log::error!("KeyError: 'state'");
                return;
            }
            Some("printing") => self.on_print_started(&Value::Object(last_stats.clone()), false),
            Some(_) => self.update_state("operational"),
        }
        let kapis = server().klippy_apis();
        let mut q = Map::new();
        q.insert("heaters".into(), Value::Null);
        let query = kapis.query_objects(&q).await.ok();
        let mut sub_objs = Map::new();
        sub_objs.insert("display_status".into(), json!(["progress"]));
        sub_objs.insert("virtual_sdcard".into(), json!(["file_position", "progress"]));
        sub_objs.insert("bed_mesh".into(), json!(["mesh_matrix", "mesh_min", "mesh_max"]));
        sub_objs.insert("toolhead".into(), json!(["extruder"]));
        sub_objs.insert("gcode_move".into(), json!(["gcode_position"]));
        sub_objs.insert("exclude_object".into(), json!(["objects", "excluded_objects", "current_object"]));
        let mut has_amb = false;
        let cfg_amb = self.amb.sensor_name.clone();
        if let Some(q) = query {
            let heaters = q.get("heaters").cloned().unwrap_or(json!({}));
            let mut avail: Vec<String> = heaters.get("available_heaters").and_then(|v| v.as_array()).map(|a| a.iter().map(crate::pyfmt::py_str).collect()).unwrap_or_default();
            avail.sort();
            log::debug!("SimplyPrint: Heaters Detected: {}", py_repr(&json!(avail)));
            let mut htrs = self.heaters.borrow_mut();
            for h in avail {
                if h.starts_with("extruder") {
                    sub_objs.insert(h.clone(), json!(["temperature", "target"]));
                    let tool = if h == "extruder" { "tool0".to_string() } else { format!("tool{}", &h[8..]) };
                    htrs.retain(|(k, _)| *k != h);
                    htrs.push((h, tool));
                } else if h == "heater_bed" {
                    sub_objs.insert(h.clone(), json!(["temperature", "target"]));
                    htrs.retain(|(k, _)| *k != h);
                    htrs.push((h, "bed".into()));
                }
            }
            drop(htrs);
            let sensors: Vec<String> = heaters.get("available_sensors").and_then(|v| v.as_array()).map(|a| a.iter().map(crate::pyfmt::py_str).collect()).unwrap_or_default();
            if !cfg_amb.is_empty() {
                if sensors.contains(&cfg_amb) {
                    has_amb = true;
                    sub_objs.insert(cfg_amb.clone(), json!(["temperature"]));
                } else {
                    log::info!("SimplyPrint: Ambient sensor {} not configured in Klipper", cfg_amb);
                }
            }
        }
        if !self.filament_sensor.is_empty() {
            let objects = kapis.get_object_list().await.unwrap_or_default();
            if objects.iter().any(|o| o.as_str() == Some(self.filament_sensor.as_str())) {
                sub_objs.insert(self.filament_sensor.clone(), json!(["filament_detected"]));
            }
        }
        let transport: Rc<dyn ApiTransport> = self.clone();
        let status = kapis.subscribe_from_transport(&sub_objs, transport, false).await.unwrap_or(json!({}));
        if let Value::Object(status) = status {
            if !status.is_empty() {
                log::debug!("SimplyPrint: Got Initial Status: {}", py_repr(&Value::Object(status.clone())));
                *self.printer_status.borrow_mut() = status.clone();
                self.update_temps(1.0);
                self.next_temp_update_time.set(0.0);
                if status.contains_key("bed_mesh") {
                    self.send_mesh_data();
                }
                if let Some(e) = status.get("toolhead").and_then(|t| t.get("extruder")).and_then(|v| v.as_str()) {
                    self.send_active_extruder(e);
                }
                if let Some(gm) = status.get("gcode_move") {
                    self.layer_detect.borrow_mut().update(gm.get("gcode_position").unwrap_or(&Value::Null));
                }
                if let Some(eo) = status.get("exclude_object").and_then(|v| v.as_object()) {
                    self.handle_exclude_object(eo);
                }
                if !self.filament_sensor.is_empty() {
                    if let Some(fs) = status.get(&self.filament_sensor) {
                        self.send_filament_state(fs);
                    }
                }
                if has_amb {
                    if let Some(a) = status.get(&cfg_amb) {
                        self.update_ambient(a, SAMPLE_CHECK_TIME);
                    }
                }
            }
        }
        if !has_amb {
            self.amb_start();
        }
        let w = Rc::downgrade(&self);
        self.printer_info_timer.start(
            1.0,
            Rc::new(move || match w.upgrade() {
                Some(s) => {
                    if s.cache.borrow().state == "printing" {
                        s.update_job_progress();
                    }
                    s.interval("job")
                }
                None => 3600.0,
            }),
        );
    }

    pub(super) fn send_filament_state(&self, fs: &Value) {
        let detected = fs.get("filament_detected").map(crate::pyfmt::py_truthy).unwrap_or(false);
        let state = if detected { "loaded" } else { "runout" };
        self.cache.borrow_mut().filament_state = state.into();
        self.send_sp("filament_sensor", json!({"state": state}));
    }

    pub(super) fn on_websocket_identified(&self, uid: u64) {
        let conn = match server().wsm().get_client(uid) {
            Some(c) => c,
            None => return,
        };
        let cd = conn.client_data();
        if self.cache.borrow().current_wsid.is_none() && cd.get("type").map(|s| s.as_str()) == Some("web") {
            let ui = json!({"ui": cd.get("name"), "ui_version": cd.get("version")});
            {
                let mut c = self.cache.borrow_mut();
                for (k, v) in ui.as_object().unwrap() {
                    c.firmware_info.insert(k.clone(), v.clone());
                }
                c.current_wsid = Some(uid);
            }
            self.send_sp("machine_data", ui);
        }
    }

    pub(super) fn on_websocket_removed(&self, uid: u64) {
        if self.cache.borrow().current_wsid != Some(uid) {
            return;
        }
        let ui = self.get_ui_info();
        let diff = diff_obj(&ui, &self.cache.borrow().firmware_info);
        if !diff.is_empty() {
            self.cache.borrow_mut().firmware_info.extend(ui.clone());
            self.send_sp("machine_data", Value::Object(ui));
        }
    }

    pub(super) fn on_klippy_startup(&self, state: KlippyState) {
        if state != KlippyState::READY {
            self.update_state("error");
            self.send_sp("printer_error", json!({"error": server().klippy().state_message()}));
        }
        self.send_sp("connection", json!({"new": "connected"}));
        self.send_firmware_data();
    }

    pub(super) fn on_klippy_disconnected(&self) {
        self.update_state("offline");
        self.send_sp("connection", json!({"new": "disconnected"}));
        self.amb_stop();
        self.printer_info_timer.stop();
        self.cache.borrow_mut().reset_print_state();
        self.printer_status.borrow_mut().clear();
    }

    pub(super) fn on_job_state_changed(self: &Rc<Self>, args: Vec<Value>) {
        let evt = args.first().and_then(|v| v.as_str()).unwrap_or("").to_string();
        let prev = args.get(1).cloned().unwrap_or(json!({}));
        let new = args.get(2).cloned().unwrap_or(json!({}));
        match evt.as_str() {
            "started" => self.on_print_started(&new, true),
            "paused" => {
                self.send_sp("job_info", json!({"paused": true}));
                self.update_state("paused");
                self.layer_detect.borrow_mut().stop();
            }
            "resumed" => {
                self.update_state("printing");
                self.layer_detect.borrow_mut().resume();
            }
            "cancelled" | "complete" | "error" => {
                self.check_job_started(&new);
                self.reset_file();
                let payload = match evt.as_str() {
                    "cancelled" => json!({"cancelled": true}),
                    "complete" => json!({"finished": true}),
                    _ => json!({"failed": true, "error": new.get("message").cloned().unwrap_or(json!("Unknown Error"))}),
                };
                self.send_job_event(payload.as_object().cloned().unwrap());
                self.update_state_from_klippy();
                self.cache.borrow_mut().job_info.clear();
                self.layer_detect.borrow_mut().stop();
            }
            "standby" => {
                self.update_state_from_klippy();
                self.cache.borrow_mut().job_info.clear();
                self.layer_detect.borrow_mut().stop();
            }
            _ => log::info!("No defined callback for Job Event: {}", evt),
        }
        let _ = prev;
    }

    pub(super) fn on_print_started(&self, new_stats: &Value, need_start_event: bool) {
        self.update_state("printing");
        let filename = new_stats.get("filename").map(crate::pyfmt::py_str).unwrap_or_default();
        let mut job_info = Map::new();
        job_info.insert("filename".into(), json!(filename));
        let fm: Option<Rc<crate::components::file_manager::FileManager>> = server().lookup_component("file_manager");
        let metadata = fm.map(|f| f.get_file_metadata(&filename)).unwrap_or(json!({}));
        if let Some(f) = metadata.get("filament_total").and_then(|v| v.as_f64()) {
            job_info.insert("filament".into(), json!(crate::pyfmt::py_round(f, 0) as i64));
        }
        if let Some(t) = metadata.get("estimated_time").filter(|v| !v.is_null()) {
            job_info.insert("time".into(), t.clone());
        }
        {
            let mut c = self.cache.borrow_mut();
            c.metadata = metadata.clone();
            c.job_info.extend(job_info.clone());
        }
        if need_start_event {
            job_info.insert("started".into(), json!(true));
        }
        self.layer_detect.borrow_mut().start(&metadata);
        self.send_job_event(job_info);
    }

    pub(super) fn check_job_started(&self, new_stats: &Value) {
        if self.cache.borrow().job_info.is_empty() {
            let mut ji = Map::new();
            ji.insert("filename".into(), new_stats.get("filename").cloned().unwrap_or(json!("")));
            ji.insert("started".into(), json!(true));
            self.send_job_event(ji);
        }
    }

    pub(super) fn reset_file(&self) {
        let cur = self.cache.borrow().job_info.get("filename").map(crate::pyfmt::py_str).unwrap_or_default();
        let last = self.last_started.borrow().clone();
        if !last.is_empty() && last == cur {
            spawn(async {
                let _ = server().klippy_apis().run_gcode("SDCARD_RESET_FILE").await;
            });
        }
        self.last_started.borrow_mut().clear();
    }

    pub(super) fn on_proc_update(&self, stats: Value) {
        let cpu = match stats.get("system_cpu_usage") {
            Some(c) if crate::pyfmt::py_truthy(c) => c.clone(),
            _ => return,
        };
        let t = now();
        if t - self.last_cpu_update_time.get() < self.interval("cpu") {
            return;
        }
        self.last_cpu_update_time.set(t);
        let mut mem_pct = 0.0;
        if let Some(m) = stats.get("system_memory").filter(|m| crate::pyfmt::py_truthy(m)) {
            mem_pct = m["used"].as_f64().unwrap_or(0.0) / m["total"].as_f64().unwrap_or(1.0) * 100.0;
        }
        let mut cpu_data = Map::new();
        cpu_data.insert("usage".into(), json!((cpu["cpu"].as_f64().unwrap_or(0.0) + 0.5) as i64));
        cpu_data.insert("memory".into(), json!((mem_pct + 0.5) as i64));
        cpu_data.insert("flags".into(), self.cache.borrow().throttled_state.get("bits").cloned().unwrap_or(json!(0)));
        if let Some(temp) = stats.get("cpu_temp").and_then(|v| v.as_f64()) {
            cpu_data.insert("temp".into(), json!((temp + 0.5) as i64));
        }
        let diff = diff_obj(&cpu_data, &self.cache.borrow().cpu_info);
        if !diff.is_empty() {
            self.cache.borrow_mut().cpu_info.extend(cpu_data);
            self.send_sp("cpu", Value::Object(diff));
        }
    }

    pub(super) fn handle_exclude_object(&self, eo: &Map<String, Value>) {
        let diff = diff_obj(eo, &self.cache.borrow().exclude_object_status);
        if diff.is_empty() {
            return;
        }
        self.cache.borrow_mut().exclude_object_status = eo.clone();
        if let Some(objs) = diff.get("objects").filter(|o| crate::pyfmt::py_truthy(o)) {
            self.send_sp("objects", objs.clone());
        }
        let mut job = Map::new();
        if let Some(co) = diff.get("current_object") {
            job.insert("object".into(), co.clone());
        }
        if let Some(sk) = diff.get("excluded_objects").filter(|o| crate::pyfmt::py_truthy(o)) {
            job.insert("skipped_objects".into(), sk.clone());
        }
        if !job.is_empty() {
            self.send_sp("job_info", Value::Object(job));
        }
    }

    pub(super) fn update_job_progress(&self) {
        let mut job_info = Map::new();
        let md = self.cache.borrow().metadata.clone();
        let js: Option<Rc<crate::components::job_state::JobState>> = server().lookup_component("job_state");
        let last_stats = js.map(|j| j.get_last_stats()).unwrap_or_default();
        if let Some(est) = md.get("estimated_time").and_then(|v| v.as_f64()) {
            let duration = last_stats.get("print_duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let time_left = ((est - duration + 0.5) as i64).max(0);
            let last_time_left = self.cache.borrow().job_info.get("time").and_then(|v| v.as_f64()).unwrap_or(time_left as f64 + 60.0);
            let time_diff = last_time_left - time_left as f64;
            if (time_left < 60 || time_diff >= 30.0) && time_left as f64 != last_time_left {
                job_info.insert("time".into(), json!(time_left));
            }
        }
        let ps = self.printer_status.borrow().clone();
        let mut progress: Option<f64> = ps.get("display_status").and_then(|d| d.get("progress")).and_then(|v| v.as_f64()).filter(|p| *p != 0.0);
        if progress.is_none() {
            if let Some(vs) = ps.get("virtual_sdcard") {
                if let Some(fp) = vs.get("file_position").and_then(|v| v.as_f64()) {
                    let start = md.get("gcode_start_byte").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let end = md.get("gcode_end_byte").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let len = end - start;
                    if start != 0.0 && end != 0.0 && len > 0.0 {
                        progress = Some(((fp - start) / len).clamp(0.0, 1.0)).filter(|p| *p != 0.0);
                    }
                }
                if progress.is_none() {
                    progress = vs.get("progress").and_then(|v| v.as_f64()).filter(|p| *p != 0.0);
                }
            }
        }
        if let Some(p) = progress {
            let pct = (p * 100.0 + 0.5) as i64;
            if Some(pct) != Some(self.cache.borrow().job_info.get("progress").and_then(|v| v.as_i64()).unwrap_or(0)) {
                job_info.insert("progress".into(), json!(pct));
            }
        }
        let layer = last_stats
            .get("info")
            .and_then(|i| i.get("current_layer"))
            .and_then(|v| v.as_i64())
            .unwrap_or(self.layer_detect.borrow().layer);
        if Some(layer) != Some(self.cache.borrow().job_info.get("layer").and_then(|v| v.as_i64()).unwrap_or(-1)) {
            job_info.insert("layer".into(), json!(layer));
        }
        if !job_info.is_empty() {
            self.cache.borrow_mut().job_info.extend(job_info.clone());
            self.send_sp("job_info", Value::Object(job_info));
        }
    }

    pub(super) fn update_temps(&self, eventtime: f64) {
        if eventtime < self.next_temp_update_time.get() {
            return;
        }
        let mut need_rapid = false;
        let mut temp_data = Map::new();
        let heaters = self.heaters.borrow().clone();
        let ps = self.printer_status.borrow().clone();
        for (obj, key) in heaters {
            let st = match ps.get(&obj) {
                Some(s) => s,
                None => continue,
            };
            let reported = st.get("temperature").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let target = st.get("target").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let ret = [(reported + 0.5) as i64, (target + 0.5) as i64];
            let last = self
                .cache
                .borrow()
                .temps
                .get(&key)
                .and_then(|v| v.as_array().map(|a| [a.first().and_then(|x| x.as_f64()).unwrap_or(-100.0), a.get(1).and_then(|x| x.as_f64()).unwrap_or(-100.0)]))
                .unwrap_or([-100.0, -100.0]);
            if ret[1] as f64 == last[1] {
                let seeking = if ret[1] != 0 { (ret[1] - ret[0]).abs() > 5 } else { ret[0] >= self.amb.ambient.get() + 25 };
                need_rapid |= seeking;
                let lr = self.last_received_temps.borrow().get(&key).copied();
                if let (Some(lr), false) = (lr, seeking) {
                    if (reported - lr).abs() < 0.75 {
                        self.last_received_temps.borrow_mut().remove(&key);
                        continue;
                    }
                }
                if ret[0] as f64 == last[0] {
                    self.last_received_temps.borrow_mut().insert(key.clone(), reported);
                    continue;
                }
                temp_data.insert(key.clone(), json!([ret[0]]));
            } else {
                temp_data.insert(key.clone(), json!(ret));
            }
            self.last_received_temps.borrow_mut().insert(key.clone(), reported);
            self.cache.borrow_mut().temps.insert(key, json!(ret));
        }
        if need_rapid {
            let tt = self.interval("temps_target");
            self.next_temp_update_time.set(if tt < 0.2501 { 0.0 } else { eventtime + tt });
        } else {
            self.next_temp_update_time.set(eventtime + self.interval("temps"));
        }
        if !temp_data.is_empty() {
            self.send_sp("temps", Value::Object(temp_data));
        }
    }

    pub(super) fn update_state_from_klippy(&self) {
        let st = server().klippy().state();
        let sp = if st == KlippyState::READY {
            "operational"
        } else if st == KlippyState::ERROR || st == KlippyState::SHUTDOWN {
            "error"
        } else {
            "offline"
        };
        self.update_state(sp);
    }

    pub(super) fn update_state(&self, new_state: &str) {
        if self.cache.borrow().state == new_state {
            return;
        }
        self.cache.borrow_mut().state = new_state.into();
        self.send_sp("state_change", json!({"new": new_state}));
        if new_state == "operational" {
            self.print_ready_event.set();
        }
    }

    pub(super) fn send_mesh_data(&self) {
        let mesh = self.printer_status.borrow().get("bed_mesh").cloned().unwrap_or(json!({}));
        self.cache.borrow_mut().mesh = mesh.clone();
        self.send_sp("mesh_data", mesh);
    }

    pub(super) fn send_job_event(&self, mut job_info: Map<String, Value>) {
        if self.connected.get() {
            self.send_sp("job_info", Value::Object(job_info));
        } else {
            job_info.extend(self.cache.borrow().job_info.clone());
            job_info.insert("delay".into(), json!(now()));
            let mut m = self.missed_job_events.borrow_mut();
            m.push(job_info);
            if m.len() > 10 {
                m.remove(0);
            }
        }
    }

    pub(super) fn get_ui_info(&self) -> Map<String, Value> {
        let mut ui = Map::new();
        ui.insert("ui".into(), Value::Null);
        ui.insert("ui_version".into(), Value::Null);
        self.cache.borrow_mut().current_wsid = None;
        let conns = server().wsm().get_clients_by_type("web");
        if let Some(longest) = conns.first() {
            let cd = longest.client_data();
            ui.insert("ui".into(), json!(cd.get("name")));
            ui.insert("ui_version".into(), json!(cd.get("version")));
            self.cache.borrow_mut().current_wsid = Some(longest.uid());
        }
        ui
    }

    pub(super) async fn send_machine_data(&self) {
        let srv = server();
        let mut data = self.get_ui_info();
        data.insert("api".into(), json!("Moonraker"));
        data.insert("api_version".into(), srv.get_app_args().get("software_version").cloned().unwrap_or(Value::Null));
        data.insert("sp_version".into(), json!(COMPONENT_VERSION));
        let machine: Rc<crate::components::machine::Machine> = match srv.lookup_component("machine") {
            Some(m) => m,
            None => return,
        };
        let sys_info = machine.get_system_info();
        let pyver: Vec<String> = sys_info
            .get("python")
            .and_then(|p| p.get("version"))
            .and_then(|v| v.as_array())
            .map(|a| a.iter().take(3).map(crate::pyfmt::py_str).collect())
            .unwrap_or_default();
        data.insert("python_version".into(), json!(pyver.join(".")));
        let cpu = sys_info.get("cpu_info").cloned().unwrap_or(json!({}));
        let mut model = cpu.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if model.is_empty() || model.chars().all(|c| c.is_ascii_digit()) {
            model = cpu.get("cpu_desc").and_then(|v| v.as_str()).unwrap_or("Unknown").to_string();
        }
        data.insert("machine".into(), json!(model));
        data.insert("os".into(), sys_info.get("distribution").and_then(|d| d.get("name")).cloned().unwrap_or(json!("Unknown")));
        let pub_intf = machine.get_public_network().await;
        data.insert("is_ethernet".into(), json!((!pub_intf["is_wifi"].as_bool().unwrap_or(false)) as i64));
        data.insert("ssid".into(), pub_intf.get("ssid").cloned().unwrap_or(json!("")));
        data.insert("local_ip".into(), pub_intf.get("address").cloned().unwrap_or(json!("Unknown")));
        data.insert("hostname".into(), pub_intf["hostname"].clone());
        data.insert("core_count".into(), json!(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)));
        if let Some(mem) = cpu.get("total_memory").and_then(|v| v.as_i64()) {
            data.insert("total_memory".into(), json!(mem * 1024));
        }
        self.cache.borrow_mut().machine_info = data.clone();
        self.send_sp("machine_data", Value::Object(data));
    }

    pub(super) fn send_firmware_data(&self) {
        let kinfo = server().get_klippy_info();
        let version = match kinfo.get("software_version").and_then(|v| v.as_str()) {
            None => return,
            Some(v) => v.to_string(),
        };
        let mut firmware_date = String::new();
        let kpath = PathBuf::from(kinfo.get("klipper_path").and_then(|v| v.as_str()).unwrap_or("")).join("klippy");
        if kpath.is_dir() {
            if let Ok(m) = std::fs::metadata(&kpath).and_then(|m| m.modified()) {
                firmware_date = chrono::DateTime::<chrono::Utc>::from(m).format("%a %b %e %H:%M:%S %Y").to_string();
            }
        }
        let unsafe_ = version.ends_with("-dirty") || version == "?";
        let version = if unsafe_ { version.rsplit_once('-').map(|(a, _)| a.to_string()).unwrap_or(version) } else { version };
        let fw = json!({
            "firmware": "Klipper",
            "firmware_version": version,
            "firmware_date": firmware_date,
            "firmware_link": "https://github.com/Klipper3d/klipper",
        });
        let fw = fw.as_object().cloned().unwrap();
        let diff = diff_obj(&fw, &self.cache.borrow().firmware_info);
        if !diff.is_empty() {
            self.cache.borrow_mut().firmware_info = fw;
            self.send_sp("firmware", json!({"fw": diff, "raw": false, "unsafe": unsafe_}));
        }
    }

    pub(super) fn send_active_extruder(&self, new_extruder: &str) {
        let tool = if new_extruder == "extruder" { "T0".to_string() } else { format!("T{}", new_extruder.get(8..).unwrap_or("")) };
        if tool == self.cache.borrow().active_extruder {
            return;
        }
        self.cache.borrow_mut().active_extruder = tool.clone();
        self.send_sp("tool", json!({"new": tool}));
    }

    pub(super) fn push_initial_state(self: &Rc<Self>) {
        let st = self.cache.borrow().state.clone();
        self.send_sp("state_change", json!({"new": st}));
        let temps = self.cache.borrow().temps.clone();
        if !temps.is_empty() {
            self.send_sp("temps", Value::Object(temps));
        }
        let fw = self.cache.borrow().firmware_info.clone();
        if !fw.is_empty() {
            self.send_sp("firmware", json!({"fw": fw, "raw": false}));
        }
        let t = now();
        let missed = std::mem::take(&mut *self.missed_job_events.borrow_mut());
        for mut evt in missed {
            let d = evt.get("delay").and_then(|v| v.as_f64()).unwrap_or(t);
            evt.insert("delay".into(), json!(((t - d) + 0.5) as i64));
            self.send_sp("job_info", Value::Object(evt));
        }
        let ae = self.cache.borrow().active_extruder.clone();
        if !ae.is_empty() {
            self.send_sp("tool", json!({"new": ae}));
        }
        let cpu = self.cache.borrow().cpu_info.clone();
        if !cpu.is_empty() {
            self.send_sp("cpu_info", Value::Object(cpu));
        }
        self.send_sp("ambient", json!({"new": self.amb.ambient.get()}));
        if !self.power_id.is_empty() {
            let s = self.clone();
            spawn(async move {
                let mut a = Map::new();
                a.insert("device".into(), json!(s.power_id));
                if let Some(info) = Self::call_internal_api("machine.device_power.get_device", a).await {
                    let on = info.get(&s.power_id).and_then(|v| v.as_str()) == Some("on");
                    s.send_sp("power_controller", json!({"on": on}));
                }
            });
        }
        let fs = self.cache.borrow().filament_state.clone();
        if !fs.is_empty() {
            self.send_sp("filament_sensor", json!({"state": fs}));
        }
        self.send_sp("webcam_status", json!({"connected": self.webcam_connected.get()}));
        let s = self.clone();
        spawn(async move { s.send_machine_data().await });
        let s = self.clone();
        spawn(async move {
            let cfg = s.webcam_cam.borrow().as_ref().map(|c| c.as_dict()).unwrap_or(json!({}));
            s.send_sp(
                "webcam",
                json!({
                    "flipH": cfg.get("flip_horizontal").cloned().unwrap_or(json!(false)),
                    "flipV": cfg.get("flip_vertical").cloned().unwrap_or(json!(false)),
                    "rotate90": cfg.get("rotation").and_then(|v| v.as_i64()).unwrap_or(0) == 90,
                }),
            );
        });
    }

    pub(super) fn send_sp(&self, evt_name: &str, data: Value) -> bool {
        let tx = self.ws_tx.borrow().clone();
        let tx = match tx {
            Some(t) if self.connected.get() && (self.is_set_up.get() || PRE_SETUP_EVENTS.contains(&evt_name)) => t,
            _ => return false,
        };
        let mut data = data;
        if (evt_name == "file_progress" || evt_name == "job_info") && data.is_object() {
            if let Some(j) = self.current_job_id.get() {
                data["job_id"] = json!(j);
            }
        }
        let packet = json!({"type": evt_name, "data": data});
        if tx.send(Message::Text(packet.to_string().into())).is_err() {
            return false;
        }
        if evt_name != "stream" {
            self.logger.info(&format!("sent: {}", py_repr(&packet)));
        } else {
            self.logger.info("sent: webcam stream");
        }
        true
    }
}

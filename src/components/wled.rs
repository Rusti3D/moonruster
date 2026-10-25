// WLED neopixel support (port of moonraker/components/wled.py)

use crate::async_serial::AsyncSerial;
use crate::common::*;
use crate::components::http_client::{Body, HttpClient, RequestOpts};
use crate::confighelper::{ConfigError, ConfigHelper};
use crate::server::{server, spawn, Component};
use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const COLORSIZE: usize = 4;

enum Transport {
    Http { url: String, timeout: f64, client: Rc<HttpClient> },
    Serial { serial: Rc<AsyncSerial>, enabled: Cell<bool>, task: crate::server::TaskSlot, pending: RefCell<Option<tokio::sync::oneshot::Sender<Value>>> },
}

pub struct Strip {
    name: String,
    request_mutex: tokio::sync::Mutex<()>,
    initial_preset: i64,
    initial_red: f64,
    initial_green: f64,
    initial_blue: f64,
    initial_white: f64,
    chain_count: i64,
    chain_data: RefCell<Vec<u8>>,
    onoff: RefCell<String>,
    preset: Cell<i64>,
    brightness: Cell<i64>,
    intensity: Cell<i64>,
    speed: Cell<i64>,
    error_state: RefCell<Option<String>>,
    send_full_chain_data: Cell<bool>,
    transport: Transport,
}

impl Strip {
    fn new(name: &str, config: &ConfigHelper, strip_type: &str) -> Result<Rc<Self>, ConfigError> {
        let initial_preset = config.getint_or("initial_preset", -1)?;
        let initial_red = config.getfloat_or("initial_red", 0.5)?;
        let initial_green = config.getfloat_or("initial_green", 0.5)?;
        let initial_blue = config.getfloat_or("initial_blue", 0.5)?;
        let initial_white = config.getfloat_or("initial_white", 0.5)?;
        let chain_count = config.getint_or("chain_count", 1)?;
        let transport = if strip_type == "HTTP" {
            let addr = config.get("address")?;
            let port = config.getint_or("port", 80)?;
            let protocol = config.get_or("protocol", "http")?;
            let timeout = config.getfloat_or("timeout", 2.0)?;
            let client = server().get::<HttpClient>().ok_or_else(|| ConfigHelper::error("http_client"))?;
            Transport::Http { url: format!("{}://{}:{}/json", protocol, addr, port), timeout, client }
        } else {
            Transport::Serial {
                serial: AsyncSerial::from_config(config, 115200)?,
                enabled: Cell::new(true),
                task: Default::default(),
                pending: RefCell::new(None),
            }
        };
        Ok(Rc::new(Strip {
            name: name.to_string(),
            request_mutex: tokio::sync::Mutex::new(()),
            initial_preset,
            initial_red,
            initial_green,
            initial_blue,
            initial_white,
            chain_count,
            chain_data: RefCell::new(vec![0; chain_count.max(0) as usize * COLORSIZE]),
            onoff: RefCell::new("off".into()),
            preset: Cell::new(initial_preset),
            brightness: Cell::new(-1),
            intensity: Cell::new(-1),
            speed: Cell::new(-1),
            error_state: RefCell::new(None),
            send_full_chain_data: Cell::new(false),
            transport,
        }))
    }

    fn get_strip_info(&self) -> Value {
        json!({
            "strip": self.name,
            "status": *self.onoff.borrow(),
            "chain_count": self.chain_count,
            "preset": self.preset.get(),
            "brightness": self.brightness.get(),
            "intensity": self.intensity.get(),
            "speed": self.speed.get(),
            "error": *self.error_state.borrow(),
        })
    }

    async fn initialize(self: &Rc<Self>) -> Result<(), ServerError> {
        if let Transport::Serial { serial, task, .. } = &self.transport {
            if !task.is_set() {
                let s = self.clone();
                task.set(spawn(async move { s.run_serial().await }));
                for _ in 0..5 {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    if serial.connected() {
                        break;
                    }
                }
            }
        }
        let ret = self.send_wled_command(json!({"v": true})).await?;
        log::debug!("Initial WLED State for strip {}:\n{}", self.name, crate::pyfmt::py_repr(&ret));
        self.set_initial_state().await
    }

    async fn run_serial(self: Rc<Self>) {
        let (serial, enabled, pending) = match &self.transport {
            Transport::Serial { serial, enabled, pending, .. } => (serial.clone(), enabled, pending),
            _ => return,
        };
        let mut last_err = String::new();
        while enabled.get() {
            if let Err(e) = serial.open(true) {
                if e != last_err {
                    log::error!("WLED Serial Open Error\n{}", e);
                    last_err = e;
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
            while let Some(line) = serial.read_line().await {
                let text = String::from_utf8_lossy(&line).trim().to_string();
                match serde_json::from_str::<Value>(&text) {
                    Err(_) => log::debug!("Received Serial Data: {:?}", String::from_utf8_lossy(&line)),
                    Ok(v) => {
                        if let Some(tx) = pending.borrow_mut().take() {
                            let _ = tx.send(v);
                        }
                    }
                }
            }
            if enabled.get() {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            last_err.clear();
        }
    }

    async fn set_initial_state(&self) -> Result<(), ServerError> {
        self.send_full_chain_data.set(true);
        *self.onoff.borrow_mut() = "on".into();
        self.preset.set(self.initial_preset);
        self.brightness.set(255);
        self.intensity.set(-1);
        self.speed.set(-1);
        if self.initial_preset >= 0 {
            self.update_color_data(self.initial_red, self.initial_green, self.initial_blue, self.initial_white, None);
            Box::pin(self.wled_on(self.initial_preset)).await
        } else {
            self.set_wled(self.initial_red, self.initial_green, self.initial_blue, self.initial_white, None, true).await
        }
    }

    fn update_color_data(&self, r: f64, g: f64, b: f64, w: f64, index: Option<i64>) {
        let c = |v: f64| (v * 255.0 + 0.5) as i64 as u8;
        let led = [c(r), c(g), c(b), c(w)];
        let mut data = self.chain_data.borrow_mut();
        match index {
            None => {
                for chunk in data.chunks_mut(COLORSIZE) {
                    chunk.copy_from_slice(&led);
                }
            }
            Some(i) => {
                let start = ((i - 1) as usize) * COLORSIZE;
                if start + COLORSIZE <= data.len() {
                    data[start..start + COLORSIZE].copy_from_slice(&led);
                }
            }
        }
    }

    fn wled_pixel(&self, index: i64) -> Vec<u8> {
        let data = self.chain_data.borrow();
        let start = ((index - 1).max(0) as usize) * COLORSIZE;
        data.get(start..(start + COLORSIZE).min(data.len())).map(|s| s.to_vec()).unwrap_or_default()
    }

    async fn send_wled_command(&self, state: Value) -> Result<Value, ServerError> {
        *self.error_state.borrow_mut() = None;
        match self.send_wled_command_impl(state).await {
            Ok(v) => Ok(v),
            Err(e) => {
                let msg = format!("WLED: Error {}", e);
                *self.error_state.borrow_mut() = Some(msg.clone());
                log::error!("{}", msg);
                Err(ServerError::bad(msg))
            }
        }
    }

    async fn send_wled_command_impl(&self, state: Value) -> Result<Value, String> {
        let _g = self.request_mutex.lock().await;
        let want = state.get("v").and_then(|v| v.as_bool()).unwrap_or(false);
        match &self.transport {
            Transport::Http { url, timeout, client } => {
                log::debug!("WLED: url:{} json:{}", url, crate::pyfmt::py_repr(&state));
                let opts = RequestOpts { attempts: 3, connect_timeout: *timeout, request_timeout: *timeout, ..Default::default() };
                let resp = client.post(url, Body::Json(state), &[], opts).await;
                if !want {
                    return Ok(json!({}));
                }
                resp.json().map_err(|e| e.message)
            }
            Transport::Serial { serial, pending, .. } => {
                log::debug!("WLED: serial:{} json:{}", serial.port, crate::pyfmt::py_repr(&state));
                if want {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    *pending.borrow_mut() = Some(tx);
                    serial.send(state.to_string().into_bytes());
                    let r = tokio::time::timeout(std::time::Duration::from_secs(2), rx).await;
                    *pending.borrow_mut() = None;
                    match r {
                        Ok(Ok(v)) => Ok(v),
                        Ok(Err(_)) => Err("Server Shutdown".into()),
                        Err(_) => Err(String::new()),
                    }
                } else {
                    serial.send(state.to_string().into_bytes());
                    Ok(json!({}))
                }
            }
        }
    }

    async fn wled_on(&self, preset: i64) -> Result<(), ServerError> {
        *self.onoff.borrow_mut() = "on".into();
        log::debug!("WLED: {} on PRESET={}", self.name, preset);
        if preset < 0 {
            Box::pin(self.set_initial_state()).await
        } else {
            self.send_full_chain_data.set(true);
            self.preset.set(preset);
            self.brightness.set(-1);
            self.intensity.set(-1);
            self.speed.set(-1);
            self.send_wled_command(json!({"on": true, "ps": preset})).await.map(|_| ())
        }
    }

    async fn wled_off(&self) -> Result<(), ServerError> {
        log::debug!("WLED: {} off", self.name);
        *self.onoff.borrow_mut() = "off".into();
        self.send_full_chain_data.set(true);
        self.send_wled_command(json!({"on": false})).await.map(|_| ())
    }

    async fn wled_control(&self, brightness: i64, intensity: i64, speed: i64) -> Result<(), ServerError> {
        log::debug!(
            "WLED: {} control {} BRIGHTNESS={} INTENSITY={} SPEED={} CURRENTPRESET={}",
            self.name,
            self.onoff.borrow(),
            brightness,
            intensity,
            speed,
            self.preset.get()
        );
        if *self.onoff.borrow() == "off" {
            log::info!("wled control only permitted when strip is on");
            return Ok(());
        }
        let has_preset = self.preset.get() != -1;
        let mut control = if has_preset { json!({"tt": 0, "seg": {}}) } else { json!({"tt": 0}) };
        let mut should_send = false;
        if brightness > 0 {
            if brightness > 255 {
                log::info!("BRIGHTNESS should be between 1 and 255");
            } else {
                should_send = true;
                self.brightness.set(brightness);
                control["bri"] = json!(brightness);
                if has_preset {
                    control["seg"]["bri"] = json!(brightness);
                }
            }
        }
        if intensity > -1 && has_preset {
            if intensity > 255 {
                log::info!("INTENSITY should be between 0 and 255");
            } else {
                should_send = true;
                self.intensity.set(intensity);
                control["seg"]["ix"] = json!(intensity);
            }
        }
        if speed > -1 && has_preset {
            if speed > 255 {
                log::info!("SPEED should be between 0 and 255");
            } else {
                should_send = true;
                self.speed.set(speed);
                control["seg"]["sx"] = json!(speed);
            }
        }
        if should_send {
            self.send_wled_command(control).await?;
        }
        Ok(())
    }

    async fn set_wled(&self, r: f64, g: f64, b: f64, w: f64, index: Option<i64>, transmit: bool) -> Result<(), ServerError> {
        log::debug!(
            "WLED: {} R={} G={} B={} W={} INDEX={} TRANSMIT={}",
            self.name,
            crate::pyfmt::py_float_repr(r),
            crate::pyfmt::py_float_repr(g),
            crate::pyfmt::py_float_repr(b),
            crate::pyfmt::py_float_repr(w),
            index.map(|i| i.to_string()).unwrap_or("None".into()),
            crate::pyfmt::py_bool(transmit)
        );
        self.update_color_data(r, g, b, w, index);
        if !transmit {
            self.send_full_chain_data.set(true);
            return Ok(());
        }
        self.preset.set(-1);
        if self.brightness.get() == -1 {
            self.brightness.set(255);
        }
        let bri = self.brightness.get();
        let mut state = json!({"on": true, "tt": 0, "bri": bri, "seg": {"bri": bri, "i": []}});
        match index {
            None => {
                self.send_full_chain_data.set(false);
                state["seg"]["i"] = json!([0, self.chain_count, self.wled_pixel(1)]);
            }
            Some(_) if self.send_full_chain_data.get() => {
                self.send_full_chain_data.set(false);
                let cdata: Vec<Vec<u8>> = (0..self.chain_count).map(|i| self.wled_pixel(i + 1)).collect();
                state["seg"]["i"] = json!(cdata);
            }
            Some(i) => {
                state["seg"]["i"] = json!([i - 1, self.wled_pixel(i)]);
            }
        }
        self.send_wled_command(state.clone()).await?;
        if *self.onoff.borrow() == "off" {
            *self.onoff.borrow_mut() = "on".into();
            self.send_wled_command(state).await?;
        }
        Ok(())
    }

    async fn close(&self) {
        if let Transport::Serial { serial, enabled, task, pending } = &self.transport {
            enabled.set(false);
            serial.close().await;
            let t = task.take();
            if let Some(t) = t {
                let _ = t.await;
            }
            pending.borrow_mut().take();
        }
    }
}

pub struct Wled {
    strips: IndexMap<String, Rc<Strip>>,
}

impl Wled {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, ConfigError> {
        let srv = server();
        let sections = config.get_prefix_sections("wled");
        log::info!("WLED component loading strips: {}", crate::pyfmt::py_repr(&json!(sections)));
        let mut strips = IndexMap::new();
        for section in sections {
            let cfg = config.getsection(&section);
            let res: Result<(String, Rc<Strip>), ConfigError> = (|| {
                let n = cfg.get_name().to_string();
                let name = match n.split_once(char::is_whitespace) {
                    Some((_, rest)) if !rest.trim().is_empty() => rest.trim_start().to_string(),
                    _ => return Err(ConfigHelper::error(format!("Invalid Section Name: {}", n))),
                };
                log::info!("WLED strip: {}", name);
                let _ = cfg.get_value_or("color_order", json!(""), true)?;
                let strip_type = cfg.get_or("type", "http")?.to_uppercase();
                if strip_type != "HTTP" && strip_type != "SERIAL" {
                    return Err(ConfigHelper::error(format!("Unsupported Strip Type: {}", strip_type.to_lowercase())));
                }
                Ok((name.clone(), Strip::new(&name, &cfg, &strip_type)?))
            })();
            match res {
                Ok((n, s)) => {
                    strips.insert(n, s);
                }
                Err(e) => {
                    srv.add_warning(&format!("Failed to initialise strip [{}]\n{}", cfg.get_name(), e.0), None, true);
                }
            }
        }
        let w = Rc::new(Wled { strips });
        let ww = Rc::downgrade(&w);
        let _ = srv.register_remote_method(
            "set_wled_state",
            crate::components::klippy_connection::remote_method(move |args| {
                if let Some(w) = ww.upgrade() {
                    spawn(async move {
                        if let Err(e) = w.set_wled_state(args).await {
                            log::error!("Error running remote method: set_wled_state
{}", e.message);
                        }
                    });
                }
            }),
        );
        let ww = Rc::downgrade(&w);
        let _ = srv.register_remote_method(
            "set_wled",
            crate::components::klippy_connection::remote_method(move |args| {
                if let Some(w) = ww.upgrade() {
                    spawn(async move {
                        if let Err(e) = w.set_wled(args).await {
                            log::error!("Error running remote method: set_wled
{}", e.message);
                        }
                    });
                }
            }),
        );
        let ws = w.clone();
        let _ = srv.register_endpoint(
            "/machine/wled/strips",
            RequestType::GET,
            callback(move |_req| {
                let w = ws.clone();
                async move {
                    let strips: Map<String, Value> = w.strips.iter().map(|(k, s)| (k.clone(), s.get_strip_info())).collect();
                    Ok(json!({"strips": strips}))
                }
            }),
        );
        for ep in ["status", "on", "off", "toggle"] {
            let ws = w.clone();
            let rt = if ep == "status" { RequestType::GET } else { RequestType::POST };
            let _ = srv.register_endpoint(
                &format!("/machine/wled/{}", ep),
                rt,
                callback(move |req| {
                    let w = ws.clone();
                    async move { w.handle_batch(req).await }
                }),
            );
        }
        let ws = w.clone();
        let _ = srv.register_endpoint(
            "/machine/wled/strip",
            RequestType::GET | RequestType::POST,
            callback(move |req| {
                let w = ws.clone();
                async move { w.handle_single(req).await }
            }),
        );
        Ok(w)
    }

    /// WLED.wled_on(strip, preset)
    pub async fn wled_on(&self, strip: &str, preset: i64) -> Result<(), ServerError> {
        match self.strips.get(strip).cloned() {
            None => {
                log::info!("Unknown WLED strip: {}", strip);
                Ok(())
            }
            Some(s) => s.wled_on(preset).await,
        }
    }

    /// WLED.set_wled_state(strip, state, preset, brightness, intensity,
    /// speed), the arguments by name
    pub async fn set_wled_state(&self, args: Map<String, Value>) -> Result<(), ServerError> {
        let strip = match args.get("strip") {
            Some(s) => crate::pyfmt::py_str(s),
            None => return Ok(()),
        };
        let geti = |k: &str| args.get(k).and_then(|v| v.as_i64()).unwrap_or(-1);
        let (preset, brightness, intensity, speed) = (geti("preset"), geti("brightness"), geti("intensity"), geti("speed"));
        let status: Option<String> = match args.get("state") {
            Some(Value::Bool(b)) => Some(if *b { "on".into() } else { "off".into() }),
            Some(Value::String(s)) => {
                let s = s.to_lowercase();
                Some(match s.as_str() {
                    "true" => "on".into(),
                    "false" => "off".into(),
                    _ => s,
                })
            }
            _ => None,
        };
        if status.is_none() && preset == -1 && brightness == -1 && intensity == -1 && speed == -1 {
            log::info!("Invalid state received but no control or preset data passed");
            return Ok(());
        }
        let s = match self.strips.get(&strip) {
            None => {
                log::info!("Unknown WLED strip: {}", strip);
                return Ok(());
            }
            Some(s) => s.clone(),
        };
        if status.as_deref() == Some("off") {
            s.wled_off().await?;
        }
        if status.as_deref() == Some("on") || preset != -1 {
            s.wled_on(preset).await?;
        }
        if brightness != -1 || intensity != -1 || speed != -1 {
            s.wled_control(brightness, intensity, speed).await?;
        }
        Ok(())
    }

    /// WLED.set_wled(strip, red, green, blue, white, index, transmit), the
    /// arguments by name
    pub async fn set_wled(&self, args: Map<String, Value>) -> Result<(), ServerError> {
        let strip = match args.get("strip") {
            Some(s) => crate::pyfmt::py_str(s),
            None => return Ok(()),
        };
        let s = match self.strips.get(&strip) {
            None => {
                log::info!("Unknown WLED strip: {}", strip);
                return Ok(());
            }
            Some(s) => s.clone(),
        };
        let f = |k: &str| args.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
        let index = args.get("index").and_then(|v| v.as_i64()).filter(|i| *i >= 0);
        // `transmit == 1` (True == 1 in Python)
        let transmit = match args.get("transmit") {
            Some(Value::Bool(b)) => *b,
            Some(v) => v.as_f64() == Some(1.0),
            None => true,
        };
        s.set_wled(f("red"), f("green"), f("blue"), f("white"), index, transmit).await
    }

    async fn handle_single(&self, req: WebRequest) -> ApiResult {
        let strip_name = req.get_str("strip")?;
        let preset = req.get_int_or("preset", -1)?;
        let brightness = req.get_int_or("brightness", -1)?;
        let intensity = req.get_int_or("intensity", -1)?;
        let speed = req.get_int_or("speed", -1)?;
        let strip = self.strips.get(&strip_name).cloned().ok_or_else(|| ServerError::bad(format!("No valid strip named {}", strip_name)))?;
        if req.get_request_type() == RequestType::GET {
            return Ok(json!({strip_name: strip.get_strip_info()}));
        }
        let action = req.get_str("action")?.to_lowercase();
        if !["on", "off", "toggle", "control"].contains(&action.as_str()) {
            return Err(ServerError::bad(format!("Invalid requested action '{}'", action)));
        }
        let result = Self::process_request(&strip, &action, preset, brightness, intensity, speed).await?;
        Ok(json!({strip_name: result}))
    }

    async fn handle_batch(&self, req: WebRequest) -> ApiResult {
        let args = req.get_args().clone();
        if args.is_empty() {
            return Err(ServerError::bad("No arguments provided"));
        }
        let action = req.get_endpoint().rsplit('/').next().unwrap_or("").to_string();
        let mut result = Map::new();
        for name in args.keys() {
            match self.strips.get(name).cloned() {
                Some(s) => {
                    result.insert(name.clone(), Self::process_request(&s, &action, -1, -1, -1, -1).await?);
                }
                None => {
                    result.insert(name.clone(), json!({"error": "strip_not_found"}));
                }
            }
        }
        Ok(Value::Object(result))
    }

    async fn process_request(strip: &Rc<Strip>, req: &str, preset: i64, brightness: i64, intensity: i64, speed: i64) -> ApiResult {
        if req == "status" {
            return Ok(strip.get_strip_info());
        }
        let mut req = req.to_string();
        if req == "toggle" {
            req = if *strip.onoff.borrow() == "off" { "on".into() } else { "off".into() };
        }
        match req.as_str() {
            "on" | "control" => {
                if req == "on" {
                    strip.wled_on(preset).await?;
                }
                if brightness != -1 || intensity != -1 || speed != -1 {
                    strip.wled_control(brightness, intensity, speed).await?;
                }
                Ok(strip.get_strip_info())
            }
            "off" => {
                strip.wled_off().await?;
                Ok(strip.get_strip_info())
            }
            _ => Err(ServerError::bad(format!("Unsupported wled request: {}", req))),
        }
    }
}

#[async_trait(?Send)]
impl Component for Wled {
    fn has_init(&self) -> bool {
        true
    }

    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        log::debug!("Initializing wled");
        let endtime = crate::utils::monotonic() + 120.0;
        let mut query: Vec<Rc<Strip>> = self.strips.values().cloned().collect();
        let mut failed: Vec<Rc<Strip>>;
        loop {
            failed = Vec::new();
            for s in &query {
                // an error ends the init (Moonraker's try around the loop
                // logs it)
                if let Err(e) = s.initialize().await {
                    log::error!("{}", e.message);
                    return Ok(());
                }
                if s.error_state.borrow().is_some() {
                    failed.push(s.clone());
                }
            }
            if failed.is_empty() {
                log::debug!("All wled strips initialized");
                return Ok(());
            }
            query = failed.clone();
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if crate::utils::monotonic() >= endtime {
                break;
            }
        }
        let names: Vec<String> = failed.iter().map(|s| s.name.clone()).collect();
        server().add_warning(
            &format!("The following wled strips failed init: {}", crate::pyfmt::py_repr(&json!(names))),
            None,
            true,
        );
        Ok(())
    }

    async fn close(self: Rc<Self>) {
        for s in self.strips.values() {
            s.close().await;
        }
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<Wled>, ConfigError> {
    Wled::new(config)
}

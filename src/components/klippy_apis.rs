// Helper for Moonraker to Klippy API calls (port of moonraker/components/klippy_apis.py)

use crate::common::*;
use crate::confighelper::ConfigHelper;
use crate::server::{server, spawn, Component, EventObject};
use futures::future::LocalBoxFuture;
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::rc::{Rc, Weak};

pub const INFO_ENDPOINT: &str = "info";
pub const ESTOP_ENDPOINT: &str = "emergency_stop";
pub const LIST_EPS_ENDPOINT: &str = "list_endpoints";
pub const GC_OUTPUT_ENDPOINT: &str = "gcode/subscribe_output";
pub const GCODE_ENDPOINT: &str = "gcode/script";
pub const SUBSCRIPTION_ENDPOINT: &str = "objects/subscribe";
pub const STATUS_ENDPOINT: &str = "objects/query";
pub const OBJ_LIST_ENDPOINT: &str = "objects/list";
pub const REG_METHOD_ENDPOINT: &str = "register_remote_method";

pub type SubCallback = Rc<dyn Fn(Map<String, Value>, f64) -> Option<LocalBoxFuture<'static, ()>>>;

pub struct KlippyApi {
    pub version: Value,
    host_subscription: RefCell<Map<String, Value>>,
    subscription_callbacks: RefCell<Vec<SubCallback>>,
    me: RefCell<Weak<KlippyApi>>,
}

impl Component for KlippyApi {}

/// The host transport used for moonraker's own subscriptions
struct HostTransport(Weak<KlippyApi>);

impl ApiTransport for HostTransport {
    // Moonraker's KlippyAPI is its own transport
    fn py_object(self: Rc<Self>) -> Option<(Rc<dyn std::any::Any>, &'static str, &'static str)> {
        let api = self.0.upgrade()?;
        Some((api, "moonraker.components.klippy_apis", "KlippyAPI"))
    }
    fn send_status(&self, status: &Map<String, Value>, eventtime: f64) {
        if let Some(api) = self.0.upgrade() {
            api.send_status(status, eventtime);
        }
    }
    fn transport_id(&self) -> u64 {
        2
    }
}

macro_rules! gcode_endpoint {
    ($srv:expr, $path:expr, $api:expr, $body:expr) => {{
        let a = $api.clone();
        let _ = $srv.register_endpoint(
            $path,
            RequestType::POST,
            callback(move |req: WebRequest| {
                let a = a.clone();
                async move { $body(a, req).await }
            }),
        );
    }};
}

impl KlippyApi {
    pub fn new(_config: &ConfigHelper) -> Rc<Self> {
        let srv = server();
        let api = Rc::new(KlippyApi {
            version: srv.get_app_arg("software_version").unwrap_or(Value::Null),
            host_subscription: RefCell::new(Map::new()),
            subscription_callbacks: RefCell::new(Vec::new()),
            me: RefCell::new(Weak::new()),
        });
        *api.me.borrow_mut() = Rc::downgrade(&api);
        gcode_endpoint!(srv, "/printer/print/pause", api, |a: Rc<KlippyApi>, _r: WebRequest| async move { a.pause_print().await });
        gcode_endpoint!(srv, "/printer/print/resume", api, |a: Rc<KlippyApi>, _r: WebRequest| async move { a.resume_print().await });
        gcode_endpoint!(srv, "/printer/print/cancel", api, |a: Rc<KlippyApi>, _r: WebRequest| async move { a.cancel_print().await });
        gcode_endpoint!(srv, "/printer/print/start", api, |a: Rc<KlippyApi>, r: WebRequest| async move {
            let filename = r.get_str("filename")?;
            let user = r.get_current_user().cloned();
            a.start_print(&filename, false, user).await
        });
        gcode_endpoint!(srv, "/printer/restart", api, |a: Rc<KlippyApi>, _r: WebRequest| async move { a.do_restart("RESTART", false).await });
        gcode_endpoint!(srv, "/printer/firmware_restart", api, |a: Rc<KlippyApi>, _r: WebRequest| async move {
            a.do_restart("FIRMWARE_RESTART", false).await
        });
        let a = Rc::downgrade(&api);
        srv.on_event("server:klippy_disconnect", move |_| {
            if let Some(a) = a.upgrade() {
                a.host_subscription.borrow_mut().clear();
                a.subscription_callbacks.borrow_mut().clear();
            }
        });
        api
    }

    fn host_transport(&self) -> Rc<dyn ApiTransport> {
        Rc::new(HostTransport(self.me.borrow().clone()))
    }

    pub async fn send_klippy_request(&self, method: &str, params: Value, transport: Option<Rc<dyn ApiTransport>>) -> ApiResult {
        let args = match params {
            Value::Object(o) => o,
            _ => Map::new(),
        };
        let req = WebRequest::new(method, args, RequestType::empty(), Some(transport.unwrap_or_else(|| self.host_transport())), None, None);
        server().klippy().request(req).await
    }

    pub async fn run_gcode(&self, script: &str) -> ApiResult {
        self.send_klippy_request(GCODE_ENDPOINT, json!({"script": script}), None).await
    }

    pub async fn start_print(&self, filename: &str, wait_klippy_started: bool, user: Option<UserInfo>) -> ApiResult {
        let mut filename = filename.to_string();
        if filename.starts_with('/') {
            filename = filename[1..].to_string();
        }
        let filename = filename.replace('"', "\\\"");
        let script = format!("SDCARD_PRINT_FILE FILENAME=\"{}\"", filename);
        if wait_klippy_started {
            server().klippy().wait_started(20.0).await;
        }
        log::info!("Requesting Job Start, filename = {}", filename);
        let ret = self.run_gcode(&script).await?;
        let uval = user.as_ref().map(|u| u.as_dict()).unwrap_or(Value::Null);
        let _ = server().send_event_object("klippy_apis:job_start_complete", vec![uval], EventObject::User(user));
        Ok(ret)
    }

    pub async fn pause_print(&self) -> ApiResult {
        let _ = server().send_event("klippy_apis:pause_requested", vec![]);
        log::info!("Requesting job pause...");
        self.send_klippy_request("pause_resume/pause", json!({}), None).await
    }

    pub async fn resume_print(&self) -> ApiResult {
        let _ = server().send_event("klippy_apis:resume_requested", vec![]);
        log::info!("Requesting job resume...");
        self.send_klippy_request("pause_resume/resume", json!({}), None).await
    }

    pub async fn cancel_print(&self) -> ApiResult {
        let _ = server().send_event("klippy_apis:cancel_requested", vec![]);
        log::info!("Requesting job cancel...");
        self.send_klippy_request("pause_resume/cancel", json!({}), None).await
    }

    pub async fn do_restart(&self, gc: &str, wait_klippy_started: bool) -> ApiResult {
        if wait_klippy_started {
            server().klippy().wait_started(20.0).await;
        }
        match self.run_gcode(gc).await {
            Ok(r) => Ok(r),
            Err(e) if e.message == "Klippy Disconnected" => Ok(json!("ok")),
            Err(e) => Err(e),
        }
    }

    pub async fn list_endpoints(&self) -> ApiResult {
        self.send_klippy_request(LIST_EPS_ENDPOINT, json!({}), None).await
    }

    pub async fn emergency_stop(&self) -> ApiResult {
        self.send_klippy_request(ESTOP_ENDPOINT, json!({}), None).await
    }

    pub async fn get_klippy_info(&self, send_id: bool) -> ApiResult {
        let params = if send_id {
            json!({"client_info": {"program": "Moonraker", "version": self.version}})
        } else {
            json!({})
        };
        self.send_klippy_request(INFO_ENDPOINT, params, None).await
    }

    pub async fn get_object_list(&self) -> Result<Vec<Value>, ServerError> {
        let result = self.send_klippy_request(OBJ_LIST_ENDPOINT, json!({}), None).await?;
        match result.get("objects").and_then(|o| o.as_array()) {
            Some(a) => Ok(a.clone()),
            None => Err(ServerError::new("Invalid response received from Klippy", 500)),
        }
    }

    pub async fn query_objects(&self, objects: &Map<String, Value>) -> Result<Map<String, Value>, ServerError> {
        let result = self.send_klippy_request(STATUS_ENDPOINT, json!({"objects": objects}), None).await?;
        match result.get("status").and_then(|o| o.as_object()) {
            Some(s) => Ok(s.clone()),
            None => Err(ServerError::new("Invalid response received from Klippy", 500)),
        }
    }

    pub async fn subscribe_objects(&self, objects: &Map<String, Value>, cb: Option<SubCallback>) -> Result<Map<String, Value>, ServerError> {
        {
            let mut hs = self.host_subscription.borrow_mut();
            for (obj, items) in objects {
                match hs.get(obj).cloned() {
                    Some(prev) => {
                        if items.is_null() || prev.is_null() {
                            hs.insert(obj.clone(), Value::Null);
                        } else {
                            let mut u: Vec<Value> = prev.as_array().cloned().unwrap_or_default();
                            for i in items.as_array().cloned().unwrap_or_default() {
                                if !u.contains(&i) {
                                    u.push(i);
                                }
                            }
                            hs.insert(obj.clone(), Value::Array(u));
                        }
                    }
                    None => {
                        hs.insert(obj.clone(), items.clone());
                    }
                }
            }
        }
        let params = json!({"objects": Value::Object(self.host_subscription.borrow().clone())});
        let result = self.send_klippy_request(SUBSCRIPTION_ENDPOINT, params, None).await?;
        match result.get("status").and_then(|o| o.as_object()) {
            Some(s) => {
                if let Some(cb) = cb {
                    self.subscription_callbacks.borrow_mut().push(cb);
                }
                Ok(s.clone())
            }
            None => Err(ServerError::new("Invalid response received from Klippy", 500)),
        }
    }

    pub async fn subscribe_from_transport(
        &self,
        objects: &Map<String, Value>,
        transport: Rc<dyn ApiTransport>,
        full_response: bool,
    ) -> ApiResult {
        let result = self.send_klippy_request(SUBSCRIPTION_ENDPOINT, json!({"objects": objects}), Some(transport)).await?;
        match result.get("status") {
            Some(s) if result.is_object() => {
                if full_response {
                    Ok(result.clone())
                } else {
                    Ok(s.clone())
                }
            }
            _ => Err(ServerError::new("Invalid response received from Klippy", 500)),
        }
    }

    pub async fn subscribe_gcode_output(&self) -> ApiResult {
        self.send_klippy_request(GC_OUTPUT_ENDPOINT, json!({"response_template": {"method": "process_gcode_response"}}), None)
            .await
    }

    pub async fn register_method(&self, method_name: &str) -> ApiResult {
        self.send_klippy_request(
            REG_METHOD_ENDPOINT,
            json!({"response_template": {"method": method_name}, "remote_method": method_name}),
            None,
        )
        .await
    }

    pub fn send_status(&self, status: &Map<String, Value>, eventtime: f64) {
        // deferred like moonraker's register_callback, one task for all
        let cbs: Vec<SubCallback> = self.subscription_callbacks.borrow().clone();
        if !cbs.is_empty() {
            let st = status.clone();
            spawn(async move {
                for cb in cbs {
                    if let Some(f) = cb(st.clone(), eventtime) {
                        spawn(f);
                    }
                }
            });
        }
        let _ = server().send_event("server:status_update", vec![Value::Object(status.clone())]);
    }
}

pub fn load_component(config: &ConfigHelper) -> Rc<KlippyApi> {
    KlippyApi::new(config)
}

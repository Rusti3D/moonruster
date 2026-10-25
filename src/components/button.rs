// Support for GPIO Button actions (port of moonraker/components/button.py)

use crate::common::*;
use crate::components::gpio::{getgpioevent, GpioEvent};
use crate::components::template::{AsyncFn, JinjaTemplate};
use crate::confighelper::{Bounds, ConfigError, ConfigHelper};
use crate::server::{server, spawn, Component};
use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

pub struct GpioButton {
    name: String,
    mutex: tokio::sync::Mutex<()>,
    gpio_event: RefCell<Option<Rc<GpioEvent>>>,
    min_event_time: Cell<f64>,
    press_template: RefCell<Option<JinjaTemplate>>,
    release_template: RefCell<Option<JinjaTemplate>>,
    notification_sent: Cell<bool>,
    user_data: std::sync::Arc<crate::components::template::SharedDict>,
    event: RefCell<Value>,
}

impl GpioButton {
    fn new(config: &ConfigHelper) -> Result<Rc<Self>, ConfigError> {
        let name = config.get_name().split_whitespace().last().unwrap_or("").to_string();
        let btn = Rc::new(GpioButton {
            name,
            mutex: tokio::sync::Mutex::new(()),
            gpio_event: RefCell::new(None),
            min_event_time: Cell::new(0.0),
            press_template: RefCell::new(None),
            release_template: RefCell::new(None),
            notification_sent: Cell::new(false),
            user_data: std::sync::Arc::new(Default::default()),
            event: RefCell::new(json!({"elapsed_time": 0.0, "received_time": 0.0, "render_time": 0.0, "pressed": false})),
        });
        let w = Rc::downgrade(&btn);
        let cb = Rc::new(move |eventtime: f64, elapsed: f64, pressed: i32| {
            if let Some(b) = w.upgrade() {
                spawn(async move { b.on_gpio_event(eventtime, elapsed, pressed).await });
            }
        });
        let gpio_event = getgpioevent(config, "pin", cb)?;
        btn.min_event_time.set(config.getfloat_b("minimum_event_time", Some(0.0), Bounds::min(0.0))?);
        let debounce = config.getfloat_b("debounce_period", Some(0.05), Bounds::min(0.01))?;
        let name = btn.name.clone();
        gpio_event.setup_debounce(
            debounce,
            Some(Rc::new(move |msg: String| {
                server().add_warning(&format!("Button {}: {}", name, msg), None, true);
            })),
        );
        *btn.gpio_event.borrow_mut() = Some(gpio_event);
        *btn.press_template.borrow_mut() = config.gettemplate_opt("on_press", Some(None), true)?;
        *btn.release_template.borrow_mut() = config.gettemplate_opt("on_release", Some(None), true)?;
        if btn.press_template.borrow().is_none() && btn.release_template.borrow().is_none() {
            return Err(ConfigHelper::error(format!("[{}]: No template option configured", config.get_name())));
        }
        Ok(btn)
    }

    fn initialize(&self) {
        if let Some(e) = self.gpio_event.borrow().as_ref() {
            e.start();
            self.event.borrow_mut()["pressed"] = json!(e.get_value() != 0);
        }
    }

    fn get_status(&self) -> Value {
        json!({"name": self.name, "type": "gpio", "event": self.event.borrow().clone()})
    }

    fn send_notification(&self, result: Value) {
        if self.notification_sent.get() {
            return;
        }
        self.notification_sent.set(true);
        let mut data = self.get_status();
        data["aux"] = result;
        server().send_event("button:button_event", vec![data]);
    }

    async fn on_gpio_event(self: Rc<Self>, eventtime: f64, elapsed_time: f64, pressed: i32) {
        if elapsed_time < self.min_event_time.get() {
            return;
        }
        let template = if pressed != 0 { self.press_template.borrow().clone() } else { self.release_template.borrow().clone() };
        let template = match template {
            None => return,
            Some(t) => t,
        };
        let _g = self.mutex.lock().await;
        self.notification_sent.set(false);
        *self.event.borrow_mut() = json!({
            "elapsed_time": elapsed_time,
            "received_time": eventtime,
            "render_time": crate::utils::monotonic(),
            "pressed": pressed != 0,
        });
        let ctx = json!({"event": self.event.borrow().clone()});
        let ud = self.user_data.clone();
        let user_data: crate::components::template::ObjBuilder = Box::new(move |_| minijinja::Value::from_dyn_object(ud));
        let call_method: AsyncFn = Rc::new(|args: Vec<Value>, kwargs: Map<String, Value>| {
            Box::pin(async move {
                let method = args.first().map(crate::pyfmt::py_str).ok_or_else(|| ServerError::bad("missing method name"))?;
                let req_args = match args.get(1) {
                    Some(Value::Object(m)) if !m.is_empty() => m.clone(),
                    _ => kwargs,
                };
                let it = crate::components::application::InternalTransport;
                it.call_method(&method, req_args.into_iter().collect()).await
            })
        });
        let w = Rc::downgrade(&self);
        let send_notification: AsyncFn = Rc::new(move |args: Vec<Value>, _kw: Map<String, Value>| {
            let w = w.clone();
            Box::pin(async move {
                if let Some(b) = w.upgrade() {
                    b.send_notification(args.into_iter().next().unwrap_or(Value::Null));
                }
                Ok(Value::Null)
            })
        });
        let res = template
            .render_async_ext(&ctx, vec![("call_method", call_method), ("send_notification", send_notification)], vec![("user_data", user_data)])
            .await;
        if let Err(e) = res {
            let action = if pressed != 0 { "on_press" } else { "on_release" };
            log::error!("Button {}: '{}' template error\n{}", self.name, action, e.message);
        }
    }
}

pub struct ButtonManager {
    buttons: IndexMap<String, Rc<GpioButton>>,
}

impl ButtonManager {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, ConfigError> {
        let srv = server();
        let sections = config.get_prefix_sections("button");
        log::info!("Loading Buttons: {}", crate::pyfmt::py_repr(&json!(sections)));
        let mut buttons = IndexMap::new();
        for section in sections {
            let cfg = config.getsection(&section);
            let _btn_type = cfg.get_or("type", "gpio")?;
            match GpioButton::new(&cfg) {
                Ok(b) => {
                    buttons.insert(b.name.clone(), b);
                }
                Err(e) => {
                    srv.add_warning(&format!("Failed to load button [{}]\n{}", cfg.get_name(), e.0), None, true);
                }
            }
        }
        srv.register_notification("button:button_event", None);
        Ok(Rc::new(ButtonManager { buttons }))
    }
}

#[async_trait(?Send)]
impl Component for ButtonManager {
    fn has_init(&self) -> bool {
        true
    }
    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        for b in self.buttons.values() {
            b.initialize();
        }
        Ok(())
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<ButtonManager>, ConfigError> {
    ButtonManager::new(config)
}

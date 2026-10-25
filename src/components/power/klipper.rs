// The klipper_device type

use super::*;

impl PowerDevice {
    pub(super) async fn klipper_handle_ready(self: Rc<Self>) {
        let (object_name, unit) = match &self.kind {
            Kind::Klipper { object_name, .. } => (object_name.clone(), server().klippy().unit_name()),
            _ => return,
        };
        if !unit.is_empty() && self.bound_services.borrow().contains(&unit) {
            server().add_warning(
                &format!(
                    "Power: Klipper Device {} contains invalid bound service {}.  Remove this service from the configuration and restart Moonraker.",
                    self.name, unit
                ),
                Some(&format!("kpd_{}", self.name)),
                true,
            );
            self.bound_services.borrow_mut().retain(|s| *s != unit);
        }
        let w = Rc::downgrade(&self);
        let cb: crate::components::klippy_apis::SubCallback = Rc::new(move |status, _| {
            if let Some(d) = w.upgrade() {
                d.klipper_set_state_from_data(&status);
            }
            None
        });
        let mut sub = Map::new();
        sub.insert(object_name, Value::Null);
        let data = server().klippy_apis().subscribe_objects(&sub, Some(cb)).await.ok();
        if !self.klipper_validate(data.as_ref()) {
            self.set_state_val("error");
        } else {
            let _g = self.request_lock.lock().await;
            self.klipper_set_state_from_data(data.as_ref().unwrap());
            self.set_initial_state().await;
            self.notify_power_changed();
        }
    }

    pub(super) fn klipper_validate(&self, data: Option<&Map<String, Value>>) -> bool {
        let object_name = match &self.kind {
            Kind::Klipper { object_name, .. } => object_name,
            _ => return false,
        };
        match data {
            None => log::info!("Error querying klipper object: {}", object_name),
            Some(d) => match d.get(object_name) {
                None => log::info!("[power]: Invalid Klipper Device {}, no response returned from subscription.", object_name),
                Some(o) if o.get("value").is_none() => log::info!(
                    "[power]: Invalid Klipper Device {}, response does not contain a 'value' parameter",
                    object_name
                ),
                Some(_) => return true,
            },
        }
        false
    }

    pub(super) fn klipper_set_state_from_data(self: &Rc<Self>, data: &Map<String, Value>) {
        let (object_name, update_tx) = match &self.kind {
            Kind::Klipper { object_name, update_tx, .. } => (object_name, update_tx),
            _ => return,
        };
        let value = match data.get(object_name).and_then(|o| o.get("value")) {
            None | Some(Value::Null) => return,
            Some(v) => v.clone(),
        };
        let state = if crate::pyfmt::py_truthy(&value) { "on" } else { "off" };
        self.klipper_set_state(state);
        if let Some(tx) = update_tx.borrow_mut().take() {
            let _ = tx.send(state.to_string());
        }
    }

    pub(super) fn klipper_set_state(self: &Rc<Self>, state: &str) {
        let in_request = self.request_lock.try_lock().is_err();
        let last_state = self.state();
        self.set_state_val(state);
        self.last_update_time.set(loop_time());
        if last_state != state && last_state != "init" && !in_request {
            spawn(self.clone().notify_external_change());
        }
    }
}

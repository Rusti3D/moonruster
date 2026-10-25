// Klipper status

use super::*;

impl MqttClient {
    pub(super) async fn handle_klippy_started(self: Rc<Self>) {
        if self.status_objs.is_empty() {
            return;
        }
        let kapi = server().klippy_apis();
        let transport: Rc<dyn ApiTransport> = self.clone();
        if let Ok(result) = kapi.subscribe_from_transport(&self.status_objs, transport, true).await {
            let status = result.get("status").and_then(|v| v.as_object()).cloned().unwrap_or_default();
            let eventtime = result.get("eventtime").and_then(|v| v.as_f64()).unwrap_or(0.0);
            ApiTransport::send_status(&*self, &status, eventtime);
        }
        if self.status_interval > 0.0 {
            let c = Rc::downgrade(&self);
            let interval = self.status_interval;
            let h = spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs_f64(interval)).await;
                    match c.upgrade() {
                        Some(c) => c.handle_timed_status_update(),
                        None => break,
                    }
                }
            });
            self.status_timer.replace(h);
        }
    }

    pub(super) fn handle_klippy_disconnect(&self) {
        self.status_timer.cancel();
        self.handle_timed_status_update();
    }

    pub(super) fn handle_timed_status_update(&self) {
        let payload = std::mem::take(&mut *self.status_cache.borrow_mut());
        if !payload.is_empty() {
            self.publish_status_update(&payload, self.last_status_time.get());
        }
    }

    pub(super) fn publish_status_update(&self, status: &Map<String, Value>, eventtime: f64) {
        if self.publish_split_status {
            for (objkey, objval) in status {
                if let Value::Object(o) = objval {
                    for (statekey, v) in o {
                        let payload = json!({"eventtime": eventtime, "value": v});
                        self.publish_topic(&format!("{}/{}/{}", self.klipper_state_prefix, objkey, statekey), payload, None, true);
                    }
                }
            }
        } else {
            let payload = json!({"eventtime": eventtime, "status": status});
            self.publish_topic(&self.klipper_status_topic, payload, None, false);
        }
    }

    pub(super) async fn publish_from_klipper(self: Rc<Self>, args: Map<String, Value>) {
        let mut topic = match args.get("topic") {
            Some(t) => crate::pyfmt::py_str(t),
            None => {
                log::error!("publish_mqtt_topic() missing 1 required positional argument: 'topic'");
                return;
            }
        };
        let payload = args.get("payload").cloned().unwrap_or(Value::Null);
        let qos = args.get("qos").and_then(|v| v.as_u64()).map(|q| q as u8);
        let retain = args.get("retain").map(crate::pyfmt::py_truthy).unwrap_or(false);
        let use_prefix = args.get("use_prefix").map(crate::pyfmt::py_truthy).unwrap_or(false);
        if use_prefix {
            topic = format!("{}/{}", self.instance_name, topic.trim_start_matches('/'));
        }
        if let Err(e) = self.publish_topic(&topic, payload, qos, retain).await {
            log::error!("Error publishing MQTT topic from Klipper: {}", e.message);
        }
    }
}

// The mqtt device type

use super::*;

impl PowerDevice {
    pub(super) fn mqtt_on_state_update(self: &Rc<Self>, payload: Vec<u8>) {
        let m = match &self.kind {
            Kind::Mqtt(m) => m,
            _ => return,
        };
        let last_state = self.state();
        let in_request = self.request_lock.try_lock().is_err();
        m.response_count.set(m.response_count.get() + 1);
        let text = String::from_utf8_lossy(&payload).to_string();
        let ctx = json!({"payload": text, "last_request": *m.last_request.borrow(), "response_count": m.response_count.get()});
        let mut error: Option<ServerError> = None;
        let mut response = String::new();
        match m.state_response.render(&ctx) {
            Err(e) => {
                error = Some(e);
                self.set_state_val("error");
            }
            Ok(r) => {
                response = r.to_lowercase();
                if response == "discard" {
                    return;
                } else if response != "on" && response != "off" {
                    let err_msg = format!("Invalid State Received. Raw Payload: '{}', Rendered: '{}", text, response);
                    log::info!("MQTT Power Device {}: {}", self.name, err_msg);
                    error = Some(ServerError::new(err_msg, 500));
                    self.set_state_val("error");
                } else {
                    self.set_state_val(&response);
                }
            }
        }
        self.last_update_time.set(loop_time());
        if !in_request && last_state != self.state() {
            log::info!("MQTT Power Device {}: External Power event detected, new state: {}", self.name, self.state());
            spawn(self.clone().notify_external_change());
        }
        if let Some(tx) = m.query_response.borrow_mut().take() {
            let _ = tx.send(match error {
                Some(e) => Err(e),
                None => Ok(response),
            });
        }
    }

    pub(super) async fn mqtt_wait_for_update(
        &self,
        m: &MqttDev,
        rx: tokio::sync::oneshot::Receiver<Result<String, ServerError>>,
        do_query: bool,
    ) -> Result<String, ServerError> {
        if let (Some(topic), true) = (&m.query_topic, do_query) {
            let payload = match &m.query_payload {
                Some(t) => Value::String(t.render(&json!({}))?),
                None => Value::Null,
            };
            m.mqtt.publish_topic(topic, payload, m.qos, false).await?;
        }
        match tokio::time::timeout(Duration::from_secs_f64(m.state_timeout), rx).await {
            Err(_) => Err(ServerError::new("TimeoutError", -2)),
            Ok(Err(_)) => Err(ServerError::new("cancelled", 500)),
            Ok(Ok(r)) => r,
        }
    }

    pub(super) async fn mqtt_on_connected(self: Rc<Self>) {
        let m = match &self.kind {
            Kind::Mqtt(m) => m,
            _ => return,
        };
        let _g = self.request_lock.lock().await;
        let st = self.state();
        if st == "on" || st == "off" {
            return;
        }
        self.set_state_val("init");
        let mut success = false;
        while m.mqtt.is_connected() {
            let (tx, rx) = tokio::sync::oneshot::channel();
            *m.query_response.borrow_mut() = Some(tx);
            match self.mqtt_wait_for_update(m, rx, true).await {
                Err(e) if e.status_code == -2 => {
                    if m.query_topic.is_none() {
                        log::info!("MQTT Power Device {}: Initialization Timed Out", self.name);
                        break;
                    }
                }
                Err(e) => {
                    log::error!("MQTT Power Device {}: Init Failed\n{}", self.name, e.message);
                    break;
                }
                Ok(_) => {
                    success = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        *m.query_response.borrow_mut() = None;
        if !success {
            self.set_state_val("error");
        } else {
            log::info!("MQTT Power Device {} initialized", self.name);
        }
        self.set_initial_state().await;
        self.initial_state.set(None);
        self.notify_power_changed();
    }

    pub(super) async fn mqtt_on_disconnected(self: Rc<Self>) {
        let m = match &self.kind {
            Kind::Mqtt(m) => m,
            _ => return,
        };
        if let Some(tx) = m.query_response.borrow_mut().take() {
            let _ = tx.send(Err(ServerError::new("MQTT Disconnected", 503)));
        }
        let _g = self.request_lock.lock().await;
        self.set_state_val("error");
        self.notify_power_changed();
    }

    pub(super) async fn mqtt_refresh(&self, m: &MqttDev) -> Result<(), ServerError> {
        let st = self.state();
        if m.query_topic.is_some() && (m.must_query || (st != "on" && st != "off")) {
            if !m.mqtt.is_connected() {
                return Err(ServerError::new(format!("MQTT Power Device {}: MQTT Not Connected", self.name), 503));
            }
            *m.last_request.borrow_mut() = "refresh".into();
            m.response_count.set(0);
            let (tx, rx) = tokio::sync::oneshot::channel();
            *m.query_response.borrow_mut() = Some(tx);
            if let Err(e) = self.mqtt_wait_for_update(m, rx, true).await {
                log::error!("MQTT Power Device {}: Failed to refresh state\n{}", self.name, e.message);
                self.set_state_val("error");
            }
            *m.query_response.borrow_mut() = None;
            m.last_request.borrow_mut().clear();
            m.response_count.set(0);
        }
        Ok(())
    }

    pub(super) async fn mqtt_set_power(&self, m: &MqttDev, state: &str) -> Result<(), ServerError> {
        if !m.mqtt.is_connected() {
            return Err(ServerError::new(format!("MQTT Power Device {}: MQTT Not Connected", self.name), 503));
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        *m.query_response.borrow_mut() = Some(tx);
        *m.last_request.borrow_mut() = format!("power_{}", state);
        m.response_count.set(0);
        let res: Result<String, ServerError> = async {
            let payload = m.cmd_payload.render(&json!({"command": state}))?;
            m.mqtt.publish_topic(&m.cmd_topic, Value::String(payload), m.qos, m.retain_cmd_state).await?;
            self.mqtt_wait_for_update(m, rx, m.must_query).await
        }
        .await;
        let new_state = match res {
            Ok(s) => s,
            Err(e) => {
                log::error!("MQTT Power Device {}: Failed to set state\n{}", self.name, e.message);
                "error".into()
            }
        };
        *m.query_response.borrow_mut() = None;
        m.last_request.borrow_mut().clear();
        m.response_count.set(0);
        self.set_state_val(&new_state);
        if new_state == "error" {
            return Err(ServerError::new(format!("MQTT Power Device {}: Failed to set device to state '{}'", self.name, state), 500));
        }
        Ok(())
    }
}

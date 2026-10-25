// Subscriptions and publishing

use super::*;

impl MqttClient {
    pub(super) fn subscribe_packet(&self, mid: u16, subs: &[(String, u8)]) -> Vec<u8> {
        let mut body = mid.to_be_bytes().to_vec();
        if self.protocol == 5 {
            body.push(0);
        }
        for (t, q) in subs {
            put_str(&mut body, t.as_bytes());
            body.push(*q);
        }
        packet(0x82, body)
    }

    pub fn subscribe_topic(&self, topic: &str, callback: MqttCallback, qos: Option<u8>) -> Result<Rc<SubscriptionHandle>, ServerError> {
        if topic.contains('#') || topic.contains('+') {
            return Err(ServerError::bad("Wildcards may not be used"));
        }
        let mut qos = match qos {
            Some(q) if q != 0 => q,
            _ => self.qos,
        };
        if qos > 2 {
            return Err(ServerError::bad("QOS must be between 0 and 2"));
        }
        let hdl = Rc::new(SubscriptionHandle { topic: topic.to_string(), callback });
        let mut need_sub = true;
        {
            let mut subs = self.subscribed_topics.borrow_mut();
            match subs.get_mut(topic) {
                Some((prev_qos, hdls)) => {
                    let prev = *prev_qos;
                    qos = qos.max(prev);
                    hdls.push(hdl.clone());
                    need_sub = qos != prev;
                    *prev_qos = qos;
                }
                None => {
                    subs.insert(topic.to_string(), (qos, vec![hdl.clone()]));
                }
            }
        }
        if self.is_connected() && need_sub {
            let mid = self.next_mid();
            self.send_raw(self.subscribe_packet(mid, &[(topic.to_string(), qos)]));
            self.pending_acks.borrow_mut().insert(mid, PendingAck::Subscribe(vec![topic.to_string()]));
        }
        Ok(hdl)
    }

    pub fn unsubscribe(&self, hdl: &Rc<SubscriptionHandle>) {
        let topic = hdl.topic.clone();
        let empty = {
            let mut subs = self.subscribed_topics.borrow_mut();
            match subs.get_mut(&topic) {
                Some((_, hdls)) => {
                    hdls.retain(|h| !Rc::ptr_eq(h, hdl));
                    hdls.is_empty()
                }
                None => return,
            }
        };
        if empty {
            self.subscribed_topics.borrow_mut().shift_remove(&topic);
            let mid = self.next_mid();
            let mut body = mid.to_be_bytes().to_vec();
            if self.protocol == 5 {
                body.push(0);
            }
            put_str(&mut body, topic.as_bytes());
            if self.send_raw(packet(0xA2, body)) {
                self.pending_acks.borrow_mut().insert(mid, PendingAck::Unsubscribe(vec![topic]));
            }
        }
    }

    /// Queues a publish immediately, the returned future resolves when the
    /// broker acknowledges it (immediately for qos 0)
    pub fn publish_topic(
        &self,
        topic: &str,
        payload: Value,
        qos: Option<u8>,
        retain: bool,
    ) -> Published {
        let qos = match qos {
            Some(q) if q != 0 => q,
            _ => self.qos,
        };
        if qos > 2 {
            return Completion(Box::pin(async { Err(ServerError::bad("QOS must be between 0 and 2")) }));
        }
        let data: Vec<u8> = match &payload {
            Value::Object(_) | Value::Array(_) => payload.to_string().into_bytes(),
            Value::Bool(b) => b.to_string().into_bytes(),
            Value::Null => Vec::new(),
            Value::String(s) => s.clone().into_bytes(),
            Value::Number(_) => crate::pyfmt::py_str(&payload).into_bytes(),
        };
        self.publish_bytes(topic, data, Some(qos), retain)
    }

    /// publish_topic of a payload already encoded (Python components encode
    /// it as Moonraker does, json_wrapper for dicts and lists)
    pub fn publish_bytes(
        &self,
        topic: &str,
        data: Vec<u8>,
        qos: Option<u8>,
        retain: bool,
    ) -> Published {
        let qos = match qos {
            Some(q) if q != 0 => q,
            _ => self.qos,
        };
        if qos > 2 {
            return Completion(Box::pin(async { Err(ServerError::bad("QOS must be between 0 and 2")) }));
        }
        if topic.is_empty() || topic.contains('+') || topic.contains('#') || data.len() > 268435455 {
            return Completion(Box::pin(async { Err(ServerError::new("MQTT Message Queue Full", 529)) }));
        }
        let mut body = Vec::new();
        put_str(&mut body, topic.as_bytes());
        let mid = if qos > 0 { self.next_mid() } else { 0 };
        if qos > 0 {
            body.extend_from_slice(&mid.to_be_bytes());
        }
        if self.protocol == 5 {
            body.push(0);
        }
        body.extend(data);
        let pkt = packet(0x30 | (qos << 1) | retain as u8, body);
        if qos == 0 {
            // No delivery guarantee for qos 0, resolve immediately
            self.send_raw(pkt);
            return Completion(Box::pin(async { Ok(()) }));
        }
        let (tx, rx) = oneshot::channel();
        self.pending_acks.borrow_mut().insert(mid, PendingAck::Publish(tx));
        self.inflight.borrow_mut().insert(mid, pkt.clone());
        self.send_raw(pkt);
        Completion(Box::pin(async move { rx.await.unwrap_or_else(|_| Err(ServerError::new("Moonraker Shutdown", 503))) }))
    }

    pub(super) async fn handle_publish_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let topic = req.get_str("topic")?;
        let payload = req.get_opt("payload").unwrap_or(Value::Null);
        let qos = req.get_int_or("qos", self.qos as i64)?;
        let retain = req.get_boolean_or("retain", false)?;
        let timeout = req.get_float_opt("timeout")?;
        if !(0..=2).contains(&qos) {
            return Err(ServerError::bad("QOS must be between 0 and 2"));
        }
        let fut = self.publish_topic(&topic, payload, Some(qos as u8), retain);
        match timeout {
            Some(t) => match tokio::time::timeout(Duration::from_secs_f64(t.max(0.0)), fut).await {
                Ok(r) => r?,
                Err(_) => return Err(ServerError::new("MQTT Publish Timed Out", 504)),
            },
            None => fut.await?,
        }
        Ok(json!({"topic": topic}))
    }

    pub(super) async fn handle_subscription_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let topic = req.get_str("topic")?;
        let qos = req.get_int_or("qos", self.qos as i64)?;
        let timeout = req.get_float_opt("timeout")?;
        if !(0..=2).contains(&qos) {
            return Err(ServerError::bad("QOS must be between 0 and 2"));
        }
        let (tx, rx) = oneshot::channel();
        let slot: RespSlot = Rc::new(RefCell::new(Some(tx)));
        let s = slot.clone();
        let cb: MqttCallback = Rc::new(move |payload| {
            if let Some(tx) = s.borrow_mut().take() {
                let _ = tx.send(Ok(payload));
            }
        });
        let hdl = self.subscribe_topic(&topic, cb, Some(qos as u8))?;
        self.pending_responses.borrow_mut().push(slot.clone());
        let res = match timeout {
            Some(t) => tokio::time::timeout(Duration::from_secs_f64(t.max(0.0)), rx).await.ok(),
            None => Some(rx.await),
        };
        self.pending_responses.borrow_mut().retain(|s| !Rc::ptr_eq(s, &slot));
        self.unsubscribe(&hdl);
        let ret = match res {
            None => return Err(ServerError::new("MQTT Subscribe Timed Out", 504)),
            Some(Ok(r)) => r?,
            Some(Err(_)) => return Err(ServerError::new("Moonraker Shutdown", 503)),
        };
        let payload = match serde_json::from_slice::<Value>(&ret) {
            Ok(v) => v,
            Err(_) => Value::String(String::from_utf8_lossy(&ret).to_string()),
        };
        Ok(json!({"topic": topic, "payload": payload}))
    }

    pub(super) async fn process_api_request(self: Rc<Self>, payload: Vec<u8>) {
        let rpc = server().app().json_rpc.clone();
        let transport: Rc<dyn ApiTransport> = self.clone();
        if let Some(resp) = rpc.dispatch(&payload, transport).await {
            let _ = self.publish_topic(&self.api_resp_topic, Value::String(resp), Some(self.api_qos), false).await;
        }
    }
}

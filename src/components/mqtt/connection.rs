// Connection management

use super::*;

impl MqttClient {
    pub(super) fn connect_packet(&self) -> Vec<u8> {
        let mut body = Vec::new();
        if self.protocol == 3 {
            put_str(&mut body, b"MQIsdp");
        } else {
            put_str(&mut body, b"MQTT");
        }
        body.push(self.protocol);
        let will_payload = json!({"server": "offline"}).to_string();
        let mut flags = 0x04 | (self.qos << 3) | 0x20;
        let clean = if self.protocol == 5 { self.first_connect.get() } else { true };
        if clean {
            flags |= 0x02;
        }
        if self.user_name.is_some() {
            flags |= 0x80;
            if self.password.is_some() {
                flags |= 0x40;
            }
        }
        body.push(flags);
        body.extend_from_slice(&(KEEPALIVE as u16).to_be_bytes());
        if self.protocol == 5 {
            body.push(0);
        }
        put_str(&mut body, self.client_id.as_bytes());
        if self.protocol == 5 {
            body.push(0);
        }
        put_str(&mut body, self.moonraker_status_topic.as_bytes());
        put_str(&mut body, will_payload.as_bytes());
        if let Some(u) = &self.user_name {
            put_str(&mut body, u.as_bytes());
            if let Some(p) = &self.password {
                put_str(&mut body, p.as_bytes());
            }
        }
        packet(0x10, body)
    }

    pub(super) async fn open_connection(&self) -> Result<Box<dyn Io>, String> {
        let addr = format!("{}:{}", self.address, self.port);
        let stream = tokio::time::timeout(Duration::from_secs(10), tokio::net::TcpStream::connect(&addr))
            .await
            .map_err(|_| "TimeoutError".to_string())?
            .map_err(|e| e.to_string())?;
        if !self.tls_enabled {
            return Ok(Box::new(stream));
        }
        let cfg = load_tls_config()?;
        let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(cfg));
        let name = tokio_rustls::rustls::pki_types::ServerName::try_from(self.address.clone()).map_err(|e| e.to_string())?;
        let tls = connector.connect(name, stream).await.map_err(|e| e.to_string())?;
        Ok(Box::new(tls))
    }

    pub(super) async fn do_reconnect(self: Rc<Self>, mut first: bool) {
        log::info!("Attempting MQTT Connect/Reconnect");
        let mut last_err = String::new();
        loop {
            if !first {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            first = false;
            match self.open_connection().await {
                Ok(stream) => {
                    self.clone().start_session(stream);
                    break;
                }
                Err(e) => {
                    if e != last_err {
                        log::error!("MQTT Connection Error\n{}", e);
                        last_err = e;
                    }
                }
            }
        }
        self.connect_task.clear();
    }

    pub(super) fn start_session(self: Rc<Self>, stream: Box<dyn Io>) {
        log::info!("MQTT Socket Opened");
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let sid = self.session_id.get() + 1;
        self.session_id.set(sid);
        let _ = tx.send(self.connect_packet());
        *self.writer.borrow_mut() = Some(tx);
        spawn(async move {
            let (mut rd, mut wr) = tokio::io::split(stream);
            let mut inbuf: Vec<u8> = Vec::new();
            let mut tmp = vec![0u8; 4096];
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            let mut last_out = Instant::now();
            let mut last_in = Instant::now();
            let mut ping_sent: Option<Instant> = None;
            let keepalive = Duration::from_secs(KEEPALIVE);
            let reason = loop {
                tokio::select! {
                    n = rd.read(&mut tmp) => {
                        match n {
                            Ok(0) | Err(_) => break "The connection was lost.",
                            Ok(n) => {
                                last_in = Instant::now();
                                inbuf.extend_from_slice(&tmp[..n]);
                                let mut failed = false;
                                while let Some((hdr, body)) = take_packet(&mut inbuf) {
                                    if hdr >> 4 == 13 {
                                        ping_sent = None;
                                    }
                                    if !self.handle_packet(hdr, body) {
                                        failed = true;
                                        break;
                                    }
                                }
                                if failed {
                                    break "The connection was refused.";
                                }
                            }
                        }
                    }
                    msg = rx.recv() => {
                        match msg {
                            Some(data) => {
                                let disconnect = data.first() == Some(&0xE0);
                                if wr.write_all(&data).await.is_err() {
                                    break "The connection was lost.";
                                }
                                last_out = Instant::now();
                                if disconnect {
                                    let _ = wr.shutdown().await;
                                    break "No error.";
                                }
                            }
                            None => break "No error.",
                        }
                    }
                    _ = tick.tick() => {
                        let now = Instant::now();
                        if now - last_out >= keepalive || now - last_in >= keepalive {
                            match ping_sent {
                                None if self.connect_evt.is_set() => {
                                    if wr.write_all(&[0xC0, 0]).await.is_err() {
                                        break "The connection was lost.";
                                    }
                                    last_out = now;
                                    ping_sent = Some(now);
                                }
                                Some(t) if now - t < keepalive => {}
                                _ => break "The connection was lost.",
                            }
                        }
                    }
                }
            };
            if self.session_id.get() == sid {
                *self.writer.borrow_mut() = None;
                self.on_disconnect(reason);
            }
        });
    }

    /// Returns false if the connection should be closed
    pub(super) fn handle_packet(self: &Rc<Self>, header: u8, body: Vec<u8>) -> bool {
        let v5 = self.protocol == 5;
        let mid_of = |b: &[u8]| -> u16 { if b.len() >= 2 { u16::from_be_bytes([b[0], b[1]]) } else { 0 } };
        match header >> 4 {
            2 => {
                let rc = body.get(1).copied().unwrap_or(0x80);
                return self.on_connect(rc);
            }
            3 => {
                let qos = (header >> 1) & 0x03;
                if body.len() < 2 {
                    return true;
                }
                let tlen = u16::from_be_bytes([body[0], body[1]]) as usize;
                if body.len() < 2 + tlen {
                    return true;
                }
                let topic = String::from_utf8_lossy(&body[2..2 + tlen]).to_string();
                let mut pos = 2 + tlen;
                let mut mid = 0u16;
                if qos > 0 {
                    mid = mid_of(&body[pos..]);
                    pos += 2;
                }
                if v5 {
                    if let Some(plen) = read_varint(&body, &mut pos) {
                        pos += plen;
                    }
                }
                let payload = body.get(pos..).unwrap_or(&[]).to_vec();
                match qos {
                    0 => self.on_message(&topic, payload),
                    1 => {
                        self.send_raw(packet(0x40, mid.to_be_bytes().to_vec()));
                        self.on_message(&topic, payload);
                    }
                    _ => {
                        self.incoming_qos2.borrow_mut().insert(mid, (topic, payload));
                        self.send_raw(packet(0x50, mid.to_be_bytes().to_vec()));
                    }
                }
            }
            4 | 7 => {
                let mid = mid_of(&body);
                self.inflight.borrow_mut().shift_remove(&mid);
                self.on_publish(mid);
            }
            5 => {
                let mid = mid_of(&body);
                let rel = packet(0x62, mid.to_be_bytes().to_vec());
                if let Some(p) = self.inflight.borrow_mut().get_mut(&mid) {
                    *p = rel.clone();
                }
                self.send_raw(rel);
            }
            6 => {
                let mid = mid_of(&body);
                self.send_raw(packet(0x70, mid.to_be_bytes().to_vec()));
                let msg = self.incoming_qos2.borrow_mut().remove(&mid);
                if let Some((topic, payload)) = msg {
                    self.on_message(&topic, payload);
                }
            }
            9 => {
                let mid = mid_of(&body);
                let mut pos = 2;
                if v5 {
                    if let Some(plen) = read_varint(&body, &mut pos) {
                        pos += plen;
                    }
                }
                let codes: Vec<u8> = body.get(pos..).unwrap_or(&[]).to_vec();
                if let Some(PendingAck::Subscribe(topics)) = self.pending_acks.borrow_mut().remove(&mid) {
                    let mut log_msg = String::from("MQTT Subscriptions Acknowledged");
                    if codes.len() != topics.len() {
                        log_msg.push_str(&format!(
                            "\nTopic / QOS count mismatch, \nTopics: {} \nQoS responses: {:?}",
                            crate::pyfmt::py_repr(&json!(topics)),
                            codes
                        ));
                    } else {
                        for (topic, qos) in topics.iter().zip(codes.iter()) {
                            let name = if v5 { reason_name(*qos) } else { format!("Granted QoS {}", qos) };
                            let name = if v5 && *qos == 0 { "Granted QoS 0".to_string() } else { name };
                            log_msg.push_str(&format!("\n Topic: {} | {}", topic, name));
                        }
                    }
                    log::debug!("{}", log_msg);
                }
            }
            11 => {
                let mid = mid_of(&body);
                if let Some(PendingAck::Unsubscribe(topics)) = self.pending_acks.borrow_mut().remove(&mid) {
                    let mut log_msg = String::from("MQTT Unsubscribe Acknowledged");
                    for t in topics {
                        log_msg.push_str(&format!("\n Topic: {}", t));
                    }
                    log::debug!("{}", log_msg);
                }
            }
            _ => {}
        }
        true
    }

    pub(super) fn on_connect(self: &Rc<Self>, rc: u8) -> bool {
        log::info!("MQTT Client Connected");
        if rc != 0 {
            let err_str = if self.protocol == 5 { reason_name(rc) } else { connack_string(rc) };
            let srv = server();
            srv.set_failed_component("mqtt");
            srv.add_warning(&format!("MQTT Connection Failed: {}", err_str), None, true);
            return false;
        }
        self.first_connect.set(false);
        let _ = self.publish_topic(&self.moonraker_status_topic, json!({"server": "online"}), None, true);
        let subs: Vec<(String, u8)> = self.subscribed_topics.borrow().iter().map(|(k, v)| (k.clone(), v.0)).collect();
        if !subs.is_empty() {
            let mid = self.next_mid();
            self.send_raw(self.subscribe_packet(mid, &subs));
            let topics = subs.into_iter().map(|(t, _)| t).collect();
            self.pending_acks.borrow_mut().insert(mid, PendingAck::Subscribe(topics));
        }
        // paho retransmits unacknowledged messages after a reconnect
        let pending: Vec<Vec<u8>> = self.inflight.borrow().values().cloned().collect();
        for mut p in pending {
            if p[0] >> 4 == 3 {
                p[0] |= 0x08;
            }
            self.send_raw(p);
        }
        self.connect_evt.set();
        server().emit::<crate::events::MqttConnected>(());
        true
    }

    pub(super) fn on_disconnect(self: &Rc<Self>, reason: &str) {
        log::info!("MQTT Socket Closed");
        let devt = self.disconnect_evt.borrow().clone();
        if let Some(evt) = devt {
            evt.set();
        } else if self.is_connected() {
            log::info!("MQTT Server Disconnected, reason: {}", reason);
            if !self.connect_task.is_set() {
                let h = spawn(self.clone().do_reconnect(false));
                self.connect_task.set(h);
            }
            server().emit::<crate::events::MqttDisconnected>(());
        }
        self.connect_evt.clear();
    }

    pub(super) fn on_message(&self, topic: &str, payload: Vec<u8>) {
        let hdls: Option<Vec<Rc<SubscriptionHandle>>> = self.subscribed_topics.borrow().get(topic).map(|v| v.1.clone());
        match hdls {
            Some(hdls) => {
                for hdl in hdls {
                    let p = payload.clone();
                    spawn(async move { (hdl.callback)(p) });
                }
            }
            None => log::debug!(
                "Unregistered MQTT Topic Received: {}, payload: {}",
                topic,
                String::from_utf8_lossy(&payload)
            ),
        }
    }

    pub(super) fn on_publish(&self, mid: u16) {
        if let Some(PendingAck::Publish(tx)) = self.pending_acks.borrow_mut().remove(&mid) {
            let _ = tx.send(Ok(()));
        }
    }

    pub fn qos(&self) -> u8 {
        self.qos
    }

    pub fn get_instance_name(&self) -> String {
        self.instance_name.clone()
    }

    /// Moonraker's wait_connection: whether the client connected within the
    /// timeout (None: no limit)
    pub async fn wait_connection(&self, timeout: Option<f64>) -> bool {
        match timeout {
            Some(t) => tokio::time::timeout(Duration::from_secs_f64(t.max(0.0)), self.connect_evt.wait()).await.is_ok(),
            None => {
                self.connect_evt.wait().await;
                true
            }
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connect_evt.is_set()
    }
}

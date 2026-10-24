// MQTT client implementation (port of moonraker/components/mqtt.py)
//
// paho-mqtt is replaced by a small built in MQTT v3.1 / v3.1.1 / v5 client
// that mirrors the paho behaviour Moonraker relies on.

use crate::common::*;
use crate::confighelper::{Bounds, ConfigError, ConfigHelper};
use crate::server::{server, spawn, Component};
use crate::utils::AsyncEvent;
use async_trait::async_trait;
use futures::future::LocalBoxFuture;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

const DUP_API_REQ_CODE: i64 = -10000;
const KEEPALIVE: u64 = 60;
const PROTOCOLS: [(&str, u8); 3] = [("v3.1", 3), ("v3.1.1", 4), ("v5", 5)];

pub type MqttCallback = Rc<dyn Fn(Vec<u8>)>;

pub struct SubscriptionHandle {
    pub topic: String,
    callback: MqttCallback,
}

type AckSender = oneshot::Sender<Result<(), ServerError>>;
type RespSlot = Rc<RefCell<Option<oneshot::Sender<Result<Vec<u8>, ServerError>>>>>;

trait Io: AsyncRead + AsyncWrite + Unpin {}
impl<T: AsyncRead + AsyncWrite + Unpin> Io for T {}

enum PendingAck {
    Publish(AckSender),
    Subscribe(Vec<String>),
    Unsubscribe(Vec<String>),
}

pub struct MqttClient {
    uid: u64,
    address: String,
    port: u16,
    tls_enabled: bool,
    user_name: Option<String>,
    password: Option<String>,
    protocol: u8,
    instance_name: String,
    qos: u8,
    api_qos: u8,
    publish_split_status: bool,
    client_id: String,
    connect_evt: Rc<AsyncEvent>,
    disconnect_evt: RefCell<Option<Rc<AsyncEvent>>>,
    connect_task: RefCell<Option<tokio::task::JoinHandle<()>>>,
    writer: RefCell<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    session_id: Cell<u64>,
    first_connect: Cell<bool>,
    subscribed_topics: RefCell<IndexMap<String, (u8, Vec<Rc<SubscriptionHandle>>)>>,
    pending_responses: RefCell<Vec<RespSlot>>,
    pending_acks: RefCell<HashMap<u16, PendingAck>>,
    // outgoing qos > 0 publish packets awaiting acknowledgement, resent on reconnect
    inflight: RefCell<IndexMap<u16, Vec<u8>>>,
    incoming_qos2: RefCell<HashMap<u16, (String, Vec<u8>)>>,
    next_mid: Cell<u16>,
    api_request_topic: String,
    api_resp_topic: String,
    klipper_status_topic: String,
    klipper_state_prefix: String,
    moonraker_status_topic: String,
    status_interval: f64,
    status_cache: RefCell<Map<String, Value>>,
    status_timer: RefCell<Option<tokio::task::JoinHandle<()>>>,
    last_status_time: Cell<f64>,
    status_objs: Map<String, Value>,
    timestamp_deque: RefCell<VecDeque<Value>>,
}

fn connack_string(rc: u8) -> String {
    match rc {
        0 => "Connection Accepted.",
        1 => "Connection Refused: unacceptable protocol version.",
        2 => "Connection Refused: identifier rejected.",
        3 => "Connection Refused: broker unavailable.",
        4 => "Connection Refused: bad user name or password.",
        5 => "Connection Refused: not authorised.",
        _ => "Connection Refused: unknown reason.",
    }
    .into()
}

/// paho ReasonCode.getName() for the packet types we receive
fn reason_name(rc: u8) -> String {
    match rc {
        0 => "Success",
        1 => "Granted QoS 1",
        2 => "Granted QoS 2",
        0x80 => "Unspecified error",
        0x81 => "Malformed packet",
        0x82 => "Protocol error",
        0x83 => "Implementation specific error",
        0x84 => "Unsupported protocol version",
        0x85 => "Client identifier not valid",
        0x86 => "Bad user name or password",
        0x87 => "Not authorized",
        0x88 => "Server unavailable",
        0x89 => "Server busy",
        0x8A => "Banned",
        0x8C => "Bad authentication method",
        0x8F => "Topic filter invalid",
        0x90 => "Topic name invalid",
        0x91 => "Packet identifier in use",
        0x95 => "Packet too large",
        0x97 => "Quota exceeded",
        0x99 => "Payload format invalid",
        0x9A => "Retain not supported",
        0x9B => "QoS not supported",
        0x9C => "Use another server",
        0x9D => "Server moved",
        0x9E => "Shared subscriptions not supported",
        0x9F => "Connection rate exceeded",
        0xA1 => "Subscription identifiers not supported",
        0xA2 => "Wildcard subscriptions not supported",
        _ => "Unknown",
    }
    .into()
}

// ***** Packet encoding *****

fn put_str(buf: &mut Vec<u8>, s: &[u8]) {
    buf.extend_from_slice(&(s.len() as u16).to_be_bytes());
    buf.extend_from_slice(s);
}

fn packet(header: u8, body: Vec<u8>) -> Vec<u8> {
    let mut out = vec![header];
    let mut len = body.len();
    loop {
        let mut b = (len % 128) as u8;
        len /= 128;
        if len > 0 {
            b |= 0x80;
        }
        out.push(b);
        if len == 0 {
            break;
        }
    }
    out.extend(body);
    out
}

fn read_varint(data: &[u8], pos: &mut usize) -> Option<usize> {
    let mut mult = 1usize;
    let mut val = 0usize;
    for _ in 0..4 {
        let b = *data.get(*pos)?;
        *pos += 1;
        val += (b & 0x7F) as usize * mult;
        if b & 0x80 == 0 {
            return Some(val);
        }
        mult *= 128;
    }
    None
}

/// Splits one complete packet off the front of the buffer
fn take_packet(buf: &mut Vec<u8>) -> Option<(u8, Vec<u8>)> {
    let mut pos = 1;
    let len = read_varint(buf, &mut pos)?;
    if buf.len() < pos + len {
        return None;
    }
    let header = buf[0];
    let body = buf[pos..pos + len].to_vec();
    buf.drain(..pos + len);
    Some((header, body))
}

fn base62_id() -> String {
    const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    (0..22).map(|_| CHARS[rand::random::<usize>() % 62] as char).collect()
}

fn load_tls_config() -> Result<tokio_rustls::rustls::ClientConfig, String> {
    // paho's tls_set() uses the system CA store (OpenSSL default paths,
    // honoring SSL_CERT_FILE / SSL_CERT_DIR)
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    let certs = rustls_native_certs::load_native_certs();
    for cert in certs.certs {
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        return Err("No system CA certificates found".into());
    }
    Ok(tokio_rustls::rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth())
}

impl MqttClient {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, ConfigError> {
        let srv = server();
        let address = config.get("address")?;
        let port = config.getint_or("port", 1883)?;
        let tls_enabled = config.getboolean_or("enable_tls", false)?;
        let user_name = match config.gettemplate_opt("username", Some(None), false)? {
            Some(t) => Some(t.render(&json!({})).map_err(|e| ConfigHelper::error(e.message))?),
            None => None,
        };
        let pw_file_path = config.get_opt_dep("password_file", true)?;
        let pw_template = config.gettemplate_opt("password", Some(None), false)?;
        let mut password = None;
        if let Some(p) = pw_file_path {
            let pw_file = crate::utils::absolute(&crate::utils::expand_user(&p));
            if !pw_file.exists() {
                return Err(ConfigHelper::error(format!("Password file '{}' does not exist", pw_file.display())));
            }
            password = Some(std::fs::read_to_string(&pw_file).unwrap_or_default().trim().to_string());
        }
        if let Some(t) = pw_template {
            password = Some(t.render(&json!({})).map_err(|e| ConfigHelper::error(e.message))?);
        }
        let protocol_name = config.get_or("mqtt_protocol", "v3.1.1")?;
        let protocol = match PROTOCOLS.iter().find(|(n, _)| *n == protocol_name) {
            Some((_, p)) => *p,
            None => {
                return Err(ConfigHelper::error(format!(
                    "Invalid value '{}' for option 'mqtt_protocol' in section [mqtt]. Must be one of dict_values([3, 4, 5])",
                    protocol_name
                )))
            }
        };
        let instance_name = config.get_or("instance_name", &crate::utils::hostname())?;
        if instance_name.contains('+') || instance_name.contains('#') {
            return Err(ConfigHelper::error("Option 'instance_name' in section [mqtt] cannot contain a wildcard."));
        }
        let qos = config.getint_or("default_qos", 0)?;
        if !(0..=2).contains(&qos) {
            return Err(ConfigHelper::error("Option 'default_qos' in section [mqtt] must be between 0 and 2"));
        }
        let publish_split_status = config.getboolean_or("publish_split_status", false)?;
        let mut client_id = config.get_or("client_id", "")?;
        if client_id.is_empty() && protocol == 3 {
            client_id = base62_id();
        }
        let status_cfg = config.getdict("status_objects", Some(json!({})), (Some("\n"), "="), true)?;
        let status_interval = config.getfloat_b("status_interval", Some(0.0), Bounds::above(0.25))?;
        let mut status_objs = Map::new();
        if let Value::Object(m) = &status_cfg {
            for (key, val) in m {
                let v = match val {
                    Value::String(s) => Value::Array(
                        s.split(',').map(|v| v.trim()).filter(|v| !v.is_empty()).map(|v| json!(v)).collect(),
                    ),
                    _ => Value::Null,
                };
                status_objs.insert(key.clone(), v);
            }
        }
        let api_qos = config.getint_or("api_qos", qos)?;
        let enable_api = config.getboolean_or("enable_moonraker_api", true)?;
        let client = Rc::new(MqttClient {
            uid: crate::components::websockets::next_uid(),
            address,
            port: port as u16,
            tls_enabled,
            user_name,
            password,
            protocol,
            api_request_topic: format!("{}/moonraker/api/request", instance_name),
            api_resp_topic: format!("{}/moonraker/api/response", instance_name),
            klipper_status_topic: format!("{}/klipper/status", instance_name),
            klipper_state_prefix: format!("{}/klipper/state", instance_name),
            moonraker_status_topic: format!("{}/moonraker/status", instance_name),
            instance_name,
            qos: qos as u8,
            api_qos: api_qos as u8,
            publish_split_status,
            client_id,
            connect_evt: AsyncEvent::new(),
            disconnect_evt: RefCell::new(None),
            connect_task: RefCell::new(None),
            writer: RefCell::new(None),
            session_id: Cell::new(0),
            first_connect: Cell::new(true),
            subscribed_topics: RefCell::new(IndexMap::new()),
            pending_responses: RefCell::new(Vec::new()),
            pending_acks: RefCell::new(HashMap::new()),
            inflight: RefCell::new(IndexMap::new()),
            incoming_qos2: RefCell::new(HashMap::new()),
            next_mid: Cell::new(0),
            status_interval,
            status_cache: RefCell::new(Map::new()),
            status_timer: RefCell::new(None),
            last_status_time: Cell::new(0.0),
            status_objs,
            timestamp_deque: RefCell::new(VecDeque::new()),
        });

        let ep_transports = TransportType(TransportType::all().0 & !TransportType::MQTT.0);
        let c = client.clone();
        let _ = srv.register_endpoint_ext(
            "/server/mqtt/publish",
            RequestType::POST,
            callback(move |req| {
                let c = c.clone();
                async move { c.handle_publish_request(req).await }
            }),
            ep_transports,
            true,
            None,
            true,
        );
        let c = client.clone();
        let _ = srv.register_endpoint_ext(
            "/server/mqtt/subscribe",
            RequestType::POST,
            callback(move |req| {
                let c = c.clone();
                async move { c.handle_subscription_request(req).await }
            }),
            ep_transports,
            true,
            None,
            true,
        );

        if !status_cfg.as_object().map(|m| m.is_empty()).unwrap_or(true) {
            log::debug!("MQTT: Status Objects Set: {}", crate::pyfmt::py_repr(&Value::Object(client.status_objs.clone())));
            let c = client.clone();
            srv.on_event_async("server:klippy_started", move |_| {
                let c = c.clone();
                async move { c.handle_klippy_started().await }
            });
            let c = client.clone();
            srv.on_event("server:klippy_disconnect", move |_| c.handle_klippy_disconnect());
        }

        if enable_api {
            let c = Rc::downgrade(&client);
            let cb: MqttCallback = Rc::new(move |payload| {
                if let Some(c) = c.upgrade() {
                    spawn(async move { c.process_api_request(payload).await });
                }
            });
            let topic = client.api_request_topic.clone();
            let _ = client.subscribe_topic(&topic, cb, Some(client.api_qos));
        }

        let c = Rc::downgrade(&client);
        let _ = srv.register_remote_method(
            "publish_mqtt_topic",
            crate::components::klippy_connection::remote_method(move |args| {
                if let Some(c) = c.upgrade() {
                    spawn(async move { c.publish_from_klipper(args).await });
                }
            }),
        );
        log::info!(
            "\nReserved MQTT topics:\nAPI Request: {}\nAPI Response: {}\nMoonraker Status: {}\nKlipper Status: {}",
            client.api_request_topic,
            client.api_resp_topic,
            client.moonraker_status_topic,
            client.klipper_status_topic
        );
        Ok(client)
    }

    fn next_mid(&self) -> u16 {
        let mid = if self.next_mid.get() == u16::MAX { 1 } else { self.next_mid.get() + 1 };
        self.next_mid.set(mid);
        mid
    }

    fn send_raw(&self, data: Vec<u8>) -> bool {
        match self.writer.borrow().as_ref() {
            Some(w) => w.send(data).is_ok(),
            None => false,
        }
    }

    // ***** Connection management *****

    fn connect_packet(&self) -> Vec<u8> {
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

    async fn open_connection(&self) -> Result<Box<dyn Io>, String> {
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

    async fn do_reconnect(self: Rc<Self>, mut first: bool) {
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
        *self.connect_task.borrow_mut() = None;
    }

    fn start_session(self: Rc<Self>, stream: Box<dyn Io>) {
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
    fn handle_packet(self: &Rc<Self>, header: u8, body: Vec<u8>) -> bool {
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

    fn on_connect(self: &Rc<Self>, rc: u8) -> bool {
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
        let _ = server().send_event("mqtt:connected", vec![]);
        true
    }

    fn on_disconnect(self: &Rc<Self>, reason: &str) {
        log::info!("MQTT Socket Closed");
        let devt = self.disconnect_evt.borrow().clone();
        if let Some(evt) = devt {
            evt.set();
        } else if self.is_connected() {
            log::info!("MQTT Server Disconnected, reason: {}", reason);
            if self.connect_task.borrow().is_none() {
                let h = spawn(self.clone().do_reconnect(false));
                *self.connect_task.borrow_mut() = Some(h);
            }
            let _ = server().send_event("mqtt:disconnected", vec![]);
        }
        self.connect_evt.clear();
    }

    fn on_message(&self, topic: &str, payload: Vec<u8>) {
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

    fn on_publish(&self, mid: u16) {
        if let Some(PendingAck::Publish(tx)) = self.pending_acks.borrow_mut().remove(&mid) {
            let _ = tx.send(Ok(()));
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connect_evt.is_set()
    }

    // ***** Subscriptions and publishing *****

    fn subscribe_packet(&self, mid: u16, subs: &[(String, u8)]) -> Vec<u8> {
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
    ) -> LocalBoxFuture<'static, Result<(), ServerError>> {
        let qos = match qos {
            Some(q) if q != 0 => q,
            _ => self.qos,
        };
        if qos > 2 {
            return Box::pin(async { Err(ServerError::bad("QOS must be between 0 and 2")) });
        }
        let data: Vec<u8> = match &payload {
            Value::Object(_) | Value::Array(_) => payload.to_string().into_bytes(),
            Value::Bool(b) => b.to_string().into_bytes(),
            Value::Null => Vec::new(),
            Value::String(s) => s.clone().into_bytes(),
            Value::Number(_) => crate::pyfmt::py_str(&payload).into_bytes(),
        };
        if topic.is_empty() || topic.contains('+') || topic.contains('#') || data.len() > 268435455 {
            return Box::pin(async { Err(ServerError::new("MQTT Message Queue Full", 529)) });
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
            return Box::pin(async { Ok(()) });
        }
        let (tx, rx) = oneshot::channel();
        self.pending_acks.borrow_mut().insert(mid, PendingAck::Publish(tx));
        self.inflight.borrow_mut().insert(mid, pkt.clone());
        self.send_raw(pkt);
        Box::pin(async move { rx.await.unwrap_or_else(|_| Err(ServerError::new("Moonraker Shutdown", 503))) })
    }

    async fn handle_publish_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
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

    async fn handle_subscription_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
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

    async fn process_api_request(self: Rc<Self>, payload: Vec<u8>) {
        let rpc = server().app().json_rpc.clone();
        let transport: Rc<dyn ApiTransport> = self.clone();
        if let Some(resp) = rpc.dispatch(&payload, transport).await {
            let _ = self.publish_topic(&self.api_resp_topic, Value::String(resp), Some(self.api_qos), false).await;
        }
    }

    // ***** Klipper status *****

    async fn handle_klippy_started(self: Rc<Self>) {
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
            if let Some(old) = self.status_timer.borrow_mut().replace(h) {
                old.abort();
            }
        }
    }

    fn handle_klippy_disconnect(&self) {
        if let Some(t) = self.status_timer.borrow_mut().take() {
            t.abort();
        }
        self.handle_timed_status_update();
    }

    fn handle_timed_status_update(&self) {
        let payload = std::mem::take(&mut *self.status_cache.borrow_mut());
        if !payload.is_empty() {
            self.publish_status_update(&payload, self.last_status_time.get());
        }
    }

    fn publish_status_update(&self, status: &Map<String, Value>, eventtime: f64) {
        if self.publish_split_status {
            for (objkey, objval) in status {
                if let Value::Object(o) = objval {
                    for (statekey, v) in o {
                        let payload = json!({"eventtime": eventtime, "value": v});
                        let _ = self.publish_topic(&format!("{}/{}/{}", self.klipper_state_prefix, objkey, statekey), payload, None, true);
                    }
                }
            }
        } else {
            let payload = json!({"eventtime": eventtime, "status": status});
            let _ = self.publish_topic(&self.klipper_status_topic, payload, None, false);
        }
    }

    async fn publish_from_klipper(self: Rc<Self>, args: Map<String, Value>) {
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

impl ApiTransport for MqttClient {
    fn transport_type(&self) -> TransportType {
        TransportType::MQTT
    }
    fn py_object(self: Rc<Self>) -> Option<(Rc<dyn std::any::Any>, &'static str, &'static str)> {
        Some((self, "moonraker.components.mqtt", "MQTTClient"))
    }

    fn screen_rpc_request(&self, _api_def: &ApiDefinition, _rt: RequestType, args: &mut Args) -> Result<(), ServerError> {
        if let Some(ts) = args.shift_remove("mqtt_timestamp") {
            let mut dq = self.timestamp_deque.borrow_mut();
            if dq.contains(&ts) {
                log::debug!("Duplicate MQTT API request received");
                return Err(ServerError::new("Duplicate MQTT Request", DUP_API_REQ_CODE));
            }
            if dq.len() == 20 {
                dq.pop_front();
            }
            dq.push_back(ts);
        }
        Ok(())
    }

    fn send_status(&self, status: &Map<String, Value>, eventtime: f64) {
        if status.is_empty() || !self.is_connected() {
            return;
        }
        if self.status_interval == 0.0 {
            self.publish_status_update(status, eventtime);
        } else {
            let mut cache = self.status_cache.borrow_mut();
            for (key, val) in status {
                let entry = cache.entry(key.clone()).or_insert_with(|| json!({}));
                if let (Value::Object(e), Value::Object(v)) = (entry, val) {
                    for (k, x) in v {
                        e.insert(k.clone(), x.clone());
                    }
                }
            }
            self.last_status_time.set(eventtime);
        }
    }

    fn transport_id(&self) -> u64 {
        self.uid
    }
}

#[async_trait(?Send)]
impl Component for MqttClient {
    fn has_init(&self) -> bool {
        true
    }

    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        let h = spawn(self.clone().do_reconnect(true));
        *self.connect_task.borrow_mut() = Some(h);
        Ok(())
    }

    async fn close(self: Rc<Self>) {
        if let Some(t) = self.status_timer.borrow_mut().take() {
            t.abort();
        }
        if let Some(t) = self.connect_task.borrow_mut().take() {
            t.abort();
        }
        if !self.is_connected() {
            return;
        }
        let _ = self.publish_topic(&self.moonraker_status_topic, json!({"server": "offline"}), None, true).await;
        let evt = AsyncEvent::new();
        *self.disconnect_evt.borrow_mut() = Some(evt.clone());
        self.send_raw(vec![0xE0, 0]);
        if tokio::time::timeout(Duration::from_secs(2), evt.wait()).await.is_err() {
            log::info!("MQTT Disconnect Timeout");
        }
        let acks: Vec<PendingAck> = self.pending_acks.borrow_mut().drain().map(|(_, v)| v).collect();
        for a in acks {
            if let PendingAck::Publish(tx) = a {
                let _ = tx.send(Err(ServerError::new("Moonraker Shutdown", 503)));
            }
        }
        let resps: Vec<RespSlot> = self.pending_responses.borrow_mut().drain(..).collect();
        for r in resps {
            if let Some(tx) = r.borrow_mut().take() {
                let _ = tx.send(Err(ServerError::new("Moonraker Shutdown", 503)));
            }
        }
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<MqttClient>, ConfigError> {
    MqttClient::new(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_roundtrip() {
        let body = vec![7u8; 300];
        let mut buf = packet(0x30, body.clone());
        assert_eq!(&buf[1..3], &[0xAC, 0x02]);
        buf.push(0xC0);
        let (h, b) = take_packet(&mut buf).unwrap();
        assert_eq!((h, b), (0x30, body));
        assert_eq!(buf, vec![0xC0]);
        assert!(take_packet(&mut buf).is_none());
    }
}

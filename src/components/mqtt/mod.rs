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
    connect_task: crate::server::TaskSlot,
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
    status_timer: crate::server::TaskSlot,
    last_status_time: Cell<f64>,
    status_objs: Map<String, Value>,
    timestamp_deque: RefCell<VecDeque<Value>>,
}

mod connection;
mod klipper_status;
mod protocol;
mod pubsub;

use protocol::*;

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
            connect_task: Default::default(),
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
            status_timer: Default::default(),
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
}

impl MqttClient {
    /// screen_rpc_request's check of a request's mqtt_timestamp
    pub fn screen_timestamp(&self, ts: Value) -> Result<(), ServerError> {
        if ts.is_null() {
            return Ok(());
        }
        let mut dq = self.timestamp_deque.borrow_mut();
        if dq.contains(&ts) {
            log::debug!("Duplicate MQTT API request received");
            return Err(ServerError::new("Duplicate MQTT Request", DUP_API_REQ_CODE));
        }
        if dq.len() == 20 {
            dq.pop_front();
        }
        dq.push_back(ts);
        Ok(())
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
        match args.shift_remove("mqtt_timestamp") {
            Some(ts) => self.screen_timestamp(ts),
            None => Ok(()),
        }
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
        self.connect_task.set(h);
        Ok(())
    }

    async fn close(self: Rc<Self>) {
        self.status_timer.cancel();
        self.connect_task.cancel();
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

// Zeroconf registration implementation (port of moonraker/components/zeroconf.py)
//
// python-zeroconf is replaced by the mdns-sd responder.

use crate::common::*;
use crate::components::machine::Machine;
use crate::confighelper::{ConfigError, ConfigHelper};
use crate::server::{server, spawn, Component};
use async_trait::async_trait;
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::rc::Rc;

const ZC_SERVICE_TYPE: &str = "_moonraker._tcp.local.";
const SSDP_ADDR: (Ipv4Addr, u16) = (Ipv4Addr::new(239, 255, 255, 250), 1900);
const SSDP_SERVER_ID: &str = "Moonraker SSDP/UPNP Server";
const SSDP_MAX_AGE: f64 = 1800.0;
const SSDP_DEVICE_TYPE: &str = "urn:arksine.github.io:device:Moonraker:1";

#[derive(Clone, Copy, PartialEq, Eq)]
enum IpVersion {
    All,
    V4Only,
    V6Only,
}

struct ServiceSpec {
    name: String,
    host: String,
    props: HashMap<String, String>,
    port: u16,
}

pub struct ZeroconfRegistrar {
    mdns_name: String,
    ip_version: IpVersion,
    cfg_addr: String,
    bound_all: bool,
    daemon: RefCell<Option<mdns_sd::ServiceDaemon>>,
    service: RefCell<Option<ServiceSpec>>,
    fullname: RefCell<String>,
    ssdp: Option<Rc<SsdpServer>>,
}

impl ZeroconfRegistrar {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, ConfigError> {
        let srv = server();
        let hi = srv.get_host_info();
        let hostname = hi["hostname"].as_str().unwrap_or("").to_string();
        let mdns_name = config.get_or("mdns_hostname", &hostname)?;
        let mut addr = hi["address"].as_str().unwrap_or("").to_string();
        let mut ip_version = IpVersion::All;
        if addr.to_lowercase() == "all" {
            addr = "::".into();
        } else {
            let a: IpAddr = addr
                .parse()
                .map_err(|_| ConfigHelper::error(format!("'{}' does not appear to be an IPv4 or IPv6 address", addr)))?;
            ip_version = if a.is_ipv4() { IpVersion::V4Only } else { IpVersion::V6Only };
        }
        let bound_all = addr == "0.0.0.0" || addr == "::";
        let ssdp = if config.getboolean_or("enable_ssdp", false)? { Some(SsdpServer::new()?) } else { None };
        let zc = Rc::new(ZeroconfRegistrar {
            mdns_name,
            ip_version,
            cfg_addr: addr,
            bound_all,
            daemon: RefCell::new(None),
            service: RefCell::new(None),
            fullname: RefCell::new(String::new()),
            ssdp,
        });
        if bound_all {
            let z = Rc::downgrade(&zc);
            srv.on_event("machine:net_state_changed", move |args| {
                if let Some(z) = z.upgrade() {
                    let network = args.into_iter().next().unwrap_or(Value::Null);
                    z.update_service(&network);
                }
            });
        }
        Ok(zc)
    }

    fn extract_ip_addresses(&self, network: &Value) -> Vec<IpAddr> {
        let mut out = Vec::new();
        if let Value::Object(ifaces) = network {
            for (_, info) in ifaces {
                for a in info.get("ip_addresses").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
                    if a.get("is_link_local").and_then(|v| v.as_bool()).unwrap_or(false) {
                        continue;
                    }
                    let ip: IpAddr = match a.get("address").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()) {
                        Some(ip) => ip,
                        None => continue,
                    };
                    if (self.ip_version == IpVersion::V4Only && ip.is_ipv6()) || (self.ip_version == IpVersion::V6Only && ip.is_ipv4()) {
                        continue;
                    }
                    out.push(ip);
                }
            }
        }
        out
    }

    fn register(&self, addresses: &[IpAddr]) {
        let daemon = self.daemon.borrow();
        let (Some(d), Some(spec)) = (daemon.as_ref(), self.service.borrow().as_ref().map(|s| (s.name.clone(), s.host.clone(), s.props.clone(), s.port))) else {
            return;
        };
        let (name, host, props, port) = spec;
        match mdns_sd::ServiceInfo::new(ZC_SERVICE_TYPE, &name, &host, addresses, port, props) {
            Ok(info) => {
                *self.fullname.borrow_mut() = info.get_fullname().to_string();
                if let Err(e) = d.register(info) {
                    log::error!("Zeroconf registration failed: {}", e);
                }
            }
            Err(e) => log::error!("Zeroconf service info error: {}", e),
        }
    }

    fn update_service(&self, network: &Value) {
        if self.bound_all {
            let addrs = self.extract_ip_addresses(network);
            self.register(&addrs);
        }
    }
}

#[async_trait(?Send)]
impl Component for ZeroconfRegistrar {
    fn has_init(&self) -> bool {
        true
    }

    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        log::info!("Starting Zeroconf services");
        let srv = server();
        let app = srv.app();
        let machine: Rc<Machine> = srv.lookup_component("machine").ok_or_else(|| ServerError::new("machine", 500))?;
        let app_args = srv.get_app_args();
        let instance_uuid = app_args.get("instance_uuid").map(crate::pyfmt::py_str).unwrap_or_default();
        let svc_info = machine.get_moonraker_service_info();
        let instance_name = if machine.get_provider_type().starts_with("systemd") && svc_info.get("unit_name").is_some() {
            let u = machine.unit_name();
            let mut c = u.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
                None => u,
            }
        } else {
            format!("Moonraker-{}", &instance_uuid[..instance_uuid.len().min(8)])
        };
        let hi = srv.get_host_info();
        let mut host = self.mdns_name.clone();
        let mut props = HashMap::new();
        props.insert("uuid".to_string(), instance_uuid.clone());
        props.insert(
            "https_port".to_string(),
            if app.https_enabled() { crate::pyfmt::py_str(&hi["ssl_port"]) } else { String::new() },
        );
        props.insert("version".to_string(), app_args.get("software_version").map(crate::pyfmt::py_str).unwrap_or_default());
        props.insert("route_prefix".to_string(), app.route_prefix().to_string());
        let addresses = if self.bound_all {
            if host.is_empty() {
                host = machine.public_ip().unwrap_or_default();
            }
            let network = machine.get_system_info().get("network").cloned().unwrap_or(Value::Null);
            self.extract_ip_addresses(&network)
        } else {
            if host.is_empty() {
                host = self.cfg_addr.clone();
            }
            self.cfg_addr.parse::<IpAddr>().map(|a| vec![a]).unwrap_or_default()
        };
        let server_name = if self.mdns_name.is_empty() { instance_name.to_lowercase() } else { self.mdns_name.clone() };
        let port = hi["port"].as_u64().unwrap_or(7125) as u16;
        *self.service.borrow_mut() = Some(ServiceSpec {
            name: format!("{} @ {}", instance_name, host),
            host: format!("{}.local.", server_name),
            props,
            port,
        });
        match mdns_sd::ServiceDaemon::new() {
            Ok(d) => {
                match self.ip_version {
                    IpVersion::V4Only => {
                        let _ = d.disable_interface(mdns_sd::IfKind::IPv6);
                    }
                    IpVersion::V6Only => {
                        let _ = d.disable_interface(mdns_sd::IfKind::IPv4);
                    }
                    IpVersion::All => {}
                }
                *self.daemon.borrow_mut() = Some(d);
                self.register(&addresses);
            }
            Err(e) => return Err(ServerError::new(format!("Zeroconf daemon error: {}", e), 500)),
        }
        if let Some(ssdp) = &self.ssdp {
            let mut addr = if self.bound_all { machine.public_ip().unwrap_or_default() } else { self.cfg_addr.clone() };
            if addr.is_empty() {
                addr = format!("{}.local", self.mdns_name);
            }
            let mut name = format!("{} ({})", instance_name, host);
            if name.chars().count() > 64 {
                name = instance_name.clone();
            }
            ssdp.start().await;
            ssdp.clone().register_service(&name, &addr, port);
        }
        Ok(())
    }

    async fn close(self: Rc<Self>) {
        let d = self.daemon.borrow_mut().take();
        if let Some(d) = d {
            let fname = self.fullname.borrow().clone();
            if let Ok(rx) = d.unregister(&fname) {
                let _ = tokio::task::spawn_blocking(move || rx.recv_timeout(std::time::Duration::from_secs(2))).await;
            }
            let _ = d.shutdown();
        }
        if let Some(s) = &self.ssdp {
            s.stop().await;
        }
    }
}

// ***** SSDP *****

pub struct SsdpServer {
    unique_id: uuid::Uuid,
    name: RefCell<String>,
    base_url: RefCell<String>,
    response_headers: RefCell<Vec<String>>,
    registered: Cell<bool>,
    running: Cell<bool>,
    boot_id: i64,
    config_id: i64,
    socket: RefCell<Option<Rc<tokio::net::UdpSocket>>>,
    tasks: RefCell<Vec<tokio::task::JoinHandle<()>>>,
    responding: Cell<bool>,
}

impl SsdpServer {
    fn new() -> Result<Rc<Self>, ConfigError> {
        let srv = server();
        let uuid_str = srv.get_app_args().get("instance_uuid").map(crate::pyfmt::py_str).unwrap_or_default();
        let unique_id = uuid::Uuid::parse_str(&uuid_str).map_err(|_| ConfigHelper::error("badly formed hexadecimal UUID string"))?;
        let ssdp = Rc::new(SsdpServer {
            unique_id,
            name: RefCell::new("Moonraker".into()),
            base_url: RefCell::new(String::new()),
            response_headers: RefCell::new(vec![]),
            registered: Cell::new(false),
            running: Cell::new(false),
            boot_id: crate::utils::monotonic() as i64,
            config_id: 1,
            socket: RefCell::new(None),
            tasks: RefCell::new(vec![]),
            responding: Cell::new(false),
        });
        let s = ssdp.clone();
        let _ = srv.register_endpoint_ext(
            "/server/zeroconf/ssdp",
            RequestType::GET,
            callback(move |_req| {
                let s = s.clone();
                async move { s.handle_xml_request() }
            }),
            TransportType::HTTP,
            false,
            Some("application/xml"),
            false,
        );
        Ok(ssdp)
    }

    fn create_socket() -> std::io::Result<std::net::UdpSocket> {
        use socket2::{Domain, Protocol, Socket, Type};
        let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        sock.set_reuse_address(true)?;
        let _ = sock.set_reuse_port(true);
        sock.set_broadcast(true)?;
        sock.set_multicast_if_v4(&Ipv4Addr::UNSPECIFIED)?;
        sock.set_multicast_ttl_v4(2)?;
        sock.join_multicast_v4(&SSDP_ADDR.0, &Ipv4Addr::UNSPECIFIED)?;
        sock.set_nonblocking(true)?;
        sock.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, SSDP_ADDR.1)).into())?;
        Ok(sock.into())
    }

    async fn start(self: &Rc<Self>) {
        if self.running.get() {
            return;
        }
        let sock = match Self::create_socket().and_then(tokio::net::UdpSocket::from_std) {
            Ok(s) => Rc::new(s),
            Err(_) => return,
        };
        log::debug!("SSDP Server Connected");
        *self.socket.borrow_mut() = Some(sock.clone());
        self.running.set(true);
        let w = Rc::downgrade(self);
        let h = spawn(async move {
            let mut buf = vec![0u8; 4096];
            loop {
                let (n, addr) = match sock.recv_from(&mut buf).await {
                    Ok(r) => r,
                    Err(e) => {
                        log::info!("SSDP Server Error: {}", e);
                        continue;
                    }
                };
                match w.upgrade() {
                    Some(s) => s.datagram_received(&buf[..n], addr),
                    None => return,
                }
            }
        });
        self.tasks.borrow_mut().push(h);
    }

    async fn stop(&self) {
        if !self.running.get() {
            return;
        }
        self.running.set(false);
        for t in self.tasks.borrow_mut().drain(..) {
            t.abort();
        }
        let sock = self.socket.borrow_mut().take();
        if let Some(s) = sock {
            for n in self.build_notifications("ssdp:byebye") {
                let _ = s.send_to(&n, SocketAddr::from(SSDP_ADDR)).await;
            }
        }
        log::debug!("SSDP Server Disconnected");
    }

    fn register_service(self: Rc<Self>, name: &str, host: &str, port: u16) {
        let name: String = name.chars().take(64).collect();
        *self.name.borrow_mut() = name;
        let prefix = server().app().route_prefix().to_string();
        *self.base_url.borrow_mut() = format!("http://{}:{}{}", host, port, prefix);
        *self.response_headers.borrow_mut() = vec![
            format!("USN: uuid:{}::upnp:rootdevice", self.unique_id),
            format!("LOCATION: {}/server/zeroconf/ssdp", self.base_url.borrow()),
            "ST: upnp:rootdevice".into(),
            "EXT:".into(),
            format!("SERVER: {}", SSDP_SERVER_ID),
            format!("CACHE-CONTROL: max-age={}", SSDP_MAX_AGE as i64),
            format!("BOOTID.UPNP.ORG: {}", self.boot_id),
            format!("CONFIGID.UPNP.ORG: {}", self.config_id),
        ];
        self.registered.set(true);
        let ads = self.build_notifications("ssdp:alive");
        let w = Rc::downgrade(&self);
        let h = spawn(async move {
            let sock = w.upgrade().and_then(|s| s.socket.borrow().clone());
            if let Some(s) = &sock {
                for ad in &ads {
                    let _ = s.send_to(ad, SocketAddr::from(SSDP_ADDR)).await;
                }
            }
            // FlexTimer.start() runs the advertisement immediately, then at random intervals
            let mut idx = 0;
            loop {
                let s = match w.upgrade() {
                    Some(s) => s,
                    None => return,
                };
                if s.running.get() && s.registered.get() {
                    // the borrow ends here, before the await (an `if let`
                    // scrutinee's would last the whole block)
                    let sock = s.socket.borrow().clone();
                    if let Some(sock) = sock {
                        let _ = sock.send_to(&ads[idx % ads.len()], SocketAddr::from(SSDP_ADDR)).await;
                        idx += 1;
                    }
                }
                drop(s);
                let delay = SSDP_MAX_AGE / 6.0 + rand::random::<f64>() * (SSDP_MAX_AGE / 3.0 - SSDP_MAX_AGE / 6.0);
                tokio::time::sleep(std::time::Duration::from_secs_f64(delay)).await;
            }
        });
        self.tasks.borrow_mut().push(h);
    }

    fn handle_xml_request(&self) -> ApiResult {
        if !self.registered.get() {
            return Err(ServerError::new("Moonraker SSDP Device not registered", 404));
        }
        let version = server().get_app_args().get("software_version").map(crate::pyfmt::py_str).unwrap_or_default();
        let xml = format!(
            r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0" configId="{}">
    <specVersion>
        <major>2</major>
        <minor>0</minor>
    </specVersion>
    <device>
        <deviceType>{}</deviceType>
        <friendlyName>{}</friendlyName>
        <manufacturer>Arksine</manufacturer>
        <manufacturerURL>https://github.com/Arksine/moonraker</manufacturerURL>
        <modelDescription>API Server for Klipper</modelDescription>
        <modelName>Moonraker</modelName>
        <modelNumber>{}</modelNumber>
        <modelURL>https://github.com/Arksine/moonraker</modelURL>
        <serialNumber>{}</serialNumber>
        <UDN>uuid:{}</UDN>
        <presentationURL>{}</presentationURL>
    </device>
</root>"#,
            self.config_id,
            SSDP_DEVICE_TYPE,
            self.name.borrow(),
            version,
            self.unique_id.simple(),
            self.unique_id,
            self.base_url.borrow()
        );
        Ok(Value::String(xml))
    }

    fn datagram_received(self: &Rc<Self>, data: &[u8], addr: SocketAddr) {
        if !self.registered.get() {
            return;
        }
        let text = match std::str::from_utf8(data) {
            Ok(t) => t,
            Err(_) => {
                log::error!("Data Decode Error");
                return;
            }
        };
        let header = text.split("\r\n\r\n").next().unwrap_or("");
        let mut lines = header.lines();
        let cmd = lines.next().unwrap_or("").trim().to_string();
        let mut headers: HashMap<String, String> = HashMap::new();
        for line in lines {
            if let Some((k, v)) = line.trim().split_once(':') {
                headers.insert(k.to_uppercase(), v.trim().to_string());
            }
        }
        if cmd != "M-SEARCH * HTTP/1.1" || headers.get("MAN").map(|s| s.as_str()) != Some("\"ssdp:discover\"") {
            return;
        }
        if !matches!(headers.get("ST").map(|s| s.as_str()), Some("upnp:rootdevice") | Some("ssdp:all")) {
            return;
        }
        if self.responding.get() {
            return;
        }
        let delay = match headers.get("MX") {
            Some(mx) => match mx.parse::<f64>() {
                Ok(m) => rand::random::<f64>() * m,
                Err(_) => return,
            },
            None => 0.0,
        };
        self.responding.set(true);
        let s = self.clone();
        spawn(async move {
            if delay > 0.0 {
                tokio::time::sleep(std::time::Duration::from_secs_f64(delay)).await;
            }
            s.respond_to_discovery(addr).await;
        });
    }

    async fn respond_to_discovery(&self, addr: SocketAddr) {
        self.responding.set(false);
        if !self.running.get() {
            return;
        }
        let mut response = vec!["HTTP/1.1 200 OK".to_string()];
        response.extend(self.response_headers.borrow().iter().cloned());
        response.push(format!("DATE: {}", httpdate::fmt_http_date(std::time::SystemTime::now())));
        response.extend(["".to_string(), "".to_string()]);
        let sock = self.socket.borrow().clone();
        if let Some(s) = sock {
            let _ = s.send_to(response.join("\r\n").as_bytes(), addr).await;
        }
    }

    fn build_notifications(&self, nts: &str) -> Vec<Vec<u8>> {
        let id = self.unique_id;
        let types = [
            ("upnp:rootdevice".to_string(), format!("uuid:{}::upnp:rootdevice", id)),
            (format!("uuid:{}", id), format!("uuid:{}", id)),
            (SSDP_DEVICE_TYPE.to_string(), format!("uuid:{}::{}", id, SSDP_DEVICE_TYPE)),
        ];
        types
            .iter()
            .map(|(nt, usn)| {
                [
                    "NOTIFY * HTTP/1.1".to_string(),
                    format!("HOST: {}:{}", SSDP_ADDR.0, SSDP_ADDR.1),
                    format!("NTS: {}", nts),
                    format!("NT: {}", nt),
                    format!("USN: {}", usn),
                    format!("LOCATION: {}/server/zeroconf/ssdp", self.base_url.borrow()),
                    "EXT:".into(),
                    format!("SERVER: {}", SSDP_SERVER_ID),
                    format!("CACHE-CONTROL: max-age={}", SSDP_MAX_AGE as i64),
                    format!("BOOTID.UPNP.ORG: {}", self.boot_id),
                    format!("CONFIGID.UPNP.ORG: {}", self.config_id),
                    String::new(),
                    String::new(),
                ]
                .join("\r\n")
                .into_bytes()
            })
            .collect()
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<ZeroconfRegistrar>, ConfigError> {
    ZeroconfRegistrar::new(config)
}

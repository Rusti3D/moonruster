// Utilities for enumerating devices using sysfs (port of moonraker/utils/sysfs_devs.py)
// and the CAN socket utility (moonraker/utils/cansocket.py)

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const USB_DEVICE_PATH: &str = "/sys/bus/usb/devices";
const TTY_PATH: &str = "/sys/class/tty";
const SER_BYPTH_PATH: &str = "/dev/serial/by-path";
const SER_BYID_PATH: &str = "/dev/serial/by-id";
const V4L_DEVICE_PATH: &str = "/sys/class/video4linux";
const V4L_BYPTH_PATH: &str = "/dev/v4l/by-path";
const V4L_BYID_PATH: &str = "/dev/v4l/by-id";
const OPTIONAL_USB_INFO: [&str; 3] = ["manufacturer", "product", "serial"];
const NULL_DESCRIPTIONS: [&str; 6] = ["?", "none", "undefined", "reserved/undefined", "unused", "no subclass"];

fn read_item(parent: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(parent.join(name)).ok().map(|s| s.trim().to_string())
}

fn resolve_name(p: &Path) -> String {
    std::fs::canonicalize(p)
        .ok()
        .and_then(|r| r.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_default()
}

pub fn find_usb_folder(mut usb_path: PathBuf) -> Option<String> {
    while usb_path.is_dir() && usb_path.file_name().is_some() {
        let dnum = usb_path.join("devnum");
        let bnum = usb_path.join("busnum");
        if !dnum.is_file() || !bnum.is_file() {
            usb_path = usb_path.parent()?.to_path_buf();
            continue;
        }
        let devnum: i64 = read_item(&usb_path, "devnum")?.parse().ok()?;
        let busnum: i64 = read_item(&usb_path, "busnum")?.parse().ok()?;
        return Some(format!("{}:{}", busnum, devnum));
    }
    None
}

static USB_INFO_CACHE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

pub struct UsbIdData {
    path: PathBuf,
    parsed: bool,
    usb_info: HashMap<String, String>,
}

impl UsbIdData {
    pub fn new(path: &Path) -> Self {
        let mut c = USB_INFO_CACHE.lock().unwrap();
        if c.is_none() {
            let mut m = HashMap::new();
            m.insert("DI:1d50".into(), "OpenMoko, Inc".into());
            m.insert("DI:1d50:614e".into(), "Klipper 3d-Printer Firmware".into());
            m.insert("DI:1d50:6177".into(), "Katapult Bootloader (CDC_ACM)".into());
            *c = Some(m);
        }
        UsbIdData { path: crate::utils::resolve_path(path), parsed: false, usb_info: HashMap::new() }
    }

    fn get_item(&mut self, key: &str, check_null: bool) -> Option<String> {
        let cached = USB_INFO_CACHE.lock().unwrap().as_ref().and_then(|c| c.get(key).cloned());
        let mut item = self.usb_info.get(key).cloned().or(cached);
        if item.is_none() {
            if self.parsed {
                return None;
            }
            self.parse_usb_ids();
            item = self.usb_info.get(key).cloned();
            item.as_ref()?;
        }
        let item = item.unwrap();
        if let Some(c) = USB_INFO_CACHE.lock().unwrap().as_mut() {
            c.insert(key.to_string(), item.clone());
        }
        if check_null && NULL_DESCRIPTIONS.contains(&item.to_lowercase().as_str()) {
            return None;
        }
        Some(item)
    }

    fn parse_usb_ids(&mut self) {
        self.parsed = true;
        let data = match std::fs::read(&self.path) {
            Ok(d) => d,
            Err(_) => return,
        };
        // latin-1 decode
        let text: String = data.iter().map(|b| *b as char).collect();
        let mut top_key = String::new();
        let mut sub_key = String::new();
        for line in text.split_inclusive('\n') {
            let stripped = line.trim();
            if stripped.is_empty() || stripped.starts_with('#') {
                continue;
            }
            if line.starts_with("\t\t") {
                if sub_key.is_empty() {
                    continue;
                }
                if let Some((id, desc)) = split_once_ws(stripped) {
                    self.usb_info.insert(format!("{}:{}", sub_key, id.to_lowercase()), desc);
                }
            } else if line.starts_with('\t') {
                if top_key.is_empty() {
                    continue;
                }
                if let Some((id, desc)) = split_once_ws(stripped) {
                    sub_key = format!("{}:{}", top_key, id.to_lowercase());
                    self.usb_info.insert(sub_key.clone(), desc);
                }
            } else {
                let l = line.trim_end();
                let (id_type, data) = match split_once_ws(l) {
                    Some(x) => x,
                    None => break,
                };
                if id_type.len() == 4 && u32::from_str_radix(&id_type, 16).is_ok() {
                    top_key = format!("DI:{}", id_type.to_lowercase());
                    self.usb_info.insert(top_key.clone(), data);
                } else if !id_type.is_empty() {
                    match split_once_ws(&data) {
                        Some((num_id, desc)) => {
                            top_key = format!("{}:{}", id_type, num_id.to_lowercase());
                            self.usb_info.insert(top_key.clone(), desc);
                        }
                        None => break,
                    }
                } else {
                    break;
                }
            }
        }
    }

    pub fn get_product_info(&mut self, vendor_id: &str, product_id: &str) -> Value {
        let vendor = match self.get_item(&format!("DI:{}", vendor_id), false) {
            None => return json!({"description": null, "manufacturer": null, "product": null}),
            Some(v) => v,
        };
        let product = self.get_item(&format!("DI:{}:{}", vendor_id, product_id), false);
        json!({
            "description": format!("{} {}", vendor, product.clone().unwrap_or_default()).trim(),
            "manufacturer": vendor,
            "product": product,
        })
    }

    pub fn get_class_info(&mut self, cls_id: &str, subcls_id: &str, proto_id: &str) -> Value {
        let cls = self.get_item(&format!("C:{}", cls_id), false);
        if cls.is_none() || cls_id == "00" {
            return json!({"class": null, "subclass": null, "protocol": null});
        }
        json!({
            "class": cls,
            "subclass": self.get_item(&format!("C:{}:{}", cls_id, subcls_id), true),
            "protocol": self.get_item(&format!("C:{}:{}:{}", cls_id, subcls_id, proto_id), true),
        })
    }
}

fn split_once_ws(s: &str) -> Option<(String, String)> {
    let s = s.trim_start();
    let pos = s.find(char::is_whitespace)?;
    let rest = s[pos..].trim_start();
    if rest.is_empty() {
        return None;
    }
    Some((s[..pos].to_string(), rest.trim_end().to_string()))
}

pub fn find_usb_devices() -> Vec<Map<String, Value>> {
    let dev_folder = Path::new(USB_DEVICE_PATH);
    if !dev_folder.is_dir() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for cfg in crate::utils::path_glob(dev_folder, "*/bDeviceClass") {
        let folder = match cfg.parent() {
            Some(p) => p.to_path_buf(),
            None => continue,
        };
        let r = (|| -> Option<Map<String, Value>> {
            let mut info = Map::new();
            let devnum: i64 = read_item(&folder, "devnum")?.parse().ok()?;
            let busnum: i64 = read_item(&folder, "busnum")?.parse().ok()?;
            info.insert("device_num".into(), json!(devnum));
            info.insert("bus_num".into(), json!(busnum));
            info.insert("vendor_id".into(), json!(read_item(&folder, "idVendor")?.to_lowercase()));
            info.insert("product_id".into(), json!(read_item(&folder, "idProduct")?.to_lowercase()));
            info.insert("usb_location".into(), json!(format!("{}:{}", busnum, devnum)));
            let cls = read_item(&folder, "bDeviceClass")?.to_lowercase();
            let sub = read_item(&folder, "bDeviceSubClass")?.to_lowercase();
            let proto = read_item(&folder, "bDeviceProtocol")?.to_lowercase();
            info.insert("class_ids".into(), json!([cls, sub, proto]));
            for field in OPTIONAL_USB_INFO {
                if folder.join(field).is_file() {
                    info.insert(field.into(), json!(read_item(&folder, field)?));
                } else if !info.contains_key(field) {
                    info.insert(field.into(), Value::Null);
                }
            }
            Some(info)
        })();
        if let Some(i) = r {
            out.push(i);
        }
    }
    out
}

fn link_map(dir: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    let p = Path::new(dir);
    if p.is_dir() {
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                m.insert(resolve_name(&e.path()), e.path().to_string_lossy().to_string());
            }
        }
    }
    m
}

pub fn find_serial_devices() -> Vec<Value> {
    let by_path = link_map(SER_BYPTH_PATH);
    let by_id = link_map(SER_BYID_PATH);
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(TTY_PATH) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for e in rd.flatten() {
        let tty_path = e.path();
        let device_folder = tty_path.join("device");
        if !device_folder.is_dir() {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        let driver_name = resolve_name(&device_folder.join("driver"));
        let mut info = json!({
            "device_type": "unknown",
            "device_path": format!("/dev/{}", name),
            "device_name": name,
            "driver_name": driver_name,
            "path_by_hardware": by_path.get(&name),
            "path_by_id": by_id.get(&name),
            "usb_location": null,
        });
        if tty_path.join("uartclk").is_file() && tty_path.join("port").is_file() {
            if driver_name == "serial8250" {
                let port = read_item(&tty_path, "port")
                    .and_then(|p| i64::from_str_radix(p.trim_start_matches("0x"), 16).ok())
                    .unwrap_or(-1);
                if port != 0 {
                    continue;
                }
            }
            info["device_type"] = json!("hardware_uart");
        } else if let Ok(usb_path) = std::fs::canonicalize(&device_folder) {
            if let Some(loc) = find_usb_folder(usb_path) {
                info["device_type"] = json!("usb");
                info["usb_location"] = json!(loc);
            }
        }
        out.push(info);
    }
    out
}

// ***** V4L2 *****

#[repr(C)]
struct V4l2Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

#[repr(C)]
struct V4l2FmtDesc {
    index: u32,
    typ: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    mbus_code: u32,
    reserved: [u32; 3],
}

#[repr(C)]
struct V4l2FrmSizeEnum {
    index: u32,
    pixel_format: u32,
    typ: u32,
    size: [u32; 6],
    reserved: [u32; 2],
}

const fn ioc(dir: u64, typ: u64, nr: u64, size: u64) -> u64 {
    (dir << 30) | (size << 16) | (typ << 8) | nr
}

const V4L2_CAPS: [(u32, &str); 29] = [
    (0x00000001, "VIDEO_CAPTURE"),
    (0x00000002, "VIDEO_OUTPUT"),
    (0x00000004, "VIDEO_OVERLAY"),
    (0x00000010, "VBI_CAPTURE"),
    (0x00000020, "VBI_OUTPUT"),
    (0x00000040, "SLICED_VBI_CAPTURE"),
    (0x00000080, "SLICED_VBI_OUTPUT"),
    (0x00000100, "RDS_CAPTURE"),
    (0x00000200, "VIDEO_OUTPUT_OVERLAY"),
    (0x00000400, "HW_FREQ_SEEK"),
    (0x00000800, "RDS_OUTPUT"),
    (0x00001000, "VIDEO_CAPTURE_MPLANE"),
    (0x00002000, "VIDEO_OUTPUT_MPLANE"),
    (0x00004000, "VIDEO_M2M_MPLANE"),
    (0x00008000, "VIDEO_M2M"),
    (0x00010000, "TUNER"),
    (0x00020000, "AUDIO"),
    (0x00040000, "RADIO"),
    (0x00080000, "MODULATOR"),
    (0x00100000, "SDR_CAPTURE"),
    (0x00200000, "EXT_PIX_FORMAT"),
    (0x00400000, "SDR_OUTPUT"),
    (0x00800000, "META_CAPTURE"),
    (0x01000000, "READWRITE"),
    (0x04000000, "STREAMING"),
    (0x08000000, "META_OUTPUT"),
    (0x10000000, "TOUCH"),
    (0x20000000, "IO_MC"),
    (0x80000000, "SET_DEVICE_CAPS"),
];

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|c| *c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).to_string()
}

fn fourcc(fmt: u32) -> String {
    let bytes: Vec<u8> = (0..4).map(|i| ((fmt >> (8 * i)) & 0xff) as u8).collect();
    bytes.iter().filter(|b| b.is_ascii()).map(|b| *b as char).collect()
}

fn get_resolutions(fd: i32, pixel_format: u32) -> Vec<String> {
    let req = ioc(3, b'V' as u64, 74, std::mem::size_of::<V4l2FrmSizeEnum>() as u64);
    let mut out = Vec::new();
    for idx in 0..128 {
        let mut info = V4l2FrmSizeEnum { index: idx, pixel_format, typ: 0, size: [0; 6], reserved: [0; 2] };
        if unsafe { libc::ioctl(fd, req as _, &mut info) } < 0 {
            break;
        }
        if info.typ != 1 {
            break;
        }
        out.push(format!("{}x{}", info.size[0], info.size[1]));
    }
    out
}

fn get_modes(fd: i32) -> Vec<Value> {
    let req = ioc(3, b'V' as u64, 2, std::mem::size_of::<V4l2FmtDesc>() as u64);
    let mut out = Vec::new();
    for idx in 0..128 {
        let mut info = V4l2FmtDesc { index: idx, typ: 1, flags: 0, description: [0; 32], pixelformat: 0, mbus_code: 0, reserved: [0; 3] };
        if unsafe { libc::ioctl(fd, req as _, &mut info) } < 0 {
            break;
        }
        let resolutions = get_resolutions(fd, info.pixelformat);
        if resolutions.is_empty() {
            continue;
        }
        let mut flags = Vec::new();
        if info.flags & 1 != 0 {
            flags.push("COMPRESSED");
        }
        if info.flags & 2 != 0 {
            flags.push("EMULATED");
        }
        out.push(json!({
            "format": fourcc(info.pixelformat),
            "description": cstr(&info.description),
            "flags": flags,
            "resolutions": resolutions,
        }));
    }
    out
}

pub fn find_video_devices() -> Vec<Value> {
    let v4lpath = Path::new(V4L_DEVICE_PATH);
    if !v4lpath.is_dir() {
        return Vec::new();
    }
    let by_path = link_map(V4L_BYPTH_PATH);
    let by_id = link_map(V4L_BYID_PATH);
    let mut devs: Vec<Value> = Vec::new();
    let rd = match std::fs::read_dir(v4lpath) {
        Ok(r) => r,
        Err(_) => return devs,
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let devfs_path = format!("/dev/{}", name);
        let cpath = match std::ffi::CString::new(devfs_path.clone()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
        if fd < 0 {
            continue;
        }
        let mut cap: V4l2Capability = unsafe { std::mem::zeroed() };
        let req = ioc(2, b'V' as u64, 0, std::mem::size_of::<V4l2Capability>() as u64);
        let ok = unsafe { libc::ioctl(fd, req as _, &mut cap) } >= 0;
        if !ok || cap.device_caps & 1 == 0 {
            unsafe { libc::close(fd) };
            continue;
        }
        let modes = get_modes(fd);
        unsafe { libc::close(fd) };
        let caps: Vec<&str> = V4L2_CAPS.iter().filter(|(b, _)| cap.device_caps & b != 0).map(|(_, n)| *n).collect();
        let ver: Vec<String> = [16, 8, 0].iter().map(|s| ((cap.version >> s) & 0xff).to_string()).collect();
        let mut dev = json!({
            "device_name": name,
            "device_path": devfs_path,
            "camera_name": cstr(&cap.card),
            "driver_name": cstr(&cap.driver),
            "hardware_bus": cstr(&cap.bus_info),
            "capabilities": caps,
            "version": ver.join("."),
            "path_by_hardware": by_path.get(&name),
            "path_by_id": by_id.get(&name),
            "alt_name": null,
            "usb_location": null,
            "modes": modes,
        });
        if let Some(n) = read_item(&e.path(), "name") {
            dev["alt_name"] = json!(n);
        }
        let device_path = e.path().join("device");
        if device_path.is_dir() {
            if let Ok(r) = std::fs::canonicalize(&device_path) {
                if let Some(loc) = find_usb_folder(r) {
                    dev["usb_location"] = json!(loc);
                }
            }
        }
        devs.push(dev);
    }
    devs.sort_by(|a, b| a["device_name"].as_str().cmp(&b["device_name"].as_str()));
    let idx = |v: &Value| -> i64 { v["device_name"].as_str().and_then(|s| s.get(5..)).and_then(|s| s.parse().ok()).unwrap_or(-1) };
    devs.sort_by_key(idx);
    devs
}

// ***** CAN *****

const CAN_RAW: i32 = 1;
const KLIPPER_ADMIN_ID: u32 = 0x3f0;
const CMD_QUERY_UNASSIGNED: u8 = 0x00;
const CANBUS_RESP_NEED_NODEID: u8 = 0x20;

/// Queries unassigned klipper/katapult nodes on a CAN interface
pub async fn query_can_uuids(interface: &str) -> Result<Vec<Value>, crate::common::ServerError> {
    let err = || crate::common::ServerError::new(format!("Unable to bind socket to interface '{}'", interface), 500);
    let fd = unsafe { libc::socket(libc::PF_CAN, libc::SOCK_RAW | libc::SOCK_NONBLOCK, CAN_RAW) };
    if fd < 0 {
        return Err(err());
    }
    let cname = std::ffi::CString::new(interface).map_err(|_| err())?;
    let ifindex = unsafe { libc::if_nametoindex(cname.as_ptr()) };
    let mut addr: libc::sockaddr_can = unsafe { std::mem::zeroed() };
    addr.can_family = libc::AF_CAN as u16;
    addr.can_ifindex = ifindex as i32;
    let r = unsafe {
        libc::bind(fd, &addr as *const _ as *const libc::sockaddr, std::mem::size_of::<libc::sockaddr_can>() as u32)
    };
    if ifindex == 0 || r < 0 {
        unsafe { libc::close(fd) };
        return Err(err());
    }
    // owned, so the socket is closed on every return and on cancellation
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    // SAFETY: the OwnedFd keeps fd open for the AsyncFd's lifetime
    let async_fd = unsafe { tokio::io::unix::AsyncFd::register(owned) }.map_err(|_| err())?;
    // send query frame
    let mut frame = [0u8; 16];
    frame[0..4].copy_from_slice(&KLIPPER_ADMIN_ID.to_le_bytes());
    frame[4] = 1;
    frame[8] = CMD_QUERY_UNASSIGNED;
    unsafe { libc::write(fd, frame.as_ptr() as *const libc::c_void, 16) };
    let mut uuids = Vec::new();
    let mut buffer: Vec<u8> = Vec::new();
    let end = crate::utils::monotonic() + 2.0;
    loop {
        let now = crate::utils::monotonic();
        if now >= end {
            break;
        }
        let timeout = (end - now).max(0.1);
        let readable = tokio::time::timeout(std::time::Duration::from_secs_f64(timeout), async_fd.readable()).await;
        let mut guard = match readable {
            Ok(Ok(g)) => g,
            _ => continue,
        };
        let mut buf = [0u8; 4096];
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        guard.clear_ready();
        if n <= 0 {
            continue;
        }
        for pkt in buf[..n as usize].chunks(16) {
            if pkt.len() < 16 {
                continue;
            }
            let can_id = u32::from_le_bytes(pkt[0..4].try_into().unwrap()) & 0x1FFFFFFF;
            let len = (pkt[4] as usize).min(8);
            if can_id == KLIPPER_ADMIN_ID + 1 {
                buffer.extend_from_slice(&pkt[8..8 + len]);
            }
        }
        while buffer.len() >= 8 || (!buffer.is_empty() && buffer.len() >= 7) {
            let take = buffer.len().min(8);
            let resp: Vec<u8> = buffer.drain(..take).collect();
            if resp.len() < 7 || resp[0] != CANBUS_RESP_NEED_NODEID {
                continue;
            }
            let app = if resp.len() > 7 {
                match resp[7] {
                    0x01 => "Klipper",
                    0x11 => "Katapult",
                    _ => "Unknown",
                }
            } else {
                "Unknown"
            };
            uuids.push(json!({"uuid": hex::encode(&resp[1..7]), "application": app}));
        }
    }
    drop(async_fd);
    Ok(uuids)
}

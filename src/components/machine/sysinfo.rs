// System information: distro emulation (the distro package), cpu, sd card and network

use super::*;

// ***** distro emulation *****

pub(crate) fn parse_os_release() -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    for path in ["/etc/os-release", "/usr/lib/os-release"] {
        if let Ok(data) = std::fs::read_to_string(path) {
            for line in data.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = line.split_once('=') {
                    // a line shlex cannot split is skipped
                    let Ok(v) = crate::components::shell_command::shlex_split(v) else { continue };
                    let v = v.join(" ");
                    out.insert(k.to_lowercase(), v);
                }
            }
            break;
        }
    }
    if let Some(v) = out.get("version").cloned() {
        let re = Regex::new(r"\((\D+)\)|,\s*(\D+)$").unwrap();
        if let Some(c) = re.captures(&v) {
            let cn = c.get(1).or(c.get(2)).map(|m| m.as_str().trim().to_string()).unwrap_or_default();
            out.entry("codename".into()).or_insert(cn);
        }
    }
    if let Some(cn) = out.get("version_codename").or(out.get("ubuntu_codename")).cloned() {
        out.insert("codename".into(), cn);
    }
    out
}

pub(super) fn distro_release_info() -> Value {
    let ignore = [
        "debian_version", "lsb-release", "oem-release", "os-release", "system-release", "plesk-release", "iredmail-release",
        "board-release", "ec2_version",
    ];
    let mut files: Vec<PathBuf> = crate::utils::path_glob(Path::new("/etc"), "*-release");
    files.extend(crate::utils::path_glob(Path::new("/etc"), "*_release"));
    files.extend(crate::utils::path_glob(Path::new("/etc"), "*-version"));
    files.extend(crate::utils::path_glob(Path::new("/etc"), "*_version"));
    files.sort();
    let re = Regex::new(r"(?:Linux)?\s*(?P<name>.+?)\s*release\s*(?P<version_id>[\d.]+)[^(]*(?:\((?P<codename>[^)]*)\))?").unwrap();
    for f in files {
        let fname = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if ignore.contains(&fname.as_str()) {
            continue;
        }
        if let Ok(data) = std::fs::read_to_string(&f) {
            let line = data.lines().next().unwrap_or("").trim().to_string();
            let id = fname.split(|c| c == '-' || c == '_').next().unwrap_or("").to_string();
            let mut info = Map::new();
            if let Some(c) = re.captures(&line) {
                info.insert("name".into(), json!(c.name("name").map(|m| m.as_str()).unwrap_or("")));
                info.insert("version_id".into(), json!(c.name("version_id").map(|m| m.as_str()).unwrap_or("")));
                if let Some(cn) = c.name("codename") {
                    info.insert("codename".into(), json!(cn.as_str()));
                }
            } else if !line.is_empty() {
                info.insert("name".into(), json!(line));
            }
            if !info.is_empty() {
                info.insert("id".into(), json!(id));
                return Value::Object(info);
            }
        }
    }
    json!({})
}

pub(super) fn dist_info() -> Map<String, Value> {
    let osr = parse_os_release();
    let pretty = osr.get("pretty_name").or(osr.get("name")).cloned().unwrap_or_default();
    let id = osr.get("id").cloned().unwrap_or_default().to_lowercase().replace(' ', "_");
    let version = osr.get("version_id").cloned().unwrap_or_default();
    let vre = Regex::new(r"(\d+)\.?(\d+)?\.?(\d+)?").unwrap();
    let (major, minor, build) = match vre.captures(&version) {
        Some(c) => (
            c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default(),
            c.get(2).map(|m| m.as_str().to_string()).unwrap_or_default(),
            c.get(3).map(|m| m.as_str().to_string()).unwrap_or_default(),
        ),
        None => (String::new(), String::new(), String::new()),
    };
    let mut m = Map::new();
    m.insert("name".into(), json!(pretty));
    m.insert("id".into(), json!(id));
    m.insert("version".into(), json!(version));
    m.insert("version_parts".into(), json!({"major": major, "minor": minor, "build_number": build}));
    m.insert("like".into(), json!(osr.get("id_like").cloned().unwrap_or_default()));
    m.insert("codename".into(), json!(osr.get("codename").cloned().unwrap_or_default()));
    m.insert("release_info".into(), distro_release_info());
    m.insert("kernel_version".into(), json!(kernel_release()));
    m
}

pub(super) fn kernel_release() -> String {
    // SAFETY: a C struct of byte arrays: all zero is a valid value
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: uts is exclusively borrowed for uname to fill; after success release is NUL-terminated within the array
    unsafe {
        if libc::uname(&mut uts) == 0 {
            return std::ffi::CStr::from_ptr(uts.release.as_ptr()).to_string_lossy().to_string();
        }
    }
    String::new()
}

pub(super) fn machine_arch() -> String {
    // SAFETY: a C struct of byte arrays: all zero is a valid value
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: uts is exclusively borrowed for uname to fill; after success machine is NUL-terminated within the array
    unsafe {
        if libc::uname(&mut uts) == 0 {
            return std::ffi::CStr::from_ptr(uts.machine.as_ptr()).to_string_lossy().to_string();
        }
    }
    String::new()
}

pub(super) fn native_network() -> Option<Map<String, Value>> {
    let sys = |ifname: &str, f: &str| std::fs::read_to_string(format!("/sys/class/net/{}/{}", ifname, f)).ok().map(|s| s.trim().to_string());
    let mut ifaces: Vec<(i64, String)> = std::fs::read_dir("/sys/class/net")
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| sys(n, "operstate").as_deref() == Some("up"))
        .map(|n| (sys(&n, "ifindex").and_then(|i| i.parse().ok()).unwrap_or(0), n))
        .collect();
    ifaces.sort();
    let mut addrs: IndexMap<String, Vec<Value>> = IndexMap::new();
    // SAFETY: getifaddrs fills head with a list the loop only reads (each ifa_addr is checked for NULL and read as the sockaddr its family says), freed once by freeifaddrs after the walk
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return None;
        }
        let mut cur = head;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy().to_string();
            let entry = match (*ifa.ifa_addr).sa_family as i32 {
                libc::AF_INET => {
                    let a = std::net::Ipv4Addr::from(u32::from_be((*(ifa.ifa_addr as *const libc::sockaddr_in)).sin_addr.s_addr));
                    json!({"family": "ipv4", "address": a.to_string(), "is_link_local": a.is_link_local()})
                }
                libc::AF_INET6 => {
                    let a = std::net::Ipv6Addr::from((*(ifa.ifa_addr as *const libc::sockaddr_in6)).sin6_addr.s6_addr);
                    json!({"family": "ipv6", "address": a.to_string(), "is_link_local": a.segments()[0] & 0xffc0 == 0xfe80})
                }
                _ => continue,
            };
            addrs.entry(name).or_default().push(entry);
        }
        libc::freeifaddrs(head);
    }
    let mut network = Map::new();
    for (_, ifname) in ifaces {
        match sys(&ifname, "type").as_deref() {
            Some("280") => return None,
            Some("1") => {}
            _ => continue,
        }
        let ips = addrs.shift_remove(&ifname).unwrap_or_default();
        let mac = sys(&ifname, "address");
        if ips.is_empty() || mac.is_none() {
            continue;
        }
        network.insert(ifname, json!({"mac_address": mac, "ip_addresses": ips}));
    }
    Some(network)
}

pub(super) fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
        None => String::new(),
    }
}

pub(super) fn is_global_v6(ip: &std::net::Ipv6Addr) -> bool {
    let seg = ip.segments();
    !(ip.is_loopback() || ip.is_unspecified() || (seg[0] & 0xfe00) == 0xfc00 || (seg[0] & 0xffc0) == 0xfe80 || (seg[0] == 0x2001 && seg[1] == 0xdb8))
        && (seg[0] & 0xe000) == 0x2000
}

pub(super) fn python_version() -> (Value, String) {
    let out = std::process::Command::new("python3")
        .args(["-c", "import sys,json;print(json.dumps([list(sys.version_info), sys.version.replace(chr(10), ' ')]))"])
        .output();
    if let Ok(o) = out {
        if let Ok(v) = serde_json::from_slice::<Value>(&o.stdout) {
            if let Some(a) = v.as_array() {
                if a.len() == 2 {
                    return (a[0].clone(), py_str(&a[1]));
                }
            }
        }
    }
    (json!([0, 0, 0, "final", 0]), "unknown".into())
}

pub(super) fn get_cpu_info() -> Value {
    let arch = machine_arch();
    let bits = if std::mem::size_of::<usize>() == 8 { "64bit" } else { "32bit" };
    let mut info = json!({
        "cpu_count": std::thread::available_parallelism().map(|n| n.get()).ok(),
        "bits": bits,
        "processor": arch,
        "cpu_desc": "",
        "serial_number": "",
        "hardware_desc": "",
        "model": "",
        "total_memory": null,
        "memory_units": "",
    });
    if let Ok(cnt) = std::fs::read_to_string("/proc/cpuinfo") {
        let items: Vec<&str> = cnt.trim().split("\n\n").map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
        let desc_re = Regex::new(r"model name\s+:\s+(.+)").unwrap();
        for item in &items {
            if let Some(c) = desc_re.captures(item) {
                info["cpu_desc"] = json!(c[1].trim());
                break;
            }
        }
        if let Some(last) = items.last() {
            if let Some(c) = Regex::new(r"Hardware\s+:\s+(.+)").unwrap().captures(last) {
                info["hardware_desc"] = json!(c[1].trim());
            }
            if let Some(c) = Regex::new(r"Serial\s+:\s+0*(.+)").unwrap().captures(last) {
                info["serial_number"] = json!(c[1].trim());
            }
            if let Some(c) = Regex::new(r"Model\s+:\s+(.+)").unwrap().captures(last) {
                info["model"] = json!(c[1].trim());
            }
        }
    }
    if let Ok(mem) = std::fs::read_to_string("/proc/meminfo") {
        for line in mem.trim().split('\n') {
            let line = line.trim();
            if line.starts_with("MemTotal:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 3 {
                    info["total_memory"] = json!(parts[1].parse::<i64>().unwrap_or(0));
                    info["memory_units"] = json!(parts[2]);
                }
                break;
            }
        }
    }
    info
}

pub(super) fn get_sdcard_info() -> Value {
    let cid = match std::fs::read_to_string(SD_CID_PATH) {
        Ok(c) => c.trim().to_lowercase(),
        Err(_) => return json!({}),
    };
    let parse = || -> Option<Map<String, Value>> {
        let mut m = Map::new();
        let mid = cid.get(..2)?.to_string();
        m.insert("manufacturer_id".into(), json!(mid));
        m.insert(
            "manufacturer".into(),
            json!(match mid.as_str() {
                "1b" => "Samsung",
                "03" => "Sandisk",
                "74" => "PNY",
                _ => "Unknown",
            }),
        );
        m.insert("oem_id".into(), json!(cid.get(2..6)?));
        let pn = hex::decode(cid.get(6..16)?).ok()?;
        m.insert("product_name".into(), json!(pn.iter().filter(|b| b.is_ascii()).map(|b| *b as char).collect::<String>()));
        let r1 = u8::from_str_radix(cid.get(16..17)?, 16).ok()?;
        let r2 = u8::from_str_radix(cid.get(17..18)?, 16).ok()?;
        m.insert("product_revision".into(), json!(format!("{}.{}", r1, r2)));
        m.insert("serial_number".into(), json!(cid.get(18..26)?));
        let year = i64::from_str_radix(cid.get(27..29)?, 16).ok()? + 2000;
        let month = i64::from_str_radix(cid.get(29..30)?, 16).ok()?;
        m.insert("manufacturer_date".into(), json!(format!("{}/{}", month, year)));
        Some(m)
    };
    let mut sd = match parse() {
        Some(m) => m,
        None => {
            log::info!("Error reading SDCard CID Register");
            return json!({});
        }
    };
    sd.insert("capacity".into(), json!("Unknown"));
    sd.insert("total_bytes".into(), json!(0));
    let csd = std::fs::read_to_string(SD_CSD_PATH).ok().and_then(|c| hex::decode(c.trim()).ok());
    match csd {
        Some(r) if r.len() >= 11 => {
            let t = (r[0] >> 6) & 0x3;
            let (total, cap): (u64, String) = match t {
                0 => {
                    let max_block_len = 2u64.pow((r[5] & 0xF) as u32);
                    let c_size = (((r[6] & 0x3) as u64) << 10) | ((r[7] as u64) << 2) | (((r[8] >> 6) & 0x3) as u64);
                    let c_mult_reg = (((r[9] & 0x3) as u32) << 1) | ((r[10] >> 7) as u32);
                    let total = (c_size + 1) * 2u64.pow(c_mult_reg + 2) * max_block_len;
                    (total, format!("{:.1} MiB", total as f64 / 1024f64.powi(2)))
                }
                1 => {
                    let c_size = (((r[7] & 0x3F) as u64) << 16) | ((r[8] as u64) << 8) | r[9] as u64;
                    let total = (c_size + 1) * 512 * 1024;
                    (total, format!("{:.1} GiB", total as f64 / 1024f64.powi(3)))
                }
                2 => {
                    let c_size = (((r[6] & 0xF) as u64) << 24) | ((r[7] as u64) << 16) | ((r[8] as u64) << 8) | r[9] as u64;
                    let total = (c_size + 1) * 512 * 1024;
                    (total, format!("{:.1} TiB", total as f64 / 1024f64.powi(4)))
                }
                _ => return Value::Object(sd),
            };
            sd.insert("capacity".into(), json!(cap));
            sd.insert("total_bytes".into(), json!(total));
        }
        _ => log::info!("Error Reading SDCard CSD Register"),
    }
    Value::Object(sd)
}

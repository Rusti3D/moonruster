// Moonraker Process Stat Tracking (port of moonraker/components/proc_stats.py)

use crate::common::*;
use crate::confighelper::ConfigHelper;
use crate::pyfmt::py_round;
use crate::server::{server, spawn, Component};
use crate::utils::float_value;
use async_trait::async_trait;
use futures::future::LocalBoxFuture;
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;

const VC_GEN_CMD_FILE: &str = "/usr/bin/vcgencmd";
const VCIO_PATH: &str = "/dev/vcio";
const STATM_FILE_PATH: &str = "/proc/self/statm";
const NET_DEV_PATH: &str = "/proc/net/dev";
const TEMPERATURE_PATH: &str = "/sys/class/thermal/thermal_zone0/temp";
const HWMON_ROOT_PATH: &str = "/sys/class/hwmon/";
const CPU_STAT_PATH: &str = "/proc/stat";
const MEM_AVAIL_PATH: &str = "/proc/meminfo";
const STAT_UPDATE_TIME: f64 = 1.0;
const REPORT_QUEUE_SIZE: usize = 30;
const THROTTLE_CHECK_INTERVAL: u64 = 10;
const WATCHDOG_REFRESH_TIME: f64 = 2.0;
const REPORT_BLOCKED_TIME: f64 = 4.0;

const THROTTLED_FLAGS: [(u64, &str); 8] = [
    (1, "Under-Voltage Detected"),
    (1 << 1, "Frequency Capped"),
    (1 << 2, "Currently Throttled"),
    (1 << 3, "Temperature Limit Active"),
    (1 << 16, "Previously Under-Volted"),
    (1 << 17, "Previously Frequency Capped"),
    (1 << 18, "Previously Throttled"),
    (1 << 19, "Previously Temperature Limited"),
];

pub type StatCallback = Rc<dyn Fn(u64) -> Option<LocalBoxFuture<'static, ()>>>;

fn process_time() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: ts is a valid timespec, exclusively borrowed for clock_gettime to fill
    unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

fn page_size_kb() -> i64 {
    // SAFETY: sysconf takes no pointers
    (unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as i64 / 1024).max(1)
}

fn boot_time() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: ts is a valid timespec, exclusively borrowed for clock_gettime to fill
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

pub struct ProcStats {
    vcgencmd: bool,
    vc_err_logged: Cell<bool>,
    temp_file: PathBuf,
    proc_stat_queue: RefCell<VecDeque<Value>>,
    last_update_time: Cell<f64>,
    last_proc_time: Cell<f64>,
    throttle_lock: tokio::sync::Mutex<()>,
    total_throttled: Cell<u64>,
    last_throttled: Cell<u64>,
    update_sequence: Cell<u64>,
    last_net_stats: RefCell<Map<String, Value>>,
    last_cpu_stats: RefCell<HashMap<String, (i64, i64)>>,
    cpu_usage: RefCell<Map<String, Value>>,
    memory_usage: RefCell<Map<String, Value>>,
    stat_callbacks: RefCell<Vec<StatCallback>>,
    tasks: RefCell<Vec<tokio::task::JoinHandle<()>>>,
    blocked_count: Cell<u64>,
}

#[async_trait(?Send)]
impl Component for ProcStats {
    fn has_init(&self) -> bool {
        true
    }
    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        let p = Rc::downgrade(&self);
        let stat = spawn(async move {
            loop {
                match p.upgrade() {
                    Some(ps) => ps.handle_stat_update(crate::utils::monotonic()).await,
                    None => return,
                }
                tokio::time::sleep(std::time::Duration::from_secs_f64(STAT_UPDATE_TIME)).await;
            }
        });
        let p = Rc::downgrade(&self);
        let wd = spawn(async move {
            let mut last = crate::utils::monotonic();
            loop {
                tokio::time::sleep(std::time::Duration::from_secs_f64(WATCHDOG_REFRESH_TIME)).await;
                let now = crate::utils::monotonic();
                let diff = now - last;
                if diff > REPORT_BLOCKED_TIME {
                    if let Some(ps) = p.upgrade() {
                        ps.blocked_count.set(ps.blocked_count.get() + 1);
                        log::info!(
                            "EVENT LOOP BLOCKED: {} seconds, total blocked count: {}",
                            crate::pyfmt::py_float_repr(py_round(diff, 2)),
                            ps.blocked_count.get()
                        );
                        let p2 = Rc::downgrade(&ps);
                        spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                            if let Some(ps) = p2.upgrade() {
                                ps.log_last_stats(5);
                            }
                        });
                    }
                }
                last = now;
            }
        });
        self.tasks.borrow_mut().push(stat);
        self.tasks.borrow_mut().push(wd);
        Ok(())
    }
    async fn close(self: Rc<Self>) {
        for t in self.tasks.borrow_mut().drain(..) {
            t.abort();
        }
    }
}

impl ProcStats {
    pub fn new(_config: &ConfigHelper) -> Result<Rc<Self>, crate::confighelper::ConfigError> {
        let srv = server();
        let vcgencmd = Path::new(VC_GEN_CMD_FILE).exists() && Path::new(VCIO_PATH).exists();
        if vcgencmd {
            log::info!("Detected 'vcgencmd', throttle checking enabled");
            srv.register_notification("proc_stats:cpu_throttled", None);
        } else {
            log::info!("Unable to find 'vcgencmd', throttle checking disabled");
        }
        let ps = Rc::new(ProcStats {
            vcgencmd,
            vc_err_logged: Cell::new(false),
            temp_file: PathBuf::from(Self::get_cpu_thermal_file()),
            proc_stat_queue: RefCell::new(VecDeque::new()),
            last_update_time: Cell::new(crate::utils::time_now()),
            last_proc_time: Cell::new(process_time()),
            throttle_lock: tokio::sync::Mutex::new(()),
            total_throttled: Cell::new(0),
            last_throttled: Cell::new(0),
            update_sequence: Cell::new(0),
            last_net_stats: RefCell::new(Map::new()),
            last_cpu_stats: RefCell::new(HashMap::new()),
            cpu_usage: RefCell::new(Map::new()),
            memory_usage: RefCell::new(Map::new()),
            stat_callbacks: RefCell::new(Vec::new()),
            tasks: RefCell::new(Vec::new()),
            blocked_count: Cell::new(0),
        });
        let p = ps.clone();
        srv.register_endpoint("/machine/proc_stats", RequestType::GET, callback(move |_req| {
            let p = p.clone();
            async move { p.handle_stat_request().await }
        }))?;
        let p = Rc::downgrade(&ps);
        srv.on_event_async("server:klippy_shutdown", move |_| {
            let p = p.clone();
            async move {
                if let Some(ps) = p.upgrade() {
                    ps.handle_shutdown().await;
                }
            }
        });
        srv.register_notification("proc_stats:proc_stat_update", None);
        Ok(ps)
    }

    pub fn register_stat_callback(&self, cb: StatCallback) {
        self.stat_callbacks.borrow_mut().push(cb);
    }

    async fn handle_stat_request(&self) -> ApiResult {
        let ts = if self.vcgencmd { self.check_throttled_state().await } else { Value::Null };
        Ok(json!({
            "moonraker_stats": self.proc_stat_queue.borrow().iter().cloned().collect::<Vec<_>>(),
            "throttled_state": ts,
            "cpu_temp": self.get_cpu_temperature(),
            "network": Value::Object(self.last_net_stats.borrow().clone()),
            "system_cpu_usage": Value::Object(self.cpu_usage.borrow().clone()),
            "system_uptime": float_value(boot_time()),
            "system_memory": Value::Object(self.memory_usage.borrow().clone()),
            "websocket_connections": server().wsm().get_count(),
        }))
    }

    async fn handle_shutdown(&self) {
        let mut msg = String::from("\nMoonraker System Usage Statistics:");
        for s in self.proc_stat_queue.borrow().iter() {
            msg.push_str(&format!("\n{}", Self::format_stats(s)));
        }
        msg.push_str(&format!("\nCPU Temperature: {}", crate::pyfmt::py_str(&self.get_cpu_temperature())));
        log::info!("{}", msg);
        if self.vcgencmd {
            let ts = self.check_throttled_state().await;
            let flags: Vec<String> = ts["flags"].as_array().map(|a| a.iter().map(crate::pyfmt::py_str).collect()).unwrap_or_default();
            log::info!("Throttled Flags: {}", flags.join(" "));
        }
    }

    async fn handle_stat_update(&self, eventtime: f64) {
        let proc_time = process_time();
        let time_diff = eventtime - self.last_update_time.get();
        let usage = py_round((proc_time - self.last_proc_time.get()) / time_diff * 100.0, 2);
        let (mem, mem_units) = Self::get_memory_usage();
        let cpu_temp = self.get_cpu_temperature();
        let mut net = Self::get_net_stats();
        self.update_cpu_stats();
        self.update_system_memory();
        {
            let last = self.last_net_stats.borrow();
            for (dev, stats) in net.iter_mut() {
                let mut bytes_sec = 0.0;
                if let Some(l) = last.get(dev) {
                    let cur = stats["rx_bytes"].as_i64().unwrap_or(0) + stats["tx_bytes"].as_i64().unwrap_or(0);
                    let lt = l["rx_bytes"].as_i64().unwrap_or(0) + l["tx_bytes"].as_i64().unwrap_or(0);
                    bytes_sec = py_round((cur - lt) as f64 / time_diff, 2);
                }
                stats["bandwidth"] = float_value(bytes_sec);
            }
        }
        *self.last_net_stats.borrow_mut() = net.clone();
        let result = json!({
            "time": crate::utils::time_now(),
            "cpu_usage": float_value(usage),
            "memory": mem,
            "mem_units": mem_units,
        });
        {
            let mut q = self.proc_stat_queue.borrow_mut();
            q.push_back(result.clone());
            while q.len() > REPORT_QUEUE_SIZE {
                q.pop_front();
            }
        }
        let srv = server();
        let _ = srv.send_event(
            "proc_stats:proc_stat_update",
            vec![json!({
                "moonraker_stats": result,
                "cpu_temp": cpu_temp,
                "network": Value::Object(net),
                "system_cpu_usage": Value::Object(self.cpu_usage.borrow().clone()),
                "system_memory": Value::Object(self.memory_usage.borrow().clone()),
                "websocket_connections": srv.wsm().get_count(),
            })],
        );
        if self.update_sequence.get() % THROTTLE_CHECK_INTERVAL == 0 && self.vcgencmd {
            let ts = self.check_throttled_state().await;
            let cur = ts["bits"].as_u64().unwrap_or(0);
            if cur & !self.total_throttled.get() != 0 {
                srv.add_log_rollover_item("throttled", &format!("CPU Throttled Flags: {}", crate::pyfmt::py_repr(&ts["flags"])), true);
            }
            if cur != self.last_throttled.get() {
                let _ = srv.send_event("proc_stats:cpu_throttled", vec![ts.clone()]);
            }
            self.last_throttled.set(cur);
            self.total_throttled.set(self.total_throttled.get() | cur);
        }
        let cbs: Vec<StatCallback> = self.stat_callbacks.borrow().clone();
        for cb in cbs {
            if let Some(f) = cb(self.update_sequence.get()) {
                f.await;
            }
        }
        self.last_update_time.set(eventtime);
        self.last_proc_time.set(proc_time);
        self.update_sequence.set(self.update_sequence.get() + 1);
    }

    async fn check_throttled_state(&self) -> Value {
        let mut ret = json!({"bits": 0, "flags": ["?"]});
        if self.vcgencmd {
            let _g = self.throttle_lock.lock().await;
            let resp = self.vcgencmd_run("get_throttled");
            if let Some(hex) = resp.trim().rsplit('=').next() {
                if let Ok(tstate) = u64::from_str_radix(hex.trim().trim_start_matches("0x"), 16) {
                    ret["bits"] = json!(tstate);
                    let flags: Vec<Value> = THROTTLED_FLAGS.iter().filter(|(f, _)| f & tstate != 0).map(|(_, d)| json!(d)).collect();
                    ret["flags"] = Value::Array(flags);
                }
            }
        }
        ret
    }

    /// Queries the BCM2835 mailbox directly (VCGenCmd)
    fn vcgencmd_run(&self, cmd: &str) -> String {
        const MAX_STRING_SIZE: usize = 1024;
        const GET_RESULT_CMD: u32 = 0x00030080;
        let size = 6 * 4 + MAX_STRING_SIZE + 4;
        let mut buf = vec![0u8; size];
        let put = |buf: &mut Vec<u8>, off: usize, v: u32| buf[off..off + 4].copy_from_slice(&v.to_ne_bytes());
        put(&mut buf, 0, size as u32);
        put(&mut buf, 4, 0);
        put(&mut buf, 8, GET_RESULT_CMD);
        put(&mut buf, 12, MAX_STRING_SIZE as u32);
        put(&mut buf, 16, 0);
        put(&mut buf, 20, 0);
        let cb = cmd.as_bytes();
        buf[24..24 + cb.len().min(MAX_STRING_SIZE)].copy_from_slice(&cb[..cb.len().min(MAX_STRING_SIZE)]);
        let path = std::ffi::CString::new(VCIO_PATH).unwrap();
        // SAFETY: path is a NUL-terminated CString alive for the call
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
        if fd < 0 {
            if !self.vc_err_logged.get() {
                log::error!("VCIO vcgencmd failed");
                self.vc_err_logged.set(true);
            }
            return String::new();
        }
        // _IOWR(100, 0, char *)
        let req: libc::c_ulong = (3 << 30) | ((std::mem::size_of::<*const u8>() as libc::c_ulong) << 16) | (100 << 8);
        // SAFETY: fd is the open vcio device; the mailbox ioctl reads and writes the property buffer `buf`, whose length is in its first word
        let ret = unsafe { libc::ioctl(fd, req as _, buf.as_mut_ptr()) };
        // SAFETY: fd was opened above and is closed once, after its last use
        unsafe { libc::close(fd) };
        if ret < 0 {
            if !self.vc_err_logged.get() {
                log::error!("VCIO vcgencmd failed");
                self.vc_err_logged.set(true);
            }
            return String::new();
        }
        let rc = u32::from_ne_bytes(buf[20..24].try_into().unwrap());
        if rc != 0 {
            log::info!("vcgencmd returned {}", rc);
        }
        let resp = &buf[24..24 + MAX_STRING_SIZE];
        match resp.iter().position(|b| *b == 0) {
            Some(0) | None => String::new(),
            Some(i) => String::from_utf8_lossy(&resp[..i]).to_string(),
        }
    }

    /// Rss from /proc/self/statm: the same counters as smaps_rollup's Rss
    /// without the kernel walking every mapping each second
    fn get_memory_usage() -> (Value, Value) {
        let resident = std::fs::read_to_string(STATM_FILE_PATH)
            .ok()
            .and_then(|d| d.split_whitespace().nth(1).and_then(|r| r.parse::<i64>().ok()));
        match resident {
            Some(pages) => (json!(pages * page_size_kb()), json!("kB")),
            None => (Value::Null, Value::Null),
        }
    }

    fn get_cpu_temperature(&self) -> Value {
        match std::fs::read_to_string(&self.temp_file).ok().and_then(|s| s.trim().parse::<i64>().ok()) {
            Some(v) => float_value(v as f64 / 1000.0),
            None => Value::Null,
        }
    }

    fn get_net_stats() -> Map<String, Value> {
        let mut out = Map::new();
        let data = match std::fs::read_to_string(NET_DEV_PATH) {
            Ok(d) => d,
            Err(_) => return out,
        };
        // re.findall(r"(\w+):(.+)"), without a regex every second
        for line in data.lines() {
            let (pre, rest) = match line.split_once(':') {
                Some(x) => x,
                None => continue,
            };
            let start = pre.rfind(|c: char| !(c.is_alphanumeric() || c == '_')).map(|i| i + 1).unwrap_or(0);
            let name = &pre[start..];
            if name.is_empty() || rest.is_empty() {
                continue;
            }
            let parts: Vec<i64> = rest.split_whitespace().map(|p| p.parse().unwrap_or(0)).collect();
            if parts.len() < 12 {
                return Map::new();
            }
            out.insert(
                name.to_string(),
                json!({
                    "rx_bytes": parts[0], "tx_bytes": parts[8],
                    "rx_packets": parts[1], "tx_packets": parts[9],
                    "rx_errs": parts[2], "tx_errs": parts[10],
                    "rx_drop": parts[3], "tx_drop": parts[11],
                }),
            );
        }
        out
    }

    fn update_system_memory(&self) {
        let data = match std::fs::read_to_string(MEM_AVAIL_PATH) {
            Ok(d) => d,
            Err(_) => return,
        };
        let field = |key: &str| {
            data.lines()
                .find_map(|l| l.strip_prefix(key))
                .and_then(|v| v.split_whitespace().next())
                .and_then(|v| v.parse::<i64>().ok())
        };
        let (t, a) = (field("MemTotal:"), field("MemAvailable:"));
        if let (Some(t), Some(a)) = (t, a) {
            let mut m = self.memory_usage.borrow_mut();
            m.insert("total".into(), json!(t));
            m.insert("available".into(), json!(a));
            m.insert("used".into(), json!(t - a));
        }
    }

    fn update_cpu_stats(&self) {
        let data = match std::fs::read_to_string(CPU_STAT_PATH) {
            Ok(d) => d,
            Err(_) => return,
        };
        let mut usage = Map::new();
        let mut last = self.last_cpu_stats.borrow_mut();
        for line in data.lines().filter(|l| l.contains("cpu")) {
            let start = match line.find("cpu") {
                Some(s) => s,
                None => continue,
            };
            let parts: Vec<&str> = line[start..].split_whitespace().collect();
            if parts.len() < 5 {
                return;
            }
            let name = parts[0].to_string();
            let nums: Vec<i64> = match parts[1..].iter().map(|t| t.parse::<i64>()).collect() {
                Ok(n) => n,
                Err(_) => return,
            };
            let sum: i64 = nums.iter().sum();
            let idle = nums[3];
            if let Some((ls, li)) = last.get(&name) {
                let cpu_delta = sum - ls;
                let idle_delta = idle - li;
                if cpu_delta == 0 {
                    // ZeroDivisionError aborts the update in python
                    return;
                }
                usage.insert(name.clone(), float_value(py_round(100.0 * ((cpu_delta - idle_delta) as f64 / cpu_delta as f64), 2)));
            }
            *self.cpu_usage.borrow_mut() = usage.clone();
            last.insert(name, (sum, idle));
        }
    }

    fn get_cpu_thermal_file() -> String {
        if let Ok(rd) = std::fs::read_dir(HWMON_ROOT_PATH) {
            for e in rd.flatten() {
                let p = e.path();
                if !p.is_dir() {
                    continue;
                }
                if let Ok(name) = std::fs::read_to_string(p.join("name")) {
                    let pf = match name.trim() {
                        "coretemp" => "Intel",
                        "k10temp" => "AMD",
                        "cpu_thermal" => "Raspberry Pi",
                        _ => continue,
                    };
                    log::info!("Monitoring temperature for {} CPU", pf);
                    return format!("{}/temp1_input", p.display());
                }
            }
        }
        log::info!("Monitoring temperature using default thermal zone");
        TEMPERATURE_PATH.into()
    }

    fn format_stats(s: &Value) -> String {
        format!(
            "System Time: {:.6}, Usage: {}%, Memory: {} {}",
            s["time"].as_f64().unwrap_or(0.0),
            crate::pyfmt::py_str(&s["cpu_usage"]),
            crate::pyfmt::py_str(&s["memory"]),
            crate::pyfmt::py_str(&s["mem_units"])
        )
    }

    pub fn log_last_stats(&self, count: usize) {
        let q = self.proc_stat_queue.borrow();
        let count = count.min(q.len());
        let mut msg = String::new();
        for s in q.iter().skip(q.len() - count) {
            msg.push_str(&format!("\n{}", Self::format_stats(s)));
        }
        log::info!("{}", msg);
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<ProcStats>, crate::confighelper::ConfigError> {
    ProcStats::new(config)
}

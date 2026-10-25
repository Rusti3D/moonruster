// General Server Utilities (port of moonraker/utils/__init__.py)

use std::ffi::CString;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Equivalent of asyncio.Event
#[derive(Default)]
pub struct AsyncEvent {
    flag: std::cell::Cell<bool>,
    notify: tokio::sync::Notify,
}

impl AsyncEvent {
    pub fn new() -> std::rc::Rc<Self> {
        std::rc::Rc::new(Self::default())
    }
    pub fn new_set() -> std::rc::Rc<Self> {
        let e = Self::new();
        e.set();
        e
    }
    pub fn set(&self) {
        self.flag.set(true);
        self.notify.notify_waiters();
    }
    pub fn clear(&self) {
        self.flag.set(false);
    }
    pub fn is_set(&self) -> bool {
        self.flag.get()
    }
    pub async fn wait(&self) {
        loop {
            if self.flag.get() {
                return;
            }
            let n = self.notify.notified();
            tokio::pin!(n);
            n.as_mut().enable();
            if self.flag.get() {
                return;
            }
            n.await;
        }
    }
}

// ***** Ordered timers *****
// asyncio runs timers in (deadline, scheduling order).  Independent tokio
// sleeps that land in the same timer slot may wake in any order, so short
// delays whose ordering is visible to clients (notifications) use this queue.

struct TimerQueue {
    heap: std::collections::BinaryHeap<std::cmp::Reverse<(tokio::time::Instant, u64)>>,
    waiters: std::collections::HashMap<u64, tokio::sync::oneshot::Sender<()>>,
    seq: u64,
    driver: Option<std::rc::Rc<tokio::sync::Notify>>,
}

thread_local! {
    static TIMERS: std::cell::RefCell<TimerQueue> = std::cell::RefCell::new(TimerQueue {
        heap: Default::default(),
        waiters: Default::default(),
        seq: 0,
        driver: None,
    });
}

/// asyncio.sleep() with the loop's timer ordering
pub async fn sleep_ordered(delay: std::time::Duration) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let deadline = tokio::time::Instant::now() + delay;
    let (start_driver, wake) = TIMERS.with(|t| {
        let mut t = t.borrow_mut();
        t.seq += 1;
        let seq = t.seq;
        let earliest = t.heap.peek().map(|r| r.0 .0 > deadline).unwrap_or(true);
        t.heap.push(std::cmp::Reverse((deadline, seq)));
        t.waiters.insert(seq, tx);
        match &t.driver {
            Some(n) => (None, if earliest { Some(n.clone()) } else { None }),
            None => {
                let n = std::rc::Rc::new(tokio::sync::Notify::new());
                t.driver = Some(n.clone());
                (Some(n), None)
            }
        }
    });
    if let Some(n) = wake {
        n.notify_one();
    }
    if let Some(n) = start_driver {
        crate::server::spawn(timer_driver(n));
    }
    let _ = rx.await;
}

async fn timer_driver(notify: std::rc::Rc<tokio::sync::Notify>) {
    loop {
        let next = TIMERS.with(|t| t.borrow().heap.peek().map(|r| r.0 .0));
        match next {
            None => notify.notified().await,
            Some(dl) => {
                tokio::select! {
                    _ = tokio::time::sleep_until(dl) => {}
                    _ = notify.notified() => continue,
                }
            }
        }
        let now = tokio::time::Instant::now();
        let due: Vec<tokio::sync::oneshot::Sender<()>> = TIMERS.with(|t| {
            let mut t = t.borrow_mut();
            let mut out = Vec::new();
            while let Some(std::cmp::Reverse((dl, seq))) = t.heap.peek().copied() {
                if dl > now {
                    break;
                }
                t.heap.pop();
                if let Some(tx) = t.waiters.remove(&seq) {
                    out.push(tx);
                }
            }
            out
        });
        for tx in due {
            let _ = tx.send(());
        }
    }
}

pub fn time_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Monotonic time in seconds, equivalent of asyncio's loop.time()
pub fn monotonic() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: ts is a valid timespec, exclusively borrowed for clock_gettime to fill
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

/// Converts a float to a json value, non-finite values become null since
/// they cannot be encoded
pub use crate::pycompat::pyfmt::float_value;

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| {
        // SAFETY: getpwuid's result is NULL or points to a static passwd entry, read before any other getpwuid call (single loop thread); pw_dir is NUL-terminated
        unsafe {
            let pw = libc::getpwuid(libc::getuid());
            if !pw.is_null() {
                let dir = std::ffi::CStr::from_ptr((*pw).pw_dir);
                return PathBuf::from(dir.to_string_lossy().to_string());
            }
        }
        PathBuf::from("/")
    })
}

/// python's os.path.expanduser
pub fn expand_user(p: &str) -> PathBuf {
    if p == "~" {
        return home_dir();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(p)
}

/// pathlib's absolute(): prefix with cwd without normalization
pub fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(p)
    }
}

/// os.path.normpath equivalent for absolute paths
pub fn normpath(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push("/");
    }
    out
}

/// pathlib's resolve(strict=False)
pub fn resolve_path(p: &Path) -> PathBuf {
    let abs = absolute(p);
    if let Ok(c) = std::fs::canonicalize(&abs) {
        return c;
    }
    // Resolve the longest existing prefix, then append the remainder
    let mut existing = abs.clone();
    let mut remainder: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(c) = std::fs::canonicalize(&existing) {
            let mut out = c;
            for r in remainder.iter().rev() {
                if r == ".." {
                    out.pop();
                } else if r != "." {
                    out.push(r);
                }
            }
            return out;
        }
        match (existing.file_name().map(|f| f.to_os_string()), existing.parent()) {
            (Some(name), Some(parent)) => {
                remainder.push(name);
                existing = parent.to_path_buf();
            }
            _ => return normpath(&abs),
        }
    }
}

pub fn mtime_ns(p: &Path) -> Option<i128> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(p).ok()?;
    Some(m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128)
}

fn access(p: &Path, mode: libc::c_int) -> bool {
    let c = match CString::new(p.as_os_str().as_encoded_bytes()) {
        Ok(c) => c,
        Err(_) => return false,
    };
    // SAFETY: c is a NUL-terminated CString alive for the call
    unsafe { libc::access(c.as_ptr(), mode) == 0 }
}

pub fn access_read(p: &Path) -> bool {
    access(p, libc::R_OK)
}

pub fn access_rw(p: &Path) -> bool {
    access(p, libc::R_OK | libc::W_OK)
}

pub fn access_write(p: &Path) -> bool {
    access(p, libc::W_OK)
}

pub fn access_exec(p: &Path) -> bool {
    access(p, libc::X_OK)
}

/// pathlib's Path.glob for a relative pattern
pub fn path_glob(base: &Path, pattern: &str) -> Vec<PathBuf> {
    let escaped = glob::Pattern::escape(&base.to_string_lossy());
    let full = format!("{}/{}", escaped.trim_end_matches('/'), pattern);
    let opts = glob::MatchOptions { case_sensitive: true, require_literal_separator: true, require_literal_leading_dot: false };
    match glob::glob_with(&full, opts) {
        Ok(paths) => paths.filter_map(|p| p.ok()).collect(),
        Err(_) => Vec::new(),
    }
}

pub fn parse_ip_address(addr: &str) -> Option<std::net::IpAddr> {
    if addr.is_empty() {
        return None;
    }
    // strip ipv6 zone/brackets
    let a = addr.trim_start_matches('[').trim_end_matches(']');
    let a = a.split('%').next().unwrap_or(a);
    a.parse().ok()
}

/// Returns the name of the current linux user
pub fn get_user() -> String {
    for var in ["LOGNAME", "USER", "LNAME", "USERNAME"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                return v;
            }
        }
    }
    // SAFETY: getpwuid's result is NULL or points to a static passwd entry, read before any other getpwuid call (single loop thread); pw_name is NUL-terminated
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() {
            return std::ffi::CStr::from_ptr((*pw).pw_name).to_string_lossy().to_string();
        }
    }
    "unknown".into()
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is buf.len() writable bytes for gethostname
    unsafe {
        if libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) == 0 {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            return String::from_utf8_lossy(&buf[..end]).to_string();
        }
    }
    "localhost".into()
}

pub fn get_unix_peer_credentials(fd: i32, name: &str) -> Option<(u32, u32, u32)> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: cred is a valid ucred and len its size, both exclusively borrowed for getsockopt to fill; fd is the caller's open socket
    let ret = unsafe {
        libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cred as *mut _ as *mut libc::c_void, &mut len)
    };
    if ret != 0 {
        log::error!("Failed to get Unix Socket Peer Credentials for {}", name);
        return None;
    }
    Some((cred.pid as u32, cred.uid, cred.gid))
}

/// Strftime using the local timezone
pub fn local_strftime(fmt: &str) -> String {
    chrono::Local::now().format(fmt).to_string()
}

/// tornado's url_escape(plus=False): urllib.parse.quote(s, safe="")
pub fn url_escape(s: &str) -> String {
    crate::pycompat::urllib::quote(s, "")
}

/// urllib.parse.quote(s) (safe="/")
pub fn url_quote(s: &str) -> String {
    crate::pycompat::urllib::quote(s, "/")
}

/// tornado's url_unescape(plus=False): urllib.parse.unquote(s)
pub fn url_unescape(s: &str) -> String {
    crate::pycompat::urllib::unquote(s)
}

/// os.path.splitext()[1]
pub fn splitext_ext(name: &str) -> String {
    crate::pycompat::posixpath::splitext(name).1.to_string()
}

/// os.path.splitext()[0]
pub fn splitext_root(name: &str) -> String {
    crate::pycompat::posixpath::splitext(name).0.to_string()
}

/// urllib.parse.urlencode for a list of key/value pairs
pub fn urlencode(pairs: &[(&str, String)]) -> String {
    crate::pycompat::urllib::urlencode(pairs)
}

// compiled once per pattern (crate::pycompat::re)
pub use crate::pycompat::re::compile as regex;

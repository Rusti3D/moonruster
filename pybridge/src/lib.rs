// Prototype of Moonruster's Python component support (docs/PLUGINS.md
// Phase 1): the host CPython, loaded on first use, behaving as the
// virtualenv `python` that ran Moonraker, with its asyncio loop driven by
// tokio. Single-threaded like Moonruster (current_thread + LocalSet): the
// loop thread holds the GIL except while tokio parks.
pub mod capi;

use capi::*;
use std::ffi::{CStr, CString};
use std::os::fd::{AsRawFd, RawFd};
use std::ptr::null_mut;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::sync::Notify;

/// What the venv's own `python` reports about itself
pub struct VenvInfo {
    pub python: String,
    pub version: (u32, u32),
    pub libpython: String,
    pub json: serde_json::Value,
}

/// The facts compared between `python -c` and the embedded interpreter
pub const IDENTITY: &str = "import sys, json\n\
_mr_identity = json.dumps({'executable': sys.executable, 'prefix': sys.prefix, \
'base_prefix': sys.base_prefix, 'exec_prefix': sys.exec_prefix, \
'base_exec_prefix': sys.base_exec_prefix, 'path': [p for p in sys.path if p], \
'modules': sorted(sys.modules), 'flags': repr(sys.flags), 'version': sys.version})";

/// Ask the venv's python for its version, libpython and identity
pub fn query_venv(python: &str) -> Result<VenvInfo, String> {
    let code = format!(
        "{IDENTITY}\nimport sysconfig, os\nd = json.loads(_mr_identity)\n\
         d['libdir'] = sysconfig.get_config_var('LIBDIR')\n\
         d['soname'] = sysconfig.get_config_var('INSTSONAME')\n\
         d['vi'] = list(sys.version_info[:2])\nprint(json.dumps(d))"
    );
    let out = std::process::Command::new(python).arg("-c").arg(&code).output().map_err(|e| format!("{python}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{python}: {}", String::from_utf8_lossy(&out.stderr)));
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    let vi = &json["vi"];
    let version = (vi[0].as_u64().unwrap() as u32, vi[1].as_u64().unwrap() as u32);
    let soname = json["soname"].as_str().unwrap_or("").to_string();
    // LIBDIR as built; for relocated builds (uv) fall back to <base_prefix>/lib
    let mut candidates = vec![];
    if let Some(d) = json["libdir"].as_str() {
        candidates.push(format!("{d}/{soname}"));
    }
    candidates.push(format!("{}/lib/{soname}", json["base_prefix"].as_str().unwrap_or("")));
    let libpython = candidates
        .iter()
        .find(|p| std::path::Path::new(p).is_file())
        .cloned()
        .ok_or_else(|| format!("no {soname} in {candidates:?}"))?;
    Ok(VenvInfo { python: python.to_string(), version, libpython, json })
}

static STARTED: AtomicBool = AtomicBool::new(false);
static SAVED: AtomicPtr<PyThreadState> = AtomicPtr::new(null_mut());

/// tokio on_thread_park: release the GIL while the loop thread waits
pub fn park_hook() {
    if STARTED.load(Ordering::Relaxed) {
        SAVED.store(unsafe { PyEval_SaveThread() }, Ordering::Relaxed);
    }
}

/// tokio on_thread_unpark: take the GIL back
pub fn unpark_hook() {
    let t = SAVED.swap(null_mut(), Ordering::Relaxed);
    if !t.is_null() {
        unsafe { PyEval_RestoreThread(t) };
    }
}

/// Load libpython and start it as the venv's python would be
pub fn start(info: &VenvInfo, use_init_config: bool) -> Result<(), String> {
    capi::load(&info.libpython)?;
    let prog = CString::new(info.python.clone()).unwrap();
    unsafe {
        match (&lib().init_config, use_init_config) {
            (Some(ic), true) => {
                // PyInitConfig_Create starts from the isolated configuration;
                // the python executable's is the non-isolated one
                let cfg = (ic.create)();
                let ints = [(c"isolated", 0), (c"use_environment", 1), (c"user_site_directory", 1), (c"safe_path", 0)];
                if (ic.set_str)(cfg, c"program_name".as_ptr(), prog.as_ptr()) != 0
                    || (ic.set_str)(cfg, c"executable".as_ptr(), prog.as_ptr()) != 0
                    || ints.iter().any(|(k, v)| (ic.set_int)(cfg, k.as_ptr(), *v) != 0)
                    || (ic.initialize)(cfg) != 0
                {
                    (ic.free)(cfg);
                    return Err("PyInitConfig failed".into());
                }
                (ic.free)(cfg);
            }
            (None, true) => return Err("no PyInitConfig in this CPython".into()),
            _ => {
                // the wide string must outlive the interpreter: leaked on purpose
                let w = Py_DecodeLocale(prog.as_ptr(), null_mut());
                Py_SetProgramName(w);
                Py_InitializeEx(0);
            }
        }
    }
    STARTED.store(true, Ordering::Relaxed);
    Ok(())
}

/// Owned reference
pub struct Obj(pub *mut PyObject);

impl Obj {
    pub fn new(p: *mut PyObject) -> Result<Obj, String> {
        if p.is_null() {
            Err(take_error())
        } else {
            Ok(Obj(p))
        }
    }
    pub fn attr(&self, name: &str) -> Result<Obj, String> {
        let c = CString::new(name).unwrap();
        Obj::new(unsafe { PyObject_GetAttrString(self.0, c.as_ptr()) })
    }
    pub fn call(&self, args: Vec<Obj>) -> Result<Obj, String> {
        unsafe {
            let t = PyTuple_New(args.len() as Py_ssize_t);
            for (i, a) in args.into_iter().enumerate() {
                PyTuple_SetItem(t, i as Py_ssize_t, a.0); // steals
                std::mem::forget(a);
            }
            let r = PyObject_CallObject(self.0, t);
            Py_DecRef(t);
            Obj::new(r)
        }
    }
    pub fn call_method(&self, name: &str, args: Vec<Obj>) -> Result<Obj, String> {
        self.attr(name)?.call(args)
    }
    pub fn str(&self) -> String {
        unsafe {
            let s = PyObject_Str(self.0);
            if s.is_null() {
                return take_error();
            }
            let r = to_string(s);
            Py_DecRef(s);
            r
        }
    }
    pub fn f64(&self) -> f64 {
        unsafe { PyFloat_AsDouble(self.0) }
    }
    /// Give up ownership (the reference is returned to the caller)
    pub fn into_raw(self) -> *mut PyObject {
        let p = self.0;
        std::mem::forget(self);
        p
    }
}

impl Clone for Obj {
    fn clone(&self) -> Obj {
        unsafe { Py_IncRef(self.0) };
        Obj(self.0)
    }
}

impl Drop for Obj {
    fn drop(&mut self) {
        unsafe { Py_DecRef(self.0) }
    }
}

pub unsafe fn to_string(s: *mut PyObject) -> String {
    let mut n: Py_ssize_t = 0;
    let p = PyUnicode_AsUTF8AndSize(s, &mut n);
    if p.is_null() {
        return String::new();
    }
    String::from_utf8_lossy(std::slice::from_raw_parts(p as *const u8, n as usize)).into_owned()
}

pub fn none() -> Obj {
    let n = lib().none;
    unsafe { Py_IncRef(n) };
    Obj(n)
}

pub fn float(v: f64) -> Obj {
    Obj(unsafe { PyFloat_FromDouble(v) })
}

pub fn int(v: i64) -> Obj {
    Obj(unsafe { PyLong_FromLongLong(v) })
}

pub fn string(s: &str) -> Obj {
    Obj(unsafe { PyUnicode_FromStringAndSize(s.as_ptr() as *const _, s.len() as Py_ssize_t) })
}

/// The pending Python exception as text (printed to stderr too); clears it
pub fn take_error() -> String {
    unsafe {
        if PyErr_Occurred().is_null() {
            return "NULL without exception".into();
        }
        PyErr_Print();
    }
    "python exception (traceback above)".into()
}

pub fn import(name: &str) -> Result<Obj, String> {
    let c = CString::new(name).unwrap();
    Obj::new(unsafe { PyImport_ImportModule(c.as_ptr()) })
}

/// Compile and execute `src` as module `name` (registered in sys.modules)
pub fn exec_module(name: &str, src: &str) -> Result<Obj, String> {
    let n = CString::new(name).unwrap();
    let s = CString::new(src).unwrap();
    let f = CString::new(format!("<{name}>")).unwrap();
    unsafe {
        let co = Obj::new(Py_CompileString(s.as_ptr(), f.as_ptr(), FILE_INPUT))?;
        Obj::new(PyImport_ExecCodeModule(n.as_ptr(), co.0))
    }
}

/// Add a native function to a module (the def must be 'static)
pub fn add_function(module: &Obj, def: &'static PyMethodDef, slf: Obj) -> Result<(), String> {
    unsafe {
        let f = Obj::new(PyCMethod_New(def, slf.0, null_mut(), null_mut()))?;
        if PyObject_SetAttrString(module.0, def.ml_name, f.0) != 0 {
            return Err(take_error());
        }
    }
    Ok(())
}

pub fn cstr(p: *const std::ffi::c_char) -> &'static str {
    unsafe { CStr::from_ptr(p) }.to_str().unwrap()
}

struct Fd(RawFd);

impl AsRawFd for Fd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

/// Whether the loop's epoll fd has events, without waiting
fn fd_ready_now(fd: RawFd) -> bool {
    let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    unsafe { libc::poll(&mut p, 1, 0) > 0 }
}

/// The asyncio loop, driven from tokio
pub struct Loop {
    pub driver: Obj,
    pub loop_: Obj,
    pub wake: Rc<Notify>,
}

impl Loop {
    /// Create the driver (module `_mr_aio`, aio.py) and spawn its task on
    /// the current LocalSet
    pub fn start(drive: bool, blocking: bool) -> Result<Rc<Loop>, String> {
        let m = exec_module("_mr_aio", include_str!("aio.py"))?;
        let driver = m.attr("Driver")?.call(vec![Obj(unsafe { capi::PyLong_FromLongLong(blocking as i64) })])?;
        let loop_ = driver.attr("loop")?;
        let fd = unsafe { PyLong_AsLongLong(driver.call_method("fileno", vec![])?.0) } as RawFd;
        let l = Rc::new(Loop { driver, loop_, wake: Rc::new(Notify::new()) });
        if drive {
            let l2 = l.clone();
            tokio::task::spawn_local(async move { l2.drive(fd).await });
        }
        Ok(l)
    }

    async fn drive(&self, fd: RawFd) {
        // SAFETY: the selector's epoll fd stays open as long as the loop,
        // which outlives this task
        let afd = unsafe { AsyncFd::register_with_interest(Fd(fd), tokio::io::Interest::READABLE) }
            .map_err(|e| e.to_string())
            .expect("epoll fd");
        loop {
            let next = match self.driver.call_method("step", vec![]) {
                Ok(v) => v.f64(),
                Err(e) => {
                    eprintln!("asyncio step failed: {e}");
                    -1.0
                }
            };
            if next == 0.0 || fd_ready_now(fd) {
                tokio::task::yield_now().await;
                continue;
            }
            let timer = async {
                if next > 0.0 {
                    tokio::time::sleep(Duration::from_secs_f64(next)).await
                } else {
                    std::future::pending::<()>().await
                }
            };
            tokio::select! {
                g = afd.readable() => {
                    if let Ok(mut g) = g {
                        if !fd_ready_now(fd) {
                            g.clear_ready();
                        }
                    }
                }
                _ = timer => {}
                _ = self.wake.notified() => {}
            }
        }
    }
}

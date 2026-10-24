// Python components (docs/PLUGINS.md): a Moonraker component Moonruster
// does not have built in (a third-party file in moonraker/components) runs on
// the host's CPython, the one Moonraker ran under, loaded the first time
// such a component is configured. Without one, nothing here runs and
// libpython is never mapped.
//
// Single-threaded like the rest of Moonruster (tokio current_thread +
// LocalSet): the loop thread holds the GIL except while tokio parks
// (park_hook / unpark_hook), so Python threads run where they would under
// Moonraker, where Moonraker's own Python holds it.
pub mod capi;
pub mod component;
pub mod convert;
pub mod futures;
mod impls;
pub mod mirror_ids;
pub mod native;
pub mod objects;
pub mod py;

use capi::*;
use py::*;
use std::ffi::CString;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::ptr::null_mut;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::sync::Notify;

/// The Moonraker tree components are installed into (its root, holding the
/// `moonraker` package): $MOONRUSTER_MOONRAKER_DIR, else ~/moonraker, where
/// installers put them (Happy Hare, helix, timelapse) and which a replace
/// install leaves in place
pub fn moonraker_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("MOONRUSTER_MOONRAKER_DIR") {
        return PathBuf::from(d);
    }
    crate::utils::home_dir().join("moonraker")
}

/// The components directory of that tree
pub fn components_dir() -> PathBuf {
    moonraker_dir().join("moonraker").join("components")
}

/// Whether `name` is a Python component in the components directory:
/// `<name>.py` or `<name>/__init__.py`, as importlib finds it
pub fn component_exists(name: &str) -> bool {
    if name.is_empty() || name.contains(['/', '.']) {
        return false;
    }
    let dir = components_dir();
    dir.join(format!("{name}.py")).is_file() || dir.join(name).join("__init__.py").is_file()
}

/// The Python Moonraker ran under: $MOONRUSTER_PYTHON, else the
/// moonraker-env virtualenv next to the tree, else the system python3
pub fn python_executable() -> String {
    if let Ok(p) = std::env::var("MOONRUSTER_PYTHON") {
        return p;
    }
    let venv = moonraker_dir().with_file_name("moonraker-env").join("bin").join("python");
    if venv.is_file() {
        return venv.to_string_lossy().into_owned();
    }
    "python3".to_string()
}

/// What that python reports about itself
struct PythonInfo {
    executable: String,
    libpython: String,
}

/// Ask the python for its executable path and its libpython (LIBDIR /
/// INSTSONAME; <base_prefix>/lib for relocated builds)
fn query_python(python: &str) -> Result<PythonInfo, String> {
    let code = "import sys, sysconfig, json\n\
        print(json.dumps({'executable': sys.executable, 'base_prefix': sys.base_prefix, \
        'libdir': sysconfig.get_config_var('LIBDIR'), 'soname': sysconfig.get_config_var('INSTSONAME'), \
        'version': list(sys.version_info[:2])}))";
    let out = std::process::Command::new(python).arg("-c").arg(code).output().map_err(|e| format!("{python}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{python}: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("{python}: {e}"))?;
    let ver = (v["version"][0].as_u64().unwrap_or(0), v["version"][1].as_u64().unwrap_or(0));
    if ver < (3, 10) {
        return Err(format!("{python} is Python {}.{}: Moonraker needs 3.10 or newer", ver.0, ver.1));
    }
    let soname = v["soname"].as_str().unwrap_or("");
    let mut candidates = vec![];
    if let Some(d) = v["libdir"].as_str() {
        candidates.push(format!("{d}/{soname}"));
    }
    candidates.push(format!("{}/lib/{soname}", v["base_prefix"].as_str().unwrap_or("")));
    let libpython = candidates
        .iter()
        .find(|p| !soname.is_empty() && std::path::Path::new(p).is_file())
        .cloned()
        .ok_or_else(|| format!("{python}: no shared libpython ({soname}) in {candidates:?}"))?;
    Ok(PythonInfo { executable: v["executable"].as_str().unwrap_or(python).to_string(), libpython })
}

static STARTED: AtomicBool = AtomicBool::new(false);
static SAVED: AtomicPtr<PyThreadState> = AtomicPtr::new(null_mut());

/// Whether the interpreter runs
pub fn started() -> bool {
    STARTED.load(Ordering::Relaxed)
}

/// `sys.executable METADATA_SCRIPT` when a component replaced
/// file_manager.METADATA_SCRIPT (Happy Hare runs its own script): Moonraker
/// then extracts metadata with that command. None while it is Moonraker's
/// own metadata.py, which Moonruster's extractor stands in for
pub fn metadata_script_override() -> Option<String> {
    if !started() {
        return None;
    }
    let r = (|| -> PyResult<Option<String>> {
        let m = lookup("sys", "modules")?.call_method("get", &[&py_str("moonraker.components.file_manager.file_manager")])?;
        if m.is_none() {
            return Ok(None);
        }
        let script = m.getattr("METADATA_SCRIPT")?.str()?;
        // the module's own value: os.path.abspath(os.path.join(os.path.dirname(__file__), "metadata.py"))
        let dir = lookup("os.path", "dirname")?.call(&[&m.getattr("__file__")?])?;
        let own = lookup("os.path", "join")?.call(&[&dir, &py_str("metadata.py")])?;
        let own = lookup("os.path", "abspath")?.call(&[&own])?.str()?;
        if script == own {
            return Ok(None);
        }
        Ok(Some(format!("{} {}", lookup("sys", "executable")?.str()?, script)))
    })();
    r.unwrap_or_else(|e| {
        log::error!("moonruster: reading file_manager.METADATA_SCRIPT\n{}", e.traceback);
        None
    })
}

/// ShellCommandFactory.close() for the commands Python components run
/// (nothing to do when no component started the interpreter)
pub async fn impls_cancel_shell_commands() {
    if started() {
        impls::shell_command::cancel_running().await;
    }
}

/// tokio on_thread_park: release the GIL while the loop thread waits
pub fn park_hook() {
    if started() {
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

/// Load libpython and start it as `python` itself would start: CPython then
/// finds the virtualenv's pyvenv.cfg as the executable does (docs/PLUGINS.md
/// Phase 1: identical sys.path, prefixes, flags on 3.10-3.15)
fn start_interpreter(info: &PythonInfo) -> Result<(), String> {
    capi::load(&info.libpython)?;
    let prog = CString::new(info.executable.clone()).map_err(|e| e.to_string())?;
    unsafe {
        // PEP 741 only where Py_SetProgramName is gone; it starts from the
        // isolated configuration, the executable's is not isolated
        if libc::dlsym(libc::RTLD_DEFAULT, c"Py_SetProgramName".as_ptr()).is_null() {
            let ic = lib().init_config.as_ref().ok_or("libpython has neither Py_SetProgramName nor PyInitConfig")?;
            let cfg = (ic.create)();
            let ints = [(c"isolated", 0), (c"use_environment", 1), (c"user_site_directory", 1), (c"safe_path", 0)];
            let ok = (ic.set_str)(cfg, c"program_name".as_ptr(), prog.as_ptr()) == 0
                && (ic.set_str)(cfg, c"executable".as_ptr(), prog.as_ptr()) == 0
                && ints.iter().all(|(k, v)| (ic.set_int)(cfg, k.as_ptr(), *v) == 0)
                && (ic.initialize)(cfg) == 0;
            (ic.free)(cfg);
            if !ok {
                return Err("PyInitConfig failed".into());
            }
        } else {
            // the wide string must outlive the interpreter: kept on purpose
            let w = Py_DecodeLocale(prog.as_ptr(), null_mut());
            Py_SetProgramName(w);
            Py_InitializeEx(0);
        }
    }
    STARTED.store(true, Ordering::Relaxed);
    Ok(())
}

/// Start Python (once): interpreter, asyncio driver. Err (logged by the
/// caller as the component's load failure) when no usable CPython exists.
pub fn ensure_started() -> Result<(), String> {
    thread_local! {
        static STATE: std::cell::RefCell<Option<Result<(), String>>> = const { std::cell::RefCell::new(None) };
    }
    if let Some(r) = STATE.with(|s| s.borrow().clone()) {
        return r;
    }
    let r = (|| {
        let python = python_executable();
        let info = query_python(&python)?;
        log::info!("Python components: {} ({})", info.executable, info.libpython);
        start_interpreter(&info)?;
        install().map_err(|e| if e.traceback.is_empty() { e.message.clone() } else { e.traceback.clone() })?;
        Ok(())
    })();
    STATE.with(|s| *s.borrow_mut() = Some(r.clone()));
    r
}

/// The component layer on a started interpreter: `_moonruster`, the prelude
/// `_mr` with the mirror's import finder, the handlers, the asyncio loop,
/// then the startup side effects of the modules Moonraker imports at startup
fn install() -> PyResult<()> {
    native::create_module()?;
    let prelude = exec_module("_mr", include_str!("prelude.py"))?;
    let level = match log::max_level() {
        log::LevelFilter::Debug | log::LevelFilter::Trace => 10,
        _ => 20,
    };
    let pkg = moonraker_dir().join("moonraker");
    prelude.call_method("install", &[&py_str(&pkg.to_string_lossy()), &native::mirror_modules(), &py_int(level)])?;
    let mut reg = native::Registry::default();
    impls::register(&mut reg);
    native::install_handlers(reg);
    Loop::start()?;
    // third-party side effects of modules Moonraker imports at every start
    // (http_client's AsyncHTTPClient.configure; gen_mirror.py checks the list)
    let startup: Vec<PyRef> = mirror_ids::STARTUP_MODULES.iter().map(|m| py_str(m)).collect();
    let list = tuple(&startup.iter().collect::<Vec<_>>());
    prelude.call_method("run_startup", &[&list])?;
    Ok(())
}

thread_local! {
    static LOOP: std::cell::RefCell<Option<Rc<Loop>>> = const { std::cell::RefCell::new(None) };
}

/// The asyncio loop components run on
pub fn the_loop() -> Rc<Loop> {
    LOOP.with(|l| l.borrow().clone().expect("asyncio loop not started"))
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

/// CPython's asyncio loop (aio.py), driven from tokio
pub struct Loop {
    driver: PyRef,
    pub loop_: PyRef,
    /// wakes the driver after Rust scheduled Python callbacks
    pub wake: Rc<Notify>,
}

impl Loop {
    fn start() -> PyResult<()> {
        let m = exec_module("_mr_aio", include_str!("aio.py"))?;
        let driver = m.getattr("Driver")?.call(&[])?;
        let loop_ = driver.getattr("loop")?;
        let fd = driver.call_method("fileno", &[])?.as_i64()? as RawFd;
        let l = Rc::new(Loop { driver, loop_, wake: Rc::new(Notify::new()) });
        py::set_wake(l.wake.clone());
        LOOP.with(|x| *x.borrow_mut() = Some(l.clone()));
        crate::server::spawn(async move { l.drive(fd).await });
        Ok(())
    }

    async fn drive(&self, fd: RawFd) {
        // SAFETY: the selector's epoll fd stays open as long as the loop,
        // which lives as long as the process
        let afd = match unsafe { AsyncFd::register_with_interest(Fd(fd), tokio::io::Interest::READABLE) } {
            Ok(a) => a,
            Err(e) => {
                log::error!("asyncio loop: cannot watch its selector: {}", e);
                return;
            }
        };
        loop {
            py::set_in_step(true);
            let step = self.driver.call_method("step", &[]).and_then(|v| v.as_f64());
            py::set_in_step(false);
            let next = match step {
                Ok(v) => v,
                Err(e) => {
                    log::error!("asyncio loop step failed\n{}", e.traceback);
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
                        // tokio: clear only after seeing the fd is not ready
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

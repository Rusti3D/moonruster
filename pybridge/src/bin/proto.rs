// Phase 1 prototype checks (docs/PLUGINS.md): CPython as the venv's python,
// its asyncio loop driven by a tokio current_thread runtime + LocalSet (as
// Moonruster runs), calls both ways, the GIL, threads, subprocesses,
// signals, costs and memory.
//
// usage: proto VENV_PYTHON [control...]
//   controls (each must make checks fail): nohooks (GIL never released
//   while tokio parks), nodriver (asyncio loop not driven), blocking (a
//   blocking selector inside the loop); initconfig: start with PEP 741
//   PyInitConfig instead of Py_SetProgramName (3.14+)
use moonruster_pybridge::capi::*;
use moonruster_pybridge::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ptr::null_mut;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

thread_local! {
    static LOOP: RefCell<Option<Rc<Loop>>> = const { RefCell::new(None) };
    static TICKS: Cell<i64> = const { Cell::new(0) };
    static PENDING: RefCell<HashMap<i64, oneshot::Sender<Result<String, String>>>> = RefCell::new(HashMap::new());
    static NEXT_ID: Cell<i64> = const { Cell::new(0) };
}

fn the_loop() -> Rc<Loop> {
    LOOP.with(|l| l.borrow().clone().unwrap())
}

fn mem(tag: &str) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let get = |k: &str| status.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).unwrap_or("?").to_string();
    let rollup = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap_or_default();
    let kb = |k: &str| -> i64 {
        rollup.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse().ok()).unwrap_or(0)
    };
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
    // own memory as Moonruster's benchmarks count it, plus swapped-out pages (zram)
    println!(
        "MEM {tag:28} own {:>6} kB (Pss_Anon + SwapPss)  RssAnon {:>6} kB  RssFile {:>6} kB  Threads {:>2}  libpython mapped: {}",
        kb("Pss_Anon:") + kb("SwapPss:"),
        get("RssAnon:"),
        get("RssFile:"),
        get("Threads:"),
        maps.contains("libpython")
    );
}

// ---- native module _mr_native ----

unsafe extern "C" fn k_noop(_s: *mut PyObject, _a: *const *mut PyObject, _n: Py_ssize_t) -> *mut PyObject {
    none().into_raw()
}

unsafe extern "C" fn k_ticks(_s: *mut PyObject, _a: *const *mut PyObject, _n: Py_ssize_t) -> *mut PyObject {
    int(TICKS.with(|t| t.get())).into_raw()
}

/// spin(ms, counter): busy in Rust with the GIL held; returns how much
/// counter[0] changed during that window (read through the C API, which
/// runs no bytecode and so never hands the GIL over)
unsafe extern "C" fn k_spin(_s: *mut PyObject, a: *const *mut PyObject, _n: Py_ssize_t) -> *mut PyObject {
    let ms = PyLong_AsLongLong(*a);
    let read = || PyLong_AsLongLong(PyList_GetItem(*a.add(1), 0));
    let before = read();
    let end = Instant::now() + Duration::from_millis(ms as u64);
    while Instant::now() < end {
        std::hint::spin_loop();
    }
    int(read() - before).into_raw()
}

/// A Rust future as an asyncio Future: resolved by a tokio task after `secs`
unsafe fn rust_future(a: *const *mut PyObject, fail: bool) -> *mut PyObject {
    let secs = PyFloat_AsDouble(*a);
    let value = Obj(*a.add(1));
    Py_IncRef(value.0);
    let l = the_loop();
    let fut = match l.loop_.call_method("create_future", vec![]) {
        Ok(f) => f,
        Err(_) => return null_mut(),
    };
    let f2 = fut.clone();
    tokio::task::spawn_local(async move {
        tokio::time::sleep(Duration::from_secs_f64(secs)).await;
        // the loop thread holds the GIL whenever it is not parked
        let done = f2.call_method("done", vec![]).map(|d| d.str() == "True").unwrap_or(true);
        if !done {
            let r = if fail {
                let exc = Obj(lib().runtime_error);
                Py_IncRef(exc.0);
                exc.call(vec![value]).and_then(|e| f2.call_method("set_exception", vec![e]))
            } else {
                f2.call_method("set_result", vec![value])
            };
            if let Err(e) = r {
                eprintln!("resolving future: {e}");
            }
        }
        l.wake.notify_one();
    });
    fut.into_raw()
}

unsafe extern "C" fn k_rust_sleep(_s: *mut PyObject, a: *const *mut PyObject, _n: Py_ssize_t) -> *mut PyObject {
    rust_future(a, false)
}

unsafe extern "C" fn k_rust_fail(_s: *mut PyObject, a: *const *mut PyObject, _n: Py_ssize_t) -> *mut PyObject {
    rust_future(a, true)
}

/// done callback of a task Rust awaits: slf is the pending id
unsafe extern "C" fn k_done(slf: *mut PyObject, a: *const *mut PyObject, _n: Py_ssize_t) -> *mut PyObject {
    let id = PyLong_AsLongLong(slf);
    let task = Obj(*a);
    Py_IncRef(task.0);
    let r = (|| {
        if task.call_method("cancelled", vec![])?.str() == "True" {
            return Err("cancelled".to_string());
        }
        let exc = task.call_method("exception", vec![])?;
        if exc.0 != lib().none {
            return Err(format!("{}: {}", exc.attr("__class__")?.attr("__name__")?.str(), exc.str()));
        }
        Ok(task.call_method("result", vec![])?.str())
    })();
    if let Some(tx) = PENDING.with(|p| p.borrow_mut().remove(&id)) {
        let _ = tx.send(r);
    }
    none().into_raw()
}

macro_rules! def {
    ($name:ident, $py:literal, $f:ident) => {
        static $name: PyMethodDef =
            PyMethodDef { ml_name: $py.as_ptr(), ml_meth: $f, ml_flags: METH_FASTCALL, ml_doc: std::ptr::null() };
    };
}
def!(NOOP, c"noop", k_noop);
def!(TICKS_DEF, c"ticks", k_ticks);
def!(SPIN, c"spin", k_spin);
def!(RUST_SLEEP, c"rust_sleep", k_rust_sleep);
def!(RUST_FAIL, c"rust_fail", k_rust_fail);
def!(DONE, c"_done", k_done);

/// Await a Python coroutine from Rust (task + done callback + oneshot)
async fn await_py(coro: Obj, timeout: Duration) -> Result<String, String> {
    let l = the_loop();
    let task = l.loop_.call_method("create_task", vec![coro])?;
    let id = NEXT_ID.with(|n| {
        n.set(n.get() + 1);
        n.get()
    });
    let (tx, rx) = oneshot::channel();
    PENDING.with(|p| p.borrow_mut().insert(id, tx));
    let cb = Obj::new(unsafe { PyCMethod_New(&DONE, int(id).into_raw(), null_mut(), null_mut()) })?;
    task.call_method("add_done_callback", vec![cb])?;
    l.wake.notify_one();
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => Err("callback dropped".into()),
        Err(_) => {
            PENDING.with(|p| p.borrow_mut().remove(&id));
            let _ = task.call_method("cancel", vec![]);
            Err(format!("timeout after {timeout:?}"))
        }
    }
}

async fn run(args: Vec<String>) -> i32 {
    let python = &args[1];
    let control = |c: &str| args[2..].iter().any(|a| a == c);
    mem("start");
    let t = Instant::now();
    let info = match query_venv(python) {
        Ok(i) => i,
        Err(e) => {
            println!("FAIL query_venv: {e}");
            return 1;
        }
    };
    println!("venv {python}: CPython {}.{} at {} (query {:.0} ms)", info.version.0, info.version.1, info.libpython, t.elapsed().as_secs_f64() * 1e3);
    let t = Instant::now();
    if let Err(e) = start(&info, control("initconfig")) {
        println!("FAIL start: {e}");
        return 1;
    }
    println!("interpreter start {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
    mem("after Py_Initialize");

    let mut fails = 0;
    let mut report = |name: &str, r: Result<String, String>| match r {
        Ok(d) => println!("PASS {name:20} {d}"),
        Err(e) => {
            fails += 1;
            println!("FAIL {name:20} {e}")
        }
    };

    // identity: the embedded interpreter vs `python -c`
    let ident = exec_module("_mr_ident", IDENTITY).and_then(|m| m.attr("_mr_identity")).map(|s| s.str());
    match ident {
        Ok(s) => {
            let mine: serde_json::Value = serde_json::from_str(&s).unwrap();
            let mut diffs = vec![];
            for k in ["executable", "prefix", "base_prefix", "exec_prefix", "base_exec_prefix", "path", "flags", "version"] {
                if mine[k] != info.json[k] {
                    diffs.push(format!("{k}: embedded {} vs python {}", mine[k], info.json[k]));
                }
            }
            let set = |v: &serde_json::Value| -> std::collections::BTreeSet<String> {
                v.as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect()
            };
            let (a, b) = (set(&mine["modules"]), set(&info.json["modules"]));
            let only_e: Vec<_> = a.difference(&b).collect();
            let only_p: Vec<_> = b.difference(&a).collect();
            let detail = format!("modules only embedded {only_e:?}, only python -c {only_p:?}");
            report("identity", if diffs.is_empty() { Ok(detail) } else { Err(format!("{diffs:?}; {detail}")) });
        }
        Err(e) => report("identity", Err(e)),
    }

    // native module + asyncio driver
    let native = match exec_module("_mr_native", "") {
        Ok(m) => m,
        Err(e) => {
            println!("FAIL native module: {e}");
            return 1;
        }
    };
    for d in [&NOOP, &TICKS_DEF, &SPIN, &RUST_SLEEP, &RUST_FAIL] {
        add_function(&native, d, none()).unwrap();
    }
    let l = match Loop::start(!control("nodriver"), control("blocking")) {
        Ok(l) => l,
        Err(e) => {
            println!("FAIL loop: {e}");
            return 1;
        }
    };
    LOOP.with(|x| *x.borrow_mut() = Some(l.clone()));
    tokio::task::spawn_local(async {
        let mut iv = tokio::time::interval(Duration::from_millis(5));
        loop {
            iv.tick().await;
            TICKS.with(|t| t.set(t.get() + 1));
        }
    });
    mem("after asyncio loop");
    let tests = match exec_module("_mr_proto", include_str!("proto.py")) {
        Ok(m) => m,
        Err(e) => {
            println!("FAIL proto.py: {e}");
            return 1;
        }
    };
    unsafe { PyObject_SetAttrString(tests.0, c"LOOP".as_ptr(), l.loop_.clone().into_raw()) };

    // a tokio child process runs alongside Python's subprocess check
    let rust_child = tokio::task::spawn_local(async {
        let st = tokio::process::Command::new("sh").arg("-c").arg("sleep 0.3; exit 5").status().await;
        st.map(|s| s.code())
    });
    let list = tests.attr("TESTS").unwrap();
    let n = unsafe { libc_len(&list) };
    for i in 0..n {
        let f = list.call_method("__getitem__", vec![int(i)]).unwrap();
        let name = f.attr("__name__").unwrap().str();
        let r = match f.call(vec![]) {
            Ok(coro) => await_py(coro, Duration::from_secs(5)).await,
            Err(e) => Err(e),
        };
        report(&name, r);
    }
    report(
        "tokio_child",
        match rust_child.await {
            Ok(Ok(Some(5))) => Ok("exit 5 (alongside asyncio's)".into()),
            other => Err(format!("{other:?}")),
        },
    );
    // tokio's own signal handling still works after asyncio installed one
    report("tokio_signal", {
        let mut s = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined2()).unwrap();
        unsafe { libc::kill(libc::getpid(), libc::SIGUSR2) };
        match tokio::time::timeout(Duration::from_secs(1), s.recv()).await {
            Ok(Some(())) => Ok("SIGUSR2 received".into()),
            other => Err(format!("{other:?}")),
        }
    });
    // Rust -> Python call cost
    let noop = native.attr("noop").unwrap();
    let k = 100_000;
    let t = Instant::now();
    for _ in 0..k {
        drop(noop.call(vec![]).unwrap());
    }
    println!("COST rust->python call {:.3} us", t.elapsed().as_secs_f64() * 1e6 / k as f64);
    mem("after all checks");
    println!("RESULT {} failed", fails);
    fails
}

unsafe fn libc_len(o: &Obj) -> i64 {
    PyLong_AsLongLong(o.call_method("__len__", vec![]).unwrap().0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: proto VENV_PYTHON [nohooks|nodriver|blocking|initconfig]...");
        std::process::exit(2);
    }
    let mut b = tokio::runtime::Builder::new_current_thread();
    b.enable_all();
    if !args[2..].iter().any(|a| a == "nohooks") {
        b.on_thread_park(park_hook).on_thread_unpark(unpark_hook);
    }
    let rt = b.build().unwrap();
    let local = tokio::task::LocalSet::new();
    let fails = local.block_on(&rt, run(args));
    std::process::exit(if fails == 0 { 0 } else { 1 });
}

// Futures across the boundary (docs/PLUGINS.md Phase 1, pybridge):
//  - await_py: Rust awaits a Python awaitable (a task on the asyncio loop
//    whose step that finishes the awaitable hands the outcome to a oneshot:
//    _mr.deliver)
//  - rust_future: a Rust future as a real asyncio.Future (Moonraker's
//    methods return those; timelapse checks isinstance(x, asyncio.Future))
use super::capi::*;
use super::native::py_error;
use super::py::*;
use super::the_loop;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use tokio::sync::oneshot;

thread_local! {
    static PENDING: RefCell<HashMap<i64, oneshot::Sender<PyResult<PyRef>>>> = RefCell::new(HashMap::new());
    static NEXT_ID: Cell<i64> = const { Cell::new(0) };
}

/// _mr.deliver's report of a task Rust awaits: slf is the pending id, the
/// arguments (ok, result or exception)
unsafe extern "C" fn k_done(slf: *mut PyObject, a: *const *mut PyObject, _n: Py_ssize_t, _kw: *mut PyObject) -> *mut PyObject {
    let id = PyLong_AsLongLong(slf);
    let ok = PyRef::borrowed(*a);
    let value = PyRef::borrowed(*a.add(1));
    let r = (|| {
        if ok.is_true()? {
            return Ok(value);
        }
        let exc = value;
        let type_name = exc.getattr("__class__")?.getattr("__name__")?.as_string()?;
        let message = exc.str()?;
        let tb = lookup("traceback", "format_exception")
            .and_then(|f| f.call(&[&exc.getattr("__class__")?, &exc, &exc.getattr("__traceback__")?]))
            .and_then(|l| py_str("").call_method("join", &[&l]))
            .and_then(|s| s.as_string())
            .unwrap_or_default();
        Err(PyError { type_name, message, traceback: tb.trim_end_matches('\n').into(), value: Some(exc) })
    })();
    if let Some(tx) = PENDING.with(|p| p.borrow_mut().remove(&id)) {
        let _ = tx.send(r);
    }
    none().into_raw()
}

static DONE: PyMethodDef =
    PyMethodDef { ml_name: c"_done".as_ptr(), ml_meth: k_done, ml_flags: METH_FASTCALL | METH_KEYWORDS, ml_doc: std::ptr::null() };

/// Await a Python awaitable (coroutine, Task or Future) from Rust
pub async fn await_py(aw: PyRef) -> PyResult<PyRef> {
    let l = the_loop();
    let id = NEXT_ID.with(|n| {
        n.set(n.get() + 1);
        n.get()
    });
    let (tx, rx) = oneshot::channel();
    PENDING.with(|p| p.borrow_mut().insert(id, tx));
    // the method takes its own reference to self
    let cb = PyRef::owned(unsafe { PyCMethod_New(&DONE, py_int(id).as_ptr(), std::ptr::null_mut(), std::ptr::null_mut()) })?;
    let coro = lookup("_mr", "deliver")?.call(&[&aw, &cb])?;
    if let Err(e) = l.loop_.call_method("create_task", &[&coro]) {
        PENDING.with(|p| p.borrow_mut().remove(&id));
        return Err(e);
    }
    l.wake.notify_one();
    rx.await.unwrap_or_else(|_| Err(py_error("RuntimeError", "moonruster: task callback dropped".into())))
}

/// Whether `v` is awaitable (inspect.isawaitable)
pub fn is_awaitable(v: &PyRef) -> PyResult<bool> {
    lookup("inspect", "isawaitable")?.call(&[v])?.is_true()
}

/// A Rust future as an asyncio.Future of the components' loop, resolved
/// with its result or exception when it completes
pub fn rust_future<F>(fut: F) -> PyResult<PyRef>
where
    F: Future<Output = PyResult<PyRef>> + 'static,
{
    let l = the_loop();
    let pyfut = l.loop_.call_method("create_future", &[])?;
    let f2 = pyfut.clone();
    crate::server::spawn(async move {
        let r = fut.await;
        // the loop thread holds the GIL whenever it is not parked
        let done = f2.call_method("done", &[]).and_then(|d| d.is_true()).unwrap_or(true);
        if !done {
            let res = match r {
                Ok(v) => f2.call_method("set_result", &[&v]),
                Err(e) => to_exception(&e).and_then(|exc| f2.call_method("set_exception", &[&exc])),
            };
            if let Err(e) = res {
                log::error!("moonruster: resolving a future failed\n{}", e.traceback);
            }
        }
        l.wake.notify_one();
    });
    Ok(pyfut)
}

/// The Python exception object of an error (its own one if it came from
/// Python, else a new instance of the builtin class it names)
pub fn to_exception(e: &PyError) -> PyResult<PyRef> {
    if let Some(v) = &e.value {
        return Ok(v.clone());
    }
    let cls = exc_class(&format!("PyExc_{}", e.type_name));
    let cls = if cls.is_null() { exc_class("PyExc_RuntimeError") } else { cls };
    PyRef::borrowed(cls).call(&[&py_str(&e.message)])
}

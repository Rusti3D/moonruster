// Owned Python references and conversions for the component layer
// (docs/PLUGINS.md). Every function here runs with the GIL held: on the
// loop thread that is whenever tokio is not parked.
use super::capi::*;
use std::cell::{Cell, RefCell};
use std::ffi::CString;
use std::ptr::null_mut;
use std::rc::Rc;

thread_local! {
    // set while the asyncio loop runs one iteration (mod.rs Loop::drive)
    static IN_STEP: Cell<bool> = const { Cell::new(false) };
    static WAKE: RefCell<Option<Rc<tokio::sync::Notify>>> = const { RefCell::new(None) };
}

/// The asyncio driver's wake-up (mod.rs Loop::start)
pub fn set_wake(n: Rc<tokio::sync::Notify>) {
    WAKE.with(|w| *w.borrow_mut() = Some(n));
}

/// Mark the asyncio loop's iteration (mod.rs Loop::drive)
pub fn set_in_step(v: bool) {
    IN_STEP.with(|s| s.set(v));
}

/// After Rust called Python outside the loop's own iteration (an MQTT
/// message, a remote method, an event from a Rust task): what it scheduled
/// with call_soon / create_task must run now, as it would in Moonraker's
/// loop; asyncio itself only wakes its selector for other threads
fn wake_after_call() {
    if !IN_STEP.with(|s| s.get()) {
        WAKE.with(|w| {
            if let Some(n) = w.borrow().as_ref() {
                n.notify_one();
            }
        });
    }
}

/// A Python exception taken off the interpreter: its class name, str(), and
/// the traceback text traceback.format_exception() gives (what Moonraker's
/// logging.exception() writes)
#[derive(Debug, Clone)]
pub struct PyError {
    pub type_name: String,
    pub message: String,
    pub traceback: String,
    /// the exception object itself, to raise it again in Python
    pub value: Option<PyRef>,
}

impl std::fmt::Display for PyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

pub type PyResult<T> = Result<T, PyError>;

/// An owned (strong) reference
pub struct PyRef(*mut PyObject);

impl PyRef {
    /// Takes ownership of a new reference; NULL means a Python exception
    pub fn owned(p: *mut PyObject) -> PyResult<PyRef> {
        if p.is_null() {
            Err(take_error())
        } else {
            Ok(PyRef(p))
        }
    }

    /// A new reference to a borrowed one
    pub fn borrowed(p: *mut PyObject) -> PyRef {
        // SAFETY: GIL held (module note); p is a valid object the caller borrowed, and the new reference is owned by the PyRef returned
        unsafe { Py_IncRef(p) };
        PyRef(p)
    }

    pub fn as_ptr(&self) -> *mut PyObject {
        self.0
    }

    /// Give the reference to the caller (a C API return value)
    pub fn into_raw(self) -> *mut PyObject {
        let p = self.0;
        std::mem::forget(self);
        p
    }

    pub fn getattr(&self, name: &str) -> PyResult<PyRef> {
        let c = CString::new(name).unwrap();
        // SAFETY: GIL held; self.0 is a valid object (PyRef owns a non-NULL reference) and c a NUL-terminated CString alive for the call
        PyRef::owned(unsafe { PyObject_GetAttrString(self.0, c.as_ptr()) })
    }

    pub fn hasattr(&self, name: &str) -> bool {
        let c = CString::new(name).unwrap();
        // SAFETY: GIL held; self.0 is a valid object and c a NUL-terminated CString alive for the call
        unsafe { PyObject_HasAttrString(self.0, c.as_ptr()) == 1 }
    }

    pub fn setattr(&self, name: &str, v: &PyRef) -> PyResult<()> {
        let c = CString::new(name).unwrap();
        // SAFETY: GIL held; self.0 and v.0 are valid objects (borrowed: the call takes its own reference) and c a NUL-terminated CString
        if unsafe { PyObject_SetAttrString(self.0, c.as_ptr(), v.0) } != 0 {
            return Err(take_error());
        }
        Ok(())
    }

    pub fn call(&self, args: &[&PyRef]) -> PyResult<PyRef> {
        let t = tuple(args);
        // SAFETY: GIL held; self.0 is a valid object and t a valid tuple, both kept alive across the call
        let r = PyRef::owned(unsafe { PyObject_CallObject(self.0, t.0) });
        wake_after_call();
        r
    }

    pub fn call_kw(&self, args: &[&PyRef], kwargs: &PyRef) -> PyResult<PyRef> {
        let t = tuple(args);
        // SAFETY: GIL held; self.0, the tuple t and kwargs are valid objects kept alive across the call
        let r = PyRef::owned(unsafe { PyObject_Call(self.0, t.0, kwargs.0) });
        wake_after_call();
        r
    }

    pub fn call_method(&self, name: &str, args: &[&PyRef]) -> PyResult<PyRef> {
        self.getattr(name)?.call(args)
    }

    pub fn is_none(&self) -> bool {
        self.0 == lib().none
    }

    pub fn is_true(&self) -> PyResult<bool> {
        // SAFETY: GIL held; self.0 is a valid object
        match unsafe { PyObject_IsTrue(self.0) } {
            -1 => Err(take_error()),
            v => Ok(v == 1),
        }
    }

    pub fn is_instance(&self, cls: &PyRef) -> PyResult<bool> {
        // SAFETY: GIL held; self.0 and cls.0 are valid objects
        match unsafe { PyObject_IsInstance(self.0, cls.0) } {
            -1 => Err(take_error()),
            v => Ok(v == 1),
        }
    }

    pub fn is_callable(&self) -> bool {
        // SAFETY: GIL held; self.0 is a valid object
        unsafe { PyCallable_Check(self.0) == 1 }
    }

    pub fn as_i64(&self) -> PyResult<i64> {
        // SAFETY: GIL held; self.0 is a valid object (a non-int sets an exception, checked below)
        let v = unsafe { PyLong_AsLongLong(self.0) };
        // SAFETY: GIL held; PyErr_Occurred only reads the thread's error state
        if v == -1 && unsafe { !PyErr_Occurred().is_null() } {
            return Err(take_error());
        }
        Ok(v)
    }

    pub fn as_f64(&self) -> PyResult<f64> {
        // SAFETY: GIL held; self.0 is a valid object (a non-number sets an exception, checked below)
        let v = unsafe { PyFloat_AsDouble(self.0) };
        // SAFETY: GIL held; PyErr_Occurred only reads the thread's error state
        if v == -1.0 && unsafe { !PyErr_Occurred().is_null() } {
            return Err(take_error());
        }
        Ok(v)
    }

    /// The str object's text (TypeError for anything else)
    pub fn as_string(&self) -> PyResult<String> {
        let mut n: Py_ssize_t = 0;
        // SAFETY: GIL held; self.0 is a valid object and n is exclusively borrowed for the size
        let p = unsafe { PyUnicode_AsUTF8AndSize(self.0, &mut n) };
        if p.is_null() {
            return Err(take_error());
        }
        // SAFETY: p points to n bytes of UTF-8 owned by the str object self.0, which outlives this copy
        Ok(String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(p as *const u8, n as usize) }).into_owned())
    }

    /// str(obj)
    pub fn str(&self) -> PyResult<String> {
        // SAFETY: GIL held; self.0 is a valid object
        PyRef::owned(unsafe { PyObject_Str(self.0) })?.as_string()
    }

    /// repr(obj)
    pub fn repr(&self) -> PyResult<String> {
        // SAFETY: GIL held; self.0 is a valid object
        PyRef::owned(unsafe { PyObject_Repr(self.0) })?.as_string()
    }
}

impl Clone for PyRef {
    fn clone(&self) -> PyRef {
        PyRef::borrowed(self.0)
    }
}

impl Drop for PyRef {
    fn drop(&mut self) {
        // SAFETY: GIL held (PyRef is !Send: it is only ever dropped on the loop thread); the PyRef owns this one reference, released once
        unsafe { Py_DecRef(self.0) }
    }
}

impl std::fmt::Debug for PyRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PyRef({:p})", self.0)
    }
}

pub fn none() -> PyRef {
    PyRef::borrowed(lib().none)
}

pub fn py_bool(v: bool) -> PyRef {
    PyRef::borrowed(if v { lib().true_ } else { lib().false_ })
}

pub fn py_int(v: i64) -> PyRef {
    // SAFETY: GIL held; returns a new reference (NULL only when memory is exhausted)
    PyRef(unsafe { PyLong_FromLongLong(v) })
}

pub fn py_float(v: f64) -> PyRef {
    // SAFETY: GIL held; returns a new reference (NULL only when memory is exhausted)
    PyRef(unsafe { PyFloat_FromDouble(v) })
}

pub fn py_str(s: &str) -> PyRef {
    // SAFETY: GIL held; s is s.len() bytes of valid UTF-8, copied by the call (NULL only when memory is exhausted)
    PyRef(unsafe { PyUnicode_FromStringAndSize(s.as_ptr() as *const _, s.len() as Py_ssize_t) })
}

pub fn py_bytes(b: &[u8]) -> PyResult<PyRef> {
    // SAFETY: GIL held; b is b.len() readable bytes, copied by the call
    PyRef::owned(unsafe { PyBytes_FromStringAndSize(b.as_ptr() as *const _, b.len() as Py_ssize_t) })
}

pub fn tuple(items: &[&PyRef]) -> PyRef {
    // SAFETY: GIL held; PyTuple_New gives a tuple with items.len() empty slots, each filled once; SetItem steals the reference Py_IncRef just added, so the items stay owned by their PyRefs too (NULL from PyTuple_New only when memory is exhausted)
    unsafe {
        let t = PyTuple_New(items.len() as Py_ssize_t);
        for (i, it) in items.iter().enumerate() {
            Py_IncRef(it.0);
            PyTuple_SetItem(t, i as Py_ssize_t, it.0); // steals the new reference
        }
        PyRef(t)
    }
}

pub fn import(name: &str) -> PyResult<PyRef> {
    let c = CString::new(name).unwrap();
    // SAFETY: GIL held; c is a NUL-terminated CString alive for the call
    PyRef::owned(unsafe { PyImport_ImportModule(c.as_ptr()) })
}

/// module.attr
pub fn lookup(module: &str, attr: &str) -> PyResult<PyRef> {
    import(module)?.getattr(attr)
}

/// Compile and execute `src` as module `name` (registered in sys.modules)
pub fn exec_module(name: &str, src: &str) -> PyResult<PyRef> {
    let n = CString::new(name).unwrap();
    let s = CString::new(src).unwrap();
    let f = CString::new(format!("<moonruster {name}>")).unwrap();
    // SAFETY: GIL held; s and f are NUL-terminated CStrings alive for the calls; co is a valid code object owned until the module ran
    unsafe {
        let co = PyRef::owned(Py_CompileString(s.as_ptr(), f.as_ptr(), FILE_INPUT))?;
        PyRef::owned(PyImport_ExecCodeModule(n.as_ptr(), co.0))
    }
}

/// Take the pending Python exception (NULL return without one is a bug in
/// the caller, reported as SystemError)
pub fn take_error() -> PyError {
    // SAFETY: GIL held; PyErr_Fetch moves the error's new references into t, v, tb, which the PyRefs below then own (NormalizeException may replace them, keeping ownership)
    unsafe {
        let (mut t, mut v, mut tb) = (null_mut(), null_mut(), null_mut());
        PyErr_Fetch(&mut t, &mut v, &mut tb);
        if t.is_null() {
            return PyError {
                type_name: "SystemError".into(),
                message: "error return without exception set".into(),
                traceback: String::new(),
                value: None,
            };
        }
        PyErr_NormalizeException(&mut t, &mut v, &mut tb);
        let (t, v) = (PyRef(t), PyRef(v));
        let tb = if tb.is_null() { none() } else { PyRef(tb) };
        if !tb.is_none() {
            // exc.__traceback__, as raise would have set it
            let _ = v.setattr("__traceback__", &tb);
        }
        let type_name = t.getattr("__name__").and_then(|n| n.as_string()).unwrap_or_default();
        let message = v.str().unwrap_or_default();
        // "".join(traceback.format_exception(t, v, tb)), as logging does
        let traceback = lookup("traceback", "format_exception")
            .and_then(|f| f.call(&[&t, &v, &tb]))
            .and_then(|lines| py_str("").call_method("join", &[&lines]))
            .and_then(|s| s.as_string())
            .unwrap_or_default();
        PyError { type_name, message, traceback: traceback.trim_end_matches('\n').to_string(), value: Some(v) }
    }
}

/// Raise `err` in Python again (a native function then returns NULL)
pub fn restore(err: &PyError) {
    // SAFETY: GIL held; PyErr_Restore steals the references into_raw gives it (the class and the exception); PyErr_SetString copies msg, a NUL-terminated CString alive for the call, and borrows the RuntimeError class
    unsafe {
        match &err.value {
            Some(v) => {
                let cls = v.getattr("__class__").map(|c| c.into_raw()).unwrap_or(null_mut());
                PyErr_Restore(cls, v.clone().into_raw(), null_mut());
            }
            None => {
                let msg = CString::new(err.message.clone()).unwrap_or_default();
                PyErr_SetString(exc_class("PyExc_RuntimeError"), msg.as_ptr());
            }
        }
    }
}

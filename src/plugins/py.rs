// Owned Python references and conversions for the component layer
// (docs/PLUGINS.md). Every function here runs with the GIL held: on the
// loop thread that is whenever tokio is not parked.
use super::capi::*;
use std::ffi::CString;
use std::ptr::null_mut;

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
        PyRef::owned(unsafe { PyObject_GetAttrString(self.0, c.as_ptr()) })
    }

    pub fn hasattr(&self, name: &str) -> bool {
        let c = CString::new(name).unwrap();
        unsafe { PyObject_HasAttrString(self.0, c.as_ptr()) == 1 }
    }

    pub fn setattr(&self, name: &str, v: &PyRef) -> PyResult<()> {
        let c = CString::new(name).unwrap();
        if unsafe { PyObject_SetAttrString(self.0, c.as_ptr(), v.0) } != 0 {
            return Err(take_error());
        }
        Ok(())
    }

    pub fn call(&self, args: &[&PyRef]) -> PyResult<PyRef> {
        let t = tuple(args);
        PyRef::owned(unsafe { PyObject_CallObject(self.0, t.0) })
    }

    pub fn call_kw(&self, args: &[&PyRef], kwargs: &PyRef) -> PyResult<PyRef> {
        let t = tuple(args);
        PyRef::owned(unsafe { PyObject_Call(self.0, t.0, kwargs.0) })
    }

    pub fn call_method(&self, name: &str, args: &[&PyRef]) -> PyResult<PyRef> {
        self.getattr(name)?.call(args)
    }

    pub fn is_none(&self) -> bool {
        self.0 == lib().none
    }

    pub fn is_true(&self) -> PyResult<bool> {
        match unsafe { PyObject_IsTrue(self.0) } {
            -1 => Err(take_error()),
            v => Ok(v == 1),
        }
    }

    pub fn is_instance(&self, cls: &PyRef) -> PyResult<bool> {
        match unsafe { PyObject_IsInstance(self.0, cls.0) } {
            -1 => Err(take_error()),
            v => Ok(v == 1),
        }
    }

    pub fn is_callable(&self) -> bool {
        unsafe { PyCallable_Check(self.0) == 1 }
    }

    pub fn as_i64(&self) -> PyResult<i64> {
        let v = unsafe { PyLong_AsLongLong(self.0) };
        if v == -1 && unsafe { !PyErr_Occurred().is_null() } {
            return Err(take_error());
        }
        Ok(v)
    }

    pub fn as_f64(&self) -> PyResult<f64> {
        let v = unsafe { PyFloat_AsDouble(self.0) };
        if v == -1.0 && unsafe { !PyErr_Occurred().is_null() } {
            return Err(take_error());
        }
        Ok(v)
    }

    /// The str object's text (TypeError for anything else)
    pub fn as_string(&self) -> PyResult<String> {
        let mut n: Py_ssize_t = 0;
        let p = unsafe { PyUnicode_AsUTF8AndSize(self.0, &mut n) };
        if p.is_null() {
            return Err(take_error());
        }
        Ok(String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(p as *const u8, n as usize) }).into_owned())
    }

    /// str(obj)
    pub fn str(&self) -> PyResult<String> {
        PyRef::owned(unsafe { PyObject_Str(self.0) })?.as_string()
    }

    /// repr(obj)
    pub fn repr(&self) -> PyResult<String> {
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
    PyRef(unsafe { PyLong_FromLongLong(v) })
}

pub fn py_float(v: f64) -> PyRef {
    PyRef(unsafe { PyFloat_FromDouble(v) })
}

pub fn py_str(s: &str) -> PyRef {
    PyRef(unsafe { PyUnicode_FromStringAndSize(s.as_ptr() as *const _, s.len() as Py_ssize_t) })
}

pub fn py_bytes(b: &[u8]) -> PyResult<PyRef> {
    PyRef::owned(unsafe { PyBytes_FromStringAndSize(b.as_ptr() as *const _, b.len() as Py_ssize_t) })
}

pub fn tuple(items: &[&PyRef]) -> PyRef {
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
    unsafe {
        let co = PyRef::owned(Py_CompileString(s.as_ptr(), f.as_ptr(), FILE_INPUT))?;
        PyRef::owned(PyImport_ExecCodeModule(n.as_ptr(), co.0))
    }
}

/// Take the pending Python exception (NULL return without one is a bug in
/// the caller, reported as SystemError)
pub fn take_error() -> PyError {
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

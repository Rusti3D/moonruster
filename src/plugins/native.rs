// The native module `_moonruster` the mirror modules call (prelude.py binds
// it as _mr.call / _mr.fcall / _mr.new ...), and the registry of what each
// mirrored method, constructor and attribute does in Moonruster.
// From kaliru's src/plugins/native.rs (@ b0e1435).
//
// Handlers are registered by Moonraker qualified name ("moonraker.server.
// Server.lookup_component"); mirror_ids.rs gives each name the ID the
// generated Python passes. An ID without a handler raises
// NotImplementedError naming it.
use super::capi::*;
use super::mirror_ids::{IDS, MODULES};
use super::objects;
use super::py::*;
use std::any::Any;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::c_char;
use std::io::Read;
use std::rc::Rc;

/// The arguments of a mirrored call: the Python object (methods), the
/// positional arguments and the keyword arguments, as the mirror passed them
pub struct Args {
    /// the qualified name the call was made under (a handler registered
    /// under several names: the built-in modules' load_component)
    pub qualname: &'static str,
    pub slf: Option<PyRef>,
    pub args: Vec<PyRef>,
    pub kwargs: Vec<(String, PyRef)>,
}

impl Args {
    /// Positional argument `i`, else keyword `name`, else None
    pub fn get(&self, i: usize, name: &str) -> Option<&PyRef> {
        self.args.get(i).or_else(|| self.kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v))
    }

    pub fn this<T: 'static>(&self) -> PyResult<Rc<T>> {
        objects::unwrap_as::<T>(self.slf.as_ref().expect("method without self"))
    }
}

pub type Method = fn(&Args) -> PyResult<PyRef>;
pub type Ctor = fn(&Args) -> PyResult<Rc<dyn Any>>;
pub type Getter = fn(&PyRef) -> PyResult<PyRef>;
pub type Setter = fn(&PyRef, &PyRef) -> PyResult<()>;

#[derive(Clone, Copy)]
pub enum Handler {
    Method(Method),
    Ctor(Ctor),
    Attr(Getter, Option<Setter>),
}

/// Collects handlers by qualified name (src/plugins/impls/*)
#[derive(Default)]
pub struct Registry(HashMap<&'static str, Handler>);

impl Registry {
    #[cfg(test)]
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.0.keys().copied()
    }

    pub fn method(&mut self, name: &'static str, f: Method) {
        self.0.insert(name, Handler::Method(f));
    }
    pub fn ctor(&mut self, name: &'static str, f: Ctor) {
        self.0.insert(name, Handler::Ctor(f));
    }
    pub fn attr(&mut self, name: &'static str, get: Getter, set: Option<Setter>) {
        self.0.insert(name, Handler::Attr(get, set));
    }
}

thread_local! {
    static HANDLERS: RefCell<Vec<Option<Handler>>> = const { RefCell::new(Vec::new()) };
    // (class qualname or "@<object handle>", name) a component replaced
    static PATCHED: RefCell<HashSet<(String, String)>> = RefCell::new(HashSet::new());
    static LOOP_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Resolve registered handlers to IDs; names the mirror does not have are a
/// bug in the handler table
pub fn install_handlers(reg: Registry) {
    let index: HashMap<&str, usize> = IDS.iter().enumerate().map(|(i, n)| (*n, i)).collect();
    let mut table = vec![None; IDS.len()];
    for (name, h) in reg.0 {
        match index.get(name) {
            Some(&i) => table[i] = Some(h),
            None => log::error!("plugins: handler for {} which Moonraker does not have", name),
        }
    }
    HANDLERS.with(|t| *t.borrow_mut() = table);
}

fn handler(id: usize) -> PyResult<Handler> {
    HANDLERS.with(|t| t.borrow().get(id).copied().flatten()).ok_or_else(|| {
        let name = IDS.get(id).copied().unwrap_or("?");
        py_error("NotImplementedError", format!("moonruster: {} is not mirrored yet", name))
    })
}

pub fn py_error(type_name: &str, message: String) -> PyError {
    PyError { type_name: type_name.into(), message, traceback: String::new(), value: None }
}

/// Raise `err` in Python and return NULL (a native function's error return)
fn raise(err: PyError) -> *mut PyObject {
    match &err.value {
        Some(_) => restore(&err),
        // SAFETY: GIL held (py.rs); exc_class gives the interpreter's own exception class (never freed) or NULL, replaced by RuntimeError; PyErr_SetObject takes its own reference to msg
        None => unsafe {
            let cls = exc_class(&format!("PyExc_{}", err.type_name));
            let cls = if cls.is_null() { exc_class("PyExc_RuntimeError") } else { cls };
            let msg = py_str(&err.message);
            PyErr_SetObject(cls, msg.as_ptr());
        },
    }
    std::ptr::null_mut()
}

/// The arguments of a FASTCALL|KEYWORDS call: positional, then keywords
unsafe fn collect(args: *const *mut PyObject, nargs: Py_ssize_t, kwnames: *mut PyObject) -> PyResult<(Vec<PyRef>, Vec<(String, PyRef)>)> {
    let n = nargs as usize;
    let pos: Vec<PyRef> = (0..n).map(|i| PyRef::borrowed(*args.add(i))).collect();
    let mut kw = vec![];
    if !kwnames.is_null() {
        let names = PyRef::borrowed(kwnames);
        for j in 0..PyTuple_Size(kwnames) as usize {
            let name = PyRef::borrowed(PyTuple_GetItem(names.as_ptr(), j as Py_ssize_t)).as_string()?;
            kw.push((name, PyRef::borrowed(*args.add(n + j))));
        }
    }
    Ok((pos, kw))
}

/// Calls from other threads would touch Moonruster's single-threaded state:
/// refused (Moonraker is not thread-safe there either; components hand work
/// to the loop with call_soon_threadsafe / run_in_thread)
fn check_thread() -> PyResult<()> {
    if LOOP_THREAD.with(|l| l.get()) {
        Ok(())
    } else {
        Err(py_error("RuntimeError", "moonruster: Moonraker objects can only be used from the event loop thread".into()))
    }
}

fn dispatch(f: impl FnOnce() -> PyResult<PyRef>) -> *mut PyObject {
    match check_thread().and_then(|_| f()) {
        Ok(v) => v.into_raw(),
        Err(e) => raise(e),
    }
}

// A Rust method bound to an object, for attributes that hold a method in
// Moonraker (ConfigHelper.has_section = configparser's has_section): self
// is (obj, the method's address)
unsafe extern "C" fn k_bound(slf: *mut PyObject, a: *const *mut PyObject, n: Py_ssize_t, kw: *mut PyObject) -> *mut PyObject {
    dispatch(|| {
        let (args, kwargs) = collect(a, n, kw)?;
        let obj = PyRef::borrowed(PyTuple_GetItem(slf, 0));
        let addr = PyRef::borrowed(PyTuple_GetItem(slf, 1)).as_i64()? as usize;
        // SAFETY: the address is a `Method` that bound_method() stored
        let f: Method = std::mem::transmute::<usize, Method>(addr);
        f(&Args { qualname: "", slf: Some(obj), args, kwargs })
    })
}

static BOUND: PyMethodDef =
    PyMethodDef { ml_name: c"bound".as_ptr(), ml_meth: k_bound, ml_flags: METH_FASTCALL | METH_KEYWORDS, ml_doc: std::ptr::null() };

/// A callable that calls `f` with `obj` as self
pub fn bound_method(obj: &PyRef, f: Method) -> PyResult<PyRef> {
    let slf = tuple(&[obj, &py_int(f as usize as i64)]);
    // the function takes its own reference to self
    // SAFETY: GIL held; BOUND is a static method definition; the method takes its own reference to slf
    PyRef::owned(unsafe { PyCMethod_New(&BOUND, slf.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut()) })
}

// _moonruster.call(obj, id, *args, **kw): a mirrored method
unsafe extern "C" fn k_call(_s: *mut PyObject, a: *const *mut PyObject, n: Py_ssize_t, kw: *mut PyObject) -> *mut PyObject {
    dispatch(|| {
        let (mut pos, kwargs) = collect(a, n, kw)?;
        if pos.len() < 2 {
            return Err(py_error("TypeError", "call(obj, id, ...)".into()));
        }
        let rest = pos.split_off(2);
        let id = pos[1].as_i64()? as usize;
        match handler(id)? {
            Handler::Method(f) => f(&Args { qualname: IDS[id], slf: Some(pos[0].clone()), args: rest, kwargs }),
            _ => Err(py_error("TypeError", format!("moonruster: {} is not a method", IDS[id]))),
        }
    })
}

// _moonruster.fcall(id, *args, **kw): a mirrored module function or static method
unsafe extern "C" fn k_fcall(_s: *mut PyObject, a: *const *mut PyObject, n: Py_ssize_t, kw: *mut PyObject) -> *mut PyObject {
    dispatch(|| {
        let (mut pos, kwargs) = collect(a, n, kw)?;
        if pos.is_empty() {
            return Err(py_error("TypeError", "fcall(id, ...)".into()));
        }
        let rest = pos.split_off(1);
        let id = pos[0].as_i64()? as usize;
        match handler(id)? {
            Handler::Method(f) => f(&Args { qualname: IDS[id], slf: None, args: rest, kwargs }),
            _ => Err(py_error("TypeError", format!("moonruster: {} is not a function", IDS[id]))),
        }
    })
}

// _moonruster.new(obj, id, *args, **kw): a mirrored __init__ binds `obj`
unsafe extern "C" fn k_new(_s: *mut PyObject, a: *const *mut PyObject, n: Py_ssize_t, kw: *mut PyObject) -> *mut PyObject {
    dispatch(|| {
        let (mut pos, kwargs) = collect(a, n, kw)?;
        if pos.len() < 2 {
            return Err(py_error("TypeError", "new(obj, id, ...)".into()));
        }
        let rest = pos.split_off(2);
        let id = pos[1].as_i64()? as usize;
        match handler(id)? {
            Handler::Ctor(f) => {
                let rust = f(&Args { qualname: IDS[id], slf: Some(pos[0].clone()), args: rest, kwargs })?;
                objects::bind(&pos[0], rust)?;
                Ok(none())
            }
            _ => Err(py_error("TypeError", format!("moonruster: {} is not a constructor", IDS[id]))),
        }
    })
}

// _moonruster.get(obj, id) / set(obj, id, value) / delete(obj, id): Attr descriptors
unsafe extern "C" fn k_get(_s: *mut PyObject, a: *const *mut PyObject, n: Py_ssize_t, kw: *mut PyObject) -> *mut PyObject {
    dispatch(|| {
        let (pos, _) = collect(a, n, kw)?;
        let id = pos[1].as_i64()? as usize;
        match handler(id)? {
            Handler::Attr(get, _) => get(&pos[0]),
            _ => Err(py_error("TypeError", format!("moonruster: {} is not an attribute", IDS[id]))),
        }
    })
}

unsafe extern "C" fn k_set(_s: *mut PyObject, a: *const *mut PyObject, n: Py_ssize_t, kw: *mut PyObject) -> *mut PyObject {
    dispatch(|| {
        let (pos, _) = collect(a, n, kw)?;
        let id = pos[1].as_i64()? as usize;
        match handler(id)? {
            Handler::Attr(_, Some(set)) => set(&pos[0], &pos[2]).map(|_| none()),
            Handler::Attr(_, None) => Err(py_error("AttributeError", format!("moonruster: {} is read-only here", IDS[id]))),
            _ => Err(py_error("TypeError", format!("moonruster: {} is not an attribute", IDS[id]))),
        }
    })
}

unsafe extern "C" fn k_delete(_s: *mut PyObject, a: *const *mut PyObject, n: Py_ssize_t, kw: *mut PyObject) -> *mut PyObject {
    dispatch(|| {
        let (pos, _) = collect(a, n, kw)?;
        let id = pos[1].as_i64()? as usize;
        Err(py_error("AttributeError", format!("moonruster: {} cannot be deleted", IDS[id])))
    })
}

// _moonruster.patched_class(cls, name) / patched_instance(obj, name):
// a component replaced a mirrored method (Moonruster's own call sites
// honour it: docs/PLUGINS.md "overrides")
unsafe extern "C" fn k_patched_class(_s: *mut PyObject, a: *const *mut PyObject, n: Py_ssize_t, kw: *mut PyObject) -> *mut PyObject {
    dispatch(|| {
        let (pos, _) = collect(a, n, kw)?;
        let cls = format!("{}.{}", pos[0].getattr("__module__")?.as_string()?, pos[0].getattr("__qualname__")?.as_string()?);
        let name = pos[1].as_string()?;
        PATCHED.with(|p| p.borrow_mut().insert((cls, name)));
        Ok(none())
    })
}

unsafe extern "C" fn k_patched_instance(_s: *mut PyObject, a: *const *mut PyObject, n: Py_ssize_t, kw: *mut PyObject) -> *mut PyObject {
    dispatch(|| {
        let (pos, _) = collect(a, n, kw)?;
        let h = pos[0].getattr("_mr_h")?.as_i64()?;
        let name = pos[1].as_string()?;
        PATCHED.with(|p| p.borrow_mut().insert((format!("@{h}"), name)));
        Ok(none())
    })
}

/// An object released by objects.rs: its instance patches go with it
pub fn forget_instance(handle: i64) {
    let key = format!("@{handle}");
    PATCHED.with(|p| p.borrow_mut().retain(|(k, _)| *k != key));
}

/// Whether a component replaced `name` on the class or on the object
pub fn is_patched(class_qualname: &str, handle: Option<i64>, name: &str) -> bool {
    PATCHED.with(|p| {
        let p = p.borrow();
        p.contains(&(class_qualname.to_string(), name.to_string()))
            || handle.is_some_and(|h| p.contains(&(format!("@{h}"), name.to_string())))
    })
}

// _moonruster.source(modname): a mirror module's source (mirror.bin)
unsafe extern "C" fn k_source(_s: *mut PyObject, a: *const *mut PyObject, n: Py_ssize_t, kw: *mut PyObject) -> *mut PyObject {
    dispatch(|| {
        let (pos, _) = collect(a, n, kw)?;
        let name = pos[0].as_string()?;
        mirror_source(&name)
            .map(|s| py_str(&s))
            .ok_or_else(|| py_error("ImportError", format!("moonruster: no mirror module {}", name)))
    })
}

// _moonruster.module_value(modname, name): a module name the mirror does
// not define (none are served from Rust yet)
unsafe extern "C" fn k_module_value(_s: *mut PyObject, a: *const *mut PyObject, n: Py_ssize_t, kw: *mut PyObject) -> *mut PyObject {
    dispatch(|| {
        let (pos, _) = collect(a, n, kw)?;
        Err(py_error(
            "AttributeError",
            format!("module '{}' has no attribute '{}'", pos[0].as_string()?, pos[1].as_string()?),
        ))
    })
}

// _moonruster.log(levelno, filename, funcName, lineno, message): a Python
// record, written as Moonraker's handler writes it
unsafe extern "C" fn k_log(_s: *mut PyObject, a: *const *mut PyObject, n: Py_ssize_t, kw: *mut PyObject) -> *mut PyObject {
    // logging works from any thread (the record is plain text)
    match (|| -> PyResult<PyRef> {
        let (pos, _) = collect(a, n, kw)?;
        let level = match pos[0].as_i64()? {
            l if l >= 40 => log::Level::Error,
            l if l >= 30 => log::Level::Warn,
            l if l >= 20 => log::Level::Info,
            l if l >= 10 => log::Level::Debug,
            _ => log::Level::Trace,
        };
        crate::loghelper::emit(level, &pos[1].as_string()?, &pos[2].as_string()?, pos[3].as_i64()? as u32, &pos[4].as_string()?);
        Ok(none())
    })() {
        Ok(v) => v.into_raw(),
        Err(e) => raise(e),
    }
}

macro_rules! def {
    ($name:ident, $py:literal, $f:ident) => {
        static $name: PyMethodDef =
            PyMethodDef { ml_name: $py.as_ptr(), ml_meth: $f, ml_flags: METH_FASTCALL | METH_KEYWORDS, ml_doc: std::ptr::null() };
    };
}
def!(CALL, c"call", k_call);
def!(FCALL, c"fcall", k_fcall);
def!(NEW, c"new", k_new);
def!(GET, c"get", k_get);
def!(SET, c"set", k_set);
def!(DELETE, c"delete", k_delete);
def!(PATCHED_CLASS, c"patched_class", k_patched_class);
def!(PATCHED_INSTANCE, c"patched_instance", k_patched_instance);
def!(SOURCE, c"source", k_source);
def!(MODULE_VALUE, c"module_value", k_module_value);
def!(LOG, c"log", k_log);

/// Create the module `_moonruster`
pub fn create_module() -> PyResult<()> {
    // the thread creating the module is the loop thread
    LOOP_THREAD.with(|l| l.set(true));
    let m = exec_module("_moonruster", "")?;
    for d in [&CALL, &FCALL, &NEW, &GET, &SET, &DELETE, &PATCHED_CLASS, &PATCHED_INSTANCE, &SOURCE, &MODULE_VALUE, &LOG] {
        // SAFETY: GIL held; d is a static method definition
        let f = PyRef::owned(unsafe { PyCMethod_New(d, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) })?;
        // SAFETY: ml_name is a static NUL-terminated c"" literal
        m.setattr(unsafe { std::ffi::CStr::from_ptr(d.ml_name as *const c_char) }.to_str().unwrap(), &f)?;
    }
    Ok(())
}

static MIRROR: &[u8] = include_bytes!("mirror.bin");

/// A mirror module's source, from mirror.bin (u32 count; per module: u16
/// name length, name, u32 compressed length, u32 source length, zlib)
fn mirror_source(name: &str) -> Option<String> {
    let mut p = 4usize;
    let count = u32::from_le_bytes(MIRROR.get(0..4)?.try_into().ok()?);
    for _ in 0..count {
        let nl = u16::from_le_bytes(MIRROR.get(p..p + 2)?.try_into().ok()?) as usize;
        p += 2;
        let n = std::str::from_utf8(MIRROR.get(p..p + nl)?).ok()?;
        p += nl;
        let zl = u32::from_le_bytes(MIRROR.get(p..p + 4)?.try_into().ok()?) as usize;
        let sl = u32::from_le_bytes(MIRROR.get(p + 4..p + 8)?.try_into().ok()?) as usize;
        p += 8;
        if n == name {
            let mut out = String::with_capacity(sl);
            flate2::read::ZlibDecoder::new(MIRROR.get(p..p + zl)?).read_to_string(&mut out).ok()?;
            return Some(out);
        }
        p += zl;
    }
    None
}

/// (name, is_package) of every mirrored module, as a Python list of tuples
pub fn mirror_modules() -> PyRef {
    // SAFETY: GIL held; returns a new empty list or NULL with an exception
    let list = PyRef::owned(unsafe { PyList_New(0) }).expect("list");
    for (name, is_pkg) in MODULES {
        let t = tuple(&[&py_str(name), &py_bool(*is_pkg)]);
        // SAFETY: GIL held; list and t are valid objects (Append takes its own reference to t)
        unsafe { PyList_Append(list.as_ptr(), t.as_ptr()) };
    }
    list
}

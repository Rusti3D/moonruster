// The CPython C API, loaded at run time with dlopen the first time a Python
// component is needed: a Moonruster without one never maps libpython.
// From kaliru's src/plugins/capi.rs (@ b0e1435), cut down to functions every
// CPython Moonraker supports exports (3.10+, Moonraker's requires-python;
// checked with `nm -D` on 3.10 and 3.15, docs/PLUGINS.md). The PEP 741
// PyInitConfig functions (3.14+) are optional.
//
// The library is opened RTLD_GLOBAL so C extension modules (_asyncio,
// select, tornado's speedups...) resolve their libpython symbols from it, as
// they do in the python executable.
#![allow(non_snake_case, non_camel_case_types, clippy::missing_safety_doc, dead_code)]
use std::ffi::{c_char, c_double, c_int, c_long, c_longlong, c_void, CStr, CString};
use std::sync::OnceLock;

pub type PyObject = c_void;
pub type PyThreadState = c_void;
pub type Py_ssize_t = isize;
pub type wchar_t = i32;

/// METH_FASTCALL | METH_KEYWORDS: positional args, then kwnames' values
pub type PyCFunctionFastKw = unsafe extern "C" fn(
    slf: *mut PyObject,
    args: *const *mut PyObject,
    nargs: Py_ssize_t,
    kwnames: *mut PyObject,
) -> *mut PyObject;

#[repr(C)]
pub struct PyMethodDef {
    pub ml_name: *const c_char,
    pub ml_meth: PyCFunctionFastKw,
    pub ml_flags: c_int,
    pub ml_doc: *const c_char,
}
unsafe impl Sync for PyMethodDef {}

pub const METH_FASTCALL: c_int = 0x0080;
pub const METH_KEYWORDS: c_int = 0x0002;
/// Py_file_input (compile a module)
pub const FILE_INPUT: c_int = 257;
/// PyGILState_STATE
pub type PyGILState = c_int;

macro_rules! capi {
    ($( fn $name:ident($($a:ident: $t:ty),*) $(-> $r:ty)?; )*) => {
        pub struct Api {
            $( pub $name: unsafe extern "C" fn($($t),*) $(-> $r)?, )*
        }

        impl Api {
            unsafe fn load(lib: *mut c_void) -> Result<Api, String> {
                Ok(Api {
                    $( $name: {
                        let p = libc::dlsym(lib, concat!(stringify!($name), "\0").as_ptr() as *const c_char);
                        if p.is_null() {
                            return Err(format!("libpython has no {}", stringify!($name)));
                        }
                        std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($t),*) $(-> $r)?>(p)
                    }, )*
                })
            }
        }

        $(
            #[inline]
            pub unsafe fn $name($($a: $t),*) $(-> $r)? {
                (api().$name)($($a),*)
            }
        )*
    };
}

capi! {
    fn Py_DecodeLocale(arg: *const c_char, size: *mut usize) -> *mut wchar_t;
    fn Py_SetProgramName(name: *const wchar_t);
    fn Py_InitializeEx(initsigs: c_int);
    fn Py_IncRef(o: *mut PyObject);
    fn Py_DecRef(o: *mut PyObject);

    fn Py_CompileString(src: *const c_char, filename: *const c_char, start: c_int) -> *mut PyObject;
    fn PyImport_ExecCodeModule(name: *const c_char, co: *mut PyObject) -> *mut PyObject;
    fn PyImport_ImportModule(name: *const c_char) -> *mut PyObject;
    fn PyImport_AddModule(name: *const c_char) -> *mut PyObject;
    fn PyCMethod_New(ml: *const PyMethodDef, slf: *mut PyObject, module: *mut PyObject, cls: *mut c_void) -> *mut PyObject;

    fn PyObject_GetAttrString(o: *mut PyObject, name: *const c_char) -> *mut PyObject;
    fn PyObject_SetAttrString(o: *mut PyObject, name: *const c_char, v: *mut PyObject) -> c_int;
    fn PyObject_HasAttrString(o: *mut PyObject, name: *const c_char) -> c_int;
    fn PyObject_Call(callable: *mut PyObject, args: *mut PyObject, kwargs: *mut PyObject) -> *mut PyObject;
    fn PyObject_CallObject(callable: *mut PyObject, args: *mut PyObject) -> *mut PyObject;
    fn PyObject_Str(o: *mut PyObject) -> *mut PyObject;
    fn PyObject_Repr(o: *mut PyObject) -> *mut PyObject;
    fn PyObject_IsTrue(o: *mut PyObject) -> c_int;
    fn PyObject_IsInstance(o: *mut PyObject, cls: *mut PyObject) -> c_int;
    fn PyObject_GetItem(o: *mut PyObject, key: *mut PyObject) -> *mut PyObject;
    fn PyObject_SetItem(o: *mut PyObject, key: *mut PyObject, v: *mut PyObject) -> c_int;
    fn PyObject_RichCompareBool(a: *mut PyObject, b: *mut PyObject, op: c_int) -> c_int;
    fn PyObject_GetIter(o: *mut PyObject) -> *mut PyObject;
    fn PyIter_Next(it: *mut PyObject) -> *mut PyObject;
    fn PyCallable_Check(o: *mut PyObject) -> c_int;

    fn PyLong_FromLongLong(v: c_longlong) -> *mut PyObject;
    fn PyLong_AsLongLong(o: *mut PyObject) -> c_longlong;
    fn PyLong_AsLong(o: *mut PyObject) -> c_long;
    fn PyFloat_FromDouble(v: c_double) -> *mut PyObject;
    fn PyFloat_AsDouble(o: *mut PyObject) -> c_double;
    fn PyBool_FromLong(v: c_long) -> *mut PyObject;
    fn PyUnicode_FromStringAndSize(s: *const c_char, len: Py_ssize_t) -> *mut PyObject;
    fn PyBytes_FromStringAndSize(s: *const c_char, len: Py_ssize_t) -> *mut PyObject;
    fn PyUnicode_AsUTF8AndSize(o: *mut PyObject, len: *mut Py_ssize_t) -> *const c_char;
    fn PyTuple_New(len: Py_ssize_t) -> *mut PyObject;
    fn PyTuple_SetItem(t: *mut PyObject, i: Py_ssize_t, v: *mut PyObject) -> c_int;
    fn PyTuple_GetItem(t: *mut PyObject, i: Py_ssize_t) -> *mut PyObject;
    fn PyTuple_Size(t: *mut PyObject) -> Py_ssize_t;
    fn PyList_New(len: Py_ssize_t) -> *mut PyObject;
    fn PyList_Append(l: *mut PyObject, v: *mut PyObject) -> c_int;
    fn PyList_Size(l: *mut PyObject) -> Py_ssize_t;
    fn PyList_GetItem(l: *mut PyObject, i: Py_ssize_t) -> *mut PyObject;
    fn PyDict_New() -> *mut PyObject;
    fn PyDict_SetItem(d: *mut PyObject, k: *mut PyObject, v: *mut PyObject) -> c_int;
    fn PyDict_SetItemString(d: *mut PyObject, k: *const c_char, v: *mut PyObject) -> c_int;
    fn PyDict_GetItemString(d: *mut PyObject, k: *const c_char) -> *mut PyObject;
    fn PyDict_Next(d: *mut PyObject, pos: *mut Py_ssize_t, k: *mut *mut PyObject, v: *mut *mut PyObject) -> c_int;
    fn PyModule_GetDict(m: *mut PyObject) -> *mut PyObject;

    fn PyErr_Occurred() -> *mut PyObject;
    fn PyErr_Fetch(t: *mut *mut PyObject, v: *mut *mut PyObject, tb: *mut *mut PyObject);
    fn PyErr_NormalizeException(t: *mut *mut PyObject, v: *mut *mut PyObject, tb: *mut *mut PyObject);
    fn PyErr_Restore(t: *mut PyObject, v: *mut PyObject, tb: *mut PyObject);
    fn PyErr_SetString(exc: *mut PyObject, msg: *const c_char);
    fn PyErr_SetObject(exc: *mut PyObject, v: *mut PyObject);
    fn PyErr_Print();

    fn PyEval_SaveThread() -> *mut PyThreadState;
    fn PyEval_RestoreThread(t: *mut PyThreadState);
    fn PyGILState_Ensure() -> PyGILState;
    fn PyGILState_Release(s: PyGILState);
}

/// PEP 741 configuration (CPython 3.14+), used if Py_SetProgramName is gone
pub struct InitConfigApi {
    pub create: unsafe extern "C" fn() -> *mut c_void,
    pub set_str: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int,
    pub set_int: unsafe extern "C" fn(*mut c_void, *const c_char, i64) -> c_int,
    pub initialize: unsafe extern "C" fn(*mut c_void) -> c_int,
    pub free: unsafe extern "C" fn(*mut c_void),
}

pub struct Lib {
    pub api: Api,
    pub init_config: Option<InitConfigApi>,
    pub none: *mut PyObject,
    pub true_: *mut PyObject,
    pub false_: *mut PyObject,
}
unsafe impl Sync for Lib {}
unsafe impl Send for Lib {}

static LIB: OnceLock<Lib> = OnceLock::new();

pub fn api() -> &'static Api {
    &LIB.get().expect("libpython not loaded").api
}

pub fn lib() -> &'static Lib {
    LIB.get().expect("libpython not loaded")
}

pub fn is_loaded() -> bool {
    LIB.get().is_some()
}

/// A `PyExc_*` exception class (a data symbol of libpython)
pub fn exc_class(name: &str) -> *mut PyObject {
    let c = CString::new(name).unwrap();
    // SAFETY: c is a NUL-terminated CString alive for dlsym; a PyExc_* symbol is a `PyObject *` global of the loaded libpython, read as such
    unsafe {
        let p = libc::dlsym(libc::RTLD_DEFAULT, c.as_ptr()) as *const *mut PyObject;
        if p.is_null() { std::ptr::null_mut() } else { *p }
    }
}

/// dlopen the given libpython and resolve the table (once)
pub fn load(path: &str) -> Result<(), String> {
    if is_loaded() {
        return Ok(());
    }
    let c = CString::new(path).unwrap();
    // SAFETY: c is a NUL-terminated CString alive for the call
    let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if h.is_null() {
        // SAFETY: dlopen just failed, so dlerror returns a NUL-terminated message
        let e = unsafe { CStr::from_ptr(libc::dlerror()) };
        return Err(format!("{}: {}", path, e.to_string_lossy()));
    }
    // SAFETY: h is the loaded libpython (never closed); each symbol is cast to its C API signature, and a missing symbol is an error, not a call
    unsafe {
        let api = Api::load(h)?;
        let sym = |n: &CStr| libc::dlsym(h, n.as_ptr());
        let init_config = {
            let (c, s, n, i, f) = (
                sym(c"PyInitConfig_Create"),
                sym(c"PyInitConfig_SetStr"),
                sym(c"PyInitConfig_SetInt"),
                sym(c"Py_InitializeFromInitConfig"),
                sym(c"PyInitConfig_Free"),
            );
            if c.is_null() || s.is_null() || n.is_null() || i.is_null() || f.is_null() {
                None
            } else {
                Some(InitConfigApi {
                    create: std::mem::transmute::<*mut c_void, unsafe extern "C" fn() -> *mut c_void>(c),
                    set_str: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int>(s),
                    set_int: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut c_void, *const c_char, i64) -> c_int>(n),
                    initialize: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut c_void) -> c_int>(i),
                    free: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut c_void)>(f),
                })
            }
        };
        let (none, t, f) = (sym(c"_Py_NoneStruct"), sym(c"_Py_TrueStruct"), sym(c"_Py_FalseStruct"));
        if none.is_null() || t.is_null() || f.is_null() {
            return Err("libpython has no _Py_NoneStruct / _Py_TrueStruct / _Py_FalseStruct".into());
        }
        let _ = LIB.set(Lib { api, init_config, none, true_: t, false_: f });
    }
    Ok(())
}

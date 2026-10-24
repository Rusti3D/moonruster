// The CPython C API, loaded at run time with dlopen, so that a Moonruster
// without Python components never maps libpython. Copied from kaliru
// (src/plugins/capi.rs @ b0e1435) and cut down to functions every CPython
// Moonraker supports exports (3.10+, Moonraker's requires-python). The
// PyInitConfig functions (3.14+) are optional.
//
// The library is opened RTLD_GLOBAL so C extension modules (_asyncio,
// select, ...) resolve their libpython symbols from it, as they do in the
// python executable.
#![allow(non_snake_case, non_camel_case_types, clippy::missing_safety_doc)]
use std::ffi::{c_char, c_double, c_int, c_longlong, c_void, CStr, CString};
use std::sync::OnceLock;

pub type PyObject = c_void;
pub type PyThreadState = c_void;
pub type Py_ssize_t = isize;
pub type wchar_t = i32;

pub type PyCFunctionFast =
    unsafe extern "C" fn(slf: *mut PyObject, args: *const *mut PyObject, nargs: Py_ssize_t) -> *mut PyObject;

#[repr(C)]
pub struct PyMethodDef {
    pub ml_name: *const c_char,
    pub ml_meth: PyCFunctionFast,
    pub ml_flags: c_int,
    pub ml_doc: *const c_char,
}
unsafe impl Sync for PyMethodDef {}

pub const METH_FASTCALL: c_int = 0x0080;
/// Py_file_input (compile a module)
pub const FILE_INPUT: c_int = 257;

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
    fn PyObject_CallObject(callable: *mut PyObject, args: *mut PyObject) -> *mut PyObject;
    fn PyObject_Str(o: *mut PyObject) -> *mut PyObject;

    fn PyLong_FromLongLong(v: c_longlong) -> *mut PyObject;
    fn PyLong_AsLongLong(o: *mut PyObject) -> c_longlong;
    fn PyFloat_FromDouble(v: c_double) -> *mut PyObject;
    fn PyFloat_AsDouble(o: *mut PyObject) -> c_double;
    fn PyUnicode_FromStringAndSize(s: *const c_char, len: Py_ssize_t) -> *mut PyObject;
    fn PyUnicode_AsUTF8AndSize(o: *mut PyObject, len: *mut Py_ssize_t) -> *const c_char;
    fn PyTuple_New(len: Py_ssize_t) -> *mut PyObject;
    fn PyTuple_SetItem(t: *mut PyObject, i: Py_ssize_t, v: *mut PyObject) -> c_int;
    fn PyList_GetItem(l: *mut PyObject, i: Py_ssize_t) -> *mut PyObject;

    fn PyErr_Occurred() -> *mut PyObject;
    fn PyErr_Print();
    fn PyErr_SetString(exc: *mut PyObject, msg: *const c_char);

    fn PyEval_SaveThread() -> *mut PyThreadState;
    fn PyEval_RestoreThread(t: *mut PyThreadState);
}

/// PEP 741 configuration (CPython 3.14+), used instead of Py_SetProgramName
/// when present
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
    pub runtime_error: *mut PyObject,
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

/// dlopen the given libpython and resolve the table (once)
pub fn load(path: &str) -> Result<(), String> {
    if is_loaded() {
        return Ok(());
    }
    let c = CString::new(path).unwrap();
    let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if h.is_null() {
        let e = unsafe { CStr::from_ptr(libc::dlerror()) };
        return Err(format!("{}: {}", path, e.to_string_lossy()));
    }
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
                    create: std::mem::transmute::<*mut c_void, _>(c),
                    set_str: std::mem::transmute::<*mut c_void, _>(s),
                    set_int: std::mem::transmute::<*mut c_void, _>(n),
                    initialize: std::mem::transmute::<*mut c_void, _>(i),
                    free: std::mem::transmute::<*mut c_void, _>(f),
                })
            }
        };
        let none = sym(c"_Py_NoneStruct");
        let rte = sym(c"PyExc_RuntimeError") as *const *mut PyObject;
        if none.is_null() || rte.is_null() {
            return Err("libpython has no _Py_NoneStruct / PyExc_RuntimeError".into());
        }
        let _ = LIB.set(Lib { api, init_config, none, runtime_error: *rte });
    }
    Ok(())
}

// moonraker.components.file_manager.file_manager.FileManager and
// MetadataStorage for Python components, on Moonruster's file manager
use super::*;
use crate::components::file_manager::storage::MetadataStorage;
use crate::components::file_manager::FileManager;

macro_rules! fm {
    ($m:literal) => {
        concat!("moonraker.components.file_manager.file_manager.FileManager.", $m)
    };
}

macro_rules! md {
    ($m:literal) => {
        concat!("moonraker.components.file_manager.file_manager.MetadataStorage.", $m)
    };
}

fn arg<'a>(a: &'a Args, i: usize, n: &str) -> PyResult<&'a PyRef> {
    a.get(i, n).ok_or_else(|| py_error("TypeError", format!("missing argument '{}'", n)))
}

fn str_or(a: &Args, i: usize, n: &str, default: &str) -> PyResult<String> {
    match a.get(i, n) {
        Some(v) => v.as_string(),
        None => Ok(default.to_string()),
    }
}

fn path(p: &str) -> PyResult<PyRef> {
    lookup("pathlib", "Path")?.call(&[&py_str(p)])
}

fn metadata_storage(fm: &Rc<FileManager>) -> PyResult<PyRef> {
    let md: Rc<dyn std::any::Any> = fm.get_metadata_storage();
    objects::wrap_class(md, "moonraker.components.file_manager.file_manager", "MetadataStorage")
}

pub fn register(r: &mut Registry) {
    r.method(fm!("get_directory"), |a| Ok(py_str(&a.this::<FileManager>()?.get_directory(&str_or(a, 0, "root", "gcodes")?))));
    r.method(fm!("get_registered_dirs"), |a| to_py(&Value::from(a.this::<FileManager>()?.get_registered_dirs())));
    r.method(fm!("register_directory"), |a| {
        let root = arg(a, 0, "root")?.as_string()?;
        let p = match a.get(1, "path") {
            Some(p) if !p.is_none() => Some(p.str()?),
            _ => None,
        };
        let full_access = match a.get(2, "full_access") {
            Some(v) => v.is_true()?,
            None => false,
        };
        Ok(py_bool(a.this::<FileManager>()?.register_directory(&root, p.as_deref(), full_access)))
    });
    r.method(fm!("get_relative_path"), |a| {
        let root = arg(a, 0, "root")?.as_string()?;
        let full = arg(a, 1, "full_path")?.str()?;
        Ok(py_str(&a.this::<FileManager>()?.get_relative_path(&root, &full)))
    });
    r.method(fm!("get_full_path"), |a| {
        let root = arg(a, 0, "root")?.as_string()?;
        let rel = arg(a, 1, "relative_path")?.str()?;
        match a.this::<FileManager>()?.get_full_path(&root, &rel) {
            Ok(p) => path(&p.to_string_lossy()),
            Err(e) => Err(server_error(&e.message, e.status_code)),
        }
    });
    r.method(fm!("check_file_exists"), |a| {
        let root = arg(a, 0, "root")?.as_string()?;
        let fname = arg(a, 1, "filename")?.str()?;
        let modified = match a.get(2, "modified") {
            Some(m) if !m.is_none() => Some(m.as_f64()?),
            _ => None,
        };
        Ok(py_bool(a.this::<FileManager>()?.check_file_exists(&root, &fname, modified)))
    });
    r.method(fm!("can_access_path"), |a| Ok(py_bool(a.this::<FileManager>()?.can_access_path(&arg(a, 0, "path")?.str()?))));
    r.method(fm!("get_metadata_storage"), |a| metadata_storage(&a.this::<FileManager>()?));
    r.attr(fm!("gcode_metadata"), |o| metadata_storage(&objects::unwrap_as::<FileManager>(o)?), None);
    // MetadataStorage.get / insert deep-copy: a converted value is a copy
    r.method(md!("get"), |a| {
        let key = arg(a, 0, "key")?.as_string()?;
        match a.this::<MetadataStorage>()?.get(&key) {
            Some(v) => to_py(&v),
            None => match a.get(1, "default") {
                Some(d) => lookup("copy", "deepcopy")?.call(&[d]),
                None => Ok(none()),
            },
        }
    });
    r.method(md!("insert"), |a| {
        let key = arg(a, 0, "key")?.as_string()?;
        let value = from_py(arg(a, 1, "value")?)?;
        a.this::<MetadataStorage>()?.insert(&key, value);
        Ok(none())
    });
    r.method(md!("is_processing"), |a| Ok(py_bool(a.this::<MetadataStorage>()?.is_processing())));
    r.method(md!("is_file_processing"), |a| Ok(py_bool(a.this::<MetadataStorage>()?.is_file_processing(&arg(a, 0, "fname")?.str()?))));
}

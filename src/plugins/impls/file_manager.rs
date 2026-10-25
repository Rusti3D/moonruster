// moonraker.components.file_manager.file_manager.FileManager and
// MetadataStorage for Python components, on Moonruster's file manager
use super::*;
use crate::components::file_manager::storage::MetadataStorage;
use crate::components::file_manager::FileManager;
use crate::plugins::futures::rust_future;

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

fn path(p: &str) -> PyResult<PyRef> {
    lookup("pathlib", "Path")?.call(&[&py_str(p)])
}

fn err(e: crate::common::ServerError) -> PyError {
    if e.is_exc {
        // an OSError of a file operation
        return py_error("OSError", e.message);
    }
    server_error(&e.message, e.status_code)
}

fn fm(a: &Args) -> PyResult<Rc<FileManager>> {
    a.this::<FileManager>()
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
    r.method(fm!("validate_gcode_path"), |a| {
        fm(a)?.validate_gcode_path(&arg(a, 0, "gc_path")?.str()?);
        Ok(none())
    });
    r.method(fm!("register_data_folder"), |a| {
        let p = fm(a)?.register_data_folder(&arg(a, 0, "folder_name")?.as_string()?, bool_or(a, 1, "full_access", false)?);
        path(&p.to_string_lossy())
    });
    r.method(fm!("disable_write_access"), |a| {
        fm(a)?.disable_write_access();
        Ok(none())
    });
    r.method(fm!("check_write_enabled"), |a| fm(a)?.check_write_enabled().map(|_| none()).map_err(err));
    // check_reserved_path(req_path, need_write, raise_error=True) -> bool
    r.method(fm!("check_reserved_path"), |a| {
        let (req, write) = (arg(a, 0, "req_path")?.str()?, arg(a, 1, "need_write")?.is_true()?);
        let f = fm(a)?;
        if bool_or(a, 2, "raise_error", true)? {
            f.check_reserved_path(&req, write).map_err(err)?;
            return Ok(py_bool(false));
        }
        Ok(py_bool(f.is_reserved_path(&req, write)))
    });
    r.method(fm!("add_reserved_path"), |a| {
        let res_path = std::path::PathBuf::from(arg(a, 1, "res_path")?.str()?);
        Ok(py_bool(fm(a)?.add_reserved_path(&arg(a, 0, "name")?.as_string()?, &res_path, bool_or(a, 2, "read_access", true)?)))
    });
    r.method(fm!("get_fixed_path_args"), |a| to_py(&Value::Object(fm(a)?.get_fixed_path_args())));
    r.method(fm!("upload_queue_enabled"), |a| Ok(py_bool(fm(a)?.upload_queue_enabled())));
    r.method(fm!("get_path_info"), |a| {
        let p = arg(a, 0, "path")?;
        let ps = p.str()?;
        match fm(a)?.get_path_info(&ps, &arg(a, 1, "root")?.as_string()?, bool_or(a, 2, "raise_error", true)?) {
            Ok(info) => to_py(&Value::Object(info)),
            // path.stat()'s own OSError
            Err(e) => match lookup("os", "stat").and_then(|stat| stat.call(&[p])) {
                Err(pe) => Err(pe),
                Ok(_) => Err(err(e)),
            },
        }
    });
    r.method(fm!("gen_temp_upload_path"), |a| Ok(py_str(&fm(a)?.gen_temp_upload_path())));
    // finalize_upload(form_args): the uploading user is form_args["current_user"]
    r.method(fm!("finalize_upload"), |a| {
        let form = arg(a, 0, "form_args")?;
        let user = match form.call_method("get", &[&py_str("current_user")])? {
            u if u.is_none() => None,
            u => Some(super::application::user_info_from_py(&u)?),
        };
        let items = lookup("builtins", "dict")?.call(&[form])?;
        items.call_method("pop", &[&py_str("current_user"), &none()])?;
        let Value::Object(form_args) = from_py(&items)? else {
            return Err(py_error("TypeError", "form_args must be a dict".into()));
        };
        let f = fm(a)?;
        rust_future(async move { to_py(&f.finalize_upload(form_args, user).await.map_err(err)?) })
    });
    r.method(fm!("get_file_list"), |a| {
        let (f, root) = (fm(a)?, arg(a, 0, "root")?.as_string()?);
        if bool_or(a, 1, "list_format", false)? {
            return to_py(&Value::Array(f.get_file_list_formatted(&root).map_err(err)?));
        }
        let list = f.get_file_list(&root).map_err(err)?;
        to_py(&Value::Object(list.into_iter().map(|(k, v)| (k, Value::Object(v))).collect()))
    });
    r.method(fm!("get_file_metadata"), |a| to_py(&fm(a)?.get_file_metadata(&arg(a, 0, "filename")?.as_string()?)));
    r.method(fm!("list_dir"), |a| to_py(&fm(a)?.list_dir(&arg(a, 0, "directory")?.as_string()?, bool_or(a, 1, "simple_format", false)?).map_err(err)?));
    r.method(fm!("delete_file"), |a| {
        let (f, p) = (fm(a)?, arg(a, 0, "path")?.as_string()?);
        rust_future(async move { to_py(&f.delete_file(&p).await.map_err(err)?) })
    });
    r.method(fm!("cancel_notification"), |a| {
        fm(a)?.cancel_notification(&arg(a, 0, "key")?.as_string()?);
        Ok(none())
    });
    r.method(fm!("get_metadata_storage"), |a| metadata_storage(&a.this::<FileManager>()?));
    // snapshots of the file manager's state (Moonraker's own containers)
    r.attr(fm!("file_paths"), |o| to_py(&Value::Object(objects::unwrap_as::<FileManager>(o)?.file_paths())), None);
    r.attr(
        fm!("full_access_roots"),
        |o| {
            let set = lookup("builtins", "set")?.call(&[])?;
            for r in objects::unwrap_as::<FileManager>(o)?.full_access_roots() {
                set.call_method("add", &[&py_str(&r)])?;
            }
            Ok(set)
        },
        None,
    );
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

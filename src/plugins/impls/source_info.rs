// moonraker.utils.source_info for Python components. The stdlib-only
// functions are Moonraker's own code (tools/mirror_verbatim.txt); these are
// the ones that need importlib_metadata or Moonraker's assets. Ported from
// Moonraker 9e676eba utils/source_info.py: importlib_metadata is imported
// only when one of them runs, not for every component.
use super::*;
use crate::plugins::capi::{PyDict_New, PyDict_SetItemString};

macro_rules! name {
    ($m:literal) => {
        concat!("moonraker.utils.source_info.", $m)
    };
}

fn arg<'a>(a: &'a Args, i: usize, n: &str) -> Option<&'a PyRef> {
    a.get(i, n).filter(|v| !v.is_none())
}

/// logging.info / logging.exception at source_info.py's `func`, `line`
fn log(level: log::Level, func: &str, line: u32, msg: &str) {
    crate::loghelper::emit(level, "source_info.py", func, line, msg);
}

fn kwargs(items: &[(&std::ffi::CStr, &PyRef)]) -> PyResult<PyRef> {
    let d = PyRef::owned(unsafe { PyDict_New() })?;
    for (k, v) in items {
        unsafe { PyDict_SetItemString(d.as_ptr(), k.as_ptr(), v.as_ptr()) };
    }
    Ok(d)
}

fn path(p: &str) -> PyResult<PyRef> {
    lookup("pathlib", "Path")?.call(&[&py_str(p)])
}

/// _load_release_info_json(dist_info)
fn load_release_info_json(dist_info: &PyRef) -> PyResult<PyRef> {
    let files = dist_info.getattr("files")?;
    if files.is_none() {
        return Ok(none());
    }
    let it = lookup("builtins", "iter")?.call(&[&files])?;
    let next = lookup("builtins", "next")?;
    let sentinel = lookup("builtins", "object")?.call(&[])?;
    loop {
        let f = next.call(&[&it, &sentinel])?;
        if f.as_ptr() == sentinel.as_ptr() {
            return Ok(none());
        }
        let parts = f.getattr("parts")?;
        let first = parts.call_method("__getitem__", &[&py_int(0)])?.as_string()?;
        if first == ".." || first == "/" {
            continue;
        }
        if f.getattr("name")?.as_string()? == "release_info" {
            log(log::Level::Info, "_load_release_info_json", 121, &format!("Package {}: Detected release_info json file", first));
            match f.call_method("read_text", &[]).and_then(|t| lookup("json", "loads")?.call(&[&t])) {
                Ok(v) => return Ok(v),
                Err(e) => log(
                    log::Level::Error,
                    "_load_release_info_json",
                    125,
                    &format!("Failed to load release_info from {}\n{}", f.str()?, e.traceback),
                ),
            }
        }
    }
}

/// _load_direct_url_json(dist_info)
fn load_direct_url_json(dist_info: &PyRef) -> PyResult<PyRef> {
    let ret = dist_info.call_method("read_text", &[&py_str("direct_url.json")])?;
    if ret.is_none() {
        return Ok(none());
    }
    match lookup("json", "loads")?.call(&[&ret]) {
        Ok(v) => Ok(v),
        Err(e) if e.type_name == "JSONDecodeError" => Ok(none()),
        Err(e) => Err(e),
    }
}

/// load_distribution_info(venv_path, project_name)
fn load_distribution_info(venv_path: &PyRef, project_name: &PyRef) -> PyResult<PyRef> {
    let si = import("moonraker.utils.source_info")?;
    let normalize = si.getattr("normalize_project_name")?;
    let proj_norm = normalize.call(&[project_name])?.as_string()?;
    let site_items = venv_path.call_method("joinpath", &[&py_str("lib")])?.call_method("glob", &[&py_str("python*/site-packages/")])?;
    let lib_paths = PyRef::owned(unsafe { crate::plugins::capi::PyList_New(0) })?;
    let it = lookup("builtins", "iter")?.call(&[&site_items])?;
    let next = lookup("builtins", "next")?;
    let sentinel = lookup("builtins", "object")?.call(&[])?;
    loop {
        let p = next.call(&[&it, &sentinel])?;
        if p.as_ptr() == sentinel.as_ptr() {
            break;
        }
        if p.call_method("is_dir", &[])?.is_true()? {
            lib_paths.call_method("append", &[&py_str(&p.str()?)])?;
        }
    }
    let ilm = import("importlib_metadata")?;
    let dists = ilm
        .getattr("Distribution")?
        .getattr("discover")?
        .call_kw(&[], &kwargs(&[(c"name", project_name), (c"path", &lib_paths)])?)?;
    let path_dist = ilm.getattr("PathDistribution")?;
    let it = lookup("builtins", "iter")?.call(&[&dists])?;
    loop {
        let dist_info = next.call(&[&it, &sentinel])?;
        if dist_info.as_ptr() == sentinel.as_ptr() {
            break;
        }
        let metadata = dist_info.getattr("metadata")?;
        if metadata.is_none() {
            continue;
        }
        if !dist_info.is_instance(&path_dist)? {
            log(log::Level::Info, "load_distribution_info", 152, &format!("Project {} not a PathDistribution", dist_info.getattr("name")?.str()?));
            continue;
        }
        let name = metadata.call_method("__getitem__", &[&py_str("Name")])?;
        let name = if name.is_true()? { name } else { py_str("") };
        if normalize.call(&[&name])?.as_string()? != proj_norm {
            continue;
        }
        let release_info = load_release_info_json(&dist_info)?;
        let install_info = load_direct_url_json(&dist_info)?;
        return si.getattr("PackageInfo")?.call(&[&dist_info, &metadata, &release_info, &install_info]);
    }
    Err(server_error(&format!("Failed to find distribution info for project {}", project_name.str()?), 400))
}

pub fn register(r: &mut Registry) {
    r.method(name!("read_asset"), |a| {
        let n = arg(a, 0, "asset_name").ok_or_else(|| py_error("TypeError", "missing asset_name".into()))?.as_string()?;
        // Moonraker's own assets (assets/, identical to moonraker/assets)
        Ok(crate::assets::read_asset(&n).map(|s| py_str(&s)).unwrap_or_else(none))
    });
    r.method(name!("get_asset_path"), |_| {
        let p = crate::plugins::moonraker_dir().join("moonraker").join("assets");
        if p.is_dir() {
            path(&p.to_string_lossy())
        } else {
            Ok(none())
        }
    });
    r.method(name!("_load_release_info_json"), |a| {
        load_release_info_json(arg(a, 0, "dist_info").ok_or_else(|| py_error("TypeError", "missing dist_info".into()))?)
    });
    r.method(name!("_load_direct_url_json"), |a| {
        load_direct_url_json(arg(a, 0, "dist_info").ok_or_else(|| py_error("TypeError", "missing dist_info".into()))?)
    });
    r.method(name!("load_distribution_info"), |a| {
        let venv = a.get(0, "venv_path").ok_or_else(|| py_error("TypeError", "missing venv_path".into()))?;
        let proj = a.get(1, "project_name").ok_or_else(|| py_error("TypeError", "missing project_name".into()))?;
        load_distribution_info(venv, proj)
    });
    r.method(name!("is_vitualenv_project"), |a| {
        let si = import("moonraker.utils.source_info")?;
        let venv_path = match arg(a, 0, "venv_path") {
            Some(v) => v.clone(),
            None => lookup("pathlib", "Path")?.call(&[&lookup("sys", "exec_prefix")?])?,
        };
        let pkg_path = match arg(a, 1, "pkg_path") {
            Some(v) => v.clone(),
            None => si.getattr("package_path")?.call(&[])?,
        };
        let project_name = a.get(2, "project_name").cloned().unwrap_or_else(|| py_str("moonraker"));
        if !pkg_path.call_method("exists", &[])?.is_true()? {
            return Ok(py_bool(false));
        }
        let pkg_info = match load_distribution_info(&venv_path, &project_name) {
            Ok(i) => i,
            Err(_) => return Ok(py_bool(false)),
        };
        let located = pkg_info.getattr("dist_info")?.call_method("locate_file", &[&py_str("")])?;
        let site_path = path(&located.str()?)?;
        let parents = pkg_path.getattr("parents")?;
        let it = lookup("builtins", "iter")?.call(&[&parents])?;
        let next = lookup("builtins", "next")?;
        let sentinel = lookup("builtins", "object")?.call(&[])?;
        loop {
            let parent = next.call(&[&it, &sentinel])?;
            if parent.as_ptr() == sentinel.as_ptr() {
                break;
            }
            if let Ok(same) = site_path.call_method("samefile", &[&parent]) {
                if same.is_true()? {
                    return Ok(py_bool(true));
                }
            }
        }
        // Moonraker returns True here too (source_info.py:186)
        Ok(py_bool(true))
    });
}

// libcamera cameras (Machine._try_import_libcamera / get_libcamera_devices):
// Moonraker imports the system's libcamera python bindings
// (utils.load_system_module). Moonruster runs that same python in a
// short-lived interpreter, and only where the bindings are installed: no
// python, and no memory, anywhere else.

use serde_json::Value;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// utils.SYS_MOD_PATHS: the system python's module directories
const SYS_MOD_GLOBS: [&str; 3] =
    ["/usr/lib/python3*/dist-packages", "/usr/lib/python3*/site-packages", "/usr/lib/*-linux-gnu/python3*/site-packages"];

/// Moonraker's load_system_module("libcamera"), CameraManager.singleton()
/// and, with "list", get_libcamera_devices, verbatim; one JSON line out
const PROBE: &str = r#"import glob, importlib, json, sys, traceback
SYS_MOD_PATHS = glob.glob("/usr/lib/python3*/dist-packages")
SYS_MOD_PATHS += glob.glob("/usr/lib/python3*/site-packages")
SYS_MOD_PATHS += glob.glob("/usr/lib/*-linux-gnu/python3*/site-packages")
def load_system_module(name):
    if not SYS_MOD_PATHS:
        try:
            return importlib.import_module(name)
        except ImportError as e:
            raise Exception(f"Unable to import module {name}") from e
    for module_path in SYS_MOD_PATHS:
        sys.path.insert(0, module_path)
        try:
            module = importlib.import_module(name)
        except ImportError as e:
            if not isinstance(e, ModuleNotFoundError):
                load_errors.append(f"Failed to load {name} module\n" + traceback.format_exc())
        else:
            break
        finally:
            sys.path.pop(0)
    else:
        raise Exception(f"Unable to import module {name}")
    return module
load_errors = []
try:
    libcam = load_system_module("libcamera")
    cm = libcam.CameraManager.singleton()
    out = {"version": str(cm.version)}
    if "list" in sys.argv[1:]:
        libcam_devs = []
        for cam in cm.cameras:
            device = {"libcamera_id": cam.id}
            props_by_name = {cid.name: val for cid, val in cam.properties.items()}
            device["model"] = props_by_name.get("Model")
            modes = []
            cam_config = cam.generate_configuration([libcam.StreamRole.Raw])
            for stream_cfg in cam_config:
                formats = stream_cfg.formats
                for pix_fmt in formats.pixel_formats:
                    cur_mode = {"format": str(pix_fmt)}
                    resolutions = []
                    for size in formats.sizes(pix_fmt):
                        resolutions.append(str(size))
                    cur_mode["resolutions"] = resolutions
                    modes.append(cur_mode)
            device["modes"] = modes
            libcam_devs.append(device)
        out["devices"] = libcam_devs
except Exception:
    out = {"error": traceback.format_exc()}
out["load_errors"] = load_errors
print(json.dumps(out, default=str))
"#;

/// Whether the bindings could be installed: a `libcamera` module in one of
/// the system module directories. With no such directory Moonraker tries a
/// plain import, so the probe has to run.
pub(super) fn bindings_present() -> bool {
    let dirs: Vec<_> = SYS_MOD_GLOBS
        .iter()
        .flat_map(|g| glob::glob(g).into_iter().flatten().flatten())
        .collect();
    dirs.is_empty()
        || dirs.iter().any(|d| {
            d.join("libcamera").join("__init__.py").is_file()
                || d.join("libcamera.py").is_file()
                || glob::glob(&format!("{}/libcamera.*.so", d.display())).map(|mut g| g.next().is_some()).unwrap_or(false)
        })
}

fn python() -> String {
    #[cfg(feature = "plugins")]
    return crate::plugins::python_executable();
    #[cfg(not(feature = "plugins"))]
    return "python3".to_string();
}

/// Runs the probe (blocking, at most 10 s): its JSON result. A module that
/// failed to load is logged as Moonraker logs it; libcamera's own output
/// goes to Moonruster's stderr, as it goes to Moonraker's
pub(super) fn probe(list: bool) -> Result<Value, String> {
    let mut child = Command::new(python())
        .arg("-c")
        .arg(PROBE)
        .args(list.then_some("list"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| e.to_string())?;
    // read the pipe while it runs, so a full pipe can't stall it
    fn drain<R: Read + Send + 'static>(r: Option<R>) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut r) = r {
                let _ = r.read_to_string(&mut s);
            }
            s
        })
    }
    let out = drain(child.stdout.take());
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < Duration::from_secs(10) => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("libcamera probe timed out".into());
            }
        }
    }
    let out = out.join().unwrap_or_default();
    let v: Value = serde_json::from_str(out.trim()).map_err(|e| format!("libcamera probe: {e}: {}", out.trim()))?;
    for e in v["load_errors"].as_array().into_iter().flatten() {
        log::error!("{}", e.as_str().unwrap_or_default().trim_end());
    }
    match v.get("error").and_then(Value::as_str) {
        Some(tb) => Err(tb.to_string()),
        None => Ok(v),
    }
}

// moonraker.components.webcam.WebcamManager and WebCam for Python
// components: the manager's own cameras (the same object each time), their
// fields as attributes
use super::*;
use crate::components::webcam::{WebCam, WebcamManager};
use crate::plugins::futures::rust_future;

macro_rules! wm {
    ($m:literal) => {
        concat!("moonraker.components.webcam.WebcamManager.", $m)
    };
}

macro_rules! cam {
    ($m:literal) => {
        concat!("moonraker.components.webcam.WebCam.", $m)
    };
}

fn cam_object(cam: Rc<WebCam>) -> PyResult<PyRef> {
    objects::wrap_class(cam, "moonraker.components.webcam", "WebCam")
}

/// WebCam's instance attributes (WebCam.__init__), read and written in place
macro_rules! fields {
    ($r:expr, $($f:literal),*) => {
        $(
            $r.attr(
                cam!($f),
                |o| to_py(&objects::unwrap_as::<WebCam>(o)?.get($f)),
                Some(|o, v| {
                    let v = from_py(v)?;
                    objects::unwrap_as::<WebCam>(o)?.fields.borrow_mut().insert($f.to_string(), v);
                    Ok(())
                }),
            );
        )*
    };
}

fn url_arg(a: &Args) -> PyResult<bool> {
    match a.get(0, "convert_local") {
        Some(v) => v.is_true(),
        None => Ok(false),
    }
}

/// The manager's cameras by name (a new dict over the same cameras)
fn webcams(wm: &WebcamManager) -> PyResult<PyRef> {
    let d = lookup("builtins", "dict")?.call(&[])?;
    for (name, cam) in wm.get_webcams() {
        d.call_method("__setitem__", &[&py_str(&name), &cam_object(cam)?])?;
    }
    Ok(d)
}

pub fn register(r: &mut Registry) {
    r.method(wm!("get_webcams"), |a| {
        let wm: Rc<WebcamManager> = a.this()?;
        webcams(&wm)
    });
    r.attr(
        wm!("webcams"),
        |o| {
            let wm: Rc<WebcamManager> = objects::unwrap_as(o)?;
            webcams(&wm)
        },
        None,
    );
    r.method(wm!("get_cam_by_uid"), |a| {
        let uid = a.get(0, "uid").ok_or_else(|| py_error("TypeError", "missing argument 'uid'".into()))?.as_string()?;
        match a.this::<WebcamManager>()?.get_cam_by_uid(&uid) {
            Ok(c) => cam_object(c),
            Err(e) => Err(server_error(&e.message, e.status_code)),
        }
    });
    // as_dict: the public attributes (Moonraker reads them from __dict__)
    r.method(cam!("as_dict"), |a| to_py(&a.this::<WebCam>()?.as_dict()));
    r.method(cam!("get_stream_url"), |a| {
        let cam = a.this::<WebCam>()?;
        let convert = url_arg(a)?;
        rust_future(async move { cam.get_stream_url(convert).await.map(|u| py_str(&u)).map_err(|e| server_error(&e.message, e.status_code)) })
    });
    r.method(cam!("get_snapshot_url"), |a| {
        let cam = a.this::<WebCam>()?;
        let convert = url_arg(a)?;
        rust_future(async move { cam.get_snapshot_url(convert).await.map(|u| py_str(&u)).map_err(|e| server_error(&e.message, e.status_code)) })
    });
    fields!(
        r,
        "name",
        "enabled",
        "icon",
        "aspect_ratio",
        "target_fps",
        "target_fps_idle",
        "location",
        "service",
        "stream_url",
        "snapshot_url",
        "flip_horizontal",
        "flip_vertical",
        "rotation",
        "source",
        "extra_data",
        "uid"
    );
}

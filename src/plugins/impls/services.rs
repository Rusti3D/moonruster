// Small components' API for Python components: job_state, secrets,
// job_queue, power
use super::*;
use crate::components::job_queue::JobQueue;
use crate::components::job_state::JobState;
use crate::components::power::PrinterPower;
use crate::components::secrets::Secrets;
use crate::plugins::futures::rust_future;

fn arg<'a>(a: &'a Args, i: usize, n: &str) -> PyResult<&'a PyRef> {
    a.get(i, n).ok_or_else(|| py_error("TypeError", format!("missing argument '{}'", n)))
}

fn flag(a: &Args, i: usize, n: &str, default: bool) -> PyResult<bool> {
    match a.get(i, n) {
        Some(v) => v.is_true(),
        None => Ok(default),
    }
}

/// a str or a list of str (Moonraker's Union[str, List[str]])
fn str_or_list(v: &PyRef) -> PyResult<Vec<String>> {
    match from_py(v)? {
        Value::String(s) => Ok(vec![s]),
        Value::Array(a) => Ok(a.iter().map(crate::pyfmt::py_str).collect()),
        Value::Null => Ok(vec![]),
        other => Ok(vec![crate::pyfmt::py_str(&other)]),
    }
}

pub fn register(r: &mut Registry) {
    // proc_stats: a callback per stat update, awaited when it returns an
    // awaitable (Moonraker's _handle_stat_update)
    r.method("moonraker.components.proc_stats.ProcStats.register_stat_callback", |a| {
        let cb = arg(a, 0, "callback")?.clone();
        let callback: crate::components::proc_stats::StatCallback = Rc::new(move |seq: u64| {
            match cb.call(&[&py_int(seq as i64)]) {
                Ok(ret) if !ret.is_none() => Some(Box::pin(async move {
                    if let Err(e) = crate::plugins::futures::await_py(ret).await {
                        log::error!("proc_stats callback failed\n{}", e.traceback);
                    }
                })),
                Ok(_) => None,
                Err(e) => {
                    log::error!("proc_stats callback failed\n{}", e.traceback);
                    None
                }
            }
        });
        a.this::<crate::components::proc_stats::ProcStats>()?.register_stat_callback(callback);
        Ok(none())
    });
    // spoolman
    r.method("moonraker.components.spoolman.SpoolManager.set_active_spool", |a| {
        let id = match arg(a, 0, "spool_id")? {
            v if v.is_none() => None,
            v => Some(v.as_i64()?),
        };
        a.this::<crate::components::spoolman::SpoolManager>()?.set_active_spool(id);
        Ok(none())
    });
    r.method("moonraker.components.spoolman.SpoolManager.connected", |a| {
        Ok(py_bool(a.this::<crate::components::spoolman::SpoolManager>()?.connected()))
    });
    // job_state
    r.method("moonraker.components.job_state.JobState.get_last_stats", |a| {
        to_py(&Value::Object(a.this::<JobState>()?.get_last_stats()))
    });
    r.method("moonraker.components.job_state.JobState.get_last_job_event", |a| {
        let ev = a.this::<JobState>()?.get_last_job_event();
        lookup("moonraker.common", "JobEvent")?.call_method("from_string", &[&py_str(ev.as_str())])
    });

    // secrets
    r.method("moonraker.components.secrets.Secrets.get_secrets_file", |a| {
        lookup("pathlib", "Path")?.call(&[&py_str(&a.this::<Secrets>()?.get_secrets_file().to_string_lossy())])
    });
    r.method("moonraker.components.secrets.Secrets.get_type", |a| Ok(py_str(&a.this::<Secrets>()?.get_type())));
    r.method("moonraker.components.secrets.Secrets.get", |a| {
        let key = arg(a, 0, "key")?.as_string()?;
        match a.this::<Secrets>()?.values().get(&key) {
            Some(v) => to_py(v),
            None => Ok(a.get(1, "default").cloned().unwrap_or_else(none)),
        }
    });
    r.method("moonraker.components.secrets.Secrets.__getitem__", |a| {
        let key = arg(a, 0, "key")?.as_string()?;
        match a.this::<Secrets>()?.values().get(&key) {
            Some(v) => to_py(v),
            None => Err(py_error("KeyError", key)),
        }
    });
    r.attr("moonraker.components.secrets.Secrets.values", |o| to_py(&objects::unwrap_as::<Secrets>(o)?.values()), None);

    // job_queue
    r.method("moonraker.components.job_queue.JobQueue.queue_job", |a| {
        let files = str_or_list(arg(a, 0, "filenames")?)?;
        let check = flag(a, 1, "check_exists", true)?;
        let reset = flag(a, 2, "reset", false)?;
        let user = match a.get(3, "user") {
            Some(u) if !u.is_none() => Some(super::application::user_info_from_py(u)?),
            _ => None,
        };
        let q = a.this::<JobQueue>()?;
        rust_future(async move {
            q.queue_job(files, check, reset, user).await.map(|_| none()).map_err(|e| server_error(&e.message, e.status_code))
        })
    });
    r.method("moonraker.components.job_queue.JobQueue.delete_job", |a| {
        let ids = str_or_list(arg(a, 0, "job_ids")?)?;
        let all = flag(a, 1, "all", false)?;
        let q = a.this::<JobQueue>()?;
        rust_future(async move {
            q.delete_job(ids, all).await;
            Ok(none())
        })
    });
    r.method("moonraker.components.job_queue.JobQueue.pause_queue", |a| {
        let q = a.this::<JobQueue>()?;
        rust_future(async move {
            q.pause_queue().await;
            Ok(none())
        })
    });
    r.method("moonraker.components.job_queue.JobQueue.start_queue", |a| {
        let q = a.this::<JobQueue>()?;
        rust_future(async move {
            q.start_queue().await;
            Ok(none())
        })
    });

    // power: set_device_power(device, state, force) with Moonraker's state
    // normalisation (bool, "true"/"false", on/off/toggle)
    r.method("moonraker.components.power.PrinterPower.set_device_power", |a| {
        let device = arg(a, 0, "device")?.as_string()?;
        let state = arg(a, 1, "state")?;
        let force = flag(a, 2, "force", false)?;
        let request = if state.is_instance(&lookup("builtins", "bool")?)? {
            if state.is_true()? { "on" } else { "off" }.to_string()
        } else if state.is_instance(&lookup("builtins", "str")?)? {
            let r = state.as_string()?.to_lowercase();
            match r.as_str() {
                "true" => "on".to_string(),
                "false" => "off".to_string(),
                _ => r,
            }
        } else {
            String::new()
        };
        if !["on", "off", "toggle"].contains(&request.as_str()) {
            log::info!("Invalid state received: {}", state.str()?);
            return Ok(none());
        }
        a.this::<PrinterPower>()?.set_device_power(&device, &request, force);
        Ok(none())
    });
}

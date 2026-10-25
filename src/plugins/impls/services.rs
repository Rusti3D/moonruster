// Small components' API for Python components: job_state, secrets,
// job_queue, power, proc_stats, spoolman, octoprint_compat, wled
use super::*;
use crate::components::job_queue::JobQueue;
use crate::components::job_state::JobState;
use crate::components::power::PrinterPower;
use crate::components::secrets::Secrets;
use crate::plugins::futures::rust_future;

/// a str or a list of str (Moonraker's Union[str, List[str]])
fn str_or_list(v: &PyRef) -> PyResult<Vec<String>> {
    match from_py(v)? {
        Value::String(s) => Ok(vec![s]),
        Value::Array(a) => Ok(a.iter().map(crate::pyfmt::py_str).collect()),
        Value::Null => Ok(vec![]),
        other => Ok(vec![crate::pyfmt::py_str(&other)]),
    }
}

/// A component's power device (a PowerDevice subclass: Moonraker's own
/// base class code runs on it, mirror_verbatim.txt)
struct PyDevice(PyRef);

impl PyDevice {
    fn log(&self, what: &str, e: &PyError) {
        log::error!("power device {}\n{}", what, e.traceback);
    }

    /// call, then await what is awaitable (Moonraker's
    /// `if ret is not None: await ret`)
    async fn call_await(&self, method: &str, args: &[&PyRef]) -> PyResult<PyRef> {
        let ret = self.0.call_method(method, args)?;
        if crate::plugins::futures::is_awaitable(&ret)? {
            return crate::plugins::futures::await_py(ret).await;
        }
        Ok(ret)
    }
}

#[async_trait::async_trait(?Send)]
impl crate::components::power::ExternalDevice for PyDevice {
    fn get_name(&self) -> String {
        self.0.call_method("get_name", &[]).and_then(|n| n.str()).unwrap_or_default()
    }
    fn get_device_info(&self) -> Value {
        self.0.call_method("get_device_info", &[]).and_then(|i| from_py(&i)).unwrap_or_else(|e| {
            self.log("get_device_info", &e);
            Value::Null
        })
    }
    fn process_klippy_shutdown(&self) {
        if let Err(e) = self.0.call_method("process_klippy_shutdown", &[]) {
            self.log("process_klippy_shutdown", &e);
        }
    }
    fn should_turn_on_when_queued(&self) -> bool {
        self.0.call_method("should_turn_on_when_queued", &[]).and_then(|v| v.is_true()).unwrap_or(false)
    }
    async fn initialize(&self) -> Result<bool, crate::common::ServerError> {
        let ret = self.call_await("initialize", &[]).await.map_err(|e| super::application::to_server_error(&e))?;
        ret.is_true().map_err(|e| super::application::to_server_error(&e))
    }
    async fn process_request(&self, req: &str, force: bool) -> Result<String, crate::common::ServerError> {
        let ret = self.call_await("process_request", &[&py_str(req), &py_bool(force)]).await;
        ret.and_then(|r| r.str()).map_err(|e| super::application::to_server_error(&e))
    }
    async fn close(&self) {
        if let Err(e) = self.call_await("close", &[]).await {
            self.log("close", &e);
        }
    }
}

pub fn register(r: &mut Registry) {
    // power.add_device(name, device): a component's PowerDevice
    r.method("moonraker.components.power.PrinterPower.add_device", |a| {
        let name = arg(a, 0, "name")?.as_string()?;
        let dev: Rc<dyn crate::components::power::ExternalDevice> = Rc::new(PyDevice(arg(a, 1, "device")?.clone()));
        let pp = a.this::<PrinterPower>()?;
        rust_future(async move { pp.add_device(&name, dev).await.map(|_| none()).map_err(|e| server_error(&e.message, e.status_code)) })
    });
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
    r.method("moonraker.components.proc_stats.ProcStats.log_last_stats", |a| {
        let count = opt(a, 0, "count").map(|v| v.as_i64()).unwrap_or(Ok(1))?;
        a.this::<crate::components::proc_stats::ProcStats>()?.log_last_stats(count.max(0) as usize);
        Ok(none())
    });
    // dbus_manager
    r.method("moonraker.components.dbus_manager.DbusManager.is_connected", |a| {
        Ok(py_bool(a.this::<crate::components::dbus_manager::DbusManager>()?.is_connected()))
    });
    r.method("moonraker.components.dbus_manager.DbusManager.check_permission", |a| {
        let (action, msg) = (arg(a, 0, "action")?.as_string()?, str_or(a, 1, "err_msg", "")?);
        let d = a.this::<crate::components::dbus_manager::DbusManager>()?;
        rust_future(async move { Ok(py_bool(d.check_permission(&action, &msg).await)) })
    });
    fn dbus_interfaces(a: &Args, names: PyRef) -> PyResult<PyRef> {
        if !a.this::<crate::components::dbus_manager::DbusManager>()?.is_connected() {
            return Err(server_error("Bus not available", 400));
        }
        lookup("_mr", "dbus_interfaces")?.call(&[&py_str(&arg(a, 0, "bus_name")?.as_string()?), &py_str(&arg(a, 1, "bus_path")?.as_string()?), &names])
    }
    r.method("moonraker.components.dbus_manager.DbusManager.get_interfaces", |a| dbus_interfaces(a, arg(a, 2, "interface_names")?.clone()));
    // get_interface: the first of get_interfaces
    r.method("moonraker.components.dbus_manager.DbusManager.get_interface", |a| {
        let names = lookup("builtins", "list")?.call(&[])?;
        names.call_method("append", &[arg(a, 2, "interface_name")?])?;
        let coro = dbus_interfaces(a, names)?;
        rust_future(async move { crate::plugins::futures::await_py(coro).await?.call_method("__getitem__", &[&py_int(0)]) })
    });
    r.method("moonraker.components.ldap.MoonrakerLDAP.authenticate_ldap_user", |a| {
        let (user, pass) = (arg(a, 0, "username")?.as_string()?, arg(a, 1, "password")?.as_string()?);
        let l = a.this::<crate::components::ldap::MoonrakerLdap>()?;
        rust_future(async move { l.authenticate_ldap_user(&user, &pass).await.map(|_| none()).map_err(|e| server_error(&e.message, e.status_code)) })
    });
    r.method("moonraker.components.notifier.Notifier.notify_action", |a| {
        let (name, message) = (arg(a, 0, "name")?.as_string()?, str_or(a, 1, "message", "")?);
        let n = a.this::<crate::components::notifier::Notifier>()?;
        rust_future(async move { n.notify_action(&name, &message).await.map(|_| none()).map_err(|e| server_error(&e.message, e.status_code)) })
    });
    r.attr("moonraker.components.job_state.JobState.last_print_stats", |o| {
        to_py(&Value::Object(objects::unwrap_as::<JobState>(o)?.get_last_stats()))
    }, None);
    r.attr("moonraker.components.klippy_connection.KlippyConnection.subscription_cache", |o| {
        to_py(&Value::Object(objects::unwrap_as::<crate::components::klippy_connection::KlippyConnection>(o)?.get_subscription_cache()))
    }, None);
    // spoolman
    // report_extrusion(eventtime) -> the next report time
    r.method("moonraker.components.spoolman.SpoolManager.report_extrusion", |a| {
        let eventtime = arg(a, 0, "eventtime")?.as_f64()?;
        let s = a.this::<crate::components::spoolman::SpoolManager>()?;
        rust_future(async move {
            let rate = s.sync_rate_seconds() as f64;
            if !s.connected() {
                return Ok(py_float(eventtime + rate));
            }
            s.report_extrusion().await;
            Ok(py_float(crate::utils::monotonic() + rate))
        })
    });
    // octoprint_compat
    r.method("moonraker.components.octoprint_compat.OctoPrintCompat.printer_state", |a| {
        Ok(py_str(&a.this::<crate::components::octoprint_compat::OctoPrintCompat>()?.printer_state()))
    });
    r.method("moonraker.components.octoprint_compat.OctoPrintCompat.printer_temps", |a| {
        to_py(&a.this::<crate::components::octoprint_compat::OctoPrintCompat>()?.printer_temps())
    });
    // wled: the arguments by name, as the remote methods get them
    r.method("moonraker.components.wled.WLED.wled_on", |a| {
        let (strip, preset) = (arg(a, 0, "strip")?.as_string()?, arg(a, 1, "preset")?.as_i64()?);
        let w = a.this::<crate::components::wled::Wled>()?;
        rust_future(async move { w.wled_on(&strip, preset).await.map(|_| none()).map_err(|e| server_error(&e.message, e.status_code)) })
    });
    r.method("moonraker.components.wled.WLED.set_wled_state", |a| {
        let args = named_args(a, &["strip", "state", "preset", "brightness", "intensity", "speed"])?;
        let w = a.this::<crate::components::wled::Wled>()?;
        rust_future(async move { w.set_wled_state(args).await.map(|_| none()).map_err(|e| server_error(&e.message, e.status_code)) })
    });
    r.method("moonraker.components.wled.WLED.set_wled", |a| {
        let args = named_args(a, &["strip", "red", "green", "blue", "white", "index", "transmit"])?;
        let w = a.this::<crate::components::wled::Wled>()?;
        rust_future(async move { w.set_wled(args).await.map(|_| none()).map_err(|e| server_error(&e.message, e.status_code)) })
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
        let check = bool_or(a, 1, "check_exists", true)?;
        let reset = bool_or(a, 2, "reset", false)?;
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
        let all = bool_or(a, 1, "all", false)?;
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
        let force = bool_or(a, 2, "force", false)?;
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

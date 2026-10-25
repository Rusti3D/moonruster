// moonraker.components.analysis.GcodeAnalysis for Python components: the
// estimator command runs under Moonruster's command lock through Moonraker's
// own ShellCommandFactory.exec_cmd, so its errors are Moonraker's
// (ShellCommandError, the JSON decoder's)
use super::*;
use crate::components::analysis::{check_processed, GcodeAnalysis};
use crate::plugins::futures::{await_py, rust_future};
use crate::server::server;

macro_rules! an {
    ($m:literal) => {
        concat!("moonraker.components.analysis.GcodeAnalysis.", $m)
    };
}

fn path_arg(v: Option<&PyRef>) -> PyResult<Option<std::path::PathBuf>> {
    match v {
        Some(p) if !p.is_none() => Ok(Some(std::path::PathBuf::from(p.str()?))),
        _ => Ok(None),
    }
}

/// shell_command.exec_cmd(cmd, timeout) of Moonraker's factory
async fn exec_cmd(cmd: &str, timeout: i64) -> PyResult<PyRef> {
    let scmd = super::server::component_object("shell_command", server().component_any("shell_command").expect("shell_command is core"))?;
    let kw = to_py(&serde_json::json!({ "timeout": timeout }))?;
    await_py(scmd.getattr("exec_cmd")?.call_kw(&[&py_str(cmd)], &kw)?).await
}

pub fn register(r: &mut Registry) {
    // estimator_version_tuple(): "v3.7.3" -> (3, 7, 3), as int() parses
    r.method(an!("estimator_version_tuple"), |a| {
        let v = a.this::<GcodeAnalysis>()?.estimator_version();
        let tuple_cls = lookup("builtins", "tuple")?;
        if v == "?" || v.is_empty() {
            return tuple_cls.call(&[]);
        }
        let s = v.strip_prefix('v').unwrap_or(&v);
        let parts = lookup("builtins", "list")?.call(&[])?;
        for p in s.split('.') {
            parts.call_method("append", &[&lookup("builtins", "int")?.call(&[&py_str(p)])?])?;
        }
        tuple_cls.call(&[&parts])
    });
    r.method(an!("estimate_file"), |a| {
        let an = a.this::<GcodeAnalysis>()?;
        let gc = path_arg(a.get(0, "gc_path"))?.ok_or_else(|| py_error("TypeError", "missing argument 'gc_path'".into()))?;
        let est = path_arg(a.get(1, "est_config"))?;
        rust_future(async move {
            let _g = an.cmd_lock().lock().await;
            let err = |e: crate::common::ServerError| server_error(&e.message, e.status_code);
            let est = an.check_estimate_args(&gc, est.as_deref()).map_err(err)?;
            let cmd = an.gen_estimator_cmd(&gc, &est, false).map_err(err)?;
            let ret = exec_cmd(&cmd, an.estimator_timeout()).await?;
            let data = lookup("moonraker.utils.json_wrapper", "loads")?.call(&[&ret])?;
            data.call_method("__getitem__", &[&py_str("sequences")])?.call_method("__getitem__", &[&py_int(0)])
        })
    });
    r.method(an!("post_process_file"), |a| {
        let an = a.this::<GcodeAnalysis>()?;
        let gc = path_arg(a.get(0, "gc_path"))?.ok_or_else(|| py_error("TypeError", "missing argument 'gc_path'".into()))?;
        let est = path_arg(a.get(1, "est_config"))?;
        let force = bool_or(a, 2, "force", false)?;
        rust_future(async move {
            let _g = an.cmd_lock().lock().await;
            let err = |e: crate::common::ServerError| server_error(&e.message, e.status_code);
            let est = an.check_estimate_args(&gc, est.as_deref()).map_err(err)?;
            let mut proc_ver = check_processed(&gc);
            let processed = proc_ver.is_some();
            let mut bypassed = processed;
            if !processed || force {
                let cmd = an.gen_estimator_cmd(&gc, &est, true).map_err(err)?;
                exec_cmd(&cmd, an.estimator_timeout()).await?;
                proc_ver = Some(an.estimator_version());
                bypassed = false;
            } else {
                log::info!("File {} already processed, aborting", gc.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
            }
            to_py(&serde_json::json!({"prev_processed": processed, "version": proc_ver, "bypassed": bypassed}))
        })
    });
}

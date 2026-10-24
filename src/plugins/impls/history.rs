// moonraker.components.history.History / PrinterJob and
// moonraker.common.HistoryFieldData / its trackers for Python components:
// the job being recorded (the same object while it runs), auxiliary fields a
// component registers (its reset callback in Python), their trackers
use super::*;
use crate::components::history::{History, HistoryFieldData, PrinterJob, TrackingStrategy};
use std::any::Any;

macro_rules! hist {
    ($m:literal) => {
        concat!("moonraker.components.history.History.", $m)
    };
}

macro_rules! job {
    ($m:literal) => {
        concat!("moonraker.components.history.PrinterJob.", $m)
    };
}

macro_rules! field {
    ($m:literal) => {
        concat!("moonraker.common.HistoryFieldData.", $m)
    };
}

fn arg<'a>(a: &'a Args, i: usize, n: &str) -> PyResult<&'a PyRef> {
    a.get(i, n).ok_or_else(|| py_error("TypeError", format!("missing argument '{}'", n)))
}

fn flag(a: &Args, i: usize, n: &str) -> PyResult<bool> {
    match a.get(i, n) {
        Some(v) => v.is_true(),
        None => Ok(false),
    }
}

fn opt_str(a: &Args, i: usize, n: &str) -> PyResult<Option<String>> {
    match a.get(i, n) {
        Some(v) if !v.is_none() => Ok(Some(v.as_string()?)),
        _ => Ok(None),
    }
}

/// A Python reset callback as the tracker's: its value converted, an
/// exception logged (the tracker keeps its reset value)
fn reset_callback(cb: PyRef) -> crate::components::history::ResetCallback {
    Rc::new(move || match cb.call(&[]).and_then(|v| from_py(&v)) {
        Ok(v) => v,
        Err(e) => {
            log::error!("History field reset callback failed\n{}", e.traceback);
            Value::Null
        }
    })
}

/// HistoryFieldData.tracker: the field's tracker as an object of its own
/// (one per field), of the class Moonraker's strategy creates
pub struct TrackerHandle(pub Rc<HistoryFieldData>);

thread_local! {
    static TRACKERS: std::cell::RefCell<std::collections::HashMap<usize, Rc<TrackerHandle>>> = Default::default();
}

fn tracker_object(field: &Rc<HistoryFieldData>) -> PyResult<PyRef> {
    let key = Rc::as_ptr(field) as usize;
    let h = TRACKERS.with(|t| t.borrow_mut().entry(key).or_insert_with(|| Rc::new(TrackerHandle(field.clone()))).clone());
    let class = match field.strategy() {
        TrackingStrategy::Basic => "BasicTracker",
        TrackingStrategy::Delta => "DeltaTracker",
        TrackingStrategy::Accumulate => "CumulativeTracker",
        TrackingStrategy::Average => "AveragingTracker",
        TrackingStrategy::Maximum => "MaximumTracker",
        TrackingStrategy::Minimum => "MinimumTracker",
        TrackingStrategy::Collect => "CollectionTracker",
    };
    objects::wrap_class(h as Rc<dyn Any>, "moonraker.common", class)
}

fn job_object(job: Rc<PrinterJob>) -> PyResult<PyRef> {
    objects::wrap_class(job, "moonraker.components.history", "PrinterJob")
}

/// PrinterJob's instance attributes, read and written in place
macro_rules! job_fields {
    ($r:expr, $($f:literal),*) => {
        $(
            $r.attr(
                job!($f),
                |o| to_py(&objects::unwrap_as::<PrinterJob>(o)?.get($f)),
                Some(|o, v| {
                    let v = from_py(v)?;
                    objects::unwrap_as::<PrinterJob>(o)?.fields.borrow_mut().insert($f.to_string(), v);
                    Ok(())
                }),
            );
        )*
    };
}

fn tracker_field(a: &Args) -> PyResult<Rc<HistoryFieldData>> {
    Ok(a.this::<TrackerHandle>()?.0.clone())
}

pub fn register(r: &mut Registry) {
    r.attr(
        hist!("current_job"),
        |o| match objects::unwrap_as::<History>(o)?.current_job() {
            Some(j) => job_object(j),
            None => Ok(none()),
        },
        None,
    );
    r.attr(
        hist!("current_job_id"),
        |o| Ok(objects::unwrap_as::<History>(o)?.current_job_id().map(py_int).unwrap_or_else(none)),
        None,
    );
    r.method(hist!("register_auxiliary_field"), |a| {
        let field = objects::unwrap_as::<HistoryFieldData>(arg(a, 0, "new_field")?)?;
        a.this::<History>()?.register_auxiliary_field(field).map_err(|e| server_error(&e.message, e.status_code))?;
        Ok(none())
    });

    job_fields!(r, "end_time", "filament_used", "filename", "metadata", "print_duration", "status", "start_time", "total_duration", "auxiliary_data", "user");
    r.method(job!("get"), |a| {
        let name = arg(a, 0, "name")?.as_string()?;
        let job = a.this::<PrinterJob>()?;
        if job.has(&name) {
            to_py(&job.get(&name))
        } else {
            Ok(none())
        }
    });
    r.method(job!("set"), |a| {
        let name = arg(a, 0, "name")?.as_string()?;
        a.this::<PrinterJob>()?.set(&name, from_py(arg(a, 1, "val")?)?);
        Ok(none())
    });
    r.method(job!("get_stats"), |a| to_py(&Value::Object(a.this::<PrinterJob>()?.fields.borrow().clone())));
    r.method(job!("update_from_ps"), |a| {
        if let Value::Object(m) = from_py(arg(a, 0, "data")?)? {
            a.this::<PrinterJob>()?.update_from_ps(&m);
        }
        Ok(none())
    });
    r.method(job!("finish"), |a| {
        let status = match a.get(0, "status") {
            Some(s) if !s.is_none() => s.as_string()?,
            _ => "error".to_string(),
        };
        let ps = match a.get(1, "print_stats") {
            Some(p) => match from_py(p)? {
                Value::Object(m) => m,
                _ => Default::default(),
            },
            None => Default::default(),
        };
        a.this::<PrinterJob>()?.finish(&status, &ps);
        Ok(none())
    });
    r.method(job!("set_aux_data"), |a| {
        let fields = arg(a, 0, "fields")?;
        let n = fields.call_method("__len__", &[])?.as_i64()?;
        let mut data = Vec::new();
        for i in 0..n {
            let f = objects::unwrap_as::<HistoryFieldData>(&fields.call_method("__getitem__", &[&py_int(i)])?)?;
            data.push(f.as_dict());
        }
        a.this::<PrinterJob>()?.fields.borrow_mut().insert("auxiliary_data".into(), Value::Array(data));
        Ok(none())
    });

    r.ctor(field!("__init__"), |a| {
        let cb = match a.get(5, "reset_callback") {
            Some(c) if !c.is_none() => Some(reset_callback(c.clone())),
            _ => None,
        };
        let precision = match a.get(9, "precision") {
            Some(p) if !p.is_none() => Some(p.as_i64()? as usize),
            _ => None,
        };
        let f = HistoryFieldData::new(
            &arg(a, 0, "field_name")?.as_string()?,
            &arg(a, 1, "provider")?.as_string()?,
            &arg(a, 2, "desc")?.as_string()?,
            &arg(a, 3, "strategy")?.as_string()?,
            opt_str(a, 4, "units")?.as_deref(),
            cb,
            flag(a, 6, "exclude_paused")?,
            flag(a, 7, "report_total")?,
            flag(a, 8, "report_maximum")?,
            precision,
        )
        // TrackingStrategy.from_string's error
        .map_err(|e| py_error("ValueError", e.message))?;
        Ok(f as Rc<dyn Any>)
    });
    r.method(field!("name"), |a| Ok(py_str(a.this::<HistoryFieldData>()?.name())));
    r.method(field!("provider"), |a| Ok(py_str(a.this::<HistoryFieldData>()?.provider())));
    r.method(field!("tracker"), |a| tracker_object(&a.this::<HistoryFieldData>()?));
    r.method(field!("__eq__"), |a| {
        let me = a.this::<HistoryFieldData>()?;
        match objects::unwrap_as::<HistoryFieldData>(arg(a, 0, "value")?) {
            Ok(other) => Ok(py_bool(other.provider() == me.provider() && other.name() == me.name())),
            Err(_) => Err(py_error("ValueError", "Invalid type for comparison".into())),
        }
    });
    r.method(field!("get_configuration"), |a| to_py(&a.this::<HistoryFieldData>()?.get_configuration()));
    r.method(field!("as_dict"), |a| to_py(&a.this::<HistoryFieldData>()?.as_dict()));
    r.method(field!("has_totals"), |a| Ok(py_bool(a.this::<HistoryFieldData>()?.has_totals())));
    r.method(field!("get_totals"), |a| {
        let last = match from_py(arg(a, 0, "last_totals")?)? {
            Value::Array(v) => v,
            _ => vec![],
        };
        to_py(&a.this::<HistoryFieldData>()?.get_totals(&last, flag(a, 1, "reset")?))
    });

    // the trackers: every class's methods act on the field's tracker
    macro_rules! tracker_methods {
        ($($c:literal),*) => {
            $(
                r.method(concat!("moonraker.common.", $c, ".update"), |a| {
                    tracker_field(a)?.update(from_py(arg(a, 0, "value")?)?);
                    Ok(none())
                });
                r.method(concat!("moonraker.common.", $c, ".reset"), |a| {
                    tracker_field(a)?.reset_tracker();
                    Ok(none())
                });
            )*
        };
    }
    tracker_methods!("FieldTracker", "BasicTracker", "DeltaTracker", "CumulativeTracker", "AveragingTracker", "MaximumTracker", "MinimumTracker", "CollectionTracker");
    // has_totals where Moonraker's class defines it (the others inherit)
    macro_rules! has_totals {
        ($($c:literal),*) => {
            $(
                r.method(concat!("moonraker.common.", $c, ".has_totals"), |a| {
                    Ok(py_bool(tracker_field(a)?.tracker.borrow().has_totals()))
                });
            )*
        };
    }
    has_totals!("FieldTracker", "BasicTracker", "DeltaTracker", "CumulativeTracker", "CollectionTracker");
    r.method("moonraker.common.FieldTracker.get_tracked_value", |a| to_py(&tracker_field(a)?.tracker.borrow().tracked_value));
    r.method("moonraker.common.FieldTracker.set_reset_callback", |a| {
        let cb = match a.get(0, "cb") {
            Some(c) if !c.is_none() => Some(reset_callback(c.clone())),
            _ => None,
        };
        tracker_field(a)?.tracker.borrow_mut().reset_callback = cb;
        Ok(none())
    });
    r.method("moonraker.common.FieldTracker.set_exclude_paused", |a| {
        tracker_field(a)?.tracker.borrow_mut().exclude_paused = flag(a, 0, "exclude")?;
        Ok(none())
    });
}

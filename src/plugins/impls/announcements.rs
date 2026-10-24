// moonraker.components.announcements.Announcements for Python components
use super::*;
use crate::components::announcements::Announcements;
use crate::plugins::futures::rust_future;

macro_rules! ann {
    ($m:literal) => {
        concat!("moonraker.components.announcements.Announcements.", $m)
    };
}

fn arg<'a>(a: &'a Args, i: usize, n: &str) -> PyResult<&'a PyRef> {
    a.get(i, n).ok_or_else(|| py_error("TypeError", format!("missing argument '{}'", n)))
}

pub fn register(r: &mut Registry) {
    r.method(ann!("add_internal_announcement"), |a| {
        let s = |i, n| -> PyResult<String> { arg(a, i, n)?.as_string() };
        let entry = a.this::<Announcements>()?.add_internal_announcement(
            &s(0, "title")?,
            &s(1, "desc")?,
            &s(2, "url")?,
            &s(3, "priority")?,
            &s(4, "feed")?,
        );
        to_py(&entry)
    });
    r.method(ann!("remove_announcement"), |a| {
        let id = arg(a, 0, "entry_id")?.as_string()?;
        let ann = a.this::<Announcements>()?;
        rust_future(async move { ann.remove_announcement(&id).await.map(|_| none()).map_err(|e| server_error(&e.message, e.status_code)) })
    });
    r.method(ann!("dismiss_announcement"), |a| {
        let id = arg(a, 0, "entry_id")?.as_string()?;
        let wake = match a.get(1, "wake_time") {
            Some(w) if !w.is_none() => Some(w.as_i64()?),
            _ => None,
        };
        let ann = a.this::<Announcements>()?;
        rust_future(async move { ann.dismiss_announcement(&id, wake).await.map(|_| none()).map_err(|e| server_error(&e.message, e.status_code)) })
    });
    r.method(ann!("get_announcements"), |a| {
        let inc = match a.get(0, "include_dismissed") {
            Some(v) => v.is_true()?,
            None => false,
        };
        let ann = a.this::<Announcements>()?;
        rust_future(async move { to_py(&ann.get_announcements(inc).await) })
    });
    r.method("moonraker.components.announcements.Announcements.register_feed", |a| {
        let name = a.get(0, "name").ok_or_else(|| py_error("TypeError", "missing argument 'name'".into()))?.as_string()?;
        a.this::<Announcements>()?.register_feed(&name);
        Ok(none())
    });
}

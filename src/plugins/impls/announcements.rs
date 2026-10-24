// moonraker.components.announcements.Announcements for Python components
use super::*;
use crate::components::announcements::Announcements;

pub fn register(r: &mut Registry) {
    r.method("moonraker.components.announcements.Announcements.register_feed", |a| {
        let name = a.get(0, "name").ok_or_else(|| py_error("TypeError", "missing argument 'name'".into()))?.as_string()?;
        a.this::<Announcements>()?.register_feed(&name);
        Ok(none())
    });
}

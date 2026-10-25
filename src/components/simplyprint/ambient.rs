// Ambient temperature detection

use super::*;

impl SimplyPrint {
    pub(super) fn update_ambient(&self, sensor: &Value, eventtime: f64) {
        let temp = match sensor.get("temperature").and_then(|v| v.as_f64()) {
            None => return,
            Some(t) => t,
        };
        if eventtime < self.amb.last_sample_time.get() + SAMPLE_CHECK_TIME {
            return;
        }
        self.amb.last_sample_time.set(eventtime);
        let new_amb = (temp + 0.5) as i64;
        if (new_amb - self.amb.ambient.get()).abs() < 2 {
            return;
        }
        self.amb.ambient.set(new_amb);
        self.on_ambient_changed(new_amb);
    }

    pub(super) fn on_ambient_changed(&self, new: i64) {
        self.save_item("ambient_temp", json!(new));
        self.send_sp("ambient", json!({"new": new}));
    }

    pub(super) fn amb_start(self: &Rc<Self>) {
        if self.amb.timer.is_running() {
            return;
        }
        let t0 = self.cache.borrow().temps.get("tool0").and_then(|v| v.get(0)).and_then(|v| v.as_i64());
        if let Some(cur) = t0 {
            if cur < self.amb.ambient.get() {
                self.amb.ambient.set(cur);
                self.on_ambient_changed(cur);
            }
        }
        let w = Rc::downgrade(self);
        self.amb.timer.start(
            0.0,
            Rc::new(move || match w.upgrade() {
                Some(s) => s.handle_detect_timer(now()),
                None => 3600.0,
            }),
        );
    }

    pub(super) fn amb_stop(&self) {
        self.amb.timer.stop();
        self.amb.last_sample_time.set(0.0);
    }

    pub(super) fn handle_detect_timer(&self, eventtime: f64) -> f64 {
        let a = &self.amb;
        let t0 = self.cache.borrow().temps.get("tool0").cloned();
        let (temp, target) = match t0.as_ref().and_then(|v| v.as_array()) {
            None => {
                a.initial_sample.set(-1000.0);
                return AMB_CHECK_INTERVAL;
            }
            Some(arr) => (arr.first().and_then(|v| v.as_f64()).unwrap_or(0.0), arr.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0)),
        };
        if target != 0.0 {
            a.initial_sample.set(-1000.0);
            a.last_sample_time.set(eventtime);
            a.update_interval.set(TARGET_CHECK_TIME);
            return AMB_CHECK_INTERVAL;
        }
        if eventtime - a.last_sample_time.get() < a.update_interval.get() {
            return AMB_CHECK_INTERVAL;
        }
        if a.initial_sample.get() == -1000.0 {
            a.initial_sample.set(temp);
            a.update_interval.set(SAMPLE_CHECK_TIME);
        } else {
            let diff = (temp - a.initial_sample.get()).abs();
            if diff <= 2.0 {
                let last = a.ambient.get();
                a.ambient.set(((temp + a.initial_sample.get()) / 2.0 + 0.5) as i64);
                a.initial_sample.set(-1000.0);
                a.last_sample_time.set(eventtime);
                a.update_interval.set(AMBIENT_CHECK_TIME);
                if last != a.ambient.get() {
                    log::debug!("SimplyPrint: New Ambient: {}", a.ambient.get());
                    self.on_ambient_changed(a.ambient.get());
                }
            } else {
                a.initial_sample.set(temp);
                a.update_interval.set(SAMPLE_CHECK_TIME);
            }
        }
        AMB_CHECK_INTERVAL
    }
}

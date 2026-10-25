// moonraker.components.gpio for Python components: the factory's
// setup_gpio_out / register_gpio_event and the pins (GpioOutputPin,
// GpioEvent) on Moonruster's GPIO lines
use super::*;
use crate::components::gpio::{GpioEvent, GpioFactory, GpioOutputPin, PinInfo};
use crate::plugins::futures::{await_py, is_awaitable};

macro_rules! gpio {
    ($m:literal) => {
        concat!("moonraker.components.gpio.", $m)
    };
}

enum Pin {
    Out(Rc<GpioOutputPin>),
    Event(Rc<GpioEvent>),
}

impl Pin {
    fn info(&self) -> &PinInfo {
        match self {
            Pin::Out(p) => &p.info,
            Pin::Event(e) => &e.info,
        }
    }
}

fn pin(a: &Args) -> PyResult<Pin> {
    let slf = a.slf.as_ref().expect("method without self");
    let rust = objects::unwrap(slf)?;
    if let Ok(p) = rust.clone().downcast::<GpioOutputPin>() {
        return Ok(Pin::Out(p));
    }
    rust.downcast::<GpioEvent>().map(Pin::Event).map_err(|_| py_error("TypeError", "moonruster: not a GPIO".into()))
}

fn err(e: crate::common::ServerError) -> PyError {
    server_error(&e.message, e.status_code)
}

pub fn register(r: &mut Registry) {
    r.method(gpio!("GpioFactory.setup_gpio_out"), |a| {
        let name = arg(a, 0, "pin_name")?.as_string()?;
        let initial = opt(a, 1, "initial_value").map(|v| v.as_i64()).unwrap_or(Ok(0))?;
        let p = a.this::<GpioFactory>()?.setup_gpio_out(&name, initial as i32).map_err(err)?;
        objects::wrap_class(p, "moonraker.components.gpio", "GpioOutputPin")
    });
    // register_gpio_event(pin_name, callback(eventtime, elapsed, value)): an
    // awaitable result runs as a task (GpioEvent._run_callback)
    r.method(gpio!("GpioFactory.register_gpio_event"), |a| {
        let name = arg(a, 0, "pin_name")?.as_string()?;
        let cb = arg(a, 1, "callback")?.clone();
        let callback: crate::components::gpio::GpioEventCallback = Rc::new(move |eventtime, elapsed, value| {
            let r = cb.call(&[&py_float(eventtime), &py_float(elapsed), &py_int(value as i64)]).and_then(|ret| {
                if is_awaitable(&ret)? {
                    crate::server::spawn(async move {
                        if let Err(e) = await_py(ret).await {
                            log::error!("GPIO event callback failed\n{}", e.traceback);
                        }
                    });
                }
                Ok(())
            });
            if let Err(e) = r {
                log::error!("GPIO event callback failed\n{}", e.traceback);
            }
        });
        let e = a.this::<GpioFactory>()?.register_gpio_event(&name, callback).map_err(err)?;
        objects::wrap_class(e, "moonraker.components.gpio", "GpioEvent")
    });
    fn attr_pin(o: &PyRef) -> PyResult<Pin> {
        let rust = objects::unwrap(o)?;
        if let Ok(p) = rust.clone().downcast::<GpioOutputPin>() {
            return Ok(Pin::Out(p));
        }
        rust.downcast::<GpioEvent>().map(Pin::Event).map_err(|_| py_error("TypeError", "moonruster: not a GPIO".into()))
    }
    r.attr(gpio!("GpioBase.name"), |o| Ok(py_str(&attr_pin(o)?.info().name)), None);
    r.attr(gpio!("GpioBase.orig"), |o| Ok(py_str(&attr_pin(o)?.info().orig)), None);
    r.attr(gpio!("GpioBase.inverted"), |o| Ok(py_bool(attr_pin(o)?.info().inverted)), None);
    r.attr(
        gpio!("GpioBase.value"),
        |o| {
            Ok(py_int(match attr_pin(o)? {
                Pin::Out(p) => p.get_value(),
                Pin::Event(e) => e.get_value(),
            } as i64))
        },
        None,
    );
    r.method(gpio!("GpioBase.is_inverted"), |a| Ok(py_bool(pin(a)?.info().inverted)));
    r.method(gpio!("GpioBase.get_name"), |a| Ok(py_str(&pin(a)?.info().name)));
    r.method(gpio!("GpioBase.__str__"), |a| Ok(py_str(&pin(a)?.info().orig)));
    r.method(gpio!("GpioBase.get_value"), |a| {
        Ok(py_int(match pin(a)? {
            Pin::Out(p) => p.get_value(),
            Pin::Event(e) => e.get_value(),
        } as i64))
    });
    r.method(gpio!("GpioBase.close"), |a| {
        match pin(a)? {
            Pin::Out(p) => p.close(),
            Pin::Event(e) => e.close(),
        }
        Ok(none())
    });
    r.method(gpio!("GpioOutputPin.write"), |a| {
        // int(not not value)
        let v = arg(a, 0, "value")?.is_true()?;
        a.this::<GpioOutputPin>()?.write(v as i32).map_err(err)?;
        Ok(none())
    });
    r.method(gpio!("GpioEvent.setup_debounce"), |a| {
        let period = arg(a, 0, "debounce_period")?.as_f64()?;
        let on_error: Option<Rc<dyn Fn(String)>> = opt(a, 1, "err_callback").map(|cb| {
            let cb = cb.clone();
            Rc::new(move |msg: String| {
                if let Err(e) = cb.call(&[&py_str(&msg)]) {
                    log::error!("GPIO error callback failed\n{}", e.traceback);
                }
            }) as Rc<dyn Fn(String)>
        });
        a.this::<GpioEvent>()?.setup_debounce(period, on_error);
        Ok(none())
    });
    r.method(gpio!("GpioEvent.start"), |a| {
        a.this::<GpioEvent>()?.start();
        Ok(none())
    });
    r.method(gpio!("GpioEvent.stop"), |a| {
        a.this::<GpioEvent>()?.stop();
        Ok(none())
    });
    r.method(gpio!("GpioEvent.close"), |a| {
        a.this::<GpioEvent>()?.close();
        Ok(none())
    });
    r.method(gpio!("GpioEvent.fileno"), |a| Ok(py_int(a.this::<GpioEvent>()?.fileno() as i64)));
}

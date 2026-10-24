// moonraker.components.mqtt.MQTTClient for Python components:
// publish / subscribe as Moonraker's client does (payload encoding, a
// message delivered with event_loop.register_callback(callback, payload)).
// publish_topic_with_response is Moonraker's own code (mirror_verbatim.txt)
use super::*;
use crate::components::mqtt::{MqttClient, SubscriptionHandle};
use crate::plugins::futures::rust_future;

macro_rules! mq {
    ($m:literal) => {
        concat!("moonraker.components.mqtt.MQTTClient.", $m)
    };
}

fn arg<'a>(a: &'a Args, i: usize, n: &str) -> PyResult<&'a PyRef> {
    a.get(i, n).ok_or_else(|| py_error("TypeError", format!("missing argument '{}'", n)))
}

/// qos argument: `qos or self.qos`, and Moonraker's range check
fn qos(a: &Args, i: usize, c: &MqttClient) -> PyResult<u8> {
    let q = match a.get(i, "qos") {
        Some(q) if !q.is_none() && q.is_true()? => q.as_i64()?,
        _ => c.qos() as i64,
    };
    if !(0..=2).contains(&q) {
        return Err(server_error("QOS must be between 0 and 2", 400));
    }
    Ok(q as u8)
}

/// the payload as paho sends it: bytes as they are, dict / list through
/// Moonraker's json_wrapper, bool lowercased, None empty, anything else str()
fn payload(p: Option<&PyRef>) -> PyResult<Vec<u8>> {
    let p = match p {
        Some(p) if !p.is_none() => p,
        _ => return Ok(Vec::new()),
    };
    let is = |n: &str| -> PyResult<bool> { p.is_instance(&lookup("builtins", n)?) };
    let text = if is("dict")? || is("list")? {
        let enc = lookup("moonraker.utils.json_wrapper", "dumps")
            .and_then(|d| d.call(&[p]))
            .map_err(|_| server_error("Dict or List is not json encodable", 400))?;
        if enc.is_instance(&lookup("builtins", "bytes")?)? {
            return bytes_of(&enc);
        }
        enc.as_string()?
    } else if is("bool")? {
        if p.is_true()? { "true" } else { "false" }.to_string()
    } else if is("bytes")? || is("bytearray")? {
        return bytes_of(p);
    } else {
        p.str()?
    };
    Ok(text.into_bytes())
}

fn bytes_of(b: &PyRef) -> PyResult<Vec<u8>> {
    match from_py(&lookup("builtins", "list")?.call(&[b])?)? {
        Value::Array(v) => Ok(v.iter().map(|x| x.as_u64().unwrap_or(0) as u8).collect()),
        _ => Ok(vec![]),
    }
}

pub fn register(r: &mut Registry) {
    r.method(mq!("is_connected"), |a| Ok(py_bool(a.this::<MqttClient>()?.is_connected())));
    r.method(mq!("get_instance_name"), |a| Ok(py_str(&a.this::<MqttClient>()?.get_instance_name())));
    r.attr(mq!("qos"), |o| Ok(py_int(objects::unwrap_as::<MqttClient>(o)?.qos() as i64)), None);
    r.attr(mq!("eventloop"), |_| super::server::event_loop(), None);
    r.attr(mq!("server"), |_| objects::wrap(crate::server::server()), None);
    // MQTTClient.pending_responses: Moonraker's own list (its
    // publish_topic_with_response keeps its response futures there)
    r.attr(
        mq!("pending_responses"),
        |_| {
            thread_local! {
                static PENDING: std::cell::RefCell<Option<PyRef>> = const { std::cell::RefCell::new(None) };
            }
            if let Some(l) = PENDING.with(|p| p.borrow().clone()) {
                return Ok(l);
            }
            let l = lookup("builtins", "list")?.call(&[])?;
            PENDING.with(|p| *p.borrow_mut() = Some(l.clone()));
            Ok(l)
        },
        None,
    );
    r.method(mq!("wait_connection"), |a| {
        let timeout = match a.get(0, "timeout") {
            Some(t) if !t.is_none() => Some(t.as_f64()?),
            _ => None,
        };
        let c = a.this::<MqttClient>()?;
        rust_future(async move { Ok(py_bool(c.wait_connection(timeout).await)) })
    });
    r.method(mq!("subscribe_topic"), |a| {
        let topic = arg(a, 0, "topic")?.as_string()?;
        let cb = arg(a, 1, "callback")?.clone();
        let c = a.this::<MqttClient>()?;
        let q = match a.get(2, "qos") {
            Some(q) if !q.is_none() => Some(q.as_i64()?),
            _ => None,
        };
        if topic.contains('#') || topic.contains('+') {
            return Err(server_error("Wildcards may not be used", 400));
        }
        if q.is_some_and(|q| !(0..=2).contains(&q)) {
            return Err(server_error("QOS must be between 0 and 2", 400));
        }
        let callback: crate::components::mqtt::MqttCallback = Rc::new(move |data: Vec<u8>| {
            let r = py_bytes(&data).and_then(|b| super::server::event_loop()?.call_method("register_callback", &[&cb, &b]));
            if let Err(e) = r {
                log::error!("moonruster: MQTT subscription callback\n{}", e.traceback);
            }
        });
        let hdl = c.subscribe_topic(&topic, callback, q.map(|q| q as u8)).map_err(|e| server_error(&e.message, e.status_code))?;
        objects::wrap_class(hdl, "moonraker.components.mqtt", "SubscriptionHandle")
    });
    r.attr("moonraker.components.mqtt.SubscriptionHandle.topic", |o| Ok(py_str(&objects::unwrap_as::<SubscriptionHandle>(o)?.topic)), None);
    r.method(mq!("unsubscribe"), |a| {
        let hdl = objects::unwrap_as::<SubscriptionHandle>(arg(a, 0, "hdl")?)?;
        a.this::<MqttClient>()?.unsubscribe(&hdl);
        Ok(none())
    });
    r.method(mq!("publish_topic"), |a| {
        let topic = arg(a, 0, "topic")?.as_string()?;
        let c = a.this::<MqttClient>()?;
        let q = qos(a, 2, &c)?;
        let data = payload(a.get(1, "payload"))?;
        let retain = match a.get(3, "retain") {
            Some(v) => v.is_true()?,
            None => false,
        };
        let fut = c.publish_bytes(&topic, data, Some(q), retain);
        rust_future(async move { fut.await.map(|_| none()).map_err(|e| server_error(&e.message, e.status_code)) })
    });
}

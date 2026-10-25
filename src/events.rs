// Moonraker's events with Rust handlers, one type each: the handlers get the
// typed arguments, no JSON. The values Moonraker passes (websocket
// notifications, Python handlers) are made from them only when asked for,
// once per send. An event a Python component sends is decoded from its
// values (from_values) for the Rust handlers.
use crate::common::{ApiTransport, JobEvent, KlippyState, UserInfo};
use crate::server::EventObject;
use serde_json::{json, Map, Value};
use std::rc::Rc;

pub trait Event: 'static {
    const NAME: &'static str;
    type Args: 'static;
    /// The arguments as Moonraker passes them
    fn values(args: &Self::Args) -> Vec<Value>;
    /// The arguments from values (a Python component's send_event)
    fn from_values(values: &[Value]) -> Option<Self::Args>;
    /// The first argument when Moonraker's is an object (Python handlers)
    fn object(_args: &Self::Args) -> Option<EventObject> {
        None
    }
}

/// An event without arguments
macro_rules! unit_event {
    ($($ty:ident = $name:literal;)*) => {$(
        pub struct $ty;
        impl Event for $ty {
            const NAME: &'static str = $name;
            type Args = ();
            fn values(_: &()) -> Vec<Value> {
                Vec::new()
            }
            fn from_values(_: &[Value]) -> Option<()> {
                Some(())
            }
        }
    )*};
}

/// An event with one JSON value (a dict Moonraker builds)
macro_rules! value_event {
    ($($ty:ident = $name:literal;)*) => {$(
        pub struct $ty;
        impl Event for $ty {
            const NAME: &'static str = $name;
            type Args = Value;
            fn values(v: &Value) -> Vec<Value> {
                vec![v.clone()]
            }
            fn from_values(values: &[Value]) -> Option<Value> {
                values.first().cloned()
            }
        }
    )*};
}

/// An event with one string
macro_rules! string_event {
    ($($ty:ident = $name:literal;)*) => {$(
        pub struct $ty;
        impl Event for $ty {
            const NAME: &'static str = $name;
            type Args = String;
            fn values(s: &String) -> Vec<Value> {
                vec![json!(s)]
            }
            fn from_values(values: &[Value]) -> Option<String> {
                values.first()?.as_str().map(String::from)
            }
        }
    )*};
}

unit_event! {
    KlippyIdentified = "server:klippy_identified";
    KlippyReady = "server:klippy_ready";
    KlippyShutdown = "server:klippy_shutdown";
    KlippyDisconnect = "server:klippy_disconnect";
    PauseRequested = "klippy_apis:pause_requested";
    ResumeRequested = "klippy_apis:resume_requested";
    CancelRequested = "klippy_apis:cancel_requested";
    MqttConnected = "mqtt:connected";
    MqttDisconnected = "mqtt:disconnected";
}

value_event! {
    ProcStatUpdate = "proc_stats:proc_stat_update";
    CpuThrottled = "proc_stats:cpu_throttled";
    NetStateChanged = "machine:net_state_changed";
    JobQueueChanged = "job_queue:job_queue_changed";
    GcodeResponse = "server:gcode_response";
}

string_event! {
    GcodeReceived = "klippy_connection:gcode_received";
    PublicIpChanged = "machine:public_ip_changed";
}

/// server:klippy_started: the state Klippy started in, with its message
pub struct KlippyStarted;

impl Event for KlippyStarted {
    const NAME: &'static str = "server:klippy_started";
    type Args = (KlippyState, String);
    fn values((state, _): &Self::Args) -> Vec<Value> {
        vec![json!(state.as_str())]
    }
    fn from_values(values: &[Value]) -> Option<Self::Args> {
        let state = KlippyState::from_string(values.first()?.as_str()?).ok()?;
        Some((state, String::new()))
    }
    fn object((state, message): &Self::Args) -> Option<EventObject> {
        Some(EventObject::KlippyState(state.as_str().to_string(), message.clone()))
    }
}

/// server:status_update: Klippy's status changes
pub struct StatusUpdate;

impl Event for StatusUpdate {
    const NAME: &'static str = "server:status_update";
    type Args = Map<String, Value>;
    fn values(status: &Self::Args) -> Vec<Value> {
        vec![Value::Object(status.clone())]
    }
    fn from_values(values: &[Value]) -> Option<Self::Args> {
        values.first()?.as_object().cloned()
    }
}

/// job_state:state_changed: the job event, the previous and the new print_stats
pub struct JobStateChanged;

pub struct JobStateArgs {
    pub event: JobEvent,
    pub prev: Map<String, Value>,
    pub new: Map<String, Value>,
}

impl Event for JobStateChanged {
    const NAME: &'static str = "job_state:state_changed";
    type Args = JobStateArgs;
    fn values(a: &Self::Args) -> Vec<Value> {
        vec![json!(a.event.as_str()), Value::Object(a.prev.clone()), Value::Object(a.new.clone())]
    }
    fn from_values(values: &[Value]) -> Option<Self::Args> {
        Some(JobStateArgs {
            event: JobEvent::from_string(values.first()?.as_str()?).ok()?,
            prev: values.get(1)?.as_object()?.clone(),
            new: values.get(2)?.as_object()?.clone(),
        })
    }
    fn object(a: &Self::Args) -> Option<EventObject> {
        Some(EventObject::JobEvent(a.event.as_str().to_string()))
    }
}

/// klippy_apis:job_start_complete: the user who started the job
pub struct JobStartComplete;

impl Event for JobStartComplete {
    const NAME: &'static str = "klippy_apis:job_start_complete";
    type Args = Option<UserInfo>;
    fn values(user: &Self::Args) -> Vec<Value> {
        vec![user.as_ref().map(|u| u.as_dict()).unwrap_or(Value::Null)]
    }
    fn from_values(values: &[Value]) -> Option<Self::Args> {
        let username = values.first()?.get("username").and_then(|v| v.as_str());
        Some(username.map(|u| UserInfo::new(u, "")))
    }
    fn object(user: &Self::Args) -> Option<EventObject> {
        Some(EventObject::User(user.clone()))
    }
}

/// websockets:client_*: the connection; its id is the value
macro_rules! client_event {
    ($($ty:ident = $name:literal;)*) => {$(
        pub struct $ty;
        impl Event for $ty {
            const NAME: &'static str = $name;
            type Args = Rc<dyn ApiTransport>;
            fn values(t: &Self::Args) -> Vec<Value> {
                vec![json!(t.transport_id())]
            }
            fn from_values(_: &[Value]) -> Option<Self::Args> {
                None
            }
            fn object(t: &Self::Args) -> Option<EventObject> {
                Some(EventObject::Transport(t.clone()))
            }
        }
    )*};
}

client_event! {
    ClientAdded = "websockets:client_added";
    ClientIdentified = "websockets:client_identified";
    ClientRemoved = "websockets:client_removed";
}

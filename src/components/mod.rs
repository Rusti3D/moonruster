// Component registry: maps component names to constructors (the python
// implementation imports moonraker.components.<name> dynamically)

pub mod analysis;
pub mod announcements;
pub mod application;
pub mod authorization;
pub mod button;
pub mod data_store;
pub mod database;
pub mod dbus_manager;
pub mod extensions;
pub mod file_manager;
pub mod gpio;
pub mod history;
pub mod http_client;
pub mod job_queue;
pub mod job_state;
pub mod klippy_apis;
pub mod klippy_connection;
pub mod ldap;
pub mod machine;
pub mod notifier;
pub mod mqtt;
pub mod octoprint_compat;
pub mod paneldue;
pub mod power;
pub mod proc_stats;
pub mod secrets;
pub mod sensor;
pub mod shell_command;
pub mod simplyprint;
pub mod spoolman;
pub mod td1;
pub mod template;
pub mod update_manager;
pub mod webcam;
pub mod wled;
pub mod websockets;
pub mod zeroconf;

use crate::confighelper::{ConfigError, ConfigHelper};
use crate::server::{LifecycleDyn, Server};
use std::any::Any;
use std::rc::Rc;

// Each built-in component's registered name, for server().get::<T>()
macro_rules! named {
    ($($name:literal => $ty:ty,)*) => {
        $(impl crate::server::Named for $ty {
            const NAME: &'static str = $name;
        })*
        #[cfg(test)]
        const NAMED: &[&str] = &[$($name),*];
    };
}

named! {
    "analysis" => analysis::GcodeAnalysis,
    "announcements" => announcements::Announcements,
    "application" => application::MoonrakerApp,
    "authorization" => authorization::Authorization,
    "button" => button::ButtonManager,
    "data_store" => data_store::DataStore,
    "database" => database::MoonrakerDatabase,
    "dbus_manager" => dbus_manager::DbusManager,
    "extensions" => extensions::ExtensionManager,
    "file_manager" => file_manager::FileManager,
    "gpio" => gpio::GpioFactory,
    "history" => history::History,
    "http_client" => http_client::HttpClient,
    "job_queue" => job_queue::JobQueue,
    "job_state" => job_state::JobState,
    "klippy_apis" => klippy_apis::KlippyApi,
    "klippy_connection" => klippy_connection::KlippyConnection,
    "ldap" => ldap::MoonrakerLdap,
    "machine" => machine::Machine,
    "mqtt" => mqtt::MqttClient,
    "notifier" => notifier::Notifier,
    "octoprint_compat" => octoprint_compat::OctoPrintCompat,
    "paneldue" => paneldue::PanelDue,
    "power" => power::PrinterPower,
    "proc_stats" => proc_stats::ProcStats,
    "secrets" => secrets::Secrets,
    "sensor" => sensor::Sensors,
    "shell_command" => shell_command::ShellCommandFactory,
    "simplyprint" => simplyprint::SimplyPrint,
    "spoolman" => spoolman::SpoolManager,
    "td1" => td1::Td1,
    "template" => template::TemplateFactory,
    "update_manager" => update_manager::UpdateManager,
    "webcam" => webcam::WebcamManager,
    "websockets" => websockets::WebsocketManager,
    "wled" => wled::Wled,
    "zeroconf" => zeroconf::ZeroconfRegistrar,
}

pub type Loaded = (Rc<dyn Any>, Rc<dyn LifecycleDyn>);

fn wrap<T: crate::server::Component + 'static>(c: Rc<T>) -> Loaded {
    (c.clone() as Rc<dyn Any>, c as Rc<dyn LifecycleDyn>)
}

/// Names of the available components (the modules in moonraker.components)
pub const COMPONENT_NAMES: [&str; 37] = ["application", "websockets", "klippy_connection", "klippy_apis", "database", "template", "secrets", "authorization", "file_manager", "machine", "data_store", "shell_command", "proc_stats", "job_state", "job_queue", "history", "http_client", "announcements", "webcam", "extensions", "dbus_manager", "ldap", "octoprint_compat", "mqtt", "sensor", "spoolman", "update_manager", "gpio", "button", "power", "notifier", "zeroconf", "wled", "paneldue", "analysis", "td1", "simplyprint"];

/// Returns Ok(None) when no component with the given name exists
pub fn load_component(_server: &Rc<Server>, config: &ConfigHelper, name: &str) -> Result<Option<Loaded>, ConfigError> {
    let loaded = match name {
        "application" => {
            let app = application::load_component(config)?;
            app.post_register();
            wrap(app)
        }
        "websockets" => wrap(websockets::load_component(config)),
        "klippy_connection" => wrap(klippy_connection::load_component(config)?),
        "klippy_apis" => wrap(klippy_apis::load_component(config)),
        "database" => wrap(database::load_component(config)?),
        "template" => wrap(template::load_component(config)?),
        "secrets" => wrap(secrets::load_component(config)?),
        "authorization" => wrap(authorization::load_component(config)?),
        "file_manager" => wrap(file_manager::load_component(config)?),
        "machine" => wrap(machine::load_component(config)?),
        "data_store" => wrap(data_store::load_component(config)?),
        "shell_command" => wrap(shell_command::load_component(config)),
        "proc_stats" => wrap(proc_stats::load_component(config)?),
        "job_state" => wrap(job_state::load_component(config)),
        "job_queue" => wrap(job_queue::load_component(config)?),
        "history" => wrap(history::load_component(config)?),
        "http_client" => wrap(http_client::load_component(config)),
        "announcements" => wrap(announcements::load_component(config)?),
        "webcam" => wrap(webcam::load_component(config)?),
        "extensions" => wrap(extensions::load_component(config)?),
        "dbus_manager" => wrap(dbus_manager::load_component(config)),
        "ldap" => wrap(ldap::load_component(config)?),
        "octoprint_compat" => wrap(octoprint_compat::load_component(config)?),
        "mqtt" => wrap(mqtt::load_component(config)?),
        "sensor" => wrap(sensor::load_component(config)?),
        "spoolman" => wrap(spoolman::load_component(config)?),
        "update_manager" => wrap(update_manager::load_component(config)?),
        "gpio" => wrap(gpio::load_component(config)),
        "button" => wrap(button::load_component(config)?),
        "power" => wrap(power::load_component(config)?),
        "notifier" => wrap(notifier::load_component(config)?),
        "zeroconf" => wrap(zeroconf::load_component(config)?),
        "wled" => wrap(wled::load_component(config)?),
        "paneldue" => wrap(paneldue::load_component(config)?),
        "analysis" => wrap(analysis::load_component(config)?),
        "td1" => wrap(td1::load_component(config)?),
        "simplyprint" => wrap(simplyprint::load_component(config)?),
        _ => return Ok(None),
    };
    Ok(Some(loaded))
}

#[cfg(test)]
mod tests {
    // every component that can be loaded has its type's name, and no other
    #[test]
    fn named_matches_registry() {
        let mut named = super::NAMED.to_vec();
        let mut names = super::COMPONENT_NAMES.to_vec();
        named.sort_unstable();
        names.sort_unstable();
        assert_eq!(named, names);
    }
}

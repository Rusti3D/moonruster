// A Python component: what its load_component(config) returned, kept in
// the server's component table like a built-in one (docs/PLUGINS.md).
// Moonraker's server.py load_component / _initialize_component / close:
// component_init(), on_exit() and close() are called when the object has
// them, and awaited when they return an awaitable.
use super::futures::{await_py, is_awaitable};
use super::py::*;
use crate::common::ServerError;
use crate::confighelper::{ConfigError, ConfigHelper};
use crate::server::Component;
use async_trait::async_trait;
use std::cell::RefCell;
use std::rc::Rc;

pub struct PyComponent {
    pub obj: PyRef,
    // the traceback of a failed component_init, for the warning's log entry
    traceback: RefCell<Option<String>>,
}

impl PyComponent {
    pub fn new(obj: PyRef) -> Rc<PyComponent> {
        Rc::new(PyComponent { obj, traceback: RefCell::new(None) })
    }

    /// Call obj.<name>() if it has it, awaiting an awaitable result
    async fn call_hook(&self, name: &str) -> PyResult<()> {
        if !self.obj.hasattr(name) {
            return Ok(());
        }
        let ret = self.obj.call_method(name, &[])?;
        if !ret.is_none() && is_awaitable(&ret)? {
            await_py(ret).await?;
        }
        Ok(())
    }
}

#[async_trait(?Send)]
impl Component for PyComponent {
    fn has_init(&self) -> bool {
        self.obj.hasattr("component_init")
    }

    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        self.call_hook("component_init").await.map_err(|e| {
            *self.traceback.borrow_mut() = Some(e.traceback.clone());
            ServerError::new(e.message.clone(), 500)
        })
    }

    async fn on_exit(self: Rc<Self>) {
        if let Err(e) = self.call_hook("on_exit").await {
            log::error!("Error executing 'on_exit()' for component\n{}", e.traceback);
        }
    }

    async fn close(self: Rc<Self>) {
        if let Err(e) = self.call_hook("close").await {
            log::error!("Error executing 'close()' for component\n{}", e.traceback);
        }
    }

    fn take_traceback(&self) -> Option<String> {
        self.traceback.borrow_mut().take()
    }
}

/// Load the Python component `name` with Moonraker's own
/// Server.load_component (mirror_verbatim.txt): its import, getsection,
/// load_component(config), pip recovery and log lines. It records the
/// component, or the failure, in Moonruster's tables (Server.components /
/// failed_components views)
pub fn load(name: &str, root: &ConfigHelper) -> Result<(), ConfigError> {
    if let Err(e) = super::ensure_started() {
        log::error!("Unable to load component: ({})\n{}", name, e);
        crate::server::server().set_failed_component(name);
        return Err(ConfigError(e));
    }
    let r = (|| {
        let srv = super::objects::wrap(crate::server::server())?;
        let cfg = super::objects::wrap(Rc::new(root.clone()))?;
        srv.call_method("load_component", &[&cfg, &py_str(name)])
    })();
    r.map(|_| ()).map_err(|e: PyError| ConfigError(format!("{}: {}", e.type_name, e.message)))
}

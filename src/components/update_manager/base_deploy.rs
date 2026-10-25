// The deployers' common base (base_deploy.py), the Deployer interface and the plain deployer

use super::*;

pub struct BaseDeploy {
    pub name: String,
    pub prefix: RefCell<String>,
    pub refresh_interval: f64,
    pub cfg_hash: String,
    pub last_refresh_time: Cell<f64>,
    pub last_cfg_hash: RefCell<String>,
    pub cmd_helper: Rc<CommandHelper>,
}

impl BaseDeploy {
    pub fn new(
        config: &ConfigHelper,
        cmd_helper: &Rc<CommandHelper>,
        name: Option<&str>,
        prefix: &str,
        cfg_hash: Option<&str>,
    ) -> Result<BaseDeploy, ConfigError> {
        let name = name.map(String::from).unwrap_or_else(|| Self::parse_name(config));
        let prefix = if prefix.is_empty() { String::new() } else { format!("{} {}: ", prefix, name) };
        let mut refresh_interval = cmd_helper.refresh_interval;
        if let Some(r) = config.getint_opt("refresh_interval", crate::confighelper::Bounds::none())? {
            refresh_interval = r as f64 * 60.0 * 60.0;
        }
        let cfg_hash = match cfg_hash {
            Some(h) => h.to_string(),
            None => {
                use sha2::Digest;
                hex::encode(config.get_hash().finalize())
            }
        };
        Ok(BaseDeploy {
            name,
            prefix: RefCell::new(prefix),
            refresh_interval,
            cfg_hash,
            last_refresh_time: Cell::new(0.0),
            last_cfg_hash: RefCell::new(String::new()),
            cmd_helper: cmd_helper.clone(),
        })
    }

    pub fn parse_name(config: &ConfigHelper) -> String {
        let n = config.get_name();
        let name = match n.split_once(char::is_whitespace) {
            Some((_, rest)) => rest.trim_start().to_string(),
            None => n.to_string(),
        };
        match name.strip_prefix("client ") {
            Some(n) => n.to_string(),
            None => name,
        }
    }

    pub fn initialize(&self) -> Map<String, Value> {
        let storage = self.cmd_helper.umdb.get(&self.name, json!({})).as_object().cloned().unwrap_or_default();
        self.last_refresh_time.set(storage.get("last_refresh_time").and_then(|v| v.as_f64()).unwrap_or(0.0));
        *self.last_cfg_hash.borrow_mut() =
            storage.get("last_config_hash").and_then(|v| v.as_str()).unwrap_or("").to_string();
        storage
    }

    pub fn needs_refresh(&self, log_remaining_time: bool) -> bool {
        let next = self.last_refresh_time.get() + self.refresh_interval;
        let remaining = (next - crate::utils::time_now() + 0.5) as i64;
        if self.cfg_hash != *self.last_cfg_hash.borrow() || remaining <= 0 {
            return true;
        }
        if log_remaining_time {
            self.log_info(&format!("Next refresh in: {}", pretty_print_time(remaining)));
        }
        false
    }

    pub fn persistent_data(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("last_config_hash".into(), json!(self.cfg_hash));
        m.insert("last_refresh_time".into(), json!(self.last_refresh_time.get()));
        m
    }

    pub fn log_exc(&self, msg: &str, traceback: bool) -> ServerError {
        let log_msg = format!("{}{}", self.prefix.borrow(), msg);
        if traceback {
            log::error!("{}", log_msg);
        } else {
            log::info!("{}", log_msg);
        }
        ServerError::bad(msg)
    }

    pub fn log_info(&self, msg: &str) {
        log::info!("{}{}", self.prefix.borrow(), msg);
    }

    pub fn log_debug(&self, msg: &str) {
        log::debug!("{}{}", self.prefix.borrow(), msg);
    }

    pub fn notify_status(&self, msg: &str, is_complete: bool) {
        let log_msg = format!("{}{}", self.prefix.borrow(), msg);
        log::debug!("{}", log_msg);
        self.cmd_helper.notify_update_response(&log_msg, is_complete);
    }
}

/// Saves the deployer's persistent state (BaseDeploy._save_state)
pub fn save_state(d: &dyn Deployer) {
    let b = d.base();
    b.last_refresh_time.set(crate::utils::time_now());
    *b.last_cfg_hash.borrow_mut() = b.cfg_hash.clone();
    let _ = b.cmd_helper.umdb.insert(&b.name, Value::Object(d.get_persistent_data()));
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DeployKind {
    Base,
    Git,
    Net,
    Python,
    System,
}

/// UpdateManager.register_updater's errors, as Moonraker raises them
pub enum RegisterError {
    /// self.server.error(...)
    Server(ServerError),
    /// the updater's configuration (ConfigError)
    Config(ConfigError),
    /// AppType.from_string's ValueError for an unknown type
    UnknownType(String),
}

impl From<RegisterError> for ServerError {
    fn from(e: RegisterError) -> Self {
        match e {
            RegisterError::Server(e) => e,
            RegisterError::Config(e) => ServerError::bad(e.0),
            RegisterError::UnknownType(m) => ServerError::exc(m),
        }
    }
}

#[async_trait(?Send)]
pub trait Deployer {
    fn base(&self) -> &BaseDeploy;
    fn kind(&self) -> DeployKind;
    fn app(&self) -> Option<&AppDeploy> {
        None
    }
    async fn initialize(&self) -> Map<String, Value> {
        self.base().initialize()
    }
    async fn refresh(&self) -> Result<(), ServerError> {
        Ok(())
    }
    async fn update(&self) -> Result<bool, ServerError> {
        Ok(false)
    }
    async fn rollback(&self) -> Result<bool, ServerError> {
        Err(ServerError::bad(format!("Rollback not available for {}", self.base().name)))
    }
    async fn recover(&self, _hard: bool, _force_dep_update: bool) -> Result<(), ServerError> {
        Err(ServerError::exc("NotImplementedError"))
    }
    fn get_update_status(&self) -> Value {
        json!({})
    }
    fn get_persistent_data(&self) -> Map<String, Value> {
        self.base().persistent_data()
    }
    async fn close(&self) {}
}

pub struct PlainDeploy {
    pub(super) base: BaseDeploy,
}

impl Deployer for PlainDeploy {
    fn base(&self) -> &BaseDeploy {
        &self.base
    }
    fn kind(&self) -> DeployKind {
        DeployKind::Base
    }
}

// Support for password/token secrets (port of moonraker/components/secrets.py)

use crate::configparser::ConfigParser;
use crate::confighelper::ConfigHelper;
use crate::server::{server, Component};
use serde_json::{Map, Value};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

pub struct Secrets {
    secrets_file: PathBuf,
    kind: RefCell<String>,
    values: RefCell<Value>,
}

impl Component for Secrets {}

impl Secrets {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, crate::confighelper::ConfigError> {
        let srv = server();
        let path = config.get_opt_dep("secrets_path", true)?;
        let mut fpath = PathBuf::from(srv.data_path()).join("moonraker.secrets");
        if !fpath.is_file() {
            if let Some(p) = &path {
                fpath = crate::utils::resolve_path(&crate::utils::expand_user(p));
            }
        }
        let s = Rc::new(Secrets {
            secrets_file: fpath.clone(),
            kind: RefCell::new("invalid".into()),
            values: RefCell::new(Value::Object(Map::new())),
        });
        if fpath.is_file() {
            let data = std::fs::read_to_string(&fpath).unwrap_or_default();
            match serde_json::from_str::<Value>(&data) {
                Ok(vals) => {
                    if !vals.is_object() {
                        srv.add_warning(
                            &format!(
                                "[secrets]: option 'secrets_path', top level item in json file '{}' must be an Object.",
                                fpath.display()
                            ),
                            None,
                            true,
                        );
                        return Ok(s);
                    }
                    *s.values.borrow_mut() = vals;
                    *s.kind.borrow_mut() = "json".into();
                }
                Err(_) => {
                    let mut cp = ConfigParser::new();
                    match cp.read_string(&data, "<string>") {
                        Ok(_) => {
                            let mut out = Map::new();
                            for sec in cp.sections() {
                                let items = cp.items(&sec).unwrap_or_default();
                                out.insert(
                                    sec,
                                    Value::Object(items.into_iter().map(|(k, v)| (k, Value::String(v))).collect()),
                                );
                            }
                            *s.values.borrow_mut() = Value::Object(out);
                            *s.kind.borrow_mut() = "ini".into();
                        }
                        Err(_) => {
                            srv.add_warning(
                                &format!(
                                    "[secrets]: option 'secrets_path', invalid file format, must be json or ini: '{}'",
                                    fpath.display()
                                ),
                                None,
                                true,
                            );
                            return Ok(s);
                        }
                    }
                }
            }
            log::debug!("[secrets]: Loaded {} file: {}", s.kind.borrow(), fpath.display());
        } else if path.is_some() {
            srv.add_warning(
                &format!("[secrets]: option 'secrets_path', file does not exist: '{}'", fpath.display()),
                None,
                true,
            );
        } else {
            log::debug!("[secrets]: Option `secrets_path` not supplied");
        }
        Ok(s)
    }

    pub fn get_secrets_file(&self) -> PathBuf {
        self.secrets_file.clone()
    }

    pub fn get_type(&self) -> String {
        self.kind.borrow().clone()
    }

    pub fn values(&self) -> Value {
        self.values.borrow().clone()
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        self.values.borrow().get(key).cloned()
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<Secrets>, crate::confighelper::ConfigError> {
    Secrets::new(config)
}

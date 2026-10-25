// Notifier (port of moonraker/components/notifier.py)
//
// Notifications are delivered by the Apprise command line client, which is
// part of the apprise python package Moonraker depends on.

use crate::common::*;
use crate::components::template::JinjaTemplate;
use crate::confighelper::{ConfigError, ConfigHelper};
use crate::server::{server, Component};
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::rc::Rc;

const JOB_EVENTS: [&str; 6] = ["started", "paused", "resumed", "complete", "error", "cancelled"];

/// Returns the command prefix used to invoke apprise
fn apprise_cmd() -> Vec<String> {
    if let Some(p) = crate::components::machine::which("apprise") {
        return vec![p.display().to_string()];
    }
    vec!["python3".into(), "-m".into(), "apprise".into()]
}

fn apprise_version() -> String {
    let cmd = apprise_cmd();
    std::process::Command::new(&cmd[0])
        .args(&cmd[1..])
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| {
            let out = String::from_utf8_lossy(&o.stdout).to_string();
            regex::Regex::new(r"Apprise v?([0-9][0-9A-Za-z.\-]*)").unwrap().captures(&out).map(|c| c[1].to_string())
        })
        .unwrap_or_else(|| "unknown".into())
}

pub struct NotifierInstance {
    config: ConfigHelper,
    name: String,
    attach: RefCell<Option<JinjaTemplate>>,
    url: String,
    title: Option<JinjaTemplate>,
    body: Option<JinjaTemplate>,
    body_format: String,
    events: Vec<String>,
}

impl NotifierInstance {
    fn new(config: &ConfigHelper) -> Result<Rc<Self>, ConfigError> {
        let n = config.get_name();
        let name = match n.split_once(char::is_whitespace) {
            Some((_, rest)) if !rest.trim().is_empty() => rest.trim_start().to_string(),
            _ => return Err(ConfigHelper::error(format!("Invalid Section Name: {}", n))),
        };
        let attach = config.gettemplate_opt("attach", Some(None), false)?;
        let url = config.gettemplate("url", false)?.render(&json!({})).map_err(|e| ConfigHelper::error(e.message))?;
        if !regex::Regex::new(r"^\w+?://").unwrap().is_match(&url) {
            return Err(ConfigHelper::error(format!("Invalid url for: {}", config.get_name())));
        }
        let title = config.gettemplate_opt("title", Some(None), false)?;
        let body = config.gettemplate_opt("body", Some(None), false)?;
        let body_format = config.get_or("body_format", "text")?.to_uppercase();
        if !["TEXT", "HTML", "MARKDOWN"].contains(&body_format.as_str()) {
            return Err(ConfigHelper::error(format!("Invalid body_format for {}", config.get_name())));
        }
        let events = config.getlist_sep("events", Some(","), None)?;
        Ok(Rc::new(NotifierInstance {
            config: config.clone(),
            name,
            attach: RefCell::new(attach),
            url,
            title,
            body,
            body_format: body_format.to_lowercase(),
            events,
        }))
    }

    fn as_dict(&self) -> Value {
        let opt = |o: &str| self.config.get_opt(o).ok().flatten();
        json!({
            "name": self.name,
            "url": opt("url"),
            "title": opt("title"),
            "body": opt("body"),
            "body_format": opt("body_format"),
            "events": self.events,
            "attach": self.attach.borrow().as_ref().map(|t| t.source().to_string()),
        })
    }

    async fn notify(&self, event_name: &str, event_args: Vec<Value>, message: &str) -> Result<(), ServerError> {
        let srv = server();
        let context = json!({"event_name": event_name, "event_args": event_args, "event_message": message});
        let title = match &self.title {
            None => String::new(),
            Some(t) => t.render(&context)?,
        };
        let body = match &self.body {
            None => event_name.to_string(),
            Some(t) => t.render(&context)?,
        };
        let mut attachments: Vec<String> = Vec::new();
        let attach = self.attach.borrow().clone();
        if let Some(a) = attach {
            match a.render(&context) {
                Err(_) => {
                    srv.add_warning(
                        &format!("[notifier {}]: The attachment is not valid. The template failed to render.", self.name),
                        Some(&format!("notifier {}", self.name)),
                        true,
                    );
                    *self.attach.borrow_mut() = None;
                }
                Ok(rendered) => {
                    let fm = srv.get::<crate::components::file_manager::FileManager>();
                    let url_re = regex::Regex::new(r"^https?://").unwrap();
                    for item in rendered.lines() {
                        let item = item.trim();
                        if item.is_empty() {
                            continue;
                        }
                        if url_re.is_match(item) {
                            attachments.push(item.to_string());
                            continue;
                        }
                        let path = crate::utils::resolve_path(&crate::utils::expand_user(item));
                        let ps = path.display().to_string();
                        if !path.is_file() {
                            srv.add_warning(
                                &format!("[notifier {}]: Invalid attachment detected, file does not exist: {}.", self.name, ps),
                                Some(&format!("notifier {}", self.name)),
                                true,
                            );
                        } else if !fm.as_ref().map(|f| f.can_access_path(&ps)).unwrap_or(false) {
                            srv.add_warning(
                                &format!("[notifier {}]: Invalid attachment detected, no read permission for the file {}.", self.name, ps),
                                Some(&format!("notifier {}", self.name)),
                                true,
                            );
                        } else {
                            attachments.push(ps);
                        }
                    }
                }
            }
        }
        let mut cmd = apprise_cmd();
        cmd.extend(["-b".into(), body.trim().to_string(), "-i".into(), self.body_format.clone()]);
        let title = title.trim();
        if !title.is_empty() {
            cmd.extend(["-t".into(), title.to_string()]);
        }
        for a in attachments {
            cmd.extend(["-a".into(), a]);
        }
        cmd.push(self.url.clone());
        let out = tokio::process::Command::new(&cmd[0]).args(&cmd[1..]).output().await;
        match out {
            Err(e) => log::error!("Notifier {}: unable to run apprise: {}", self.name, e),
            Ok(o) if !o.status.success() => {
                log::info!("Notifier {}: apprise failed: {}", self.name, String::from_utf8_lossy(&o.stderr).trim());
            }
            Ok(_) => {}
        }
        Ok(())
    }
}

pub struct Notifier {
    notifiers: IndexMap<String, Rc<NotifierInstance>>,
    events: IndexMap<String, Vec<Rc<NotifierInstance>>>,
}

impl Notifier {
    /// Notifier.notify_action(name, message)
    pub async fn notify_action(&self, name: &str, message: &str) -> Result<(), ServerError> {
        let inst = self.notifiers.get(name).cloned().ok_or_else(|| ServerError::new(format!("Notifier '{}' not found", name), 404))?;
        inst.notify("remote_action", vec![], message).await
    }

    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, ConfigError> {
        let srv = server();
        let sections = config.get_prefix_sections("notifier");
        let mut notifiers = IndexMap::new();
        let mut events: IndexMap<String, Vec<Rc<NotifierInstance>>> = IndexMap::new();
        log::info!("Loading notifiers, Apprise version {}", apprise_version());
        for section in sections {
            let cfg = config.getsection(&section);
            match NotifierInstance::new(&cfg) {
                Ok(n) => {
                    for evt in JOB_EVENTS {
                        if n.events.iter().any(|e| e == "*" || e == evt) {
                            events.entry(evt.to_string()).or_default().push(n.clone());
                        }
                    }
                    log::info!("Registered notifier: '{}'", n.name);
                    notifiers.insert(n.name.clone(), n);
                }
                Err(e) => {
                    srv.add_warning(&format!("Failed to load notifier[{}]\n{}", cfg.get_name(), e.0), None, true);
                }
            }
        }
        let nt = Rc::new(Notifier { notifiers, events });
        let n = Rc::downgrade(&nt);
        let _ = srv.register_remote_method(
            "notify",
            crate::components::klippy_connection::remote_method(move |args| {
                if let Some(n) = n.upgrade() {
                    crate::server::spawn(async move {
                        let name = args.get("name").map(crate::pyfmt::py_str).unwrap_or_default();
                        let message = args.get("message").map(crate::pyfmt::py_str).unwrap_or_default();
                        if let Err(e) = n.notify_action(&name, &message).await {
                            log::error!("Error running remote method: notify\n{}", e.message);
                        }
                    });
                }
            }),
        );
        let n = nt.clone();
        let _ = srv.register_endpoint(
            "/server/notifiers/list",
            RequestType::GET,
            callback(move |_req| {
                let n = n.clone();
                async move {
                    let list: Vec<Value> = n.notifiers.values().map(|i| i.as_dict()).collect();
                    Ok(json!({"notifiers": list}))
                }
            }),
        );
        let n = nt.clone();
        let _ = srv.register_debug_endpoint(
            "/debug/notifiers/test",
            RequestType::POST,
            callback(move |req| {
                let n = n.clone();
                async move { n.handle_notifier_test(req).await }
            }),
        );
        let n = Rc::downgrade(&nt);
        srv.on_event_async("job_state:state_changed", move |args| {
            let n = n.clone();
            async move {
                if let Some(n) = n.upgrade() {
                    let evt = args.first().and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let list = n.events.get(&evt).cloned().unwrap_or_default();
                    for inst in list {
                        if let Err(e) = inst.notify(&evt, args[1..].to_vec(), "").await {
                            log::error!("{}", e.message);
                        }
                    }
                }
            }
        });
        Ok(nt)
    }

    async fn handle_notifier_test(&self, req: WebRequest) -> ApiResult {
        let name = req.get_str("name")?;
        let inst = self.notifiers.get(&name).cloned().ok_or_else(|| ServerError::new(format!("Notifier '{}' not found", name), 404))?;
        let mut objs = Map::new();
        objs.insert("print_stats".into(), Value::Null);
        let result = server().klippy_apis().query_objects(&objs).await.unwrap_or_default();
        let mut print_stats = result.get("print_stats").cloned().unwrap_or(json!({}));
        print_stats["filename"] = json!("notifier_test.gcode");
        let evt = inst.events.first().cloned().unwrap_or_default();
        inst.notify(&evt, vec![print_stats.clone(), print_stats.clone()], "").await?;
        Ok(json!({"status": "success", "stats": print_stats}))
    }
}

impl Component for Notifier {}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<Notifier>, ConfigError> {
    Notifier::new(config)
}

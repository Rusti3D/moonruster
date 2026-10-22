// Support for Moonraker/Klipper/Client announcements (port of moonraker/components/announcements.py)

use crate::common::*;
use crate::components::database::NamespaceWrapper;
use crate::components::http_client::RequestOpts;
use crate::confighelper::ConfigHelper;
use crate::server::{server, spawn, Component};
use async_trait::async_trait;
use chrono::TimeZone;
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

const MOONLIGHT_URL: &str = "https://arksine.github.io/moonlight";
const UPDATE_CHECK_TIME: f64 = 1800.0;

/// python's datetime.datetime.utcnow().timestamp() (naive utc treated as local time)
fn utcnow_naive_ts() -> f64 {
    let naive = chrono::Utc::now().naive_utc();
    match chrono::Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => {
            dt.timestamp() as f64 + dt.timestamp_subsec_micros() as f64 / 1e6
        }
        chrono::LocalResult::None => crate::utils::time_now(),
    }
}

fn delayed_event(delay: f64, event: &'static str, arg: Value) {
    spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs_f64(delay)).await;
        let _ = server().send_event(event, vec![arg]);
    });
}

struct EntryManager {
    db: NamespaceWrapper,
    entry_id_map: RefCell<HashMap<String, String>>,
    next_key: Cell<i64>,
    dismiss_handles: RefCell<HashMap<String, tokio::task::JoinHandle<()>>>,
}

impl EntryManager {
    fn new() -> Result<Rc<Self>, ServerError> {
        let db = server().database();
        db.register_local_namespace("announcements", false, false)?;
        let wrapped = db.wrap_namespace("announcements", true)?;
        Ok(Rc::new(EntryManager {
            db: wrapped,
            entry_id_map: RefCell::new(HashMap::new()),
            next_key: Cell::new(0),
            dismiss_handles: RefCell::new(HashMap::new()),
        }))
    }

    fn initialize(self: &Rc<Self>) {
        let mut last_key = String::new();
        let curtime = utcnow_naive_ts();
        for (key, mut entry) in self.db.items() {
            last_key = key.clone();
            let aid = entry.get("entry_id").map(crate::pyfmt::py_str).unwrap_or_default();
            self.entry_id_map.borrow_mut().insert(aid, key.clone());
            if entry.get("dismissed").and_then(|v| v.as_bool()).unwrap_or(false) {
                if let Some(wake) = entry.get("dismiss_wake").and_then(|v| v.as_f64()) {
                    let diff = wake - curtime;
                    if diff - 10.0 < 0.0 {
                        entry["dismissed"] = json!(false);
                        entry["date_dismissed"] = Value::Null;
                        entry["dismiss_wake"] = Value::Null;
                        let _ = self.db.insert(&key, entry);
                    } else {
                        self.schedule_wake(&key, diff);
                    }
                }
            }
        }
        if !last_key.is_empty() {
            self.next_key.set(i64::from_str_radix(&last_key, 16).unwrap_or(0) + 1);
        }
    }

    fn schedule_wake(self: &Rc<Self>, key: &str, delay: f64) {
        let em = Rc::downgrade(self);
        let k = key.to_string();
        let h = spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs_f64(delay.max(0.0))).await;
            if let Some(em) = em.upgrade() {
                em.wake_dismissed(&k);
            }
        });
        if let Some(old) = self.dismiss_handles.borrow_mut().insert(key.to_string(), h) {
            old.abort();
        }
    }

    fn list_entries(&self, include_dismissed: bool) -> Vec<Value> {
        let mut vals = self.db.values();
        vals.sort_by(|a, b| {
            let da = a.get("date").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let db = b.get("date").and_then(|v| v.as_f64()).unwrap_or(0.0);
            db.partial_cmp(&da).unwrap_or(std::cmp::Ordering::Equal)
        });
        if include_dismissed {
            return vals;
        }
        vals.into_iter().filter(|v| !v.get("dismissed").and_then(|d| d.as_bool()).unwrap_or(false)).collect()
    }

    fn has_entry(&self, id: &str) -> bool {
        self.entry_id_map.borrow().contains_key(id)
    }

    fn add_entry(&self, entry: Value) {
        let aid = entry.get("entry_id").map(crate::pyfmt::py_str).unwrap_or_default();
        let key = format!("{:06X}", self.next_key.get());
        self.next_key.set(self.next_key.get() + 1);
        self.entry_id_map.borrow_mut().insert(aid, key.clone());
        let _ = self.db.insert(&key, entry);
    }

    fn remove_entry(&self, id: &str) -> Result<Value, ServerError> {
        let key = self
            .entry_id_map
            .borrow_mut()
            .remove(id)
            .ok_or_else(|| ServerError::bad(format!("No key matching entry id: {}", id)))?;
        Ok(self.db.pop(&key, Some(Value::Null)).unwrap_or(Value::Null))
    }

    fn dismiss_entry(self: &Rc<Self>, id: &str, wake_time: Option<i64>) -> Result<(), ServerError> {
        let key = self
            .entry_id_map
            .borrow()
            .get(id)
            .cloned()
            .ok_or_else(|| ServerError::bad(format!("No key matching entry id: {}", id)))?;
        let mut entry = self.db.get_strict(&key)?;
        if entry.get("dismissed").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Ok(());
        }
        entry["dismissed"] = json!(true);
        let curtime = utcnow_naive_ts();
        entry["date_dismissed"] = crate::utils::float_value(curtime);
        if let Some(w) = wake_time {
            entry["dismiss_wake"] = crate::utils::float_value(curtime + w as f64);
            self.schedule_wake(&key, w as f64);
        }
        let _ = self.db.insert(&key, entry);
        delayed_event(0.05, "announcements:dismissed", json!({"entry_id": id}));
        Ok(())
    }

    fn wake_dismissed(&self, key: &str) {
        self.dismiss_handles.borrow_mut().remove(key);
        let mut entry = self.db.get(key, Value::Null);
        if entry.is_null() || !entry.get("dismissed").and_then(|v| v.as_bool()).unwrap_or(false) {
            return;
        }
        entry["dismissed"] = json!(false);
        entry["date_dismissed"] = Value::Null;
        entry["dismiss_wake"] = Value::Null;
        let eid = entry["entry_id"].clone();
        let _ = self.db.insert(key, entry);
        let _ = server().send_event("announcements:dismiss_wake", vec![json!({"entry_id": eid})]);
    }

    fn prune_by_prefix(&self, prefix: &str, valid_ids: &[String]) -> bool {
        let mut del = Vec::new();
        let ids: Vec<String> = self.entry_id_map.borrow().keys().cloned().collect();
        for id in ids {
            if !id.starts_with(prefix) || valid_ids.contains(&id) {
                continue;
            }
            if let Some(k) = self.entry_id_map.borrow_mut().remove(&id) {
                del.push(k);
            }
        }
        if !del.is_empty() {
            let _ = self.db.delete_batch(&del);
            return true;
        }
        false
    }

    fn prune_by_feed(&self, feed: &str) -> bool {
        let mut del = Vec::new();
        for e in self.list_entries(true) {
            if e.get("feed").and_then(|v| v.as_str()).map(|f| f.to_lowercase() == feed).unwrap_or(false) {
                let id = e.get("entry_id").map(crate::pyfmt::py_str).unwrap_or_default();
                if let Some(k) = self.entry_id_map.borrow_mut().remove(&id) {
                    del.push(k);
                }
            }
        }
        if !del.is_empty() {
            let _ = self.db.delete_batch(&del);
            return true;
        }
        false
    }

    fn close(&self) {
        for (_, h) in self.dismiss_handles.borrow_mut().drain() {
            h.abort();
        }
    }
}

struct RssFeed {
    name: String,
    xml_file: String,
    asset_url: String,
    last_modified: Cell<i128>,
    etag: RefCell<Option<String>>,
    dev_xml_path: Option<PathBuf>,
}

impl RssFeed {
    fn new(name: &str, dev_mode: bool) -> Rc<Self> {
        let xml_file = format!("{}.xml", name);
        let mut dev_xml_path = None;
        if dev_mode {
            let folder = PathBuf::from(server().data_path()).join("development/announcements");
            let _ = std::fs::create_dir_all(&folder);
            let p = folder.join(&xml_file);
            log::info!("Announcement Feed {}: Dev XML path set to {}", name, p.display());
            dev_xml_path = Some(p);
        }
        Rc::new(RssFeed {
            name: name.into(),
            asset_url: format!("{}/assets/{}", MOONLIGHT_URL, xml_file),
            xml_file,
            last_modified: Cell::new(0),
            etag: RefCell::new(None),
            dev_xml_path,
        })
    }

    fn initialize(&self) {
        let v = server()
            .database()
            .get_item_sync("moonraker", Some(&json!(format!("announcements.{}.etag", self.name))), Some(Value::Null))
            .unwrap_or(Value::Null);
        *self.etag.borrow_mut() = v.as_str().map(String::from);
    }

    async fn update_entries(&self, ann: &Announcements) -> bool {
        let xml = if self.dev_xml_path.is_none() { self.fetch_moonlight(ann).await } else { self.fetch_local_folder() };
        if xml.is_empty() {
            return false;
        }
        self.parse_xml(&xml, &ann.entry_mgr)
    }

    async fn fetch_moonlight(&self, ann: &Announcements) -> String {
        if !ann.moonlight_enabled {
            return String::new();
        }
        let mut headers = vec![("Accept".to_string(), "application/xml".to_string())];
        if let Some(e) = self.etag.borrow().clone() {
            headers.push(("If-None-Match".into(), e));
        }
        let client: Rc<crate::components::http_client::HttpClient> = match server().lookup_component("http_client") {
            Some(c) => c,
            None => return String::new(),
        };
        let resp = client
            .get_opts(&self.asset_url, &headers, RequestOpts { attempts: 5, retry_pause_time: 0.5, enable_cache: false, ..Default::default() })
            .await;
        if resp.has_error() {
            log::info!("Failed to update subscription '{}': {}", self.name, resp.error_string());
            return String::new();
        }
        if resp.status_code == 304 {
            log::debug!("Content at {} not modified", self.xml_file);
            return String::new();
        }
        let etag = resp.etag();
        *self.etag.borrow_mut() = etag.clone();
        let db = server().database();
        let key = json!(format!("announcements.{}.etag", self.name));
        match etag {
            Some(e) => {
                let _ = db.insert_item_sync("moonraker", &key, json!(e));
            }
            None => {
                let _ = db.delete_item_sync("moonraker", &key);
            }
        }
        resp.text()
    }

    fn fetch_local_folder(&self) -> String {
        let p = match &self.dev_xml_path {
            Some(p) => p,
            None => return String::new(),
        };
        if !p.is_file() {
            log::info!("No file at path {}", p.display());
            return String::new();
        }
        let mtime = crate::utils::mtime_ns(p).unwrap_or(0);
        if mtime <= self.last_modified.get() {
            log::debug!("Content at {} not modified", self.xml_file);
            return String::new();
        }
        match std::fs::read_to_string(p) {
            Ok(d) => {
                self.last_modified.set(mtime);
                d
            }
            Err(e) => {
                log::error!("Unable read xml file {}\n{}", p.display(), e);
                String::new()
            }
        }
    }

    fn parse_xml(&self, xml: &str, em: &EntryManager) -> bool {
        let doc = match roxmltree::Document::parse(xml) {
            Ok(d) => d,
            Err(e) => {
                log::error!("Feed {}: xml parse error: {}", self.name, e);
                return false;
            }
        };
        let root = doc.root_element();
        let channel = match root.children().find(|n| n.has_tag_name("channel")) {
            Some(c) => c,
            None => {
                log::debug!("Feed {}: no channel found", self.name);
                return false;
            }
        };
        let text_of = |node: roxmltree::Node, tag: &str| -> Option<String> {
            node.children().find(|n| n.is_element() && n.tag_name().name() == tag && n.tag_name().namespace().is_none()).map(|n| {
                n.children().filter(|c| c.is_text()).map(|c| c.text().unwrap_or("")).collect::<String>()
            })
        };
        let mut prefix = text_of(channel, "title").unwrap_or_default().to_lowercase();
        if prefix.is_empty() {
            log::info!("Feed {}: No prefix found", self.name);
        }
        let mut valid_ids = Vec::new();
        let mut changed = false;
        for item in channel.children().filter(|n| n.is_element() && n.tag_name().name() == "item" && n.tag_name().namespace().is_none()) {
            let guid = match text_of(item, "guid") {
                Some(g) => g,
                None => {
                    log::debug!("Feed {}: Invalid Item", self.name);
                    continue;
                }
            };
            if prefix.is_empty() {
                prefix = guid.split('/').take(2).collect::<Vec<_>>().join("/");
            } else if !guid.starts_with(&prefix) {
                log::debug!("Feed {}: Guid {} is not prefixed with {}", self.name, guid, prefix);
            }
            valid_ids.push(guid.clone());
            if em.has_entry(&guid) {
                continue;
            }
            let date = text_of(item, "pubDate")
                .and_then(|d| chrono::DateTime::parse_from_rfc2822(d.trim()).ok())
                .map(|d| d.timestamp() as f64)
                .unwrap_or_else(utcnow_naive_ts);
            let entry = json!({
                "entry_id": guid,
                "url": text_of(item, "link"),
                "title": text_of(item, "title"),
                "description": text_of(item, "description"),
                "priority": text_of(item, "category"),
                "date": date,
                "dismissed": false,
                "date_dismissed": null,
                "dismiss_wake": null,
                "source": "moonlight",
                "feed": self.name,
            });
            changed = true;
            em.add_entry(entry);
        }
        log::debug!(
            "Feed {}: found entries {}",
            self.name,
            crate::pyfmt::py_repr(&Value::Array(valid_ids.iter().map(|v| json!(v)).collect()))
        );
        if !prefix.is_empty() {
            let pruned = em.prune_by_prefix(&prefix, &valid_ids);
            changed = changed || pruned;
        }
        changed
    }
}

pub struct Announcements {
    entry_mgr: Rc<EntryManager>,
    request_lock: tokio::sync::Mutex<()>,
    moonlight_enabled: bool,
    dev_mode: bool,
    subscriptions: RefCell<IndexMap<String, Rc<RssFeed>>>,
    stored_feeds: RefCell<Vec<String>>,
    configured_feeds: RefCell<Vec<String>>,
    timer: RefCell<Option<tokio::task::JoinHandle<()>>>,
}

#[async_trait(?Send)]
impl Component for Announcements {
    fn has_init(&self) -> bool {
        true
    }
    async fn component_init(self: Rc<Self>) -> Result<(), ServerError> {
        let stored = server()
            .database()
            .get_item_sync("moonraker", Some(&json!("announcements.stored_feeds")), Some(json!([])))
            .unwrap_or(json!([]));
        let stored: Vec<String> = stored.as_array().map(|a| a.iter().map(crate::pyfmt::py_str).collect()).unwrap_or_default();
        *self.stored_feeds.borrow_mut() = stored.clone();
        for name in stored {
            if !self.subscriptions.borrow().contains_key(&name) {
                self.subscriptions.borrow_mut().insert(name.clone(), RssFeed::new(&name, self.dev_mode));
            }
        }
        {
            let _g = self.request_lock.lock().await;
            self.entry_mgr.initialize();
            for sub in self.subscriptions.borrow().values() {
                sub.initialize();
            }
        }
        let a = Rc::downgrade(&self);
        *self.timer.borrow_mut() = Some(spawn(async move {
            loop {
                match a.upgrade() {
                    Some(ann) => ann.handle_update_timer().await,
                    None => return,
                }
                tokio::time::sleep(std::time::Duration::from_secs_f64(UPDATE_CHECK_TIME)).await;
            }
        }));
        Ok(())
    }
    async fn close(self: Rc<Self>) {
        if let Some(t) = self.timer.borrow_mut().take() {
            t.abort();
        }
        self.entry_mgr.close();
    }
}

macro_rules! ann_endpoint {
    ($srv:expr, $a:expr, $path:expr, $rt:expr, $method:ident) => {{
        let aa = $a.clone();
        $srv.register_endpoint($path, $rt, callback(move |req: WebRequest| {
            let aa = aa.clone();
            async move { aa.$method(req).await }
        }))?;
    }};
}

impl Announcements {
    pub fn new(config: &ConfigHelper) -> Result<Rc<Self>, crate::confighelper::ConfigError> {
        let srv = server();
        let entry_mgr = EntryManager::new()?;
        let moonlight_enabled = config.getboolean_or("enable_moonlight", true)?;
        if !moonlight_enabled {
            srv.add_log_rollover_item("moonlight_disabled", "Moonlight Disabled, remote announcement feeds unavailable", true);
        }
        let dev_mode = config.getboolean_or("dev_mode", false)?;
        let mut subs = IndexMap::new();
        subs.insert("moonraker".to_string(), RssFeed::new("moonraker", dev_mode));
        subs.insert("klipper".to_string(), RssFeed::new("klipper", dev_mode));
        let mut configured = vec!["moonraker".to_string(), "klipper".to_string()];
        for sub in config.getlist_opt("subscriptions", Some(vec![]), Some("\n"))?.unwrap_or_default() {
            let sub = sub.to_lowercase();
            if subs.contains_key(&sub) {
                continue;
            }
            configured.push(sub.clone());
            subs.insert(sub.clone(), RssFeed::new(&sub, dev_mode));
        }
        let a = Rc::new(Announcements {
            entry_mgr,
            request_lock: tokio::sync::Mutex::new(()),
            moonlight_enabled,
            dev_mode,
            subscriptions: RefCell::new(subs),
            stored_feeds: RefCell::new(Vec::new()),
            configured_feeds: RefCell::new(configured),
            timer: RefCell::new(None),
        });
        ann_endpoint!(srv, a, "/server/announcements/list", RequestType::GET, list_announcements);
        ann_endpoint!(srv, a, "/server/announcements/dismiss", RequestType::POST, handle_dismiss_request);
        ann_endpoint!(srv, a, "/server/announcements/update", RequestType::POST, handle_update_request);
        ann_endpoint!(srv, a, "/server/announcements/feed", RequestType::POST | RequestType::DELETE, handle_feed_request);
        ann_endpoint!(srv, a, "/server/announcements/feeds", RequestType::GET, handle_list_feeds);
        srv.register_notification("announcements:dismissed", Some("announcement_dismissed"));
        srv.register_notification("announcements:entries_updated", Some("announcement_update"));
        srv.register_notification("announcements:dismiss_wake", Some("announcement_wake"));
        Ok(a)
    }

    async fn handle_update_timer(&self) {
        let _g = self.request_lock.lock().await;
        let mut changed = false;
        let subs: Vec<Rc<RssFeed>> = self.subscriptions.borrow().values().cloned().collect();
        for sub in subs {
            changed |= sub.update_entries(self).await;
        }
        if changed {
            let entries = self.entry_mgr.list_entries(true);
            let _ = server().send_event("announcements:entries_updated", vec![json!({"entries": entries})]);
        }
    }

    async fn handle_dismiss_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let _g = self.request_lock.lock().await;
        let id = req.get_str("entry_id")?;
        let wake = req.get_int_opt("wake_time")?;
        self.entry_mgr.dismiss_entry(&id, wake)?;
        Ok(json!({"entry_id": id}))
    }

    async fn list_announcements(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let _g = self.request_lock.lock().await;
        let incl = req.get_boolean_or("include_dismissed", true)?;
        let entries = self.entry_mgr.list_entries(incl);
        let feeds: Vec<String> = self.subscriptions.borrow().keys().cloned().collect();
        Ok(json!({"entries": entries, "feeds": feeds}))
    }

    async fn handle_update_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let all: Vec<String> = self.subscriptions.borrow().keys().cloned().collect();
        let subs = req.get_list_opt("subscriptions", ",", None)?.unwrap_or(all);
        for s in &subs {
            if !self.subscriptions.borrow().contains_key(s) {
                return Err(ServerError::bad(format!("No subscription for {}", s)));
            }
        }
        let _g = self.request_lock.lock().await;
        let mut changed = false;
        for s in &subs {
            let feed = self.subscriptions.borrow().get(s).cloned();
            if let Some(f) = feed {
                changed |= f.update_entries(&self).await;
            }
        }
        let entries = self.entry_mgr.list_entries(true);
        if changed {
            delayed_event(0.05, "announcements:entries_updated", json!({"entries": entries.clone()}));
        }
        Ok(json!({"entries": entries, "modified": changed}))
    }

    async fn handle_list_feeds(self: Rc<Self>, _req: WebRequest) -> ApiResult {
        let feeds: Vec<String> = self.subscriptions.borrow().keys().cloned().collect();
        Ok(json!({"feeds": feeds}))
    }

    async fn handle_feed_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let rt = req.get_request_type();
        let name = req.get_str("name")?.to_lowercase();
        let mut changed = false;
        let mut result = "skipped";
        let db = server().database();
        if rt == RequestType::POST {
            if !self.subscriptions.borrow().contains_key(&name) {
                let feed = RssFeed::new(&name, self.dev_mode);
                self.subscriptions.borrow_mut().insert(name.clone(), feed.clone());
                feed.initialize();
                changed = feed.update_entries(&self).await;
                self.stored_feeds.borrow_mut().push(name.clone());
                let sf: Vec<Value> = self.stored_feeds.borrow().iter().map(|s| json!(s)).collect();
                let _ = db.insert_item_sync("moonraker", &json!("announcements.stored_feeds"), Value::Array(sf));
                result = "added";
            }
        } else if rt == RequestType::DELETE {
            if !self.stored_feeds.borrow().contains(&name) {
                return Err(ServerError::bad(format!("Feed '{}' not stored", name)));
            }
            if self.configured_feeds.borrow().contains(&name) {
                return Err(ServerError::bad(format!("Feed '{}' exists in the configuration, cannot remove", name)));
            }
            self.stored_feeds.borrow_mut().retain(|f| f != &name);
            let sf: Vec<Value> = self.stored_feeds.borrow().iter().map(|s| json!(s)).collect();
            let _ = db.insert_item_sync("moonraker", &json!("announcements.stored_feeds"), Value::Array(sf));
            if self.subscriptions.borrow_mut().shift_remove(&name).is_some() {
                changed = self.entry_mgr.prune_by_feed(&name);
                log::info!("Removed Announcement Feed: {}", name);
                result = "removed";
            } else {
                return Err(ServerError::bad(format!("Feed does not exist: {}", name)));
            }
        }
        if changed {
            let entries = self.entry_mgr.list_entries(true);
            delayed_event(0.05, "announcements:entries_updated", json!({"entries": entries}));
        }
        Ok(json!({"feed": name, "action": result}))
    }

    pub fn add_internal_announcement(&self, title: &str, desc: &str, url: &str, priority: &str, feed: &str) -> Value {
        let now = chrono::Utc::now().naive_utc();
        let entry_id = format!("{}/{}", feed, now.format("%Y-%m-%dT%H:%M:%S"));
        let entry = json!({
            "entry_id": entry_id,
            "url": url,
            "title": title,
            "description": desc,
            "priority": priority,
            "date": utcnow_naive_ts(),
            "dismissed": false,
            "date_dismissed": null,
            "dismiss_wake": null,
            "source": "internal",
            "feed": feed,
        });
        self.entry_mgr.add_entry(entry.clone());
        let em = self.entry_mgr.clone();
        spawn(async move {
            let entries = em.list_entries(true);
            let _ = server().send_event("announcements:entries_updated", vec![json!({"entries": entries})]);
        });
        entry
    }

    pub async fn remove_announcement(&self, entry_id: &str) -> Result<(), ServerError> {
        let ret = self.entry_mgr.remove_entry(entry_id)?;
        if !ret.is_null() {
            let entries = self.entry_mgr.list_entries(true);
            let _ = server().send_event("announcements:entries_updated", vec![json!({"entries": entries})]);
        }
        Ok(())
    }

    pub async fn get_announcements(&self, include_dismissed: bool) -> Value {
        Value::Array(self.entry_mgr.list_entries(include_dismissed))
    }

    pub fn register_feed(&self, name: &str) {
        let name = name.to_lowercase();
        if self.subscriptions.borrow().contains_key(&name) {
            log::info!("Feed {} already configured", name);
            return;
        }
        log::info!("Registering feed {}", name);
        self.configured_feeds.borrow_mut().push(name.clone());
        self.subscriptions.borrow_mut().insert(name.clone(), RssFeed::new(&name, self.dev_mode));
    }
}

pub fn load_component(config: &ConfigHelper) -> Result<Rc<Announcements>, crate::confighelper::ConfigError> {
    Announcements::new(config)
}

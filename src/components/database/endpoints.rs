// The database endpoints

use super::*;

impl MoonrakerDatabase {
    pub(super) fn is_printing() -> bool {
        server().klippy().is_printing()
    }

    pub(super) async fn handle_compact_request(self: Rc<Self>, _req: WebRequest) -> ApiResult {
        if Self::is_printing() {
            return Err(ServerError::bad("Cannot compact when Klipper is printing"));
        }
        let _g = self.backup_lock.lock().await;
        self.compact_database()
    }

    pub(super) fn check_backup_name(&self, db_name: &str) -> Result<PathBuf, ServerError> {
        let bkp_dir = self.get_backup_dir();
        let bkp_path = crate::utils::resolve_path(&bkp_dir.join(db_name));
        if !bkp_path.ancestors().skip(1).any(|p| p == bkp_dir) {
            return Err(ServerError::bad(format!("Invalid name {}.", db_name)));
        }
        Ok(bkp_path)
    }

    pub(super) async fn handle_backup_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let _g = self.backup_lock.lock().await;
        let rt = req.get_request_type();
        let bkp_path;
        if rt == RequestType::POST {
            if Self::is_printing() {
                return Err(ServerError::bad("Cannot backup when Klipper is printing"));
            }
            let suffix = crate::utils::local_strftime("%Y%m%d-%H%M%S");
            let db_name = req.get_str_or("filename", &format!("sqldb-backup-{}.db", suffix));
            bkp_path = self.check_backup_name(&db_name)?;
            self.backup_database(&bkp_path)?;
        } else if rt == RequestType::DELETE {
            let db_name = req.get_str("filename")?;
            bkp_path = self.check_backup_name(&db_name)?;
            if !bkp_path.is_file() {
                return Err(ServerError::new(format!("Backup file {} does not exist", db_name), 404));
            }
            std::fs::remove_file(&bkp_path)?;
        } else {
            return Err(ServerError::bad("Invalid request type"));
        }
        Ok(json!({"backup_path": bkp_path.to_string_lossy()}))
    }

    pub(super) async fn handle_restore_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        if Self::is_printing() {
            return Err(ServerError::bad("Cannot restore when Klipper is printing"));
        }
        let _g = self.backup_lock.lock().await;
        let db_name = req.get_str("filename")?;
        let restore_path = self.check_backup_name(&db_name)?;
        let info = self.restore_database(&restore_path)?;
        server().restart(Some(0.1));
        Ok(info)
    }

    pub(super) async fn handle_list_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let path = req.get_endpoint().to_string();
        let mut ns_list: Vec<String> = self.namespaces();
        let bkp_dir = self.get_backup_dir();
        let mut backups = Vec::new();
        if bkp_dir.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&bkp_dir) {
                for e in rd.flatten() {
                    if e.path().is_file() {
                        backups.push(e.file_name().to_string_lossy().to_string());
                    }
                }
            }
        }
        if !path.starts_with("/debug/") {
            let fb = self.forbidden_namespaces.borrow();
            ns_list.retain(|n| !fb.contains(n));
            Ok(json!({"namespaces": ns_list, "backups": backups}))
        } else {
            Ok(json!({"namespaces": ns_list, "backups": backups, "tables": self.tables()}))
        }
    }

    pub(super) async fn handle_item_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let req_type = req.get_request_type();
        let is_debug = req.get_endpoint().starts_with("/debug/");
        let namespace = req.get_str("namespace")?;
        if self.forbidden_namespaces.borrow().contains(&namespace) && !is_debug {
            return Err(ServerError::new(format!("Read/Write access to namespace '{}' is forbidden", namespace), 403));
        }
        let key;
        let val;
        if req_type == RequestType::GET {
            key = req.get_opt("key").unwrap_or(Value::Null);
            if !key.is_null() && !(key.is_array() || key.is_string()) {
                return Err(ServerError::bad(format!(
                    "Value for argument 'key' is an invalid type: {}",
                    crate::pyfmt::py_type_name(&key)
                )));
            }
            val = self.get_item_sync(&namespace, if key.is_null() { None } else { Some(&key) }, None)?;
        } else {
            if self.protected_namespaces.borrow().contains(&namespace) && !is_debug {
                return Err(ServerError::new(format!("Write access to namespace '{}' is forbidden", namespace), 403));
            }
            key = req.get("key")?;
            if !(key.is_array() || key.is_string()) {
                return Err(ServerError::bad(format!(
                    "Value for argument 'key' is an invalid type: {}",
                    crate::pyfmt::py_type_name(&key)
                )));
            }
            if req_type == RequestType::POST {
                val = req.get("value")?;
                self.insert_item_sync(&namespace, &key, val.clone())?;
            } else if req_type == RequestType::DELETE {
                val = self.delete_item_sync(&namespace, &key)?;
                self.drop_empty_namespace(&namespace)?;
            } else {
                return Err(ServerError::bad(format!("Invalid request type {}", req_type)));
            }
        }
        if is_debug {
            let name = req_type.name().unwrap_or("").to_lowercase();
            {
                let mut dc = self.debug_counter.borrow_mut();
                let cur = dc.get(&name).and_then(|v| v.as_i64()).unwrap_or(0);
                dc.insert(name, json!(cur + 1));
            }
            let dc = Value::Object(self.debug_counter.borrow().clone());
            self.insert_item_sync("database", &json!("debug_counter"), dc.clone())?;
            server().add_log_rollover_item(
                "database_debug_counter",
                &format!("Database Debug Counter: {}", crate::pyfmt::py_repr(&dc)),
                false,
            );
        }
        Ok(json!({"namespace": namespace, "key": key, "value": val}))
    }

    pub(super) async fn handle_table_request(self: Rc<Self>, req: WebRequest) -> ApiResult {
        let table = req.get_str("table")?;
        if !self.tables.borrow().contains(&table) {
            return Err(ServerError::new(format!("Table name '{}' does not exist", table), 404));
        }
        let rows = self.sql_query(&format!("SELECT rowid, * FROM {}", table), &[])?;
        Ok(json!({"table_name": table, "rows": rows}))
    }
}

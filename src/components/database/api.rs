// Async wrappers matching the Python API

use super::*;

impl MoonrakerDatabase {
    pub async fn insert_item(&self, namespace: &str, key: &Value, value: Value) -> Result<(), ServerError> {
        self.insert_item_sync(namespace, key, value)
    }

    pub fn register_local_namespace(self: &Rc<Self>, namespace: &str, forbidden: bool, parse_keys: bool) -> Result<NamespaceWrapper, ServerError> {
        if self.registered_namespaces.borrow().contains(namespace) {
            return Err(ServerError::bad(format!("Namespace '{}' already registered", namespace)));
        }
        self.registered_namespaces.borrow_mut().insert(namespace.to_string());
        self.namespaces.borrow_mut().insert(namespace.to_string());
        if forbidden {
            if !self.forbidden_namespaces.borrow().contains(namespace) {
                self.forbidden_namespaces.borrow_mut().insert(namespace.to_string());
                let list: Vec<Value> = self.forbidden_namespaces.borrow().iter().map(|s| json!(s)).collect();
                let _ = self.insert_item_sync("database", &json!("forbidden_namespaces"), Value::Array(list));
            }
        } else if !self.protected_namespaces.borrow().contains(namespace) {
            self.protected_namespaces.borrow_mut().insert(namespace.to_string());
            let list: Vec<Value> = self.protected_namespaces.borrow().iter().map(|s| json!(s)).collect();
            let _ = self.insert_item_sync("database", &json!("protected_namespaces"), Value::Array(list));
        }
        Ok(NamespaceWrapper { namespace: namespace.to_string(), db: self.clone(), parse_keys })
    }

    pub fn wrap_namespace(self: &Rc<Self>, namespace: &str, parse_keys: bool) -> Result<NamespaceWrapper, ServerError> {
        if !self.namespaces.borrow().contains(namespace) {
            return Err(ServerError::new(format!("Namespace '{}' not found", namespace), 404));
        }
        Ok(NamespaceWrapper { namespace: namespace.to_string(), db: self.clone(), parse_keys })
    }

    pub fn unregister_local_namespace(&self, namespace: &str) {
        self.registered_namespaces.borrow_mut().remove(namespace);
        if self.forbidden_namespaces.borrow_mut().remove(namespace) {
            let list: Vec<Value> = self.forbidden_namespaces.borrow().iter().map(|s| json!(s)).collect();
            let _ = self.insert_item_sync("database", &json!("forbidden_namespaces"), Value::Array(list));
        }
        if self.protected_namespaces.borrow_mut().remove(namespace) {
            let list: Vec<Value> = self.protected_namespaces.borrow().iter().map(|s| json!(s)).collect();
            let _ = self.insert_item_sync("database", &json!("protected_namespaces"), Value::Array(list));
        }
    }

    pub fn wipe_local_namespace(&self, namespace: &str) -> Result<(), ServerError> {
        self.clear_namespace(namespace)?;
        self.drop_empty_namespace(namespace)?;
        self.unregister_local_namespace(namespace);
        Ok(())
    }

    pub fn get_backup_dir(&self) -> PathBuf {
        crate::utils::resolve_path(&PathBuf::from(server().data_path()).join("backup/database"))
    }

    pub fn register_table(&self, table_def: &dyn SqlTableDefinition) -> Result<(), ServerError> {
        let name = table_def.name().to_string();
        if self.registered_tables.borrow().contains(&name) {
            return Err(ServerError::bad(format!("Table '{}' already registered", name)));
        }
        self.registered_tables.borrow_mut().insert(name.clone());
        if self.initialized.get() {
            return Err(ServerError::bad("Table registration must occur during during init."));
        }
        let prototype = cleandoc(&table_def.prototype());
        let (detected_proto, version) = if self.tables.borrow().contains(&name) {
            log::info!("Found registered table {}", name);
            if name == NAMESPACE_TABLE || name == REGISTRATION_TABLE {
                return Err(ServerError::bad(format!("Cannot register table '{}', it is reserved", name)));
            }
            self.lookup_registered_table(&name)
        } else {
            log::info!("Creating table {}...", name);
            self.with_commit(|c| c.execute(&format!("CREATE TABLE {}", prototype), []))?;
            (prototype.clone(), 0)
        };
        if table_def.version() > version {
            table_def.migrate(version, self)?;
            self.save_registered_table(&name, &prototype, table_def.version())?;
        } else if detected_proto != prototype {
            server().add_warning(
                &format!("Table '{}' definition does not match stored definition.  See the log for details.", name),
                None,
                true,
            );
            log::info!("Expected table prototype:\n{}\n\nStored table prototype:\n{}", prototype, detected_proto);
        }
        Ok(())
    }

    /// Execute a statement returning rows as json objects.  Columns are
    /// converted according to their declared types.
    pub fn sql_query(&self, sql: &str, params: &[rusqlite::types::Value]) -> Result<Vec<Map<String, Value>>, ServerError> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(sql)?;
        let cols: Vec<(String, Option<String>)> =
            stmt.columns().iter().map(|c| (c.name().to_string(), c.decl_type().map(String::from))).collect();
        let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let mut m = Map::new();
            for (i, (name, dt)) in cols.iter().enumerate() {
                m.insert(name.clone(), column_to_json(row.get_ref(i)?, dt.as_deref()));
            }
            out.push(m);
        }
        Ok(out)
    }

    /// Execute a statement, returning (rowcount, lastrowid)
    pub fn sql_execute(&self, sql: &str, params: &[rusqlite::types::Value]) -> Result<(usize, i64), ServerError> {
        let n = self.with_commit(|c| c.execute(sql, rusqlite::params_from_iter(params.iter())))?;
        Ok((n, self.conn.borrow().last_insert_rowid()))
    }

    pub fn sql_executemany(&self, sql: &str, params: &[Vec<rusqlite::types::Value>]) -> Result<usize, ServerError> {
        Ok(self.with_commit(|tx| {
            let mut stmt = tx.prepare(sql)?;
            let mut total = 0;
            for p in params {
                total += stmt.execute(rusqlite::params_from_iter(p.iter()))?;
            }
            Ok(total)
        })?)
    }

    pub fn compact_database(&self) -> Result<Value, ServerError> {
        if self.restored.get() {
            return Err(ServerError::bad("Cannot compact restored database, awaiting restart"));
        }
        let cur_size = std::fs::metadata(&self.sql_db).map(|m| m.len()).unwrap_or(0);
        self.conn.borrow().execute_batch("VACUUM")?;
        let new_size = std::fs::metadata(&self.sql_db).map(|m| m.len()).unwrap_or(0);
        Ok(json!({"previous_size": cur_size, "new_size": new_size}))
    }

    pub fn backup_database(&self, bkp_path: &Path) -> Result<(), ServerError> {
        if self.restored.get() {
            return Err(ServerError::bad("Cannot backup restored database, awaiting restart"));
        }
        if let Some(parent) = bkp_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            } else if bkp_path.exists() {
                std::fs::remove_file(bkp_path)?;
            }
        }
        let mut dst = Connection::open(bkp_path)?;
        let conn = self.conn.borrow();
        let backup = rusqlite::backup::Backup::new(&conn, &mut dst)?;
        backup.run_to_completion(100, std::time::Duration::from_millis(0), None)?;
        Ok(())
    }

    pub fn restore_database(&self, restore_path: &Path) -> Result<Value, ServerError> {
        if self.restored.get() {
            return Err(ServerError::bad("Database already restored"));
        }
        if !restore_path.is_file() {
            return Err(ServerError::bad(format!("Restoration File {} does not exist", restore_path.display())));
        }
        let src = Connection::open(restore_path)?;
        let tables: Vec<String> = {
            let mut stmt = src.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        if !tables.iter().any(|t| t == NAMESPACE_TABLE) {
            return Err(ServerError::bad(format!("Invalid database for restoration, missing table '{}'", NAMESPACE_TABLE)));
        }
        let missing: Vec<String> = self.tables.borrow().iter().filter(|t| !tables.contains(t)).cloned().collect();
        if !missing.is_empty() {
            log::info!("Database to restore missing tables: {:?}", missing);
        }
        let namespaces: Vec<String> = {
            let mut stmt = src.prepare(&format!("SELECT DISTINCT namespace FROM {}", NAMESPACE_TABLE))?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        let missing_ns: Vec<String> = self.namespaces.borrow().iter().filter(|n| !namespaces.contains(n)).cloned().collect();
        if !missing_ns.is_empty() {
            log::info!("Database to restore missing namespaces: {:?}", missing_ns);
        }
        {
            let mut conn = self.conn.borrow_mut();
            let backup = rusqlite::backup::Backup::new(&src, &mut conn)?;
            backup.run_to_completion(100, std::time::Duration::from_millis(0), None)?;
        }
        self.restored.set(true);
        Ok(json!({"restored_tables": tables, "restored_namespaces": namespaces}))
    }

    pub fn tables(&self) -> Vec<String> {
        self.tables.borrow().iter().cloned().collect()
    }

    pub fn namespaces(&self) -> Vec<String> {
        self.namespaces.borrow().iter().cloned().collect()
    }

    pub fn registered_namespaces(&self) -> Vec<String> {
        self.registered_namespaces.borrow().iter().cloned().collect()
    }

    pub fn protected_namespaces(&self) -> Vec<String> {
        self.protected_namespaces.borrow().iter().cloned().collect()
    }

    pub fn forbidden_namespaces(&self) -> Vec<String> {
        self.forbidden_namespaces.borrow().iter().cloned().collect()
    }
}

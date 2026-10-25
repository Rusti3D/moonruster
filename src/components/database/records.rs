// Record level operations

use super::*;

impl MoonrakerDatabase {
    pub(super) fn insert_record(&self, namespace: &str, key: &str, val: &Value) -> bool {
        if val.is_null() {
            return false;
        }
        let enc = match encode_record(val) {
            Ok(e) => e,
            Err(e) => {
                log::info!("{}", e);
                return false;
            }
        };
        let res = self.with_commit(|c| {
            c.execute(
                &format!(
                    "INSERT INTO {} VALUES(?, ?, ?) ON CONFLICT(namespace, key) DO UPDATE SET value=excluded.value",
                    NAMESPACE_TABLE
                ),
                params![namespace, key, enc],
            )
        });
        if res.is_err() {
            if server().is_verbose_enabled() {
                log::error!("Error inserting record for key");
            }
            return false;
        }
        true
    }

    pub(super) fn get_record(&self, namespace: &str, key: &str) -> Result<Option<Value>, ServerError> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare_cached(&format!("SELECT value FROM {} WHERE namespace = ? and key = ?", NAMESPACE_TABLE))?;
        let mut rows = stmt.query(params![namespace, key])?;
        match rows.next()? {
            None => Ok(None),
            Some(row) => Ok(Some(decode_record(&value_ref_bytes(row.get_ref(0)?))?)),
        }
    }

    pub(super) fn get_record_or_404(&self, namespace: &str, key: &str) -> Result<Value, ServerError> {
        self.get_record(namespace, key)?
            .ok_or_else(|| ServerError::new(format!("Key '{}' in namespace '{}' not found", key, namespace), 404))
    }

    pub fn get_namespace(&self, namespace: &str, must_exist: bool) -> Result<Map<String, Value>, ServerError> {
        if !self.namespaces.borrow().contains(namespace) {
            if !must_exist {
                return Ok(Map::new());
            }
            return Err(ServerError::new(format!("Namespace {} not found", namespace), 404));
        }
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(&format!("SELECT key, value FROM {} WHERE namespace = ?", NAMESPACE_TABLE))?;
        let mut rows = stmt.query(params![namespace])?;
        let mut out = Map::new();
        while let Some(row) = rows.next()? {
            let k: String = row.get(0)?;
            let v = decode_record(&value_ref_bytes(row.get_ref(1)?))?;
            out.insert(k, v);
        }
        Ok(out)
    }

    pub fn clear_namespace(&self, namespace: &str) -> Result<(), ServerError> {
        self.with_commit(|c| c.execute(&format!("DELETE FROM {} WHERE namespace = ?", NAMESPACE_TABLE), params![namespace]))?;
        Ok(())
    }

    pub fn drop_empty_namespace(&self, namespace: &str) -> Result<(), ServerError> {
        if self.namespaces.borrow().contains(namespace) && self.get_namespace_length(namespace)? == 0 {
            self.namespaces.borrow_mut().remove(namespace);
        }
        Ok(())
    }

    pub fn get_namespace_length(&self, namespace: &str) -> Result<i64, ServerError> {
        Ok(self.conn.borrow().query_row(
            &format!("SELECT COUNT(namespace) FROM {} WHERE namespace = ?", NAMESPACE_TABLE),
            params![namespace],
            |r| r.get(0),
        )?)
    }

    pub fn get_namespace_keys(&self, namespace: &str) -> Result<Vec<String>, ServerError> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(&format!("SELECT key FROM {} WHERE namespace = ?", NAMESPACE_TABLE))?;
        let rows = stmt.query_map(params![namespace], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn get_namespace_values(&self, namespace: &str) -> Result<Vec<Value>, ServerError> {
        Ok(self.get_namespace_items(namespace)?.into_iter().map(|(_, v)| v).collect())
    }

    pub fn get_namespace_items(&self, namespace: &str) -> Result<Vec<(String, Value)>, ServerError> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(&format!("SELECT key, value FROM {} WHERE namespace = ?", NAMESPACE_TABLE))?;
        let mut rows = stmt.query(params![namespace])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push((row.get::<_, String>(0)?, decode_record(&value_ref_bytes(row.get_ref(1)?))?));
        }
        Ok(out)
    }

    pub fn insert_item_sync(&self, namespace: &str, key: &Value, value: Value) -> Result<(), ServerError> {
        let key_list = parse_namespace_key(key)?;
        let record = if key_list.len() > 1 {
            let mut record = self.get_record(namespace, &key_list[0])?.unwrap_or(json!({}));
            if !record.is_object() {
                log::info!(
                    "Warning: Key {} contains a value of type <class '{}'>. Overwriting with an object.",
                    key_list[0],
                    crate::pyfmt::py_type_name(&record)
                );
                record = json!({});
            }
            {
                let mut item = &mut record;
                for k in &key_list[1..key_list.len() - 1] {
                    let tname = crate::pyfmt::py_type_name(item);
                    let obj = item
                        .as_object_mut()
                        .ok_or_else(|| ServerError::bad(format!("Cannot reduce a value of type <class '{}'>", tname)))?;
                    item = obj.entry(k.clone()).or_insert_with(|| json!({}));
                }
                match item.as_object_mut() {
                    Some(o) => {
                        o.insert(key_list.last().unwrap().clone(), value);
                    }
                    None => {
                        let rpt_key = key_list[..key_list.len() - 1].join(".");
                        return Err(ServerError::bad(format!(
                            "Item at key '{}' in namespace '{}'is not a dictionary object, cannot insert",
                            rpt_key, namespace
                        )));
                    }
                }
            }
            record
        } else {
            value
        };
        if !self.insert_record(namespace, &key_list[0], &record) {
            log::info!("Error inserting key '{}' in namespace '{}'", crate::pyfmt::py_str(key), namespace);
        } else {
            self.namespaces.borrow_mut().insert(namespace.to_string());
        }
        Ok(())
    }

    pub fn delete_item_sync(&self, namespace: &str, key: &Value) -> Result<Value, ServerError> {
        let key_list = parse_namespace_key(key)?;
        let mut record = self.get_record_or_404(namespace, &key_list[0])?;
        let mut val = record.clone();
        let mut remove_record = true;
        if key_list.len() > 1 {
            let not_found = || ServerError::new(format!("Key '{}' in namespace '{}' not found", crate::pyfmt::py_str(key), namespace), 404);
            if !record.is_object() {
                return Err(not_found());
            }
            {
                let mut item = &mut record;
                for k in &key_list[1..key_list.len() - 1] {
                    item = item.as_object_mut().and_then(|o| o.get_mut(k)).ok_or_else(not_found)?;
                }
                val = item
                    .as_object_mut()
                    .and_then(|o| o.shift_remove(key_list.last().unwrap()))
                    .ok_or_else(not_found)?;
            }
            remove_record = record.as_object().map(|o| o.is_empty()).unwrap_or(true);
        }
        if remove_record {
            self.with_commit(|c| {
                c.execute(&format!("DELETE FROM {} WHERE namespace = ? and key = ?", NAMESPACE_TABLE), params![namespace, key_list[0]])
            })?;
        } else if !self.insert_record(namespace, &key_list[0], &record) {
            log::info!("Error deleting key '{}' from namespace '{}'", crate::pyfmt::py_str(key), namespace);
        }
        Ok(val)
    }

    /// get_item with optional default (None = MISSING)
    pub fn get_item_sync(&self, namespace: &str, key: Option<&Value>, default: Option<Value>) -> Result<Value, ServerError> {
        let res: Result<Value, ServerError> = (|| {
            let key = match key {
                None => return Ok(Value::Object(self.get_namespace(namespace, true)?)),
                Some(k) => k,
            };
            let key_list = parse_namespace_key(key)?;
            let rec = self.get_record_or_404(namespace, &key_list[0])?;
            let mut cur = &rec;
            for k in &key_list[1..] {
                cur = getitem(cur, k)
                    .ok_or_else(|| ServerError::new(format!("Key '{}' in namespace '{}' not found", crate::pyfmt::py_str(key), namespace), 404))?;
            }
            Ok(cur.clone())
        })();
        match res {
            Ok(v) => Ok(v),
            Err(e) => match default {
                Some(d) => Ok(d),
                None => Err(e),
            },
        }
    }

    /// SqliteProvider.update_item: dict values merge into a dict record or
    /// item, anything else replaces it
    pub fn update_item_sync(&self, namespace: &str, key: &Value, value: Value) -> Result<(), ServerError> {
        let key_list = parse_namespace_key(key)?;
        let mut record = self.get_record_or_404(namespace, &key_list[0])?;
        let merge = |slot: &mut Value, value: Value| match (slot.as_object_mut(), value) {
            (Some(o), Value::Object(v)) => o.extend(v),
            (_, v) => *slot = v,
        };
        if key_list.len() == 1 {
            merge(&mut record, value);
        } else {
            let not_found = || ServerError::new(format!("Key '{}' in namespace '{}' not found", crate::pyfmt::py_str(key), namespace), 404);
            if !record.is_object() {
                return Err(not_found());
            }
            let mut item = &mut record;
            for k in &key_list[1..key_list.len() - 1] {
                item = item.as_object_mut().and_then(|o| o.get_mut(k)).ok_or_else(not_found)?;
            }
            let last = key_list.last().unwrap();
            match item.as_object_mut().and_then(|o| o.get_mut(last)) {
                Some(slot) => merge(slot, value),
                None => {
                    return Err(ServerError::bad(format!(
                        "Item at key '{}' in namespace '{}'is not a dictionary object, cannot update",
                        key_list[..key_list.len() - 1].join("."),
                        namespace
                    )))
                }
            }
        }
        if !self.insert_record(namespace, &key_list[0], &record) {
            log::info!("Error updating key '{}' in namespace '{}'", crate::pyfmt::py_str(key), namespace);
        }
        Ok(())
    }

    /// SqliteProvider.insert_batch: every record in one transaction
    pub fn insert_batch_sync(&self, namespace: &str, records: &Map<String, Value>) -> Result<(), ServerError> {
        let encoded = records.iter().map(|(k, v)| Ok((k, encode_record(v)?))).collect::<Result<Vec<_>, ServerError>>()?;
        self.with_commit(|tx| {
        for (k, enc) in encoded {
            tx.execute(
                &format!("INSERT INTO {} VALUES(?, ?, ?) ON CONFLICT(namespace, key) DO UPDATE SET value=excluded.value", NAMESPACE_TABLE),
                params![namespace, k, enc],
            )?;
        }
            Ok(())
        })?;
        self.namespaces.borrow_mut().insert(namespace.to_string());
        Ok(())
    }

    /// SqliteProvider.sync_namespace: replaces the namespace's records. As
    /// there, the values go in through sqlite3's adapters, not encode_record
    pub fn sync_namespace_sync(&self, namespace: &str, values: &Map<String, Value>) -> Result<(), ServerError> {
        self.with_commit(|tx| {
        tx.execute(&format!("DELETE FROM {} WHERE namespace = ?", NAMESPACE_TABLE), params![namespace])?;
        for (k, v) in values {
            tx.execute(&format!("INSERT INTO {} VALUES(?, ?, ?)", NAMESPACE_TABLE), params![namespace, k, json_to_sql(v)])?;
        }
            Ok(())
        })?;
        Ok(())
    }

    /// SqliteProvider.is_alive(): its thread starts in component_init, before
    /// that calls run in place
    pub fn is_alive(&self) -> bool {
        self.initialized.get()
    }

    pub fn move_batch_sync(&self, namespace: &str, source: &[String], dest: &[String]) -> Result<(), ServerError> {
        self.with_commit(|tx| {
        for (s, d) in source.iter().zip(dest.iter()) {
            tx.execute(
                &format!("UPDATE OR REPLACE {} SET key = ? WHERE namespace = ? and key = ?", NAMESPACE_TABLE),
                params![d, namespace, s],
            )?;
        }
            Ok(())
        })?;
        Ok(())
    }

    pub fn delete_batch_sync(&self, namespace: &str, keys: &[String]) -> Result<Map<String, Value>, ServerError> {
        let vals = self.get_batch_sync(namespace, keys)?;
        self.with_commit(|tx| {
        for k in keys {
            tx.execute(&format!("DELETE FROM {} WHERE namespace = ? and key = ?", NAMESPACE_TABLE), params![namespace, k])?;
        }
            Ok(())
        })?;
        Ok(vals)
    }

    pub fn get_batch_sync(&self, namespace: &str, keys: &[String]) -> Result<Map<String, Value>, ServerError> {
        let mut out = Map::new();
        if keys.is_empty() {
            return Ok(out);
        }
        let placeholders = vec!["?"; keys.len()].join(",");
        let sql = format!("SELECT key, value FROM {} WHERE namespace = ? and key IN ({})", NAMESPACE_TABLE, placeholders);
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(&sql)?;
        let mut p: Vec<&dyn rusqlite::ToSql> = vec![&namespace];
        for k in keys {
            p.push(k);
        }
        let mut rows = stmt.query(p.as_slice())?;
        while let Some(row) = rows.next()? {
            out.insert(row.get::<_, String>(0)?, decode_record(&value_ref_bytes(row.get_ref(1)?))?);
        }
        Ok(out)
    }
}

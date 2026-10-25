// NamespaceWrapper

use super::*;

#[derive(Clone)]
pub struct NamespaceWrapper {
    pub namespace: String,
    pub db: Rc<MoonrakerDatabase>,
    pub parse_keys: bool,
}

impl NamespaceWrapper {
    pub(super) fn key(&self, key: &str) -> Value {
        if self.parse_keys {
            Value::String(key.to_string())
        } else {
            json!([key])
        }
    }

    pub fn insert(&self, key: &str, value: Value) -> Result<(), ServerError> {
        self.db.insert_item_sync(&self.namespace, &self.key(key), value)
    }

    pub fn get(&self, key: &str, default: Value) -> Value {
        self.db.get_item_sync(&self.namespace, Some(&self.key(key)), Some(default.clone())).unwrap_or(default)
    }

    pub fn get_strict(&self, key: &str) -> ApiResult {
        self.db.get_item_sync(&self.namespace, Some(&self.key(key)), None)
    }

    pub fn delete(&self, key: &str) -> ApiResult {
        self.db.delete_item_sync(&self.namespace, &self.key(key))
    }

    pub fn move_batch(&self, source: &[String], dest: &[String]) -> Result<(), ServerError> {
        self.db.move_batch_sync(&self.namespace, source, dest)
    }

    pub fn delete_batch(&self, keys: &[String]) -> Result<Map<String, Value>, ServerError> {
        self.db.delete_batch_sync(&self.namespace, keys)
    }

    pub fn as_dict(&self) -> Map<String, Value> {
        self.db.get_namespace(&self.namespace, false).unwrap_or_default()
    }

    pub fn keys(&self) -> Vec<String> {
        self.db.get_namespace_keys(&self.namespace).unwrap_or_default()
    }

    pub fn values(&self) -> Vec<Value> {
        self.db.get_namespace_values(&self.namespace).unwrap_or_default()
    }

    pub fn items(&self) -> Vec<(String, Value)> {
        self.db.get_namespace_items(&self.namespace).unwrap_or_default()
    }

    pub fn pop(&self, key: &str, default: Option<Value>) -> ApiResult {
        match self.delete(key) {
            Ok(v) => Ok(v),
            Err(e) => match default {
                Some(d) => Ok(d),
                None => Err(e),
            },
        }
    }

    pub fn clear(&self) -> Result<(), ServerError> {
        self.db.clear_namespace(&self.namespace)
    }
}

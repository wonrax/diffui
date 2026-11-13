//! `kv`: one JSON file per plugin.

use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::host::HostError;

/// How large a plugin's store may grow, serialised.
const MAX_BYTES: usize = 1024 * 1024;

pub(crate) struct Kv {
    path: PathBuf,
    /// Read on first use.
    data: Option<Map<String, Value>>,
}

impl Kv {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path, data: None }
    }

    fn data(&mut self) -> Result<&mut Map<String, Value>, HostError> {
        if self.data.is_none() {
            let data = match std::fs::read_to_string(&self.path) {
                Ok(text) => {
                    serde_json::from_str(&text).map_err(|error| self.failed("read", error))?
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Map::new(),
                Err(error) => return Err(self.failed("read", error)),
            };
            self.data = Some(data);
        }
        Ok(self.data.get_or_insert_default())
    }

    fn failed(&self, verb: &str, error: impl std::fmt::Display) -> HostError {
        HostError::new(
            "Error",
            format!("kv: can't {verb} {}: {error}", self.path.display()),
        )
    }

    pub(crate) fn get(&mut self, key: &str) -> Result<Value, HostError> {
        Ok(self.data()?.get(key).cloned().unwrap_or(Value::Null))
    }

    pub(crate) fn set(&mut self, key: String, value: Value) -> Result<(), HostError> {
        let before = self.data()?.insert(key.clone(), value);
        self.save().inspect_err(|_| {
            // What's in memory stays what's on disk.
            let data = self.data.get_or_insert_default();
            match before {
                Some(before) => data.insert(key, before),
                None => data.remove(&key),
            };
        })
    }

    pub(crate) fn delete(&mut self, key: &str) -> Result<(), HostError> {
        if self.data()?.remove(key).is_some() {
            self.save()?;
        }
        Ok(())
    }

    fn save(&mut self) -> Result<(), HostError> {
        let text = serde_json::to_string(self.data.get_or_insert_default())
            .map_err(|error| self.failed("write", error))?;
        if text.len() > MAX_BYTES {
            return Err(HostError::new(
                "Error",
                format!(
                    "kv: the store would pass {} KB; delete something first",
                    MAX_BYTES / 1024
                ),
            ));
        }
        // Whole-file replace, so a crash mid-write leaves the old store.
        let scratch = self.path.with_extension("json.tmp");
        self.path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&scratch, text))
            .and_then(|()| std::fs::rename(&scratch, &self.path))
            .map_err(|error| self.failed("write", error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_survive_a_reopen_and_a_full_store_refuses() {
        let dir = std::env::temp_dir().join(format!("diffui-kv-{}", std::process::id()));
        let path = dir.join("demo.json");
        let mut kv = Kv::new(path.clone());
        assert_eq!(kv.get("seen").unwrap(), Value::Null);
        kv.set("seen".into(), serde_json::json!({ "pr": 41 }))
            .unwrap();
        kv.set("gone".into(), Value::Bool(true)).unwrap();
        kv.delete("gone").unwrap();

        let mut reopened = Kv::new(path);
        assert_eq!(
            reopened.get("seen").unwrap(),
            serde_json::json!({ "pr": 41 })
        );
        assert_eq!(reopened.get("gone").unwrap(), Value::Null);

        let error = reopened
            .set("big".into(), Value::String("x".repeat(MAX_BYTES)))
            .unwrap_err();
        assert!(
            error.message.contains("the store would pass 1024 KB"),
            "{error:?}"
        );
        assert_eq!(reopened.get("big").unwrap(), Value::Null);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

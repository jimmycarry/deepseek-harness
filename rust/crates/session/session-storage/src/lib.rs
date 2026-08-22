//! In-memory blob store.

use std::collections::HashMap;
use std::sync::Mutex;

/// Byte store keyed by caller-chosen ids.
#[derive(Default)]
pub struct BlobStore {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
}

impl BlobStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace `bytes` under `key`.
    pub fn put(&self, key: impl Into<String>, bytes: impl Into<Vec<u8>>) {
        self.blobs
            .lock()
            .expect("blobs")
            .insert(key.into(), bytes.into());
    }

    /// Read a copy of the bytes stored under `key`.
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.blobs.lock().expect("blobs").get(key).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_then_get() {
        let store = BlobStore::new();
        store.put("a", b"hello".as_slice());
        assert_eq!(store.get("a").as_deref(), Some(b"hello".as_slice()));
        assert!(store.get("missing").is_none());
    }
}

//! Attachment save/load over [`BlobStore`](dsh_session_storage::BlobStore).

use dsh_session_storage::BlobStore;
use uuid::Uuid;

/// Durable attachment reference stored beside its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// Blob-store key.
    pub id: String,
    /// Declared media type.
    pub media_type: String,
}

impl Attachment {
    /// Persist `bytes` and return the reference.
    pub fn save(store: &BlobStore, media_type: impl Into<String>, bytes: &[u8]) -> Self {
        let id = Uuid::new_v4().to_string();
        store.put(&id, bytes.to_vec());
        Self {
            id,
            media_type: media_type.into(),
        }
    }

    /// Load bytes for a previously saved id.
    pub fn load(store: &BlobStore, id: &str) -> Option<Vec<u8>> {
        store.get(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_then_load() {
        let store = BlobStore::new();
        let attachment = Attachment::save(&store, "image/png", b"\x89PNG");
        assert_eq!(
            Attachment::load(&store, &attachment.id).as_deref(),
            Some(b"\x89PNG".as_slice())
        );
        assert!(Attachment::load(&store, "missing").is_none());
    }
}

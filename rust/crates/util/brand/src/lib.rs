//! The `Branded<B>` nominal-typing primitive.
//!
//! A brand makes structurally identical strings non-interchangeable at the
//! type level: a `SessionId` cannot be passed where a `CallId` is expected.
//! Construction goes through a per-id factory in the owning package.
//!
//! This crate owns only the primitive — no concrete id.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

/// A string carrying a compile-time-only brand `B`.
#[derive(Eq)]
pub struct Branded<B> {
    value: String,
    _brand: PhantomData<B>,
}

impl<B> Branded<B> {
    /// Brand a raw string. Call only from the owning package's factory.
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            _brand: PhantomData,
        }
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Consume and return the raw string.
    pub fn into_inner(self) -> String {
        self.value
    }
}

impl<B> Clone for Branded<B> {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
            _brand: PhantomData,
        }
    }
}

impl<B> PartialEq for Branded<B> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<B> Hash for Branded<B> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

impl<B> fmt::Debug for Branded<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Branded").field(&self.value).finish()
    }
}

impl<B> fmt::Display for Branded<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.value)
    }
}

impl<B> AsRef<str> for Branded<B> {
    fn as_ref(&self) -> &str {
        &self.value
    }
}

impl<B> Serialize for Branded<B> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.value)
    }
}

impl<'de, B> Deserialize<'de> for Branded<B> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::new(String::deserialize(deserializer)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SessionBrand;
    struct CallBrand;
    type SessionId = Branded<SessionBrand>;
    type CallId = Branded<CallBrand>;

    #[test]
    fn same_brand_compares_as_string() {
        let a = SessionId::new("abc");
        let b = SessionId::new("abc");
        assert_eq!(a, b);
        assert_eq!(a.as_str(), "abc");
    }

    #[test]
    fn distinct_brands_are_distinct_types() {
        let session = SessionId::new("shared");
        let call = CallId::new("shared");
        assert_eq!(session.as_str(), call.as_str());
        fn takes_session(_: &SessionId) {}
        takes_session(&session);
        // takes_session(&call); // does not compile
    }

    #[test]
    fn serde_round_trip_is_a_plain_string() {
        let id = SessionId::new("s1");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"s1\"");
        let back: SessionId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }
}

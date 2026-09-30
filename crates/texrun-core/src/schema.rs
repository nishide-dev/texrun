//! Wire-format conventions for serialized domain types.
//!
//! Conventions shared by every serializable type in this crate:
//!
//! - field names are `snake_case`;
//! - enums are serialized as `snake_case` strings (unit variants) so that new
//!   variants can be added without changing the shape of existing ones;
//! - durations are integer milliseconds in fields suffixed `_ms`;
//! - paths are workspace-relative, `/`-separated strings
//!   ([`WorkspacePath`](crate::WorkspacePath)).
//!
//! The domain types themselves do not carry a schema version. A top-level
//! document (e.g. the output of `texrun compile --json`) wraps its payload in
//! [`Versioned`], which adds a `schema_version` field next to the payload's own
//! fields. Keeping the version in the envelope means it appears exactly once
//! per document, and nested values (a `Diagnostic` inside a `CompileResult`)
//! stay free of it.

use serde::{Deserialize, Serialize};

/// Version of the JSON schema produced by the types in this crate.
///
/// Bump when a serialized shape changes incompatibly (field removed or
/// renamed, meaning changed). Adding optional fields or enum variants does not
/// require a bump; consumers must tolerate unknown fields and variants.
pub const SCHEMA_VERSION: u32 = 1;

/// A top-level JSON document: `{"schema_version": N, ...payload fields}`.
///
/// The payload must serialize as a JSON object (a struct), since its fields are
/// flattened next to `schema_version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Versioned<T> {
    /// Schema version of this document; see [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The document payload.
    #[serde(flatten)]
    pub payload: T,
}

impl<T> Versioned<T> {
    /// Wraps `payload` with the current [`SCHEMA_VERSION`].
    pub fn new(payload: T) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            payload,
        }
    }
}

/// `serde(with = ...)` helpers for [`std::time::Duration`] as integer
/// milliseconds. Sub-millisecond precision is truncated; values beyond
/// `u64::MAX` ms saturate.
pub(crate) mod duration_ms {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(value: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(u64::try_from(value.as_millis()).unwrap_or(u64::MAX))
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        u64::deserialize(d).map(Duration::from_millis)
    }

    pub(crate) mod option {
        use std::time::Duration;

        use serde::{Deserialize, Deserializer, Serializer};

        #[expect(clippy::ref_option, reason = "signature required by serde(with)")]
        pub(crate) fn serialize<S: Serializer>(
            value: &Option<Duration>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            match value {
                Some(d) => super::serialize(d, s),
                None => s.serialize_none(),
            }
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> Result<Option<Duration>, D::Error> {
            Ok(Option::<u64>::deserialize(d)?.map(Duration::from_millis))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Payload {
        name: String,
    }

    #[test]
    fn versioned_flattens_payload_next_to_schema_version() {
        let doc = Versioned::new(Payload {
            name: "x".to_owned(),
        });
        let json = serde_json::to_value(&doc).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "schema_version": SCHEMA_VERSION, "name": "x" })
        );
        let back: Versioned<Payload> = serde_json::from_value(json).unwrap();
        assert_eq!(back, doc);
    }
}

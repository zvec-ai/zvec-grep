//! Compact JSON representation for encoded binary content.

use std::fmt;

use base64::{Engine as _, display::Base64Display, engine::general_purpose::STANDARD};
use serde::{
    Deserializer, Serializer,
    de::{Error, Visitor},
};

pub(crate) fn serialize<S: Serializer>(data: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(&Base64Display::new(data, &STANDARD))
}

pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    struct BytesVisitor;

    impl Visitor<'_> for BytesVisitor {
        type Value = Vec<u8>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a base64-encoded byte string")
        }

        fn visit_str<E: Error>(self, value: &str) -> Result<Self::Value, E> {
            STANDARD.decode(value).map_err(E::custom)
        }
    }

    deserializer.deserialize_str(BytesVisitor)
}

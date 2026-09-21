//! Canonical decimal strings avoid JSON/JavaScript and Lua integer narrowing.
use serde::{Deserialize, Deserializer, Serializer, de::Error};

fn parse(value: &str) -> Result<u64, &'static str> {
    if value.is_empty()
        || value.len() > 20
        || !value.bytes().all(|b| b.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err("usage count must be a canonical unsigned decimal string");
    }
    value
        .parse()
        .map_err(|_| "usage count exceeds the supported 64-bit range")
}

pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.to_string())
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    parse(&String::deserialize(deserializer)?).map_err(D::Error::custom)
}

pub mod optional {
    use super::*;
    pub fn serialize<S: Serializer>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => serializer.serialize_some(&value.to_string()),
            None => serializer.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u64>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|value| parse(&value).map_err(D::Error::custom))
            .transpose()
    }
}

//! A JSON value that survives the wire.
//!
//! `serde_json::Value` cannot be used here. The transport is postcard, which is
//! not self-describing: it decodes by asking the *type* what comes next, and
//! `Value` decodes by asking the *format* what it is looking at. So the wire
//! needs its own shape, and `serde_json` is used only at the edges — parsing
//! what a client typed, and rendering what a script reads.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A JSON value.
///
/// Objects are a `Vec` of pairs rather than a map: it round-trips through
/// postcard without ordering surprises, and an event payload is small enough
/// that lookup cost is irrelevant.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum Json {
    #[default]
    Null,
    Bool(bool),
    /// Kept apart from `Float` on purpose: a window or space id past 2^53
    /// silently loses precision if everything is a double.
    Int(i64),
    Float(f64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    /// Looks up a key, for an object.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Self::Object(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Parses JSON, falling back to a plain string.
    ///
    /// A config triggering an event is as likely to pass a word as a document,
    /// and `--info hello` should not be an error.
    #[must_use]
    pub fn parse_or_string(text: &str) -> Self {
        serde_json::from_str::<serde_json::Value>(text)
            .map_or_else(|_| Self::String(text.to_owned()), Self::from)
    }
}

impl From<serde_json::Value> for Json {
    fn from(value: serde_json::Value) -> Self {
        use serde_json::Value;
        match value {
            Value::Null => Self::Null,
            Value::Bool(b) => Self::Bool(b),
            Value::Number(n) => n
                .as_i64()
                .map_or_else(|| Self::Float(n.as_f64().unwrap_or(f64::NAN)), Self::Int),
            Value::String(s) => Self::String(s),
            Value::Array(items) => Self::Array(items.into_iter().map(Self::from).collect()),
            Value::Object(fields) => Self::Object(
                fields
                    .into_iter()
                    .map(|(k, v)| (k, Self::from(v)))
                    .collect(),
            ),
        }
    }
}

impl From<&Json> for serde_json::Value {
    fn from(value: &Json) -> Self {
        use serde_json::Value;
        match value {
            Json::Null => Value::Null,
            Json::Bool(b) => Value::Bool(*b),
            Json::Int(i) => Value::from(*i),
            Json::Float(f) => serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number),
            Json::String(s) => Value::String(s.clone()),
            Json::Array(items) => Value::Array(items.iter().map(Value::from).collect()),
            Json::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::from(v)))
                    .collect(),
            ),
        }
    }
}

impl fmt::Display for Json {
    /// What a script sees. A bare string renders unquoted, because a config
    /// asking for an app name wants `Finder`, not `"Finder"`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => Ok(()),
            Self::String(s) => f.write_str(s),
            other => f.write_str(&serde_json::Value::from(other).to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_word_is_a_string_not_an_error() {
        assert_eq!(Json::parse_or_string("hello"), Json::String("hello".into()));
    }

    #[test]
    fn a_document_parses() {
        let value = Json::parse_or_string(r#"{"state":"playing","track":3}"#);
        assert_eq!(value.get("state"), Some(&Json::String("playing".into())));
        assert_eq!(value.get("track"), Some(&Json::Int(3)));
    }

    #[test]
    fn integers_stay_integers() {
        // A space id past 2^53 must not become a double.
        let big = 9_007_199_254_740_993_i64;
        assert_eq!(Json::parse_or_string(&big.to_string()), Json::Int(big));
    }

    #[test]
    fn a_string_renders_unquoted_but_a_document_does_not() {
        assert_eq!(Json::String("Finder".into()).to_string(), "Finder");
        assert_eq!(Json::Int(42).to_string(), "42");
        assert_eq!(Json::Null.to_string(), "");
        let object = Json::Object(vec![("a".into(), Json::Int(1))]);
        assert_eq!(object.to_string(), r#"{"a":1}"#);
    }

    #[test]
    fn it_round_trips_through_the_wire_format() {
        // The whole reason this type exists rather than serde_json::Value.
        let value = Json::parse_or_string(r#"{"a":[1,true,"x"],"b":null}"#);
        let bytes = postcard::to_allocvec(&value).unwrap();
        assert_eq!(postcard::from_bytes::<Json>(&bytes).unwrap(), value);
    }
}

//! The format the protocol travels in, and the port types spelt with it.
//!
//! `async-mach-ports` carries no format of its own: a port is handed a
//! [`Codec`](async_mach_ports::Codec) when it is created and uses it for
//! everything that crosses it. Which one coolabah uses is a protocol decision, so
//! it is made here, once, and the daemon, the CLI and the Lua module all reach
//! for the same aliases rather than each naming a format.
//!
//! # Why `MessagePack`
//!
//! It is *self-describing*: `to_vec_named` writes struct fields as a map of
//! names, so a reader can ask what is in front of it — `deserialize_any` — and
//! serde's declarative attributes work. That is not a nicety here. The protocol
//! accepts two spellings of several properties (`label = "12:00"` as well as
//! `label = { string = "12:00" }`, `drawing = true` as well as
//! `drawing = "on"`), which is exactly what `#[serde(untagged)]` is for, and
//! capturing keys the daemon does not name is exactly what `#[serde(flatten)]`
//! is for. Both need `deserialize_any`.
//!
//! The format this replaced, postcard, has none of that: it writes no field
//! names, refuses `deserialize_any` outright, and — because the reader recovers
//! the shape from the type rather than the bytes — turns a type whose
//! `Serialize` and `Deserialize` disagree into wrong data rather than an error.
//! Three such bugs in this crate went undetected that way. Self-description
//! costs roughly two to three times the bytes on a small message; an coolabah
//! request is tens of bytes either way, sent at human speed, so that is not a
//! cost worth a class of silent bugs.

use async_mach_ports::{Error, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The coolabah wire format: `MessagePack`, with struct fields written as names.
///
/// `rmp_serde::to_vec` would write a struct as a bare positional array, which
/// is exactly as opaque as postcard was and would give up everything this
/// choice is for. `to_vec_named` is the whole point.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MessagePack;

impl async_mach_ports::Codec for MessagePack {
    fn encode<T: Serialize + ?Sized>(&self, value: &T) -> Result<Vec<u8>> {
        rmp_serde::to_vec_named(value).map_err(|_| Error::Encode)
    }

    fn decode<T: DeserializeOwned>(&self, bytes: &[u8]) -> Result<T> {
        rmp_serde::from_slice(bytes).map_err(|_| Error::Decode)
    }
}

/// The client end of the daemon's service.
pub type Sender<T> = async_mach_ports::Sender<T, MessagePack>;
/// The daemon end of its service, and the client end of an event subscription.
pub type Receiver<T> = async_mach_ports::Receiver<T, MessagePack>;
/// One value off the wire, with whatever channels the sender attached.
pub type Delivery<T> = async_mach_ports::Delivery<T, MessagePack>;
/// The one-shot answer to a request.
pub type Reply = async_mach_ports::Reply<MessagePack>;
/// A lasting channel to a client that asked for events.
pub type Subscriber = async_mach_ports::Subscriber<MessagePack>;

#[cfg(test)]
mod tests {
    use super::MessagePack;
    use async_mach_ports::Codec as _;
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;

    /// The reason for the format, asserted directly: postcard answers
    /// "this is a feature that `PostCard` will never implement" to both of
    /// these, because both need `deserialize_any`.
    #[test]
    fn the_declarative_serde_attributes_the_protocol_wants_actually_work() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        #[serde(from = "LabelRepr")]
        struct Label {
            string: String,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum LabelRepr {
            Text(String),
            Object { string: String },
        }

        impl From<LabelRepr> for Label {
            fn from(value: LabelRepr) -> Self {
                match value {
                    LabelRepr::Text(string) | LabelRepr::Object { string } => Self { string },
                }
            }
        }

        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Patch {
            label: Label,
            #[serde(flatten)]
            unknown: BTreeMap<String, u32>,
        }

        let patch = Patch {
            label: Label {
                string: "12:00".to_owned(),
            },
            unknown: BTreeMap::from([("y_offset".to_owned(), 2)]),
        };
        let bytes = MessagePack.encode(&patch).expect("encode");
        assert_eq!(MessagePack.decode::<Patch>(&bytes).expect("decode"), patch);
    }

    /// Field names on the wire, which is what `to_vec_named` buys and
    /// `to_vec` would not: a positional array does not read back as a map.
    #[test]
    fn struct_fields_travel_as_names() {
        #[derive(Serialize)]
        struct Two {
            first: u8,
            second: u8,
        }

        let bytes = MessagePack
            .encode(&Two {
                first: 1,
                second: 2,
            })
            .unwrap();
        let named: BTreeMap<String, u8> = MessagePack.decode(&bytes).expect("a map of field names");
        assert_eq!(named["first"], 1);
        assert_eq!(named["second"], 2);
    }
}

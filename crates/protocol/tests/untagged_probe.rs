//! Why the patch types may now spell their two accepted shapes with
//! `#[serde(untagged)]`, and could not before.
//!
//! The untagged representation-enum is the natural way to say "a bare string
//! or the whole object", and it reads far better than a hand-written visitor:
//!
//! ```ignore
//! #[derive(Deserialize)] #[serde(untagged)]
//! enum LabelRepr { Text(String), Object { .. } }
//! ```
//!
//! It used to be unusable on anything that crossed the IPC socket. `untagged`
//! buffers the input and re-reads it, which needs `deserialize_any`, and the
//! transport was postcard — not self-describing — which refuses that outright.
//! The patch types travel inside `Request::Set`, so they were exactly the types
//! that could not have it, and each one grew a visitor answering `visit_str`
//! and `visit_map` by hand instead.
//!
//! The wire is [`MessagePack`](coolabah_protocol::wire::MessagePack) now:
//! `to_vec_named` puts struct field names on the wire, so `deserialize_any`
//! has something to describe and both `untagged` and `flatten` work. This file
//! is kept as the standing proof of that, and of what it cost before, so that
//! a change back to a non-self-describing format fails here loudly rather than
//! in whichever patch type is converted next.

use async_mach_ports::Codec as _;
use coolabah_protocol::wire::MessagePack;
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "BobRepr")]
struct Bob {
    name: Option<String>,
    age: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum BobRepr {
    Name(String),
    Object {
        name: Option<String>,
        age: Option<u32>,
    },
}

impl From<BobRepr> for Bob {
    fn from(value: BobRepr) -> Self {
        match value {
            BobRepr::Name(name) => Self {
                name: Some(name),
                age: None,
            },
            BobRepr::Object { name, age } => Self { name, age },
        }
    }
}

#[test]
fn untagged_reads_both_shapes_on_a_self_describing_format() {
    let bob = Bob {
        name: Some("x".into()),
        age: Some(3),
    };
    let json = serde_json::to_string(&bob).unwrap();
    assert_eq!(serde_json::from_str::<Bob>(&json).unwrap(), bob);
    assert_eq!(
        serde_json::from_str::<Bob>("\"just-a-name\"").unwrap(),
        Bob {
            name: Some("just-a-name".into()),
            age: None
        },
    );
}

/// The one that matters: the same type, over the format that actually crosses
/// the socket. A patch type carrying `untagged` is now a thing that can be
/// sent, not only a thing that can be written.
#[test]
fn untagged_survives_the_wire_format_the_daemon_and_its_clients_use() {
    let bob = Bob {
        name: Some("x".into()),
        age: Some(3),
    };
    let bytes = MessagePack.encode(&bob).unwrap();
    assert_eq!(MessagePack.decode::<Bob>(&bytes).unwrap(), bob);
}

/// And what it looked like before, kept so the reason is not lost: postcard
/// serializes this happily and then cannot read a byte of it back.
#[test]
fn and_was_refused_by_postcard_which_is_why_the_wire_types_could_not_use_it() {
    let bob = Bob {
        name: Some("x".into()),
        age: Some(3),
    };
    let bytes = postcard::to_allocvec(&bob).expect("serializing is fine; only reading back is not");
    let err = postcard::from_bytes::<Bob>(&bytes).unwrap_err();
    assert!(
        err.to_string().contains("never implement"),
        "postcard refuses deserialize_any: {err}"
    );
}

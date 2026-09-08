//! A hand-written [`serde::Serializer`] that renders `SketchyBar`'s own
//! `--query` JSON shape and spellings, so a config parsing rsbar's output
//! (`menu_items[1]:query().geometry.drawing == "on"`) sees the same thing it
//! would from real `SketchyBar` — see `bar_item_serialize` in
//! `SketchyBar`'s own `bar_item.c` for the shape being matched.
//!
//! Two spellings differ from what a plain derived `Serialize` would produce,
//! and both are handled here rather than on the types themselves, since they
//! are properties of this output format, not of the data: every boolean is
//! the string `"on"`/`"off"`, never a JSON literal, and [`rsbar_protocol::Position`]
//! prints `SketchyBar`'s own single-letter abbreviations for the two
//! centre-adjacent variants (`"q"`, `"e"`) rather than its own Rust name.

use serde::Serialize;
use serde::ser::{
    self, SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant, SerializeTuple,
    SerializeTupleStruct, SerializeTupleVariant,
};
use std::fmt::{self, Write as _};

/// Renders `value` as `SketchyBar`-flavoured JSON.
///
/// Infallible: every type actually put through this (the wire state structs)
/// serializes without error, so there is no error path worth exposing to
/// callers.
#[must_use]
pub fn to_sketchybar_json<T: Serialize + ?Sized>(value: &T) -> String {
    let mut out = String::new();
    // The `Ok = ()` types below never fail: `Error` exists only because the
    // trait requires one.
    value
        .serialize(Writer { out: &mut out })
        .unwrap_or_else(|err: Error| match err {});
    out
}

/// An uninhabited error type: nothing this serializer does can fail.
#[derive(Debug)]
pub enum Error {}

impl fmt::Display for Error {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {}
    }
}

impl std::error::Error for Error {}

impl ser::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        // Reachable only if some future value's `Serialize` impl calls
        // `Error::custom` itself, which none of today's wire types do.
        panic!("sketchybar json serializer: {msg}")
    }
}

fn escape_into(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Writer<'a> {
    out: &'a mut String,
}

impl<'a> ser::Serializer for Writer<'a> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = Seq<'a>;
    type SerializeTuple = Seq<'a>;
    type SerializeTupleStruct = Seq<'a>;
    type SerializeTupleVariant = Seq<'a>;
    type SerializeMap = Map<'a>;
    type SerializeStruct = Struct<'a>;
    type SerializeStructVariant = Struct<'a>;

    fn serialize_bool(self, v: bool) -> Result<Self::Ok, Self::Error> {
        self.out.push_str(if v { "\"on\"" } else { "\"off\"" });
        Ok(())
    }

    fn serialize_i8(self, v: i8) -> Result<Self::Ok, Self::Error> {
        self.serialize_i64(i64::from(v))
    }
    fn serialize_i16(self, v: i16) -> Result<Self::Ok, Self::Error> {
        self.serialize_i64(i64::from(v))
    }
    fn serialize_i32(self, v: i32) -> Result<Self::Ok, Self::Error> {
        self.serialize_i64(i64::from(v))
    }
    fn serialize_i64(self, v: i64) -> Result<Self::Ok, Self::Error> {
        self.out.push_str(&v.to_string());
        Ok(())
    }
    fn serialize_u8(self, v: u8) -> Result<Self::Ok, Self::Error> {
        self.serialize_u64(u64::from(v))
    }
    fn serialize_u16(self, v: u16) -> Result<Self::Ok, Self::Error> {
        self.serialize_u64(u64::from(v))
    }
    fn serialize_u32(self, v: u32) -> Result<Self::Ok, Self::Error> {
        self.serialize_u64(u64::from(v))
    }
    fn serialize_u64(self, v: u64) -> Result<Self::Ok, Self::Error> {
        self.out.push_str(&v.to_string());
        Ok(())
    }
    fn serialize_f32(self, v: f32) -> Result<Self::Ok, Self::Error> {
        self.serialize_f64(f64::from(v))
    }
    fn serialize_f64(self, v: f64) -> Result<Self::Ok, Self::Error> {
        if v.is_finite() {
            self.out.push_str(&v.to_string());
        } else {
            self.out.push('0');
        }
        Ok(())
    }
    fn serialize_char(self, v: char) -> Result<Self::Ok, Self::Error> {
        let mut buf = [0u8; 4];
        self.serialize_str(v.encode_utf8(&mut buf))
    }
    fn serialize_str(self, v: &str) -> Result<Self::Ok, Self::Error> {
        escape_into(self.out, v);
        Ok(())
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<Self::Ok, Self::Error> {
        let seq = self.serialize_seq(Some(v.len()))?;
        let mut seq = seq;
        for byte in v {
            SerializeSeq::serialize_element(&mut seq, byte)?;
        }
        SerializeSeq::end(seq)
    }
    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        self.out.push_str("null");
        Ok(())
    }
    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<Self::Ok, Self::Error> {
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        self.out.push_str("null");
        Ok(())
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
        self.serialize_unit()
    }
    fn serialize_unit_variant(
        self,
        name: &'static str,
        variant_index: u32,
        variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        // `SketchyBar`'s own spelling for the two centre-adjacent positions
        // is a single letter, not the Rust variant name — see the `position`
        // switch in `bar_item_serialize` (bar_item.c).
        let spelling = if name == "Position" {
            match variant_index {
                0 => "left",
                1 => "q",
                2 => "center",
                3 => "e",
                4 => "right",
                _ => variant,
            }
        } else {
            variant
        };
        self.serialize_str(spelling)
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        self.out.push('{');
        escape_into(self.out, variant);
        self.out.push(':');
        value.serialize(Writer { out: self.out })?;
        self.out.push('}');
        Ok(())
    }
    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        self.out.push('[');
        Ok(Seq {
            out: self.out,
            first: true,
        })
    }
    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        self.out.push('{');
        escape_into(self.out, variant);
        self.out.push(':');
        self.serialize_seq(Some(len))
    }
    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        self.out.push('{');
        Ok(Map {
            out: self.out,
            first: true,
        })
    }
    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        self.out.push('{');
        Ok(Struct {
            out: self.out,
            first: true,
            closing: 1,
        })
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        self.out.push('{');
        escape_into(self.out, variant);
        self.out.push_str(":{");
        Ok(Struct {
            out: self.out,
            first: true,
            closing: 2,
        })
    }

    fn collect_str<T: fmt::Display + ?Sized>(self, value: &T) -> Result<Self::Ok, Self::Error> {
        escape_into(self.out, &value.to_string());
        Ok(())
    }
}

struct Seq<'a> {
    out: &'a mut String,
    first: bool,
}

fn comma(out: &mut String, first: &mut bool) {
    if *first {
        *first = false;
    } else {
        out.push(',');
    }
}

impl SerializeSeq for Seq<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
        comma(self.out, &mut self.first);
        value.serialize(Writer { out: self.out })
    }
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.out.push(']');
        Ok(())
    }
}

impl SerializeTuple for Seq<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
        SerializeSeq::serialize_element(self, value)
    }
    fn end(self) -> Result<Self::Ok, Self::Error> {
        SerializeSeq::end(self)
    }
}

impl SerializeTupleStruct for Seq<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
        SerializeSeq::serialize_element(self, value)
    }
    fn end(self) -> Result<Self::Ok, Self::Error> {
        SerializeSeq::end(self)
    }
}

impl SerializeTupleVariant for Seq<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
        SerializeSeq::serialize_element(self, value)
    }
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.out.push(']');
        self.out.push('}');
        Ok(())
    }
}

struct Map<'a> {
    out: &'a mut String,
    first: bool,
}

impl SerializeMap for Map<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Self::Error> {
        comma(self.out, &mut self.first);
        // Map keys in these wire types are always strings; `collect_str`
        // covers anything `Display`-shaped that isn't already `serialize_str`.
        key.serialize(Writer { out: self.out })
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.out.push(':');
        value.serialize(Writer { out: self.out })
    }
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.out.push('}');
        Ok(())
    }
}

struct Struct<'a> {
    out: &'a mut String,
    first: bool,
    /// How many closing `}` this struct owes: 1 for a plain struct, 2 for a
    /// struct variant (its own object nested inside `{"variant": {...}}`).
    closing: u8,
}

impl SerializeStruct for Struct<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Self::Error> {
        comma(self.out, &mut self.first);
        escape_into(self.out, key);
        self.out.push(':');
        value.serialize(Writer { out: self.out })
    }
    fn end(self) -> Result<Self::Ok, Self::Error> {
        for _ in 0..self.closing {
            self.out.push('}');
        }
        Ok(())
    }
}

impl SerializeStructVariant for Struct<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Self::Error> {
        SerializeStruct::serialize_field(self, key, value)
    }
    fn end(self) -> Result<Self::Ok, Self::Error> {
        SerializeStruct::end(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsbar_protocol::{
        Background, Geometry, ItemName, ItemState, Kind, Position, Run, Scripting,
    };
    use serde::Serialize;

    #[derive(Serialize)]
    struct Flag {
        on: bool,
        off: bool,
    }

    #[test]
    fn bools_render_as_on_off_strings() {
        let json = to_sketchybar_json(&Flag {
            on: true,
            off: false,
        });
        assert_eq!(json, r#"{"on":"on","off":"off"}"#);
    }

    #[test]
    fn position_uses_sketchybars_own_spellings() {
        for (position, expected) in [
            (Position::Left, "\"left\""),
            (Position::CenterLeft, "\"q\""),
            (Position::Center, "\"center\""),
            (Position::CenterRight, "\"e\""),
            (Position::Right, "\"right\""),
        ] {
            assert_eq!(to_sketchybar_json(&position), expected);
        }
    }

    #[derive(Serialize)]
    struct Maybe {
        width: Option<f64>,
    }

    #[test]
    fn a_none_renders_as_null() {
        assert_eq!(
            to_sketchybar_json(&Maybe { width: None }),
            r#"{"width":null}"#
        );
    }

    #[test]
    fn strings_escape_quotes_and_backslashes() {
        let json = to_sketchybar_json("a \"quote\" and a \\backslash\\");
        assert_eq!(json, r#""a \"quote\" and a \\backslash\\""#);
    }

    #[test]
    fn an_item_state_renders_geometry_and_style() {
        let state = ItemState {
            popup: rsbar_protocol::PopupState {
                drawing: false.into(),
                horizontal: false.into(),
                align: rsbar_protocol::PopupAlign::Left,
                topmost: true.into(),
                height: 0.0,
                y_offset: 0.0,
                background: rsbar_protocol::Background::default(),
            },
            name: ItemName::new("clock").unwrap(),
            geometry: Geometry {
                drawing: true.into(),
                position: Position::Right,
                y_offset: 0.0,
                padding_left: 2.0,
                padding_right: 2.0,
                width: None,
                display: rsbar_protocol::DisplayTarget::All,
                background: Background::default(),
            },
            icon: Run::default(),
            label: Run::default(),
            scripting: Scripting {
                script: None,
                click_script: None,
                update_freq: 0,
                updates: true.into(),
            },
            events: Vec::<Kind>::new(),
            alias: None,
            members: Vec::new(),
            associated_space: None,
            percentage: 0,
            knob: Run::default(),
            highlight_color: rsbar_protocol::Color::default(),
        };
        let json = to_sketchybar_json(&state);
        assert!(json.contains(r#""geometry":{"#));
        assert!(json.contains(r#""drawing":"on""#));
        assert!(json.contains(r#""color":"0x"#));
    }
}

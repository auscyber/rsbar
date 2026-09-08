//! A hand-written [`serde::Deserializer`] over `key=value` command-line
//! pairs, so a patch struct's own field types decide how each value is
//! parsed instead of a hand-written `match key { ... }` block that has to be
//! kept in sync with them by hand.
//!
//! The key in each pair is a dotted path (`label.color`, `background.height`)
//! into the target struct. No intermediate value tree is built: a scalar's
//! Rust type (`f64` vs `bool` vs `String` vs an enum) is only known once
//! serde's derived `Deserialize` impl asks for it, so the coercion happens
//! directly against the string that arrived in argv.

use serde::Deserialize;
use serde::de::{
    self, DeserializeSeed, Deserializer, EnumAccess, IntoDeserializer, MapAccess, SeqAccess,
    VariantAccess, Visitor,
};
use std::fmt;

/// A dotted-path deserialization failure, naming the key it happened at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgsError {
    /// The dotted path so far, e.g. `label.color`. Empty at the top level.
    path: String,
    message: String,
}

impl ArgsError {
    fn at(path: &str, message: impl Into<String>) -> Self {
        Self {
            path: path.to_owned(),
            message: message.into(),
        }
    }

    /// A leaf type's own `Deserialize` (`Color`, `FontSpec`, `Selector`) can
    /// only reach `serde::de::Error::custom`, a bare associated function with
    /// no path to attach — so an error surfacing from one of those arrives
    /// path-less. Whichever `MapAccess`/`SeqAccess` called into it knows the
    /// path it dispatched to, so it fills this in on the way back out rather
    /// than losing which key was actually invalid.
    fn with_path_if_empty(self, path: &str) -> Self {
        if self.path.is_empty() {
            Self {
                path: path.to_owned(),
                ..self
            }
        } else {
            self
        }
    }
}

impl fmt::Display for ArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(f, "`{}`: {}", self.path, self.message)
        }
    }
}

impl std::error::Error for ArgsError {}

impl de::Error for ArgsError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self::at("", msg.to_string())
    }
}

/// Deserializes `T` from a flat list of dotted-path `(key, value)` pairs,
/// e.g. `[("label.color", "0xffffffff"), ("drawing", "off")]`.
///
/// Every key must already be exactly the dotted path into `T`: bare-key
/// sugar (`icon=` meaning `icon.text=`) is the caller's job, not this
/// function's.
///
/// # Errors
///
/// Returns [`ArgsError`] naming the offending dotted key.
pub fn from_pairs<'de, T: Deserialize<'de>>(
    pairs: &'de [(String, String)],
) -> Result<T, ArgsError> {
    let refs: Vec<(&'de str, &'de str)> = pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    T::deserialize(GroupDeserializer {
        pairs: refs,
        path: String::new(),
    })
}

/// One segment of a dotted key, split at the first `.`.
fn split_first(key: &str) -> (&str, Option<&str>) {
    match key.split_once('.') {
        Some((head, rest)) => (head, Some(rest)),
        None => (key, None),
    }
}

fn join_path(path: &str, segment: &str) -> String {
    if path.is_empty() {
        segment.to_owned()
    } else {
        format!("{path}.{segment}")
    }
}

/// A group of pairs sharing a common prefix already stripped off — the
/// top-level call has an empty prefix, a nested `icon.*` group has already
/// had `icon.` removed from every key. Deserializes as a map/struct: each
/// distinct first remaining segment is one field.
///
/// `pub(crate)` so [`super::grammar`]'s own argv deserializer can hand a
/// domain's trailing `key=value` tokens straight to this, the same way
/// [`from_pairs`] does, without going through a function that expects to
/// fully deserialize `T` itself rather than drive a borrowed `Visitor`.
pub(crate) struct GroupDeserializer<'de> {
    /// Owned so a nested group can be handed down without leaking or
    /// borrowing from a temporary: the strings inside still borrow from the
    /// original `'de` input, only the `Vec` itself is fresh.
    pub(crate) pairs: Vec<(&'de str, &'de str)>,
    /// The dotted path to this group, for error messages.
    pub(crate) path: String,
}

/// One first-segment group: either a leaf (a single pair whose key equals
/// the segment) or a nested set of pairs still carrying a `.` in their key.
enum Member<'de> {
    Leaf(&'de str),
    Nested(Vec<(&'de str, &'de str)>),
}

/// The members of one group, in the order argv named them.
///
/// A head that arrives both ways — `label=12:34 label.color=0xff00ff00` — is
/// yielded **twice**, bare form first. That is not a contradiction to be
/// rejected: it is the flat spelling of one table, and it is what every real
/// `SketchyBar` config writes. The patch folds the two into one value, and
/// which field a bare string lands in is the patch type's own business
/// (`#[changes(scalar = ...)]`) rather than something this reader has to
/// know — so a type with no bare form, like a background, still fails, and
/// fails saying so.
///
/// What is still a contradiction, and still an error: the same key given two
/// different values in one command.
fn group_members<'de>(
    pairs: &[(&'de str, &'de str)],
    path: &str,
) -> Result<Vec<(&'de str, Member<'de>)>, ArgsError> {
    let mut order: Vec<&'de str> = Vec::new();
    let mut leaves: std::collections::HashMap<&'de str, &'de str> =
        std::collections::HashMap::new();
    let mut nested: std::collections::HashMap<&'de str, Vec<(&'de str, &'de str)>> =
        std::collections::HashMap::new();

    for &(key, value) in pairs {
        let (head, rest) = split_first(key);
        if !leaves.contains_key(head) && !nested.contains_key(head) {
            order.push(head);
        }
        match rest {
            None => {
                if let Some(previous) = leaves.insert(head, value)
                    && previous != value
                {
                    return Err(ArgsError::at(
                        &join_path(path, head),
                        format!("set twice in one command, to `{previous}` and to `{value}`"),
                    ));
                }
            }
            Some(rest) => {
                nested.entry(head).or_default().push((rest, value));
            }
        }
    }

    let mut result = Vec::with_capacity(order.len());
    for head in order {
        // Bare form first, so an explicit nested key for the same property
        // is the later of the two and wins.
        if let Some(value) = leaves.get(head) {
            result.push((head, Member::Leaf(value)));
        }
        if let Some(rest) = nested.remove(head) {
            result.push((head, Member::Nested(rest)));
        }
    }
    Ok(result)
}

impl<'de> Deserializer<'de> for GroupDeserializer<'de> {
    type Error = ArgsError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_map(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        let members = group_members(&self.pairs, &self.path)?;
        visitor.visit_map(GroupMapAccess {
            members: members.into_iter(),
            path: self.path,
            current: None,
        })
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_map(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_some(self)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf unit unit_struct newtype_struct seq tuple
        tuple_struct enum identifier ignored_any
    }
}

struct GroupMapAccess<'de> {
    members: std::vec::IntoIter<(&'de str, Member<'de>)>,
    path: String,
    /// The full dotted path to the member [`Self::next_value_seed`] is about
    /// to be asked for, computed when its key was yielded.
    current: Option<(String, Member<'de>)>,
}

impl<'de> MapAccess<'de> for GroupMapAccess<'de> {
    type Error = ArgsError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        match self.members.next() {
            None => Ok(None),
            Some((key, member)) => {
                self.current = Some((join_path(&self.path, key), member));
                seed.deserialize(KeyDeserializer(key)).map(Some)
            }
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        match self.current.take() {
            Some((path, Member::Leaf(value))) => seed
                .deserialize(ScalarDeserializer {
                    value,
                    path: path.clone(),
                })
                .map_err(|e| e.with_path_if_empty(&path)),
            Some((path, Member::Nested(rest))) => seed
                .deserialize(GroupDeserializer {
                    pairs: rest,
                    path: path.clone(),
                })
                .map_err(|e| e.with_path_if_empty(&path)),
            None => Err(ArgsError::at(&self.path, "value requested before key")),
        }
    }
}

/// Hands a map/struct key's raw text straight to serde's derived field
/// matcher, so `#[serde(alias = "...")]` resolution "just works" without any
/// special case here.
struct KeyDeserializer<'de>(&'de str);

impl<'de> Deserializer<'de> for KeyDeserializer<'de> {
    type Error = ArgsError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_borrowed_str(self.0)
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_borrowed_str(self.0)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum ignored_any
    }
}

/// One leaf value string. Its target type — asked for via whichever
/// `deserialize_*` method serde's derived impl calls — decides how the
/// string is read; this deserializer never guesses.
struct ScalarDeserializer<'de> {
    value: &'de str,
    /// The dotted path to this value, for error messages (does not include
    /// the leaf's own segment name, which the caller already folded in via
    /// [`join_path`] when constructing this — see call sites).
    path: String,
}

impl ScalarDeserializer<'_> {
    fn err(&self, message: impl Into<String>) -> ArgsError {
        ArgsError::at(&self.path, message)
    }

    /// `SketchyBar`'s own boolean spellings, `!`-negatable
    /// (`ARGUMENT_COMMON_VAL_*` in `defines.h`).
    fn parse_bool(&self) -> Result<bool, ArgsError> {
        match self.value {
            "on" | "!off" | "true" | "!false" | "1" | "!0" | "yes" | "!no" => Ok(true),
            "off" | "!on" | "false" | "!true" | "0" | "!1" | "no" | "!yes" => Ok(false),
            other => Err(self.err(format!(
                "`{other}` is not on/off, true/false, yes/no or 1/0 (optionally `!`-negated)"
            ))),
        }
    }

    fn parse_num<T: std::str::FromStr>(&self) -> Result<T, ArgsError>
    where
        T::Err: fmt::Display,
    {
        self.value
            .parse()
            .map_err(|e| self.err(format!("`{}` is not a number: {e}", self.value)))
    }
}

impl<'de> Deserializer<'de> for ScalarDeserializer<'de> {
    type Error = ArgsError;

    /// A value out of argv is a string and nothing else, so that is what a
    /// type asking "what have you got" is told. It matters for the types that
    /// take more than one spelling — a [`Color`](rsbar_protocol::Color) is a
    /// string here and a number in a Lua config, and asks `any` rather than
    /// declaring which — and it is what `#[serde(untagged)]` needs to work
    /// through this deserializer at all.
    ///
    /// A type that cannot be a string still fails, just one step later and
    /// with its own message: "invalid type: string" from the visitor that
    /// wanted something else, rather than a blanket refusal from here.
    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_borrowed_str(self.value)
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_bool(self.parse_bool()?)
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_f64(self.parse_num()?)
    }

    fn deserialize_u32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_u32(self.parse_num()?)
    }

    fn deserialize_i32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_i32(self.parse_num()?)
    }

    // A slider's percentage is a `u8`, and a graph sample an `f32`. Without
    // these they fell through to `deserialize_any`, which reports that the
    // shape is unsupported -- a confusing answer to `percentage=60`.
    fn deserialize_u8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_u8(self.parse_num()?)
    }

    fn deserialize_u16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_u16(self.parse_num()?)
    }

    fn deserialize_u64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_u64(self.parse_num()?)
    }

    fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_i64(self.parse_num()?)
    }

    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_f32(self.parse_num()?)
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_borrowed_str(self.value)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_borrowed_str(self.value)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        if !variants.contains(&self.value) {
            return Err(self.err(format!(
                "`{}` is not one of: {}",
                self.value,
                variants.join(", ")
            )));
        }
        visitor.visit_enum(UnitVariantAccess { value: self.value })
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        let path = self.path.clone();
        let items: Vec<&'de str> = if self.value.is_empty() {
            Vec::new()
        } else {
            self.value.split(',').map(str::trim).collect()
        };
        visitor.visit_seq(ScalarSeqAccess {
            items: items.into_iter(),
            path,
        })
    }

    serde::forward_to_deserialize_any! {
        i8 i16 i128 u128 char bytes byte_buf unit
        unit_struct tuple tuple_struct map struct identifier ignored_any
    }
}

struct ScalarSeqAccess<'de> {
    items: std::vec::IntoIter<&'de str>,
    path: String,
}

impl<'de> SeqAccess<'de> for ScalarSeqAccess<'de> {
    type Error = ArgsError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        match self.items.next() {
            None => Ok(None),
            Some(value) => seed
                .deserialize(ScalarDeserializer {
                    value,
                    path: self.path.clone(),
                })
                .map_err(|e| e.with_path_if_empty(&self.path))
                .map(Some),
        }
    }
}

/// A unit-variant-only `EnumAccess`/`VariantAccess`: the whole leaf string
/// already matched one of the target enum's variant names by the time this
/// is constructed.
struct UnitVariantAccess<'de> {
    value: &'de str,
}

impl<'de> EnumAccess<'de> for UnitVariantAccess<'de> {
    type Error = ArgsError;
    type Variant = Self;

    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant), Self::Error> {
        let value = seed.deserialize(self.value.into_deserializer())?;
        Ok((value, self))
    }
}

impl<'de> VariantAccess<'de> for UnitVariantAccess<'de> {
    type Error = ArgsError;

    fn unit_variant(self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(
        self,
        _seed: T,
    ) -> Result<T::Value, Self::Error> {
        Err(de::Error::custom(format!(
            "`{}` is not a value-carrying property",
            self.value
        )))
    }

    fn tuple_variant<V: Visitor<'de>>(
        self,
        _len: usize,
        _visitor: V,
    ) -> Result<V::Value, Self::Error> {
        Err(de::Error::custom(format!(
            "`{}` is not a value-carrying property",
            self.value
        )))
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, Self::Error> {
        Err(de::Error::custom(format!(
            "`{}` is not a value-carrying property",
            self.value
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsbar_protocol::{
        BackgroundPatch, Color, FontSpec, ItemName, ItemPatch, Position, RunPatch, Selector,
    };

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn a_flat_patch_deserializes() {
        let p = pairs(&[("padding_left", "4"), ("padding_right", "6")]);
        let patch: RunPatch = from_pairs(&p).unwrap();
        assert_eq!(
            patch,
            RunPatch {
                padding_left: Some(4.0),
                padding_right: Some(6.0),
                ..Default::default()
            }
        );
    }

    #[test]
    fn a_dotted_patch_nests() {
        let p = pairs(&[
            ("icon.color", "0xff112233"),
            ("label.font", "Hack:Bold:14"),
            ("background.corner_radius", "6"),
        ]);
        let patch: ItemPatch = from_pairs(&p).unwrap();
        assert_eq!(patch.icon.unwrap().color, Some(Color(0xff11_2233)));
        assert_eq!(
            patch.label.unwrap().font,
            Some(FontSpec::parse("Hack:Bold:14"))
        );
        assert_eq!(
            patch
                .geometry
                .and_then(|geometry| geometry.background)
                .and_then(|background| background.corner_radius),
            Some(6.0)
        );
    }

    #[test]
    fn every_bool_spelling_is_accepted() {
        for (text, expected) in [
            ("on", true),
            ("!off", true),
            ("true", true),
            ("!false", true),
            ("1", true),
            ("!0", true),
            ("yes", true),
            ("!no", true),
            ("off", false),
            ("!on", false),
            ("false", false),
            ("!true", false),
            ("0", false),
            ("!1", false),
            ("no", false),
            ("!yes", false),
        ] {
            let p = pairs(&[("drawing", text)]);
            let patch: RunPatch = from_pairs(&p).unwrap();
            assert_eq!(
                patch.drawing,
                Some(if expected {
                    rsbar_protocol::BoolChange::True
                } else {
                    rsbar_protocol::BoolChange::False
                }),
                "for {text}"
            );
        }
    }

    #[test]
    fn a_bad_bool_names_the_key() {
        let p = pairs(&[("drawing", "sometimes")]);
        let err = from_pairs::<RunPatch>(&p).unwrap_err();
        assert_eq!(err.path, "drawing");
    }

    #[test]
    fn toggle_is_a_boolean_spelling_now_that_the_daemon_resolves_it() {
        // The field is `BoolChange`: the CLI carries the intent, the daemon
        // (which knows the current value) resolves `toggle`.
        let p = pairs(&[("drawing", "toggle")]);
        let patch: RunPatch = from_pairs(&p).unwrap();
        assert_eq!(patch.drawing, Some(rsbar_protocol::BoolChange::Toggle));
    }

    #[test]
    fn numbers_parse_including_negative_floats() {
        let p = pairs(&[("padding_left", "-5.5")]);
        let patch: RunPatch = from_pairs(&p).unwrap();
        assert_eq!(patch.padding_left, Some(-5.5));
    }

    #[test]
    fn an_unknown_field_is_named_and_skipped_rather_than_rejected() {
        // The patch's own `Deserialize` warns and carries on -- see
        // `rsbar_protocol::patch`. What reaches here is a patch with the
        // properties that were understood and nothing for the one that was
        // not.
        let p = pairs(&[("wat", "1"), ("padding_left", "4")]);
        let patch: RunPatch = from_pairs(&p).unwrap();
        assert_eq!(patch.padding_left, Some(4.0));
        assert_eq!(patch.text, None);
    }

    #[test]
    fn position_parses_through_the_enum_path() {
        let p = pairs(&[("position", "left")]);
        let patch: ItemPatch = from_pairs(&p).unwrap();
        assert_eq!(
            patch.geometry.and_then(|geometry| geometry.position),
            Some(Position::Left)
        );

        let p = pairs(&[("position", "nope")]);
        assert!(from_pairs::<ItemPatch>(&p).is_err());
    }

    #[test]
    fn members_split_on_commas_into_selectors() {
        let p = pairs(&[("members", r"a,b,/menu\..*/")]);
        let patch: ItemPatch = from_pairs(&p).unwrap();
        assert_eq!(
            patch.members,
            Some(vec![
                Selector::Name(ItemName::new("a").unwrap()),
                Selector::Name(ItemName::new("b").unwrap()),
                Selector::Pattern(r"menu\..*".into()),
            ])
        );
    }

    #[test]
    fn a_bare_value_and_its_nested_properties_are_one_table() {
        // `--set clock label=12:34 label.color=...` is what every real
        // SketchyBar config writes: the flat spelling of
        // `label = { string = "12:34", color = ... }`, not a contradiction.
        let p = pairs(&[
            ("label", "12:34"),
            ("icon", "T"),
            ("label.color", "0xff00ff00"),
        ]);
        let patch: ItemPatch = from_pairs(&p).unwrap();
        let label = patch.label.expect("label was set");
        assert_eq!(label.text.as_deref(), Some("12:34"));
        assert_eq!(label.color, Some(Color(0xff00_ff00)));
        assert_eq!(patch.icon.and_then(|icon| icon.text).as_deref(), Some("T"));
    }

    #[test]
    fn the_same_property_set_twice_to_different_values_is_still_an_error() {
        let p = pairs(&[("label", "a"), ("label", "b")]);
        let err = from_pairs::<ItemPatch>(&p).unwrap_err();
        assert_eq!(err.path, "label");
        assert!(err.message.contains("set twice"), "{err}");
        // The same value twice says nothing new and is not a contradiction.
        let p = pairs(&[("label", "a"), ("label", "a")]);
        assert!(from_pairs::<ItemPatch>(&p).is_ok());
    }

    #[test]
    fn a_bare_value_for_a_table_with_no_bare_form_still_fails() {
        // A background is not a value: there is no `#[changes(scalar = ...)]`
        // on it, so nothing says what `background=x` would mean.
        let p = pairs(&[("background", "x"), ("background.height", "20")]);
        let err = from_pairs::<ItemPatch>(&p).unwrap_err();
        assert!(err.to_string().contains("background"), "{err}");
    }

    #[test]
    fn a_colour_is_spelled_either_way_from_argv_too() {
        // The alias is the patch type's, so it works through whichever door
        // reaches the same `Deserialize`.
        let p = pairs(&[("label.colour", "0xffaa00ff")]);
        let patch: ItemPatch = from_pairs(&p).unwrap();
        assert_eq!(
            patch.label.and_then(|label| label.color),
            Some(Color(0xffaa_00ff))
        );
    }

    #[test]
    fn a_flattened_property_needs_no_prefix_out_of_argv() {
        // `padding_left` lives on `Geometry` and `script` on `Scripting`, and
        // a config writes neither prefix. It also proves nothing buffers: a
        // buffered `f64` handed the string `4` would fail here.
        let p = pairs(&[("padding_left", "4"), ("script", "s"), ("update_freq", "5")]);
        let patch: ItemPatch = from_pairs(&p).unwrap();
        assert_eq!(
            patch.geometry.and_then(|geometry| geometry.padding_left),
            Some(4.0)
        );
        let scripting = patch.scripting.expect("scripting keys landed");
        assert_eq!(scripting.script.as_deref(), Some("s"));
        assert_eq!(scripting.update_freq, Some(5));
    }

    #[test]
    fn color_and_font_round_trip_their_real_spellings() {
        let p = pairs(&[("color", "0xffaa00ff")]);
        let patch: BackgroundPatch = from_pairs(&p).unwrap();
        assert_eq!(patch.color, Some(Color(0xffaa_00ff)));

        let p = pairs(&[("color", "0xffaa00ff"), ("font", "Hack:Bold:14")]);
        let patch: RunPatch = from_pairs(&p).unwrap();
        assert_eq!(patch.color, Some(Color(0xffaa_00ff)));
        assert_eq!(patch.font, Some(FontSpec::parse("Hack:Bold:14")));
    }
}

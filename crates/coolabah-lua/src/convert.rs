//! Lua tables on one side, `coolabah-protocol` types on the other.
//!
//! Almost nothing here reads a field: a patch is deserialized straight out of
//! the options table by `mlua`'s own serde bridge, so the *target field's*
//! own type says how each value is read and a new property on [`ItemPatch`]
//! needs no change at all on this side. What is left is the handful of
//! conversions that are not a patch — an event's name, a trigger's variables,
//! `coolabah.default`'s merge — and the other direction, where a query result is
//! already `Serialize` in exactly the shape a config should see.
//!
//! There used to be a second serde `Deserializer` here, hand-written over
//! `mlua::Value`, because the protocol types could not read what a config
//! actually writes: `drawing = true` where they wanted `"on"`, a colour as a
//! number, `label = "12:00"` for a whole `RunPatch`. Every one of those is now
//! the *type's* own `Deserialize` — one spelling for the CLI, this module and
//! the wire alike — so what is left here is `lua.from_value`.

use std::str::FromStr;

use mlua::{IntoLua, Lua, LuaSerdeExt, Table, Value};
use serde::Serialize;

use crate::protocol::{
    BarPatch, BarState, Event, EventName, ItemName, ItemPatch, ItemState, Kind, NotificationName,
    Position, Selector,
};

use crate::error::{ApiError, Result};

/// # Errors
///
/// Returns [`ApiError::InvalidPosition`] if `s` is not a recognised position.
pub fn position_from_str(s: &str) -> Result<Position> {
    Position::from_str(s).map_err(|e| ApiError::InvalidPosition(s.to_string(), e))
}

/// # Errors
///
/// Returns [`ApiError::InvalidEvent`] if `s` is not a valid event name.
pub fn kind_from_str(s: &str) -> Result<Kind> {
    Kind::from_str(s).map_err(|e| ApiError::InvalidEvent(s.to_string(), e))
}

/// # Errors
///
/// Returns [`ApiError::InvalidName`] if `s` is not a valid item name.
pub fn item_name_from_str(s: &str) -> Result<ItemName> {
    ItemName::new(s).map_err(|e| ApiError::InvalidName(s.to_string(), e))
}

/// The name of an event a config declares with `add("event", ...)`.
///
/// Stricter than [`kind_from_str`] on purpose: a built-in is already defined,
/// so naming one here is a mistake rather than a redeclaration.
///
/// # Errors
///
/// Returns [`ApiError::InvalidEventName`] if `s` is not a name a config may
/// add.
pub fn event_name_from_str(s: &str) -> Result<EventName> {
    EventName::from_str(s).map_err(|e| ApiError::InvalidEventName(s.to_string(), e))
}

/// The `NSDistributedNotificationCenter` name an event is bridged from.
///
/// # Errors
///
/// Returns [`ApiError::InvalidNotificationName`] if `s` is empty or carries
/// control characters.
pub fn notification_name_from_str(s: &str) -> Result<NotificationName> {
    NotificationName::from_str(s).map_err(|e| ApiError::InvalidNotificationName(s.to_string(), e))
}

/// `"volume_changed"`, or `{ "volume_changed", "brightness_changed" }`.
///
/// # Errors
///
/// Returns a Lua error if `value` is neither shape, or names an unknown event.
pub fn kinds_from_value(value: &Value) -> mlua::Result<Vec<Kind>> {
    match value {
        Value::String(s) => Ok(vec![kind_from_str(&s.to_str()?)?]),
        Value::Table(names) => names
            .clone()
            .sequence_values::<mlua::LuaString>()
            .map(|entry| Ok(kind_from_str(&entry?.to_str()?)?))
            .collect(),
        other => Err(mlua::Error::RuntimeError(format!(
            "expected an event name or a table of names, got {}",
            other.type_name()
        ))),
    }
}

/// `name`, as a [`Selector`] — an exact name, or `SketchyBar`'s own
/// `/menu\..*/` shorthand for "every item whose name matches this regex".
/// Never compiled or matched here: only the daemon has a live item list, so
/// only it can resolve a [`Selector::Pattern`] without racing a config that
/// is still adding items — see [`Selector`]'s own doc comment.
///
/// # Errors
///
/// Returns [`ApiError::InvalidName`] if `name` is shaped as a literal name
/// (not `/.../`) but is not one — empty, say.
pub fn selector_from_name(name: &str) -> Result<Selector> {
    Selector::from_str(name).map_err(|e| ApiError::InvalidName(name.to_string(), e))
}

/// `sbar.trigger("demo", { VAR = "Test" })` -- the variables a custom event
/// carries, which reach a script as `$VAR`.
///
/// Values are stringified because that is what an environment variable is; a
/// table for a value is an error rather than a silently JSON-encoded blob,
/// since nothing on the other side would unpack it.
///
/// # Errors
///
/// Returns a Lua error if `value` is not a table, or if any value in it is
/// not a string, number or boolean.
pub fn vars_from_value(value: &Value) -> mlua::Result<std::collections::BTreeMap<String, String>> {
    let Value::Table(table) = value else {
        return Err(mlua::Error::RuntimeError(format!(
            "a trigger's variables must be a table, got {}",
            value.type_name()
        )));
    };
    let mut vars = std::collections::BTreeMap::new();
    for pair in table.clone().pairs::<String, Value>() {
        let (key, value) = pair?;
        let text = match value {
            Value::String(s) => s.to_str()?.to_string(),
            Value::Integer(i) => i.to_string(),
            Value::Number(n) => n.to_string(),
            Value::Boolean(b) => on_off(b).to_owned(),
            other => {
                return Err(mlua::Error::RuntimeError(format!(
                    "`{key}` must be a string, number or boolean, got {}",
                    other.type_name()
                )));
            }
        };
        vars.insert(key, text);
    }
    Ok(vars)
}

/// Merges `overlay` onto `base`, recursively wherever both sides have a table
/// at the same key, so `coolabah.default`'s `icon = { font = { family = ... } }`
/// combines with an item's own `icon = { font = { size = ... } }` rather than
/// one replacing the other outright. Neither input is mutated; the result is
/// a fresh table naming exactly the keys either side actually set — nothing
/// is invented, which is what keeps a merged patch as narrow as the item's
/// own table would have produced alone.
///
/// # Errors
///
/// Returns a Lua error only if table creation or iteration itself fails.
pub fn deep_merge(lua: &Lua, base: &Table, overlay: &Table) -> mlua::Result<Table> {
    let result = lua.create_table()?;
    for pair in base.clone().pairs::<Value, Value>() {
        let (key, value) = pair?;
        result.set(key, value)?;
    }
    for pair in overlay.clone().pairs::<Value, Value>() {
        let (key, value) = pair?;
        let merged = match (result.get::<Value>(key.clone())?, &value) {
            (Value::Table(existing), Value::Table(new)) => {
                Value::Table(deep_merge(lua, &existing, new)?)
            }
            _ => value,
        };
        result.set(key, merged)?;
    }
    Ok(result)
}

/// The `position` an add options table carries, or [`Position::default`] if
/// it names none — `coolabah.add` reads position out of the options table rather
/// than as a separate positional argument, matching how a real `SketchyBar`
/// config always writes it (`{ position = "left", ... }`). Also accepts
/// `"popup.<owner item>"`, which is a popup anchor rather than a bar bucket.
///
/// Read here as well as through the patch because it is needed *before* the
/// patch: it is where the `Request::Add` puts the item.
///
/// # Errors
///
/// Returns a Lua error if `position` is set but not a recognised spelling.
pub fn item_position_from_table(table: &Table) -> mlua::Result<Position> {
    match table.get::<Option<String>>("position")? {
        Some(text) => Ok(position_from_str(&text)?),
        // `Position` deliberately has no `Default` -- an `--add` with no
        // position has nowhere to put the item -- but a Lua `add` with no
        // `position` key is the shape SbarLua has always allowed, and the
        // bucket it means is the left one.
        None => Ok(Position::Left),
    }
}

/// `coolabah.bar({...})`.
///
/// # Errors
///
/// Returns a Lua error if a value cannot be read as the field it was written
/// for — a colour that is neither a number nor `#rrggbb`, a `drawing` that is
/// neither on nor off. An unrecognised *key* is named through `tracing` and
/// dropped rather than raised, by the patch type itself: see
/// [`crate::protocol::patch`].
pub fn bar_patch_from_table(table: &Table) -> mlua::Result<BarPatch> {
    patch_from_table(table)
}

/// `coolabah.add(kind, name, {...})` / `item:set({...})`.
///
/// # Errors
///
/// Same as [`bar_patch_from_table`].
pub fn item_patch_from_table(table: &Table) -> mlua::Result<ItemPatch> {
    patch_from_table(table)
}

/// `mlua`'s own serde bridge, with no reading of our own on top of it.
///
/// Every spelling a config uses that a derived `Deserialize` would refuse —
/// `drawing = true`, `color = 0xff0000ff`, `label = "12:00"`, a font written
/// as a table — is answered by the target type, so this is the whole of the
/// Lua side. `Deserializer::new` rather than `Lua::from_value` only because a
/// `Table` already knows which interpreter it belongs to and this way the
/// callers do not have to.
///
/// The one thing wrapped around it is [`serde_path_to_error`], which watches
/// which key the reading was on and tells nobody how to read anything. A leaf
/// type reports what a value should have been — that is its business, and it
/// is the same sentence whichever front end asked — but it cannot know it was
/// being read for `popup.drawing`, because it is never told. Without this a
/// config author is left with a message naming only the value, which is the
/// half they can already see.
fn patch_from_table<T: serde::de::DeserializeOwned>(table: &Table) -> mlua::Result<T> {
    let table = mlua::serde::Deserializer::new(Value::Table(table.clone()));
    serde_path_to_error::deserialize(table).map_err(|error| {
        let path = error.path().to_string();
        // Unwrapped rather than printed: the inner error is already an
        // `mlua::Error::DeserializeError`, and re-wrapping its `Display`
        // would say "deserialize error" twice.
        let message = match error.into_inner() {
            mlua::Error::DeserializeError(message) => message,
            other => other.to_string(),
        };
        mlua::Error::DeserializeError(if path.is_empty() || path == "." {
            message
        } else {
            // `` `popup.drawing`: <what the value should have been> `` --
            // the same shape the CLI's own `ArgsError` prints, so one
            // mistake reads the same whichever door a config came through.
            format!("`{path}`: {message}")
        })
    })
}

/// One property assigned through an item's metatable — `item.popup.drawing =
/// true` — as the table `item:set{ popup = { drawing = true } }` would have
/// passed, then read like any other.
///
/// The nesting is rebuilt rather than deserialized from the path directly
/// because a dotted path and the nested table it is sugar for deserve one
/// reading, not two — and that one already exists.
///
/// # Errors
///
/// Returns whatever [`item_patch_from_table`] would for the same table.
pub fn item_patch_at<S: AsRef<str>>(
    lua: &Lua,
    path: &[S],
    key: &str,
    value: Value,
) -> mlua::Result<ItemPatch> {
    let mut nested = lua.create_table()?;
    nested.set(key, value)?;
    for group in path.iter().rev() {
        let outer = lua.create_table()?;
        outer.set(group.as_ref(), nested)?;
        nested = outer;
    }
    item_patch_from_table(&nested)
}

/// A `Serialize` value as the `Table` a query result hands back to a config,
/// via `mlua`'s `serde` support rather than a hand-written mirror of the same
/// field list: [`ItemState`]/[`BarState`] and everything they nest already
/// serialize in exactly the shape a config should see — `Color` as `"0x..."`,
/// `Position`/`Edge` `snake_case`, `FontSpec` as `Family:Style:Size`, a
/// `Boolish` flag as `"on"`/`"off"` rather than a Lua boolean — so
/// building the table by hand here would just be a second copy of that same
/// shape, free to drift from it.
///
/// # Errors
///
/// Returns a Lua error if `value` does not serialize as a table at all — it
/// always should for every type this is called with.
fn serde_table<T: Serialize>(lua: &Lua, value: &T) -> mlua::Result<Table> {
    match lua.to_value(value)? {
        Value::Table(table) => Ok(table),
        other => Err(mlua::Error::RuntimeError(format!(
            "expected a table, serialized as {}",
            other.type_name()
        ))),
    }
}

/// `SketchyBar`'s own spelling for a flag, which is what a config compares
/// against. Only [`vars_from_value`] needs it by hand now: every flag on a
/// state type is a `Boolish`, and that spells itself.
fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

/// # Errors
///
/// Returns a Lua error only if table creation itself fails.
pub fn item_state_to_table(lua: &Lua, state: &ItemState) -> mlua::Result<Table> {
    serde_table(lua, state)
}

/// # Errors
///
/// Returns a Lua error only if table creation itself fails.
pub fn bar_state_to_table(lua: &Lua, state: &BarState) -> mlua::Result<Table> {
    serde_table(lua, state)
}

/// A field value as a script should see it: numbers stay numbers, everything
/// else is a string. `Event::fields()` renders every field with `Display`, so
/// this is the one place that tries to undo that for Lua's benefit.
fn scalar(lua: &Lua, text: &str) -> mlua::Result<Value> {
    if let Ok(i) = text.parse::<i64>() {
        return i.into_lua(lua);
    }
    if let Ok(f) = text.parse::<f64>() {
        return f.into_lua(lua);
    }
    text.into_lua(lua)
}

/// What a subscribed callback is handed: `event` (the kind that fired) plus
/// every field the built-in event carries, or `data` for a custom one.
///
/// # Errors
///
/// Returns a Lua error only if table creation itself fails.
pub fn event_to_table(lua: &Lua, item: &ItemName, event: &Event) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("item", item.as_str())?;
    table.set("event", event.kind().name())?;
    for (key, value) in event.fields() {
        table.set(key, scalar(lua, &value)?)?;
    }
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Boolish, Color, Edge};

    #[test]
    fn a_plain_name_is_a_selector_name() {
        assert_eq!(
            selector_from_name("front_app").unwrap(),
            Selector::Name(item_name_from_str("front_app").unwrap())
        );
    }

    #[test]
    fn a_slash_wrapped_name_is_a_selector_pattern() {
        assert_eq!(
            selector_from_name("/menu\\..*/").unwrap(),
            Selector::Pattern("menu\\..*".to_string())
        );
    }

    #[test]
    fn a_malformed_regex_still_parses_as_a_pattern() {
        // Not compiled here any more — only the daemon has a live item list
        // to resolve one against, so it is the one that rejects a bad regex.
        assert_eq!(
            selector_from_name("/[/").unwrap(),
            Selector::Pattern("[".to_string())
        );
    }

    #[test]
    fn deep_merge_combines_nested_tables_rather_than_replacing_them() {
        let lua = Lua::new();
        let base: Table = lua
            .load(r#"{ icon = { color = 1, font = { family = "A", style = "B" } } }"#)
            .eval()
            .unwrap();
        let overlay: Table = lua
            .load(r#"{ icon = { font = { size = 11 } }, label = "hi" }"#)
            .eval()
            .unwrap();

        let merged = deep_merge(&lua, &base, &overlay).unwrap();
        let icon: Table = merged.get("icon").unwrap();
        assert_eq!(icon.get::<i64>("color").unwrap(), 1);
        let font: Table = icon.get("font").unwrap();
        assert_eq!(font.get::<String>("family").unwrap(), "A");
        assert_eq!(font.get::<String>("style").unwrap(), "B");
        assert_eq!(font.get::<i64>("size").unwrap(), 11);
        assert_eq!(merged.get::<String>("label").unwrap(), "hi");
    }

    #[test]
    fn deep_merge_lets_the_overlay_win_on_a_leaf_conflict() {
        let lua = Lua::new();
        let base: Table = lua.load(r"{ color = 1 }").eval().unwrap();
        let overlay: Table = lua.load(r"{ color = 2 }").eval().unwrap();
        let merged = deep_merge(&lua, &base, &overlay).unwrap();
        assert_eq!(merged.get::<i64>("color").unwrap(), 2);
    }

    #[test]
    fn deep_merge_does_not_mutate_either_input() {
        let lua = Lua::new();
        let base: Table = lua.load(r"{ color = 1 }").eval().unwrap();
        let overlay: Table = lua.load(r"{ color = 2 }").eval().unwrap();
        deep_merge(&lua, &base, &overlay).unwrap();
        assert_eq!(base.get::<i64>("color").unwrap(), 1);
        assert_eq!(overlay.get::<i64>("color").unwrap(), 2);
    }

    #[test]
    fn icon_string_is_sketchybars_own_spelling_for_the_glyph_text() {
        let lua = Lua::new();
        let table: Table = lua.load(r#"{ icon = { string = "" } }"#).eval().unwrap();
        let patch = item_patch_from_table(&table).unwrap();
        assert_eq!(patch.icon.unwrap().text.as_deref(), Some(""));
    }

    #[test]
    fn a_flat_font_string_passes_through_unchanged() {
        let lua = Lua::new();
        let table: Table = lua
            .load(r#"{ label = { font = "Hack:Bold:14" } }"#)
            .eval()
            .unwrap();
        let patch = item_patch_from_table(&table).unwrap();
        assert_eq!(
            patch.label.unwrap().font.unwrap().to_string(),
            "Hack:Bold:14"
        );
    }

    #[test]
    fn a_nested_font_table_joins_into_the_flat_spelling() {
        let lua = Lua::new();
        let table: Table = lua
            .load(r#"{ icon = { font = { family = "Hack", style = "Bold", size = 14 } } }"#)
            .eval()
            .unwrap();
        let patch = item_patch_from_table(&table).unwrap();
        assert_eq!(
            patch.icon.unwrap().font.unwrap().to_string(),
            "Hack:Bold:14"
        );
    }

    #[test]
    fn a_patch_only_carries_the_fields_the_table_actually_set() {
        // A config that only sets `label` must not synthesise `icon`,
        // `background`, or the untouched half of `label` itself — the daemon
        // marks a component dirty just by touching it, so a full patch would
        // repaint fields nothing changed.
        let lua = Lua::new();
        let table: Table = lua.load(r#"{ label = "hi" }"#).eval().unwrap();
        let patch = item_patch_from_table(&table).unwrap();
        assert!(patch.icon.is_none());
        assert!(patch.geometry.is_none());
        let label = patch.label.unwrap();
        assert_eq!(label.text.as_deref(), Some("hi"));
        assert_eq!(label.color, None);
        assert_eq!(label.font, None);
    }

    #[test]
    fn an_alias_table_degrades_to_none_instead_of_erroring() {
        // A real config tints a mirrored menu bar icon with
        // `alias = { color = ... }` — a table, not the plain name string
        // `ItemPatch::alias` actually carries. coolabah has no field for the
        // tint, but that is one dropped property, not a reason to fail the
        // whole `add`.
        let lua = Lua::new();
        let table: Table = lua
            .load(r"{ alias = { color = 0xffff0000 } }")
            .eval()
            .unwrap();
        let patch = item_patch_from_table(&table).unwrap();
        assert_eq!(patch.alias, None);
    }

    #[test]
    fn a_popup_position_names_the_item_it_hangs_off() {
        // `position = "popup.<owner>"` is a relationship, not a bar bucket:
        // the item goes inside that item's popup and takes no space on the
        // bar at all.
        let lua = Lua::new();
        let table: Table = lua.load(r#"{ position = "popup.volume" }"#).eval().unwrap();
        assert_eq!(
            item_position_from_table(&table).unwrap(),
            Position::Popup(crate::protocol::ItemName::new("volume").unwrap())
        );
    }

    #[test]
    fn a_popup_drawing_string_is_read_as_an_on_off_flag_not_lua_truthiness() {
        // A real config round-trips `popup.drawing` as `overflow:query().popup.drawing
        // == "on" and "off" or "on"` -- a Lua *string*, not a boolean. `mlua`'s
        // own `FromLua<bool>` follows Lua's truthiness rule, under which the
        // non-nil, non-false string `"off"` is truthy, so naively using it here
        // would turn "off" into `true` and never actually close the popup.
        let lua = Lua::new();
        let table: Table = lua
            .load(r#"{ popup = { drawing = "off" } }"#)
            .eval()
            .unwrap();
        let patch = item_patch_from_table(&table).unwrap();
        assert_eq!(
            patch.popup.unwrap().drawing,
            Some(crate::protocol::BoolChange::False)
        );

        let table: Table = lua
            .load(r#"{ popup = { drawing = "on" } }"#)
            .eval()
            .unwrap();
        let patch = item_patch_from_table(&table).unwrap();
        assert_eq!(
            patch.popup.unwrap().drawing,
            Some(crate::protocol::BoolChange::True)
        );
    }

    #[test]
    fn an_unrecognised_bool_string_is_a_named_error_not_a_silent_true() {
        // Lua truthiness makes any non-nil string true, so a bad spelling
        // used to become `drawing = on` silently.
        let lua = Lua::new();
        let table: Table = lua
            .load(r#"{ popup = { drawing = "perhaps" } }"#)
            .eval()
            .unwrap();
        let err = item_patch_from_table(&table).unwrap_err().to_string();
        assert!(err.contains("perhaps"), "{err}");
        assert!(err.contains("on/off"), "{err}");
    }

    #[test]
    fn a_bad_value_names_the_property_it_was_written_for() {
        // The half the author cannot see. A leaf type says what the value
        // should have been and has no idea which key it was reached
        // through, so the path comes from tracking the reading -- see
        // `patch_from_table`. This has now been lost twice; the assertion is
        // the point of the test.
        let lua = Lua::new();
        for (source, path) in [
            (r#"{ popup = { drawing = "perhaps" } }"#, "popup.drawing"),
            (
                r#"{ background = { color = "not-a-colour" } }"#,
                "background.color",
            ),
            (r#"{ label = { color = "puce" } }"#, "label.color"),
            (r#"{ position = "sideways" }"#, "position"),
        ] {
            let table: Table = lua.load(source).eval().unwrap();
            let err = item_patch_from_table(&table).unwrap_err().to_string();
            // The shape the CLI's own `ArgsError` prints for the same
            // mistake through `--set`: the path, backticked, then what the
            // value should have been.
            assert!(err.contains(&format!("`{path}`: ")), "{source}: {err}");
        }
    }

    #[test]
    fn a_triggers_variables_become_named_strings() {
        // sbar.trigger("demo", { VAR = "Test" }) reaches a script as $VAR.
        let lua = Lua::new();
        let table: Value = lua
            .load(r#"{ VAR = "Test", COUNT = 2, ON = true }"#)
            .eval()
            .unwrap();
        let vars = vars_from_value(&table).unwrap();
        assert_eq!(vars["VAR"], "Test");
        assert_eq!(vars["COUNT"], "2");
        assert_eq!(vars["ON"], "on");
    }

    #[test]
    fn a_nested_table_is_not_a_variable() {
        // Nothing on the other side would unpack it, so it is an error rather
        // than a silently JSON-encoded blob.
        let lua = Lua::new();
        let table: Value = lua.load(r"{ nested = { a = 1 } }").eval().unwrap();
        let err = vars_from_value(&table).unwrap_err();
        assert!(err.to_string().contains("nested"), "{err}");
    }

    #[test]
    fn a_popup_can_be_asked_to_toggle() {
        // What the config's click script does:
        // `sketchybar -m --set $NAME popup.drawing=toggle`. Only the daemon
        // can resolve it, so it has to survive the conversion intact.
        let lua = Lua::new();
        let table: Table = lua
            .load(r#"{ popup = { drawing = "toggle" } }"#)
            .eval()
            .unwrap();
        let patch = item_patch_from_table(&table).unwrap();
        assert_eq!(
            patch.popup.unwrap().drawing,
            Some(crate::protocol::BoolChange::Toggle)
        );
    }

    #[test]
    fn a_background_table_only_carries_the_fields_it_set() {
        let lua = Lua::new();
        let table: Table = lua
            .load(r"{ background = { color = 0xffff0000 } }")
            .eval()
            .unwrap();
        let patch = item_patch_from_table(&table).unwrap();
        let background = patch.geometry.unwrap().background.unwrap();
        assert_eq!(background.color, Some(Color(0xffff_0000)));
        assert_eq!(background.height, None);
        assert_eq!(background.corner_radius, None);
    }

    #[test]
    fn a_query_result_renders_flags_as_on_off_strings_not_booleans() {
        // Real `SketchyBar` never puts a JSON/Lua boolean in a query result —
        // a config reads `:query().geometry.drawing == "on"` — and coolabah's
        // own `item:query()` has to match that, not `mlua`'s own default
        // `bool -> boolean` serialization.
        let lua = Lua::new();
        let state = ItemState {
            popup: crate::protocol::PopupState {
                drawing: Boolish(false),
                horizontal: Boolish(false),
                align: crate::protocol::PopupAlign::Left,
                topmost: Boolish(true),
                height: 0.0,
                y_offset: 0.0,
                background: crate::protocol::Background::default(),
            },
            name: item_name_from_str("clock").unwrap(),
            geometry: crate::protocol::Geometry {
                drawing: Boolish(true),
                position: Position::Right,
                y_offset: 0.0,
                padding_left: 2.0,
                padding_right: 2.0,
                width: None,
                display: crate::protocol::DisplayTarget::default(),
                background: crate::protocol::Background {
                    drawing: Boolish(false),
                    ..Default::default()
                },
            },
            icon: crate::protocol::Run {
                drawing: Boolish(true),
                ..Default::default()
            },
            label: crate::protocol::Run::default(),
            scripting: crate::protocol::Scripting {
                script: None,
                click_script: None,
                update_freq: 0,
                updates: Boolish(true),
            },
            events: Vec::new(),
            alias: None,
            members: Vec::new(),
            associated_space: None,
            percentage: 0,
            knob: crate::protocol::Run::default(),
            highlight_color: Color::BLACK,
        };

        let table = item_state_to_table(&lua, &state).unwrap();
        let geometry: Table = table.get("geometry").unwrap();
        assert_eq!(geometry.get::<String>("drawing").unwrap(), "on");
        let background: Table = geometry.get("background").unwrap();
        assert_eq!(background.get::<String>("drawing").unwrap(), "off");
        let icon: Table = table.get("icon").unwrap();
        assert_eq!(icon.get::<String>("drawing").unwrap(), "on");
        let scripting: Table = table.get("scripting").unwrap();
        assert_eq!(scripting.get::<String>("updates").unwrap(), "on");
    }

    #[test]
    fn a_bar_query_result_renders_flags_as_on_off_strings() {
        let lua = Lua::new();
        let state = BarState {
            height: 32.0,
            edge: Edge::Top,
            color: Color::BLACK,
            margin: 0.0,
            y_offset: 0.0,
            corner_radius: 0.0,
            blur_radius: 0,
            topmost: Boolish(true),
            hidden: Boolish(false),
            displays: 1,
            padding_left: 0.0,
            padding_right: 0.0,
            display: crate::protocol::DisplayTarget::All,
            sticky: Boolish(false),
            show_in_fullscreen: Boolish(false),
            notch_width: 0.0,
            notch_offset: 0.0,
            notch_display_height: 0.0,
        };
        let table = bar_state_to_table(&lua, &state).unwrap();
        assert_eq!(table.get::<String>("topmost").unwrap(), "on");
        assert_eq!(table.get::<String>("hidden").unwrap(), "off");
    }

    #[test]
    fn position_defaults_to_left_when_unset() {
        let lua = Lua::new();
        let table: Table = lua.load(r"{ }").eval().unwrap();
        assert_eq!(item_position_from_table(&table).unwrap(), Position::Left);
    }

    #[test]
    fn position_is_read_out_of_the_options_table() {
        let lua = Lua::new();
        let table: Table = lua.load(r#"{ position = "right" }"#).eval().unwrap();
        assert_eq!(item_position_from_table(&table).unwrap(), Position::Right);
    }

    #[test]
    fn a_bar_table_reads_top_and_bottom_from_position_like_sketchybar_does() {
        let lua = Lua::new();
        let table: Table = lua.load(r#"{ position = "bottom" }"#).eval().unwrap();
        assert_eq!(
            bar_patch_from_table(&table).unwrap().edge,
            Some(Edge::Bottom)
        );
    }

    #[test]
    fn a_bar_table_still_accepts_the_older_edge_spelling() {
        let lua = Lua::new();
        let table: Table = lua.load(r#"{ edge = "bottom" }"#).eval().unwrap();
        assert_eq!(
            bar_patch_from_table(&table).unwrap().edge,
            Some(Edge::Bottom)
        );
    }
}

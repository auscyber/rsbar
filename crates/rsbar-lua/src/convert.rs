//! Lua tables on one side, `rsbar-protocol` types on the other.
//!
//! Every conversion here is typed: a colour is parsed by the same parser the
//! CLI uses, an event kind by the same `FromStr` `rsbar-cli` parses `--trigger`
//! with, and a patch is built field by field rather than handed to Lua as an
//! opaque blob. A malformed value fails at the call site with a message
//! naming the field, not three requests later as a daemon rejection.

use std::str::FromStr;

use mlua::{IntoLua, Lua, Table, Value};
use rsbar_protocol::style::Color;
use rsbar_protocol::{
    BarPatch, BarState, Edge, Event, ItemName, ItemPatch, ItemState, Kind, Position,
};

use crate::error::{ApiError, Result};

/// A colour as a config writes one: `0xaarrggbb`, `0xrrggbb` (opaque) or a
/// `"#rrggbb"` / `"#aarrggbb"` string.
///
/// # Errors
///
/// Returns [`ApiError::InvalidColor`] if `value` is not a number in range, a
/// parseable colour string, or the wrong Lua type entirely.
pub fn color_from_value(value: &Value) -> Result<u32> {
    match value {
        Value::Integer(n) => u32::try_from(*n).map_err(|_| ApiError::InvalidColor(n.to_string())),
        // A truncating, sign-losing cast on purpose: a colour is never
        // fractional or negative, and `try_from` below rejects it if it is.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Value::Number(n) => {
            u32::try_from(*n as i64).map_err(|_| ApiError::InvalidColor(n.to_string()))
        }
        Value::String(s) => {
            let s = s
                .to_str()
                .map_err(|_| ApiError::InvalidColor("<non-utf8>".into()))?;
            Color::from_str(&s)
                .map(|c| c.0)
                .map_err(|_| ApiError::InvalidColor(s.to_string()))
        }
        other => Err(ApiError::InvalidColor(format!("{other:?}"))),
    }
}

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

fn opt<T: mlua::FromLua>(table: &Table, key: &str) -> mlua::Result<Option<T>> {
    table.get::<Option<T>>(key)
}

fn opt_color(table: &Table, key: &str) -> mlua::Result<Option<u32>> {
    match table.get::<Value>(key)? {
        Value::Nil => Ok(None),
        value => Ok(Some(color_from_value(&value)?)),
    }
}

fn opt_position(table: &Table, key: &str) -> mlua::Result<Option<Position>> {
    opt::<String>(table, key)?
        .map(|s| position_from_str(&s))
        .transpose()
        .map_err(Into::into)
}

/// `icon = "text"` or `icon = { text = "...", color = 0x..., font = "Family:Style:Size" }`.
/// Expands into the flat `text`/`color`/`font` fields a patch carries.
struct IconOrLabel {
    text: Option<String>,
    color: Option<u32>,
    font: Option<String>,
}

impl IconOrLabel {
    fn read(table: &Table, key: &str) -> mlua::Result<Self> {
        match table.get::<Value>(key)? {
            Value::Nil => Ok(Self {
                text: None,
                color: None,
                font: None,
            }),
            Value::String(s) => Ok(Self {
                text: Some(s.to_str()?.to_string()),
                color: None,
                font: None,
            }),
            Value::Table(sub) => Ok(Self {
                text: opt::<String>(&sub, "text")?,
                color: opt_color(&sub, "color")?,
                font: opt::<String>(&sub, "font")?,
            }),
            other => Err(mlua::Error::RuntimeError(format!(
                "`{key}` must be a string or a table, got {}",
                other.type_name()
            ))),
        }
    }
}

/// `rsbar.bar({...})`.
///
/// # Errors
///
/// Returns a Lua error if any field has the wrong type or an invalid value.
pub fn bar_patch_from_table(table: &Table) -> mlua::Result<BarPatch> {
    Ok(BarPatch {
        height: opt(table, "height")?,
        edge: opt::<String>(table, "edge")?
            .map(|s| match s.to_ascii_lowercase().as_str() {
                "top" => Ok(Edge::Top),
                "bottom" => Ok(Edge::Bottom),
                other => Err(mlua::Error::RuntimeError(format!(
                    "`edge` must be \"top\" or \"bottom\", got \"{other}\""
                ))),
            })
            .transpose()?,
        color: opt_color(table, "color")?,
        margin: opt(table, "margin")?,
        y_offset: opt(table, "y_offset")?,
        corner_radius: opt(table, "corner_radius")?,
        blur_radius: opt(table, "blur_radius")?,
        hidden: opt(table, "hidden")?,
        topmost: opt(table, "topmost")?,
    })
}

/// `rsbar.add(name, position, {...})` / `item:set({...})`.
///
/// # Errors
///
/// Returns a Lua error if any field has the wrong type or an invalid value.
pub fn item_patch_from_table(table: &Table) -> mlua::Result<ItemPatch> {
    let icon = IconOrLabel::read(table, "icon")?;
    let label = IconOrLabel::read(table, "label")?;

    Ok(ItemPatch {
        // `IconOrLabel::read` already covers both the bare-string and the
        // `{ text = ... }` spellings of `icon`/`label` — there is no separate
        // flat key left to fall back to, and `icon`/`label` themselves are
        // not strings once they are a sugar table.
        icon: icon.text,
        label: label.text,
        icon_font: icon.font.or(opt(table, "icon_font")?),
        label_font: label.font.or(opt(table, "label_font")?),
        icon_color: icon.color.or(opt_color(table, "icon_color")?),
        label_color: label.color.or(opt_color(table, "label_color")?),
        background_color: opt_color(table, "background_color")?,
        corner_radius: opt(table, "corner_radius")?,
        padding_left: opt(table, "padding_left")?,
        padding_right: opt(table, "padding_right")?,
        y_offset: opt(table, "y_offset")?,
        position: opt_position(table, "position")?,
        drawing: opt(table, "drawing")?,
        script: opt(table, "script")?,
        click_script: opt(table, "click_script")?,
        alias: opt(table, "alias")?,
        update_freq: opt(table, "update_freq")?,
    })
}

/// # Errors
///
/// Returns a Lua error only if table creation itself fails.
pub fn item_state_to_table(lua: &Lua, state: &ItemState) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("name", state.name.as_str())?;
    table.set("position", format!("{:?}", state.position).to_lowercase())?;
    table.set("icon", state.icon.clone())?;
    table.set("label", state.label.clone())?;
    table.set("drawing", state.drawing)?;
    table.set("script", state.script.clone())?;
    table.set("click_script", state.click_script.clone())?;
    table.set("update_freq", state.update_freq)?;
    table.set("alias", state.alias.clone())?;
    let events = lua.create_table()?;
    for (index, kind) in state.events.iter().enumerate() {
        events.set(index + 1, kind.name())?;
    }
    table.set("events", events)?;
    Ok(table)
}

/// # Errors
///
/// Returns a Lua error only if table creation itself fails.
pub fn bar_state_to_table(lua: &Lua, state: &BarState) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("height", state.height)?;
    table.set(
        "edge",
        if state.edge == Edge::Top {
            "top"
        } else {
            "bottom"
        },
    )?;
    table.set("color", state.color)?;
    table.set("margin", state.margin)?;
    table.set("y_offset", state.y_offset)?;
    table.set("corner_radius", state.corner_radius)?;
    table.set("blur_radius", state.blur_radius)?;
    table.set("topmost", state.topmost)?;
    table.set("hidden", state.hidden)?;
    table.set("displays", state.displays)?;
    Ok(table)
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

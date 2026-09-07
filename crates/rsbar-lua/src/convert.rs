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

/// `SketchyBar`'s own convention for matching several items by name at once:
/// `/menu\..*/` rather than one literal name. Only `rsbar.set` and bracket
/// membership understand this — it is resolved entirely on this side, against
/// a snapshot of every item's name, since the protocol has no such concept.
///
/// # Errors
///
/// Returns [`ApiError::InvalidPattern`] if `name` looks like a pattern (is
/// wrapped in `/.../`) but its interior is not a valid regex.
pub fn pattern_from_name(name: &str) -> Result<Option<regex::Regex>> {
    let Some(inner) = name.strip_prefix('/').and_then(|s| s.strip_suffix('/')) else {
        return Ok(None);
    };
    regex::Regex::new(inner)
        .map(Some)
        .map_err(|e| ApiError::InvalidPattern(name.to_string(), e.to_string()))
}

/// Merges `overlay` onto `base`, recursively wherever both sides have a table
/// at the same key, so `rsbar.default`'s `icon = { font = { family = ... } }`
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

/// The `position` an add options table carries, or [`Position::default`] if
/// it names none — `rsbar.add`'s own vocabulary reads position out of the
/// options table rather than as a separate positional argument, matching how
/// a real `SketchyBar` config always writes it (`{ position = "left", ... }`).
///
/// # Errors
///
/// Returns a Lua error if `position` is set but not a recognised spelling.
pub fn item_position_from_table(table: &Table) -> mlua::Result<Position> {
    Ok(opt_position(table, "position")?.unwrap_or_default())
}

/// `icon = "text"` or `icon = { text = "...", color = 0x..., font = "Family:Style:Size" }`.
/// Expands into the flat `text`/`color`/`font` fields a patch carries.
#[derive(Default)]
struct IconOrLabel {
    text: Option<String>,
    color: Option<u32>,
    font: Option<String>,
    drawing: Option<bool>,
    padding_left: Option<f64>,
    padding_right: Option<f64>,
}

impl IconOrLabel {
    /// `None` when the table said nothing at all, so an untouched half is left
    /// alone rather than patched with a set of `None`s.
    fn into_patch(self) -> Option<rsbar_protocol::RunPatch> {
        let patch = rsbar_protocol::RunPatch {
            text: self.text,
            color: self.color,
            font: self.font,
            drawing: self.drawing,
            padding_left: self.padding_left,
            padding_right: self.padding_right,
        };
        (patch != rsbar_protocol::RunPatch::default()).then_some(patch)
    }
}

/// The nested `background = { ... }` table.
fn background_patch(table: &Table) -> mlua::Result<Option<rsbar_protocol::BackgroundPatch>> {
    const KNOWN: &[&str] = &[
        "color",
        "corner_radius",
        "height",
        "padding_left",
        "padding_right",
        "border_color",
        "border_width",
    ];
    let Some(sub) = opt::<Table>(table, "background")? else {
        return Ok(None);
    };
    warn_unknown_with(&sub, "background", KNOWN, BACKGROUND_UNSUPPORTED);
    Ok(Some(rsbar_protocol::BackgroundPatch {
        drawing: None,
        color: opt_color(&sub, "color")?,
        corner_radius: opt(&sub, "corner_radius")?,
        height: opt(&sub, "height")?,
        padding_left: opt(&sub, "padding_left")?,
        padding_right: opt(&sub, "padding_right")?,
        border_color: opt_color(&sub, "border_color")?,
        border_width: opt(&sub, "border_width")?,
    }))
}

/// Reports every key in `table` the caller does not know about.
///
/// Silence here is the failure this exists to stop: a converter reads the keys
/// it recognises and ignores the rest, so a typo — or a property `SketchyBar` has
/// and rsbar does not — configures nothing and says nothing. `label.drawing`
/// and `icon.drawing` were both written during a config port, both dropped,
/// and only found by reading this file.
///
/// Logged rather than rejected. A config that is right apart from one key
/// should still come up, with the key named loudly enough to fix.
fn warn_unknown(table: &Table, what: &str, known: &[&str]) {
    warn_unknown_with(table, what, known, &[]);
}

/// As [`warn_unknown`], but `unsupported` names real `SketchyBar` keys that
/// rsbar recognises and deliberately does not implement yet — a per-item
/// `display`, say. Those get their own message (what's missing, not "did you
/// mean") rather than being mistaken for a typo.
fn warn_unknown_with(table: &Table, what: &str, known: &[&str], unsupported: &[(&str, &str)]) {
    for pair in table.clone().pairs::<Value, Value>().flatten() {
        let Value::String(key) = pair.0 else { continue };
        let Ok(key) = key.to_str() else { continue };
        if known.contains(&&*key) {
            continue;
        }
        if let Some((_, note)) = unsupported.iter().find(|(k, _)| *k == &*key) {
            tracing::error!(%what, key = %key, "recognised, but not implemented: {note}");
            continue;
        }
        // A key that is real but in the wrong place is the likelier mistake,
        // and the more annoying one to find: `label = { drawing = false }`
        // reads perfectly and does nothing, because `drawing` belongs to the
        // item rather than to its label.
        if known.as_ptr() != ITEM_KEYS.as_ptr()
            && ITEM_KEYS.contains(&&*key)
            && !known.contains(&&*key)
        {
            tracing::error!(
                %what,
                key = %key,
                "unknown setting here; `{key}` belongs to the item, not to its `{what}`"
            );
            continue;
        }
        if let Some(guess) = nearest(&key, known) {
            tracing::error!(%what, key = %key, "unknown setting; did you mean `{guess}`?");
        } else {
            tracing::error!(%what, key = %key, "unknown setting; it does nothing");
        }
    }
}

/// `SketchyBar` item properties rsbar has no `ItemPatch` field for yet. Each
/// needs a protocol change; noted here so a config that uses one is loud about
/// it instead of silently dropping it.
const ITEM_UNSUPPORTED: &[(&str, &str)] = &[
    (
        "updates",
        "toggling routine/forced updates independently of `drawing`; needs an `ItemPatch` field",
    ),
    (
        "width",
        "a fixed-width spacer item; needs an `ItemPatch::width`",
    ),
    (
        "display",
        "targeting one display; needs an `ItemPatch::display` (tracked separately)",
    ),
    (
        "popup",
        "a popup attached to this item; needs popup support (tracked separately)",
    ),
];

/// As above, for the nested `background = { ... }` table.
const BACKGROUND_UNSUPPORTED: &[(&str, &str)] = &[(
    "drawing",
    "toggling the background independently of the item; needs a `BackgroundPatch::drawing`",
)];

/// As above, for `rsbar.bar({...})`.
const BAR_UNSUPPORTED: &[(&str, &str)] = &[
    (
        "display",
        "the bar always spans every display; needs a `BarPatch::display`",
    ),
    (
        "padding_left",
        "space before the first/after the last item, distinct from `margin`; needs `BarPatch::padding_left`",
    ),
    (
        "padding_right",
        "space before the first/after the last item, distinct from `margin`; needs `BarPatch::padding_right`",
    ),
];

/// The known key closest to `key`, when one is close enough to be worth
/// suggesting — a prefix, a suffix, or a one-character slip.
fn nearest<'a>(key: &str, known: &[&'a str]) -> Option<&'a str> {
    known
        .iter()
        .copied()
        .find(|candidate| {
            candidate.starts_with(key)
                || candidate.ends_with(key)
                || key.starts_with(*candidate)
                || key.ends_with(candidate)
        })
        .or_else(|| {
            known.iter().copied().find(|candidate| {
                candidate.len().abs_diff(key.len()) <= 1
                    && candidate
                        .chars()
                        .zip(key.chars())
                        .filter(|(a, b)| a != b)
                        .count()
                        <= 1
            })
        })
}

impl IconOrLabel {
    // `string` is the spelling a real SketchyBar config uses for the text
    // content of an icon/label; `text` is kept too so nothing that already
    // wrote it stops working. If a table somehow sets both, `string` wins.
    const KNOWN: &'static [&'static str] = &[
        "string",
        "text",
        "color",
        "font",
        "drawing",
        "padding_left",
        "padding_right",
    ];

    fn read(table: &Table, key: &str) -> mlua::Result<Self> {
        match table.get::<Value>(key)? {
            Value::Nil => Ok(Self::default()),
            Value::String(s) => Ok(Self {
                text: Some(s.to_str()?.to_string()),
                ..Self::default()
            }),
            Value::Table(sub) => {
                warn_unknown(&sub, key, Self::KNOWN);
                let string = opt::<String>(&sub, "string")?;
                let text = opt::<String>(&sub, "text")?;
                Ok(Self {
                    text: string.or(text),
                    color: opt_color(&sub, "color")?,
                    font: font_from_value(sub.get("font")?)?,
                    drawing: opt(&sub, "drawing")?,
                    padding_left: opt(&sub, "padding_left")?,
                    padding_right: opt(&sub, "padding_right")?,
                })
            }
            other => Err(mlua::Error::RuntimeError(format!(
                "`{key}` must be a string or a table, got {}",
                other.type_name()
            ))),
        }
    }
}

/// `font = "Family:Style:Size"` or `font = { family = ..., style = ...,
/// size = ... }`. The protocol only carries the flat, colon-joined spelling
/// (`FontSpec::parse` on the daemon side), so a nested table is joined into
/// it here; a field the table does not set joins as empty, which
/// `FontSpec::parse` already treats as "keep the default for this part".
///
/// A *partial* nested font (`font = { size = 11.0 }` alone, no family or
/// style) only makes sense combined with `rsbar.default`'s own `font` table —
/// see `api::merged_opts` — since rsbar has nothing to read a running item's
/// current font back from to merge against otherwise.
fn font_from_value(value: Value) -> mlua::Result<Option<String>> {
    const KNOWN: &[&str] = &["family", "style", "size"];
    match value {
        Value::Nil => Ok(None),
        Value::String(s) => Ok(Some(s.to_str()?.to_string())),
        Value::Table(sub) => {
            warn_unknown(&sub, "font", KNOWN);
            let family = opt::<String>(&sub, "family")?.unwrap_or_default();
            let style = opt::<String>(&sub, "style")?.unwrap_or_default();
            let size = match sub.get::<Value>("size")? {
                Value::Nil => String::new(),
                Value::Integer(i) => i.to_string(),
                Value::Number(n) => n.to_string(),
                other => {
                    return Err(mlua::Error::RuntimeError(format!(
                        "`font.size` must be a number, got {}",
                        other.type_name()
                    )));
                }
            };
            Ok(Some(format!("{family}:{style}:{size}")))
        }
        other => Err(mlua::Error::RuntimeError(format!(
            "`font` must be a string or a table, got {}",
            other.type_name()
        ))),
    }
}

/// `rsbar.bar({...})`.
///
/// # Errors
///
/// Returns a Lua error if any field has the wrong type or an invalid value.
/// Every key a bar table may carry.
const BAR_KEYS: &[&str] = &[
    "height",
    "edge",
    // A real SketchyBar config's own spelling for `edge` — the bar itself
    // uses `position` for top/bottom, distinct from an *item*'s `position`
    // (left/center/right). Accepted as a plain alias rather than requiring a
    // second, rsbar-only spelling every bar table would need translating.
    "position",
    "color",
    "margin",
    "y_offset",
    "corner_radius",
    "blur_radius",
    "hidden",
    "topmost",
];

fn edge_from_str(key: &str, s: &str) -> mlua::Result<Edge> {
    match s.to_ascii_lowercase().as_str() {
        "top" => Ok(Edge::Top),
        "bottom" => Ok(Edge::Bottom),
        other => Err(mlua::Error::RuntimeError(format!(
            "`{key}` must be \"top\" or \"bottom\", got \"{other}\""
        ))),
    }
}

/// # Errors
///
/// Returns a Lua error if a value has the wrong type or cannot be parsed —
/// a colour that is neither a number nor `#rrggbb`, or a position that names
/// no bucket. An unrecognised *key* is logged rather than raised: a config
/// that is right apart from one setting should still come up.
pub fn bar_patch_from_table(table: &Table) -> mlua::Result<BarPatch> {
    warn_unknown_with(table, "bar", BAR_KEYS, BAR_UNSUPPORTED);
    // `position` is what a real config actually writes; `edge` predates it
    // here and stays accepted too. `position` wins if a table somehow sets
    // both.
    let position = opt::<String>(table, "position")?;
    let edge = match position {
        Some(s) => Some(edge_from_str("position", &s)?),
        None => opt::<String>(table, "edge")?
            .map(|s| edge_from_str("edge", &s))
            .transpose()?,
    };
    Ok(BarPatch {
        padding_left: None,
        padding_right: None,
        display: None,
        height: opt(table, "height")?,
        edge,
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
/// Every key an item table may carry. `icon` and `label` also accept the
/// nested sugar table, whose own keys are checked separately.
const ITEM_KEYS: &[&str] = &[
    "icon",
    "label",
    "background",
    "padding_left",
    "padding_right",
    "y_offset",
    "position",
    "drawing",
    "script",
    "click_script",
    "alias",
    "members",
    "update_freq",
];

/// The items a bracket draws across, named in a list.
fn opt_members(table: &Table) -> mlua::Result<Option<Vec<rsbar_protocol::ItemName>>> {
    let Some(list) = opt::<Vec<String>>(table, "members")? else {
        return Ok(None);
    };
    list.into_iter()
        .map(|name| {
            rsbar_protocol::ItemName::new(name)
                .map_err(|err| mlua::Error::RuntimeError(err.to_string()))
        })
        .collect::<mlua::Result<Vec<_>>>()
        .map(Some)
}

/// # Errors
///
/// Returns a Lua error if a value has the wrong type or cannot be parsed —
/// a colour that is neither a number nor `#rrggbb`, or a position that names
/// no bucket. An unrecognised *key* is logged rather than raised: a config
/// that is right apart from one setting should still come up.
pub fn item_patch_from_table(table: &Table) -> mlua::Result<ItemPatch> {
    warn_unknown_with(table, "item", ITEM_KEYS, ITEM_UNSUPPORTED);
    let icon = IconOrLabel::read(table, "icon")?;
    let label = IconOrLabel::read(table, "label")?;

    Ok(ItemPatch {
        updates: None,
        width: None,
        display: None,
        // `IconOrLabel::read` already covers both the bare-string and the
        // `{ text = ... }` spellings of `icon`/`label` — there is no separate
        // flat key left to fall back to, and `icon`/`label` themselves are
        // not strings once they are a sugar table.
        icon: icon.into_patch(),
        label: label.into_patch(),
        background: background_patch(table)?,
        padding_left: opt(table, "padding_left")?,
        padding_right: opt(table, "padding_right")?,
        y_offset: opt(table, "y_offset")?,
        position: opt_position(table, "position")?,
        drawing: opt(table, "drawing")?,
        script: opt(table, "script")?,
        click_script: opt(table, "click_script")?,
        alias: opt(table, "alias")?,
        members: opt_members(table)?,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_name_is_not_a_pattern() {
        assert!(pattern_from_name("front_app").unwrap().is_none());
    }

    #[test]
    fn a_slash_wrapped_name_compiles_as_a_regex() {
        let pattern = pattern_from_name("/menu\\..*/").unwrap().unwrap();
        assert!(pattern.is_match("menu.1"));
        assert!(!pattern.is_match("front_app"));
    }

    #[test]
    fn an_invalid_pattern_names_itself_in_the_error() {
        let err = pattern_from_name("/[/").unwrap_err();
        assert!(matches!(err, ApiError::InvalidPattern(name, _) if name == "/[/"));
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
        assert_eq!(patch.label.unwrap().font.as_deref(), Some("Hack:Bold:14"));
    }

    #[test]
    fn a_nested_font_table_joins_into_the_flat_spelling() {
        let lua = Lua::new();
        let table: Table = lua
            .load(r#"{ icon = { font = { family = "Hack", style = "Bold", size = 14 } } }"#)
            .eval()
            .unwrap();
        let patch = item_patch_from_table(&table).unwrap();
        assert_eq!(patch.icon.unwrap().font.as_deref(), Some("Hack:Bold:14"));
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
        assert!(patch.background.is_none());
        let label = patch.label.unwrap();
        assert_eq!(label.text.as_deref(), Some("hi"));
        assert_eq!(label.color, None);
        assert_eq!(label.font, None);
    }

    #[test]
    fn a_background_table_only_carries_the_fields_it_set() {
        let lua = Lua::new();
        let table: Table = lua
            .load(r"{ background = { color = 0xffff0000 } }")
            .eval()
            .unwrap();
        let patch = item_patch_from_table(&table).unwrap();
        let background = patch.background.unwrap();
        assert_eq!(background.color, Some(0xffff_0000));
        assert_eq!(background.height, None);
        assert_eq!(background.corner_radius, None);
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

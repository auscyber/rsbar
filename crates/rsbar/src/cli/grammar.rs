//! `SketchyBar`'s command grammar, parsed into [`Request`]s.
//!
//! The domains, subdomains and property names below are `SketchyBar`'s own —
//! see `src/misc/defines.h` and `src/message.c` in the `SketchyBar` source —
//! not invented here. Where a real `SketchyBar` property has no matching field
//! on [`ItemPatch`] or [`BarPatch`], parsing fails with [`ParseError::KnownGap`]
//! naming it, rather than silently accepting and dropping it.
//!
//! One call to [`parse`] walks the whole argument list once, in order, and
//! returns every request it names. Lexing is [`lexopt`]'s: it hands back a
//! `--domain` as [`lexopt::Arg::Long`] and everything else as
//! [`lexopt::Arg::Value`], so a domain's own arguments are read with
//! [`lexopt::Parser::value`] (one, taken even if it looks like a flag — the
//! right behaviour for a mandatory name) and [`lexopt::Parser::values`] (a
//! greedy run that stops at the next flag-shaped token or the end of argv —
//! exactly `message.c`'s own rule for where one domain ends and the next
//! begins). No grammar beyond that lexing is needed: each domain has a fixed
//! shape, so a flat `match` on the domain name is all there is.

use lexopt::{Arg, Parser, ValueExt};
use rsbar_protocol::event::Custom;
use rsbar_protocol::style::Color;
use rsbar_protocol::{
    BackgroundPatch, BarPatch, Edge, Event, ItemName, ItemPatch, Json, Kind, Position, Query,
    Request, RunPatch,
};

/// A `SketchyBar` command line could not be turned into requests.
///
/// Every variant names the offending token, so a config typo produces a
/// message that points at it rather than a generic "invalid argument".
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ParseError {
    #[error(
        "`{0}` is not a recognised command: expected one of --bar, --set, --add, --remove, \
         --subscribe, --trigger, --query, --reload, --update, --exit"
    )]
    UnknownDomain(String),

    #[error("`{domain}` needs {expected}")]
    MissingArgument {
        domain: &'static str,
        expected: &'static str,
    },

    #[error("`{0}` is not `key=value`: expected e.g. `label.color=0xffffffff`")]
    NotKeyValue(String),

    #[error("`{key}` is not a `{domain}` property")]
    UnknownProperty { domain: &'static str, key: String },

    #[error("`{0}` names a real SketchyBar property that rsbar does not have yet: {1}")]
    KnownGap(String, &'static str),

    #[error("`{value}` is not valid for `{key}`: {reason}")]
    InvalidValue {
        key: String,
        value: String,
        reason: String,
    },

    #[error(transparent)]
    InvalidName(#[from] rsbar_protocol::InvalidName),

    #[error("`{0}` is not `item`, `bracket` or `alias`")]
    UnknownAddKind(String),

    #[error(
        "rsbar's `--reload` takes no path (unlike SketchyBar's): it re-runs the daemon's own \
         configured file; got `{0}`"
    )]
    ReloadTakesNoPath(String),

    #[error(
        "`{0}` is a built-in event and carries its own payload; only a custom event accepts \
         `key=value` pairs after it"
    )]
    BuiltinEventHasNoPayload(String),

    /// `lexopt`'s own errors: a value that is not valid Unicode, or one asked
    /// for where none remains. `lexopt::Error` is neither `Clone` nor
    /// `PartialEq`, so its message is captured rather than the error itself.
    #[error("{0}")]
    Lex(String),
}

fn lex_err(err: &lexopt::Error) -> ParseError {
    ParseError::Lex(err.to_string())
}

/// The domain flags that start a new command, spelled as `lexopt` reports
/// them (no leading dashes) — used only to recognise one that was consumed as
/// if it were an ordinary value, e.g. `--set --bar height=1` with the item
/// name missing. `lexopt::Parser::value` takes the next token unconditionally
/// (correctly, since a real name may itself start with `-`), so this is the
/// one place that still has to notice a domain flag by name instead of by
/// shape.
const DOMAINS: &[&str] = &[
    "bar",
    "set",
    "add",
    "remove",
    "subscribe",
    "trigger",
    "query",
    "reload",
    "update",
    "exit",
];

fn looks_like_a_domain(token: &str) -> bool {
    token
        .strip_prefix("--")
        .is_some_and(|name| DOMAINS.contains(&name))
}

/// One value, taken even if it looks like a flag — the right behaviour for a
/// mandatory name or position. Errors with `expected` rather than `lexopt`'s
/// own message, and rejects a value that is actually the next domain, which
/// almost always means the argument was left out rather than meant literally.
fn required_string(
    parser: &mut Parser,
    domain: &'static str,
    expected: &'static str,
) -> Result<String, ParseError> {
    let raw = parser
        .value()
        .map_err(|_| ParseError::MissingArgument { domain, expected })?;
    let text = raw.string().map_err(|e| lex_err(&e))?;
    if looks_like_a_domain(&text) {
        return Err(ParseError::MissingArgument { domain, expected });
    }
    Ok(text)
}

fn required_item_name(
    parser: &mut Parser,
    domain: &'static str,
    expected: &'static str,
) -> Result<ItemName, ParseError> {
    Ok(ItemName::new(required_string(parser, domain, expected)?)?)
}

/// A domain's greedy tail: every value up to the next flag-shaped token or
/// the end of argv, via [`Parser::values`] — [`lexopt`]'s own version of
/// `message.c`'s "read until the next domain" rule. Unlike `values` itself,
/// an empty tail is not an error: `--bar` with no properties, or `--reload`
/// with no path, are both legitimate.
fn greedy_strings(parser: &mut Parser) -> Result<Vec<String>, ParseError> {
    match parser.values() {
        Ok(values) => values
            .map(|v| v.string().map_err(|e| lex_err(&e)))
            .collect(),
        Err(lexopt::Error::MissingValue { .. }) => Ok(Vec::new()),
        Err(err) => Err(lex_err(&err)),
    }
}

fn split_kv(token: &str) -> Result<(&str, &str), ParseError> {
    token
        .split_once('=')
        .ok_or_else(|| ParseError::NotKeyValue(token.to_owned()))
}

fn invalid(key: &str, value: &str, reason: impl std::fmt::Display) -> ParseError {
    ParseError::InvalidValue {
        key: key.to_owned(),
        value: value.to_owned(),
        reason: reason.to_string(),
    }
}

fn parse_f64(key: &str, value: &str) -> Result<f64, ParseError> {
    value.parse().map_err(|e| invalid(key, value, e))
}

fn parse_u32(key: &str, value: &str) -> Result<u32, ParseError> {
    value.parse().map_err(|e| invalid(key, value, e))
}

fn parse_i32(key: &str, value: &str) -> Result<i32, ParseError> {
    value.parse().map_err(|e| invalid(key, value, e))
}

/// Colours as `0xaarrggbb` (`SketchyBar`'s own form) or `#rrggbb`/`#rgb`, both
/// handled by [`Color`]'s own parser.
fn parse_color(key: &str, value: &str) -> Result<u32, ParseError> {
    value
        .parse::<Color>()
        .map(|c| c.0)
        .map_err(|e| invalid(key, value, e))
}

fn parse_position_value(key: &str, value: &str) -> Result<Position, ParseError> {
    value
        .parse()
        .map_err(|e: rsbar_protocol::InvalidPosition| invalid(key, value, e))
}

fn parse_edge(key: &str, value: &str) -> Result<Edge, ParseError> {
    match value.to_ascii_lowercase().as_str() {
        "top" => Ok(Edge::Top),
        "bottom" => Ok(Edge::Bottom),
        _ => Err(invalid(key, value, "expected top or bottom")),
    }
}

/// The boolean spellings `SketchyBar` itself accepts (`ARGUMENT_COMMON_VAL_*`
/// in `defines.h`), `!`-negated forms included. `toggle` is a real spelling
/// too, but flipping a value needs to know the current one, which a one-shot
/// CLI does not — so it is a [`ParseError::KnownGap`], not a guess.
fn parse_bool(key: &str, value: &str) -> Result<bool, ParseError> {
    match value {
        "on" | "!off" | "true" | "!false" | "1" | "!0" | "yes" | "!no" => Ok(true),
        "off" | "!on" | "false" | "!true" | "0" | "!1" | "no" | "!yes" => Ok(false),
        "toggle" => Err(ParseError::KnownGap(
            key.to_owned(),
            "`toggle` needs the item's current value, which a one-shot CLI never has",
        )),
        _ => Err(invalid(
            key,
            value,
            "expected on/off, true/false, yes/no or 1/0 (optionally `!`-negated)",
        )),
    }
}

fn parse_members(value: &str) -> Result<Vec<ItemName>, ParseError> {
    value
        .split(',')
        .map(|name| ItemName::new(name.trim()).map_err(ParseError::from))
        .collect()
}

/// Accepts `SketchyBar`'s own event spelling (`volume_change`) alongside
/// rsbar's (`volume_changed`) by trying the bare name and then the same name
/// with a trailing `d` against the built-in list *before* ever falling back
/// to treating it as a custom event. Without that order, a mistyped built-in
/// name would silently become a custom event nobody ever triggers instead of
/// a reported error.
fn parse_event_name(token: &str) -> Result<Kind, ParseError> {
    let normalized = token.replace('-', "_").to_ascii_lowercase();
    if let Some(kind) = Kind::built_in()
        .into_iter()
        .find(|k| k.name() == normalized)
    {
        return Ok(kind);
    }
    let lengthened = format!("{normalized}d");
    if let Some(kind) = Kind::built_in()
        .into_iter()
        .find(|k| k.name() == lengthened)
    {
        return Ok(kind);
    }
    token
        .parse::<Kind>()
        .map_err(|e| invalid("event", token, e))
}

/// `--bar <k>=<v> ...`. See `DOMAIN_BAR` in `defines.h`.
fn apply_bar_property(patch: &mut BarPatch, key: &str, value: &str) -> Result<(), ParseError> {
    match key {
        "height" => patch.height = Some(parse_f64(key, value)?),
        "position" => patch.edge = Some(parse_edge(key, value)?),
        "color" => patch.color = Some(parse_color(key, value)?),
        "margin" => patch.margin = Some(parse_f64(key, value)?),
        "y_offset" => patch.y_offset = Some(parse_f64(key, value)?),
        "corner_radius" => patch.corner_radius = Some(parse_f64(key, value)?),
        "blur_radius" => patch.blur_radius = Some(parse_i32(key, value)?),
        "hidden" => patch.hidden = Some(parse_bool(key, value)?),
        "topmost" => patch.topmost = Some(parse_bool(key, value)?),
        "display"
        | "space"
        | "sticky"
        | "show_in_fullscreen"
        | "font_smoothing"
        | "shadow"
        | "align"
        | "notch_width"
        | "notch_offset"
        | "notch_display_height"
        | "horizontal" => {
            return Err(ParseError::KnownGap(
                key.to_owned(),
                "no matching BarPatch field yet",
            ));
        }
        _ => {
            return Err(ParseError::UnknownProperty {
                domain: "--bar",
                key: key.to_owned(),
            });
        }
    }
    Ok(())
}

/// A bare (no-subdomain) `--set` property, e.g. `script=...`.
///
/// A bare `icon=`/`label=` is sugar for `icon.text=`/`label.text=`, the same
/// way the Lua API treats a bare string.
fn apply_item_bare_property(
    patch: &mut ItemPatch,
    key: &str,
    value: &str,
) -> Result<(), ParseError> {
    match key {
        "icon" => patch.icon.get_or_insert_with(RunPatch::default).text = Some(value.to_owned()),
        "label" => {
            patch.label.get_or_insert_with(RunPatch::default).text = Some(value.to_owned());
        }
        "drawing" => patch.drawing = Some(parse_bool(key, value)?),
        "script" => patch.script = Some(value.to_owned()),
        "click_script" => patch.click_script = Some(value.to_owned()),
        "update_freq" => patch.update_freq = Some(parse_u32(key, value)?),
        "position" => patch.position = Some(parse_position_value(key, value)?),
        "padding_left" => patch.padding_left = Some(parse_f64(key, value)?),
        "padding_right" => patch.padding_right = Some(parse_f64(key, value)?),
        "y_offset" => patch.y_offset = Some(parse_f64(key, value)?),
        // Neither of these is a SketchyBar property: SketchyBar identifies an
        // alias by the argument to `--add alias`, and has no `--set` property
        // for bracket membership at all. Both are real ItemPatch fields with
        // no SketchyBar-shaped home, so they get the plainest key rsbar has.
        "alias" => patch.alias = Some(value.to_owned()),
        "members" => patch.members = Some(parse_members(value)?),
        "updates" | "scroll_texts" | "width" | "align" | "associated_display" | "display"
        | "associated_space" | "space" | "blur_radius" | "shadow" | "lazy" | "cache_scripts"
        | "ignore_association" | "max_chars" => {
            return Err(ParseError::KnownGap(
                key.to_owned(),
                "no matching ItemPatch field yet",
            ));
        }
        _ => {
            return Err(ParseError::UnknownProperty {
                domain: "--set",
                key: key.to_owned(),
            });
        }
    }
    Ok(())
}

/// A dotted `--set` property, e.g. `label.color=...` or `background.corner_radius=...`.
///
/// `icon`/`label`/`background` are components in their own right now
/// ([`RunPatch`], [`RunPatch`], [`BackgroundPatch`]), mirroring the dotted key
/// directly instead of translating it onto a flat field — so a property such
/// as `icon.padding_left` is simply the field of the same name on the
/// component it names, not a special case.
fn apply_item_dotted_property(
    patch: &mut ItemPatch,
    subdomain: &str,
    prop: &str,
    full_key: &str,
    value: &str,
) -> Result<(), ParseError> {
    match subdomain {
        "icon" => apply_run_property(
            patch.icon.get_or_insert_with(RunPatch::default),
            prop,
            full_key,
            value,
        )?,
        "label" => apply_run_property(
            patch.label.get_or_insert_with(RunPatch::default),
            prop,
            full_key,
            value,
        )?,
        "background" => apply_background_property(
            patch
                .background
                .get_or_insert_with(BackgroundPatch::default),
            prop,
            full_key,
            value,
        )?,
        "popup" | "slider" | "knob" | "graph" | "shadow" | "image" | "alias" => {
            return Err(ParseError::KnownGap(
                full_key.to_owned(),
                "no matching ItemPatch field yet",
            ));
        }
        _ => {
            return Err(ParseError::UnknownProperty {
                domain: "--set",
                key: full_key.to_owned(),
            });
        }
    }
    Ok(())
}

/// `icon.*`/`label.*`, onto a [`RunPatch`].
fn apply_run_property(
    run: &mut RunPatch,
    prop: &str,
    full_key: &str,
    value: &str,
) -> Result<(), ParseError> {
    match prop {
        // `icon.string=`/`label.string=` is the dotted spelling of the bare
        // sugar (`icon=`/`label=`); `.text=` is rsbar's own name for the
        // field, accepted too since it is what the property is actually
        // called.
        "string" | "text" => run.text = Some(value.to_owned()),
        "color" => run.color = Some(parse_color(full_key, value)?),
        "font" => run.font = Some(value.to_owned()),
        "drawing" => run.drawing = Some(parse_bool(full_key, value)?),
        "padding_left" => run.padding_left = Some(parse_f64(full_key, value)?),
        "padding_right" => run.padding_right = Some(parse_f64(full_key, value)?),
        "highlight" | "highlight_color" | "y_offset" | "scroll_duration" | "width" | "align"
        | "max_chars" => {
            return Err(ParseError::KnownGap(
                full_key.to_owned(),
                "no matching RunPatch field yet",
            ));
        }
        _ => {
            return Err(ParseError::UnknownProperty {
                domain: "--set",
                key: full_key.to_owned(),
            });
        }
    }
    Ok(())
}

/// `background.*`, onto a [`BackgroundPatch`].
fn apply_background_property(
    background: &mut BackgroundPatch,
    prop: &str,
    full_key: &str,
    value: &str,
) -> Result<(), ParseError> {
    match prop {
        "color" => background.color = Some(parse_color(full_key, value)?),
        "corner_radius" => background.corner_radius = Some(parse_f64(full_key, value)?),
        "height" => background.height = Some(parse_f64(full_key, value)?),
        "padding_left" => background.padding_left = Some(parse_f64(full_key, value)?),
        "padding_right" => background.padding_right = Some(parse_f64(full_key, value)?),
        "border_color" => background.border_color = Some(parse_color(full_key, value)?),
        "border_width" => background.border_width = Some(parse_f64(full_key, value)?),
        "drawing" | "clip" | "x_offset" | "y_offset" | "image" => {
            return Err(ParseError::KnownGap(
                full_key.to_owned(),
                "no matching BackgroundPatch field yet",
            ));
        }
        _ => {
            return Err(ParseError::UnknownProperty {
                domain: "--set",
                key: full_key.to_owned(),
            });
        }
    }
    Ok(())
}

/// `--add item|bracket|alias ...`. See `DOMAIN_ADD` in `defines.h`.
///
/// `--add alias <name> <position>` is treated exactly like `--add item`: in
/// `SketchyBar` the argument to `--add alias` *is* the mirror target, but in
/// rsbar the target is the `alias=Owner,Name` property already on
/// [`ItemPatch`] (`Owner,Name` cannot be a valid [`ItemName`] — it has a comma
/// and usually spaces). So the name given here is the new item's own name,
/// and the target is set the way any other property is: chained, with
/// `--set <name> alias="Owner,Name"` right after.
fn parse_add(parser: &mut Parser) -> Result<Vec<Request>, ParseError> {
    let kind = required_string(parser, "--add", "item, bracket or alias")?;
    match kind.as_str() {
        "item" | "alias" => {
            let domain: &'static str = if kind == "item" {
                "--add item"
            } else {
                "--add alias"
            };
            let name = required_item_name(parser, domain, "a name")?;
            let position_token = required_string(parser, domain, "a position")?;
            let position = parse_position_value("position", &position_token)?;
            Ok(vec![Request::AddItem { name, position }])
        }
        "bracket" => {
            let name = required_item_name(parser, "--add bracket", "a name")?;
            let member_group = greedy_strings(parser)?;
            if member_group.is_empty() {
                return Err(ParseError::MissingArgument {
                    domain: "--add bracket",
                    expected: "at least one member",
                });
            }
            let members = member_group
                .iter()
                .map(|m| ItemName::new(m.as_str()).map_err(ParseError::from))
                .collect::<Result<Vec<_>, _>>()?;
            // SketchyBar's own `--add bracket` takes no position: a bracket's
            // frame comes from its members. rsbar's `AddItem` still needs one
            // to file the item under, so it defaults to `left`, same as
            // `--add item` without a `--position` used to.
            Ok(vec![
                Request::AddItem {
                    name: name.clone(),
                    position: Position::Left,
                },
                Request::SetItem {
                    name,
                    patch: Box::new(ItemPatch {
                        members: Some(members),
                        ..Default::default()
                    }),
                },
            ])
        }
        "graph" | "space" | "slider" => Err(ParseError::KnownGap(
            kind,
            "this component type is not modelled by rsbar's ItemPatch",
        )),
        "event" => Err(ParseError::KnownGap(
            kind,
            "custom events need no registration in rsbar: --trigger accepts any name directly",
        )),
        _ => Err(ParseError::UnknownAddKind(kind)),
    }
}

fn parse_bar(parser: &mut Parser) -> Result<Request, ParseError> {
    let mut patch = BarPatch::default();
    for token in greedy_strings(parser)? {
        let (key, value) = split_kv(&token)?;
        apply_bar_property(&mut patch, key, value)?;
    }
    Ok(Request::SetBar(patch))
}

fn parse_set(parser: &mut Parser) -> Result<Request, ParseError> {
    let name = required_item_name(parser, "--set", "an item name")?;
    let mut patch = ItemPatch::default();
    for token in greedy_strings(parser)? {
        let (key, value) = split_kv(&token)?;
        match key.split_once('.') {
            Some((sub, prop)) => apply_item_dotted_property(&mut patch, sub, prop, key, value)?,
            None => apply_item_bare_property(&mut patch, key, value)?,
        }
    }
    Ok(Request::SetItem {
        name,
        patch: Box::new(patch),
    })
}

fn parse_remove(parser: &mut Parser) -> Result<Request, ParseError> {
    let token = required_string(parser, "--remove", "an item name")?;
    if token.len() > 1 && token.starts_with('/') && token.ends_with('/') {
        return Err(ParseError::KnownGap(
            token,
            "regex removal (SketchyBar's /pattern/) is not supported; rsbar removes one exact \
             name at a time",
        ));
    }
    Ok(Request::RemoveItem(ItemName::new(token)?))
}

fn parse_subscribe(parser: &mut Parser) -> Result<Request, ParseError> {
    let name = required_item_name(parser, "--subscribe", "an item name")?;
    let group = greedy_strings(parser)?;
    if group.is_empty() {
        return Err(ParseError::MissingArgument {
            domain: "--subscribe",
            expected: "at least one event",
        });
    }
    let events = group
        .iter()
        .map(|e| parse_event_name(e))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Request::Subscribe { name, events })
}

fn parse_trigger(parser: &mut Parser) -> Result<Request, ParseError> {
    let event_token = required_string(parser, "--trigger", "an event name")?;
    let kind = parse_event_name(&event_token)?;
    let group = greedy_strings(parser)?;
    let is_builtin = Kind::built_in().contains(&kind);
    let event = if group.is_empty() {
        kind.into_event()
    } else if is_builtin {
        return Err(ParseError::BuiltinEventHasNoPayload(kind.name().to_owned()));
    } else {
        let mut fields = Vec::with_capacity(group.len());
        for token in &group {
            let (key, value) = split_kv(token)?;
            fields.push((key.to_owned(), Json::parse_or_string(value)));
        }
        Event::Custom(Custom {
            name: kind.name().to_owned(),
            data: Json::Object(fields),
        })
    };
    Ok(Request::Trigger(event))
}

fn parse_query(parser: &mut Parser) -> Result<Request, ParseError> {
    let token = required_string(parser, "--query", "bar, items, menu-items or an item name")?;
    let query = match token.as_str() {
        "bar" => Query::Bar,
        "items" => Query::Items,
        "menu-items" | "menu_items" | "default_menu_items" => Query::MenuItems,
        "defaults" => {
            return Err(ParseError::KnownGap(
                token,
                "there is no notion of --default properties for rsbar to query yet",
            ));
        }
        "events" => {
            return Err(ParseError::KnownGap(
                token,
                "rsbar does not track registered custom events",
            ));
        }
        "displays" => {
            return Err(ParseError::KnownGap(
                token,
                "querying displays is not implemented",
            ));
        }
        _ => Query::Item(ItemName::new(token.as_str())?),
    };
    Ok(Request::Query(query))
}

fn parse_reload(parser: &mut Parser) -> Result<Request, ParseError> {
    let group = greedy_strings(parser)?;
    if let Some(path) = group.into_iter().next() {
        return Err(ParseError::ReloadTakesNoPath(path));
    }
    Ok(Request::Reload)
}

/// Parses a whole `SketchyBar`-shaped command line into the requests it
/// names, in order.
///
/// # Errors
///
/// Returns the first [`ParseError`] encountered, naming the offending token.
pub fn parse(args: &[String]) -> Result<Vec<Request>, ParseError> {
    let mut parser = Parser::from_args(args.iter().cloned());
    let mut requests = Vec::new();

    while let Some(arg) = parser.next().map_err(|e| lex_err(&e))? {
        match arg {
            Arg::Long("bar") => requests.push(parse_bar(&mut parser)?),
            Arg::Long("set") => requests.push(parse_set(&mut parser)?),
            Arg::Long("add") => requests.extend(parse_add(&mut parser)?),
            Arg::Long("remove") => requests.push(parse_remove(&mut parser)?),
            Arg::Long("subscribe") => requests.push(parse_subscribe(&mut parser)?),
            Arg::Long("trigger") => requests.push(parse_trigger(&mut parser)?),
            Arg::Long("query") => requests.push(parse_query(&mut parser)?),
            Arg::Long("reload") => requests.push(parse_reload(&mut parser)?),
            Arg::Long("update") => requests.push(Request::UpdateAll),
            // SketchyBar's own domain for quitting the running instance.
            Arg::Long("exit") => requests.push(Request::Shutdown),
            Arg::Long(other) => return Err(ParseError::UnknownDomain(format!("--{other}"))),
            Arg::Short(short) => return Err(ParseError::UnknownDomain(format!("-{short}"))),
            Arg::Value(value) => {
                return Err(ParseError::UnknownDomain(
                    value.to_string_lossy().into_owned(),
                ));
            }
        }
    }

    Ok(requests)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsbar_protocol::event::PowerChange;

    fn args(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|s| (*s).to_owned()).collect()
    }

    fn name(s: &str) -> ItemName {
        ItemName::new(s).unwrap()
    }

    #[test]
    fn bar_properties_apply_in_one_request() {
        let requests = parse(&args(&["--bar", "height=32", "color=0xff000000"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::SetBar(BarPatch {
                height: Some(32.0),
                color: Some(0xff00_0000),
                ..Default::default()
            })]
        );
    }

    #[test]
    fn hash_colours_still_parse() {
        let requests = parse(&args(&["--bar", "color=#ff0000"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::SetBar(BarPatch {
                color: Some(0xffff_0000),
                ..Default::default()
            })]
        );
    }

    #[test]
    fn dotted_item_properties_map_onto_the_matching_component() {
        let requests = parse(&args(&[
            "--set",
            "clock",
            "label.color=0xffffffff",
            "icon.font=Hack:Bold:14",
            "background.corner_radius=6",
        ]))
        .unwrap();
        assert_eq!(
            requests,
            vec![Request::SetItem {
                name: name("clock"),
                patch: Box::new(ItemPatch {
                    label: Some(RunPatch {
                        color: Some(0xffff_ffff),
                        ..Default::default()
                    }),
                    icon: Some(RunPatch {
                        font: Some("Hack:Bold:14".into()),
                        ..Default::default()
                    }),
                    background: Some(BackgroundPatch {
                        corner_radius: Some(6.0),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            }]
        );
    }

    #[test]
    fn a_bare_icon_or_label_is_sugar_for_its_text_field() {
        let requests = parse(&args(&["--set", "clock", "icon=🕐", "label=09:41"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::SetItem {
                name: name("clock"),
                patch: Box::new(ItemPatch {
                    icon: Some(RunPatch {
                        text: Some("🕐".into()),
                        ..Default::default()
                    }),
                    label: Some(RunPatch {
                        text: Some("09:41".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            }]
        );
    }

    #[test]
    fn icon_padding_is_expressible_now_that_it_is_its_own_component() {
        // This used to be a `ParseError::KnownGap`: flat `ItemPatch` had
        // nowhere to put an icon-specific padding distinct from the item's
        // own. Nested `RunPatch` gives it a home.
        let requests = parse(&args(&["--set", "clock", "icon.padding_left=4"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::SetItem {
                name: name("clock"),
                patch: Box::new(ItemPatch {
                    icon: Some(RunPatch {
                        padding_left: Some(4.0),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            }]
        );
    }

    #[test]
    fn background_height_and_border_are_expressible_now_too() {
        let requests = parse(&args(&[
            "--set",
            "clock",
            "background.height=20",
            "background.border_color=0xff00ff00",
            "background.border_width=2",
        ]))
        .unwrap();
        assert_eq!(
            requests,
            vec![Request::SetItem {
                name: name("clock"),
                patch: Box::new(ItemPatch {
                    background: Some(BackgroundPatch {
                        height: Some(20.0),
                        border_color: Some(0xff00_ff00),
                        border_width: Some(2.0),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            }]
        );
    }

    #[test]
    fn several_domains_chain_in_one_invocation() {
        let requests = parse(&args(&[
            "--add",
            "item",
            "clock",
            "right",
            "--set",
            "clock",
            "update_freq=1",
            "script=/plugins/clock.sh",
            "--add",
            "item",
            "battery",
            "right",
            "--set",
            "battery",
            "script=/plugins/battery.sh",
            "--subscribe",
            "battery",
            "power_source_change",
        ]))
        .unwrap();

        assert_eq!(
            requests,
            vec![
                Request::AddItem {
                    name: name("clock"),
                    position: Position::Right,
                },
                Request::SetItem {
                    name: name("clock"),
                    patch: Box::new(ItemPatch {
                        update_freq: Some(1),
                        script: Some("/plugins/clock.sh".into()),
                        ..Default::default()
                    }),
                },
                Request::AddItem {
                    name: name("battery"),
                    position: Position::Right,
                },
                Request::SetItem {
                    name: name("battery"),
                    patch: Box::new(ItemPatch {
                        script: Some("/plugins/battery.sh".into()),
                        ..Default::default()
                    }),
                },
                Request::Subscribe {
                    name: name("battery"),
                    events: vec![Kind::PowerSourceChanged],
                },
            ]
        );
    }

    #[test]
    fn add_bracket_sets_members_via_a_second_request() {
        let requests = parse(&args(&[
            "--add",
            "bracket",
            "group",
            "left_item",
            "right_item",
        ]))
        .unwrap();
        assert_eq!(
            requests,
            vec![
                Request::AddItem {
                    name: name("group"),
                    position: Position::Left,
                },
                Request::SetItem {
                    name: name("group"),
                    patch: Box::new(ItemPatch {
                        members: Some(vec![name("left_item"), name("right_item")]),
                        ..Default::default()
                    }),
                },
            ]
        );
    }

    #[test]
    fn add_alias_creates_a_plain_item_for_a_chained_set_to_target() {
        let requests = parse(&args(&[
            "--add",
            "alias",
            "mirror",
            "right",
            "--set",
            "mirror",
            "alias=Control Center,Battery",
        ]))
        .unwrap();
        assert_eq!(
            requests,
            vec![
                Request::AddItem {
                    name: name("mirror"),
                    position: Position::Right,
                },
                Request::SetItem {
                    name: name("mirror"),
                    patch: Box::new(ItemPatch {
                        alias: Some("Control Center,Battery".into()),
                        ..Default::default()
                    }),
                },
            ]
        );
    }

    #[test]
    fn subscribe_accepts_sketchybars_shorter_event_spelling() {
        let requests = parse(&args(&[
            "--subscribe",
            "battery",
            "power_source_change",
            "volume_change",
        ]))
        .unwrap();
        assert_eq!(
            requests,
            vec![Request::Subscribe {
                name: name("battery"),
                events: vec![Kind::PowerSourceChanged, Kind::VolumeChanged],
            }]
        );
    }

    #[test]
    fn trigger_with_no_payload_uses_the_events_default() {
        let requests = parse(&args(&["--trigger", "power_source_changed"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::Trigger(Event::PowerSourceChanged(
                PowerChange::default()
            ))]
        );
    }

    #[test]
    fn trigger_on_a_custom_event_collects_key_value_pairs_into_one_object() {
        let requests = parse(&args(&["--trigger", "my.event", "foo=1", "bar=hello"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::Trigger(Event::Custom(Custom {
                name: "my.event".into(),
                data: Json::Object(vec![
                    ("foo".into(), Json::Int(1)),
                    ("bar".into(), Json::String("hello".into())),
                ]),
            }))]
        );
    }

    #[test]
    fn a_builtin_event_rejects_a_payload() {
        let err = parse(&args(&["--trigger", "volume_changed", "level=5"])).unwrap_err();
        assert_eq!(
            err,
            ParseError::BuiltinEventHasNoPayload("volume_changed".into())
        );
    }

    #[test]
    fn query_keywords_and_item_names_are_told_apart() {
        assert_eq!(
            parse(&args(&["--query", "bar"])).unwrap(),
            vec![Request::Query(Query::Bar)]
        );
        assert_eq!(
            parse(&args(&["--query", "items"])).unwrap(),
            vec![Request::Query(Query::Items)]
        );
        assert_eq!(
            parse(&args(&["--query", "menu-items"])).unwrap(),
            vec![Request::Query(Query::MenuItems)]
        );
        assert_eq!(
            parse(&args(&["--query", "clock"])).unwrap(),
            vec![Request::Query(Query::Item(name("clock")))]
        );
    }

    #[test]
    fn remove_reload_update_and_exit() {
        assert_eq!(
            parse(&args(&["--remove", "clock"])).unwrap(),
            vec![Request::RemoveItem(name("clock"))]
        );
        assert_eq!(parse(&args(&["--reload"])).unwrap(), vec![Request::Reload]);
        assert_eq!(
            parse(&args(&["--update"])).unwrap(),
            vec![Request::UpdateAll]
        );
        assert_eq!(parse(&args(&["--exit"])).unwrap(), vec![Request::Shutdown]);
    }

    #[test]
    fn an_unrecognised_domain_names_itself() {
        let err = parse(&args(&["--nope", "clock"])).unwrap_err();
        assert_eq!(err, ParseError::UnknownDomain("--nope".into()));
    }

    #[test]
    fn a_missing_item_name_is_not_silently_taken_from_the_next_domain() {
        // Without the flag-shaped guard, `lexopt`'s `value()` would happily
        // hand back "--bar" as if it were the item name (it is valid
        // `ItemName` syntax) and swallow the whole next domain with it.
        let err = parse(&args(&["--set", "--bar", "height=1"])).unwrap_err();
        assert_eq!(
            err,
            ParseError::MissingArgument {
                domain: "--set",
                expected: "an item name",
            }
        );
    }

    #[test]
    fn a_dash_leading_value_is_still_a_value_not_a_flag() {
        // `y_offset=-5` does not itself start with `-`, so it is business as
        // usual for `lexopt`; a negative offset should not need special
        // handling in the grammar.
        let requests = parse(&args(&["--set", "clock", "y_offset=-5"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::SetItem {
                name: name("clock"),
                patch: Box::new(ItemPatch {
                    y_offset: Some(-5.0),
                    ..Default::default()
                }),
            }]
        );
    }

    #[test]
    fn a_missing_key_value_pair_names_the_bad_token() {
        let err = parse(&args(&["--set", "clock", "label"])).unwrap_err();
        assert_eq!(err, ParseError::NotKeyValue("label".into()));
    }

    #[test]
    fn an_unknown_item_property_names_itself() {
        let err = parse(&args(&["--set", "clock", "wat=1"])).unwrap_err();
        assert_eq!(
            err,
            ParseError::UnknownProperty {
                domain: "--set",
                key: "wat".into(),
            }
        );
    }

    #[test]
    fn an_unknown_subdomain_is_an_unknown_property_not_a_known_gap() {
        let err = parse(&args(&["--set", "clock", "wat.color=1"])).unwrap_err();
        assert_eq!(
            err,
            ParseError::UnknownProperty {
                domain: "--set",
                key: "wat.color".into(),
            }
        );
    }

    #[test]
    fn a_real_but_unmapped_property_is_a_known_gap() {
        let err = parse(&args(&[
            "--set",
            "clock",
            "icon.highlight_color=0xffffffff",
        ]))
        .unwrap_err();
        assert_eq!(
            err,
            ParseError::KnownGap(
                "icon.highlight_color".into(),
                "no matching RunPatch field yet"
            )
        );
    }

    #[test]
    fn a_bad_colour_names_the_value_and_the_key() {
        let err = parse(&args(&["--bar", "color=not-a-colour"])).unwrap_err();
        assert!(matches!(
            err,
            ParseError::InvalidValue { key, value, .. }
                if key == "color" && value == "not-a-colour"
        ));
    }

    #[test]
    fn reload_with_a_path_is_reported_rather_than_silently_dropped() {
        let err = parse(&args(&["--reload", "/tmp/other.rc"])).unwrap_err();
        assert_eq!(err, ParseError::ReloadTakesNoPath("/tmp/other.rc".into()));
    }

    #[test]
    fn regex_remove_is_a_reported_gap() {
        let err = parse(&args(&["--remove", "/clock.*/"])).unwrap_err();
        assert_eq!(
            err,
            ParseError::KnownGap(
                "/clock.*/".into(),
                "regex removal (SketchyBar's /pattern/) is not supported; rsbar removes one \
                 exact name at a time"
            )
        );
    }

    #[test]
    fn toggle_is_a_known_gap_not_a_guess() {
        let err = parse(&args(&["--bar", "hidden=toggle"])).unwrap_err();
        assert_eq!(
            err,
            ParseError::KnownGap(
                "hidden".into(),
                "`toggle` needs the item's current value, which a one-shot CLI never has"
            )
        );
    }
}

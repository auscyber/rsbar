//! `SketchyBar`'s command grammar, parsed into [`Request`]s.
//!
//! The domains, subdomains and property names below are `SketchyBar`'s own —
//! see `src/misc/defines.h` and `src/message.c` in the `SketchyBar` source —
//! not invented here.
//!
//! One call to [`parse`] walks the whole argument list once, in order, and
//! returns every request it names. Lexing is [`lexopt`]'s: it hands back a
//! `--domain` as [`lexopt::Arg::Long`] and everything else as
//! [`lexopt::Arg::Value`], so a domain's own arguments are read with
//! [`lexopt::Parser::value`] (one, taken even if it looks like a flag — the
//! right behaviour for a mandatory name) and [`lexopt::Parser::values`] (a
//! greedy run that stops at the next flag-shaped token or the end of argv —
//! exactly `message.c`'s own rule for where one domain ends and the next
//! begins).
//!
//! A domain that carries `key=value` properties (`--bar`, `--set`,
//! `--default`) does not match property names by hand: the tokens become a
//! run of `(dotted.path, value)` pairs, and [`super::args::from_pairs`]
//! deserializes that straight into [`BarPatch`] or [`ItemPatch`] via a
//! hand-written [`serde::Deserializer`](serde::Deserializer). Coercion comes
//! from the *target field's* type — `padding_left=4` parses as `f64` because
//! that is what [`ItemPatch::padding_left`] is, not because this module knows
//! it — so a new field on those patch structs is accepted here with no
//! grammar change at all, and `#[serde(deny_unknown_fields)]` turns a typo
//! into a named error by itself. What is left to this module by hand is
//! exactly what a type cannot express: which `SketchyBar` properties are real
//! but not modelled yet ([`ParseError::KnownGap`], checked against a short
//! list before deserializing), the bare `icon=`/`label=` sugar, and telling a
//! `/pattern/` bulk selector from a literal name by shape.

use lexopt::{Arg, Parser, ValueExt};
use rsbar_protocol::event::Custom;
use rsbar_protocol::{
    BarPatch, ComponentKind, Event, ItemName, ItemPatch, Json, Kind, Position, Query, Relative,
    Request, Selector,
};

use super::args::{self, ArgsError};

/// A `SketchyBar` command line could not be turned into requests.
///
/// Every variant names the offending token, so a config typo produces a
/// message that points at it rather than a generic "invalid argument".
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ParseError {
    #[error(
        "`{0}` is not a recognised command: expected one of --bar, --set, --add, --remove, \
         --subscribe, --trigger, --query, --default, --move, --reorder, --reload, --update, \
         --exit"
    )]
    UnknownDomain(String),

    #[error("`{domain}` needs {expected}")]
    MissingArgument {
        domain: &'static str,
        expected: &'static str,
    },

    #[error("`{0}` is not `key=value`: expected e.g. `label.color=0xffffffff`")]
    NotKeyValue(String),

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

    #[error("`{0}` is not `item`, `bracket`, `alias`, `space`, `graph` or `slider`")]
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

    /// From [`super::args`]'s deserializer: a bad value for a property whose
    /// key it already knows, since it walked the same dotted path this
    /// module built.
    #[error("{0}")]
    Args(String),

    /// `lexopt`'s own errors: a value that is not valid Unicode, or one asked
    /// for where none remains. `lexopt::Error` is neither `Clone` nor
    /// `PartialEq`, so its message is captured rather than the error itself.
    #[error("{0}")]
    Lex(String),
}

impl From<ArgsError> for ParseError {
    fn from(err: ArgsError) -> Self {
        Self::Args(err.to_string())
    }
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
    "default",
    "move",
    "reorder",
    "push",
    "reload",
    "update",
    "exit",
    "press",
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

fn required_selector(
    parser: &mut Parser,
    domain: &'static str,
    expected: &'static str,
) -> Result<Selector, ParseError> {
    Ok(required_string(parser, domain, expected)?.parse()?)
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

fn parse_position_value(key: &str, value: &str) -> Result<Position, ParseError> {
    value
        .parse()
        .map_err(|e: rsbar_protocol::InvalidPosition| invalid(key, value, e))
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

/// The bare `icon=`/`label=` sugar rewritten into its dotted form before a
/// token list becomes the `(path, value)` pairs [`super::args::from_pairs`]
/// deserializes: a config's own `icon=🔋` sets the same field as
/// `icon.string=🔋`/`icon.text=🔋`, and neither [`ItemPatch`] nor `serde` has
/// anywhere else to put that alias, since `icon` itself names a nested
/// component, not a string.
fn rewrite_bare_sugar(key: &str) -> &str {
    match key {
        "icon" => "icon.text",
        "label" => "label.text",
        other => other,
    }
}

fn pairs_from_tokens(tokens: &[String]) -> Result<Vec<(String, String)>, ParseError> {
    tokens
        .iter()
        .map(|token| {
            let (key, value) = split_kv(token)?;
            Ok((rewrite_bare_sugar(key).to_owned(), value.to_owned()))
        })
        .collect()
}

/// A real `SketchyBar` property with no home on the target patch struct yet.
/// `(dotted key, reason)`. Checked before deserializing, so these still get a
/// [`ParseError::KnownGap`] naming the struct rather than `serde`'s generic
/// "unknown field" from `#[serde(deny_unknown_fields)]` — that message is
/// exactly right for a typo, but wrong for a property `SketchyBar` really
/// has.
fn known_gap(pairs: &[(String, String)], gaps: &[(&str, &'static str)]) -> Result<(), ParseError> {
    for (key, _) in pairs {
        if let Some((_, reason)) = gaps.iter().find(|(gap, _)| gap == key) {
            return Err(ParseError::KnownGap(key.clone(), reason));
        }
    }
    Ok(())
}

const BAR_GAPS: &[(&str, &str)] = &[
    ("space", "no matching BarPatch field yet"),
    ("font_smoothing", "no matching BarPatch field yet"),
    ("shadow", "no matching BarPatch field yet"),
    ("align", "no matching BarPatch field yet"),
    ("horizontal", "no matching BarPatch field yet"),
    ("border_color", "no matching BarPatch field yet"),
    ("border_width", "no matching BarPatch field yet"),
    ("x_offset", "no matching BarPatch field yet"),
    ("image", "no matching BarPatch field yet"),
    ("clip", "no matching BarPatch field yet"),
    ("drawing", "no matching BarPatch field yet"),
];

const ITEM_GAPS: &[(&str, &str)] = &[
    ("scroll_texts", "no matching ItemPatch field yet"),
    ("align", "no matching ItemPatch field yet"),
    ("associated_display", "no matching ItemPatch field yet"),
    ("blur_radius", "no matching ItemPatch field yet"),
    ("shadow", "no matching ItemPatch field yet"),
    ("lazy", "no matching ItemPatch field yet"),
    ("cache_scripts", "no matching ItemPatch field yet"),
    ("ignore_association", "no matching ItemPatch field yet"),
    ("max_chars", "no matching ItemPatch field yet"),
    ("icon.highlight", "no matching RunPatch field yet"),
    ("icon.highlight_color", "no matching RunPatch field yet"),
    ("icon.scroll_duration", "no matching RunPatch field yet"),
    ("icon.width", "no matching RunPatch field yet"),
    ("icon.align", "no matching RunPatch field yet"),
    ("icon.max_chars", "no matching RunPatch field yet"),
    ("label.highlight", "no matching RunPatch field yet"),
    ("label.highlight_color", "no matching RunPatch field yet"),
    ("label.scroll_duration", "no matching RunPatch field yet"),
    ("label.width", "no matching RunPatch field yet"),
    ("label.align", "no matching RunPatch field yet"),
    ("label.max_chars", "no matching RunPatch field yet"),
    (
        "background.drawing",
        "no matching BackgroundPatch field yet",
    ),
    ("background.clip", "no matching BackgroundPatch field yet"),
    (
        "background.x_offset",
        "no matching BackgroundPatch field yet",
    ),
    ("background.image", "no matching BackgroundPatch field yet"),
    (
        "popup",
        "no matching ItemPatch field yet: popups are not modelled",
    ),
    (
        "slider",
        "no matching ItemPatch field yet: no matching component",
    ),
    (
        "knob",
        "no matching ItemPatch field yet: no matching component",
    ),
    (
        "graph",
        "no matching ItemPatch field yet: no matching component",
    ),
    (
        "image",
        "no matching ItemPatch field yet: no matching component",
    ),
    ("alias.pid", "no matching ItemPatch field yet"),
];

/// `--add item|bracket|alias|space|graph|slider ...`. See `DOMAIN_ADD` in
/// `defines.h`.
///
/// `--add alias <name> <position>` is treated exactly like `--add item`: in
/// `SketchyBar` the argument to `--add alias` *is* the mirror target, but in
/// rsbar the target is the `alias=Owner,Name` property already on
/// [`ItemPatch`] (`Owner,Name` cannot be a valid [`ItemName`] — it has a comma
/// and usually spaces). So the name given here is the new item's own name,
/// and the target is set the way any other property is: chained, with
/// `--set <name> alias="Owner,Name"` right after.
fn parse_add(parser: &mut Parser) -> Result<Vec<Request>, ParseError> {
    let kind = required_string(
        parser,
        "--add",
        "item, bracket, alias, space, graph or slider",
    )?;
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
            let kind = if kind == "alias" {
                ComponentKind::Alias { name, position }
            } else {
                ComponentKind::Item { name, position }
            };
            Ok(vec![Request::Add(kind)])
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
                .map(|m| m.parse::<Selector>().map_err(ParseError::from))
                .collect::<Result<Vec<_>, _>>()?;
            // SketchyBar's own `--add bracket` takes no position: a bracket's
            // frame comes from its members, which is why the kind carries them
            // instead.
            Ok(vec![Request::Add(ComponentKind::Bracket { name, members })])
        }
        "space" | "graph" | "slider" => {
            let domain: &'static str = "--add";
            let name = required_item_name(parser, domain, "a name")?;
            let position_token = required_string(parser, domain, "a position")?;
            let position = parse_position_value("position", &position_token)?;
            let component_kind = match kind.as_str() {
                "space" => ComponentKind::Space { name, position },
                "graph" => ComponentKind::Graph { name, position },
                "slider" => ComponentKind::Slider { name, position },
                _ => unreachable!("matched above"),
            };
            // `--add graph <name> <position> <width>` and
            // `--add slider <name> <position> <width>` carry an extra
            // positional width rsbar has nowhere to put yet, since nothing on
            // the daemon side can draw either component. Consumed and
            // dropped rather than left for the next domain to trip over.
            let _ = greedy_strings(parser)?;
            Ok(vec![Request::Add(component_kind)])
        }
        "event" => Err(ParseError::KnownGap(
            kind,
            "custom events need no registration in rsbar: --trigger accepts any name directly",
        )),
        _ => Err(ParseError::UnknownAddKind(kind)),
    }
}

fn parse_bar(parser: &mut Parser) -> Result<Request, ParseError> {
    let pairs = pairs_from_tokens(&greedy_strings(parser)?)?;
    known_gap(&pairs, BAR_GAPS)?;
    let patch: BarPatch = args::from_pairs(&pairs)?;
    Ok(Request::SetBar(patch))
}

fn parse_item_patch(pairs: &[(String, String)]) -> Result<ItemPatch, ParseError> {
    known_gap(pairs, ITEM_GAPS)?;
    Ok(args::from_pairs(pairs)?)
}

fn parse_set(parser: &mut Parser) -> Result<Request, ParseError> {
    let selector = required_selector(parser, "--set", "an item name")?;
    let pairs = pairs_from_tokens(&greedy_strings(parser)?)?;
    let patch = Box::new(parse_item_patch(&pairs)?);
    Ok(Request::Set(selector, patch))
}

fn parse_default(parser: &mut Parser) -> Result<Request, ParseError> {
    let pairs = pairs_from_tokens(&greedy_strings(parser)?)?;
    let patch = parse_item_patch(&pairs)?;
    Ok(Request::SetDefault(Box::new(patch)))
}

fn parse_remove(parser: &mut Parser) -> Result<Request, ParseError> {
    let selector = required_selector(parser, "--remove", "an item name")?;
    Ok(Request::Remove(selector))
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
    let token = required_string(
        parser,
        "--query",
        "bar, items, menu-items, app-menus, defaults or an item name",
    )?;
    let query = match token.as_str() {
        "bar" => Query::Bar,
        "items" => Query::Items,
        "menu-items" | "menu_items" | "default_menu_items" => Query::MenuItems,
        "app-menus" | "app_menus" => Query::AppMenus,
        "defaults" => Query::Defaults,
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

/// `--press <index>` opens one of the frontmost application's own menus;
/// `--press <name>` opens the real menu behind a mirrored item. The two are
/// told apart by shape, since an index is never a valid item name in the
/// configs that use this — it is what the config's `menus -s <n>` helper took.
fn parse_press(parser: &mut Parser) -> Result<Request, ParseError> {
    let token = required_string(parser, "--press", "a menu index or an alias item name")?;
    if let Ok(index) = token.parse::<usize>() {
        return Ok(Request::Press(rsbar_protocol::PressTarget::AppMenu(index)));
    }
    Ok(Request::Press(rsbar_protocol::PressTarget::Alias(
        ItemName::new(token.as_str())?,
    )))
}

fn parse_move(parser: &mut Parser) -> Result<Request, ParseError> {
    let name = required_item_name(parser, "--move", "an item name")?;
    let relative_token = required_string(parser, "--move", "before or after")?;
    let relative = match relative_token.as_str() {
        "before" => Relative::Before,
        "after" => Relative::After,
        _ => {
            return Err(invalid(
                "direction",
                &relative_token,
                "expected before or after",
            ));
        }
    };
    let reference = required_item_name(parser, "--move", "a reference item name")?;
    Ok(Request::Move {
        name,
        relative,
        reference,
    })
}

fn parse_reorder(parser: &mut Parser) -> Result<Request, ParseError> {
    let tokens = greedy_strings(parser)?;
    if tokens.is_empty() {
        return Err(ParseError::MissingArgument {
            domain: "--reorder",
            expected: "at least one item name",
        });
    }
    let names = tokens
        .iter()
        .map(|n| ItemName::new(n.as_str()).map_err(ParseError::from))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Request::Reorder(names))
}

/// `--push <name> <value>`: one more sample for a graph.
fn parse_push(parser: &mut Parser) -> Result<Request, ParseError> {
    let name = match required_selector(parser, "--push", "a graph's name")? {
        Selector::Name(name) => name,
        // A sample belongs to one graph's ring buffer; pushing the same value
        // into every graph matching a pattern is not a thing SketchyBar does.
        Selector::Pattern(pattern) => {
            return Err(ParseError::KnownGap(
                format!("/{pattern}/"),
                "--push takes one graph, not a pattern",
            ));
        }
    };
    let value = required_string(parser, "--push", "a sample value")?;
    let value = value
        .parse::<f32>()
        .map_err(|e| invalid("--push", &value, e))?;
    Ok(Request::Push { name, value })
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
            Arg::Long("default") => requests.push(parse_default(&mut parser)?),
            Arg::Long("move") => requests.push(parse_move(&mut parser)?),
            Arg::Long("reorder") => requests.push(parse_reorder(&mut parser)?),
            Arg::Long("push") => requests.push(parse_push(&mut parser)?),
            Arg::Long("press") => requests.push(parse_press(&mut parser)?),
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
    use rsbar_protocol::{BackgroundPatch, Color, FontSpec, RunPatch};

    fn args_(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|s| (*s).to_owned()).collect()
    }

    fn name(s: &str) -> ItemName {
        ItemName::new(s).unwrap()
    }

    fn sel(s: &str) -> Selector {
        s.parse().unwrap()
    }

    #[test]
    fn bar_properties_apply_in_one_request() {
        let requests = parse(&args_(&["--bar", "height=32", "color=0xff000000"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::SetBar(BarPatch {
                height: Some(32.0),
                color: Some(Color(0xff00_0000)),
                ..Default::default()
            })]
        );
    }

    #[test]
    fn hash_colours_still_parse() {
        let requests = parse(&args_(&["--bar", "color=#ff0000"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::SetBar(BarPatch {
                color: Some(Color(0xffff_0000)),
                ..Default::default()
            })]
        );
    }

    #[test]
    fn dotted_item_properties_map_onto_the_matching_component() {
        let requests = parse(&args_(&[
            "--set",
            "clock",
            "label.color=0xffffffff",
            "icon.font=Hack:Bold:14",
            "background.corner_radius=6",
        ]))
        .unwrap();
        assert_eq!(
            requests,
            vec![Request::Set(
                Selector::Name(name("clock")),
                Box::new(ItemPatch {
                    label: Some(RunPatch {
                        color: Some(Color(0xffff_ffff)),
                        ..Default::default()
                    }),
                    icon: Some(RunPatch {
                        font: Some(FontSpec::parse("Hack:Bold:14")),
                        ..Default::default()
                    }),
                    background: Some(BackgroundPatch {
                        corner_radius: Some(6.0),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            )]
        );
    }

    #[test]
    fn a_bare_icon_or_label_is_sugar_for_its_text_field() {
        let requests = parse(&args_(&["--set", "clock", "icon=🕐", "label=09:41"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::Set(
                Selector::Name(name("clock")),
                Box::new(ItemPatch {
                    icon: Some(RunPatch {
                        text: Some("🕐".into()),
                        ..Default::default()
                    }),
                    label: Some(RunPatch {
                        text: Some("09:41".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            )]
        );
    }

    #[test]
    fn sketchybars_string_spelling_is_accepted_alongside_text() {
        let requests = parse(&args_(&["--set", "clock", "icon.string=🕐"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::Set(
                Selector::Name(name("clock")),
                Box::new(ItemPatch {
                    icon: Some(RunPatch {
                        text: Some("🕐".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            )]
        );
    }

    #[test]
    fn icon_padding_is_expressible_now_that_it_is_its_own_component() {
        let requests = parse(&args_(&["--set", "clock", "icon.padding_left=4"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::Set(
                Selector::Name(name("clock")),
                Box::new(ItemPatch {
                    icon: Some(RunPatch {
                        padding_left: Some(4.0),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            )]
        );
    }

    #[test]
    fn background_height_and_border_are_expressible_now_too() {
        let requests = parse(&args_(&[
            "--set",
            "clock",
            "background.height=20",
            "background.border_color=0xff00ff00",
            "background.border_width=2",
        ]))
        .unwrap();
        assert_eq!(
            requests,
            vec![Request::Set(
                Selector::Name(name("clock")),
                Box::new(ItemPatch {
                    background: Some(BackgroundPatch {
                        height: Some(20.0),
                        border_color: Some(Color(0xff00_ff00)),
                        border_width: Some(2.0),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            )]
        );
    }

    #[test]
    fn fields_added_to_the_patch_types_need_no_grammar_change() {
        // `updates`, `width` and `display` used to be `ParseError::KnownGap`
        // entries here, restating a field list `ItemPatch` already owns —
        // exactly the drift deriving from the type is meant to remove. They
        // are real fields now, and nothing in this module named them.
        let requests = parse(&args_(&[
            "--set",
            "clock",
            "updates=off",
            "width=40",
            "display=2",
        ]))
        .unwrap();
        assert_eq!(
            requests,
            vec![Request::Set(
                Selector::Name(name("clock")),
                Box::new(ItemPatch {
                    updates: Some(false),
                    width: Some(40.0),
                    display: Some("2".into()),
                    ..Default::default()
                })
            )]
        );
    }

    #[test]
    fn several_domains_chain_in_one_invocation() {
        let requests = parse(&args_(&[
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
                Request::Add(ComponentKind::Item {
                    name: name("clock"),
                    position: Position::Right
                }),
                Request::Set(
                    Selector::Name(name("clock")),
                    Box::new(ItemPatch {
                        update_freq: Some(1),
                        script: Some("/plugins/clock.sh".into()),
                        ..Default::default()
                    })
                ),
                Request::Add(ComponentKind::Item {
                    name: name("battery"),
                    position: Position::Right
                }),
                Request::Set(
                    Selector::Name(name("battery")),
                    Box::new(ItemPatch {
                        script: Some("/plugins/battery.sh".into()),
                        ..Default::default()
                    })
                ),
                Request::Subscribe {
                    name: name("battery"),
                    events: vec![Kind::PowerSourceChanged],
                },
            ]
        );
    }

    #[test]
    fn add_bracket_carries_its_members() {
        let requests = parse(&args_(&[
            "--add",
            "bracket",
            "group",
            "left_item",
            "right_item",
        ]))
        .unwrap();
        assert_eq!(
            requests,
            vec![Request::Add(ComponentKind::Bracket {
                name: name("group"),
                members: vec![sel("left_item"), sel("right_item")],
            })]
        );
    }

    #[test]
    fn add_bracket_accepts_a_pattern_member() {
        // ~/dendritic/sketchybar/items/menus.lua:
        // sbar.add("bracket", { "/menu\\..*/" }, { ... })
        let requests = parse(&args_(&["--add", "bracket", "group", r"/menu\..*/"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::Add(ComponentKind::Bracket {
                name: name("group"),
                members: vec![sel(r"/menu\..*/")],
            })]
        );
    }

    #[test]
    fn add_alias_is_its_own_kind_and_takes_a_chained_set() {
        let requests = parse(&args_(&[
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
                Request::Add(ComponentKind::Alias {
                    name: name("mirror"),
                    position: Position::Right,
                }),
                Request::Set(
                    Selector::Name(name("mirror")),
                    Box::new(ItemPatch {
                        alias: Some("Control Center,Battery".into()),
                        ..Default::default()
                    })
                ),
            ]
        );
    }

    #[test]
    fn add_accepts_component_kinds_it_cannot_yet_draw() {
        let requests = parse(&args_(&["--add", "graph", "cpu", "right", "50"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::Add(ComponentKind::Graph {
                name: name("cpu"),
                position: Position::Right
            })]
        );

        let requests = parse(&args_(&["--add", "space", "space.1", "left"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::Add(ComponentKind::Space {
                name: name("space.1"),
                position: Position::Left
            })]
        );
    }

    #[test]
    fn subscribe_accepts_sketchybars_shorter_event_spelling() {
        let requests = parse(&args_(&[
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
        let requests = parse(&args_(&["--trigger", "power_source_changed"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::Trigger(Event::PowerSourceChanged(
                PowerChange::default()
            ))]
        );
    }

    #[test]
    fn trigger_on_a_custom_event_collects_key_value_pairs_into_one_object() {
        let requests = parse(&args_(&["--trigger", "my.event", "foo=1", "bar=hello"])).unwrap();
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
        let err = parse(&args_(&["--trigger", "volume_changed", "level=5"])).unwrap_err();
        assert_eq!(
            err,
            ParseError::BuiltinEventHasNoPayload("volume_changed".into())
        );
    }

    #[test]
    fn query_keywords_and_item_names_are_told_apart() {
        assert_eq!(
            parse(&args_(&["--query", "bar"])).unwrap(),
            vec![Request::Query(Query::Bar)]
        );
        assert_eq!(
            parse(&args_(&["--query", "items"])).unwrap(),
            vec![Request::Query(Query::Items)]
        );
        assert_eq!(
            parse(&args_(&["--query", "menu-items"])).unwrap(),
            vec![Request::Query(Query::MenuItems)]
        );
        assert_eq!(
            parse(&args_(&["--query", "clock"])).unwrap(),
            vec![Request::Query(Query::Item(name("clock")))]
        );
        assert_eq!(
            parse(&args_(&["--query", "app-menus"])).unwrap(),
            vec![Request::Query(Query::AppMenus)]
        );
        assert_eq!(
            parse(&args_(&["--query", "defaults"])).unwrap(),
            vec![Request::Query(Query::Defaults)]
        );
    }

    #[test]
    fn press_tells_a_menu_index_from_an_alias_name() {
        // The Apple menu is index 0, which is what the config's click script
        // passed to its helper.
        assert_eq!(
            parse(&args_(&["--press", "0"])).unwrap(),
            vec![Request::Press(rsbar_protocol::PressTarget::AppMenu(0))]
        );
        assert_eq!(
            parse(&args_(&["--press", "Amphetamine,Amphetamine"])).unwrap(),
            vec![Request::Press(rsbar_protocol::PressTarget::Alias(name(
                "Amphetamine,Amphetamine"
            )))]
        );
    }

    #[test]
    fn remove_reload_update_and_exit() {
        assert_eq!(
            parse(&args_(&["--remove", "clock"])).unwrap(),
            vec![Request::Remove(Selector::Name(name("clock")))]
        );
        assert_eq!(parse(&args_(&["--reload"])).unwrap(), vec![Request::Reload]);
        assert_eq!(
            parse(&args_(&["--update"])).unwrap(),
            vec![Request::UpdateAll]
        );
        assert_eq!(parse(&args_(&["--exit"])).unwrap(), vec![Request::Shutdown]);
    }

    #[test]
    fn set_on_a_pattern_is_a_bulk_request_the_daemon_resolves() {
        // ~/dendritic/sketchybar/items/menus.lua:
        // sbar.set("/menu\\..*/", { drawing = false })
        let requests = parse(&args_(&["--set", r"/menu\..*/", "drawing=off"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::Set(
                Selector::Pattern(r"menu\..*".into()),
                Box::new(ItemPatch {
                    drawing: Some(rsbar_protocol::Toggle::Off),
                    ..Default::default()
                })
            )]
        );
    }

    #[test]
    fn remove_on_a_pattern_is_now_implemented_rather_than_a_gap() {
        let requests = parse(&args_(&["--remove", "/clock.*/"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::Remove(Selector::Pattern("clock.*".into()))]
        );
    }

    #[test]
    fn default_stashes_only_the_properties_it_was_given() {
        // ~/dendritic/sketchybar/default.lua sets icon/label fonts and
        // colours, and padding — but never every ItemPatch field, and the
        // daemon's damage tracking depends on that staying true here too.
        let requests = parse(&args_(&[
            "--default",
            "padding_left=5",
            "icon.color=0xffffffff",
        ]))
        .unwrap();
        assert_eq!(
            requests,
            vec![Request::SetDefault(Box::new(ItemPatch {
                padding_left: Some(5.0),
                icon: Some(RunPatch {
                    color: Some(Color(0xffff_ffff)),
                    ..Default::default()
                }),
                ..Default::default()
            }))]
        );
    }

    #[test]
    fn move_reorders_relative_to_a_reference_item() {
        // ~/dendritic/sketchybar/items/left.lua:
        // sketchybar --move chevron after <space>
        let requests = parse(&args_(&["--move", "chevron", "after", "space.1"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::Move {
                name: name("chevron"),
                relative: Relative::After,
                reference: name("space.1"),
            }]
        );
    }

    #[test]
    fn move_rejects_a_direction_that_is_neither_before_nor_after() {
        let err = parse(&args_(&["--move", "chevron", "sideways", "space.1"])).unwrap_err();
        assert!(matches!(
            err,
            ParseError::InvalidValue { key, value, .. }
                if key == "direction" && value == "sideways"
        ));
    }

    #[test]
    fn reorder_takes_the_bars_new_left_to_right_order() {
        let requests = parse(&args_(&["--reorder", "front_app", "space.1", "clock"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::Reorder(vec![
                name("front_app"),
                name("space.1"),
                name("clock"),
            ])]
        );
    }

    #[test]
    fn an_unrecognised_domain_names_itself() {
        let err = parse(&args_(&["--nope", "clock"])).unwrap_err();
        assert_eq!(err, ParseError::UnknownDomain("--nope".into()));
    }

    #[test]
    fn a_missing_item_name_is_not_silently_taken_from_the_next_domain() {
        // Without the flag-shaped guard, `lexopt`'s `value()` would happily
        // hand back "--bar" as if it were the item name (it is valid
        // `ItemName` syntax) and swallow the whole next domain with it.
        let err = parse(&args_(&["--set", "--bar", "height=1"])).unwrap_err();
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
        let requests = parse(&args_(&["--set", "clock", "y_offset=-5"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::Set(
                Selector::Name(name("clock")),
                Box::new(ItemPatch {
                    y_offset: Some(-5.0),
                    ..Default::default()
                })
            )]
        );
    }

    #[test]
    fn a_missing_key_value_pair_names_the_bad_token() {
        let err = parse(&args_(&["--set", "clock", "label"])).unwrap_err();
        assert_eq!(err, ParseError::NotKeyValue("label".into()));
    }

    #[test]
    fn an_unknown_item_property_names_itself() {
        let err = parse(&args_(&["--set", "clock", "wat=1"])).unwrap_err();
        assert!(matches!(err, ParseError::Args(msg) if msg.contains("wat")));
    }

    #[test]
    fn an_unknown_subdomain_is_an_unknown_property_not_a_known_gap() {
        let err = parse(&args_(&["--set", "clock", "wat.color=1"])).unwrap_err();
        assert!(matches!(err, ParseError::Args(msg) if msg.contains("wat")));
    }

    #[test]
    fn a_real_but_unmapped_property_is_a_known_gap() {
        let err = parse(&args_(&[
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
        let err = parse(&args_(&["--bar", "color=not-a-colour"])).unwrap_err();
        assert!(matches!(err, ParseError::Args(msg) if msg.contains("color")));
    }

    #[test]
    fn reload_with_a_path_is_reported_rather_than_silently_dropped() {
        let err = parse(&args_(&["--reload", "/tmp/other.rc"])).unwrap_err();
        assert_eq!(err, ParseError::ReloadTakesNoPath("/tmp/other.rc".into()));
    }

    #[test]
    fn toggle_is_still_rejected_since_a_one_shot_cli_has_no_current_value() {
        let err = parse(&args_(&["--bar", "hidden=toggle"])).unwrap_err();
        assert!(matches!(err, ParseError::Args(msg) if msg.contains("hidden")));
    }
}

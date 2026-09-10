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
//! grammar change at all. A key no patch has a field for is named through
//! `tracing` and skipped by the patch itself, so a typo costs one property
//! rather than the whole command. What is left to this module by hand is
//! exactly what a type cannot express: which verb was written, and telling a
//! `/pattern/` bulk selector from a literal name by shape.

use crate::protocol::event::Custom;
use crate::protocol::{
    BarPatch, ComponentTag, Event, EventName, GeometryPatch, ItemName, ItemPatch, Kind,
    NotificationName, Position, Query, Relative, Request, Selector,
};
use lexopt::{Arg, Parser, ValueExt};

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

    /// A real `SketchyBar` *verb* coolabah does not implement — `--query
    /// events`, a `--push` to a pattern. Properties are not in here: an
    /// unknown one is the patch's own business, and it warns and carries on
    /// rather than failing the command.
    #[error("`{0}` names something real that coolabah does not do yet: {1}")]
    Unsupported(String, &'static str),

    #[error("`{value}` is not valid for `{key}`: {reason}")]
    InvalidValue {
        key: String,
        value: String,
        reason: String,
    },

    #[error(transparent)]
    InvalidName(#[from] crate::protocol::InvalidName),

    /// A property `--add` cannot do without and the command did not name —
    /// `--add item clock` with no position. The same error the Lua host
    /// raises for the same omission, since both ask the patch rather than
    /// deciding for themselves.
    #[error(transparent)]
    Missing(#[from] crate::protocol::Missing),

    #[error(transparent)]
    InvalidEventName(#[from] crate::protocol::event::InvalidEventName),

    #[error(transparent)]
    InvalidNotificationName(#[from] crate::protocol::event::InvalidNotificationName),

    #[error("`{0}` is not `item`, `bracket`, `alias`, `space`, `graph`, `slider` or `event`")]
    UnknownAddKind(String),

    #[error(
        "coolabah's `--reload` takes no path (unlike SketchyBar's): it re-runs the daemon's own \
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
        .map_err(|e: crate::protocol::InvalidPosition| invalid(key, value, e))
}

/// Accepts `SketchyBar`'s own event spelling (`volume_change`) alongside
/// coolabah's (`volume_changed`) by trying the bare name and then the same name
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

fn pairs_from_tokens(tokens: &[String]) -> Result<Vec<(String, String)>, ParseError> {
    tokens
        .iter()
        .map(|token| {
            let (key, value) = split_kv(token)?;
            Ok((key.to_owned(), value.to_owned()))
        })
        .collect()
}

/// `--add item|bracket|alias|space|graph|slider ...`. See `DOMAIN_ADD` in
/// `defines.h`.
///
/// `--add alias <name> <position>` is treated exactly like `--add item`: in
/// `SketchyBar` the argument to `--add alias` *is* the mirror target, but in
/// coolabah the target is the `alias=Owner,Name` property already on
/// [`ItemPatch`] (`Owner,Name` cannot be a valid [`ItemName`] — it has a comma
/// and usually spaces). So the name given here is the new item's own name,
/// and the target is set the way any other property is: chained, with
/// `--set <name> alias="Owner,Name"` right after.
///
/// The positional arguments become an [`ItemPatch`] and
/// [`ComponentTag::build`] decides whether that is enough, rather than this
/// module deciding for itself: `--add item foo` with no position and
/// `coolabah.add("item", "foo", {})` are the same omission and now get the same
/// sentence. What a bare `--add` may leave out is [`Geometry`]'s own
/// `#[changes(required)]`, not a rule spelled here.
///
/// [`Geometry`]: crate::protocol::Geometry
fn parse_add(parser: &mut Parser) -> Result<Vec<Request>, ParseError> {
    let word = required_string(
        parser,
        "--add",
        "item, bracket, alias, space, graph, slider or event",
    )?;
    if word == "event" {
        return parse_add_event(parser).map(|request| vec![request]);
    }
    let tag: ComponentTag = word
        .parse()
        .map_err(|_| ParseError::UnknownAddKind(word.clone()))?;
    let name = required_item_name(parser, "--add", "a name")?;
    // Everything up to the next domain flag: a position, or a bracket's
    // members. `--add graph <name> <position> <width>` and `--add slider`
    // carry a trailing width coolabah has nowhere to put yet, since nothing on
    // the daemon side draws either; it is consumed here rather than left for
    // the next domain to trip over.
    let tail = greedy_strings(parser)?;
    let mut patch = ItemPatch::default();
    if tag == ComponentTag::Bracket {
        // SketchyBar's own `--add bracket` takes no position: a bracket's
        // frame comes from its members, which is why the patch carries them
        // instead.
        patch.members = Some(
            tail.iter()
                .map(|member| member.parse::<Selector>().map_err(ParseError::from))
                .collect::<Result<Vec<_>, _>>()?,
        );
    } else if let Some(token) = tail.first() {
        patch.geometry = Some(GeometryPatch {
            position: Some(parse_position_value("position", token)?),
            ..Default::default()
        });
    }
    Ok(vec![Request::Add(tag.build(name, &patch)?)])
}

/// `--add event <name> [<NSDistributedNotificationName>]`.
///
/// The name goes through [`parse_event_name`] first, so `--add event
/// volume_change` is refused as the built-in it names rather than quietly
/// declaring a second event spelled almost like one. A second positional is
/// the distributed notification to bridge from; without it the event is one
/// only `--trigger` ever fires.
fn parse_add_event(parser: &mut Parser) -> Result<Request, ParseError> {
    let token = required_string(parser, "--add event", "an event name")?;
    let name: EventName = parse_event_name(&token)?.name().parse()?;
    let mut rest = greedy_strings(parser)?.into_iter();
    let notification = rest
        .next()
        .map(|n| n.parse::<NotificationName>())
        .transpose()?;
    if let Some(extra) = rest.next() {
        return Err(invalid(
            "--add event",
            &extra,
            "an event takes a name and at most one notification to bridge it from",
        ));
    }
    Ok(Request::AddEvent { name, notification })
}

fn parse_bar(parser: &mut Parser) -> Result<Request, ParseError> {
    let pairs = pairs_from_tokens(&greedy_strings(parser)?)?;
    let patch: BarPatch = args::from_pairs(&pairs)?;
    Ok(Request::SetBar(patch))
}

fn parse_item_patch(pairs: &[(String, String)]) -> Result<ItemPatch, ParseError> {
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
        let mut vars = std::collections::BTreeMap::new();
        for token in &group {
            let (key, value) = split_kv(token)?;
            vars.insert(key.to_owned(), value.to_owned());
        }
        Event::Custom(Custom {
            name: kind.name().to_owned(),
            vars,
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
            return Err(ParseError::Unsupported(
                token,
                "coolabah does not track registered custom events",
            ));
        }
        "displays" => {
            return Err(ParseError::Unsupported(
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
        return Ok(Request::Press(crate::protocol::PressTarget::AppMenu(index)));
    }
    Ok(Request::Press(crate::protocol::PressTarget::Alias(
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
            return Err(ParseError::Unsupported(
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
    use crate::protocol::event::PowerChange;
    use crate::protocol::{
        BackgroundPatch, Color, ComponentKind, FontSpec, GeometryPatch, RunPatch, ScriptingPatch,
    };

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
                    geometry: Some(GeometryPatch {
                        background: Some(BackgroundPatch {
                            corner_radius: Some(6.0),
                            ..Default::default()
                        }),
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
                    geometry: Some(GeometryPatch {
                        background: Some(BackgroundPatch {
                            height: Some(20.0),
                            border_color: Some(Color(0xff00_ff00)),
                            border_width: Some(2.0),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            )]
        );
    }

    #[test]
    fn fields_added_to_the_patch_types_need_no_grammar_change() {
        // Nothing in this module names `updates`, `width` or `display`: they
        // reach the patch by deriving from `ItemPatch`'s own field list.
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
                    scripting: Some(ScriptingPatch {
                        updates: Some(crate::protocol::BoolChange::False),
                        ..Default::default()
                    }),
                    geometry: Some(GeometryPatch {
                        width: Some(40.0),
                        display: Some("2".parse().unwrap()),
                        ..Default::default()
                    }),
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
                        scripting: Some(ScriptingPatch {
                            update_freq: Some(1),
                            script: Some("/plugins/clock.sh".into()),
                            ..Default::default()
                        }),
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
                        scripting: Some(ScriptingPatch {
                            script: Some("/plugins/battery.sh".into()),
                            ..Default::default()
                        }),
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
    fn adding_without_a_position_says_which_property_is_missing() {
        // The requirement lives on the state type, so the CLI and the Lua
        // host ask the same patch about it rather than each deciding for
        // itself.
        let err = parse(&args_(&["--add", "item", "clock"])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "`position`: required to build an item, and not set"
        );
        // And the next domain is not eaten looking for one.
        let err = parse(&args_(&[
            "--add", "item", "clock", "--set", "clock", "label=x",
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("position"), "{err}");
    }

    #[test]
    fn a_bracket_with_no_members_says_so_in_the_same_words() {
        let err = parse(&args_(&["--add", "bracket", "group"])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "`members`: required to build an item, and not set"
        );
    }

    #[test]
    fn a_bare_value_and_a_nested_one_for_the_same_property_are_one_set() {
        // The command that failed live:
        //   coolabah --set clock label=12:34 icon=T label.color=0xff00ff00
        let requests = parse(&args_(&[
            "--set",
            "clock",
            "label=12:34",
            "icon=T",
            "label.color=0xff00ff00",
        ]))
        .unwrap();
        assert_eq!(
            requests,
            vec![Request::Set(
                Selector::Name(name("clock")),
                Box::new(ItemPatch {
                    label: Some(RunPatch {
                        text: Some("12:34".into()),
                        color: Some(Color(0xff00_ff00)),
                        ..Default::default()
                    }),
                    icon: Some(RunPatch {
                        text: Some("T".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            )]
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
    fn trigger_on_a_custom_event_carries_its_variables_by_name() {
        // SketchyBar: `--trigger demo VAR=Test` sets `$VAR` for the script.
        let requests = parse(&args_(&["--trigger", "demo", "VAR=Test", "OTHER=2"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::Trigger(Event::Custom(Custom {
                name: "demo".into(),
                vars: std::collections::BTreeMap::from([
                    ("VAR".to_owned(), "Test".to_owned()),
                    ("OTHER".to_owned(), "2".to_owned()),
                ]),
            }))]
        );
    }

    #[test]
    fn add_event_declares_an_event_the_config_fires_itself() {
        let requests = parse(&args_(&["--add", "event", "demo"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::AddEvent {
                name: "demo".parse().unwrap(),
                notification: None,
            }]
        );
    }

    #[test]
    fn add_event_with_a_notification_bridges_one_the_system_fires() {
        let requests = parse(&args_(&["--add", "event", "demo", "com.example.thing"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::AddEvent {
                name: "demo".parse().unwrap(),
                notification: Some("com.example.thing".parse().unwrap()),
            }]
        );
    }

    #[test]
    fn add_event_refuses_a_builtin_name() {
        // Including SketchyBar's own shorter spelling, which would otherwise
        // become a second event named almost like the real one.
        for token in ["volume_changed", "volume_change"] {
            let err = parse(&args_(&["--add", "event", token])).unwrap_err();
            assert_eq!(
                err,
                ParseError::InvalidEventName(crate::protocol::event::InvalidEventName::BuiltIn(
                    "volume_changed".into()
                )),
                "{token}"
            );
        }
    }

    #[test]
    fn add_event_needs_a_name() {
        let err = parse(&args_(&["--add", "event"])).unwrap_err();
        assert_eq!(
            err,
            ParseError::MissingArgument {
                domain: "--add event",
                expected: "an event name",
            }
        );
    }

    #[test]
    fn add_event_takes_at_most_one_notification() {
        let err = parse(&args_(&[
            "--add",
            "event",
            "demo",
            "com.example.thing",
            "com.example.other",
        ]))
        .unwrap_err();
        assert!(
            matches!(&err, ParseError::InvalidValue { value, .. } if value == "com.example.other"),
            "{err}"
        );
    }

    #[test]
    fn trigger_without_variables_carries_none() {
        let requests = parse(&args_(&["--trigger", "demo"])).unwrap();
        assert_eq!(
            requests,
            vec![Request::Trigger(Event::Custom(Custom::new("demo")))]
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
            vec![Request::Press(crate::protocol::PressTarget::AppMenu(0))]
        );
        assert_eq!(
            parse(&args_(&["--press", "Amphetamine,Amphetamine"])).unwrap(),
            vec![Request::Press(crate::protocol::PressTarget::Alias(name(
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
                    geometry: Some(GeometryPatch {
                        drawing: Some(crate::protocol::BoolChange::False),
                        ..Default::default()
                    }),
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
                geometry: Some(GeometryPatch {
                    padding_left: Some(5.0),
                    ..Default::default()
                }),
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
                    geometry: Some(GeometryPatch {
                        y_offset: Some(-5.0),
                        ..Default::default()
                    }),
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
    fn an_unknown_property_costs_that_property_and_nothing_else() {
        // The patch names it through `tracing` and skips it; the command
        // still runs, so the forty properties that were right still apply.
        // A config being ported is the case this is for, and it is the same
        // answer `item:set{ wat = 1 }` gets from the same type.
        let requests = parse(&args_(&["--set", "clock", "wat=1", "y_offset=3"])).unwrap();
        let [Request::Set(_, patch)] = &requests[..] else {
            panic!("one --set, not {requests:?}");
        };
        assert_eq!(
            patch
                .geometry
                .as_ref()
                .and_then(|geometry| geometry.y_offset),
            Some(3.0)
        );
    }

    #[test]
    fn an_unknown_subdomain_is_skipped_the_same_way_a_bare_key_is() {
        let requests = parse(&args_(&["--set", "clock", "wat.color=1"])).unwrap();
        let [Request::Set(_, patch)] = &requests[..] else {
            panic!("one --set, not {requests:?}");
        };
        assert_eq!(**patch, ItemPatch::default());
    }

    #[test]
    fn a_real_but_unmapped_property_is_skipped_rather_than_listed_by_hand() {
        // A generic warning, not a hand-kept list of SketchyBar properties
        // coolabah has no field for: it cannot rot when a field is added.
        let requests = parse(&args_(&["--set", "clock", "scroll_texts=on"])).unwrap();
        let [Request::Set(_, patch)] = &requests[..] else {
            panic!("one --set, not {requests:?}");
        };
        assert_eq!(**patch, ItemPatch::default());
    }

    #[test]
    fn a_bad_colour_names_the_value_and_the_key() {
        let err = parse(&args_(&["--bar", "color=not-a-colour"])).unwrap_err();
        assert!(matches!(err, ParseError::Args(msg) if msg.contains("color")));
    }

    #[test]
    fn a_bad_value_names_its_dotted_property_the_way_a_lua_config_is_told() {
        // The same mistake through the other door reads the same:
        // `coolabah_lua::convert` prints this shape too, from the path
        // `serde_path_to_error` tracks over `mlua`'s bridge. A config being
        // ported is read by one person moving between the two, and a
        // property name they can search for is most of the fix.
        let err = parse(&args_(&["--set", "clock", "label.color=puce"])).unwrap_err();
        let ParseError::Args(message) = err else {
            panic!("a value error, not {err:?}");
        };
        assert!(message.starts_with("`label.color`: "), "{message}");
        assert!(message.contains("puce"), "{message}");
    }

    #[test]
    fn reload_with_a_path_is_reported_rather_than_silently_dropped() {
        let err = parse(&args_(&["--reload", "/tmp/other.rc"])).unwrap_err();
        assert_eq!(err, ParseError::ReloadTakesNoPath("/tmp/other.rc".into()));
    }

    #[test]
    fn toggle_is_carried_for_the_daemon_to_resolve() {
        // `hidden` is a `BoolChange`: the CLI carries the intent and the
        // daemon, which knows the current value, resolves it — the same way
        // `--set <item> drawing=toggle` works.
        let requests = parse(&args_(&["--bar", "hidden=toggle"])).unwrap();
        let [Request::SetBar(patch)] = requests.as_slice() else {
            panic!("expected one --bar request, got {requests:?}");
        };
        assert_eq!(patch.hidden, Some(crate::protocol::BoolChange::Toggle));
    }

    #[test]
    fn a_negated_boolean_is_accepted_wherever_a_boolean_is() {
        let requests = parse(&args_(&["--bar", "hidden=!on"])).unwrap();
        let [Request::SetBar(patch)] = requests.as_slice() else {
            panic!("expected one --bar request, got {requests:?}");
        };
        assert_eq!(patch.hidden, Some(crate::protocol::BoolChange::False));
    }
}

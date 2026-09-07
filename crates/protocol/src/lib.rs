//! The wire vocabulary shared by the rsbar daemon, its CLI and its Lua module.
//!
//! This crate is the protocol. Everything that crosses a process boundary is
//! defined here exactly once, so a client cannot drift from the daemon.

#![cfg(target_os = "macos")]

pub mod event;
pub mod json;
pub mod style;

pub use event::{Event, Kind, Modifiers, MouseButton, PowerSource};
pub use json::Json;
pub use style::{Color, FontSpec};

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// The bootstrap name the daemon claims and clients look up.
///
/// Overridable so a development build can run beside an installed one.
#[must_use]
pub fn service_name() -> String {
    std::env::var("RSBAR_SERVICE").unwrap_or_else(|_| "com.auscyber.rsbar".to_owned())
}

/// A boolean a config can also ask to flip.
///
/// `SketchyBar` accepts `drawing=toggle` wherever it accepts `on`/`off`, and
/// the user's own config binds a click to `popup.drawing=toggle`. A plain
/// `bool` cannot carry that: whether it ends up true or false depends on what
/// it already was, which only the daemon knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    On,
    Off,
    Flip,
}

impl Toggle {
    /// What this makes of a value that is currently `current`.
    #[must_use]
    pub fn resolve(self, current: bool) -> bool {
        match self {
            Self::On => true,
            Self::Off => false,
            Self::Flip => !current,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not on/off, true/false, yes/no, 1/0 or toggle")]
pub struct InvalidToggle(String);

impl FromStr for Toggle {
    type Err = InvalidToggle;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "on" | "true" | "yes" | "1" => Ok(Self::On),
            "off" | "false" | "no" | "0" => Ok(Self::Off),
            "toggle" => Ok(Self::Flip),
            _ => Err(InvalidToggle(s.to_owned())),
        }
    }
}

impl fmt::Display for Toggle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::On => "on",
            Self::Off => "off",
            Self::Flip => "toggle",
        })
    }
}

impl Serialize for Toggle {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Toggle {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Where the daemon puts a `sketchybar`-named link to itself.
///
/// A config's plugin scripts shell out to `sketchybar` by name to set the item
/// they were run for, and its `init.lua` finishes with `sketchybar --update`.
/// The daemon is that CLI, so it puts a link under this directory and anything
/// running a config's commands -- the daemon's own script runner, and the Lua
/// host, which is a separate process -- puts it in front of `PATH`. Agreed
/// here because it is a convention between the daemon and its clients, which
/// is what this crate is for.
#[must_use]
pub fn shim_dir() -> std::path::PathBuf {
    // SAFETY: `getuid` cannot fail and touches nothing.
    let uid = unsafe { libc::getuid() };
    std::env::temp_dir().join(format!("rsbar-{uid}-bin"))
}

/// `PATH` with [`shim_dir`] in front of it.
#[must_use]
pub fn shimmed_path() -> std::ffi::OsString {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let mut path = std::ffi::OsString::from(shim_dir());
    if !inherited.is_empty() {
        path.push(":");
        path.push(&inherited);
    }
    path
}

/// An item's identity. A newtype so an item name and a stray string cannot be
/// swapped for one another.
///
/// The string is shared rather than owned: a name is cloned constantly — into
/// the index, into every job, into every response — and none of those want a
/// copy of the bytes. Cloning this is a reference count.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ItemName(std::sync::Arc<str>);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidName {
    #[error("an item name cannot be empty")]
    Empty,
    #[error("`{0}` is not a valid item name: control characters are not allowed")]
    Character(String),
}

impl ItemName {
    /// An alias names the menu bar item it mirrors — `Control Centre,Clock` —
    /// so a name has to carry a comma and a space, and the character set is
    /// only as narrow as the daemon's own handling requires. A name reaches a
    /// script as the value of `RSBAR_NAME` and never as part of the command
    /// string, so a shell never re-parses one; what it cannot survive is a
    /// control character, which an env var cannot carry.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidName`] for an empty name or one containing a control
    /// character.
    pub fn new(name: impl Into<String>) -> Result<Self, InvalidName> {
        let name = name.into();
        if name.is_empty() {
            return Err(InvalidName::Empty);
        }
        if name.chars().any(char::is_control) {
            return Err(InvalidName::Character(name));
        }
        Ok(Self(name.into()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ItemName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ItemName {
    type Err = InvalidName;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// A `--set`/`--remove` target, or a bracket member: an exact name, or
/// `SketchyBar`'s own `/pattern/` shorthand for "every item whose name
/// matches this regex" (`REGEX_DELIMITER` in `message.c`).
///
/// Never resolved here or by any other client: only the daemon has a live
/// item list, so only it can turn a [`Selector::Pattern`] into the items it
/// names without racing a config that is still adding them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    Name(ItemName),
    /// The regex source, without the surrounding `/.../`.
    Pattern(String),
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name(name) => name.fmt(f),
            Self::Pattern(pattern) => write!(f, "/{pattern}/"),
        }
    }
}

impl FromStr for Selector {
    type Err = InvalidName;
    /// A pattern is told from a literal name by shape alone — leading and
    /// trailing `/` — never by charset, since [`ItemName`] permits almost
    /// anything a pattern could too.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() > 1 && s.starts_with('/') && s.ends_with('/') {
            Ok(Self::Pattern(s[1..s.len() - 1].to_owned()))
        } else {
            Ok(Self::Name(ItemName::new(s)?))
        }
    }
}

impl Serialize for Selector {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Selector {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Which edge of the display the bar occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Edge {
    #[default]
    Top,
    Bottom,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a bar position: expected top or bottom")]
pub struct InvalidEdge(String);

impl FromStr for Edge {
    type Err = InvalidEdge;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "top" => Ok(Self::Top),
            "bottom" => Ok(Self::Bottom),
            _ => Err(InvalidEdge(s.to_owned())),
        }
    }
}

impl fmt::Display for Edge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Top => "top",
            Self::Bottom => "bottom",
        })
    }
}

impl Serialize for Edge {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Through [`FromStr`] for the same reason [`Position`] is: the config's
/// spelling is the authority, not serde's derived one. Paired with a
/// `Serialize` that writes the same string, because the wire format is
/// `postcard`, which is not self-describing -- a derived enum there is a
/// variant index, and reading that back as a string decodes nothing.
impl<'de> Deserialize<'de> for Edge {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Where an item sits along the bar.
///
/// The two centre-adjacent buckets exist so a config can put something beside
/// a centred item without it being re-centred along with it.
/// Not `Copy`: the popup case names the item it hangs off, and that name is
/// an `Arc<str>`. Cloning one is a reference count, which is the same price
/// every other clone in this protocol pays.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Position {
    #[default]
    Left,
    CenterLeft,
    Center,
    CenterRight,
    Right,
    /// Inside the named item's popup rather than anywhere on the bar, which
    /// is how a config writes `position = "popup.volume"`.
    Popup(ItemName),
}

/// `SketchyBar`'s own spellings, which is what its `--query` prints -- `q`
/// and `e` for the centre-adjacent buckets, per `bar_item_serialize` in
/// `bar_item.c`. [`FromStr`] accepts these as well as the long forms, so this
/// round-trips.
impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Left => "left",
            Self::CenterLeft => "q",
            Self::Center => "center",
            Self::CenterRight => "e",
            Self::Right => "right",
            Self::Popup(host) => return write!(f, "popup.{host}"),
        })
    }
}

impl Serialize for Position {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Through [`FromStr`], not serde's enum path, so `q` and `e` keep working.
/// Those are not shorthand this crate invented: they are how `SketchyBar`
/// itself spells the two centre-adjacent buckets, so a config already writes
/// them.
impl<'de> Deserialize<'de> for Position {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a position: expected left, center-left, center, center-right or right")]
pub struct InvalidPosition(String);

impl FromStr for Position {
    type Err = InvalidPosition;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.replace('_', "-").to_ascii_lowercase().as_str() {
            "left" | "l" => Ok(Self::Left),
            "center-left" | "centre-left" | "q" => Ok(Self::CenterLeft),
            "center" | "centre" | "c" => Ok(Self::Center),
            "center-right" | "centre-right" | "e" => Ok(Self::CenterRight),
            "right" | "r" => Ok(Self::Right),
            // Matched before the error so the host's name keeps its own case,
            // which the lowercased copy above has already lost.
            _ => match s.split_once('.') {
                Some(("popup", host)) => ItemName::new(host)
                    .map(Self::Popup)
                    .map_err(|_| InvalidPosition(s.to_owned())),
                _ => Err(InvalidPosition(s.to_owned())),
            },
        }
    }
}

/// A partial update. `None` means "leave as it is" — the distinction from
/// "set to the default" is why every field is an `Option`.
///
/// `deny_unknown_fields` is what turns a typo, or a real `SketchyBar`
/// property this struct has no field for, into a named error at
/// deserialization time — the rsbar CLI's grammar deserializes a `--bar`
/// invocation straight into this type rather than matching keys by hand.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BarPatch {
    pub height: Option<f64>,
    /// `SketchyBar` calls this `position`, and a config writes that; `edge`
    /// is this crate's own name for it and both are accepted.
    #[serde(alias = "position")]
    pub edge: Option<Edge>,
    /// ARGB.
    pub color: Option<Color>,
    pub margin: Option<f64>,
    pub y_offset: Option<f64>,
    pub corner_radius: Option<f64>,
    pub blur_radius: Option<i32>,
    pub hidden: Option<bool>,
    /// Whether the bar sits above the system menu bar or below it.
    pub topmost: Option<bool>,
    /// Space before the first item and after the last, which is not the same
    /// as [`Self::margin`]: margin insets the bar from the screen edge, this
    /// insets the items from the bar.
    pub padding_left: Option<f64>,
    pub padding_right: Option<f64>,
    /// Which displays the bar appears on: `all`, or a 1-based index.
    pub display: Option<String>,
    /// Whether the bar stays put across a space switch.
    pub sticky: Option<bool>,
    /// Whether the bar draws over a fullscreen app.
    pub show_in_fullscreen: Option<bool>,
    /// The gap the centre buckets leave around the notch. Built-in display
    /// only, which is the only one that has one.
    pub notch_width: Option<f64>,
    /// Added to the bar's frame on the built-in display.
    pub notch_offset: Option<f64>,
    /// Overrides the bar's height on the built-in display, when above zero.
    pub notch_display_height: Option<f64>,
}

/// A partial update to one item.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemPatch {
    /// The glyph half of the item.
    pub icon: Option<RunPatch>,
    /// The text half of the item.
    pub label: Option<RunPatch>,
    /// The surface drawn behind the item.
    pub background: Option<BackgroundPatch>,
    pub padding_left: Option<f64>,
    pub padding_right: Option<f64>,
    pub y_offset: Option<f64>,
    pub position: Option<Position>,
    pub drawing: Option<Toggle>,
    /// Run on every update. Receives `RSBAR_NAME`, `RSBAR_SENDER` and the
    /// event's payload as named variables.
    pub script: Option<String>,
    /// Run when this item is clicked. Receives the same, plus `RSBAR_BUTTON`
    /// and `RSBAR_MODIFIERS`.
    pub click_script: Option<String>,
    /// A menu bar item to mirror, as `Owner,Name` — the form
    /// `--query menu-items` lists. An empty string stops mirroring.
    pub alias: Option<String>,
    /// The items this one draws behind, as one surface. An empty list stops
    /// it being a bracket.
    ///
    /// A [`Selector::Pattern`] is resolved against the daemon's own live item
    /// list at apply time, not by whoever built this patch: a config still
    /// adding items must never race a client's stale snapshot.
    pub members: Option<Vec<Selector>>,
    /// Seconds between routine updates. Zero means "only on subscribed
    /// events".
    pub update_freq: Option<u32>,
    /// Whether the item runs its script and receives events at all.
    ///
    /// Distinct from [`Self::drawing`], which only stops it being drawn: a
    /// hidden item that still updates costs work nobody can see, and an item
    /// that is drawn from a value someone else sets wants the opposite.
    pub updates: Option<bool>,
    /// A fixed width, overriding what the item's contents come to. `None`
    /// leaves it measured; a config sets this for a spacer, or to stop an
    /// item's width jittering as its text changes.
    pub width: Option<f64>,
    /// Which displays this item appears on: `all`, or a 1-based index.
    pub display: Option<String>,
    /// Which space a `space` item stands for. `Some(None)` unsets it, which
    /// is why it is doubly optional: the outer layer is "did the patch say
    /// anything", the inner one is the value.
    pub associated_space: Option<Option<std::num::NonZeroU64>>,
    /// A slider's fill, 0-100.
    pub percentage: Option<u8>,
    /// A slider's knob.
    pub knob: Option<RunPatch>,
    /// A space item's colour when it is the selected one.
    pub highlight_color: Option<Color>,
    /// The popup this item hangs off itself, shown by `popup.drawing=on`.
    pub popup: Option<PopupPatch>,
}

/// A partial update to an item's own popup.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PopupPatch {
    pub drawing: Option<Toggle>,
    /// Rows run left to right instead of stacking.
    pub horizontal: Option<bool>,
    /// Which edge of the host the popup lines up with: `left`, `center` or
    /// `right`.
    pub align: Option<String>,
    pub topmost: Option<bool>,
    /// Each row's height.
    pub height: Option<f64>,
    /// Gap between the host item and the popup.
    pub y_offset: Option<f64>,
    pub background: Option<BackgroundPatch>,
}

// `struct_patch::Patch` compares each field to decide what a patch changed.
// For floats clippy calls that suspicious; here the values are config literals
// — a corner radius someone typed — not the result of arithmetic, so exact
// comparison is exactly right. The allow sits here because the lint fires
// inside the derive's own expansion, where an item-level allow does not reach.
#[allow(clippy::float_cmp)]
mod parts {
    use super::{Color, Deserialize, FontSpec, Serialize};

    /// One half of an item's text: its glyph, or its label.
    ///
    /// A config writes this nested — `label.color`, or `{ label = { color = ... }
    /// }` — because it is one thing on the other side too. [`RunPatch`] is
    /// derived from it, so a new property is one line in one place and the two
    /// cannot drift.
    ///
    /// `deny_unknown_fields` on the generated [`RunPatch`] is what lets the
    /// CLI grammar deserialize a `--set foo icon.<key>=<value>` straight into
    /// this type and get a named error for a typo, rather than matching keys
    /// by hand.
    #[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, struct_patch::Patch)]
    #[patch(name = "RunPatch")]
    #[patch(attribute(derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)))]
    #[patch(attribute(serde(deny_unknown_fields)))]
    pub struct Run {
        /// `SketchyBar` calls this property `string`; rsbar calls the field
        /// `text` since that is what it is. The patch struct accepts both
        /// spellings, so neither client has to know which name won.
        #[patch(attribute(serde(alias = "string")))]
        pub text: String,
        pub color: Color,
        pub font: FontSpec,
        /// Whether this half is drawn, independently of the item's own `drawing`
        /// — an item that shows its glyph but not its text.
        pub drawing: bool,
        /// Space either side of this half alone, on top of the item's own.
        pub padding_left: f64,
        pub padding_right: f64,
        /// Shifts this half alone, on top of the item's own offset -- how a
        /// config nudges a glyph into line with the text beside it.
        pub y_offset: f64,
    }

    /// A bare string is sugar for the text, which is how every config writes the
    /// common case — `label = "12:00"` rather than `label = { text = "12:00" }`.
    impl<S: Into<String>> From<S> for RunPatch {
        fn from(text: S) -> Self {
            Self {
                text: Some(text.into()),
                ..Self::default()
            }
        }
    }

    /// The surface drawn behind an item.
    #[derive(
        Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, struct_patch::Patch,
    )]
    #[patch(name = "BackgroundPatch")]
    #[patch(attribute(derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)))]
    #[patch(attribute(serde(deny_unknown_fields)))]
    pub struct Background {
        /// Whether the surface is drawn. Separate from the item's own
        /// `drawing` so a config can turn a pill off and leave the text.
        pub drawing: bool,
        pub color: Color,
        pub corner_radius: f64,
        /// A fixed height, or zero for the bar's own. A shorter surface is how a
        /// pill sits inside the bar with a margin above and below.
        pub height: f64,
        /// Inset from the item's own edges, so the surface can be tighter or
        /// wider than what it sits behind.
        pub padding_left: f64,
        pub padding_right: f64,
        pub border_color: Color,
        pub border_width: f64,
    }
}

pub use parts::{Background, BackgroundPatch, Run, RunPatch};
/// Re-exported so the daemon can apply a patch without depending on
/// `struct_patch` directly — the derive lives here, so the trait should too.
pub use struct_patch::Patch;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Query {
    Bar,
    Items,
    Item(ItemName),
    /// Every menu bar item that can be mirrored, as `Owner,Name` — the form
    /// an item's `alias` takes. There is no discovering these otherwise.
    MenuItems,
    /// The frontmost application's own menu titles, in on-screen order,
    /// starting with the Apple menu. Not mirrorable, only listable: see
    /// `rsbar::menus`.
    AppMenus,
    /// The properties [`Request::SetDefault`] has stashed, applied to every
    /// item added since.
    Defaults,
}

/// `before`/`after` in `--move <item> before|after <reference>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relative {
    Before,
    After,
}

/// A component kind `--add` accepts but rsbar cannot yet draw.
///
/// Modelled rather than rejected at parse time, so a config using one gets a
/// clear "not implemented" from the daemon instead of never reaching it —
/// see `TYPE_GRAPH`/`TYPE_SPACE`/`TYPE_SLIDER` in `SketchyBar`'s own
/// `defines.h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentKind {
    Space,
    Graph,
    Slider,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    SetBar(BarPatch),
    AddItem {
        name: ItemName,
        position: Position,
    },
    /// `--add space|graph|slider ...`: recognised, but nothing on the daemon
    /// side can draw one yet.
    AddComponent {
        name: ItemName,
        position: Position,
        kind: ComponentKind,
    },
    SetItem {
        name: ItemName,
        /// Boxed: an item patch is a dozen options and dwarfs every other
        /// variant, so every request would be as big as the largest one.
        patch: Box<ItemPatch>,
    },
    /// `--set /pattern/ ...`: every item the daemon's own live list matches,
    /// resolved here rather than by the caller — see [`Selector`].
    SetMatching {
        pattern: String,
        patch: Box<ItemPatch>,
    },
    RemoveItem(ItemName),
    /// `--remove /pattern/`, resolved the same way as [`Request::SetMatching`].
    RemoveMatching(String),
    /// `--default ...`: properties the daemon copies onto every item added
    /// from here on, the way `SketchyBar`'s own `default_item` does.
    ///
    /// Never expanded into a fully-populated [`ItemPatch`] by a client: only
    /// the fields a `--default` invocation actually named are `Some` here,
    /// same as [`Request::SetItem`]'s own patch, so the daemon's damage
    /// tracking still sees only what really changed.
    SetDefault(Box<ItemPatch>),
    /// `--move <item> before|after <reference>`.
    Move {
        name: ItemName,
        relative: Relative,
        reference: ItemName,
    },
    /// `--reorder <name> ...`: the bar's new left-to-right item order.
    Reorder(Vec<ItemName>),
    /// Replaces the item's subscriptions.
    Subscribe {
        name: ItemName,
        events: Vec<Kind>,
    },
    /// `--push <name> <value>`: one more sample for a graph.
    Push {
        name: ItemName,
        /// `f32` to match the sample type a graph stores.
        value: f32,
    },
    /// Fires an event now, as if a source had produced it.
    Trigger(Event),
    /// Runs every item's script immediately, ignoring update frequency.
    UpdateAll,
    /// Re-runs the config from scratch, as a file change does.
    Reload,
    /// Marks every item as unclaimed, so a client can rebuild the bar without
    /// first working out what it wants to remove.
    ///
    /// Whatever the client does not touch before [`Request::EndConfig`] is
    /// what it no longer wants, and goes. Nothing is removed until then, so a
    /// config that fails half way leaves the bar it had rather than an empty
    /// one.
    BeginConfig,
    /// Sweeps every item untouched since [`Request::BeginConfig`].
    EndConfig,
    Query(Query),
    /// Opens one of the frontmost application's own menus, by the index
    /// [`Query::AppMenus`] reported.
    PressAppMenu(usize),
    /// Opens the real menu behind a mirrored menu bar item, so a click on an
    /// alias does what a click on the thing it mirrors would.
    PressAlias(ItemName),
    Shutdown,
}

/// What the daemon currently believes the bar looks like.
// A report, not a configuration object: every flag the bar has is on it by
// definition, so grouping them into sub-structs would only make a caller
// reassemble what it asked for.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BarState {
    pub height: f64,
    pub edge: Edge,
    pub color: Color,
    pub margin: f64,
    pub y_offset: f64,
    pub corner_radius: f64,
    pub blur_radius: i32,
    pub topmost: bool,
    pub hidden: bool,
    pub displays: usize,
    pub sticky: bool,
    pub show_in_fullscreen: bool,
    pub notch_width: f64,
    pub notch_offset: f64,
    pub notch_display_height: f64,
}

/// An item's on-screen geometry and background, mirroring the nesting
/// `SketchyBar`'s own `--query` uses (`bar_item_serialize` in `bar_item.c`)
/// closely enough that a config's own field paths — `geometry.drawing`,
/// `geometry.position` — work unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Geometry {
    pub drawing: bool,
    pub position: Position,
    pub y_offset: f64,
    pub padding_left: f64,
    pub padding_right: f64,
    /// A fixed width, or `None` for measured — `SketchyBar` prints `-1` for
    /// the same thing, which is `bar_item->has_const_width` being false.
    pub width: Option<f64>,
    pub background: Background,
}

/// An item's scripting state, mirroring `SketchyBar`'s own `scripting` key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scripting {
    pub script: Option<String>,
    pub click_script: Option<String>,
    pub update_freq: u32,
    pub updates: bool,
}

/// An item's popup, as `--query` reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PopupState {
    pub drawing: bool,
    pub horizontal: bool,
    pub align: String,
    pub topmost: bool,
    pub height: f64,
    pub y_offset: f64,
    pub background: Background,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemState {
    pub name: ItemName,
    pub geometry: Geometry,
    /// Always present, even on an item with no popup, because a config reads
    /// it without checking -- `overflow:query().popup.drawing == "on"` -- and
    /// a missing key there is a crash rather than a false.
    pub popup: PopupState,
    pub icon: Run,
    pub label: Run,
    pub scripting: Scripting,
    pub events: Vec<Kind>,
    /// What this item mirrors, if it is an alias.
    pub alias: Option<String>,
    /// What this item brackets, if it is a bracket.
    pub members: Vec<ItemName>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Bar(Box<BarState>),
    Items(Vec<ItemState>),
    Item(Box<ItemState>),
    /// Mirrorable menu bar items, as `Owner,Name`.
    MenuItems(Vec<String>),
    /// The frontmost application's own menu titles, in on-screen order.
    AppMenus(Vec<String>),
    /// The properties [`Request::SetDefault`] has stashed. Only the fields a
    /// `--default` invocation actually named are `Some`.
    Defaults(Box<ItemPatch>),
    /// The request was understood but could not be carried out.
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_names_reject_shell_hostile_input() {
        assert!(ItemName::new("clock").is_ok());
        assert!(ItemName::new("space.1").is_ok());
        assert!(ItemName::new("front-app_2").is_ok());
        // An alias is named for what it mirrors, and those names have commas
        // and spaces in them.
        assert!(ItemName::new("Amphetamine,Amphetamine").is_ok());
        assert!(ItemName::new("Control Centre,FocusModes").is_ok());
        assert_eq!(ItemName::new(""), Err(InvalidName::Empty));
        assert!(matches!(
            ItemName::new("two\nlines"),
            Err(InvalidName::Character(_))
        ));
    }

    #[test]
    fn a_position_deserializes_by_the_spellings_a_config_uses() {
        // Not serde's derived enum path, which would only take the exact
        // variant names -- `q` and `e` are SketchyBar's own spellings for the
        // centre-adjacent buckets and appear in real configs.
        for (text, expected) in [
            ("q", Position::CenterLeft),
            ("e", Position::CenterRight),
            ("centre", Position::Center),
            ("center_left", Position::CenterLeft),
        ] {
            let json = format!("\"{text}\"");
            assert_eq!(
                serde_json::from_str::<Position>(&json).unwrap(),
                expected,
                "{text}"
            );
        }
        assert_eq!(
            serde_json::from_str::<Edge>("\"bottom\"").unwrap(),
            Edge::Bottom
        );
        // And what it writes, it can read back: --query prints these and a
        // config feeds them straight back in.
        for position in [
            Position::Left,
            Position::CenterLeft,
            Position::Center,
            Position::CenterRight,
            Position::Right,
        ] {
            assert_eq!(position.to_string().parse(), Ok(position));
        }
    }

    #[test]
    fn positions_accept_the_spellings_a_config_uses() {
        assert_eq!("left".parse(), Ok(Position::Left));
        assert_eq!("center_right".parse(), Ok(Position::CenterRight));
        assert_eq!("centre".parse(), Ok(Position::Center));
        assert_eq!("R".parse(), Ok(Position::Right));
        assert!("sideways".parse::<Position>().is_err());
    }

    #[test]
    fn requests_round_trip_through_postcard() {
        let request = Request::SetItem {
            name: ItemName::new("clock").unwrap(),
            patch: Box::new(ItemPatch {
                label: Some(RunPatch {
                    text: Some("09:41".into()),
                    color: Some(Color(0xffff_ffff)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };
        let bytes = postcard::to_allocvec(&request).unwrap();
        assert_eq!(postcard::from_bytes::<Request>(&bytes).unwrap(), request);
    }

    #[test]
    fn every_hand_written_codec_round_trips_through_postcard() {
        // postcard is not self-describing, so a type whose Serialize and
        // Deserialize disagree about the shape -- one writing a variant
        // index, the other reading a string -- encodes fine and decodes to
        // nothing. Every type here spells itself out by hand for the CLI's
        // sake, so every one needs both halves checked against the real wire
        // format rather than against JSON, which forgives the mismatch.
        let requests = [
            Request::AddItem {
                name: ItemName::new("clock").unwrap(),
                position: Position::CenterLeft,
            },
            Request::SetBar(BarPatch {
                edge: Some(Edge::Bottom),
                color: Some(Color(0xff00_ff00)),
                ..Default::default()
            }),
            Request::SetItem {
                name: ItemName::new("clock").unwrap(),
                patch: Box::new(ItemPatch {
                    position: Some(Position::Right),
                    icon: Some(RunPatch {
                        font: Some(FontSpec::parse("Menlo:Bold:15")),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            },
        ];
        for request in requests {
            let bytes = postcard::to_allocvec(&request).unwrap();
            assert_eq!(
                postcard::from_bytes::<Request>(&bytes).unwrap(),
                request,
                "{request:?}"
            );
        }
    }

    #[test]
    fn a_selector_is_a_pattern_only_by_shape() {
        assert_eq!(
            "front_app".parse(),
            Ok(Selector::Name(ItemName::new("front_app").unwrap()))
        );
        assert_eq!(
            r"/menu\..*/".parse(),
            Ok(Selector::Pattern(r"menu\..*".into()))
        );
        // A single `/` has no interior: not pattern-shaped.
        assert_eq!("/".parse(), Ok(Selector::Name(ItemName::new("/").unwrap())));
    }

    #[test]
    fn run_patch_accepts_sketchybars_string_spelling_as_an_alias_for_text() {
        let patch: RunPatch = serde_json::from_str(r#"{"string": "hi"}"#).unwrap();
        assert_eq!(
            patch,
            RunPatch {
                text: Some("hi".into()),
                ..Default::default()
            }
        );
    }

    #[test]
    fn run_patch_rejects_an_unknown_property() {
        assert!(serde_json::from_str::<RunPatch>(r#"{"wat": 1}"#).is_err());
    }

    #[test]
    fn colors_and_fonts_serialize_in_sketchybars_own_spelling() {
        let patch = RunPatch {
            color: Some(Color(0xffff_0000)),
            font: Some(FontSpec::parse("Menlo:Bold:15")),
            ..Default::default()
        };
        let json = serde_json::to_string(&patch).unwrap();
        assert!(json.contains(r#""color":"0xffff0000""#));
        assert!(json.contains(r#""font":"Menlo:Bold:15""#));
    }
}

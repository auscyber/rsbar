//! The wire vocabulary shared by the rsbar daemon, its CLI and its Lua module.
//!
//! This crate is the protocol. Everything that crosses a process boundary is
//! defined here exactly once, so a client cannot drift from the daemon.

#![cfg(target_os = "macos")]

// So the code `#[derive(EnvFields)]` generates can name this crate by the one
// path that works everywhere, here included.
extern crate self as rsbar_protocol;

pub mod event;
pub mod json;
pub mod patch;
pub mod style;
pub mod wire;

pub use event::{
    EnvFields, Event, EventName, Field, Kind, Modifiers, MouseButton, NotificationName, PowerSource,
};
pub use json::Json;
pub use patch::Missing;
pub use rsbar_protocol_macros::{Changes, EnvFields, Spelling, events};
pub use style::{Color, FontSpec};

use serde::de::DeserializeSeed;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

/// The bootstrap name the daemon claims and clients look up.
///
/// Overridable so a development build can run beside an installed one.
#[must_use]
pub fn service_name() -> String {
    std::env::var("RSBAR_SERVICE").unwrap_or_else(|_| "com.auscyber.rsbar".to_owned())
}

/// Which side of the host item a popup's own edge lines up with --
/// `SketchyBar`'s `popup.align`. The popup's width and the host's are
/// independent, so `Left`/`Right` are about edges, not centring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Spelling)]
#[spelling(error = InvalidAlign, fold = lower, serde)]
pub enum PopupAlign {
    #[default]
    #[spell("left", "l")]
    Left,
    #[spell("center", "centre", "c")]
    Center,
    #[spell("right", "r")]
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a popup alignment: expected left, center or right")]
pub struct InvalidAlign(String);

/// Which display something is restricted to: every one, or a single 1-based
/// index into the active displays, which is the same order the bar builds its
/// panels in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Spelling)]
#[spelling(error = InvalidDisplay, fold = lower, serde)]
pub enum DisplayTarget {
    #[default]
    #[spell("all")]
    All,
    /// Spelled as the number itself, which is both what `--query` prints and
    /// what a config writes.
    #[spell("{}")]
    Index(std::num::NonZeroU32),
}

impl DisplayTarget {
    /// Whether this includes the display at `ordinal`, 1-based.
    #[must_use]
    pub fn matches(self, ordinal: u32) -> bool {
        match self {
            Self::All => true,
            Self::Index(index) => index.get() == ordinal,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a display: expected `all` or a 1-based index")]
pub struct InvalidDisplay(String);

/// A boolean as a config spells one: `on`/`off`, `yes`/`no`, `true`/`false`,
/// `1`/`0`.
///
/// Every one of those spellings is in a `SketchyBar` config somewhere, and
/// both front ends used to coerce them separately -- the CLI grammar in one
/// place, the Lua host's `opt_bool` in another, and nothing making the two
/// agree. The coercion belongs to the type, so there is one spelling of what
/// `off` means.
///
/// Hand-written rather than derived, because the config's spelling is the
/// authority and serde's derived one is not: `on` and `off` are what
/// `--query` prints and what a config compares against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Boolish(pub bool);

impl Boolish {
    pub const TRUE: Self = Self(true);
    pub const FALSE: Self = Self(false);

    /// The plain boolean, for the arithmetic and branching a `bool` is for.
    #[must_use]
    pub const fn get(self) -> bool {
        self.0
    }
}

impl From<Boolish> for bool {
    fn from(value: Boolish) -> Self {
        value.0
    }
}

impl From<bool> for Boolish {
    fn from(value: bool) -> Self {
        Self(value)
    }
}

/// One error for both spellings, because they are one vocabulary: a value and
/// a change to it differ only by `toggle`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not on/off, true/false, yes/no, 1/0 or toggle, optionally `!`-negated")]
pub struct InvalidBool(String);

impl FromStr for Boolish {
    type Err = InvalidBool;

    /// Every spelling `SketchyBar` takes, including a leading `!` for the
    /// opposite of one — `drawing=!off`. The negation lives here rather than
    /// in a front end so the CLI, Lua and anything else read it the same way;
    /// it used to be in the argv parser alone, which stopped being reached
    /// the moment these became string-deserialised types.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (negated, spelling) = match s.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        let value = match spelling.to_ascii_lowercase().as_str() {
            "on" | "true" | "yes" | "1" => true,
            "off" | "false" | "no" | "0" => false,
            _ => return Err(InvalidBool(s.to_owned())),
        };
        Ok(Self(value != negated))
    }
}

impl fmt::Display for Boolish {
    /// `on`/`off`, which is what `--query` has always printed and what a
    /// config compares against: `overflow:query().popup.drawing == "on"`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0 { "on" } else { "off" })
    }
}

impl Serialize for Boolish {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Reads a boolean written either way a caller might have it.
///
/// A visitor rather than a `Cow<str>`, because the two inputs disagree about
/// what they hand over: the wire carries the string [`Boolish`]'s `Serialize`
/// wrote, while a Lua config writes a native `true`. Asked for through
/// `deserialize_any`, which every format the protocol now meets can answer --
/// the *value* says which it is, and this takes either.
struct BoolishVisitor;

impl serde::de::Visitor<'_> for BoolishVisitor {
    type Value = Boolish;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("on/off, true/false, yes/no, 1/0, optionally `!`-negated, or a boolean")
    }

    fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Boolish, E> {
        text.parse().map_err(E::custom)
    }

    fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Boolish, E> {
        Ok(Boolish(value))
    }
}

impl<'de> Deserialize<'de> for Boolish {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(BoolishVisitor)
    }
}

/// What a patch says about a boolean: what a [`Changes`] field holds where
/// the target holds a [`Boolish`].
///
/// `SketchyBar` accepts `drawing=toggle` wherever it accepts `on`/`off`, and
/// the user's own config binds a click to `popup.drawing=toggle`. A plain
/// `bool` cannot carry that: what a flip ends up as depends on what it already
/// was, which only the daemon knows -- which is exactly why this belongs in a
/// patch and [`Boolish`] does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Spelling)]
#[spelling(error = InvalidBool, fold = lower, rest = BoolChange::valued, serde(bool))]
#[spelling(
    expecting = "on/off, true/false, yes/no, 1/0 or toggle, optionally `!`-negated, or a boolean"
)]
pub enum BoolChange {
    #[spell("on")]
    True,
    #[spell("off")]
    False,
    #[spell("toggle")]
    Toggle,
}

impl BoolChange {
    /// What this makes of a value that is currently `current`.
    #[must_use]
    pub fn resolve(self, current: Boolish) -> Boolish {
        match self {
            Self::True => Boolish(true),
            Self::False => Boolish(false),
            Self::Toggle => Boolish(!current.0),
        }
    }

    /// The same, but only when applying it would change something.
    ///
    /// `True` against a value already true is no change at all, and the
    /// daemon should not repaint because a request mentioned a property.
    /// `Toggle` always is one.
    #[must_use]
    pub fn change(self, current: Boolish) -> Option<Boolish> {
        let next = self.resolve(current);
        (next != current).then_some(next)
    }
}

impl BoolChange {
    /// Every spelling that is a *value* rather than a change, read by
    /// [`Boolish`] itself.
    ///
    /// Delegated rather than tabled here, because `on`/`true`/`yes`/`1` and
    /// the leading `!` belong to the value: a second copy of them beside
    /// [`BoolChange`]'s own three words is exactly the drift this derive
    /// exists to stop. Only `toggle` is this type's own.
    fn valued(text: &str) -> Result<Self, InvalidBool> {
        text.parse::<Boolish>().map(Self::from)
    }
}

impl From<Boolish> for BoolChange {
    fn from(value: Boolish) -> Self {
        if value.0 { Self::True } else { Self::False }
    }
}

/// What a Lua config writes when it means one: a native boolean, which the
/// generated reader takes alongside the spellings.
impl From<bool> for BoolChange {
    fn from(value: bool) -> Self {
        Boolish(value).into()
    }
}

/// What a patch would make of a value it is applied to.
///
/// `Some` only when applying it changes something: the daemon should repaint
/// because a value changed, not because a request mentioned a property. That
/// one rule covers all three shapes a patch field can have, which is why they
/// are one trait -- an ordinary value that differs from what is there, a
/// [`BoolChange`] resolved against it, and a nested patch that walks its own
/// fields the same way.
///
/// Borrowed rather than cloned where the new value is already in hand, which
/// is why the one method every impl writes is [`changed`](Self::changed): a
/// caller asking whether a label moved should not pay for a `String` to find
/// out it did not. A computed value -- a flip resolved against what is there,
/// a nested patch walked field by field -- comes back owned, and for those it
/// is a `Copy` payload or a struct that had to be built anyway.
pub trait Changes<T: Clone> {
    /// Which parts of `T` moved.
    ///
    /// `()` for a leaf: it moved or it did not, and there is nothing finer to
    /// say. A derived patch reports one entry per field, and a nested patch
    /// puts its own record in that entry -- so a caller learns "the background
    /// moved, and inside it only the corner radius" rather than "something
    /// under the background moved", and re-does only the work that implies.
    type Moved;

    /// What `current` becomes and what moved to get there, or `None` when
    /// applying this changes nothing.
    ///
    /// Borrowed out of the patch where the value is stored there, owned where
    /// it had to be worked out.
    fn moved(&self, current: &T) -> Option<(Cow<'_, T>, Self::Moved)>;

    /// The same, for a caller that only wants the value.
    fn changed(&self, current: &T) -> Option<Cow<'_, T>> {
        self.moved(current).map(|(value, _)| value)
    }

    /// The same, owned -- for a caller that is going to keep it either way.
    fn changes(&self, current: &T) -> Option<T> {
        self.changed(current).map(Cow::into_owned)
    }
}

/// The ordinary leaf: a value replaces one it differs from, and nothing else.
/// Nothing is computed, so nothing is cloned to answer.
impl<T: Clone + PartialEq> Changes<T> for T {
    type Moved = ();

    fn moved(&self, current: &T) -> Option<(Cow<'_, T>, ())> {
        (self != current).then(|| (Cow::Borrowed(self), ()))
    }
}

/// The one field type a patch spells differently from its target.
impl Changes<Boolish> for BoolChange {
    type Moved = ();

    fn moved(&self, current: &Boolish) -> Option<(Cow<'_, Boolish>, ())> {
        self.change(*current).map(|next| (Cow::Owned(next), ()))
    }
}

/// A property a config sets by naming it and unsets by naming nothing:
/// `script=""` removes the script, `alias=""` stops mirroring. One spelling
/// for both, because `SketchyBar` has one -- the empty string *is* how a
/// config says "no longer".
impl Changes<Option<String>> for String {
    type Moved = ();

    fn moved(&self, current: &Option<String>) -> Option<(Cow<'_, Option<String>>, ())> {
        let next = (!self.is_empty()).then(|| self.clone());
        (next != *current).then_some((Cow::Owned(next), ()))
    }
}

/// A measurement a config can only set, never unset: `width=100` fixes it and
/// there is no spelling for "go back to measuring", the same as
/// `SketchyBar`'s own `has_const_width`.
// The values are config literals -- a width somebody typed -- not the result
// of arithmetic, so exact comparison is exactly right.
#[allow(clippy::float_cmp)]
impl Changes<Option<f64>> for f64 {
    type Moved = ();

    fn moved(&self, current: &Option<f64>) -> Option<(Cow<'_, Option<f64>>, ())> {
        (Some(*self) != *current).then_some((Cow::Owned(Some(*self)), ()))
    }
}

/// What a bracket names, against what it currently holds.
///
/// A real substitution rather than a rename: a config writes selectors and an
/// item holds names, and `/pattern/` only becomes names against a live item
/// list. So a patch carrying one answers "I cannot tell from here" -- `None`
/// -- and the daemon, which does have the list, resolves it itself.
impl Changes<Vec<ItemName>> for Vec<Selector> {
    type Moved = ();

    fn moved(&self, current: &Vec<ItemName>) -> Option<(Cow<'_, Vec<ItemName>>, ())> {
        let named: Option<Vec<ItemName>> = self
            .iter()
            .map(|selector| match selector {
                Selector::Name(name) => Some(name.clone()),
                Selector::Pattern(_) => None,
            })
            .collect();
        let named = named?;
        (named != *current).then_some((Cow::Owned(named), ()))
    }
}

/// An item's identity. A newtype so an item name and a stray string cannot be
/// swapped for one another.
///
/// The string is shared rather than owned: a name is cloned constantly — into
/// the index, into every job, into every response — and none of those want a
/// copy of the bytes. Cloning this is a reference count.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
// Read back through [`Self::new`] rather than by unwrapping the string, so a
// name that arrives off the wire is held to the same rule as one a config
// typed. The derived reader took whatever was there, empty names and control
// characters included, which is the whole point of the newtype going missing
// at the one boundary that matters.
#[serde(try_from = "String")]
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

impl TryFrom<String> for ItemName {
    type Error = InvalidName;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// A `--set`/`--remove` target, or a bracket member: an exact name, or
/// `SketchyBar`'s own `/pattern/` shorthand for "every item whose name
/// matches this regex" (`REGEX_DELIMITER` in `message.c`).
///
/// Never resolved here or by any other client: only the daemon has a live
/// item list, so only it can turn a [`Selector::Pattern`] into the items it
/// names without racing a config that is still adding them.
///
/// A pattern is told from a literal name by shape alone -- the `/.../` around
/// it -- never by charset, since [`ItemName`] permits almost anything a
/// pattern could too. That is what the two spellings say: the affixed one is
/// tried first, and whatever it declines is a name.
#[derive(Debug, Clone, PartialEq, Eq, Spelling)]
#[spelling(error = InvalidName, serde)]
pub enum Selector {
    #[spell("{}", rest = Selector::named)]
    Name(ItemName),
    /// The regex source, without the surrounding `/.../`.
    #[spell("/{}/")]
    Pattern(String),
}

impl Selector {
    /// Whatever is not pattern-shaped, which is a name or nothing at all.
    fn named(text: &str) -> Result<Self, InvalidName> {
        ItemName::new(text).map(Self::Name)
    }
}

/// Which edge of the display the bar occupies.
///
/// Spelled rather than derived, for the same reason [`Position`] is: the
/// config's word is the authority, not serde's derived one, and a derived
/// enum would put a Rust variant name where a config's spelling belongs. One
/// table drives all four impls, so they cannot disagree about the shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Spelling)]
#[spelling(error = InvalidEdge, fold = lower, serde)]
pub enum Edge {
    #[default]
    #[spell("top")]
    Top,
    #[spell("bottom")]
    Bottom,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a bar position: expected top or bottom")]
pub struct InvalidEdge(String);

/// Where an item sits along the bar.
///
/// The two centre-adjacent buckets exist so a config can put something beside
/// a centred item without it being re-centred along with it.
/// Not `Copy`: the popup case names the item it hangs off, and that name is
/// an `Arc<str>`. Cloning one is a reference count, which is the same price
/// every other clone in this protocol pays.
///
/// Deliberately not [`Default`]. There is no sensible "somewhere" for an item
/// to be, and the `Default` that used to exist here served the *patch* case
/// -- where an absent position means "leave it where it is" -- while quietly
/// leaking into the *add* case, where `--add item foo` with no position
/// silently landed on the left. `#[changes(required)]` on
/// [`Geometry::position`] is where that difference lives now, so the old
/// behaviour is unrepresentable rather than merely unreached.
///
/// Each bucket is spelled once and read back by every one of its spellings.
/// The canonical form is `SketchyBar`'s own, which is what its `--query`
/// prints -- `q` and `e` for the centre-adjacent buckets, per
/// `bar_item_serialize` in `bar_item.c` -- and the long forms are accepted
/// beside them, so a config that writes either round-trips. Neither is
/// shorthand this crate invented.
///
/// The host's name in `popup.<host>` keeps its own case: the literal table is
/// consulted against the folded text, and a payload spelling against the
/// original.
#[derive(Debug, Clone, PartialEq, Eq, Spelling)]
#[spelling(error = InvalidPosition, fold = dashes, serde)]
pub enum Position {
    #[spell("left", "l")]
    Left,
    #[spell("q", "center-left", "centre-left")]
    CenterLeft,
    #[spell("center", "centre", "c")]
    Center,
    #[spell("e", "center-right", "centre-right")]
    CenterRight,
    #[spell("right", "r")]
    Right,
    /// Inside the named item's popup rather than anywhere on the bar, which
    /// is how a config writes `position = "popup.volume"`.
    #[spell("popup.{}")]
    Popup(ItemName),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a position: expected left, center-left, center, center-right or right")]
pub struct InvalidPosition(String);

// `Changes` compares each field to decide what a patch changed. For floats
// clippy calls that suspicious; here the values are config literals — a corner
// radius someone typed — not the result of arithmetic, so exact comparison is
// exactly right. The allow sits here because the lint fires inside the derive's
// own expansion, where an item-level allow does not reach.
#[allow(clippy::float_cmp)]
mod parts {
    use super::{BoolChange, Boolish, Changes, Color, Deserialize, FontSpec, Serialize};

    /// One half of an item's text: its glyph, or its label.
    ///
    /// A config writes this nested — `label.color`, or `{ label = { color = ... }
    /// }` — because it is one thing on the other side too. [`RunPatch`] is
    /// derived from it, so a new property is one line in one place and the two
    /// cannot drift.
    ///
    /// `#[changes(scalar = text)]` is the other spelling every config uses:
    /// `label = "12:00"` for `label = { string = "12:00" }`. It belongs to
    /// [`RunPatch`] rather than to whoever is reading one, so argv, Lua and
    /// the wire all take it without a line of code between them.
    #[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, Changes)]
    #[changes(name = RunPatch, what = "an item's icon or label")]
    #[changes(derive(Debug, Clone, Default, PartialEq))]
    #[changes(scalar = text)]
    pub struct Run {
        /// `SketchyBar` calls this property `string`; rsbar calls the field
        /// `text` since that is what it is. The patch struct accepts both
        /// spellings, so neither client has to know which name won.
        #[serde(alias = "string")]
        pub text: String,
        pub color: Color,
        pub font: FontSpec,
        /// Whether this half is drawn, independently of the item's own `drawing`
        /// — an item that shows its glyph but not its text.
        #[changes(as = BoolChange)]
        pub drawing: Boolish,
        /// Space either side of this half alone, on top of the item's own.
        pub padding_left: f64,
        pub padding_right: f64,
        /// Shifts this half alone, on top of the item's own offset -- how a
        /// config nudges a glyph into line with the text beside it.
        pub y_offset: f64,
        /// Draw in `highlight_color` rather than `color`. Per half, matching
        /// `struct text` in `SketchyBar`'s `text.c`: a space item's script
        /// sets `icon.highlight=$SELECTED`.
        #[changes(as = BoolChange)]
        pub highlight: Boolish,
        pub highlight_color: Color,
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
    #[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, Changes)]
    #[changes(name = BackgroundPatch, what = "an item's background")]
    #[changes(derive(Debug, Clone, Copy, Default, PartialEq))]
    pub struct Background {
        /// Whether the surface is drawn. Separate from the item's own
        /// `drawing` so a config can turn a pill off and leave the text.
        #[changes(as = BoolChange)]
        pub drawing: Boolish,
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

/// What `--query` was asked about.
///
/// Spelled rather than tagged, because the last variant is a fallback: any
/// word that is not one of these is an item name, which a derived
/// externally-tagged enum cannot express. `fold = none` is deliberate --
/// `BAR` is an item called `BAR`, not the bar.
#[derive(Debug, Clone, PartialEq, Eq, Spelling)]
#[spelling(error = InvalidQuery, serde)]
pub enum Query {
    #[spell("bar")]
    Bar,
    #[spell("items")]
    Items,
    #[spell("{}", rest = Query::named)]
    Item(ItemName),
    /// Every menu bar item that can be mirrored, as `Owner,Name` — the form
    /// an item's `alias` takes. There is no discovering these otherwise.
    #[spell("menu_items", "menu-items", "default_menu_items")]
    MenuItems,
    /// The frontmost application's own menu titles, in on-screen order,
    /// starting with the Apple menu. Not mirrorable, only listable: see
    /// `rsbar::menus`.
    #[spell("app_menus", "app-menus")]
    AppMenus,
    /// The properties [`Request::SetDefault`] has stashed, applied to every
    /// item added since.
    #[spell("defaults")]
    Defaults,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidQuery {
    #[error("`{0}` is not `bar`, `items`, `menu-items`, `app-menus`, `defaults` or an item name")]
    Unknown(String),
    /// A real `SketchyBar` query this daemon does not support yet — distinct
    /// from [`Self::Unknown`] so a config gets told the difference between a
    /// typo and a thing genuinely not implemented.
    #[error("`{0}` is a real SketchyBar query rsbar does not support yet: {1}")]
    Unsupported(String, &'static str),
    #[error(transparent)]
    Name(#[from] InvalidName),
}

impl Query {
    /// Every word the table did not claim: a query `SketchyBar` has and rsbar
    /// does not, or an item name.
    ///
    /// The unsupported pair is refused here rather than spelled as variants,
    /// because they are the one case where a word is neither a query this
    /// answers nor a name -- and a config author wants to be told which.
    fn named(text: &str) -> Result<Self, InvalidQuery> {
        match text {
            "events" => Err(InvalidQuery::Unsupported(
                text.to_owned(),
                "rsbar does not track registered custom events",
            )),
            "displays" => Err(InvalidQuery::Unsupported(
                text.to_owned(),
                "querying displays is not implemented",
            )),
            _ => Ok(Self::Item(ItemName::new(text)?)),
        }
    }
}

/// `--press <index>` opens one of the frontmost application's own menus;
/// `--press <name>` opens the real menu behind a mirrored item. Told apart by
/// shape, not by a tag: an index is never a valid item name in the configs
/// that use this — it is what a config's own menu-opening helper passed.
#[derive(Debug, Clone, PartialEq, Eq, Spelling)]
#[spelling(error = InvalidName, serde)]
pub enum PressTarget {
    #[spell("{}")]
    AppMenu(usize),
    #[spell("{}", rest = PressTarget::alias)]
    Alias(ItemName),
}

impl PressTarget {
    /// Whatever did not read as an index, which is a name or nothing.
    fn alias(text: &str) -> Result<Self, InvalidName> {
        ItemName::new(text).map(Self::Alias)
    }
}

/// Reconstructs an already-known value as if it had just been deserialized.
///
/// The CLI's hand-written argv deserializer resolves a couple of leaf values
/// — an event's [`Kind`], the [`Event`] a `--trigger` builds — through this
/// crate's own ordinary Rust API (`Kind::from_str`, `Kind::into_event`)
/// rather than by re-deriving their shape from a token, because their
/// derived [`Deserialize`] expects a self-describing tag matching the Rust
/// variant name, not `SketchyBar`'s own spelling (dashes, a missing `d`,
/// aliases). This hands such a value back through `serde`'s generic
/// [`DeserializeSeed`] contract via a JSON round trip, so the CLI never has
/// to re-match the type's variants by name itself.
///
/// # Errors
///
/// Only if `T`'s `Serialize` and `Deserialize` impls disagree about its
/// shape.
pub fn reify<'de, T: Serialize, S: DeserializeSeed<'de>>(
    value: &T,
    seed: S,
) -> Result<S::Value, serde_json::Error> {
    seed.deserialize(serde_json::to_value(value)?)
}

impl ComponentKind {
    /// The name every kind has, and where it goes.
    ///
    /// A bracket has no position of its own -- its frame comes from the items
    /// it names -- so it is filed under the left bucket and takes no space
    /// there, the same as any other bracket.
    #[must_use]
    pub fn placement(&self) -> (&ItemName, Position) {
        match self {
            Self::Item { name, position }
            | Self::Alias { name, position }
            | Self::Space { name, position }
            | Self::Graph { name, position }
            | Self::Slider { name, position } => (name, position.clone()),
            Self::Bracket { name, .. } => (name, Position::Left),
        }
    }
}

/// `before`/`after` in `--move <item> before|after <reference>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relative {
    Before,
    After,
}

/// What `--add` builds, and its own shape — `SketchyBar`'s own
/// `TYPE_ITEM`/`TYPE_BRACKET`/`TYPE_ALIAS`/`TYPE_SPACE`/`TYPE_GRAPH`/
/// `TYPE_SLIDER` in `defines.h`. One [`Request::Add`] variant per kind
/// rather than a shared shape, because their arguments genuinely differ: a
/// bracket takes members instead of a position, and nothing on the daemon
/// side can draw a space, graph or slider yet.
///
/// `Alias` is spelled out separately from `Item` even though rsbar treats
/// them identically — the mirror target is set afterward, by a chained
/// `--set <name> alias=...` — because that is what `--add` itself spells.
/// Which component `--add` names, before it is known where it goes.
///
/// The tag alone, so both front ends can read the word a config wrote and
/// then ask [`Self::build`] for the component — rather than each deciding for
/// itself what to do when the properties are incomplete, which is how the
/// CLI came to error on a missing position while Lua quietly put the item on
/// the left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Spelling)]
#[serde(rename_all = "snake_case")]
#[spelling(error = InvalidComponent)]
pub enum ComponentTag {
    #[spell("item")]
    Item,
    #[spell("alias")]
    Alias,
    #[spell("bracket")]
    Bracket,
    #[spell("space")]
    Space,
    #[spell("graph")]
    Graph,
    #[spell("slider")]
    Slider,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not `item`, `bracket`, `alias`, `space`, `graph` or `slider`")]
pub struct InvalidComponent(String);

impl ComponentTag {
    /// The component this names, from the properties the caller gave.
    ///
    /// The one place `--add` and `--set` differ: adding has to end up with a
    /// whole item, so a property with no meaningful default has to have been
    /// named, while setting one leaves whatever it does not mention alone.
    /// Which properties those are is not decided here — `position` is
    /// [`Geometry`]'s own `#[changes(required)]`, and this asks the patch.
    ///
    /// # Errors
    ///
    /// Returns [`Missing`] naming every required property the patch does not
    /// set: `position` for anything placed along the bar, `members` for a
    /// bracket, whose frame comes from what it names rather than from a
    /// bucket.
    ///
    /// # Panics
    ///
    /// Never in practice: the only unwrap is of a position this function has
    /// already established is present.
    pub fn build(self, name: ItemName, patch: &ItemPatch) -> Result<ComponentKind, Missing> {
        if self == Self::Bracket {
            let members = patch.members.clone().unwrap_or_default();
            return if members.is_empty() {
                Err(Missing {
                    what: <ItemPatch as patch::Fields>::WHAT,
                    fields: vec!["members"],
                })
            } else {
                Ok(ComponentKind::Bracket { name, members })
            };
        }
        // Asked of the patch rather than read off it, so a property that
        // becomes required later is required here with no edit.
        if let Some(missing) = patch.missing() {
            return Err(missing);
        }
        let position = patch
            .geometry
            .as_ref()
            .and_then(|geometry| geometry.position.clone())
            .expect("`missing` reported none absent");
        Ok(match self {
            Self::Item => ComponentKind::Item { name, position },
            Self::Alias => ComponentKind::Alias { name, position },
            Self::Space => ComponentKind::Space { name, position },
            Self::Graph => ComponentKind::Graph { name, position },
            Self::Slider => ComponentKind::Slider { name, position },
            Self::Bracket => unreachable!("answered above"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentKind {
    Item {
        name: ItemName,
        position: Position,
    },
    Alias {
        name: ItemName,
        position: Position,
    },
    /// No position: a bracket's frame comes from its members.
    Bracket {
        name: ItemName,
        members: Vec<Selector>,
    },
    Space {
        name: ItemName,
        position: Position,
    },
    Graph {
        name: ItemName,
        position: Position,
    },
    Slider {
        name: ItemName,
        position: Position,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Request {
    #[serde(rename = "bar")]
    SetBar(BarPatch),
    Add(ComponentKind),
    /// `--add event <name> [<NSDistributedNotificationName>]`.
    ///
    /// Not a [`ComponentKind`]: an event has no name-and-position to place,
    /// and nothing is drawn. A name alone declares the event, which is what
    /// makes `--trigger <name>` a deliberate act rather than a typo nobody
    /// notices. A name with a notification also bridges it, so the event
    /// fires whenever any process on this machine posts that distributed
    /// notification -- the difference between an event this config fires and
    /// one the system does.
    AddEvent {
        name: EventName,
        notification: Option<NotificationName>,
    },
    /// `--set <selector> ...`. A [`Selector::Pattern`] is resolved against
    /// the daemon's own live item list, not by whoever built this request —
    /// see [`Selector`]'s own doc comment for why only the daemon may.
    ///
    /// Boxed: an item patch is a dozen options and dwarfs every other
    /// variant, so every request would be as big as the largest one.
    Set(Selector, Box<ItemPatch>),
    /// `--remove <selector>`, resolved the same way as [`Request::Set`].
    Remove(Selector),
    /// `--default ...`: properties the daemon copies onto every item added
    /// from here on, the way `SketchyBar`'s own `default_item` does.
    ///
    /// Never expanded into a fully-populated [`ItemPatch`] by a client: only
    /// the fields a `--default` invocation actually named are `Some` here,
    /// same as [`Request::Set`]'s own patch, so the daemon's damage tracking
    /// still sees only what really changed.
    #[serde(rename = "default")]
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
    #[serde(rename = "update")]
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
    /// `--press <index-or-alias-name>`, told apart by shape — see
    /// [`PressTarget`].
    Press(PressTarget),
    #[serde(rename = "exit")]
    Shutdown,
}

/// What the daemon currently believes the bar looks like, and — through the
/// patch derived from it — everything a config can set about it.
///
/// One declaration, not two: [`BarPatch`] is generated from this, so adding a
/// bar property is one line here and the patch field, its `skip_serializing_if`,
/// its per-field accessor and its entry in the change record all follow.
// A report, not a configuration object: every flag the bar has is on it by
// definition, so grouping them into sub-structs would only make a caller
// reassemble what it asked for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Changes)]
#[changes(name = BarPatch, what = "the bar")]
#[changes(derive(Debug, Clone, Default, PartialEq))]
pub struct BarState {
    pub height: f64,
    /// `SketchyBar` calls this `position`, and a config writes that; `edge`
    /// is this crate's own name for it and both are accepted.
    #[serde(alias = "position")]
    pub edge: Edge,
    /// ARGB.
    pub color: Color,
    pub margin: f64,
    pub y_offset: f64,
    pub corner_radius: f64,
    pub blur_radius: i32,
    /// Whether the bar sits above the system menu bar or below it.
    #[changes(as = BoolChange)]
    pub topmost: Boolish,
    #[changes(as = BoolChange)]
    pub hidden: Boolish,
    /// How many displays have a panel. Counted by the daemon, so a config
    /// cannot set it -- see [`Self::display`] for the one it can.
    #[changes(skip)]
    pub displays: usize,
    /// Space before the first item and after the last, which is not the same
    /// as [`Self::margin`]: margin insets the bar from the screen edge, this
    /// insets the items from the bar.
    pub padding_left: f64,
    pub padding_right: f64,
    /// Which displays the bar appears on.
    pub display: DisplayTarget,
    /// Whether the bar stays put across a space switch.
    #[changes(as = BoolChange)]
    pub sticky: Boolish,
    /// Whether the bar draws over a fullscreen app.
    #[changes(as = BoolChange)]
    pub show_in_fullscreen: Boolish,
    /// The gap the centre buckets leave around the notch. Built-in display
    /// only, which is the only one that has one.
    pub notch_width: f64,
    /// Added to the bar's frame on the built-in display.
    pub notch_offset: f64,
    /// Overrides the bar's height on the built-in display, when above zero.
    pub notch_display_height: f64,
}

/// An item's on-screen geometry and background, mirroring the nesting
/// `SketchyBar`'s own `--query` uses (`bar_item_serialize` in `bar_item.c`)
/// closely enough that a config's own field paths — `geometry.drawing`,
/// `geometry.position` — work unchanged.
///
/// The nesting is the state's, not the patch's: [`GeometryPatch`] is
/// flattened into [`ItemPatch`], so a config still writes `padding_left=4`
/// with no `geometry.` in front of it. Two shapes for two jobs, from one
/// declaration — `--query` returns state, and a state's shape is what a
/// config reads back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Changes)]
#[changes(name = GeometryPatch, what = "an item")]
#[changes(derive(Debug, Clone, Default, PartialEq))]
pub struct Geometry {
    #[changes(as = BoolChange)]
    pub drawing: Boolish,
    /// Where along the bar this item sits.
    ///
    /// Required to *build* an item and optional to *change* one, which is the
    /// whole difference between `--add` and `--set`: an add with no position
    /// has nowhere to put the item, while a set with none means "leave it
    /// where it is". [`Position`] has no [`Default`] for exactly this reason.
    #[changes(required)]
    pub position: Position,
    pub y_offset: f64,
    pub padding_left: f64,
    pub padding_right: f64,
    /// A fixed width, or `None` for measured — `SketchyBar` prints `-1` for
    /// the same thing, which is `bar_item->has_const_width` being false.
    #[changes(as = f64)]
    pub width: Option<f64>,
    /// Which displays this item appears on.
    pub display: DisplayTarget,
    #[changes(as = BackgroundPatch)]
    pub background: Background,
}

/// An item's scripting state, mirroring `SketchyBar`'s own `scripting` key.
///
/// Flattened into [`ItemPatch`] the same way [`Geometry`] is: a config writes
/// `script=...`, and `--query` reports it under `scripting`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, Changes)]
#[changes(name = ScriptingPatch, what = "an item")]
#[changes(derive(Debug, Clone, Default, PartialEq))]
pub struct Scripting {
    /// Run on every update. Receives `RSBAR_NAME`, `RSBAR_SENDER` and the
    /// event's payload as named variables. An empty string removes it.
    #[changes(as = String)]
    pub script: Option<String>,
    /// Run when this item is clicked. Receives the same, plus `RSBAR_BUTTON`
    /// and `RSBAR_MODIFIERS`. An empty string removes it.
    #[changes(as = String)]
    pub click_script: Option<String>,
    /// Seconds between routine updates. Zero means "only on subscribed
    /// events".
    pub update_freq: u32,
    /// Whether the item runs its script and receives events at all.
    ///
    /// Distinct from [`Geometry::drawing`], which only stops it being drawn:
    /// a hidden item that still updates costs work nobody can see, and an
    /// item drawn from a value someone else sets wants the opposite.
    #[changes(as = BoolChange)]
    pub updates: Boolish,
}

/// An item's popup, as `--query` reports it -- and, through the patch derived
/// from it, everything a config can set about one.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, Changes)]
#[changes(name = PopupPatch, what = "an item's popup")]
#[changes(derive(Debug, Clone, Default, PartialEq))]
pub struct PopupState {
    #[changes(as = BoolChange)]
    pub drawing: Boolish,
    /// Rows run left to right instead of stacking.
    #[changes(as = BoolChange)]
    pub horizontal: Boolish,
    pub align: PopupAlign,
    #[changes(as = BoolChange)]
    pub topmost: Boolish,
    /// Each row's height.
    pub height: f64,
    /// Gap between the host item and the popup.
    pub y_offset: f64,
    #[changes(as = BackgroundPatch)]
    pub background: Background,
}

/// What the daemon currently believes one item looks like, and — through the
/// patch derived from it — everything a config can set about it.
///
/// One declaration, not two. `--query` returns *this*, never a patch: the
/// nested `geometry`/`scripting`/`popup` shape a config reads back is this
/// struct's own, while [`ItemPatch`] flattens the first two so a config still
/// writes `padding_left=4`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Changes)]
#[changes(name = ItemPatch, what = "an item")]
#[changes(derive(Debug, Clone, Default, PartialEq))]
#[changes(gap("slider" = "a slider's own fill, knob and track; nothing draws a slider yet"))]
#[changes(gap("graph" = "a graph's line and fill; nothing plots a graph yet"))]
pub struct ItemState {
    /// Identifies rather than configures, so no patch carries it:
    /// [`ItemPatch::construct`] takes it as an argument instead.
    #[changes(skip)]
    pub name: ItemName,
    #[changes(as = GeometryPatch, flatten)]
    pub geometry: Geometry,
    /// Always present, even on an item with no popup, because a config reads
    /// it without checking -- `overflow:query().popup.drawing == "on"` -- and
    /// a missing key there is a crash rather than a false.
    #[changes(as = PopupPatch)]
    pub popup: PopupState,
    /// The glyph half of the item.
    #[changes(as = RunPatch)]
    pub icon: Run,
    /// The text half of the item.
    #[changes(as = RunPatch)]
    pub label: Run,
    #[changes(as = ScriptingPatch, flatten)]
    pub scripting: Scripting,
    /// Replaced by `--subscribe`, not by a property, so no patch carries it.
    #[changes(skip)]
    pub events: Vec<Kind>,
    /// A menu bar item to mirror, as `Owner,Name` — the form
    /// `--query menu-items` lists. An empty string stops mirroring.
    ///
    /// A config may also write `alias = { color = ... }` to tint what it
    /// mirrors; rsbar has no field for the tint, so that table is named and
    /// dropped — see [`patch::name_or_tinted`].
    #[changes(as = String, read_with = patch::name_or_tinted)]
    pub alias: Option<String>,
    /// The items this one draws behind, as one surface. An empty list stops
    /// it being a bracket.
    ///
    /// A [`Selector::Pattern`] in the patch is resolved against the daemon's
    /// own live item list at apply time, not by whoever built it: a config
    /// still adding items must never race a client's stale snapshot.
    #[changes(as = Vec<Selector>)]
    pub members: Vec<ItemName>,
    /// Which space a `space` item stands for. The patch spells it doubly
    /// optional -- the outer layer is "did the request say anything", the
    /// inner one is the value -- so a config can unset it.
    pub associated_space: Option<std::num::NonZeroU64>,
    /// A slider's fill, 0-100.
    pub percentage: u8,
    /// A slider's knob.
    #[changes(as = RunPatch)]
    pub knob: Run,
    /// A space item's colour when it is the selected one.
    pub highlight_color: Color,
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
    use async_mach_ports::Codec as _;

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
    fn a_display_target_parses_what_a_config_writes() {
        assert_eq!("all".parse(), Ok(DisplayTarget::All));
        assert_eq!("ALL".parse(), Ok(DisplayTarget::All));
        assert_eq!(
            "2".parse(),
            Ok(DisplayTarget::Index(std::num::NonZeroU32::new(2).unwrap()))
        );
        // Rejected rather than quietly meaning "all", which is what the
        // daemon's own parser used to do: a typo now fails at the client with
        // the offending text named, and an invalid one never reaches the bar.
        assert!("0".parse::<DisplayTarget>().is_err());
        assert!("second".parse::<DisplayTarget>().is_err());
        assert!("".parse::<DisplayTarget>().is_err());
    }

    #[test]
    fn a_popup_alignment_parses_what_a_config_writes() {
        assert_eq!("centre".parse(), Ok(PopupAlign::Center));
        assert_eq!("r".parse(), Ok(PopupAlign::Right));
        assert!("middle".parse::<PopupAlign>().is_err());
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
    fn requests_round_trip_through_the_wire_format() {
        let request = Request::Set(
            Selector::Name(ItemName::new("clock").unwrap()),
            Box::new(ItemPatch {
                label: Some(RunPatch {
                    text: Some("09:41".into()),
                    color: Some(Color(0xffff_ffff)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        );
        let bytes = wire::MessagePack.encode(&request).unwrap();
        assert_eq!(
            wire::MessagePack.decode::<Request>(&bytes).unwrap(),
            request
        );
    }

    #[test]
    fn every_hand_written_codec_round_trips_through_the_wire_format() {
        // Every type here spells itself out for the CLI's sake -- by hand
        // once, now from one `Spelling` table -- so every one needs both
        // halves checked against the format that actually crosses the socket
        // rather than against JSON. This caught
        // two real bugs under postcard, where a Serialize and a Deserialize
        // disagreeing about the shape encoded fine and decoded to nothing;
        // MessagePack reports that mismatch instead of hiding it, which is
        // why the test is kept and merely retargeted.
        let requests = [
            Request::Add(ComponentKind::Item {
                name: ItemName::new("clock").unwrap(),
                position: Position::CenterLeft,
            }),
            Request::Add(ComponentKind::Bracket {
                name: ItemName::new("group").unwrap(),
                members: vec![Selector::Pattern(r"menu\..*".into())],
            }),
            Request::SetBar(BarPatch {
                edge: Some(Edge::Bottom),
                color: Some(Color(0xff00_ff00)),
                // Every boolean a patch spells is a `BoolChange`, and a
                // `toggle` has to survive the trip as itself.
                hidden: Some(BoolChange::Toggle),
                topmost: Some(BoolChange::False),
                sticky: Some(BoolChange::True),
                ..Default::default()
            }),
            Request::Set(
                Selector::Name(ItemName::new("clock").unwrap()),
                Box::new(ItemPatch {
                    geometry: Some(GeometryPatch {
                        position: Some(Position::Right),
                        ..Default::default()
                    }),
                    icon: Some(RunPatch {
                        font: Some(FontSpec::parse("Menlo:Bold:15")),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            ),
            Request::Remove(Selector::Pattern(r"clock.*".into())),
            Request::Query(Query::Item(ItemName::new("clock").unwrap())),
            Request::Query(Query::MenuItems),
            Request::Press(PressTarget::AppMenu(0)),
            Request::Press(PressTarget::Alias(
                ItemName::new("Amphetamine,Amphetamine").unwrap(),
            )),
            Request::AddEvent {
                name: "demo".parse().unwrap(),
                notification: None,
            },
            Request::AddEvent {
                name: "demo".parse().unwrap(),
                notification: Some("com.example.thing".parse().unwrap()),
            },
            Request::Subscribe {
                name: ItemName::new("clock").unwrap(),
                // A plain built-in, a scoped one and a custom name. A `Kind`
                // spells itself as its name on the wire, so the scoped
                // variant's `()` must not put a byte there either.
                events: vec![
                    Kind::VolumeChanged,
                    Kind::MouseClicked(()),
                    Kind::Custom("demo".into()),
                ],
            },
            Request::Trigger(Event::Custom(event::Custom {
                name: "demo".into(),
                vars: std::collections::BTreeMap::from([("VAR".to_owned(), "1".to_owned())]),
            })),
        ];
        for request in requests {
            let bytes = wire::MessagePack.encode(&request).unwrap();
            assert_eq!(
                wire::MessagePack.decode::<Request>(&bytes).unwrap(),
                request,
                "{request:?}"
            );
        }
    }

    /// One value's worth of the property the [`Spelling`] derive exists for.
    ///
    /// Printing it and reading it back must be a round trip, and so must the
    /// wire codec -- the two halves that used to be written separately and
    /// twice came apart in silence.
    fn spells<T>(values: &[T])
    where
        T: fmt::Display
            + FromStr
            + Serialize
            + serde::de::DeserializeOwned
            + PartialEq
            + fmt::Debug,
        <T as FromStr>::Err: fmt::Debug,
    {
        for value in values {
            let printed = value.to_string();
            assert_eq!(&printed.parse::<T>().unwrap(), value, "{printed}");
            let bytes = wire::MessagePack.encode(value).unwrap();
            assert_eq!(
                &wire::MessagePack.decode::<T>(&bytes).unwrap(),
                value,
                "{printed}"
            );
            // And the three agree about *which* word, not merely that each
            // survives its own trip: what crosses the wire is the spelling
            // printed, which is the whole claim.
            assert_eq!(
                wire::MessagePack.decode::<String>(&bytes).unwrap(),
                printed,
                "{printed}"
            );
        }
    }

    #[test]
    fn one_spelling_drives_display_from_str_and_the_wire() {
        // The whole point of the derive: the words are declared once, so
        // there is no second copy for the first to disagree with. Every
        // sample list below is checked exhaustively by a `match` with no
        // wildcard, so a variant added without a spelling fails to compile
        // here rather than slipping through.
        let alignments = [PopupAlign::Left, PopupAlign::Center, PopupAlign::Right];
        for value in &alignments {
            match value {
                PopupAlign::Left | PopupAlign::Center | PopupAlign::Right => {}
            }
        }
        spells(&alignments);

        let edges = [Edge::Top, Edge::Bottom];
        for value in &edges {
            match value {
                Edge::Top | Edge::Bottom => {}
            }
        }
        spells(&edges);

        let changes = [BoolChange::True, BoolChange::False, BoolChange::Toggle];
        for value in &changes {
            match value {
                BoolChange::True | BoolChange::False | BoolChange::Toggle => {}
            }
        }
        spells(&changes);

        let displays = [
            DisplayTarget::All,
            DisplayTarget::Index(std::num::NonZeroU32::new(2).unwrap()),
        ];
        for value in &displays {
            match value {
                DisplayTarget::All | DisplayTarget::Index(_) => {}
            }
        }
        spells(&displays);

        let tags = [
            ComponentTag::Item,
            ComponentTag::Alias,
            ComponentTag::Bracket,
            ComponentTag::Space,
            ComponentTag::Graph,
            ComponentTag::Slider,
        ];
        for value in &tags {
            match value {
                ComponentTag::Item
                | ComponentTag::Alias
                | ComponentTag::Bracket
                | ComponentTag::Space
                | ComponentTag::Graph
                | ComponentTag::Slider => {}
            }
        }
        spells(&tags);
    }

    #[test]
    fn a_spelling_that_carries_a_payload_round_trips_too() {
        let positions = [
            Position::Left,
            Position::CenterLeft,
            Position::Center,
            Position::CenterRight,
            Position::Right,
            // Mixed case deliberately: the host's name is read off the
            // original text, not the folded copy the table matches against.
            Position::Popup(ItemName::new("Volume Control").unwrap()),
        ];
        for value in &positions {
            match value {
                Position::Left
                | Position::CenterLeft
                | Position::Center
                | Position::CenterRight
                | Position::Right
                | Position::Popup(_) => {}
            }
        }
        spells(&positions);
    }

    #[test]
    fn a_spelling_computed_from_a_payload_round_trips_too() {
        // The escape hatch: a variant whose word is worked out from what it
        // carries. Declared once as `"/{}/"` or `"popup.{}"`, so the prefix
        // that prints it is the prefix that recognises it.
        let selectors = [
            Selector::Name(ItemName::new("clock").unwrap()),
            Selector::Pattern(r"menu\..*".into()),
        ];
        for value in &selectors {
            match value {
                Selector::Name(_) | Selector::Pattern(_) => {}
            }
        }
        spells(&selectors);

        let queries = [
            Query::Bar,
            Query::Items,
            Query::Item(ItemName::new("clock").unwrap()),
            Query::MenuItems,
            Query::AppMenus,
            Query::Defaults,
        ];
        for value in &queries {
            match value {
                Query::Bar
                | Query::Items
                | Query::Item(_)
                | Query::MenuItems
                | Query::AppMenus
                | Query::Defaults => {}
            }
        }
        spells(&queries);

        let presses = [
            PressTarget::AppMenu(3),
            PressTarget::Alias(ItemName::new("Control Centre,Clock").unwrap()),
        ];
        for value in &presses {
            match value {
                PressTarget::AppMenu(_) | PressTarget::Alias(_) => {}
            }
        }
        spells(&presses);
    }

    #[test]
    fn every_spelling_a_variant_declares_reads_back_as_that_variant() {
        // The alternates, which `Display` never prints and so the round trip
        // above never reaches. Declared beside the canonical form rather than
        // in a second table, which is what makes this a list of samples and
        // not the specification.
        assert_eq!("l".parse(), Ok(Position::Left));
        assert_eq!("centre-left".parse(), Ok(Position::CenterLeft));
        assert_eq!("center_right".parse(), Ok(Position::CenterRight));
        assert_eq!("menu-items".parse(), Ok(Query::MenuItems));
        assert_eq!("default_menu_items".parse(), Ok(Query::MenuItems));
        assert_eq!("app-menus".parse(), Ok(Query::AppMenus));
        assert_eq!("BOTTOM".parse(), Ok(Edge::Bottom));
        assert_eq!("centre".parse(), Ok(PopupAlign::Center));
        // And a spelling nothing declares is refused rather than guessed at.
        assert!("sideways".parse::<Position>().is_err());
        assert!("middle".parse::<PopupAlign>().is_err());
        assert!("sideways".parse::<Edge>().is_err());
        assert!("0".parse::<DisplayTarget>().is_err());
    }

    #[test]
    fn an_item_name_off_the_wire_is_held_to_the_rule_a_typed_one_is() {
        // The newtype's whole job, at the one boundary where the derived
        // reader used to skip it.
        assert!(serde_json::from_str::<ItemName>(r#""clock""#).is_ok());
        assert!(serde_json::from_str::<ItemName>(r#""""#).is_err());
        assert!(serde_json::from_str::<ItemName>("\"two\\nlines\"").is_err());
    }

    #[test]
    fn a_boolean_reads_every_spelling_a_config_writes() {
        for on in ["on", "true", "yes", "1", "ON", "True"] {
            assert_eq!(on.parse(), Ok(Boolish(true)), "{on}");
        }
        for off in ["off", "false", "no", "0", "OFF"] {
            assert_eq!(off.parse(), Ok(Boolish(false)), "{off}");
        }
        assert!("maybe".parse::<Boolish>().is_err());
        // `toggle` is a change, not a value: only the patch side takes it.
        assert!("toggle".parse::<Boolish>().is_err());
        assert_eq!("toggle".parse(), Ok(BoolChange::Toggle));
        assert_eq!("yes".parse(), Ok(BoolChange::True));
        assert_eq!(Boolish(true).to_string(), "on");
        assert!(bool::from(Boolish(true)));
    }

    #[test]
    fn a_change_that_changes_nothing_is_not_one() {
        // Damage tracking is honest only if a request mentioning a property
        // is not the same as the property changing.
        assert_eq!(BoolChange::True.change(Boolish(true)), None);
        assert_eq!(BoolChange::False.change(Boolish(false)), None);
        assert_eq!(BoolChange::True.change(Boolish(false)), Some(Boolish(true)));
        // A flip always is one.
        assert_eq!(
            BoolChange::Toggle.change(Boolish(true)),
            Some(Boolish(false))
        );
        assert_eq!(
            BoolChange::Toggle.change(Boolish(false)),
            Some(Boolish(true))
        );
    }

    #[test]
    fn booleans_round_trip_through_the_wire_format_as_themselves() {
        for value in [Boolish(true), Boolish(false)] {
            let bytes = wire::MessagePack.encode(&value).unwrap();
            assert_eq!(wire::MessagePack.decode::<Boolish>(&bytes).unwrap(), value);
        }
        for value in [BoolChange::True, BoolChange::False, BoolChange::Toggle] {
            let bytes = wire::MessagePack.encode(&value).unwrap();
            assert_eq!(
                wire::MessagePack.decode::<BoolChange>(&bytes).unwrap(),
                value
            );
        }
    }

    /// A nested patch and a substituted leaf in one struct, which is the
    /// combination `struct-patch` could not express.
    #[derive(Debug, Clone, PartialEq, Default, Changes)]
    #[changes(name = OuterPatch, what = "a test patch")]
    #[changes(derive(Debug, Clone, Default, PartialEq))]
    struct Outer {
        #[changes(as = BackgroundPatch)]
        background: Background,
        #[changes(as = BoolChange)]
        on: Boolish,
        label: String,
    }

    #[test]
    fn a_patch_that_would_change_nothing_reports_nothing() {
        let current = Outer {
            background: Background {
                drawing: Boolish(true),
                color: Color(0xff11_2233),
                ..Background::default()
            },
            on: Boolish(true),
            label: "clock".to_owned(),
        };
        // Every field named, every value the one already there.
        let patch = OuterPatch {
            background: Some(BackgroundPatch {
                drawing: Some(BoolChange::True),
                color: Some(Color(0xff11_2233)),
                ..BackgroundPatch::default()
            }),
            on: Some(BoolChange::True),
            label: Some("clock".to_owned()),
        };
        assert_eq!(patch.changes(&current), None);
    }

    #[test]
    fn a_nested_patch_changes_only_what_it_moves() {
        let current = Outer {
            background: Background {
                drawing: Boolish(true),
                corner_radius: 4.0,
                ..Background::default()
            },
            on: Boolish(false),
            label: "clock".to_owned(),
        };
        let patch = OuterPatch {
            background: Some(BackgroundPatch {
                corner_radius: Some(8.0),
                // Named, and already what it is.
                drawing: Some(BoolChange::True),
                ..BackgroundPatch::default()
            }),
            ..OuterPatch::default()
        };
        let next = patch.changes(&current).expect("the radius moved");
        assert_eq!(
            next.background,
            Background {
                corner_radius: 8.0,
                ..current.background
            }
        );
        assert_eq!(next.on, Boolish(false));
        assert_eq!(next.label, "clock");
    }

    #[test]
    fn a_flip_is_always_a_change_and_needs_what_is_there_to_say_what_of() {
        let off = Outer::default();
        let patch = OuterPatch {
            on: Some(BoolChange::Toggle),
            ..OuterPatch::default()
        };
        assert_eq!(patch.changes(&off).map(|next| next.on), Some(Boolish(true)));
        let on = Outer {
            on: Boolish(true),
            ..Outer::default()
        };
        assert_eq!(patch.changes(&on).map(|next| next.on), Some(Boolish(false)));
    }

    #[test]
    fn a_patch_does_not_pay_for_the_properties_nobody_set() {
        // `to_vec_named` writes a key for every field, so an unskipped patch
        // spends its whole size naming properties that are `None`. Bounds
        // rather than exact figures: this is a regression guard, not a
        // snapshot of the encoder.
        let encode = |value: &Request| rmp_serde::to_vec_named(value).unwrap().len();

        let query = encode(&Request::Query(Query::Item(
            ItemName::new("clock").unwrap(),
        )));
        let add = encode(&Request::Add(ComponentKind::Item {
            name: ItemName::new("clock").unwrap(),
            position: Position::Right,
        }));
        let set = encode(&Request::Set(
            Selector::Name(ItemName::new("clock").unwrap()),
            Box::new(ItemPatch {
                label: Some(RunPatch {
                    text: Some("12:00".into()),
                    color: Some(Color(0xffff_0000)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        ));
        let empty = encode(&Request::Set(
            Selector::Name(ItemName::new("clock").unwrap()),
            Box::default(),
        ));

        // An empty patch is the sharpest test: with nothing set there is
        // nothing to write, so all that is left is the request around it.
        assert!(empty < 24, "an empty patch costs {empty} bytes");
        assert!(set < 80, "a two-property set costs {set} bytes");
        assert!(add < 60, "an add costs {add} bytes");
        assert!(query < 30, "a query costs {query} bytes");
        println!("query {query} / add {add} / set {set} / empty {empty}");
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
    fn run_patch_names_an_unknown_property_without_losing_the_rest() {
        // A stray key is reported and dropped rather than failing the whole
        // patch: one property rsbar has no field for must not stop the other
        // forty from coming up. See [`crate::patch`].
        let patch = serde_json::from_str::<RunPatch>(r#"{"wat": 1, "color": "0xffff0000"}"#)
            .expect("a stray key is named, not rejected");
        assert_eq!(patch.color, Some(Color(0xffff_0000)));
    }

    fn an_item() -> ItemState {
        ItemState {
            name: ItemName::new("clock").unwrap(),
            geometry: Geometry {
                drawing: Boolish(true),
                position: Position::Right,
                y_offset: 0.0,
                padding_left: 2.0,
                padding_right: 2.0,
                width: None,
                display: DisplayTarget::All,
                background: Background::default(),
            },
            popup: PopupState::default(),
            icon: Run::default(),
            label: Run::default(),
            scripting: Scripting::default(),
            events: Vec::new(),
            alias: None,
            members: Vec::new(),
            associated_space: None,
            percentage: 0,
            knob: Run::default(),
            highlight_color: Color::default(),
        }
    }

    #[test]
    fn a_query_answers_with_the_nesting_a_config_reads_back() {
        // `--query` returns state, never a patch: the nesting a config walks
        // (`overflow:query().geometry.drawing`) is this struct's own shape.
        // Pinned rather than described, because the patch beside it is
        // deliberately flat and the two must not be confused for each other.
        let json = serde_json::to_value(an_item()).unwrap();
        for key in ["name", "geometry", "popup", "icon", "label", "scripting"] {
            assert!(json.get(key).is_some(), "{key}");
        }
        let geometry = json.get("geometry").expect("nested");
        for key in [
            "drawing",
            "position",
            "y_offset",
            "padding_left",
            "padding_right",
            "width",
            "background",
        ] {
            assert!(geometry.get(key).is_some(), "geometry.{key}");
        }
        assert!(json.get("scripting").unwrap().get("script").is_some());
        // And the flat spellings belong to the patch, not to this.
        assert!(json.get("padding_left").is_none());
    }

    #[test]
    fn an_item_patch_is_flat_where_the_state_is_nested() {
        // One declaration, two shapes: `--set clock padding_left=4` names a
        // key that lives on `Geometry`, and no `geometry.` prefix appears on
        // the wire.
        let patch: ItemPatch = serde_json::from_str(
            r#"{"padding_left": 4, "script": "s", "label": {"color": "0xffff0000"}}"#,
        )
        .unwrap();
        assert_eq!(
            patch.geometry.as_ref().and_then(|g| g.padding_left),
            Some(4.0)
        );
        assert_eq!(
            patch.scripting.as_ref().and_then(|s| s.script.clone()),
            Some("s".to_owned())
        );
        let json = serde_json::to_value(&patch).unwrap();
        assert_eq!(
            json.get("padding_left").and_then(serde_json::Value::as_f64),
            Some(4.0)
        );
        assert!(json.get("geometry").is_none());
    }

    #[test]
    fn a_flattened_sub_patch_does_not_swallow_another_ones_keys() {
        // The failure `#[serde(flatten)]` would have produced: whichever
        // catch-all serde tried first took every key the others wanted. The
        // generated reader offers each key to each sub-patch in turn instead.
        let patch: ItemPatch =
            serde_json::from_str(r#"{"script": "a", "y_offset": 3, "update_freq": 5}"#).unwrap();
        assert_eq!(patch.geometry.and_then(|g| g.y_offset), Some(3.0));
        let scripting = patch.scripting.expect("scripting keys landed");
        assert_eq!(scripting.script.as_deref(), Some("a"));
        assert_eq!(scripting.update_freq, Some(5));
    }

    #[test]
    fn a_colour_is_spelled_either_way_on_the_way_in() {
        // A rule the derive applies to every field whose name carries
        // `color`, rather than an alias somebody remembered to write.
        let patch: RunPatch =
            serde_json::from_str(r#"{"colour": "0xffff0000", "highlight_colour": "0xff00ff00"}"#)
                .unwrap();
        assert_eq!(patch.color, Some(Color(0xffff_0000)));
        assert_eq!(patch.highlight_color, Some(Color(0xff00_ff00)));
        // And one canonical spelling on the way out.
        let json = serde_json::to_string(&patch).unwrap();
        assert!(json.contains(r#""color""#));
        assert!(!json.contains("colour"));
    }

    #[test]
    fn one_property_written_twice_folds_rather_than_replacing() {
        // `label=12:34 label.color=...` is the flat spelling of one table,
        // and reaches the reader as the key `label` twice.
        let mut patch = ItemPatch::default();
        let bare: RunPatch = serde_json::from_str(r#""12:34""#).unwrap();
        let nested: RunPatch = serde_json::from_str(r#"{"color": "0xff00ff00"}"#).unwrap();
        patch.label = Some(bare.folded(nested));
        let label = patch.label.expect("set");
        assert_eq!(label.text.as_deref(), Some("12:34"));
        assert_eq!(label.color, Some(Color(0xff00_ff00)));
    }

    #[test]
    fn adding_needs_a_position_and_setting_does_not() {
        // The whole of task #44 in one assertion: the same patch type serves
        // both, and the difference is which operation is asked of it.
        let empty = ItemPatch::default();
        let missing = ComponentTag::Item
            .build(ItemName::new("clock").unwrap(), &empty)
            .expect_err("an add with no position has nowhere to put it");
        assert_eq!(missing.fields, vec!["position"]);
        assert_eq!(
            missing.to_string(),
            "`position`: required to build an item, and not set"
        );
        // The same patch is a perfectly good `--set`: it changes nothing.
        assert_eq!(empty.changes(&an_item()), None);

        let placed = ItemPatch {
            geometry: Some(GeometryPatch {
                position: Some(Position::Right),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            ComponentTag::Item.build(ItemName::new("clock").unwrap(), &placed),
            Ok(ComponentKind::Item {
                name: ItemName::new("clock").unwrap(),
                position: Position::Right,
            })
        );
    }

    #[test]
    fn a_bracket_needs_members_rather_than_a_position() {
        let bare = ItemPatch::default();
        assert_eq!(
            ComponentTag::Bracket
                .build(ItemName::new("group").unwrap(), &bare)
                .unwrap_err()
                .fields,
            vec!["members"]
        );
    }

    #[test]
    fn construction_fills_in_everything_a_patch_did_not_say() {
        // Exhaustive by construction: a property added to `ItemState` has to
        // appear in the generated literal or this does not compile.
        let patch = ItemPatch {
            geometry: Some(GeometryPatch {
                position: Some(Position::Center),
                padding_left: Some(6.0),
                ..Default::default()
            }),
            scripting: Some(ScriptingPatch {
                script: Some("update".to_owned()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let built = patch
            .construct(ItemName::new("clock").unwrap(), Vec::new())
            .expect("a position was named");
        assert_eq!(built.geometry.position, Position::Center);
        assert!((built.geometry.padding_left - 6.0).abs() < f64::EPSILON);
        assert_eq!(built.scripting.script.as_deref(), Some("update"));
        assert_eq!(built.name.as_str(), "clock");
        assert_eq!(built.popup, PopupState::default());
    }

    #[test]
    fn an_empty_script_unsets_it_rather_than_setting_it_to_nothing() {
        let current = ItemState {
            scripting: Scripting {
                script: Some("old".to_owned()),
                ..Scripting::default()
            },
            ..an_item()
        };
        let patch: ItemPatch = serde_json::from_str(r#"{"script": ""}"#).unwrap();
        let next = patch.changes(&current).expect("it moved");
        assert_eq!(next.scripting.script, None);
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

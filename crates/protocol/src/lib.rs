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

/// Which edge of the display the bar occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Edge {
    #[default]
    Top,
    Bottom,
}

/// Where an item sits along the bar.
///
/// The two centre-adjacent buckets exist so a config can put something beside
/// a centred item without it being re-centred along with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Position {
    #[default]
    Left,
    CenterLeft,
    Center,
    CenterRight,
    Right,
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
            _ => Err(InvalidPosition(s.to_owned())),
        }
    }
}

/// A partial update. `None` means "leave as it is" — the distinction from
/// "set to the default" is why every field is an `Option`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BarPatch {
    pub height: Option<f64>,
    pub edge: Option<Edge>,
    /// ARGB, the form the CLI parses `#rrggbb` into.
    pub color: Option<u32>,
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
}

/// A partial update to one item.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    pub drawing: Option<bool>,
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
    pub members: Option<Vec<ItemName>>,
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
}

// `struct_patch::Patch` compares each field to decide what a patch changed.
// For floats clippy calls that suspicious; here the values are config literals
// — a corner radius someone typed — not the result of arithmetic, so exact
// comparison is exactly right. The allow sits here because the lint fires
// inside the derive's own expansion, where an item-level allow does not reach.
#[allow(clippy::float_cmp)]
mod parts {
    use super::{Deserialize, Serialize};

    /// One half of an item's text: its glyph, or its label.
    ///
    /// A config writes this nested — `label.color`, or `{ label = { color = ... }
    /// }` — because it is one thing on the other side too. [`RunPatch`] is
    /// derived from it, so a new property is one line in one place and the two
    /// cannot drift.
    #[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, struct_patch::Patch)]
    #[patch(
        name = "RunPatch",
        attribute(derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize))
    )]
    pub struct Run {
        pub text: String,
        /// ARGB, the form the CLI parses `#rrggbb` into.
        pub color: u32,
        /// `Family:Style:Size`.
        pub font: String,
        /// Whether this half is drawn, independently of the item's own `drawing`
        /// — an item that shows its glyph but not its text.
        pub drawing: bool,
        /// Space either side of this half alone, on top of the item's own.
        pub padding_left: f64,
        pub padding_right: f64,
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
    #[patch(
        name = "BackgroundPatch",
        attribute(derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize))
    )]
    pub struct Background {
        /// Whether the surface is drawn. Separate from the item's own
        /// `drawing` so a config can turn a pill off and leave the text.
        pub drawing: bool,
        /// ARGB.
        pub color: u32,
        pub corner_radius: f64,
        /// A fixed height, or zero for the bar's own. A shorter surface is how a
        /// pill sits inside the bar with a margin above and below.
        pub height: f64,
        /// Inset from the item's own edges, so the surface can be tighter or
        /// wider than what it sits behind.
        pub padding_left: f64,
        pub padding_right: f64,
        /// ARGB.
        pub border_color: u32,
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    SetBar(BarPatch),
    AddItem {
        name: ItemName,
        position: Position,
    },
    SetItem {
        name: ItemName,
        /// Boxed: an item patch is a dozen options and dwarfs every other
        /// variant, so every request would be as big as the largest one.
        patch: Box<ItemPatch>,
    },
    RemoveItem(ItemName),
    /// Replaces the item's subscriptions.
    Subscribe {
        name: ItemName,
        events: Vec<Kind>,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BarState {
    pub height: f64,
    pub edge: Edge,
    pub color: u32,
    pub margin: f64,
    pub y_offset: f64,
    pub corner_radius: f64,
    pub blur_radius: i32,
    pub topmost: bool,
    pub hidden: bool,
    pub displays: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemState {
    pub name: ItemName,
    pub position: Position,
    pub icon: String,
    pub label: String,
    pub drawing: bool,
    pub script: Option<String>,
    pub click_script: Option<String>,
    pub update_freq: u32,
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
                    color: Some(0xffff_ffff),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };
        let bytes = postcard::to_allocvec(&request).unwrap();
        assert_eq!(postcard::from_bytes::<Request>(&bytes).unwrap(), request);
    }
}

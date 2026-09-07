//! The wire vocabulary shared by the rsbar daemon, its CLI and its Lua module.
//!
//! This crate is the protocol. Everything that crosses a process boundary is
//! defined here exactly once, so a client cannot drift from the daemon.

#![cfg(target_os = "macos")]

pub mod event;
pub mod style;

pub use event::Event;

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
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ItemName(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidName {
    #[error("an item name cannot be empty")]
    Empty,
    #[error("`{0}` is not a valid item name: use letters, digits, `_`, `-` or `.`")]
    Character(String),
}

impl ItemName {
    /// Names travel in env vars and are matched by clients, so the character
    /// set is restricted rather than accepting anything a shell survives.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidName`] for an empty name or one containing anything
    /// outside letters, digits, `_`, `-` and `.`.
    pub fn new(name: impl Into<String>) -> Result<Self, InvalidName> {
        let name = name.into();
        if name.is_empty() {
            return Err(InvalidName::Empty);
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        {
            return Err(InvalidName::Character(name));
        }
        Ok(Self(name))
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
}

/// A partial update to one item.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ItemPatch {
    pub icon: Option<String>,
    pub label: Option<String>,
    pub icon_font: Option<String>,
    pub label_font: Option<String>,
    pub icon_color: Option<u32>,
    pub label_color: Option<u32>,
    pub background_color: Option<u32>,
    pub corner_radius: Option<f64>,
    pub padding_left: Option<f64>,
    pub padding_right: Option<f64>,
    pub y_offset: Option<f64>,
    pub position: Option<Position>,
    pub drawing: Option<bool>,
    /// Run on every update. Receives `RSBAR_NAME`, `RSBAR_SENDER` and,
    /// where the event carries one, `RSBAR_INFO`.
    pub script: Option<String>,
    /// Seconds between routine updates. Zero means "only on subscribed
    /// events", which is the right default for anything event-driven.
    pub update_freq: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Query {
    Bar,
    Items,
    Item(ItemName),
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
        patch: ItemPatch,
    },
    RemoveItem(ItemName),
    /// Replaces the item's subscriptions.
    Subscribe {
        name: ItemName,
        events: Vec<Event>,
    },
    /// Fires an event now, as if a source had produced it.
    Trigger {
        event: Event,
        info: Option<String>,
    },
    /// Runs every item's script immediately, ignoring update frequency.
    UpdateAll,
    /// Re-runs the config from scratch, as a file change does.
    Reload,
    Query(Query),
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
    pub update_freq: u32,
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Bar(Box<BarState>),
    Items(Vec<ItemState>),
    Item(Box<ItemState>),
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
        assert_eq!(ItemName::new(""), Err(InvalidName::Empty));
        assert!(matches!(
            ItemName::new("a b"),
            Err(InvalidName::Character(_))
        ));
        assert!(matches!(
            ItemName::new("rm -rf /"),
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
            patch: ItemPatch {
                label: Some("09:41".into()),
                label_color: Some(0xffff_ffff),
                ..Default::default()
            },
        };
        let bytes = postcard::to_allocvec(&request).unwrap();
        assert_eq!(postcard::from_bytes::<Request>(&bytes).unwrap(), request);
    }
}

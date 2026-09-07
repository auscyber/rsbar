//! Items, as entities.
//!
//! Every component here is plain data. Nothing holds a `CFRetained` or any
//! other platform object, because a Bevy component must be `Send + Sync` and
//! CoreText and the window server are neither. The shaped text those fields
//! imply lives in [`crate::shaping`], a `NonSend` cache keyed by entity and
//! rebuilt from `Changed<…>` — which is also what makes the cache correct by
//! construction rather than by remembering to invalidate it.

use bevy_ecs::prelude::*;
use rsbar_protocol::style::{Color, FontSpec};
use rsbar_protocol::{ItemName, Kind, Position};
use std::collections::{BTreeSet, HashMap};

/// Marks an entity as a bar item.
#[derive(Component)]
pub struct Item;

/// Marks an item that predates the config run currently in flight.
///
/// A reload cannot simply clear the bar first: the config might fail, and a
/// broken edit should change nothing rather than leave an empty bar. So items
/// are marked instead, the mark is lifted from any item the config touches, and
/// what is still marked when the run *succeeds* is what the new config dropped.
#[derive(Component)]
pub struct Stale;

#[derive(Component, Debug, Clone, PartialEq, Eq, Hash)]
pub struct Name(pub ItemName);

/// One run of text with its own font and colour. Icon and label differ only in
/// which defaults they start from and where layout puts them.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct Icon(pub Run);

#[derive(Component, Debug, Clone, PartialEq)]
pub struct Label(pub Run);

#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub string: String,
    pub font: FontSpec,
    pub color: Color,
}

impl Run {
    #[must_use]
    pub fn new(font: &str, color: Color) -> Self {
        Self {
            string: String::new(),
            font: FontSpec::parse(font),
            color,
        }
    }

    /// Empty text takes no space, so an icon-only item has no phantom gap
    /// where its label would be.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.string.is_empty()
    }
}

#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Background {
    pub color: Color,
    pub corner_radius: f64,
}

#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Padding {
    pub left: f64,
    pub right: f64,
    /// Gap between icon and label when both are present.
    pub between: f64,
}

#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Offset(pub f64);

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement(pub Position);

/// Whether the item is drawn at all. An undrawn item keeps its state and its
/// subscriptions; it simply takes no space.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Drawing(pub bool);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct Script(pub String);

/// A routine update, in whole seconds.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Routine {
    pub every: u32,
    /// Seconds since this item last ran.
    pub elapsed: u32,
}

impl Routine {
    /// Advances a second, reporting whether the item is due.
    pub fn tick(&mut self) -> bool {
        if self.every == 0 {
            return false;
        }
        self.elapsed += 1;
        if self.elapsed >= self.every {
            self.elapsed = 0;
            return true;
        }
        false
    }
}

#[derive(Component, Debug, Clone, Default, PartialEq, Eq)]
pub struct Subscriptions(pub BTreeSet<Kind>);

/// Name to entity, so a request naming an item does not scan every one.
///
/// Kept in step by the systems that spawn and despawn; nothing else writes it.
#[derive(Resource, Default)]
pub struct Index(HashMap<ItemName, Entity>);

impl Index {
    #[must_use]
    pub fn get(&self, name: &ItemName) -> Option<Entity> {
        self.0.get(name).copied()
    }

    pub fn insert(&mut self, name: ItemName, entity: Entity) {
        self.0.insert(name, entity);
    }

    pub fn remove(&mut self, name: &ItemName) -> Option<Entity> {
        self.0.remove(name)
    }
}

/// Everything a new item starts as. Defaults match what a bar wants before a
/// config says otherwise: visible, no background, modest padding.
#[must_use]
pub fn bundle(name: ItemName, position: Position) -> impl Bundle {
    (
        Item,
        Name(name),
        Icon(Run::new("Menlo:Bold:15", Color::WHITE)),
        Label(Run::new("Menlo:Regular:13", Color::WHITE)),
        Background {
            color: Color::TRANSPARENT,
            corner_radius: 0.0,
        },
        Padding {
            left: 8.0,
            right: 8.0,
            between: 5.0,
        },
        Offset(0.0),
        Placement(position),
        Drawing(true),
        Routine {
            every: 0,
            elapsed: 0,
        },
        Subscriptions::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_frequency_never_comes_due() {
        let mut routine = Routine {
            every: 0,
            elapsed: 0,
        };
        // Event-driven items sit at zero; ticking them forever must do nothing.
        for _ in 0..100 {
            assert!(!routine.tick());
        }
    }

    #[test]
    fn a_frequency_fires_on_its_period_and_resets() {
        let mut routine = Routine {
            every: 3,
            elapsed: 0,
        };
        assert!(!routine.tick());
        assert!(!routine.tick());
        assert!(routine.tick(), "due on the third second");
        assert!(!routine.tick(), "and the clock restarts");
        assert!(!routine.tick());
        assert!(routine.tick());
    }

    #[test]
    fn every_second_fires_every_second() {
        let mut routine = Routine {
            every: 1,
            elapsed: 0,
        };
        assert!(routine.tick());
        assert!(routine.tick());
    }

    #[test]
    fn the_index_is_keyed_by_name() {
        // Name is the identity that survives a reload: the same name must map
        // back to the same entity, which is what lets a config update an item
        // in place rather than replacing it.
        let mut index = Index::default();
        let clock = ItemName::new("clock").unwrap();
        let entity = Entity::from_raw_u32(7).unwrap();

        assert_eq!(index.get(&clock), None);
        index.insert(clock.clone(), entity);
        assert_eq!(index.get(&clock), Some(entity));

        assert_eq!(index.remove(&clock), Some(entity));
        assert_eq!(index.get(&clock), None);
        assert_eq!(index.remove(&clock), None, "removing twice is not an error");
    }

    #[test]
    fn an_empty_run_contributes_no_width() {
        let run = Run::new("Menlo:Bold:15", Color::WHITE);
        assert!(run.is_empty(), "a fresh run has no text");
    }
}

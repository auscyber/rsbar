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
    /// Space either side of this half alone, on top of the item's own.
    pub padding_left: f64,
    pub padding_right: f64,
    /// Whether this half of the item is shown.
    ///
    /// Separate from the item's own `Drawing` so an icon can be shown without
    /// its label, or the other way round — an item that reveals its text on
    /// hover, or one that is only ever a glyph. Hiding a run takes its width
    /// out of the layout as well as its ink off the screen.
    pub drawing: bool,
}

/// The wire form of a run, so `struct_patch` can apply a [`rsbar_protocol::RunPatch`]
/// to it. Exhaustive both ways on purpose: a property added to one end has to
/// be written down at the other before this compiles.
impl From<&Run> for rsbar_protocol::Run {
    fn from(run: &Run) -> Self {
        Self {
            text: run.string.clone(),
            color: run.color.0,
            font: run.font.to_string(),
            drawing: run.drawing,
            padding_left: run.padding_left,
            padding_right: run.padding_right,
        }
    }
}

impl From<&rsbar_protocol::Run> for Run {
    fn from(run: &rsbar_protocol::Run) -> Self {
        Self {
            string: run.text.clone(),
            color: Color(run.color),
            font: FontSpec::parse(&run.font),
            drawing: run.drawing,
            padding_left: run.padding_left,
            padding_right: run.padding_right,
        }
    }
}

impl From<&Background> for rsbar_protocol::Background {
    fn from(background: &Background) -> Self {
        Self {
            drawing: background.drawing,
            color: background.color.0,
            corner_radius: background.corner_radius,
            height: background.height,
            padding_left: background.padding_left,
            padding_right: background.padding_right,
            border_color: background.border_color.0,
            border_width: background.border_width,
        }
    }
}

impl From<&rsbar_protocol::Background> for Background {
    fn from(background: &rsbar_protocol::Background) -> Self {
        Self {
            drawing: background.drawing,
            color: Color(background.color),
            corner_radius: background.corner_radius,
            height: background.height,
            padding_left: background.padding_left,
            padding_right: background.padding_right,
            border_color: Color(background.border_color),
            border_width: background.border_width,
        }
    }
}

impl Run {
    #[must_use]
    pub fn new(font: &str, color: Color) -> Self {
        Self {
            string: String::new(),
            font: FontSpec::parse(font),
            color,
            drawing: true,
            padding_left: 0.0,
            padding_right: 0.0,
        }
    }

    /// Empty or hidden text takes no space, so an icon-only item has no
    /// phantom gap where its label would be.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.string.is_empty() || !self.drawing
    }
}

#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Background {
    /// Whether the surface is drawn at all.
    ///
    /// Separate from the item's own [`Drawing`] so a config can turn a pill
    /// off and leave the text on it, which is what a bar does to mark an
    /// item inactive without moving anything.
    pub drawing: bool,
    pub color: Color,
    pub corner_radius: f64,
    /// A fixed height, or zero for the bar's own.
    ///
    /// A shorter surface than the bar is how a pill sits inside it with a
    /// margin above and below, which is what most bars actually look like.
    pub height: f64,
    /// Inset from the item's own edges, so the surface can be tighter or
    /// wider than the text it sits behind.
    pub padding_left: f64,
    pub padding_right: f64,
    pub border_color: Color,
    pub border_width: f64,
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
pub struct Script(pub std::sync::Arc<str>);

/// Run when this item is clicked, instead of the update script.
///
/// Separate from [`Script`] because the two answer different questions: one
/// keeps the item's contents current, the other acts on the user. An item can
/// have either, both, or neither.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ClickScript(pub std::sync::Arc<str>);

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

/// Which item something belongs to.
///
/// Both halves of an item's identity, because they answer different
/// questions and neither does the other's job. The entity is how the daemon
/// finds it again — stable across a reload, unlike a name, which a config can
/// drop and re-add. The name is what a script reads and a client asks by, and
/// is the only half that means anything outside this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemHandle {
    entity: Entity,
    name: ItemName,
}

impl ItemHandle {
    #[must_use]
    pub fn new(entity: Entity, name: ItemName) -> Self {
        Self { entity, name }
    }

    #[must_use]
    pub fn entity(&self) -> Entity {
        self.entity
    }

    #[must_use]
    pub fn name(&self) -> &ItemName {
        &self.name
    }
}

impl std::fmt::Display for ItemHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

/// The menu bar item this one mirrors, as `Owner,Name`.
///
/// Only the spec lives here. The captured image is a `CGImage` and so cannot
/// be a component; it sits in the [`Captures`](crate::alias::Captures) cache
/// alongside the shaped text, keyed by entity.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct AliasSpec(pub String);

/// The items this one draws behind, as one surface.
///
/// A bracket is an ordinary item with no content: it takes its frame from its
/// members rather than from text, and draws before them so its background
/// lands underneath. Membership is by name because that is what a config
/// writes and what survives a reload — the members may not exist yet when the
/// bracket is declared.
#[derive(Component, Debug, Clone, Default, PartialEq, Eq)]
pub struct Members(pub Vec<ItemName>);

/// A digest of what the alias last drew.
///
/// Its own component so that change detection sees a menu bar item whose icon
/// actually changed, and does not see one that was re-captured and came back
/// identical — which is most re-captures.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AliasContent(pub u64);

/// The claims this item holds on the sources behind its subscriptions.
///
/// Kept as a component rather than a list in the registry so that the ECS
/// releases them: despawning the item drops this, and replacing it drops the
/// claims it used to hold. A source outliving everything that wanted it is
/// invisible — it just keeps observing — so the release is better made
/// impossible to forget than remembered at every despawn.
#[derive(Component, Debug, Default)]
pub struct Watching(pub Vec<crate::sources::Watch>);

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
            drawing: true,
            color: Color::TRANSPARENT,
            corner_radius: 0.0,
            height: 0.0,
            padding_left: 0.0,
            padding_right: 0.0,
            border_color: Color::TRANSPARENT,
            border_width: 0.0,
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
        Watching::default(),
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

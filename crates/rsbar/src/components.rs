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
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::num::{NonZeroU32, NonZeroU64};

/// Which display something is restricted to: every one, or a single 1-based
/// index into [`crate::display::active`]'s order — the same order
/// [`crate::bar::Panels`] builds its panels in, so an item's `display` and a
/// panel's ordinal are always talking about the same numbering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisplayTarget {
    #[default]
    All,
    Index(NonZeroU32),
}

impl DisplayTarget {
    /// Parses the wire form: `"all"`, case-insensitively, or a positive
    /// decimal index. Anything else falls back to `All` — a config typo
    /// should not make an item vanish, only fail to restrict it.
    #[must_use]
    pub fn parse(spec: &str) -> Self {
        if spec.eq_ignore_ascii_case("all") {
            return Self::All;
        }
        if let Some(index) = spec.parse::<u32>().ok().and_then(NonZeroU32::new) {
            return Self::Index(index);
        }
        tracing::warn!(
            spec,
            "not a display: expected `all` or a 1-based index; showing on all"
        );
        Self::All
    }

    /// Whether this target includes the display at `ordinal`, 1-based.
    #[must_use]
    pub fn matches(self, ordinal: u32) -> bool {
        match self {
            Self::All => true,
            Self::Index(index) => index.get() == ordinal,
        }
    }
}

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
    /// Shifts this half alone, on top of the item's own offset.
    pub y_offset: f64,
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
            color: run.color,
            font: run.font.clone(),
            drawing: run.drawing,
            padding_left: run.padding_left,
            padding_right: run.padding_right,
            y_offset: run.y_offset,
        }
    }
}

impl From<&rsbar_protocol::Run> for Run {
    fn from(run: &rsbar_protocol::Run) -> Self {
        Self {
            string: run.text.clone(),
            color: run.color,
            font: run.font.clone(),
            drawing: run.drawing,
            padding_left: run.padding_left,
            padding_right: run.padding_right,
            y_offset: run.y_offset,
        }
    }
}

impl From<&Background> for rsbar_protocol::Background {
    fn from(background: &Background) -> Self {
        Self {
            drawing: background.drawing,
            color: background.color,
            corner_radius: background.corner_radius,
            height: background.height,
            padding_left: background.padding_left,
            padding_right: background.padding_right,
            border_color: background.border_color,
            border_width: background.border_width,
        }
    }
}

impl From<&rsbar_protocol::Background> for Background {
    fn from(background: &rsbar_protocol::Background) -> Self {
        Self {
            drawing: background.drawing,
            color: background.color,
            corner_radius: background.corner_radius,
            height: background.height,
            padding_left: background.padding_left,
            padding_right: background.padding_right,
            border_color: background.border_color,
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
            y_offset: 0.0,
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

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct Placement(pub Position);

/// Where the item sits among the others in its bucket, left to right.
///
/// Explicit rather than implied by iteration order, which in an ECS is
/// archetype order: adding a component to one item could otherwise silently
/// reshuffle the bar. `--move` and `--reorder` rewrite these.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Order(pub u32);

/// Whether the item is drawn at all. An undrawn item keeps its state and its
/// subscriptions; it simply takes no space.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Drawing(pub bool);

/// Whether the item runs its script and receives events at all.
///
/// Distinct from [`Drawing`], which only stops it being drawn: a hidden item
/// that still updates costs work nobody can see, and an item that is drawn
/// from a value someone else sets wants the opposite. Turning this off does
/// not touch [`Subscriptions`] — they are what a config would otherwise have
/// to redo, and the whole point is that turning updates back on resumes
/// without re-subscribing.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Updates(pub bool);

/// A fixed width, overriding what the item's contents come to.
///
/// `None` leaves it measured. Distinct from [`Padding`], which only insets
/// content that is still sized by it — a spacer with no icon or label wants
/// an exact width, not a wider gap around nothing.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Width(pub Option<f64>);

/// Which display this item appears on.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemDisplay(pub DisplayTarget);

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

/// Which Mission Control space a `space` item represents — `SketchyBar`'s own
/// `associated_space` on `struct bar_item` (`bar_item.h`).
///
/// `None` until something sets it. Nothing can yet: real `SketchyBar` takes
/// this as an ordinary `--set` property, same class as `icon` or `label`, and
/// [`rsbar_protocol::ItemPatch`] has no field for it — see the daemon's own
/// notes on this pass for why that is a protocol gap rather than something
/// invented here.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AssociatedSpace(pub Option<NonZeroU64>);

/// Whether a space item's own [`AssociatedSpace`] is the one currently on
/// screen — `SketchyBar`'s own `bar_item->selected`, recomputed in
/// `bar_manager_update_space_components` every time a space changes.
///
/// Recomputed in `ecs.rs`'s `update_space_selection`, off `Event::SpaceChanged`.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Selected(pub bool);

/// How many samples a graph keeps by default, absent any way to say
/// otherwise.
///
/// Real `SketchyBar` takes this as a width argument on `--add graph <name>
/// <position> <width>` (`graph_setup` in `graph.c`); `AddComponent` carries no
/// such argument, so every graph starts at this fixed size instead — see the
/// daemon's own notes on this pass.
pub const DEFAULT_GRAPH_SAMPLES: usize = 100;

/// A graph's rolling window of samples, oldest first — `struct graph` in
/// `SketchyBar`'s own `graph.c`, which is exactly this ring buffer. Pushed by
/// `--push <name> <value>` there; `rsbar_protocol::Request` has no variant
/// for that yet, so `requests::push` exists and is tested but nothing routes
/// a request to it — see the daemon's own notes on this pass.
///
/// Fixed capacity at creation and never grown, same as `graph_setup`'s own
/// `malloc`'d buffer: a graph forgets its oldest sample rather than widening.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct Graph {
    pub samples: VecDeque<f32>,
    pub capacity: usize,
    pub line_color: Color,
    pub fill_color: Color,
    pub fill: bool,
    pub line_width: f64,
}

impl Graph {
    /// Matches `graph_init`'s own defaults in `graph.c`.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(capacity),
            capacity,
            line_color: Color(0xffcc_cccc),
            fill_color: Color(0xffcc_cccc),
            fill: true,
            line_width: 0.5,
        }
    }

    /// Whether pushing `value` now would move anything visible.
    ///
    /// Checked before ever reaching for a `Mut<Graph>` — see the module doc on
    /// why that matters. A script re-reporting the same reading is the common
    /// case, not the exception, and must not reshape or repaint the item.
    #[must_use]
    pub fn would_change(&self, value: f32) -> bool {
        self.samples.back() != Some(&value)
    }

    /// Pushes one new sample onto the end, dropping the oldest once full —
    /// `graph_push_back` in `graph.c`. Callers check [`Self::would_change`]
    /// first; this always writes.
    pub fn push(&mut self, value: f32) {
        if self.capacity == 0 {
            return;
        }
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(value);
    }
}

/// A slider's own width, absent any way to say otherwise.
///
/// Real `SketchyBar` takes this as a width argument on `--add slider <name>
/// <position> <width>` (`slider_setup` in `slider.c`), exactly the same gap as
/// [`DEFAULT_GRAPH_SAMPLES`] — see the daemon's own notes on this pass.
pub const DEFAULT_SLIDER_WIDTH: f64 = 100.0;

/// A slider's own state — `struct slider` in `SketchyBar`'s own `slider.c`:
/// how far along the knob sits, and the track it slides on.
///
/// Nothing writes [`Self::percentage`] after creation yet: real `SketchyBar`
/// takes it as an ordinary `--set <name> percentage=<n>` property, and
/// [`rsbar_protocol::ItemPatch`] has no field for it, the same gap
/// [`AssociatedSpace`] has — see the daemon's own notes on this pass. Dragging
/// the knob, which is how a volume slider is actually used, additionally
/// needs pointer-drag routing that only exists for clicks today.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct Slider {
    /// Clamped to 0-100 by [`Self::clamp`], same as `slider_set_percentage`.
    pub percentage: u8,
    pub width: f64,
    pub track_color: Color,
    pub fill_color: Color,
    pub knob: Run,
}

impl Slider {
    /// Matches `slider_init`'s own defaults in `slider.c`.
    #[must_use]
    pub fn new(width: f64) -> Self {
        Self {
            percentage: 0,
            width,
            track_color: Color(0xff00_0000),
            fill_color: Color(0xff00_00ff),
            knob: Run::new("sketchybar-app-font:Regular:16.0", Color::WHITE),
        }
    }

    /// 0-100, the same clamp `slider_set_percentage` applies before ever
    /// comparing against the value already there.
    #[must_use]
    pub fn clamp(percentage: u32) -> u8 {
        u8::try_from(percentage.min(100)).unwrap_or(100)
    }
}

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
pub struct Index {
    names: HashMap<ItemName, Entity>,
    /// Handed out on every add and never reused, so a later item always sorts
    /// after an earlier one until something moves it.
    next_order: u32,
}

impl Index {
    #[must_use]
    pub fn get(&self, name: &ItemName) -> Option<Entity> {
        self.names.get(name).copied()
    }

    pub fn insert(&mut self, name: ItemName, entity: Entity) {
        self.names.insert(name, entity);
    }

    pub fn remove(&mut self, name: &ItemName) -> Option<Entity> {
        self.names.remove(name)
    }

    /// The next place in the bar, for an item being added now.
    pub fn next_order(&mut self) -> Order {
        let order = Order(self.next_order);
        self.next_order += 1;
        order
    }
}

/// Everything a new item starts as. Defaults match what a bar wants before a
/// config says otherwise: visible, no background, modest padding.
#[must_use]
pub fn bundle(name: ItemName, position: Position, order: Order) -> impl Bundle {
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
        // Nested only because a flat tuple this long stops being a `Bundle`.
        (
            Placement(position),
            order,
            Drawing(true),
            Updates(true),
            Width(None),
            ItemDisplay(DisplayTarget::All),
            Routine {
                every: 0,
                elapsed: 0,
            },
            Subscriptions::default(),
            Watching::default(),
        ),
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

    #[test]
    fn display_targets_parse_the_spellings_a_config_uses() {
        assert_eq!(DisplayTarget::parse("all"), DisplayTarget::All);
        assert_eq!(DisplayTarget::parse("ALL"), DisplayTarget::All);
        assert_eq!(
            DisplayTarget::parse("1"),
            DisplayTarget::Index(NonZeroU32::new(1).unwrap())
        );
        assert_eq!(
            DisplayTarget::parse("2"),
            DisplayTarget::Index(NonZeroU32::new(2).unwrap())
        );
    }

    #[test]
    fn an_unparsable_display_falls_back_to_all_rather_than_hiding_the_item() {
        assert_eq!(DisplayTarget::parse("0"), DisplayTarget::All);
        assert_eq!(DisplayTarget::parse("second"), DisplayTarget::All);
        assert_eq!(DisplayTarget::parse(""), DisplayTarget::All);
    }

    #[test]
    fn display_target_matching() {
        let two = DisplayTarget::Index(NonZeroU32::new(2).unwrap());
        assert!(DisplayTarget::All.matches(1));
        assert!(DisplayTarget::All.matches(2));
        assert!(!two.matches(1));
        assert!(two.matches(2));
    }

    #[test]
    fn a_graph_forgets_its_oldest_sample_once_full() {
        let mut graph = Graph::new(3);
        for sample in [1.0, 2.0, 3.0, 4.0] {
            graph.push(sample);
        }
        assert_eq!(
            graph.samples,
            std::collections::VecDeque::from([2.0, 3.0, 4.0])
        );
    }

    #[test]
    fn pushing_the_value_a_graph_already_ends_on_changes_nothing() {
        // The hard requirement this whole pass is about: a script re-reporting
        // an unchanged reading must not be visible as a change, or it repaints
        // the bar for as long as it keeps running.
        let mut graph = Graph::new(3);
        graph.push(42.0);
        assert!(!graph.would_change(42.0));
        assert!(graph.would_change(43.0));
    }

    #[test]
    fn a_graph_with_no_capacity_never_keeps_a_sample() {
        let mut graph = Graph::new(0);
        graph.push(1.0);
        assert!(graph.samples.is_empty());
    }

    #[test]
    fn a_slider_percentage_clamps_into_range() {
        assert_eq!(Slider::clamp(0), 0);
        assert_eq!(Slider::clamp(100), 100);
        assert_eq!(Slider::clamp(150), 100);
    }

    #[test]
    fn a_fresh_slider_starts_at_zero() {
        let slider = Slider::new(DEFAULT_SLIDER_WIDTH);
        assert_eq!(slider.percentage, 0);
        assert!((slider.width - DEFAULT_SLIDER_WIDTH).abs() < f64::EPSILON);
    }

    #[test]
    fn a_fresh_space_item_is_unassigned_and_unselected() {
        assert_eq!(AssociatedSpace::default(), AssociatedSpace(None));
        assert_eq!(Selected::default(), Selected(false));
    }
}

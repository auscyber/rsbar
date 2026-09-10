//! Applying requests to the item world.

#![allow(
    clippy::needless_pass_by_value,
    reason = "Bevy system parameters are taken by value by contract"
)]

use crate::bar::{Changes, Panels, Settings};
use crate::components::{
    AliasContent, AliasSpec, AssociatedSpace, Background, ClickScript, DEFAULT_GRAPH_SAMPLES,
    DEFAULT_SLIDER_WIDTH, Drawing, Graph, Icon, Index, ItemDisplay, ItemHandle, Label, Members,
    Name, Offset, Order, Padding, Placement, Routine, Run, Script, Selected, Slider, Stale,
    Subscriptions, Updates, Watching, Width, bundle,
};
use crate::popup::{PopupConfig, PopupOf};
use crate::script::Job;
use crate::shaping::Cache;
use crate::sources::Registry;
use crate::subscribers::Subscribers;
use bevy_ecs::prelude::*;
use bevy_ecs::query::QueryData;
use bevy_ecs::system::SystemParam;
// `Changes` is `coolabah_protocol`'s trait; the bitflags of the same name from
// `crate::bar` is what this module's own `set_bar` reads, so the trait comes
// in anonymously and neither shadows the other.
use crate::protocol::Changes as _;
use crate::protocol::{
    BackgroundPatch, ComponentKind, Event, ItemName, ItemPatch, ItemState, Kind, Position,
    PressTarget, Query as ProtocolQuery, Relative, Request, Response, RunPatch, Selector,
};
use std::collections::BTreeSet;
use std::sync::Arc;

/// Every component a request can write.
///
/// Named fields rather than a tuple. This has grown to a dozen columns, and a
/// positional row is a bug waiting to happen at that size: inserting one
/// silently rebinds everything after it, which is how the click script once
/// ended up where the update script belonged and stopped every
/// subscription-driven script with no error and no log line.
#[derive(QueryData)]
#[query_data(mutable)]
pub struct ItemWrite {
    pub entity: Entity,
    pub name: &'static Name,
    pub icon: &'static mut Icon,
    pub label: &'static mut Label,
    pub background: &'static mut Background,
    pub padding: &'static mut Padding,
    pub offset: &'static mut Offset,
    pub placement: &'static mut Placement,
    pub order: &'static mut Order,
    pub drawing: &'static mut Drawing,
    pub updates: &'static mut Updates,
    pub width: &'static mut Width,
    pub display: &'static mut ItemDisplay,
    pub routine: &'static mut Routine,
    pub subscriptions: &'static mut Subscriptions,
    pub script: Option<&'static Script>,
    pub click: Option<&'static ClickScript>,
    pub alias: Option<&'static AliasSpec>,
    pub members: Option<&'static Members>,
    pub popup: Option<&'static PopupConfig>,
    pub slider: Option<&'static mut Slider>,
    pub associated_space: Option<&'static mut AssociatedSpace>,
}

/// Every component a query or a dispatch reads.
#[derive(QueryData)]
pub struct ItemRead {
    pub entity: Entity,
    pub name: &'static Name,
    pub icon: &'static Icon,
    pub label: &'static Label,
    pub background: &'static Background,
    pub padding: &'static Padding,
    pub offset: &'static Offset,
    pub placement: &'static Placement,
    pub drawing: &'static Drawing,
    /// Gates whether this item's script runs for a subscribed event at all —
    /// not whether it *would* match — so an item that turned updates off
    /// keeps its subscriptions inert rather than dropping them.
    pub updates: &'static Updates,
    pub width: &'static Width,
    pub routine: &'static Routine,
    pub subscriptions: &'static Subscriptions,
    pub script: Option<&'static Script>,
    pub click: Option<&'static ClickScript>,
    pub alias: Option<&'static AliasSpec>,
    pub members: Option<&'static Members>,
    pub popup: Option<&'static PopupConfig>,
}

/// Everything a request may write to in the item world.
///
/// One query holding all of it, rather than several: Bevy derives a system's
/// access statically, and two queries that could both match an item would be a
/// conflict at runtime.
#[derive(SystemParam)]
pub struct Items<'w, 's> {
    pub commands: Commands<'w, 's>,
    pub index: ResMut<'w, Index>,
    pub write: Query<'w, 's, ItemWrite>,
    /// Items nothing has claimed since the config began. Read-only and
    /// disjoint from what `write` mutates, so the two can share a system.
    pub unclaimed: Query<'w, 's, (Entity, &'static Name), With<Stale>>,
    /// A graph's own samples, queried separately from [`Self::write`]: a
    /// different component type never conflicts with it no matter which
    /// entities overlap, so this costs nothing on every item that is not a
    /// graph.
    pub graphs: Query<'w, 's, &'static mut Graph>,
}

/// Reads only, for the systems that do not also write.
///
/// Deliberately *not* used alongside [`Items`] in one system: Bevy derives
/// access statically, so a read query and a write query over the same
/// components is a B0001 conflict at startup, not a borrow error at compile
/// time. Systems that write use [`Items`], which can read through its own
/// query.
#[derive(SystemParam)]
pub struct ItemsRead<'w, 's> {
    pub read: Query<'w, 's, ItemRead>,
    /// The three components [`ItemRead`] does not carry, queried separately
    /// so both projections of an item report the same thing. Read-only and
    /// over the same entities, which Bevy allows: two reads never conflict.
    pub extra: Query<
        'w,
        's,
        (
            &'static ItemDisplay,
            Option<&'static Slider>,
            Option<&'static AssociatedSpace>,
        ),
    >,
}

/// What [`ItemsRead::extra`] yields for one item.
type Extra<'a> = (
    Option<&'a ItemDisplay>,
    Option<&'a Slider>,
    Option<&'a AssociatedSpace>,
);

impl Items<'_, '_> {
    /// The scripts to run for `event`, for the items that depend on it.
    ///
    /// `dependents` comes from the claims, so this never scans: an event goes
    /// to what asked for it, and an event nothing asked for costs nothing.
    #[must_use]
    pub fn jobs_for(&self, event: &Arc<Event>, dependents: &[Entity]) -> Vec<Job> {
        dependents
            .iter()
            .filter_map(|entity| {
                let row = self.write.get(*entity).ok()?;
                if !row.updates.0 {
                    return None;
                }
                Some(Job {
                    item: ItemHandle::new(row.entity, row.name.0.clone()),
                    script: Arc::clone(&row.script?.0),
                    event: Arc::clone(event),
                })
            })
            .collect()
    }

    /// Every item's script, regardless of frequency, subscription, or
    /// `updates` — this is the forced update `UpdateAll` asks for, and a
    /// forced update runs even an item that has turned its own updates off,
    /// same as re-running its update frequency early would.
    #[must_use]
    pub fn all_jobs(&self) -> Vec<Job> {
        self.write
            .iter()
            .filter_map(|row| {
                Some(Job {
                    item: ItemHandle::new(row.entity, row.name.0.clone()),
                    script: Arc::clone(&row.script?.0),
                    event: Arc::new(Event::Forced(crate::protocol::event::Forced {})),
                })
            })
            .collect()
    }

    #[must_use]
    /// Every item, in the order the bar draws them -- not the query's own,
    /// which is archetype order and means nothing to a caller.
    pub fn states(&self) -> Vec<ItemState> {
        let mut rows: Vec<_> = self.write.iter().collect();
        rows.sort_unstable_by_key(|row| *row.order);
        rows.iter().map(write_state).collect()
    }

    #[must_use]
    pub fn state(&self, name: &ItemName) -> Option<ItemState> {
        self.write
            .iter()
            .find(|row| &row.name.0 == name)
            .map(|row| write_state(&row))
    }
}

/// An item's popup as a caller sees it, defaulted when it has none -- a
/// config reads `query().popup.drawing` without checking, so a missing key
/// there is a crash rather than a false.
fn popup_state(config: Option<&PopupConfig>) -> crate::protocol::PopupState {
    let config = config.copied().unwrap_or_default();
    crate::protocol::PopupState {
        drawing: config.drawing.into(),
        horizontal: config.horizontal.into(),
        align: config.align,
        topmost: config.topmost.into(),
        height: config.height,
        y_offset: config.y_offset,
        background: (&config.background).into(),
    }
}

/// The same projection as [`state_of`], off the read-only view of the write
/// query — `iter()` on a mutable query yields plain references, not `Mut`.
fn write_state(row: &ItemWriteReadOnlyItem<'_, '_>) -> ItemState {
    ItemState {
        name: row.name.0.clone(),
        popup: popup_state(row.popup),
        geometry: crate::protocol::Geometry {
            drawing: row.drawing.0.into(),
            position: row.placement.0.clone(),
            y_offset: row.offset.0,
            padding_left: row.padding.left,
            padding_right: row.padding.right,
            width: row.width.0,
            display: row.display.0,
            background: row.background.into(),
        },
        icon: (&row.icon.0).into(),
        label: (&row.label.0).into(),
        scripting: crate::protocol::Scripting {
            script: row.script.map(|s| s.0.to_string()),
            click_script: row.click.map(|s| s.0.to_string()),
            update_freq: row.routine.every,
            updates: row.updates.0.into(),
        },
        events: row.subscriptions.0.iter().cloned().collect(),
        alias: row.alias.map(|alias| alias.0.clone()),
        members: row.members.map(|m| m.0.clone()).unwrap_or_default(),
        associated_space: row.associated_space.and_then(|space| space.0),
        percentage: row.slider.map_or(0, |slider| slider.percentage),
        knob: row
            .slider
            .map(|slider| (&slider.knob).into())
            .unwrap_or_default(),
        highlight_color: row.icon.0.highlight_color,
    }
}

impl ItemsRead<'_, '_> {
    /// The scripts one item runs for `event`.
    ///
    /// A click belongs to whatever is under the cursor, not to everything that
    /// subscribed — so it is dispatched by entity rather than by matching every
    /// subscriber. Both the click script and a `mouse.clicked` subscription
    /// fire, because they are different questions: one is "do this when
    /// clicked", the other is "tell me when anything happens".
    #[must_use]
    pub fn jobs_for_item(&self, entity: Entity, event: &Arc<Event>) -> Vec<Job> {
        let Ok(row) = self.read.get(entity) else {
            return Vec::new();
        };
        let (name, subscriptions, script, click) =
            (row.name, row.subscriptions, row.script, row.click);

        let mut jobs = Vec::new();
        if let Some(click) = click {
            jobs.push(Job {
                item: ItemHandle::new(entity, name.0.clone()),
                script: Arc::clone(&click.0),
                event: Arc::clone(event),
            });
        }
        // The click script above is unconditional — a click is a direct user
        // action, not something `updates` is about — but a subscription
        // match is exactly the "woken by a subscribed event" `updates` turns
        // off.
        if row.updates.0
            && subscriptions.0.iter().any(|kind| kind.matches(event))
            && let Some(script) = script
        {
            jobs.push(Job {
                item: ItemHandle::new(entity, name.0.clone()),
                script: Arc::clone(&script.0),
                event: Arc::clone(event),
            });
        }
        jobs
    }

    /// The scripts to run for `event`, for the items that depend on it.
    ///
    /// `dependents` comes from the claims the items hold, so this never scans
    /// for subscribers: an event goes to whatever asked for it, and one nobody
    /// asked for costs nothing. Items without a script are skipped —
    /// subscribing a scriptless item is legal and simply does nothing.
    #[must_use]
    pub fn jobs_for(&self, event: &Arc<Event>, dependents: &[Entity]) -> Vec<Job> {
        let mut into = Vec::new();
        self.push_jobs(event, dependents, &mut into);
        into
    }

    /// The same, appended to a queue the caller already has.
    ///
    /// Straight into the destination: an event with three dependents should
    /// not build a three-element `Vec` for something else to copy out of.
    pub fn push_jobs(&self, event: &Arc<Event>, dependents: &[Entity], into: &mut Vec<Job>) {
        into.reserve(dependents.len());
        for entity in dependents {
            let Ok(row) = self.read.get(*entity) else {
                continue;
            };
            if !row.updates.0 {
                continue;
            }
            // Indexed, not destructured with `..`. A `(_, name, .., script)`
            // pattern silently followed the row when a column was added,
            // binding the click script instead — which broke every
            // subscription-driven script and raised no error.
            let Some(script) = row.script else {
                continue;
            };
            into.push(Job {
                item: ItemHandle::new(*entity, row.name.0.clone()),
                script: Arc::clone(&script.0),
                event: Arc::clone(event),
            });
        }
    }

    /// Every item's script, regardless of frequency, subscription, or
    /// `updates` — see [`Items::all_jobs`].
    #[must_use]
    pub fn all_jobs(&self) -> Vec<Job> {
        self.read
            .iter()
            .filter_map(|row| {
                Some(Job {
                    item: ItemHandle::new(row.entity, row.name.0.clone()),
                    script: Arc::clone(&row.script?.0),
                    event: Arc::new(Event::Forced(crate::protocol::event::Forced {})),
                })
            })
            .collect()
    }

    #[must_use]
    pub fn states(&self) -> Vec<ItemState> {
        self.read
            .iter()
            .map(|row| {
                let extra = self.extra.get(row.entity).ok();
                state_of(&row, extra.map(|(d, s, a)| (Some(d), s, a)))
            })
            .collect()
    }

    #[must_use]
    pub fn state(&self, name: &ItemName) -> Option<ItemState> {
        self.read.iter().find(|row| &row.name.0 == name).map(|row| {
            let extra = self.extra.get(row.entity).ok();
            state_of(&row, extra.map(|(d, s, a)| (Some(d), s, a)))
        })
    }
}

/// The same projection off the read-only query, with the components
/// [`ItemRead`] does not carry read from [`ItemsRead::extra`].
fn state_of(row: &ItemReadItem<'_, '_>, extra: Option<Extra<'_>>) -> ItemState {
    let (display, slider, space) = extra.unwrap_or_default();
    ItemState {
        name: row.name.0.clone(),
        popup: popup_state(row.popup),
        geometry: crate::protocol::Geometry {
            drawing: row.drawing.0.into(),
            position: row.placement.0.clone(),
            y_offset: row.offset.0,
            padding_left: row.padding.left,
            padding_right: row.padding.right,
            width: row.width.0,
            display: display.map(|d| d.0).unwrap_or_default(),
            background: row.background.into(),
        },
        icon: (&row.icon.0).into(),
        label: (&row.label.0).into(),
        scripting: crate::protocol::Scripting {
            script: row.script.map(|s| s.0.to_string()),
            click_script: row.click.map(|s| s.0.to_string()),
            update_freq: row.routine.every,
            updates: row.updates.0.into(),
        },
        events: row.subscriptions.0.iter().cloned().collect(),
        alias: row.alias.map(|alias| alias.0.clone()),
        members: row.members.map(|m| m.0.clone()).unwrap_or_default(),
        associated_space: space.and_then(|space| space.0),
        percentage: slider.map_or(0, |slider| slider.percentage),
        knob: slider
            .map(|slider| (&slider.knob).into())
            .unwrap_or_default(),
        highlight_color: row.icon.0.highlight_color,
    }
}

/// What applying a request produced, beyond its answer.
pub struct Outcome {
    pub response: Response,
    pub jobs: Vec<Job>,
    pub exit: bool,
    /// The caller should tear the item world down and re-run the config.
    pub reload: bool,
}

impl Outcome {
    fn ok() -> Self {
        Self {
            response: Response::Ok,
            jobs: Vec::new(),
            exit: false,
            reload: false,
        }
    }

    fn error(message: String) -> Self {
        Self {
            response: Response::Error(message),
            jobs: Vec::new(),
            exit: false,
            reload: false,
        }
    }

    fn answer(response: Response) -> Self {
        Self {
            response,
            jobs: Vec::new(),
            exit: false,
            reload: false,
        }
    }
}

/// Whether a subscription is about the pointer.
fn is_pointer(kind: &Kind) -> bool {
    matches!(
        kind,
        Kind::MouseClicked(())
            | Kind::MouseScrolled(())
            | Kind::MouseScrolledGlobal
            | Kind::MouseEntered(())
            | Kind::MouseExited(())
            | Kind::MouseEnteredGlobal
            | Kind::MouseExitedGlobal
    )
}

/// Everything an item keeps running: what it subscribed to, plus the pointer
/// if it has a click script.
///
/// A click script is not a subscription but needs the same source, so it is
/// folded in here rather than being a second, competing claim — the registry
/// takes one set per item and replacing it releases the rest.
fn needs(subscribed: &BTreeSet<Kind>, clickable: bool) -> impl Iterator<Item = Kind> + use<'_> {
    subscribed
        .iter()
        .cloned()
        .chain(clickable.then_some(Kind::MouseClicked(())))
}

/// Lets the bar take clicks at all.
///
/// The source reports them and this tag is what gets them delivered; without
/// either there is nothing to report.
fn take_clicks(connected: &skylight::Connected, panels: &Panels) {
    if let Err(err) = panels.set_clickable(connected, true) {
        tracing::error!(%err, "could not make the bar take clicks");
    }
}

fn no_such(name: &ItemName) -> Outcome {
    Outcome::error(format!("no item named `{name}`"))
}

/// Everything outside the item world that a request can reach.
pub struct Context<'a> {
    /// Proof of the thread the window server talks to.
    ///
    /// Carried rather than asked for: applying a request reshapes windows, and
    /// the caller — a system on the run loop's own thread — already has it.
    pub main: objc2::MainThreadMarker,
    /// The window server connection, exclusive for this pass — see
    /// [`crate::ecs::ConnectedRes`].
    pub connected: &'a skylight::Connected,
    pub settings: &'a mut Settings,
    pub panels: &'a mut Panels,
    pub cache: &'a mut Cache,
    /// The mirrored images, so an item that goes takes its capture with it.
    pub captures: &'a mut crate::alias::Captures,
    pub sources: &'a mut Registry,
    pub subscribers: &'a mut Subscribers,
    /// A port this request arrived with, if the client wants its events
    /// pushed back rather than run as a script.
    pub subscriber: Option<crate::protocol::wire::Subscriber>,
    /// What `--default` last set, applied to every item added from here on.
    pub defaults: &'a mut Defaults,
}

/// The properties every item added from here on starts with.
///
/// Only what a `--default` actually named, never a fully-populated patch: an
/// add that wrote every property would defeat the "changed nothing" check that
/// keeps a repaint from happening for no reason.
#[derive(Resource, Default)]
pub struct Defaults(pub ItemPatch);

/// A just-added item that has not had `--default`'s properties yet.
///
/// A marker and a system rather than a command, because the entity does not
/// exist until the spawn is flushed and its components cannot be patched
/// before they are there. Applying them through the ordinary patch path is
/// what makes a default and an explicit `--set` of the same property behave
/// identically -- including doing nothing at all when the value already
/// matches, which is what keeps a fresh item off the repaint list.
#[derive(Component)]
pub struct NeedsDefaults;

/// Applies `--default`'s properties to every item added since the last run.
pub fn apply_defaults(
    mut items: Items,
    defaults: Res<Defaults>,
    fresh: Query<Entity, With<NeedsDefaults>>,
) {
    let known: Vec<ItemName> = items.index.names().cloned().collect();
    for entity in &fresh {
        if let Ok(mut row) = items.write.get_mut(entity) {
            set_item(entity, &mut row, &defaults.0, &known, &mut items.commands);
        }
        items.commands.entity(entity).remove::<NeedsDefaults>();
    }
}

/// Merges a `popup.*` patch into the host item's own popup configuration.
///
/// Written only when the merge makes a difference, like every other property
/// here: a script toggling `popup.drawing` to what it already is must not
/// repaint the popup, and `Mut::as_mut` would mark it changed regardless.
fn apply_popup(
    entity: Entity,
    current: Option<&PopupConfig>,
    patch: &crate::protocol::PopupPatch,
    commands: &mut Commands,
) {
    let existing = current.copied().unwrap_or_default();
    let patch = crate::popup::PopupPatch {
        // Every popup boolean resolves `toggle` against what the popup
        // already is.
        drawing: patch
            .drawing
            .map(|d| d.resolve(existing.drawing.into()).get()),
        horizontal: patch
            .horizontal
            .map(|h| h.resolve(existing.horizontal.into()).get()),
        align: patch.align,
        topmost: patch
            .topmost
            .map(|t| t.resolve(existing.topmost.into()).get()),
        height: patch.height,
        y_offset: patch.y_offset,
        background: patch.background,
    };
    if let Some(next) = crate::popup::patched(&existing, &patch) {
        commands.entity(entity).insert(next);
    } else if current.is_none() {
        // First mention of a popup on this item, even one that changed
        // nothing against the defaults, is what brings it into being.
        commands.entity(entity).insert(existing);
    }
}

/// Attaches an item to the popup it named, or detaches it from one.
///
/// A popup is a relationship rather than a place along the bar, so it is a
/// component of its own: `place()` skips anything carrying one, and a popup's
/// contents are laid out and repainted entirely separately. The host is
/// resolved by name here because only the daemon has a live item list -- and
/// a config may well name a host that does not exist yet, which is why an
/// unresolved name leaves the item off the bar rather than dropping it into
/// the left bucket by surprise.
fn host_of(position: &Position, index: &Index, entity: Entity, commands: &mut Commands) {
    let Position::Popup(host) = position else {
        commands.entity(entity).remove::<PopupOf>();
        return;
    };
    if let Some(found) = index.get(host) {
        commands.entity(entity).insert(PopupOf(found));
    } else {
        tracing::warn!(%host, "no such item to hang a popup off");
    }
}

/// Every item, in the order the bar draws them.
fn current_order(items: &Items<'_, '_>) -> Vec<(Entity, ItemName)> {
    let mut order: Vec<_> = items
        .write
        .iter()
        .map(|row| (*row.order, row.entity, row.name.0.clone()))
        .collect();
    order.sort_unstable_by_key(|(order, ..)| *order);
    order
        .into_iter()
        .map(|(_, entity, name)| (entity, name))
        .collect()
}

/// Writes a new left-to-right order, touching only what actually moved.
///
/// Renumbering from zero every time keeps the keys dense and the comparison
/// exact; writing only where the value differs is what keeps a move from
/// marking every item changed and repainting the whole bar.
fn renumber(order: &[(Entity, ItemName)], items: &mut Items<'_, '_>) {
    for (place, (entity, _)) in order.iter().enumerate() {
        let Ok(mut row) = items.write.get_mut(*entity) else {
            continue;
        };
        let next = Order(u32::try_from(place).unwrap_or(u32::MAX));
        if *row.order != next {
            *row.order = next;
        }
    }
}

/// Every item whose name matches `pattern`.
///
/// Resolved here rather than by the caller because only the daemon has a live
/// item list — a client resolving it would race a config still adding items,
/// which is exactly what a config does when it sets `/space\..*/` while the
/// spaces are still arriving.
fn matching(pattern: &str, items: &Items<'_, '_>) -> Result<Vec<Entity>, regex::Error> {
    let regex = regex::Regex::new(pattern)?;
    Ok(items
        .write
        .iter()
        .filter(|row| regex.is_match(row.name.0.as_str()))
        .map(|row| row.entity)
        .collect())
}

/// Opens the menu behind an `Owner,Name` alias spec.
///
/// Against the cached scan: a click must not wait on a walk of every running
/// application, and does not need to — the item it is opening was resolved
/// well before it could be clicked.
fn press_alias(spec: &str) -> Result<(), crate::alias::Error> {
    let (owner, name) = spec.split_once(',').unwrap_or((spec, spec));
    crate::alias::press_item(&crate::alias::Snapshot::cached()?, owner, name)
}

/// Applies one request.
#[allow(
    clippy::too_many_lines,
    reason = "one arm per request; splitting hides the shape"
)]
pub fn apply(request: Request, items: &mut Items, ctx: &mut Context<'_>) -> Outcome {
    let Context {
        main,
        connected,
        settings,
        panels,
        cache,
        captures,
        sources,
        subscribers,
        subscriber,
        defaults,
    } = ctx;
    match request {
        Request::SetBar(patch) => {
            tracing::debug!(?patch, "set bar");
            let changes = settings.apply(&patch);
            let result = (|| -> skylight::Result<()> {
                if changes.contains(Changes::DISPLAYS) {
                    // A superset of `reframe`: it also creates and destroys
                    // panels, so a plain reframe afterwards would be redundant.
                    panels.rebuild(*main, connected, settings)?;
                } else if changes.contains(Changes::GEOMETRY) {
                    panels.reframe(connected, settings)?;
                }
                if changes.contains(Changes::BLUR) {
                    panels.set_blur(connected, settings.blur_radius)?;
                }
                if changes.contains(Changes::VISIBILITY) {
                    panels.set_hidden(connected, settings.hidden)?;
                }
                if changes.contains(Changes::LEVEL) {
                    panels.set_level(connected, settings)?;
                }
                // Retagging a window that already exists. Without these, only
                // a panel built fresh -- a new display, or a restart -- ever
                // picked the setting up.
                if changes.contains(Changes::STICKY) {
                    panels.set_sticky(connected, settings.sticky)?;
                }
                if changes.contains(Changes::FULLSCREEN) {
                    panels.set_show_in_fullscreen(connected, settings.show_in_fullscreen)?;
                }
                Ok(())
            })();
            match result {
                Ok(()) => Outcome::ok(),
                Err(err) => Outcome::error(err.to_string()),
            }
        }

        Request::Add(ComponentKind::Item { name, position }) => {
            tracing::debug!(%name, ?position, "add item");
            if let Some(entity) = items.index.get(&name) {
                // Re-adding an item that exists moves it rather than replacing
                // it. That is what makes a reload cheap: entity identity
                // survives, so change detection sees only what actually
                // changed rather than every item disappearing and coming back.
                if let Ok(mut row) = items.write.get_mut(entity)
                    && row.placement.0 != position
                {
                    row.placement.0 = position.clone();
                }
                host_of(&position, &items.index, entity, &mut items.commands);
                items.commands.entity(entity).remove::<Stale>();
                return Outcome::ok();
            }
            let order = items.index.next_order();
            let entity = items
                .commands
                .spawn(bundle(name.clone(), position.clone(), order))
                .id();
            items.index.insert(name, entity);
            host_of(&position, &items.index, entity, &mut items.commands);
            // See `NeedsDefaults`: applied through the ordinary patch path,
            // not baked into the bundle.
            if defaults.0 != ItemPatch::default() {
                items.commands.entity(entity).insert(NeedsDefaults);
            }
            Outcome::ok()
        }

        Request::AddEvent { name, notification } => {
            sources.declare_event(name, notification);
            Outcome::ok()
        }

        Request::Add(kind) => {
            let (name, position) = kind.placement();
            let (name, position) = (name.clone(), position);
            tracing::debug!(%name, ?position, ?kind, "add");
            if let Some(entity) = items.index.get(&name) {
                // Same re-add-moves-it semantics as `AddItem`: entity identity
                // survives, so a reload only touches what actually changed.
                if let Ok(mut row) = items.write.get_mut(entity) {
                    row.placement.0 = position;
                }
                items.commands.entity(entity).remove::<Stale>();
                return Outcome::ok();
            }
            let order = items.index.next_order();
            let mut spawned = items
                .commands
                .spawn(bundle(name.clone(), position.clone(), order));
            // A space auto-subscribes to space changes the way `SketchyBar`'s
            // own `bar_item_set_type` sets `UPDATE_SPACE_CHANGE` — a config
            // never has to ask for it. An alias does the same with
            // application launches, and for the same kind of reason: an alias
            // naming an application that is not running cannot resolve, and a
            // launch is the moment it can. It is the *only* thing that brings
            // such an alias back, since nothing else observes an owner that
            // was never found. Graph and slider have nothing to subscribe to:
            // they are driven by `--push`/a percentage set, not an event.
            let watch: Vec<Kind> = match kind {
                ComponentKind::Item { .. } => Vec::new(),
                ComponentKind::Alias { name, .. } => {
                    // The name *is* the spec for `--add alias Owner,Name`, the
                    // way a space's name is the space it follows. Without this
                    // insert the item exists but draws nothing forever:
                    // `ecs::refresh_aliases` queries for `AliasSpec` and
                    // silently skips anything without it.
                    spawned.insert((
                        AliasSpec(name.to_string()),
                        crate::components::AliasContent::default(),
                    ));
                    vec![Kind::AppLaunched]
                }
                ComponentKind::Bracket { members, .. } => {
                    spawned.insert(Members(
                        members
                            .iter()
                            .filter_map(|m| match m {
                                Selector::Name(name) => Some(name.clone()),
                                Selector::Pattern(_) => None,
                            })
                            .collect(),
                    ));
                    Vec::new()
                }
                ComponentKind::Space { .. } => {
                    spawned.insert((AssociatedSpace::default(), Selected::default()));
                    vec![Kind::SpaceChanged]
                }
                ComponentKind::Graph { .. } => {
                    spawned.insert(Graph::new(DEFAULT_GRAPH_SAMPLES));
                    Vec::new()
                }
                ComponentKind::Slider { .. } => {
                    spawned.insert(Slider::new(DEFAULT_SLIDER_WIDTH));
                    Vec::new()
                }
            };
            let entity = spawned.id();
            items.index.insert(name, entity);
            // Same attachment `Request::Add(Item)` does: an alias, a bracket
            // or a space placed in a popup needs [`PopupOf`] too, or nothing
            // downstream -- including the popup's own drawing state -- can
            // tell it is inside one.
            host_of(&position, &items.index, entity, &mut items.commands);
            if defaults.0 != ItemPatch::default() {
                items.commands.entity(entity).insert(NeedsDefaults);
            }
            if !watch.is_empty() {
                let watches = sources.watch_all(entity, watch.iter().cloned());
                items.commands.entity(entity).insert((
                    Watching(watches),
                    // So `--query` reports it: a space item watches this on
                    // its own, and a client asking what it is subscribed to
                    // should see that, the same as it would for an explicit
                    // `--subscribe`.
                    Subscriptions(watch.into_iter().collect()),
                ));
            }
            Outcome::ok()
        }

        Request::Set(Selector::Name(name), patch) => {
            let known: Vec<ItemName> = items.index.names().cloned().collect();
            tracing::trace!(%name, ?patch, "set item");
            let Some(entity) = items.index.get(&name) else {
                return no_such(&name);
            };
            let Ok(mut row) = items.write.get_mut(entity) else {
                return no_such(&name);
            };
            let wants_clicks = patch
                .scripting
                .as_ref()
                .and_then(|scripting| scripting.click_script.as_ref())
                .is_some_and(|script| !script.is_empty());
            let subscribed = wants_clicks.then(|| row.subscriptions.0.clone());
            set_item(entity, &mut row, &patch, &known, &mut items.commands);
            // Touching an item during a reload is what keeps it: a config that
            // only sets an existing item, without re-adding it, must not have
            // it swept up as stale.
            items.commands.entity(entity).remove::<Stale>();
            if let Some(subscribed) = subscribed {
                let watches = sources.watch_all(entity, needs(&subscribed, true));
                items.commands.entity(entity).insert(Watching(watches));
                take_clicks(connected, panels);
            }
            Outcome::ok()
        }

        Request::Remove(Selector::Name(name)) => {
            tracing::debug!(%name, "remove item");
            let Some(entity) = items.index.remove(&name) else {
                return no_such(&name);
            };
            cache.forget(entity);
            captures.forget(entity);
            subscribers.clear(entity);
            // The item's claims go with it: `Watching` is a component, so the
            // despawn drops them and the sources nothing wants any more stop.
            items.commands.entity(entity).despawn();
            Outcome::ok()
        }

        Request::Set(Selector::Pattern(pattern), patch) => {
            let matched = match matching(&pattern, items) {
                Ok(matched) => matched,
                Err(err) => return Outcome::error(err.to_string()),
            };
            tracing::debug!(pattern, matched = matched.len(), "set matching");
            let known: Vec<ItemName> = items.index.names().cloned().collect();
            for entity in matched {
                let Ok(mut row) = items.write.get_mut(entity) else {
                    continue;
                };
                set_item(entity, &mut row, &patch, &known, &mut items.commands);
                items.commands.entity(entity).remove::<Stale>();
            }
            Outcome::ok()
        }

        Request::Remove(Selector::Pattern(pattern)) => {
            let matched = match matching(&pattern, items) {
                Ok(matched) => matched,
                Err(err) => return Outcome::error(err.to_string()),
            };
            tracing::debug!(pattern, matched = matched.len(), "remove matching");
            for entity in matched {
                let Ok(row) = items.write.get(entity) else {
                    continue;
                };
                let name = row.name.0.clone();
                items.index.remove(&name);
                cache.forget(entity);
                captures.forget(entity);
                subscribers.clear(entity);
                items.commands.entity(entity).despawn();
            }
            Outcome::ok()
        }

        Request::SetDefault(patch) => {
            tracing::debug!(?patch, "set defaults");
            **defaults = Defaults(*patch);
            Outcome::ok()
        }

        Request::Move {
            name,
            relative,
            reference,
        } => {
            let mut order = current_order(items);
            let Some(from) = order.iter().position(|(_, n)| *n == name) else {
                return no_such(&name);
            };
            let moved = order.remove(from);
            let Some(to) = order.iter().position(|(_, n)| *n == reference) else {
                return no_such(&reference);
            };
            let to = match relative {
                Relative::Before => to,
                Relative::After => to + 1,
            };
            order.insert(to, moved);
            renumber(&order, items);
            Outcome::ok()
        }

        Request::Reorder(names) => {
            // Anything the caller did not name keeps its relative place after
            // the ones it did, rather than being dropped to an arbitrary spot.
            let current = current_order(items);
            let mut order: Vec<(Entity, ItemName)> = Vec::with_capacity(current.len());
            for name in &names {
                if let Some(found) = current.iter().find(|(_, n)| n == name) {
                    order.push(found.clone());
                } else {
                    return no_such(name);
                }
            }
            order.extend(
                current
                    .into_iter()
                    .filter(|(_, n)| !names.contains(n))
                    .collect::<Vec<_>>(),
            );
            renumber(&order, items);
            Outcome::ok()
        }

        Request::Subscribe { name, events } => {
            tracing::debug!(%name, ?events, "subscribe");
            let Some(entity) = items.index.get(&name) else {
                return no_such(&name);
            };
            let Ok(mut row) = items.write.get_mut(entity) else {
                return no_such(&name);
            };
            // Subscribing is where a config decides what this process actually
            // observes, so it is what starts and stops a source.
            let subscribed: BTreeSet<Kind> = events.into_iter().collect();
            let clickable = row.click.is_some_and(|script| !script.0.is_empty())
                || subscribed.iter().any(is_pointer);
            // Inserting replaces whatever it held before, and dropping those
            // releases exactly what this item stopped wanting.
            // A port arriving with the subscription is the client saying it
            // will handle these itself. Arriving without one is it saying the
            // opposite, so the previous port goes — a subscription replaces.
            match subscriber.take() {
                Some(port) => subscribers.set(entity, port),
                None => subscribers.clear(entity),
            }
            let watches = sources.watch_all(entity, needs(&subscribed, clickable));
            items.commands.entity(entity).insert(Watching(watches));
            if clickable {
                take_clicks(connected, panels);
            }
            row.subscriptions.0 = subscribed;
            Outcome::ok()
        }

        Request::Trigger(event) => {
            tracing::debug!(kind = %event.kind(), "trigger");
            // A triggered event needs no source and no claim: this request
            // arriving *is* the event arriving, so it is emitted rather than
            // dispatched directly here -- the emitter sets the ready bit and
            // wakes the run loop, and the ordinary drain delivers it the same
            // way a source's event would. One delivery path, not a
            // hand-copied second one.
            //
            // It is also the whole difference between the two kinds of custom
            // event: a bridged one needs an observer on a notification centre,
            // a triggered one needs nothing at all.
            sources.emitter().send(event);
            Outcome::ok()
        }

        Request::UpdateAll => Outcome {
            jobs: items.all_jobs(),
            ..Outcome::ok()
        },

        Request::Query(ProtocolQuery::Bar) => {
            Outcome::answer(Response::Bar(Box::new(settings.state(panels.len()))))
        }
        Request::Query(ProtocolQuery::Items) => Outcome::answer(Response::Items(items.states())),
        Request::Query(ProtocolQuery::Item(name)) => match items.state(&name) {
            Some(state) => Outcome::answer(Response::Item(Box::new(state))),
            None => no_such(&name),
        },
        // `cached_or_scan`, not `cached`: this query's entire output is
        // owner names, and a config reads it to decide which aliases to
        // create. Answering before any scan has landed hands back
        // `Control Centre,Fantastical` for an item really owned by
        // `Fantastical Helper`, and the config writes that down for good.
        Request::Query(ProtocolQuery::MenuItems) => {
            match crate::alias::Snapshot::cached_or_scan(*main)
                .and_then(|snapshot| crate::alias::list_menu_bar_items(&snapshot))
            {
                Ok(found) => Outcome::answer(Response::MenuItems(
                    found
                        .into_iter()
                        .map(|item| format!("{},{}", item.owner, item.name))
                        .collect(),
                )),
                Err(err) => Outcome::error(err.to_string()),
            }
        }
        Request::Push { name, value } => push(&name, value, items),

        // `Query(AppMenus)` and `Press(AppMenu)` are answered before `apply`
        // is called: [`crate::menus::list`] and [`crate::menus::press`] are
        // `async`, so `ecs::defer` takes their reply and finishes them off the
        // main thread.
        //
        // Reported rather than `unreachable!`: getting here means a caller
        // routed around `defer` -- which `harness` could, since it calls
        // `apply` directly -- and a daemon that answers "not here" is better
        // than one that takes the process down over a routing mistake.
        Request::Query(ProtocolQuery::AppMenus) | Request::Press(PressTarget::AppMenu(_)) => {
            Outcome::error("this request is answered asynchronously; see `ecs::defer`".to_owned())
        }

        Request::Query(ProtocolQuery::Defaults) => {
            Outcome::answer(Response::Defaults(Box::new(defaults.0.clone())))
        }

        Request::Press(PressTarget::Alias(name)) => {
            // The item's own `alias` is what names the thing to press, not the
            // item's name: the two are usually equal, but a config is free to
            // call a mirrored item anything it likes.
            let Some(entity) = items.index.get(&name) else {
                return no_such(&name);
            };
            let spec = items
                .write
                .get(entity)
                .ok()
                .and_then(|row| row.alias.map(|alias| alias.0.clone()));
            let Some(spec) = spec else {
                return Outcome::error(format!("`{name}` is not an alias"));
            };
            match press_alias(&spec) {
                Ok(()) => Outcome::ok(),
                Err(err) => Outcome::error(err.to_string()),
            }
        }

        Request::BeginConfig => {
            tracing::debug!("config began");
            for row in &items.write {
                items.commands.entity(row.entity).insert(Stale);
            }
            // Declared events are marked by the same bracket and swept by the
            // same `--end`: there is no `--remove event`, so this is the only
            // thing that ever retires one.
            sources.begin_config();
            Outcome::ok()
        }

        Request::EndConfig => {
            // Whatever the client did not touch is what it stopped wanting.
            // Adding or setting an item clears the mark, so this is exactly
            // the set the new config left out.
            let dropped: Vec<(Entity, ItemName)> = items
                .unclaimed
                .iter()
                .map(|(entity, name)| (entity, name.0.clone()))
                .collect();
            tracing::debug!(count = dropped.len(), "config ended");
            for (entity, name) in dropped {
                cache.forget(entity);
                captures.forget(entity);
                subscribers.clear(entity);
                items.index.remove(&name);
                items.commands.entity(entity).despawn();
            }
            sources.end_config();
            Outcome::ok()
        }

        Request::Reload => {
            tracing::info!("reload requested");
            Outcome {
                reload: true,
                ..Outcome::ok()
            }
        }

        Request::Shutdown => {
            tracing::info!("shutting down");
            Outcome {
                exit: true,
                ..Outcome::ok()
            }
        }
    }
}

/// Pushes one sample onto a graph — `SketchyBar`'s `--push <name> <value>`.
///
/// `crate::protocol::Request` has no variant for this yet — see the daemon's
/// own notes on this pass — so nothing calls this outside a test. Written
/// now so wiring it up is a one-line match arm once the variant exists.
///
/// `Graph::would_change` is read before `graphs.get_mut` is ever touched
/// mutably, the same discipline `patched` uses: a script re-reporting the
/// reading it already pushed must not mark the component changed.
#[must_use]
pub fn push(name: &ItemName, value: f32, items: &mut Items) -> Outcome {
    let Some(entity) = items.index.get(name) else {
        return no_such(name);
    };
    let Ok(mut graph) = items.graphs.get_mut(entity) else {
        return Outcome::error(format!("`{name}` is not a graph"));
    };
    if graph.would_change(value) {
        graph.push(value);
    }
    Outcome::ok()
}

/// Writes a patch onto one item.
///
/// Every assignment goes through `set_if_neq` where the type allows, so a
/// config that re-sets an unchanged value does not mark the component changed
/// and trigger a repaint.
fn set_item(
    entity: Entity,
    row: &mut ItemWriteItem<'_, '_>,
    patch: &ItemPatch,
    known: &[ItemName],
    commands: &mut Commands,
) {
    let (icon, label, background, padding, offset, placement, drawing, routine) = (
        &mut row.icon,
        &mut row.label,
        &mut row.background,
        &mut row.padding,
        &mut row.offset,
        &mut row.placement,
        &mut row.drawing,
        &mut row.routine,
    );

    // Only touched when the patch actually carries one of these, and only
    // written when the value differs. `Mut::as_mut` is a `deref_mut`, which
    // marks the component changed whatever you then do with it — so reaching
    // for the icon unconditionally marked every item's text dirty on every
    // `SetItem`, including ones that only set an update frequency. That fed
    // straight into `reshape` and `needs_repaint`, so a script re-setting an
    // unchanged value repainted the bar for as long as it kept running.
    if let Some(next) = patched_run(&icon.0, patch.icon.as_ref()) {
        icon.0 = next;
    }
    if let Some(next) = patched_run(&label.0, patch.label.as_ref()) {
        label.0 = next;
    }

    // The two flattened halves. A config spells their properties as the
    // item's own -- `padding_left=4`, `script=...` -- and the patch keeps
    // them where the state keeps them, so `--query` still answers with the
    // nesting a config reads back.
    let geometry = patch.geometry.as_ref();
    let scripting = patch.scripting.as_ref();

    if let Some(next) = patched_background(
        background,
        geometry.and_then(|geometry| geometry.background.as_ref()),
    ) {
        **background = next;
    }

    // `Padding`, `Offset` and `Placement` are all in the dirty-item query, so
    // they are asked what would *change* before the `Mut` is touched at all —
    // the same discipline `Width` and `ItemDisplay` already had, and which
    // these three were missing: a script re-stating the padding it set last
    // time marked the component changed and repainted for nothing.
    if let Some(left) = geometry
        .and_then(|geometry| geometry.padding_left)
        .and_then(|p| p.changes(&padding.left))
    {
        padding.left = left;
    }
    if let Some(right) = geometry
        .and_then(|geometry| geometry.padding_right)
        .and_then(|p| p.changes(&padding.right))
    {
        padding.right = right;
    }
    if let Some(y) = geometry
        .and_then(|geometry| geometry.y_offset)
        .and_then(|y| y.changes(&offset.0))
    {
        offset.0 = y;
    }
    if let Some(p) = geometry
        .and_then(|geometry| geometry.position.as_ref())
        .and_then(|p| p.changes(&placement.0))
    {
        placement.0 = p;
    }
    if let Some(d) = geometry.and_then(|geometry| geometry.drawing) {
        // Resolved here, not by the caller: `toggle` means "the opposite of
        // whatever it is now", and only the daemon knows what that is.
        drawing.set_if_neq(Drawing(d.resolve(drawing.0.into()).get()));
    }
    if let Some(u) = scripting.and_then(|scripting| scripting.updates) {
        row.updates
            .set_if_neq(Updates(u.resolve(row.updates.0.into()).get()));
    }
    if let Some(w) = geometry.and_then(|geometry| geometry.width) {
        row.width.set_if_neq(Width(Some(w)));
    }
    if let Some(spec) = geometry.and_then(|geometry| geometry.display) {
        row.display.set_if_neq(ItemDisplay(spec));
    }
    if let Some(freq) = scripting
        .and_then(|scripting| scripting.update_freq)
        .and_then(|f| f.changes(&routine.every))
    {
        routine.every = freq;
        // A changed frequency restarts the clock, so setting it twice does not
        // fire early on the second set. Only a *changed* one: re-stating the
        // frequency it already has used to postpone the tick for ever.
        routine.elapsed = 0;
    }
    // Kind-specific properties, written only where they differ so a script
    // re-setting one to what it already is repaints nothing.
    if let Some(percentage) = patch.percentage
        && let Some(slider) = row.slider.as_mut()
    {
        let next = Slider {
            percentage: percentage.min(100),
            ..slider.clone()
        };
        slider.set_if_neq(next);
    }
    if let Some(space) = patch.associated_space
        && let Some(current) = row.associated_space.as_mut()
    {
        current.set_if_neq(AssociatedSpace(space));
    }
    if let Some(popup) = &patch.popup {
        apply_popup(entity, row.popup, popup, commands);
    }
    set_item_components(entity, patch, known, commands);
}

/// The half of a patch that adds or removes whole components rather than
/// writing fields, split out only because the two together outgrew what is
/// readable in one function.
fn set_item_components(
    entity: Entity,
    patch: &ItemPatch,
    known: &[ItemName],
    commands: &mut Commands,
) {
    let scripting = patch.scripting.as_ref();
    if let Some(script) = scripting.and_then(|scripting| scripting.script.as_ref()) {
        if script.is_empty() {
            commands.entity(entity).remove::<Script>();
        } else {
            commands
                .entity(entity)
                .insert(Script(script.as_str().into()));
        }
    }
    if let Some(script) = scripting.and_then(|scripting| scripting.click_script.as_ref()) {
        if script.is_empty() {
            commands.entity(entity).remove::<ClickScript>();
        } else {
            commands
                .entity(entity)
                .insert(ClickScript(script.as_str().into()));
        }
    }
    if let Some(members) = &patch.members {
        // A pattern is resolved against the items that exist now, which is
        // what a config means by it: `sbar.add("bracket", { "/menu\\..*/" })`
        // comes after the items it names. An item added later does not join
        // the bracket -- SketchyBar's own `--add bracket` is likewise a
        // one-time membership, not a standing query.
        let names: Vec<ItemName> = members
            .iter()
            .flat_map(|selector| match selector {
                Selector::Name(name) => vec![name.clone()],
                Selector::Pattern(pattern) => match regex::Regex::new(pattern) {
                    Ok(regex) => known
                        .iter()
                        .filter(|name| regex.is_match(name.as_str()))
                        .cloned()
                        .collect(),
                    Err(err) => {
                        tracing::warn!(pattern, %err, "a bracket's member pattern is not a regex");
                        Vec::new()
                    }
                },
            })
            .collect();
        if names.is_empty() {
            commands.entity(entity).remove::<Members>();
        } else {
            commands.entity(entity).insert(Members(names));
        }
    }
    if let Some(alias) = &patch.alias {
        if alias.is_empty() {
            commands
                .entity(entity)
                .remove::<(AliasSpec, AliasContent)>();
        } else {
            commands
                .entity(entity)
                .insert((AliasSpec(alias.clone()), AliasContent::default()));
        }
    }
}

/// What the patch would make of this run, or `None` if it would make no
/// difference.
///
/// Compared before anything is built, so the common case — a script re-setting
/// the value it set last time — allocates nothing and, at the call site, never
/// reaches for the `Mut`. `Mut::as_mut` is a `deref_mut`: touching a component
/// at all marks it changed, and a component marked changed reshapes its text
/// and repaints its rect whether or not a pixel moved.
fn patched_run(current: &Run, patch: Option<&RunPatch>) -> Option<Run> {
    patched(current, patch)
}

/// The same for the surface behind an item.
fn patched_background(current: &Background, patch: Option<&BackgroundPatch>) -> Option<Background> {
    patched(current, patch)
}

/// Applies a patch to a copy, and reports it only if the copy differs.
///
/// The merge is the patch type's own derived [`Changes`] impl, generated from
/// the same declaration as the patch struct, so a property added to one is a
/// property the other already knows about. What is applied to is the wire
/// form, because that is what the patch is defined against; the daemon's own
/// form parses a font and a colour out of it, and the two conversions are
/// exhaustive struct literals, so a new field fails to compile here rather
/// than silently going unread.
///
/// `Changes` answers what actually *changed*, not what the patch *wrote* — a
/// config setting a colour to the colour it already had writes it and changes
/// nothing. That difference is the whole question when an item repaints more
/// than it should, and it is now the only thing this reports: a `None` here
/// is a repaint that does not happen.
fn patched<T, W, P>(current: &T, patch: Option<&P>) -> Option<T>
where
    T: PartialEq + for<'a> From<&'a W>,
    W: Clone + for<'a> From<&'a T>,
    P: crate::protocol::Changes<W>,
{
    let next = T::from(&patch?.changes(&W::from(current))?);
    (next != *current).then_some(next)
}

const _: fn(&Run) = |_| {};

#[cfg(test)]
mod patch_tests {
    use super::{patched_background, patched_run};
    use crate::components::{Background, Run};
    use crate::protocol::style::Color;
    use crate::protocol::{BackgroundPatch, RunPatch};

    fn background() -> Background {
        Background {
            drawing: true,
            color: Color(0xff11_2233),
            corner_radius: 4.0,
            height: 20.0,
            padding_left: 2.0,
            padding_right: 3.0,
            border_color: Color(0xff44_5566),
            border_width: 1.5,
        }
    }

    /// `Mut::as_mut` marks a component changed the moment it is touched,
    /// whatever is then written — so re-applying the value a background
    /// already has must be caught here, before the write, or every one of
    /// these new fields would repaint the bar for as long as a script kept
    /// re-setting them.
    #[test]
    fn a_background_patch_matching_every_current_field_changes_nothing() {
        let current = background();
        let patch = BackgroundPatch {
            drawing: Some(if current.drawing {
                crate::protocol::BoolChange::True
            } else {
                crate::protocol::BoolChange::False
            }),
            color: Some(current.color),
            corner_radius: Some(current.corner_radius),
            height: Some(current.height),
            padding_left: Some(current.padding_left),
            padding_right: Some(current.padding_right),
            border_color: Some(current.border_color),
            border_width: Some(current.border_width),
        };
        assert_eq!(patched_background(&current, Some(&patch)), None);
    }

    #[test]
    fn a_background_patch_moving_one_new_field_is_reported() {
        let current = background();
        let patch = BackgroundPatch {
            border_width: Some(current.border_width + 1.0),
            ..BackgroundPatch::default()
        };
        let next = patched_background(&current, Some(&patch)).expect("border width moved");
        assert!((next.border_width - (current.border_width + 1.0)).abs() < 1e-9);
        assert!(
            (next.height - current.height).abs() < 1e-9,
            "nothing else should move"
        );
    }

    #[test]
    fn a_run_patch_matching_its_current_padding_changes_nothing() {
        let mut current = Run::new("Menlo:Regular:13", Color::WHITE);
        current.padding_left = 4.0;
        current.padding_right = 6.0;
        let patch = RunPatch {
            padding_left: Some(current.padding_left),
            padding_right: Some(current.padding_right),
            ..RunPatch::default()
        };
        assert_eq!(patched_run(&current, Some(&patch)), None);
    }

    #[test]
    fn a_run_patch_widening_its_padding_is_reported() {
        let current = Run::new("Menlo:Regular:13", Color::WHITE);
        let patch = RunPatch {
            padding_left: Some(5.0),
            ..RunPatch::default()
        };
        let next = patched_run(&current, Some(&patch)).expect("padding moved");
        assert!((next.padding_left - 5.0).abs() < 1e-9);
    }
}

#[cfg(test)]
mod component_tests {
    use crate::harness::Harness;
    use crate::protocol::{ComponentKind, ItemName, Position, Request, Response};

    fn name(s: &str) -> ItemName {
        ItemName::new(s).expect("valid name")
    }

    #[test]
    fn adding_a_space_spawns_a_real_item_and_watches_space_changes_on_its_own() {
        // Real `SketchyBar` sets `UPDATE_SPACE_CHANGE` the moment a `space`
        // item is created (`bar_item_set_type` in `bar_item.c`) -- a config
        // never has to `--subscribe` it itself.
        let mut bar = Harness::new();
        let outcome = bar.apply(Request::Add(ComponentKind::Space {
            name: name("space.1"),
            position: Position::Left,
        }));
        assert_eq!(outcome.response, Response::Ok);
        assert_eq!(bar.order(), ["space.1"]);
        assert!(
            bar.running("spaces"),
            "a space item should start the spaces source unasked"
        );
        assert_eq!(
            bar.items()[0].events,
            vec![crate::protocol::Kind::SpaceChanged],
            "and report it, the same as an explicit --subscribe would"
        );
    }

    #[test]
    fn adding_a_graph_or_a_slider_spawns_a_real_item_and_watches_nothing() {
        // Neither is driven by an event: a graph is driven by `--push`, a
        // slider by a percentage set. Nothing here should start a source.
        let mut bar = Harness::new();
        for (item, kind) in [("cpu", "graph"), ("volume_options", "slider")] {
            let kind = match kind {
                "graph" => ComponentKind::Graph {
                    name: name(item),
                    position: Position::Right,
                },
                _ => ComponentKind::Slider {
                    name: name(item),
                    position: Position::Right,
                },
            };
            let outcome = bar.apply(Request::Add(kind));
            assert_eq!(outcome.response, Response::Ok);
        }
        assert_eq!(bar.order(), ["cpu", "volume_options"]);
        assert!(!bar.running("spaces"));
    }

    #[test]
    fn re_adding_a_component_by_name_moves_it_rather_than_duplicating_it() {
        // Same identity-survives-a-reload contract `AddItem` already has.
        let mut bar = Harness::new();
        bar.apply(Request::Add(ComponentKind::Graph {
            name: name("cpu"),
            position: Position::Left,
        }));
        bar.apply(Request::Add(ComponentKind::Graph {
            name: name("cpu"),
            position: Position::Right,
        }));

        let items = bar.items();
        assert_eq!(items.len(), 1, "the same name is the same item");
        assert_eq!(items[0].geometry.position, Position::Right, "and it moved");
    }
}

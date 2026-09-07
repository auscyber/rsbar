//! Applying requests to the item world.

#![allow(
    clippy::needless_pass_by_value,
    reason = "Bevy system parameters are taken by value by contract"
)]

use crate::bar::{Changes, Panels, Settings};
use crate::components::{
    AliasContent, AliasSpec, Background, ClickScript, DisplayTarget, Drawing, Icon, Index,
    ItemDisplay, ItemHandle, Label, Members, Name, Offset, Order, Padding, Placement, Routine, Run,
    Script, Stale, Subscriptions, Updates, Watching, Width, bundle,
};
use crate::script::Job;
use crate::shaping::Cache;
use crate::sources::{Registry, Target};
use crate::subscribers::Subscribers;
use bevy_ecs::prelude::*;
use bevy_ecs::query::QueryData;
use bevy_ecs::system::SystemParam;
use rsbar_protocol::{
    BackgroundPatch, Event, ItemName, ItemPatch, ItemState, Kind, Patch, Query as ProtocolQuery,
    Relative, Request, Response, RunPatch, Selector,
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
}

impl Items<'_, '_> {
    /// The scripts to run for `event`, read through the write query.
    /// The jobs an event produces for the items that depend on it.
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
                    event: Arc::new(Event::Forced(rsbar_protocol::event::Forced {})),
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

/// The same projection as [`state_of`], off the read-only view of the write
/// query — `iter()` on a mutable query yields plain references, not `Mut`.
fn write_state(row: &ItemWriteReadOnlyItem<'_, '_>) -> ItemState {
    ItemState {
        name: row.name.0.clone(),
        geometry: rsbar_protocol::Geometry {
            drawing: row.drawing.0,
            position: row.placement.0,
            y_offset: row.offset.0,
            padding_left: row.padding.left,
            padding_right: row.padding.right,
            width: row.width.0,
            background: row.background.into(),
        },
        icon: (&row.icon.0).into(),
        label: (&row.label.0).into(),
        scripting: rsbar_protocol::Scripting {
            script: row.script.map(|s| s.0.to_string()),
            click_script: row.click.map(|s| s.0.to_string()),
            update_freq: row.routine.every,
            updates: row.updates.0,
        },
        events: row.subscriptions.0.iter().cloned().collect(),
        alias: row.alias.map(|alias| alias.0.clone()),
        members: row.members.map(|m| m.0.clone()).unwrap_or_default(),
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
                    event: Arc::new(Event::Forced(rsbar_protocol::event::Forced {})),
                })
            })
            .collect()
    }

    #[must_use]
    pub fn states(&self) -> Vec<ItemState> {
        self.read.iter().map(|row| state_of(&row)).collect()
    }

    #[must_use]
    pub fn state(&self, name: &ItemName) -> Option<ItemState> {
        self.read
            .iter()
            .find(|row| &row.name.0 == name)
            .map(|row| state_of(&row))
    }
}

fn state_of(row: &ItemReadItem<'_, '_>) -> ItemState {
    ItemState {
        name: row.name.0.clone(),
        geometry: rsbar_protocol::Geometry {
            drawing: row.drawing.0,
            position: row.placement.0,
            y_offset: row.offset.0,
            padding_left: row.padding.left,
            padding_right: row.padding.right,
            width: row.width.0,
            background: row.background.into(),
        },
        icon: (&row.icon.0).into(),
        label: (&row.label.0).into(),
        scripting: rsbar_protocol::Scripting {
            script: row.script.map(|s| s.0.to_string()),
            click_script: row.click.map(|s| s.0.to_string()),
            update_freq: row.routine.every,
            updates: row.updates.0,
        },
        events: row.subscriptions.0.iter().cloned().collect(),
        alias: row.alias.map(|alias| alias.0.clone()),
        members: row.members.map(|m| m.0.clone()).unwrap_or_default(),
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
        Kind::MouseClicked
            | Kind::MouseScrolled
            | Kind::MouseScrolledGlobal
            | Kind::MouseEntered
            | Kind::MouseExited
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
        .chain(clickable.then_some(Kind::MouseClicked))
}

/// Lets the bar take clicks at all.
///
/// The source reports them and this tag is what gets them delivered; without
/// either there is nothing to report.
fn take_clicks(panels: &Panels) {
    if let Err(err) = panels.set_clickable(true) {
        tracing::error!(%err, "could not make the bar take clicks");
    }
}

fn no_such(name: &ItemName) -> Outcome {
    Outcome::error(format!("no item named `{name}`"))
}

/// Everything outside the item world that a request can reach.
pub struct Context<'a> {
    pub settings: &'a mut Settings,
    pub panels: &'a mut Panels,
    pub cache: &'a mut Cache,
    /// The mirrored images, so an item that goes takes its capture with it.
    pub captures: &'a mut crate::alias::Captures,
    pub sources: &'a mut Registry,
    pub subscribers: &'a mut Subscribers,
    /// A port this request arrived with, if the client wants its events
    /// pushed back rather than run as a script.
    pub subscriber: Option<async_mach_ports::Subscriber>,
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
    for entity in &fresh {
        if let Ok(mut row) = items.write.get_mut(entity) {
            set_item(entity, &mut row, &defaults.0, &mut items.commands);
        }
        items.commands.entity(entity).remove::<NeedsDefaults>();
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
fn press_alias(spec: &str) -> Result<(), crate::alias::Error> {
    let (owner, name) = spec.split_once(',').unwrap_or((spec, spec));
    crate::alias::press_item(owner, name)
}

/// Applies one request.
#[allow(
    clippy::too_many_lines,
    reason = "one arm per request; splitting hides the shape"
)]
pub fn apply(request: Request, items: &mut Items, ctx: &mut Context<'_>) -> Outcome {
    let Context {
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
                    panels.rebuild(settings)?;
                } else if changes.contains(Changes::GEOMETRY) {
                    panels.reframe(settings)?;
                }
                if changes.contains(Changes::BLUR) {
                    panels.set_blur(settings.blur_radius)?;
                }
                if changes.contains(Changes::VISIBILITY) {
                    panels.set_hidden(settings.hidden)?;
                }
                if changes.contains(Changes::LEVEL) {
                    panels.set_level(settings)?;
                }
                Ok(())
            })();
            match result {
                Ok(()) => Outcome::ok(),
                Err(err) => Outcome::error(err.to_string()),
            }
        }

        Request::AddItem { name, position } => {
            tracing::debug!(%name, ?position, "add item");
            if let Some(entity) = items.index.get(&name) {
                // Re-adding an item that exists moves it rather than replacing
                // it. That is what makes a reload cheap: entity identity
                // survives, so change detection sees only what actually
                // changed rather than every item disappearing and coming back.
                if let Ok(mut row) = items.write.get_mut(entity) {
                    row.placement.0 = position;
                }
                items.commands.entity(entity).remove::<Stale>();
                return Outcome::ok();
            }
            let order = items.index.next_order();
            let entity = items
                .commands
                .spawn(bundle(name.clone(), position, order))
                .id();
            items.index.insert(name, entity);
            // Applied through the ordinary patch path rather than baked into
            // the bundle, so a default and an explicit `--set` of the same
            // property behave identically -- including doing nothing when the
            // value is already what the bundle starts with.
            if defaults.0 != ItemPatch::default() {
                items.commands.entity(entity).insert(NeedsDefaults);
            }
            Outcome::ok()
        }

        // `ComponentKind` is modelled in the protocol precisely so a config
        // using one gets a named error here rather than a plain item it
        // never asked for, or never reaching the daemon at all — see the
        // doc on `Request::AddComponent`. Drawing a slider or graph is a
        // different feature from the properties this pass covers; nothing
        // here draws one yet.
        Request::AddComponent { name, kind, .. } => Outcome::error(format!(
            "`{name}`: `{kind:?}` items are not implemented yet"
        )),

        Request::SetItem { name, patch } => {
            tracing::trace!(%name, ?patch, "set item");
            let Some(entity) = items.index.get(&name) else {
                return no_such(&name);
            };
            let Ok(mut row) = items.write.get_mut(entity) else {
                return no_such(&name);
            };
            let wants_clicks = patch.click_script.as_ref().is_some_and(|s| !s.is_empty());
            let subscribed = wants_clicks.then(|| row.subscriptions.0.clone());
            set_item(entity, &mut row, &patch, &mut items.commands);
            // Touching an item during a reload is what keeps it: a config that
            // only sets an existing item, without re-adding it, must not have
            // it swept up as stale.
            items.commands.entity(entity).remove::<Stale>();
            if let Some(subscribed) = subscribed {
                let watches = sources.watch_all(entity, needs(&subscribed, true));
                items.commands.entity(entity).insert(Watching(watches));
                take_clicks(panels);
            }
            Outcome::ok()
        }

        Request::RemoveItem(name) => {
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

        Request::SetMatching { pattern, patch } => {
            let matched = match matching(&pattern, items) {
                Ok(matched) => matched,
                Err(err) => return Outcome::error(err.to_string()),
            };
            tracing::debug!(pattern, matched = matched.len(), "set matching");
            for entity in matched {
                let Ok(mut row) = items.write.get_mut(entity) else {
                    continue;
                };
                set_item(entity, &mut row, &patch, &mut items.commands);
                items.commands.entity(entity).remove::<Stale>();
            }
            Outcome::ok()
        }

        Request::RemoveMatching(pattern) => {
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
                take_clicks(panels);
            }
            row.subscriptions.0 = subscribed;
            Outcome::ok()
        }

        Request::Trigger(event) => {
            tracing::debug!(kind = %event.kind(), "trigger");
            // A triggered event is not aimed anywhere, so it reaches whoever
            // claimed it — the same path a source's event takes.
            let event = Arc::new(event);
            let mut dependents = sources.dependents(&event, &Target::All);
            // Whoever holds a port takes it; only the rest fall back to a
            // script, exactly as a source's event does.
            dependents.retain(|item| !subscribers.push(*item, &event));
            Outcome {
                jobs: items.jobs_for(&event, &dependents),
                ..Outcome::ok()
            }
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
        Request::Query(ProtocolQuery::MenuItems) => match crate::alias::list_menu_bar_items() {
            Ok(found) => Outcome::answer(Response::MenuItems(
                found
                    .into_iter()
                    .map(|item| format!("{},{}", item.owner, item.name))
                    .collect(),
            )),
            Err(err) => Outcome::error(err.to_string()),
        },
        Request::Query(ProtocolQuery::AppMenus) => match crate::menus::list() {
            Ok(found) => Outcome::answer(Response::AppMenus(
                found.into_iter().map(|menu| menu.title).collect(),
            )),
            Err(err) => Outcome::error(err.to_string()),
        },

        // `SetDefault` is not implemented (see its own arm above), so there
        // is never anything stashed to report — an empty patch is the
        // honest answer, not a guess.
        Request::Query(ProtocolQuery::Defaults) => {
            Outcome::answer(Response::Defaults(Box::default()))
        }

        Request::PressAppMenu(index) => match crate::menus::press(index) {
            Ok(()) => Outcome::ok(),
            Err(err) => Outcome::error(err.to_string()),
        },

        Request::PressAlias(name) => {
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

/// Writes a patch onto one item.
///
/// Every assignment goes through `set_if_neq` where the type allows, so a
/// config that re-sets an unchanged value does not mark the component changed
/// and trigger a repaint.
fn set_item(
    entity: Entity,
    row: &mut ItemWriteItem<'_, '_>,
    patch: &ItemPatch,
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
    if let Some(next) = patched_background(background, patch.background.as_ref()) {
        **background = next;
    }

    if let Some(p) = patch.padding_left {
        padding.left = p;
    }
    if let Some(p) = patch.padding_right {
        padding.right = p;
    }
    if let Some(y) = patch.y_offset {
        offset.0 = y;
    }
    if let Some(p) = patch.position {
        placement.0 = p;
    }
    if let Some(d) = patch.drawing {
        drawing.0 = d;
    }
    // `Width` and `ItemDisplay` affect layout, so — unlike the plain
    // assignments above — they go through `set_if_neq`: a script re-setting
    // the display or width it already has must not repaint the bar.
    if let Some(u) = patch.updates {
        row.updates.set_if_neq(Updates(u));
    }
    if let Some(w) = patch.width {
        row.width.set_if_neq(Width(Some(w)));
    }
    if let Some(spec) = &patch.display {
        row.display
            .set_if_neq(ItemDisplay(DisplayTarget::parse(spec)));
    }
    if let Some(freq) = patch.update_freq {
        routine.every = freq;
        // A changed frequency restarts the clock, so setting it twice does not
        // fire early on the second set.
        routine.elapsed = 0;
    }
    if let Some(script) = &patch.script {
        if script.is_empty() {
            commands.entity(entity).remove::<Script>();
        } else {
            commands
                .entity(entity)
                .insert(Script(script.as_str().into()));
        }
    }
    if let Some(script) = &patch.click_script {
        if script.is_empty() {
            commands.entity(entity).remove::<ClickScript>();
        } else {
            commands
                .entity(entity)
                .insert(ClickScript(script.as_str().into()));
        }
    }
    if let Some(members) = &patch.members {
        // A member named by pattern would have to be re-resolved against the
        // live item list on every change to it, not just when the bracket
        // itself is set — nothing here does that yet, so a pattern is
        // reported and dropped rather than silently matching nothing forever.
        let names: Vec<ItemName> = members
            .iter()
            .filter_map(|selector| match selector {
                Selector::Name(name) => Some(name.clone()),
                Selector::Pattern(pattern) => {
                    tracing::warn!(
                        pattern,
                        "bracket membership by pattern is not implemented yet; ignoring"
                    );
                    None
                }
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
/// The same for the surface behind an item.
fn patched_run(current: &Run, patch: Option<&RunPatch>) -> Option<Run> {
    patched(current, patch)
}

/// The same for the surface behind an item.
fn patched_background(current: &Background, patch: Option<&BackgroundPatch>) -> Option<Background> {
    patched(current, patch)
}

/// Applies a patch to a copy, and reports it only if the copy differs.
///
/// The merge itself is `struct_patch`'s own [`Patch::apply_with_log`],
/// generated from the same declaration as the patch struct, so a property
/// added to one is a property the other already knows about. What is applied
/// to is the wire form, because that is what the patch is defined against;
/// the daemon's own form parses a font and a colour out of it, and the two
/// conversions are exhaustive struct literals, so a new field fails to
/// compile here rather than silently going unread.
///
/// The log names the fields the patch *wrote*, which is not the same as the
/// fields that *changed* — a config setting a colour to the colour it already
/// had writes it and changes nothing. That difference is the whole question
/// when an item is repainting more than it should, so both halves are traced:
/// the field names here, and whether anything came of them below.
fn patched<T, W, P>(current: &T, patch: Option<&P>) -> Option<T>
where
    T: Clone + PartialEq + for<'a> From<&'a W>,
    W: for<'a> From<&'a T> + Patch<P>,
    P: Clone,
{
    let patch = patch?;
    let mut wire = W::from(current);
    wire.apply_with_log(patch.clone(), |field| {
        tracing::trace!(field, "patch wrote");
    });
    let next = T::from(&wire);
    (next != *current).then_some(next)
}

const _: fn(&Run) = |_| {};

#[cfg(test)]
mod patch_tests {
    use super::{patched_background, patched_run};
    use crate::components::{Background, Run};
    use rsbar_protocol::style::Color;
    use rsbar_protocol::{BackgroundPatch, RunPatch};

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
            drawing: Some(current.drawing),
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

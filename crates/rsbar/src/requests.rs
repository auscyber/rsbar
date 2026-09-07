//! Applying requests to the item world.

#![allow(
    clippy::needless_pass_by_value,
    reason = "Bevy system parameters are taken by value by contract"
)]

use crate::bar::{Changes, Panels, Settings};
use crate::components::{
    AliasContent, AliasSpec, Background, ClickScript, Drawing, Icon, Index, ItemHandle, Label,
    Name, Offset, Padding, Placement, Routine, Run, Script, Stale, Subscriptions, Watching, bundle,
};
use crate::script::Job;
use crate::shaping::Cache;
use crate::sources::{Registry, Target};
use crate::subscribers::Subscribers;
use bevy_ecs::prelude::*;
use bevy_ecs::query::QueryData;
use bevy_ecs::system::SystemParam;
use rsbar_protocol::style::{Color, FontSpec};
use rsbar_protocol::{
    Event, ItemName, ItemPatch, ItemState, Kind, Query as ProtocolQuery, Request, Response,
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
    pub drawing: &'static mut Drawing,
    pub routine: &'static mut Routine,
    pub subscriptions: &'static mut Subscriptions,
    pub script: Option<&'static Script>,
    pub click: Option<&'static ClickScript>,
    pub alias: Option<&'static AliasSpec>,
}

/// Every component a query or a dispatch reads.
#[derive(QueryData)]
pub struct ItemRead {
    pub entity: Entity,
    pub name: &'static Name,
    pub icon: &'static Icon,
    pub label: &'static Label,
    pub placement: &'static Placement,
    pub drawing: &'static Drawing,
    pub routine: &'static Routine,
    pub subscriptions: &'static Subscriptions,
    pub script: Option<&'static Script>,
    pub click: Option<&'static ClickScript>,
    pub alias: Option<&'static AliasSpec>,
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
                Some(Job {
                    item: ItemHandle::new(row.entity, row.name.0.clone()),
                    script: Arc::clone(&row.script?.0),
                    event: Arc::clone(event),
                })
            })
            .collect()
    }

    /// Every item's script, regardless of frequency or subscription.
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
    pub fn states(&self) -> Vec<ItemState> {
        self.write.iter().map(|row| write_state(&row)).collect()
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
        position: row.placement.0,
        icon: row.icon.0.string.clone(),
        label: row.label.0.string.clone(),
        drawing: row.drawing.0,
        script: row.script.map(|s| s.0.to_string()),
        click_script: row.click.map(|s| s.0.to_string()),
        update_freq: row.routine.every,
        events: row.subscriptions.0.iter().cloned().collect(),
        alias: row.alias.map(|alias| alias.0.clone()),
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
        if subscriptions.0.iter().any(|kind| kind.matches(event))
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

    /// Every item's script, regardless of frequency or subscription.
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
        position: row.placement.0,
        icon: row.icon.0.string.clone(),
        label: row.label.0.string.clone(),
        drawing: row.drawing.0,
        script: row.script.map(|s| s.0.to_string()),
        click_script: row.click.map(|s| s.0.to_string()),
        update_freq: row.routine.every,
        events: row.subscriptions.0.iter().cloned().collect(),
        alias: row.alias.map(|alias| alias.0.clone()),
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
    } = ctx;
    match request {
        Request::SetBar(patch) => {
            tracing::debug!(?patch, "set bar");
            let changes = settings.apply(&patch);
            let result = (|| -> skylight::Result<()> {
                if changes.contains(Changes::GEOMETRY) {
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
            let entity = items.commands.spawn(bundle(name.clone(), position)).id();
            items.index.insert(name, entity);
            Outcome::ok()
        }

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
    if let Some(next) = patched_run(
        &icon.0,
        patch.icon.as_deref(),
        patch.icon_font.as_deref(),
        patch.icon_color,
    ) {
        icon.0 = next;
    }
    if let Some(next) = patched_run(
        &label.0,
        patch.label.as_deref(),
        patch.label_font.as_deref(),
        patch.label_color,
    ) {
        label.0 = next;
    }

    if let Some(c) = patch.background_color {
        background.color = Color(c);
    }
    if let Some(r) = patch.corner_radius {
        background.corner_radius = r;
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
fn patched_run(
    current: &Run,
    string: Option<&str>,
    font: Option<&str>,
    color: Option<u32>,
) -> Option<Run> {
    let font = font.map(FontSpec::parse);
    let differs = string.is_some_and(|s| s != current.string)
        || font.as_ref().is_some_and(|f| *f != current.font)
        || color.is_some_and(|c| Color(c) != current.color);
    if !differs {
        return None;
    }

    let mut next = current.clone();
    if let Some(s) = string {
        s.clone_into(&mut next.string);
    }
    if let Some(f) = font {
        next.font = f;
    }
    if let Some(c) = color {
        next.color = Color(c);
    }
    Some(next)
}

const _: fn(&Run) = |_| {};

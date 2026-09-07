//! Applying requests to the item world.

#![allow(
    clippy::needless_pass_by_value,
    reason = "Bevy system parameters are taken by value by contract"
)]

use crate::bar::{Panels, Settings};
use crate::components::{
    Background, ClickScript, Drawing, Icon, Index, Label, Name, Offset, Padding, Placement,
    Routine, Run, Script, Stale, Subscriptions, bundle,
};
use crate::script::Job;
use crate::shaping::Cache;
use crate::sources::Registry;
use bevy_ecs::prelude::*;
use bevy_ecs::system::SystemParam;
use rsbar_protocol::style::{Color, FontSpec};
use rsbar_protocol::{
    Event, ItemName, ItemPatch, ItemState, Kind, Query as ProtocolQuery, Request, Response,
};

/// Every component a request can write.
type WriteComponents = (
    &'static Name,
    &'static mut Icon,
    &'static mut Label,
    &'static mut Background,
    &'static mut Padding,
    &'static mut Offset,
    &'static mut Placement,
    &'static mut Drawing,
    &'static mut Routine,
    &'static mut Subscriptions,
    Option<&'static Script>,
    Option<&'static ClickScript>,
);

/// Every component a query or a dispatch reads.
type ReadComponents = (
    Entity,
    &'static Name,
    &'static Icon,
    &'static Label,
    &'static Placement,
    &'static Drawing,
    &'static Routine,
    &'static Subscriptions,
    Option<&'static Script>,
    Option<&'static ClickScript>,
);

/// Everything a request may write to in the item world.
///
/// One query holding all of it, rather than several: Bevy derives a system's
/// access statically, and two queries that could both match an item would be a
/// conflict at runtime.
#[derive(SystemParam)]
pub struct Items<'w, 's> {
    pub commands: Commands<'w, 's>,
    pub index: ResMut<'w, Index>,
    pub write: Query<'w, 's, WriteComponents>,
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
    pub read: Query<'w, 's, ReadComponents>,
}

impl Items<'_, '_> {
    /// The scripts to run for `event`, read through the write query.
    #[must_use]
    pub fn jobs_for(&self, event: &Event) -> Vec<Job> {
        self.write
            .iter()
            .filter(|row| row.9.0.iter().any(|kind| kind.matches(event)))
            .filter_map(|row| {
                Some(Job {
                    item: row.0.0.clone(),
                    script: row.10?.0.clone(),
                    event: event.clone(),
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
                    item: row.0.0.clone(),
                    script: row.10?.0.clone(),
                    event: Event::Forced(rsbar_protocol::event::Forced {}),
                })
            })
            .collect()
    }

    #[must_use]
    pub fn states(&self) -> Vec<ItemState> {
        self.write.iter().map(write_state).collect()
    }

    #[must_use]
    pub fn state(&self, name: &ItemName) -> Option<ItemState> {
        self.write
            .iter()
            .find(|row| &row.0.0 == name)
            .map(write_state)
    }
}

/// The same projection as [`state_of`], off the read-only view of the write
/// query — `iter()` on a mutable query yields plain references, not `Mut`.
#[allow(clippy::type_complexity, reason = "this is the query's own row shape")]
fn write_state(
    row: (
        &Name,
        &Icon,
        &Label,
        &Background,
        &Padding,
        &Offset,
        &Placement,
        &Drawing,
        &Routine,
        &Subscriptions,
        Option<&Script>,
        Option<&ClickScript>,
    ),
) -> ItemState {
    let (name, icon, label, _, _, _, placement, drawing, routine, subscriptions, script, click) =
        row;
    ItemState {
        name: name.0.clone(),
        position: placement.0,
        icon: icon.0.string.clone(),
        label: label.0.string.clone(),
        drawing: drawing.0,
        script: script.map(|s| s.0.clone()),
        click_script: click.map(|s| s.0.clone()),
        update_freq: routine.every,
        events: subscriptions.0.iter().cloned().collect(),
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
    pub fn jobs_for_item(&self, entity: Entity, event: &Event) -> Vec<Job> {
        let Ok(row) = self.read.get(entity) else {
            return Vec::new();
        };
        let (_, name, .., subscriptions, script, click) = row;

        let mut jobs = Vec::new();
        if let Some(click) = click {
            jobs.push(Job {
                item: name.0.clone(),
                script: click.0.clone(),
                event: event.clone(),
            });
        }
        if subscriptions.0.iter().any(|kind| kind.matches(event))
            && let Some(script) = script
        {
            jobs.push(Job {
                item: name.0.clone(),
                script: script.0.clone(),
                event: event.clone(),
            });
        }
        jobs
    }

    /// The scripts to run for `event`. Items without a script are skipped:
    /// subscribing a scriptless item is legal and simply does nothing.
    #[must_use]
    pub fn jobs_for(&self, event: &Event) -> Vec<Job> {
        self.read
            .iter()
            .filter(|row| row.7.0.iter().any(|kind| kind.matches(event)))
            .filter_map(|(_, name, .., script)| {
                Some(Job {
                    item: name.0.clone(),
                    script: script?.0.clone(),
                    event: event.clone(),
                })
            })
            .collect()
    }

    /// Every item's script, regardless of frequency or subscription.
    #[must_use]
    pub fn all_jobs(&self) -> Vec<Job> {
        self.read
            .iter()
            .filter_map(|(_, name, .., script)| {
                Some(Job {
                    item: name.0.clone(),
                    script: script?.0.clone(),
                    event: Event::Forced(rsbar_protocol::event::Forced {}),
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
            .find(|(_, n, ..)| &n.0 == name)
            .map(|row| state_of(&row))
    }
}

type ReadRow<'a> = (
    Entity,
    &'a Name,
    &'a Icon,
    &'a Label,
    &'a Placement,
    &'a Drawing,
    &'a Routine,
    &'a Subscriptions,
    Option<&'a Script>,
    Option<&'a ClickScript>,
);

fn state_of(row: &ReadRow<'_>) -> ItemState {
    let (_, name, icon, label, placement, drawing, routine, subscriptions, script, click) = row;
    ItemState {
        name: name.0.clone(),
        position: placement.0,
        icon: icon.0.string.clone(),
        label: label.0.string.clone(),
        drawing: drawing.0,
        script: script.map(|s| s.0.clone()),
        click_script: click.map(|s| s.0.clone()),
        update_freq: routine.every,
        events: subscriptions.0.iter().cloned().collect(),
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

/// Starts the mouse source and lets the bar take clicks.
///
/// Both happen together on purpose: without the source nothing is reported, and
/// without the window tag nothing is delivered to report.
fn make_clickable(sources: &mut Registry, panels: &Panels) {
    sources.ensure(&Kind::MouseClicked);
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
    pub sources: &'a mut Registry,
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
        sources,
    } = ctx;
    match request {
        Request::SetBar(patch) => {
            tracing::debug!(?patch, "set bar");
            let changes = settings.apply(&patch);
            let result = (|| -> skylight::Result<()> {
                if changes.geometry {
                    panels.reframe(settings)?;
                }
                if changes.blur {
                    panels.set_blur(settings.blur_radius)?;
                }
                if changes.visibility {
                    panels.set_hidden(settings.hidden)?;
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
                    row.6.0 = position;
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
            let Ok(row) = items.write.get_mut(entity) else {
                return no_such(&name);
            };
            let wants_clicks = patch.click_script.as_ref().is_some_and(|s| !s.is_empty());
            set_item(entity, row, &patch, &mut items.commands);
            // Touching an item during a reload is what keeps it: a config that
            // only sets an existing item, without re-adding it, must not have
            // it swept up as stale.
            items.commands.entity(entity).remove::<Stale>();
            if wants_clicks {
                make_clickable(sources, panels);
            }
            Outcome::ok()
        }

        Request::RemoveItem(name) => {
            tracing::debug!(%name, "remove item");
            let Some(entity) = items.index.remove(&name) else {
                return no_such(&name);
            };
            cache.forget(entity);
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
            // observes, so it is what starts a source.
            sources.ensure_all(&events);
            if events.iter().any(is_pointer) {
                make_clickable(sources, panels);
            }
            row.9.0 = events.into_iter().collect();
            Outcome::ok()
        }

        Request::Trigger(event) => {
            tracing::debug!(kind = %event.kind(), "trigger");
            Outcome {
                jobs: items.jobs_for(&event),
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

type WriteRow<'a> = (
    &'a Name,
    bevy_ecs::change_detection::Mut<'a, Icon>,
    bevy_ecs::change_detection::Mut<'a, Label>,
    bevy_ecs::change_detection::Mut<'a, Background>,
    bevy_ecs::change_detection::Mut<'a, Padding>,
    bevy_ecs::change_detection::Mut<'a, Offset>,
    bevy_ecs::change_detection::Mut<'a, Placement>,
    bevy_ecs::change_detection::Mut<'a, Drawing>,
    bevy_ecs::change_detection::Mut<'a, Routine>,
    bevy_ecs::change_detection::Mut<'a, Subscriptions>,
    Option<&'a Script>,
    Option<&'a ClickScript>,
);

/// Writes a patch onto one item.
///
/// Every assignment goes through `set_if_neq` where the type allows, so a
/// config that re-sets an unchanged value does not mark the component changed
/// and trigger a repaint.
fn set_item(entity: Entity, mut row: WriteRow<'_>, patch: &ItemPatch, commands: &mut Commands) {
    let (_, icon, label, background, padding, offset, placement, drawing, routine, _, _, _) =
        &mut row;

    apply_run(
        icon.as_mut(),
        patch.icon.as_deref(),
        patch.icon_font.as_deref(),
        patch.icon_color,
    );
    apply_run_label(label.as_mut(), patch);

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
            commands.entity(entity).insert(Script(script.clone()));
        }
    }
    if let Some(script) = &patch.click_script {
        if script.is_empty() {
            commands.entity(entity).remove::<ClickScript>();
        } else {
            commands.entity(entity).insert(ClickScript(script.clone()));
        }
    }
}

fn apply_run(run: &mut Icon, string: Option<&str>, font: Option<&str>, color: Option<u32>) {
    if let Some(s) = string {
        s.clone_into(&mut run.0.string);
    }
    if let Some(f) = font {
        run.0.font = FontSpec::parse(f);
    }
    if let Some(c) = color {
        run.0.color = Color(c);
    }
}

fn apply_run_label(run: &mut Label, patch: &ItemPatch) {
    if let Some(s) = &patch.label {
        run.0.string.clone_from(s);
    }
    if let Some(f) = &patch.label_font {
        run.0.font = FontSpec::parse(f);
    }
    if let Some(c) = patch.label_color {
        run.0.color = Color(c);
    }
}

const _: fn(&Run) = |_| {};

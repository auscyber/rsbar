//! The daemon as a Bevy app.
//!
//! # Why a custom runner
//!
//! Two constraints meet here and have the same answer.
//!
//! A window server window only composites while its process pumps a
//! `CFRunLoop` — drawing from a process that then blocks in `sleep` succeeds at
//! every call and never appears on screen. And a status bar sits idle almost
//! all the time, so it must not burn CPU doing nothing.
//!
//! So the runner blocks in `CFRunLoopRunInMode` with
//! `return_after_source_handled`, which sleeps in the kernel until a source
//! fires, and ticks the schedules once per wake. The run loop stays pumped, the
//! process is genuinely asleep between events, and the app is event-driven
//! rather than frame-driven — which is what a bar actually is.
//!
//! `examples/pump_spike.rs` in the `skylight` crate is the evidence.
//!
//! # Why there is no task pool
//!
//! `bevy_ecs` is taken without its `multi_threaded` feature, so
//! `default_executor()` is the single-threaded one and no task pool is built at
//! all. paneru measured the handoff at roughly 45% of main-thread time against
//! 16% of real work, and found that even an empty schedule costs a scope per
//! frame. Most systems here touch `NonSend` platform objects pinned to this
//! thread regardless, so there is nothing to overlap.

#![allow(
    clippy::needless_pass_by_value,
    reason = "Bevy system parameters are taken by value by contract"
)]

use crate::bar::{Panels, Settings};
use crate::components::{
    AliasContent, AliasSpec, AssociatedSpace, ClickScript, Icon, Index, Item, ItemHandle, Label,
    Name, Routine, Script, Selected, Stale, Subscriptions, Updates,
};
use crate::config::Shared as SharedConfig;
use crate::layout::{self, ForceRepaint, Hit, Placements};
use crate::requests::{Context, Items, ItemsRead};
use crate::script::{Job, Runner};
use crate::shaping::Cache;
use crate::sources::{Registry, Target};
use bevy_app::{App, First, Last, PostUpdate, PreUpdate, Update};
use bevy_ecs::prelude::*;
use objc2_core_foundation::{CFRunLoop, CFRunLoopRunResult, kCFRunLoopDefaultMode};
use rsbar_protocol::event::{MouseEnter, MouseExit, SpaceChange};
use rsbar_protocol::{Event, Kind, Request};
use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::time::Duration;

/// How many further waiting sources a wake will take before running a pass.
///
/// A burst of client requests should cost one repaint rather than one each,
/// and the bound is what stops a source that never stops firing from starving
/// the pass entirely.
const COALESCE_LIMIT: usize = 64;

/// How long the runner will sleep with nothing to do — also the routine tick.
/// Item update frequencies are whole seconds, so waking more often would only
/// cost battery to do nothing.
const TICK: Duration = Duration::from_secs(1);

/// A request that arrived over the Mach port, with wherever its answer goes.
///
/// Deliberately not a `Message`: a `Reply` is a send-once right and answering
/// consumes it, but messages are read by reference and may have many readers.
/// A request has exactly one consumer, so the bus would cost the ability to
/// answer and buy nothing.
pub struct IpcRequest {
    pub request: Box<Request>,
    pub reply: Option<async_mach_ports::Reply>,
    /// A port the client attached, wanting its events pushed back rather than
    /// turned into a forked script.
    pub subscriber: Option<async_mach_ports::Subscriber>,
}

/// An event, from a source or from this thread.
///
/// This one *is* a message: an event legitimately has several readers, and a
/// reader missing a frame beats acting on a backlog.
#[derive(Message)]
pub struct EventMessage(pub std::sync::Arc<Event>);

/// The request queue, which is `!Sync` and so cannot be a plain resource.
///
/// Events do not come through here: each source owns its own feed and the
/// registry drains them, so an emission arrives tagged with what produced it.
pub struct Inbox {
    pub requests: std::sync::mpsc::Receiver<IpcRequest>,
}

pub struct Sources(pub Registry);

/// Scripts to run, queued by whichever system produced them and drained once.
#[derive(Resource, Default)]
pub struct Queue(pub Vec<Job>);

#[derive(Resource)]
pub struct Scripts(pub Runner);

/// The config, shared with the watcher thread.
#[derive(Resource)]
pub struct ConfigHandle(pub SharedConfig);

/// The bootstrap name, so a reloaded config script can find us.
#[derive(Resource)]
pub struct Service(pub String);

/// Set when the item world should be torn down and the config re-run.
#[derive(Resource, Default)]
pub struct Reloading(pub bool);

/// Builds the app.
pub fn build(
    inbox: Inbox,
    settings: Settings,
    panels: Panels,
    registry: Registry,
    scripts: Runner,
    config: SharedConfig,
    service: String,
) -> App {
    let config_exists = config.blocking_read().path.is_some();
    let mut app = App::new();

    app.add_plugins(bevy_time::TimePlugin)
        // `add_message`, not `init_resource`: the latter never registers the
        // buffer with the message registry, so it is never double-buffered and
        // grows for the life of the process instead.
        .add_message::<EventMessage>()
        .insert_non_send(inbox)
        .insert_non_send(panels)
        .insert_non_send(Sources(registry))
        .insert_non_send(Cache::default())
        .insert_non_send(crate::alias::Captures::default())
        .insert_non_send(crate::subscribers::Subscribers::default())
        .insert_resource(settings)
        .init_resource::<Index>()
        .init_resource::<Queue>()
        .init_resource::<ActiveSpaces>()
        .init_resource::<ReloadWatch>()
        .init_resource::<ForceRepaint>()
        .init_resource::<Placements>()
        .init_resource::<crate::requests::Defaults>()
        .init_resource::<crate::popup::PopupPlacements>()
        .init_non_send::<crate::popup::Popups>()
        // Starts true, so the first tick runs the config. The bar comes up
        // empty and fills in a moment later, which is what a config run is.
        .insert_resource(Reloading(config_exists))
        .insert_resource(ConfigHandle(config))
        .insert_resource(Service(service))
        .insert_resource(Scripts(scripts));

    // Explicit ordering rather than DAG tie-breaks, which reorder silently when
    // a system is added.
    app.add_systems(First, drain_events)
        .add_systems(
            PreUpdate,
            (apply_requests, crate::requests::apply_defaults).chain(),
        )
        .add_systems(
            Update,
            (
                route_pointer,
                track_hover,
                dispatch_events,
                update_space_selection,
                tick,
                run_queued,
                reload_config,
                settle_reload,
            )
                .chain(),
        )
        .add_systems(PostUpdate, (rebuild_panels, reshape))
        .add_systems(
            Last,
            (
                settle_sources,
                refresh_aliases,
                layout::repaint.run_if(layout::needs_repaint),
                // Its own gate, and its own retained placements: a popup
                // opening must not repaint the bar, and a change inside one
                // must not either.
                crate::popup::repaint_popups.run_if(crate::popup::needs_repaint_popups),
                track_hover_rects,
                layout::clear_force_repaint,
            )
                .chain(),
        );

    app
}

/// Runs until something asks the app to exit.
pub fn run(mut app: App) -> bevy_app::AppExit {
    app.finish();
    app.cleanup();

    loop {
        // Sleeps in the kernel until a run loop source fires, or the tick
        // elapses. This is both the pump the window server needs and the reason
        // the process costs nothing at idle.
        let woke =
            unsafe { CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, TICK.as_secs_f64(), true) };
        // `Stopped` means someone called `CFRunLoopStop`; `Finished` means the
        // loop had no sources left. Either way there is nothing left to pump.
        if matches!(
            woke,
            CFRunLoopRunResult::Stopped | CFRunLoopRunResult::Finished
        ) {
            return bevy_app::AppExit::Success;
        }

        // Take everything else already waiting before doing a pass. The sleep
        // above returns on the *first* source it handles, so without this a
        // client sending fifty updates gets fifty passes and fifty repaints,
        // each redrawing one item — sixty window server round trips a second
        // where one would do. Bounded, so a source firing continuously cannot
        // hold the pass off forever.
        for _ in 0..COALESCE_LIMIT {
            let more = unsafe { CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, 0.0, true) };
            if !matches!(more, CFRunLoopRunResult::HandledSource) {
                break;
            }
        }

        // The run loop does not serve AppKit's or Carbon's event queues, and a
        // click on the bar arrives through them. Non-blocking: the sleep
        // already happened above, so this takes what is there and returns.
        crate::runloop::pump_platform_events();

        app.update();

        if let Some(exit) = app.should_exit() {
            return exit;
        }
    }
}

/// Reacts to a display appearing, disappearing or moving.
///
/// The panels' geometry is derived from the display layout, so it has to be
/// rebuilt before anything draws into them again.
fn rebuild_panels(
    mut events: MessageReader<EventMessage>,
    mut panels: NonSendMut<Panels>,
    settings: Res<Settings>,
    mut repaint: ResMut<ForceRepaint>,
) {
    if !events
        .read()
        .any(|EventMessage(event)| Kind::DisplayChanged.matches(event))
    {
        return;
    }
    if let Err(err) = panels.rebuild(&settings) {
        tracing::error!(%err, "could not rebuild the bar after a display change");
    }
    // Nothing about the items moved, but their panels did.
    repaint.0 = true;
}

/// Takes everything queued across every running source.
/// Everything an item owns outside the ECS, which has to be released when it
/// is despawned.
///
/// Grouped because forgetting one of them is the mistake: each is a map keyed
/// by entity, and one left unpruned strands a shaped line, a captured image or
/// a Mach port for the life of the process.
#[derive(bevy_ecs::system::SystemParam)]
pub struct Belongings<'w> {
    cache: NonSendMut<'w, Cache>,
    captures: NonSendMut<'w, crate::alias::Captures>,
    subscribers: NonSendMut<'w, crate::subscribers::Subscribers>,
}

/// Starts and stops sources to match the claims items are holding.
///
/// Separate from taking and dropping a claim because those happen anywhere —
/// a `Watch` is dropped by a despawn, on whatever thread the ECS pleases —
/// while registering and deregistering have to happen here, on the main
/// thread, with the run loop current.
fn settle_sources(mut sources: NonSendMut<Sources>) {
    sources.0.settle();
}

/// Re-captures every alias item, and says so only when one actually changed.
///
/// `SketchyBar` redraws an alias whether or not it moved. Comparing what came
/// back against what was drawn means a menu bar clock that changes once a
/// minute costs one repaint a minute rather than one a second — and because
/// the digest lands on a component, the damage tracker repaints just that
/// item's rect.
fn refresh_aliases(
    mut captures: NonSendMut<crate::alias::Captures>,
    mut items: Query<(Entity, &AliasSpec, &mut AliasContent)>,
) {
    // No throttle, because there is nothing to throttle: a capture only
    // happens when the owning application's Accessibility notification says
    // this item changed, or the first time it is seen. Asking the window
    // server for a fresh image and hashing every pixel of it on a timer, to
    // learn that a static icon is still static, was the largest single cost
    // in this process.
    for (entity, spec, mut content) in &mut items {
        if let Some(digest) = captures.refresh(entity, &spec.0) {
            content.set_if_neq(AliasContent(digest));
        }
    }
}

fn drain_events(mut sources: NonSendMut<Sources>, mut out: MessageWriter<EventMessage>) {
    sources.0.drain(|id, event| {
        tracing::trace!(source = %id, kind = %event.kind(), "drained");
        // Shared from here on. Everything downstream — the reactions, the
        // routing, the job handed to a worker — takes a reference count
        // rather than a copy of the payload.
        out.write(EventMessage(std::sync::Arc::new(event)));
    });
}

/// Drains the request queue and applies each one.
///
/// Drains rather than taking one: run loop wakes coalesce, so a burst can
/// arrive between two ticks — a config run is hundreds of sets.
#[allow(clippy::too_many_arguments, reason = "a request can touch all of it")]
fn apply_requests(
    inbox: NonSend<Inbox>,
    mut items: Items,
    mut settings: ResMut<Settings>,
    mut panels: NonSendMut<Panels>,
    mut cache: NonSendMut<Cache>,
    mut captures: NonSendMut<crate::alias::Captures>,
    mut sources: NonSendMut<Sources>,
    mut subscribers: NonSendMut<crate::subscribers::Subscribers>,
    mut queue: ResMut<Queue>,
    mut reloading: ResMut<Reloading>,
    mut defaults: ResMut<crate::requests::Defaults>,
    mut exit: MessageWriter<bevy_app::AppExit>,
) {
    while let Ok(IpcRequest {
        request,
        reply,
        subscriber,
    }) = inbox.requests.try_recv()
    {
        // Only a bar request may mark the settings changed. Handing out
        // `&mut settings` is a `deref_mut`, which marks them whatever the
        // request turns out to be — and a changed `Settings` means a whole-bar
        // repaint, so setting one item's label was repainting every item on
        // every display.
        let touches_bar = matches!(*request, Request::SetBar(_));
        let settings: &mut Settings = if touches_bar {
            &mut settings
        } else {
            settings.bypass_change_detection()
        };
        let mut ctx = Context {
            settings,
            panels: &mut panels,
            cache: &mut cache,
            captures: &mut captures,
            sources: &mut sources.0,
            subscribers: &mut subscribers,
            subscriber,
            defaults: &mut defaults,
        };
        let outcome = crate::requests::apply(*request, &mut items, &mut ctx);
        queue.0.extend(outcome.jobs);
        if outcome.reload {
            reloading.0 = true;
        }
        if outcome.exit {
            exit.write(bevy_app::AppExit::Success);
        }
        if let Some(reply) = reply
            && let Err(err) = reply.send(&outcome.response)
        {
            tracing::debug!(%err, "client stopped waiting for its answer");
        }
    }
}

/// The space each display is currently showing, by `CGDirectDisplayID`.
///
/// What real `SketchyBar` reads off `bar->sid` per bar in
/// `bar_manager_update_space_components` — kept here instead because this
/// daemon has no standing per-display bar state to hang it on.
#[derive(Resource, Default)]
pub struct ActiveSpaces(BTreeMap<u32, NonZeroU64>);

/// Recomputes every space item's [`Selected`] after one `space_changed`
/// event — `bar_manager_update_space_components` in `SketchyBar`'s
/// `bar_item.c`: a space is selected if some display is currently showing it.
///
/// Checked and written through [`Mut::set_if_neq`] rather than
/// `Mut::as_mut`, and the whole scan is skipped unless the event actually
/// moved a display's space — a config with a dozen space items must not have
/// all twelve mark themselves changed just because one space elsewhere came
/// and went.
pub(crate) fn recompute_space_selection(
    event: &Event,
    active: &mut ActiveSpaces,
    spaces: &mut Query<(&AssociatedSpace, &mut Selected)>,
) {
    let Event::SpaceChanged(SpaceChange { display, space }) = event else {
        return;
    };
    let Some(space) = NonZeroU64::new(*space) else {
        return;
    };
    if active.0.insert(*display, space) == Some(space) {
        return;
    }
    for (associated, mut selected) in spaces.iter_mut() {
        let is_selected = associated
            .0
            .is_some_and(|mine| active.0.values().any(|&shown| shown == mine));
        selected.set_if_neq(Selected(is_selected));
    }
}

fn update_space_selection(
    mut events: MessageReader<EventMessage>,
    mut active: ResMut<ActiveSpaces>,
    mut spaces: Query<(&AssociatedSpace, &mut Selected)>,
) {
    for EventMessage(event) in events.read() {
        recompute_space_selection(event, &mut active, &mut spaces);
    }
}

fn dispatch_events(
    mut events: MessageReader<EventMessage>,
    sources: NonSend<Sources>,
    read: ItemsRead,
    mut queue: ResMut<Queue>,
    mut reloading: ResMut<Reloading>,
    mut subscribers: NonSendMut<crate::subscribers::Subscribers>,
    // Kept between passes: the dependent list is rebuilt per event and would
    // otherwise be an allocation each time.
    mut dependents: Local<Vec<Entity>>,
) {
    for EventMessage(event) in events.read() {
        tracing::debug!(kind = %event.kind(), "event");
        if Kind::ConfigReloaded.matches(event) {
            reloading.0 = true;
        }
        // Pushed to whatever holds a claim on it, rather than matched against
        // every item's subscriptions. Dispatched as well as acted on, so a
        // config can subscribe to its own reload like anything else.
        sources
            .0
            .dependents_into(event, &Target::All, &mut dependents);
        // A client holding a port takes the event itself. It does not also
        // get its script run, or a Lua config would fork a shell for every
        // event it is already handling in process.
        dependents.retain(|item| !subscribers.push(*item, event));
        read.push_jobs(event, &dependents, &mut queue.0);
    }
}

/// Starts a reload: marks every item stale, then runs the config.
///
/// Nothing is despawned here. The config might fail, and a broken edit should
/// change nothing rather than leave an empty bar; and re-adding an item that
/// already exists updates it in place, so entity identity survives a reload and
/// change detection sees only what actually changed.
fn reload_config(
    mut reloading: ResMut<Reloading>,
    mut commands: Commands,
    items: Query<Entity, With<Item>>,
    config: Res<ConfigHandle>,
    service: Res<Service>,
    mut settle: ResMut<ReloadWatch>,
) {
    if !reloading.0 {
        return;
    }
    reloading.0 = false;

    for entity in &items {
        commands.entity(entity).insert(Stale);
    }
    settle.pending = true;
    crate::sources::config::reload(&config.0, service.0.clone());
}

/// Tracks a reload that is in flight.
#[derive(Resource, Default)]
pub struct ReloadWatch {
    pending: bool,
    seen_generation: u64,
}

/// Finishes a reload once the config thread has stopped.
///
/// On success, whatever is still marked stale is what the new config dropped,
/// so it goes. On failure nothing goes — the mark is simply lifted and the
/// previous bar stands.
fn settle_reload(
    mut watch: ResMut<ReloadWatch>,
    mut commands: Commands,
    mut index: ResMut<Index>,
    mut belongings: Belongings,
    stale: Query<(Entity, &Name), With<Stale>>,
    config: Res<ConfigHandle>,
) {
    let Belongings {
        cache,
        captures,
        subscribers,
    } = &mut belongings;
    if !watch.pending {
        return;
    }
    let guard = config.0.blocking_read();
    if guard.running {
        return;
    }
    let succeeded = guard.generation > watch.seen_generation;
    watch.seen_generation = guard.generation;
    drop(guard);
    watch.pending = false;

    for (entity, name) in &stale {
        if succeeded {
            cache.forget(entity);
            captures.forget(entity);
            subscribers.clear(entity);
            index.remove(&name.0);
            // The item's claims are components, so the despawn releases them
            // and a source nothing wants any more stops on the next settle.
            commands.entity(entity).despawn();
        } else {
            commands.entity(entity).remove::<Stale>();
        }
    }
}

/// Sends a pointer event to whatever is under it.
///
/// Runs before the general dispatch, and the two do not overlap: a pointer
/// event is deliberately *not* broadcast to every subscriber, because a click
/// belongs to one item. What is under the cursor comes from the retained
/// layout, so the answer matches what is actually drawn.
fn route_pointer(
    mut events: MessageReader<EventMessage>,
    placements: Res<Placements>,
    read: ItemsRead,
    mirrors: Query<(Option<&AliasSpec>, Option<&ClickScript>)>,
    mut subscribers: NonSendMut<crate::subscribers::Subscribers>,
    mut queue: ResMut<Queue>,
) {
    for EventMessage(event) in events.read() {
        let Some((x, y)) = pointer_location(event) else {
            continue;
        };
        match placements.hit(objc2_core_foundation::CGPoint::new(x, y)) {
            Hit::Item { entity, .. } => {
                if matches!(**event, Event::MouseClicked(_)) {
                    press_mirrored(entity, &mirrors);
                }
                if !subscribers.push(entity, event) {
                    queue.0.extend(read.jobs_for_item(entity, event));
                }
            }
            // On the bar but not on an item. The `.global` events exist for
            // exactly this, and are matched the ordinary way.
            Hit::Bar { .. } | Hit::Nothing => {}
        }
    }
}

/// Tells an item when the cursor enters or leaves it.
///
/// Hover has no queued event to route the way a click does. `sources::mouse`
/// only records *where* a tracking rect was last crossed — it has no layout
/// to say which item that was, and (see its own module doc) the boundary a
/// window server tracking rect fires on is a Carbon-level `kEventMouseEntered`
/// /`kEventMouseExited`, not a per-move event this could instead react to. So
/// this hit-tests that point against the retained layout itself, the same
/// layout [`route_pointer`] answers a click against, and only acts when the
/// hit entity actually changed since the last tick — a cursor crossing the
/// bar must cost one script run per item boundary crossed, never one per
/// crossing Carbon happens to report.
/// Tells the window server which rectangles to report the cursor crossing.
///
/// One per item that actually subscribes to hover, re-registered whenever the
/// layout moves. It has to be per item rather than one rect for the whole bar:
/// crossing from one hoverable item straight to its neighbour never leaves the
/// bar's own bounds, so a single rect would report neither. There is no call
/// to remove one rectangle, so a change means clearing the window's and adding
/// back what still applies.
fn track_hover_rects(
    placements: Res<Placements>,
    panels: NonSend<Panels>,
    hoverable: Query<&Subscriptions>,
    resubscribed: Query<(), Changed<Subscriptions>>,
) {
    // Subscribing does not move anything, so watching the layout alone would
    // never register a rect for an item that asks for hover after it was
    // already placed -- which is the order every config does it in.
    if !placements.is_changed() && resubscribed.is_empty() {
        return;
    }
    for placed in placements.panels() {
        let Some(panel) = panels.iter().find(|p| p.display.id == placed.display) else {
            continue;
        };
        if let Err(err) = panel.window.clear_tracking_rects() {
            tracing::debug!(%err, "could not clear the hover rectangles");
            continue;
        }
        for (entity, rect) in &placed.items {
            let wanted = hoverable.get(*entity).is_ok_and(|subs| {
                subs.0.contains(&Kind::MouseEntered(())) || subs.0.contains(&Kind::MouseExited(()))
            });
            if !wanted {
                continue;
            }
            match panel.window.add_tracking_rect(*rect) {
                Ok(()) => tracing::debug!(?entity, ?rect, "tracking an item for hover"),
                Err(err) => tracing::debug!(%err, "could not track an item for hover"),
            }
        }
    }
}

fn track_hover(
    placements: Res<Placements>,
    read: ItemsRead,
    mut subscribers: NonSendMut<crate::subscribers::Subscribers>,
    mut queue: ResMut<Queue>,
    mut hovered: Local<Option<Entity>>,
) {
    let hit =
        crate::sources::mouse::current_position().and_then(|point| match placements.hit(point) {
            Hit::Item { entity, .. } => Some(entity),
            Hit::Bar { .. } | Hit::Nothing => None,
        });
    if hit == *hovered {
        return;
    }
    if let Some(left) = hovered.take() {
        deliver_hover(
            left,
            Event::MouseExited(MouseExit {}),
            &read,
            &mut subscribers,
            &mut queue,
        );
    }
    if let Some(entered) = hit {
        deliver_hover(
            entered,
            Event::MouseEntered(MouseEnter {}),
            &read,
            &mut subscribers,
            &mut queue,
        );
    }
    *hovered = hit;
}

/// One item's hover event, through whichever of a subscriber or a script it
/// actually goes to — the same choice [`route_pointer`] makes for a click.
fn deliver_hover(
    entity: Entity,
    event: Event,
    read: &ItemsRead,
    subscribers: &mut crate::subscribers::Subscribers,
    queue: &mut Queue,
) {
    let event = std::sync::Arc::new(event);
    if !subscribers.push(entity, &event) {
        queue.0.extend(read.jobs_for_item(entity, &event));
    }
}

/// Opens the real menu behind a clicked alias.
///
/// An alias is a picture of someone else's menu bar item, so clicking one and
/// having nothing happen is the obvious wrong behaviour. A config that set its
/// own `click_script` means something more specific by the click than "do what
/// the real one does", so that wins and this stays out of the way.
fn press_mirrored(entity: Entity, mirrors: &Query<(Option<&AliasSpec>, Option<&ClickScript>)>) {
    let Ok((Some(alias), None)) = mirrors.get(entity) else {
        return;
    };
    let (owner, name) = alias.0.split_once(',').unwrap_or((&alias.0, &alias.0));
    if let Err(err) = crate::alias::press_item(owner, name) {
        tracing::warn!(spec = %alias.0, %err, "could not press the mirrored item");
    }
}

/// Where a pointer event happened, if it is one.
fn pointer_location(event: &Event) -> Option<(f64, f64)> {
    match event {
        Event::MouseClicked(click) => Some((click.x, click.y)),
        Event::MouseScrolled(scroll) => Some((scroll.x, scroll.y)),
        _ => None,
    }
}

/// Advances every item's routine clock, once per elapsed second.
///
/// Driven by accumulated time rather than by wakes: the runner wakes whenever
/// anything happens, so a per-wake tick would run fast under load and slow when
/// idle — exactly backwards.
///
/// `Time<Real>` specifically, not the default virtual clock. The virtual one
/// clamps delta to 250 ms so a stalled frame cannot make a game jump, which
/// here would quietly stretch a one-second update frequency to four. Wall clock
/// is what an item asking for `--update-freq 10` means.
/// What advancing a second needs to know about one item.
#[derive(bevy_ecs::query::QueryData)]
#[query_data(mutable)]
struct Ticking {
    entity: Entity,
    name: &'static Name,
    clock: &'static mut Routine,
    script: Option<&'static Script>,
    updates: Option<&'static Updates>,
}

fn tick(
    time: Res<bevy_time::Time<bevy_time::Real>>,
    mut since: Local<Duration>,
    mut items: Query<Ticking>,
    mut subscribers: NonSendMut<crate::subscribers::Subscribers>,
    mut queue: ResMut<Queue>,
) {
    *since += time.delta();
    if *since < TICK {
        return;
    }
    // Subtract rather than zero, so a late wake does not lose the remainder.
    *since -= TICK;

    let routine = std::sync::Arc::new(Event::Routine(rsbar_protocol::event::Routine {}));
    for mut row in &mut items {
        // `bypass_change_detection`, because a routine clock ticking is not a
        // reason to repaint — only what the script then sets is.
        if !row.clock.bypass_change_detection().tick() {
            continue;
        }
        // The clock still advances while updates are off, so turning them back
        // on resumes the item's own rhythm rather than restarting it.
        if row.updates.is_some_and(|updates| !updates.0) {
            continue;
        }
        // A client holding a port takes the tick itself. Checked before the
        // script, and before asking whether there *is* one: an item that only
        // exists to be updated by a Lua callback has no script at all, and
        // gating on one meant its tick went nowhere.
        if subscribers.push(row.entity, &routine) {
            continue;
        }
        if let Some(script) = row.script {
            queue.0.push(Job {
                item: ItemHandle::new(row.entity, row.name.0.clone()),
                script: std::sync::Arc::clone(&script.0),
                event: std::sync::Arc::clone(&routine),
            });
        }
    }
}

/// Hands queued scripts to the worker pool. One place, so nothing can run a
/// script twice by queueing it in two systems.
fn run_queued(mut queue: ResMut<Queue>, scripts: Res<Scripts>) {
    for job in queue.0.drain(..) {
        scripts.0.run(job);
    }
}

/// The items whose shaped text is out of date.
type Reshaped<'w, 's> =
    Query<'w, 's, (Entity, &'static Icon, &'static Label), Or<(Changed<Icon>, Changed<Label>)>>;

/// Rebuilds the shaped text of any item whose text or font moved.
///
/// This is the cache's whole invalidation strategy: `Changed` says which items
/// need it, so it cannot go stale without someone having changed something.
fn reshape(changed: Reshaped, mut cache: NonSendMut<Cache>) {
    for (entity, icon, label) in &changed {
        cache.refresh(entity, &icon.0, &label.0);
    }
}

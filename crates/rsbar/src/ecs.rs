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
use crate::components::{Icon, Index, Item, Label, Name, Routine, Script, Stale};
use crate::config::Shared as SharedConfig;
use crate::layout::{self, ForceRepaint, Hit, Placements};
use crate::requests::{Context, Items, ItemsRead};
use crate::script::{Job, Runner};
use crate::shaping::Cache;
use crate::sources::Registry;
use bevy_app::{App, First, Last, PostUpdate, PreUpdate, Update};
use bevy_ecs::prelude::*;
use objc2_core_foundation::{CFRunLoop, CFRunLoopRunResult, kCFRunLoopDefaultMode};
use rsbar_protocol::{Event, Kind, Request};
use std::time::Duration;

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
}

/// An event, from a source or from this thread.
///
/// This one *is* a message: an event legitimately has several readers, and a
/// reader missing a frame beats acting on a backlog.
#[derive(Message)]
pub struct EventMessage(pub Event);

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
        .insert_resource(settings)
        .init_resource::<Index>()
        .init_resource::<Queue>()
        .init_resource::<ReloadWatch>()
        .init_resource::<ForceRepaint>()
        .init_resource::<Placements>()
        // Starts true, so the first tick runs the config. The bar comes up
        // empty and fills in a moment later, which is what a config run is.
        .insert_resource(Reloading(config_exists))
        .insert_resource(ConfigHandle(config))
        .insert_resource(Service(service))
        .insert_resource(Scripts(scripts));

    // Explicit ordering rather than DAG tie-breaks, which reorder silently when
    // a system is added.
    app.add_systems(First, drain_events)
        .add_systems(PreUpdate, apply_requests)
        .add_systems(
            Update,
            (
                route_pointer,
                dispatch_events,
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
                layout::repaint.run_if(layout::needs_repaint),
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
fn drain_events(mut sources: NonSendMut<Sources>, mut out: MessageWriter<EventMessage>) {
    for (id, event) in sources.0.drain() {
        tracing::trace!(source = %id, kind = %event.kind(), "drained");
        out.write(EventMessage(event));
    }
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
    mut sources: NonSendMut<Sources>,
    mut queue: ResMut<Queue>,
    mut reloading: ResMut<Reloading>,
    mut exit: MessageWriter<bevy_app::AppExit>,
) {
    while let Ok(IpcRequest { request, reply }) = inbox.requests.try_recv() {
        let mut ctx = Context {
            settings: &mut settings,
            panels: &mut panels,
            cache: &mut cache,
            sources: &mut sources.0,
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

fn dispatch_events(
    mut events: MessageReader<EventMessage>,
    read: ItemsRead,
    mut queue: ResMut<Queue>,
    mut reloading: ResMut<Reloading>,
) {
    for EventMessage(event) in events.read() {
        tracing::debug!(kind = %event.kind(), "event");
        if Kind::ConfigReloaded.matches(event) {
            reloading.0 = true;
        }
        // Dispatched as well as acted on, so a config can subscribe to its own
        // reload the same way it subscribes to anything else.
        queue.0.extend(read.jobs_for(event));
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
    mut cache: NonSendMut<Cache>,
    mut sources: NonSendMut<Sources>,
    stale: Query<(Entity, &Name), With<Stale>>,
    config: Res<ConfigHandle>,
) {
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
            index.remove(&name.0);
            // A config that dropped the only item watching the volume stops
            // the audio listener with it.
            sources.0.release(entity);
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
    mut queue: ResMut<Queue>,
) {
    for EventMessage(event) in events.read() {
        let Some((x, y)) = pointer_location(event) else {
            continue;
        };
        match placements.hit(objc2_core_foundation::CGPoint::new(x, y)) {
            Hit::Item { entity, .. } => {
                queue.0.extend(read.jobs_for_item(entity, event));
            }
            // On the bar but not on an item. The `.global` events exist for
            // exactly this, and are matched the ordinary way.
            Hit::Bar { .. } | Hit::Nothing => {}
        }
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
fn tick(
    time: Res<bevy_time::Time<bevy_time::Real>>,
    mut since: Local<Duration>,
    mut items: Query<(&Name, &mut Routine, Option<&Script>)>,
    mut queue: ResMut<Queue>,
) {
    *since += time.delta();
    if *since < TICK {
        return;
    }
    // Subtract rather than zero, so a late wake does not lose the remainder.
    *since -= TICK;

    for (name, mut routine, script) in &mut items {
        // `bypass_change_detection`, because a routine clock ticking is not a
        // reason to repaint — only what the script then sets is.
        if routine.bypass_change_detection().tick()
            && let Some(script) = script
        {
            queue.0.push(Job {
                item: name.0.clone(),
                script: script.0.clone(),
                event: Event::Routine(rsbar_protocol::event::Routine {}),
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

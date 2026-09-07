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
//! `examples/pump_spike.rs` in the `skylight` crate is the evidence: still
//! composited a minute later, 0.0% CPU, and twenty seconds of Time Profiler
//! sampling caught it on-CPU zero times.
//!
//! # Why there is no task pool
//!
//! `bevy_ecs` is taken without its `multi_threaded` feature, so
//! `default_executor()` is the single-threaded one and no task pool is built at
//! all. That is deliberate: paneru measured the task-pool handoff at roughly
//! 45% of main-thread time against 16% of real work, and found that even an
//! empty schedule costs a scope per frame. This app is smaller still, and most
//! of its systems touch `NonSend` platform objects pinned to this thread
//! regardless — there is nothing to overlap.

#![allow(
    clippy::needless_pass_by_value,
    reason = "Bevy system parameters are taken by value by contract"
)]

use crate::bar::Bar;
use crate::script::Runner;
use crate::sources::{Emission, Events, Registry};
use bevy_app::{App, First, Last, PreUpdate, Update};
use bevy_ecs::prelude::*;
use objc2_core_foundation::{CFRunLoop, CFRunLoopRunResult, kCFRunLoopDefaultMode};
use rsbar_protocol::Request;
use std::time::Duration;

/// How long the runner will sleep with nothing to do.
///
/// This is the routine tick: item update frequencies are whole seconds, so
/// waking more often would only cost battery to do nothing.
const TICK: Duration = Duration::from_secs(1);

/// A request that arrived over the Mach port, with wherever its answer goes.
///
/// Deliberately *not* a `Message`. A `Reply` is a send-once right and consuming
/// it is what spends it, but messages are read by reference and may be read by
/// several systems. A request has exactly one consumer, so the bus would buy
/// nothing and cost the ability to answer.
pub struct IpcRequest {
    pub request: Box<Request>,
    pub reply: Option<async_mach_ports::Reply>,
}

/// An event, from a source or from this thread.
///
/// This one *is* a message: an event legitimately has several readers, and a
/// reader missing a frame is better than a backlog.
#[derive(Message)]
pub struct EventMessage(pub Emission);

/// The receiving ends, which are `!Sync` and so cannot be plain resources.
pub struct Inbox {
    pub requests: std::sync::mpsc::Receiver<IpcRequest>,
    pub events: Events,
}

/// The bar and its windows. `NonSend` because it owns window server handles.
pub struct BarState(pub Bar);

/// `NonSend` because a running source owns a handle to its thread's run loop,
/// which may only be touched from a thread that is allowed to stop it.
pub struct Sources(pub Registry);

#[derive(Resource)]
pub struct Scripts(pub Runner);

/// Builds the app. Split out so tests can drive the same schedules with the
/// platform resources left out.
pub fn build(inbox: Inbox, bar: Bar, registry: Registry, scripts: Runner) -> App {
    let mut app = App::new();

    app.add_plugins(bevy_time::TimePlugin)
        // `add_message`, not `init_resource`: the latter never registers the
        // buffer with the message registry, so it is never double-buffered and
        // grows for the life of the process instead.
        .add_message::<EventMessage>()
        .insert_non_send(inbox)
        .insert_non_send(BarState(bar))
        .insert_non_send(Sources(registry))
        .insert_resource(Scripts(scripts));

    // Explicit ordering rather than DAG tie-breaks, which reorder silently when
    // a system is added.
    app.add_systems(First, drain_events)
        .add_systems(PreUpdate, apply_requests)
        .add_systems(Update, (dispatch_events, tick))
        .add_systems(Last, redraw);

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

        app.update();

        if let Some(exit) = app.should_exit() {
            return exit;
        }
    }
}

fn drain_events(inbox: NonSend<Inbox>, mut out: MessageWriter<EventMessage>) {
    while let Ok(emission) = inbox.events.try_recv() {
        out.write(EventMessage(emission));
    }
}

/// Drains the request queue and applies each one.
///
/// Drains rather than taking one: run loop wakes coalesce, so a burst can
/// arrive between two ticks — a config run is hundreds of sets.
fn apply_requests(
    inbox: NonSend<Inbox>,
    mut bar: NonSendMut<BarState>,
    mut sources: NonSendMut<Sources>,
    scripts: Res<Scripts>,
    mut exit: MessageWriter<bevy_app::AppExit>,
) {
    while let Ok(IpcRequest { request, reply }) = inbox.requests.try_recv() {
        let (response, jobs) =
            crate::handle::apply(&mut bar.0, &mut sources.0, *request, &mut exit);
        for job in jobs {
            scripts.0.run(job);
        }
        if let Some(reply) = reply
            && let Err(err) = reply.send(&response)
        {
            tracing::debug!(%err, "client stopped waiting for its answer");
        }
    }
}

fn dispatch_events(
    mut events: MessageReader<EventMessage>,
    bar: NonSend<BarState>,
    scripts: Res<Scripts>,
) {
    for EventMessage(emission) in events.read() {
        tracing::debug!(event = %emission.event, info = ?emission.info, "event");
        for job in bar.0.jobs_for(&emission.event, emission.info.as_deref()) {
            scripts.0.run(job);
        }
    }
}

fn redraw(mut bar: NonSendMut<BarState>) {
    bar.0.redraw_if_dirty();
}

/// Advances every item's routine clock, once per elapsed second.
///
/// Driven by accumulated time rather than by wakes: the runner wakes whenever
/// anything happens, so a per-wake tick would run fast under load and slow when
/// idle — exactly backwards.
///
/// `Time<Real>` specifically, not the default virtual clock. The virtual one
/// clamps delta to 250 ms so a stalled frame cannot make a game jump, which
/// here would quietly stretch a one-second update frequency to four. Wall
/// clock is what an item asking for `--update-freq 10` means.
fn tick(
    time: Res<bevy_time::Time<bevy_time::Real>>,
    mut since: Local<Duration>,
    mut bar: NonSendMut<BarState>,
    scripts: Res<Scripts>,
) {
    *since += time.delta();
    if *since < TICK {
        return;
    }
    // Subtract rather than zero, so a late wake does not lose the remainder.
    *since -= TICK;

    for job in bar.0.tick() {
        scripts.0.run(job);
    }
}

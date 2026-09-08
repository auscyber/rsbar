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
//! So the app is not a loop at all: it is parked in Carbon's event loop, and
//! a pass is something the run loop *calls* — from a source a worker
//! signalled, from a request the service port delivered, or from a deadline
//! an item asked for. The run loop stays pumped, the process is genuinely
//! asleep between those, and the app is event-driven rather than
//! frame-driven — which is what a bar actually is.
//!
//! **Nothing here ticks.** There is no periodic wake anywhere in this
//! process: a bar showing one static block of colour arms no timer and
//! signals no source, and `runloop::armed()` is how that is asserted rather
//! than hoped for. An item asking for `--update-freq 60` is one deadline at
//! the next minute boundary, and it is the only reason such a bar wakes.
//!
//! `examples/pump_spike.rs` in the `skylight` crate is the evidence.
//!
//! # The schedule has no task pool
//!
//! `bevy_ecs` is taken without its `multi_threaded` feature, so
//! `default_executor()` is the single-threaded one and no task pool is built at
//! all. paneru measured the handoff at roughly 45% of main-thread time against
//! 16% of real work, and found that even an empty schedule costs a scope per
//! frame. Most systems here touch `NonSend` platform objects pinned to this
//! thread regardless, so there is nothing to overlap.
//!
//! # Where the asynchronous work actually is
//!
//! None of it is in the schedule, which is why it is easy to look for it here
//! and not find it. Every system in this module runs on the main thread, start
//! to finish; the work that does not belong there is owned by resources this
//! module inserts, and reaches a system as data on a later pass.
//!
//! | pool | owner | inserted by | how a result gets back |
//! |---|---|---|---|
//! | `rsbar-ax` ×16 | [`crate::alias::Captures`]'s scanner | [`build`] | wakes [`waker`], read on the next pass |
//! | `rsbar-capture` ×4 | [`crate::alias::Captures`]'s captor | [`build`] | the same |
//! | `rsbar-script-N` | [`Scripts`] | [`build`], from `main` | a finished job wakes [`waker`] |
//! | `rsbar-subscriber-N` | [`crate::subscribers::Subscribers`] | on subscribe | nothing comes back; it only sends |
//! | the local executor | `runloop`'s thread-local | on first [`crate::runloop::spawn`] | polled *on this thread* by a run loop source |
//!
//! The last one is the one worth being careful about: it is not a pool and it
//! is not off-thread. `runloop::spawn` puts a task on a `LocalSet` that a run
//! loop source drives on the main thread, so a task there can hold a
//! `CGImage` or an `Rc` — and equally, a task there that blocks stops the bar.
//! Work that should not be on this thread goes to one of the pools above
//! instead, and comes back as data.
//!
//! The crate root states the rule the whole arrangement follows; this is where
//! the pieces of it are.

#![allow(
    clippy::needless_pass_by_value,
    reason = "Bevy system parameters are taken by value by contract"
)]

use crate::alias::{Due, Look};
use crate::bar::{Panels, Settings};
use crate::components::{
    AliasContent, AliasSpec, AssociatedSpace, ClickScript, Drawing, Icon, Index, Item, ItemHandle,
    Label, Name, Routine, Script, Selected, Stale, Updates,
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
use rsbar_protocol::{Event, Kind, PressTarget, Query as ProtocolQuery, Request, Response};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::time::{Duration, Instant};

/// How many further waiting sources a wake will take before running a pass.
///
/// A burst of client requests should cost one repaint rather than one each,
/// and the bound is what stops a source that never stops firing from starving
/// the pass entirely.
const COALESCE_LIMIT: usize = 64;

/// One present per display frame, with everything that changed in between
/// drawn once — and a frame that could not be had *dropped*, never queued.
///
/// # Two kinds of work, two opposite rules
///
/// These look contradictory side by side, so the reason is worth writing
/// down rather than inferring.
///
/// **An event is never dropped.** It carries information that exists nowhere
/// else — a click at a position, a script's output, a request's fields — so
/// losing one loses the only copy. Events queue, and a wake stands until
/// something takes them.
///
/// **A frame is dropped on purpose.** It carries no information at all: a
/// present draws whatever the world says *now*, so a present one interval
/// late and a present five intervals late put the same pixels on the screen.
/// There is nothing to catch up on, which makes catching up pure cost — and
/// worse than that, a burst of catch-up presents is what a stutter *is*. So
/// a budget that has fallen behind, because a pass ran long or the machine
/// slept or a Carbon modal held the loop, presents **once** and re-phases
/// from that present. Never five presents for five missed frames.
///
/// # Why the world still updates in between
///
/// A script setting a label takes effect immediately, and only the pixels
/// wait. What makes that safe is that [`layout::repaint`] is *skipped* rather
/// than run and thrown away, so its `Changed` queries keep accumulating and
/// the draw at the boundary covers every item touched since the last one —
/// the damage tracker sees the union, not the last wake's slice.
///
/// The cap is global rather than per panel. Drawing is one system over every
/// panel and one ECS pass behind it, so a per-panel budget would mean
/// splitting the draw and carrying a damage set per display for no gain the
/// window server will show: the interval is the *fastest* attached display's,
/// so no panel is ever presented slower than it can refresh, and a slower
/// panel getting an occasional present it cannot show costs a blit the
/// compositor was going to drop anyway.
#[derive(Resource)]
pub struct FrameBudget {
    /// The fastest attached display's refresh interval.
    interval: Duration,
    /// When the last present was, or `None` before the first one — which is
    /// why the bar's first draw is not held back by a frame it was never
    /// going to fill.
    ///
    /// An instant rather than a countdown, so there is no accumulator to fall
    /// behind and no backlog to work through: a present sets this to *now*,
    /// and the next boundary is one interval after it whether the last frame
    /// was missed by a microsecond or by a minute.
    last_present: Option<Instant>,
    /// Sticky, because a run condition cannot write. A gate that said "yes"
    /// during a frame we did not draw has to still say so at the boundary —
    /// its own change detection will have moved on by then.
    bar: bool,
    popups: bool,
    presenting: bool,
    /// How long until the frame this pass could not have. [`arm_wake`] turns
    /// it into the one thing the ECS cannot do for itself.
    deferred: Option<Duration>,
}

impl FrameBudget {
    #[must_use]
    pub const fn new(interval: Duration) -> Self {
        Self {
            interval,
            last_present: None,
            bar: false,
            popups: false,
            presenting: false,
            deferred: None,
        }
    }

    /// Retunes to a new refresh rate — a display arriving or leaving.
    pub const fn set_interval(&mut self, interval: Duration) {
        self.interval = interval;
    }

    /// Records that the bar wants the screen, whether or not it gets it now.
    pub const fn wants_bar(&mut self) {
        self.bar = true;
    }

    pub const fn wants_popups(&mut self) {
        self.popups = true;
    }

    /// Decides whether this pass draws, given the moment it is running at.
    ///
    /// Handed `now` rather than a delta: a delta only means anything while
    /// something runs once per wake, and nothing does — a pass happens when a
    /// wake was asked for. Explicit also makes the gate drivable by hand,
    /// which is how `frame_tests` reaches it.
    pub fn open(&mut self, now: Instant) {
        if !(self.bar || self.popups) {
            self.presenting = false;
            self.deferred = None;
            return;
        }
        let waited = self
            .last_present
            .map(|at| now.saturating_duration_since(at));
        match waited {
            // Inside the frame the last present opened. The draw waits, and
            // `arm_wake` turns what is left into the wake that serves it.
            Some(waited) if waited < self.interval => {
                self.presenting = false;
                self.deferred = Some(self.interval.saturating_sub(waited));
            }
            // Due, or long overdue, or never presented — all one present.
            // Phasing from `now` rather than from the boundary that was
            // missed is what stops a late pass owing the screen a burst.
            _ => {
                self.presenting = true;
                self.deferred = None;
                self.last_present = Some(now);
            }
        }
    }

    /// Retires the wants a present just satisfied. A deferred pass keeps them.
    pub fn close(&mut self) {
        if !std::mem::take(&mut self.presenting) {
            return;
        }
        self.bar = false;
        self.popups = false;
    }

    #[must_use]
    pub const fn presenting(&self) -> bool {
        self.presenting
    }

    /// How long until the outstanding draw, if one is waiting on a frame.
    #[must_use]
    pub const fn deferred(&self) -> Option<Duration> {
        self.deferred
    }
}

/// The deadline that makes a deferred frame actually happen.
///
/// Kept out of [`FrameBudget`] deliberately: the policy is a plain resource a
/// test can drive with a clock it advances by hand, and this — the one part
/// that needs a live run loop — is the only thing that has to be `NonSend`.
#[derive(Default)]
pub struct Wake(Option<crate::runloop::Registration>);

/// Arms a wake for the frame boundary, or drops the one that has served.
///
/// Nothing is armed while nothing is pending, and nothing else in this process
/// arms anything either — so on an idle bar this is the difference between a
/// wake and no wake at all, rather than between a wake and a tick. The armed
/// wake is left alone while it stands: the boundary it targets does not move
/// until a present resets the frame clock. Losing one would now mean a frozen
/// bar rather than a briefly stale one, which is why the registration is held
/// rather than detached — dropping it is the only thing that cancels it.
fn arm_wake(budget: Res<FrameBudget>, mut wake: NonSendMut<Wake>, main: NonSend<Main>) {
    match budget.deferred() {
        Some(left) if wake.0.is_none() => {
            tracing::trace!(?left, "a draw is waiting on the frame boundary");
            wake.0 = Some(crate::runloop::wake_at(
                main.0,
                Instant::now() + left,
                pass_waker(main.0),
            ));
        }
        Some(_) => {}
        None => wake.0 = None,
    }
}

thread_local! {
    /// The one run loop source everything in this process wakes the app
    /// through.
    ///
    /// **One, and shared.** Signals coalesce inside CoreFoundation, so an IPC
    /// request, a finished script and a routine's deadline all landing in the
    /// same turn of the loop cost one pass and one repaint between them.
    /// Two sources would coalesce within each but not across, and the bar
    /// would draw twice for one moment's worth of change — which is what a
    /// clock updating unevenly looks like.
    ///
    /// Installed on the first ask and never removed: it is the thread's own
    /// handle on itself, and `pass` does nothing until an app is parked, so
    /// it is safe to hold before one exists.
    ///
    /// A cell rather than a lazy initialiser because installing needs proof of
    /// the main thread, and a `thread_local!` initialiser is handed nothing.
    /// The first caller with proof supplies it; every caller after that gets
    /// the same source.
    static PASS: std::cell::RefCell<Option<crate::runloop::Waker>> =
        const { std::cell::RefCell::new(None) };
}

/// The handle a worker thread, a source or a deadline wakes the app with.
///
/// Everything that wants a pass asks here rather than installing a source of
/// its own, which is what makes a turn of the run loop worth exactly one pass.
///
/// The proof is what makes the source land on the loop this daemon actually
/// turns; the returned [`crate::runloop::Waker`] is `Send`, so a worker takes
/// one from here and wakes with it from anywhere.
#[must_use]
#[skylight::main_thread]
pub fn waker() -> crate::runloop::Waker {
    PASS.with_borrow_mut(|slot| {
        slot.get_or_insert_with(|| crate::runloop::Waker::install(proof, pass))
            .clone()
    })
}

/// The process's main-thread proof, parked where a system can ask for it.
///
/// `NonSend` is not decoration: bevy will only run a system taking one on the
/// thread that owns the `World`, which here is the thread this marker is proof
/// of. So a system declares `main: NonSend<Main>` and has the proof, without
/// anything checking a thread — the schedule already guaranteed it.
pub struct Main(pub objc2::MainThreadMarker);

/// A run loop source that ends the daemon when it is signalled.
///
/// The exit arrives as an ordinary run loop callout, like every other thing
/// this daemon reacts to — which is the point. Whatever asked (today, a stop
/// signal; see [`crate::signals`]) does nothing but signal this source, so no
/// caller has to know how the app is torn down, and none of the teardown runs
/// in whatever context that caller happened to be in.
///
/// It writes the app's own `AppExit` and runs a pass, so a signal leaves by
/// exactly the path a `--exit` request leaves by: the same systems observe it,
/// and [`crate::runloop::AppLoop`]'s token is still the only thing that ends
/// Carbon's loop.
#[must_use]
#[skylight::main_thread]
pub fn exit_waker() -> crate::runloop::Waker {
    crate::runloop::Waker::install(proof, || {
        APP.with(|slot| {
            if let Ok(mut slot) = slot.try_borrow_mut()
                && let Some(app) = slot.as_mut()
            {
                app.world_mut().write_message(bevy_app::AppExit::Success);
            }
        });
        pass();
    })
}

/// The same wake, as a [`std::task::Waker`], for the deadline reactor.
#[skylight::main_thread]
fn pass_waker() -> std::task::Waker {
    crate::runloop::Waker::to_std(&waker(proof))
}

/// The frame interval to cap presenting at: the fastest attached display's.
fn frame_interval() -> Duration {
    let displays = skylight::display::active().unwrap_or_else(|err| {
        tracing::warn!(%err, "could not read the display list; assuming 60 Hz");
        Vec::new()
    });
    let fastest = displays
        .iter()
        .map(|display| display.refresh_rate())
        .fold(0.0_f64, f64::max);
    let hz = if fastest > 0.0 {
        fastest
    } else {
        skylight::display::ASSUMED_REFRESH_RATE
    };
    Duration::from_secs_f64(1.0 / hz)
}

/// Folds what [`layout::needs_repaint`] found into the budget.
pub fn remember_bar(In(wanted): In<bool>, mut budget: ResMut<FrameBudget>) {
    if wanted {
        budget.wants_bar();
    }
}

pub fn remember_popups(In(wanted): In<bool>, mut budget: ResMut<FrameBudget>) {
    if wanted {
        budget.wants_popups();
    }
}

pub fn open_frame(mut budget: ResMut<FrameBudget>) {
    budget.open(Instant::now());
}

pub fn close_frame(mut budget: ResMut<FrameBudget>) {
    budget.close();
}

#[must_use]
pub fn presenting(budget: Res<FrameBudget>) -> bool {
    budget.presenting()
}

#[must_use]
pub fn presenting_bar(budget: Res<FrameBudget>) -> bool {
    budget.presenting() && budget.bar
}

#[must_use]
pub fn presenting_popups(budget: Res<FrameBudget>) -> bool {
    budget.presenting() && budget.popups
}

/// A request that arrived over the Mach port, with wherever its answer goes.
///
/// Deliberately not a `Message`: a `Reply` is a send-once right and answering
/// consumes it, but messages are read by reference and may have many readers.
/// A request has exactly one consumer, so the bus would cost the ability to
/// answer and buy nothing.
pub struct IpcRequest {
    pub request: Box<Request>,
    pub reply: Option<rsbar_protocol::wire::Reply>,
    /// A port the client attached, wanting its events pushed back rather than
    /// turned into a forked script.
    pub subscriber: Option<rsbar_protocol::wire::Subscriber>,
}

/// An event, from a source or from this thread.
///
/// This one *is* a message: an event legitimately has several readers, and a
/// reader missing a frame beats acting on a backlog.
#[derive(Message)]
pub struct EventMessage(pub std::sync::Arc<Event>);

/// The request queue, which is `!Sync` and so cannot be a plain resource.
///
/// Shared with the run loop's Mach port handler rather than owned: both ends
/// are on this thread now, so the handover is an `Rc` and not a channel. See
/// [`crate::ipc`].
///
/// Events do not come through here: each source owns its own feed and the
/// registry drains them, so an emission arrives tagged with what produced it.
pub struct Inbox(pub std::rc::Rc<crate::ipc::Requests>);

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
// One over clippy's limit, and the one over is the proof. Bundling the rest
// into a struct to get under it would hide what this takes rather than reduce
// it.
#[allow(clippy::too_many_arguments, reason = "the extra argument is the proof")]
#[skylight::main_thread]
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
        // The shared wake, not one of its own: a finished Accessibility scan
        // and a matched AX notification each have to bring a pass about --
        // without that the dirty flag would sit there until something
        // unrelated ran one, which on an idle bar is never.
        .insert_non_send(Main(proof.marker()))
        .insert_non_send(crate::alias::Captures::new(waker(proof)))
        .insert_non_send(crate::subscribers::Subscribers::default())
        .insert_resource(settings)
        .init_resource::<Index>()
        .init_resource::<Queue>()
        .init_resource::<ActiveSpaces>()
        .init_non_send::<ReloadWatch>()
        .init_resource::<ForceRepaint>()
        .insert_resource(FrameBudget::new(frame_interval()))
        .init_non_send::<Wake>()
        .init_non_send::<Routines>()
        .init_resource::<Placements>()
        .init_resource::<crate::requests::Defaults>()
        .init_resource::<crate::popup::PopupPlacements>()
        .init_non_send::<crate::popup::Popups>()
        // Starts true, so the first pass runs the config. The bar comes up
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
                routines,
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
                collect_belongings,
                settle_sources,
                note_launched_apps,
                note_display_changes,
                refresh_aliases.run_if(aliases_need_refresh),
                // Piped into the budget rather than left as run conditions: a
                // condition cannot write, and what a gate found has to
                // outlive the pass when the frame is not open yet.
                layout::needs_repaint.pipe(remember_bar),
                // Its own gate, and its own retained placements: a popup
                // opening must not repaint the bar, and a change inside one
                // must not either.
                crate::popup::needs_repaint_popups.pipe(remember_popups),
                open_frame,
                arm_wake,
                layout::repaint.run_if(presenting_bar),
                crate::popup::repaint_popups.run_if(presenting_popups),
                crate::tracking::place,
                crate::tracking::rebuild,
                // Only once the pixels it asked for exist. Cleared on a
                // deferred pass, a forced repaint would reach the frame
                // boundary as an ordinary one and redraw less than it was
                // asked to.
                layout::clear_force_repaint.run_if(presenting),
                close_frame,
            )
                .chain(),
        );

    app
}

thread_local! {
    /// The app, reachable from the run loop callbacks that drive it.
    ///
    /// `RunApplicationEventLoop` never returns, so a pass cannot be the body
    /// of a loop the way it was under `CFRunLoopRunInMode` -- it has to be
    /// something the run loop calls. Everything here is on one thread, and the
    /// `RefCell` is what makes a callback that re-enters during a pass a panic
    /// rather than two mutable borrows.
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

#[cfg(test)]
thread_local! {
    /// When each pass this thread has run happened, on the wall clock.
    ///
    /// The measurement the whole stage is for: idle CPU says nothing about
    /// whether a wake was *scheduled*, and this counts the schedule runs an
    /// interval really cost — and, because a clock is supposed to be on the
    /// second, says when each of them was. Thread-local, because the tests
    /// asserting on it run beside each other.
    static PASSES: RefCell<Vec<Duration>> = const { RefCell::new(Vec::new()) };
}

/// When each pass this thread has run happened, since the epoch.
#[cfg(test)]
fn passes() -> Vec<Duration> {
    PASSES.with(|passes| passes.borrow().clone())
}

/// Advances the app once, and quits the event loop if it asked to exit.
///
/// Called by every waker: a source signalling, a request arriving, a deadline
/// an item registered coming due. Doing nothing when the app is absent
/// matters: the waker is installed before the app is built, so it can fire
/// first.
pub fn pass() {
    APP.with(|slot| {
        let Ok(mut slot) = slot.try_borrow_mut() else {
            // Re-entered from inside a pass -- a Carbon handler dispatched
            // while we were already updating. The outer pass will see
            // whatever this one would have.
            return;
        };
        let Some(app) = slot.as_mut() else {
            return;
        };
        #[cfg(test)]
        PASSES.with(|passes| {
            passes.borrow_mut().push(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default(),
            );
        });

        // Take everything else already waiting before doing a pass. A client
        // sending fifty updates would otherwise get fifty passes and fifty
        // repaints, each redrawing one item -- sixty window server round trips
        // a second where one would do. Bounded, so a source firing
        // continuously cannot hold the pass off forever.
        for _ in 0..COALESCE_LIMIT {
            // SAFETY: `kCFRunLoopDefaultMode` is a `'static` constant, and
            // this is the thread that owns the run loop being pumped.
            let more = unsafe { CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, 0.0, true) };
            if !matches!(more, CFRunLoopRunResult::HandledSource) {
                break;
            }
        }

        app.update();

        if app.should_exit().is_some()
            && let Some(loop_) = crate::runloop::AppLoop::take()
        {
            loop_.quit();
        }
    });
}

/// Runs until something asks the app to exit.
///
/// [`AppLoop`](crate::runloop::AppLoop) rather than a loop around
/// `CFRunLoopRunInMode`: see its own doc for what Carbon's loop dispatches
/// that this one would not, and for why entering it and leaving it are two
/// halves of one type rather than a guard.
pub fn run(mut app: App) -> bevy_app::AppExit {
    app.finish();
    app.cleanup();
    APP.with(|slot| *slot.borrow_mut() = Some(app));

    // The one pass nothing asks for. Everything after this is a wake someone
    // registered -- a source, a request, a deadline -- but the first schedule
    // run is what runs the config and arms whatever it creates, and with no
    // tick left there is nothing else to bring it about.
    pass();

    crate::runloop::AppLoop::enter();

    // Taken out, not left behind. A thread local is destroyed at thread exit
    // in an order nothing here chooses, and dropping the whole `App` from
    // inside that destructor means its own drops reach thread locals that may
    // already be gone -- which panics, and a panic in a thread local's
    // destructor aborts the process. That turned every clean exit, a `--exit`
    // request included, into "fatal runtime error: thread local panicked on
    // drop". Out here it is an ordinary local, dropped on the stack while
    // everything it might touch is still alive.
    let mut app = APP.with(|slot| slot.borrow_mut().take());
    app.as_mut()
        .and_then(|app| app.should_exit())
        .unwrap_or(bevy_app::AppExit::Success)
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
    mut budget: ResMut<FrameBudget>,
    main: NonSend<Main>,
) {
    if !events
        .read()
        .any(|EventMessage(event)| Kind::DisplayChanged.matches(event))
    {
        return;
    }
    if let Err(err) = panels.rebuild(main.0, &settings) {
        tracing::error!(%err, "could not rebuild the bar after a display change");
    }
    // A display that arrived may refresh faster than anything already here.
    budget.set_interval(frame_interval());
    // Nothing about the items moved, but their panels did.
    repaint.0 = true;
}

/// Everything an item owns outside the ECS, which has to be released when it
/// is despawned.
///
/// Grouped because forgetting one of them is the mistake: each is a map keyed
/// by entity, and one left unpruned strands a shaped line, a captured image, a
/// Mach port — or a deadline the run loop keeps waking for — for the life of
/// the process.
#[derive(bevy_ecs::system::SystemParam)]
pub struct Belongings<'w> {
    cache: NonSendMut<'w, Cache>,
    captures: NonSendMut<'w, crate::alias::Captures>,
    subscribers: NonSendMut<'w, crate::subscribers::Subscribers>,
    routines: NonSendMut<'w, Routines>,
}

/// Releases what despawned items left behind.
///
/// Reads it off the ECS rather than each despawn site remembering it by hand:
/// missing one leaks a `CTLine`, a captured image, a Mach port, or a standing
/// wake, for the life of the process — and asking the ECS which entities went
/// cannot be forgotten at a new despawn site the way a manual list can.
///
/// Removals, not a scan of the live items: `RemovedComponents` names the
/// entities that went this pass, so the sweep costs nothing on a pass where
/// nothing was despawned. `Item` is the marker every item carries, so losing
/// it is the despawn.
fn collect_belongings(mut removed: RemovedComponents<Item>, mut belongings: Belongings) {
    let Belongings {
        cache,
        captures,
        subscribers,
        routines,
    } = &mut belongings;
    for entity in removed.read() {
        cache.forget(entity);
        let _ = captures.forget(entity);
        subscribers.clear(entity);
        // The registration's own drop is what disarms the reactor, so an item
        // that goes away takes its wake with it and a bar emptied by a reload
        // is back to no timer at all.
        routines.forget(entity);
    }
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

/// Re-captures the alias items that something asked about, and says so only
/// when one actually changed.
///
/// Two rules decide what this touches, and both are "work follows demand".
///
/// **Only what is visible.** An alias inside a closed popup, or one whose
/// `drawing` is off, is nobody's pixels: it holds no capture, no dirty
/// subscription and no `AXObserver`. Hiding one takes all three down;
/// showing it again is the same path a brand new alias takes, so there is no
/// second mechanism and no stale capture to draw — the held pixels went with
/// the watch, and [`AliasContent`] is reset so the re-capture reads as a
/// change even if the item looks identical.
///
/// **Only what changed.** A notification names one status item, so it costs
/// one capture. Walking every alias and asking each whether it was the one
/// made a clock tick cost the whole config. [`Due`] is that distinction; only
/// something that changes what an alias *resolves to* — a fresh owner scan,
/// an application launching — makes a pass look at all of them.
fn refresh_aliases(
    main: NonSend<Main>,
    mut captures: NonSendMut<crate::alias::Captures>,
    mut items: Query<(
        Entity,
        &AliasSpec,
        &Drawing,
        Option<&crate::popup::PopupOf>,
        &mut AliasContent,
    )>,
    hosts: Query<&crate::popup::PopupConfig>,
) {
    let started = std::time::Instant::now();
    // Collects a finished owner scan and a finished window list repair --
    // see `alias::Captures::table` -- and decides whether this pass looks at
    // every alias.
    captures.begin_pass();
    let due = captures.due();
    let everything = matches!(due, Due::Everything);

    // Pictures taken since the last pass, installed before anything asks for
    // another. A capture is started on one pass and lands on a later one, so
    // this is where an alias's pixels actually change -- `refresh` below only
    // ever asks.
    let landed = captures.collect();

    // The menu bar layer as of its last repair, shared rather than fetched:
    // a read here is a lock, not a window server round trip -- see
    // `alias::Captures::table`. Held for the whole pass; the layer does not
    // change between two items read in one turn of the run loop.
    let table = captures.table();
    let snapshot = table
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    captures.note_rescan(main.0, &snapshot);

    for (entity, spec, drawing, popup, mut content) in &mut items {
        if !shown(*drawing, popup, &hosts) {
            if captures.forget(entity) {
                tracing::debug!(spec = %spec.0, "an alias went out of sight; dropping its watch");
                // The component is a repaint token, not retained state:
                // resetting it is what makes the re-capture read as a change
                // even when the item comes back looking identical. Without
                // it the pixels would be gone and nothing would ask for them
                // to be drawn again.
                content.set_if_neq(AliasContent(0));
            }
            continue;
        }
        // A notification names one item and means its pixels changed; a scan
        // or a launch means any alias may resolve somewhere else now, which
        // is a much cheaper question. An alias with no mirror has never been
        // looked at at all -- new, or just back in sight.
        let look = if matches!(&due, Due::These(named) if named.contains(&entity)) {
            Look::Changed
        } else if everything || !captures.holds(entity) {
            Look::Resolve
        } else {
            continue;
        };
        captures.refresh(main.0, entity, &spec.0, &snapshot, look);
    }
    drop(snapshot);
    for (entity, digest) in landed {
        // `holds` because the loop above may have just dropped this alias for
        // going out of sight, and a picture that arrived a moment too late
        // must not mark a hidden item as having something new to draw.
        if captures.holds(entity)
            && let Ok((.., mut content)) = items.get_mut(entity)
        {
            content.set_if_neq(AliasContent(digest));
        }
    }
    captures.note_pass(started.elapsed());
}

/// Whether anyone can see this alias: it is drawn, and if it lives in a
/// popup, that popup is open.
///
/// The two are genuinely separate — a child's own `drawing` says nothing
/// about whether its host popup is showing — and a popup whose host has gone
/// away is not showing either.
fn shown(
    drawing: Drawing,
    popup: Option<&crate::popup::PopupOf>,
    hosts: &Query<&crate::popup::PopupConfig>,
) -> bool {
    drawing.0
        && popup.is_none_or(|crate::popup::PopupOf(host)| {
            hosts.get(*host).is_ok_and(|config| config.drawing)
        })
}

/// An alias that is new, rewritten, or has just been shown or hidden. `Added`
/// as well as `Changed` because a component inserted with a value bevy
/// already had reports only the first.
type ChangedAlias = (
    With<AliasSpec>,
    Or<(Added<AliasSpec>, Changed<AliasSpec>, Changed<Drawing>)>,
);

/// Whether anything could have changed about what an alias draws.
///
/// The system had no condition at all, so every pass listed the menu bar
/// layer and — before [`crate::extras::Scanner`] — could reach a
/// second-and-a-half Accessibility walk from it. The causes are all edges: an
/// alias arriving or being rewritten, a notification from the application
/// that owns one, a finished owner scan, or an application launching (which
/// [`note_launched_apps`] turns into the same flag).
fn aliases_need_refresh(
    captures: NonSend<crate::alias::Captures>,
    changed: Query<(), ChangedAlias>,
    popups: Query<(), Changed<crate::popup::PopupConfig>>,
) -> bool {
    // A popup opening or closing changes which aliases are visible, and
    // visibility is what decides whether one is watched at all.
    captures.pending() || !changed.is_empty() || !popups.is_empty()
}

/// An application launching is the one thing that can make an alias that has
/// never resolved resolve: its owner is now running.
///
/// This is what replaced a five-second retry backoff. Strictly better than
/// one, too — an alias for an application the user has just started appears
/// when it starts, rather than up to five seconds later.
fn note_launched_apps(
    mut captures: NonSendMut<crate::alias::Captures>,
    mut events: MessageReader<EventMessage>,
) {
    if events
        .read()
        .any(|event| matches!(&*event.0, Event::AppLaunched(_)))
    {
        captures.rescan();
    }
}

/// A display change repositions every window in the menu bar layer, so every
/// bound the alias table is holding is stale.
///
/// Separate from [`rebuild_panels`], which reacts to the same event: that one
/// rebuilds our own panels, this one asks for the *window server's* view of
/// everyone else's windows to be re-read. Not a [`crate::alias::Captures::rescan`]
/// -- a display moving changes where windows are, never who owns them.
fn note_display_changes(
    captures: NonSend<crate::alias::Captures>,
    mut events: MessageReader<EventMessage>,
) {
    if events
        .read()
        .any(|EventMessage(event)| Kind::DisplayChanged.matches(event))
    {
        captures.note_reconfigured();
    }
}

/// Takes everything queued across every running source.
fn drain_events(mut sources: NonSendMut<Sources>, mut out: MessageWriter<EventMessage>) {
    sources.0.drain(|id, event| {
        tracing::trace!(source = %id, kind = %event.kind(), "drained");
        // Shared from here on. Everything downstream — the reactions, the
        // routing, the job handed to a worker — takes a reference count
        // rather than a copy of the payload.
        out.write(EventMessage(std::sync::Arc::new(event)));
    });
}

/// Hands `request` to a worker and answers `reply` once it is done, for the
/// two requests [`apply_requests`] cannot answer synchronously.
///
/// [`crate::menus::list`] and [`crate::menus::press`] are `async`: the only
/// part that needs the main thread is `AppKit`'s idea of the frontmost
/// application, and the rest -- a cross-process Accessibility RPC per menu --
/// runs on [`crate::pool`] instead of stalling the thread that composites the
/// bar. `requests::apply` is synchronous and answers straight onto the reply
/// that arrived with the request, so it cannot await either of them; this is
/// the seam instead, spawned on [`crate::pool`] and answering the same
/// [`rsbar_protocol::wire::Reply`] once the walk finishes.
///
/// `Ok(())` if `request` was one of the two this handles -- the reply is
/// spoken for, spawned onto a worker. `Err` hands `reply` straight back
/// otherwise, so [`apply_requests`] can carry on as before.
fn defer(
    request: &Request,
    reply: Option<rsbar_protocol::wire::Reply>,
) -> std::result::Result<(), Option<rsbar_protocol::wire::Reply>> {
    match *request {
        Request::Query(ProtocolQuery::AppMenus) => {
            crate::pool::spawn(async move {
                let response = match crate::menus::list().await {
                    Ok(found) => {
                        Response::AppMenus(found.into_iter().map(|menu| menu.title).collect())
                    }
                    Err(err) => Response::Error(err.to_string()),
                };
                if let Some(reply) = reply
                    && let Err(err) = reply.send(&response)
                {
                    tracing::debug!(%err, "client stopped waiting for its answer");
                }
            });
            Ok(())
        }
        Request::Press(PressTarget::AppMenu(index)) => {
            crate::pool::spawn(async move {
                let response = match crate::menus::press(index).await {
                    Ok(()) => Response::Ok,
                    Err(err) => Response::Error(err.to_string()),
                };
                if let Some(reply) = reply
                    && let Err(err) = reply.send(&response)
                {
                    tracing::debug!(%err, "client stopped waiting for its answer");
                }
            });
            Ok(())
        }
        _ => Err(reply),
    }
}

/// Drains the request queue and applies each one.
///
/// Drains rather than taking one: run loop wakes coalesce, so a burst can
/// arrive between two wakes — a config run is hundreds of sets.
#[allow(clippy::too_many_arguments, reason = "a request can touch all of it")]
fn apply_requests(
    main: NonSend<Main>,
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
    while let Some(IpcRequest {
        request,
        reply,
        subscriber,
    }) = inbox.0.pop()
    {
        let reply = match defer(&request, reply) {
            Ok(()) => continue,
            Err(reply) => reply,
        };
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
            main: main.0,
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
    mut watch: NonSendMut<ReloadWatch>,
    main: NonSend<Main>,
) {
    if !reloading.0 {
        return;
    }
    reloading.0 = false;

    for entity in &items {
        commands.entity(entity).insert(Stale);
    }

    watch.0 = Some(crate::sources::config::reload(
        &config.0,
        service.0.clone(),
        &waker(main.0),
    ));
}

/// The reload in flight, if one is.
///
/// The receiver *is* the state: holding one means a run was started and has
/// not answered, taking the value means it has, and the value is what it came
/// to. There is no flag to keep in step with it and nothing to poll — the
/// config thread wakes the app when it sends, so the one pass that reads this
/// is the pass that reading it is for.
///
/// Not awaited as a task, though the daemon now has an executor for it: what
/// happens with the answer is despawning entities, which needs `&mut World`,
/// and a task does not have one. Moving the await into a task would only move
/// the handover, and the handover is the part with a shape.
#[derive(Default)]
pub struct ReloadWatch(Option<tokio::sync::oneshot::Receiver<crate::config::Outcome>>);

/// Finishes a reload once the config thread has stopped.
///
/// On success, whatever is still marked stale is what the new config dropped,
/// so it goes. On failure nothing goes — the mark is simply lifted and the
/// previous bar stands.
fn settle_reload(
    mut watch: NonSendMut<ReloadWatch>,
    mut commands: Commands,
    mut index: ResMut<Index>,
    stale: Query<(Entity, &Name), With<Stale>>,
) {
    let Some(finished) = watch.0.as_mut() else {
        return;
    };
    let outcome = match finished.try_recv() {
        Ok(outcome) => outcome,
        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return,
        // The thread went without answering, which is a failed run: nothing
        // replaced what is on the bar, so nothing on the bar should go.
        Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
            Err("the config thread went away".to_owned())
        }
    };
    watch.0 = None;
    let succeeded = outcome.is_ok();

    for (entity, name) in &stale {
        if succeeded {
            index.remove(&name.0);
            // The item's claims are components, so the despawn releases them
            // and a source nothing wants any more stops on the next settle.
            // What it owns outside the ECS goes with [`collect_belongings`].
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
    // The cached scan, not a fresh one: a click may not wait on an
    // Accessibility walk of every running application.
    let pressed = crate::alias::Snapshot::cached()
        .and_then(|snapshot| crate::alias::press_item(&snapshot, owner, name));
    if let Err(err) = pressed {
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

/// What a routine needs to know about one item.
#[derive(bevy_ecs::query::QueryData)]
struct Ticking {
    entity: Entity,
    name: &'static Name,
    clock: &'static Routine,
    script: Option<&'static Script>,
    updates: Option<&'static Updates>,
}

/// One item's standing deadline.
struct Armed {
    /// What it was armed for, so a pass can tell an item that has come due
    /// from one that is merely being looked at again.
    at: Instant,
    /// The frequency it was armed for. A `--set update_freq` that moves it
    /// has to move the deadline now rather than at the end of the old period.
    every: u32,
    /// Cancels on drop, which is the whole mechanism: the entry going is the
    /// wake going.
    _registration: crate::runloop::Registration,
}

/// The deadline each item with an `--update-freq` is waiting on.
///
/// `NonSend` because a [`crate::runloop::Registration`] belongs to the thread
/// whose reactor holds it, and the type says so. Keyed by entity, so
/// [`collect_belongings`] disarms a despawned item by forgetting it — there is
/// no separate teardown to remember.
#[derive(Default)]
pub struct Routines(std::collections::HashMap<Entity, Armed>);

impl Routines {
    /// Drops the deadline a despawned item was holding.
    fn forget(&mut self, entity: Entity) {
        self.0.remove(&entity);
    }
}

/// The next boundary of a `freq`-second period, on the wall clock.
///
/// **A deliberate departure from `SketchyBar`**, whose routines are a
/// phase-free counter: an item added at 12:00:00.4 with `--update-freq 60`
/// runs at :00.4 past every minute there, and a clock showing minutes is
/// therefore wrong for four tenths of every one of them. Aligning to the
/// epoch means a 60 s item fires at `:00` and a 1 s item fires on the second,
/// which is what "a clock" means and what the wake is *for*. Two items at the
/// same frequency also come due together, so they cost one pass between them.
///
/// A period that does not divide the epoch evenly still gets an even rhythm,
/// just not one aligned to anything a human names — there is no better answer
/// for `--update-freq 7`.
fn next_boundary(every: u32, now: Instant) -> Instant {
    let period = u128::from(every) * 1_000_000_000;
    let wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // A boundary landing exactly on `now` is the *next* one, not this one:
    // zero would be a deadline already past and a wake that never sleeps.
    let left = period - (wall % period);
    now + Duration::from_nanos(u64::try_from(left).unwrap_or(u64::MAX))
}

/// Runs the routines that have come due, and arms the ones that have not.
///
/// The replacement for a one-second tick over every item. An item asking for
/// `--update-freq 60` is one deadline at the next minute boundary and nothing
/// in between; an item that never asked for one is nothing at all, which is
/// what lets a static bar hold no timer.
///
/// Arming and firing are the same system on purpose. The reactor hands back
/// only a wake, not which deadline caused it, so what makes an item due is
/// its own recorded deadline having passed — and the pass that notices is the
/// one that arms the next.
fn routines(
    main: NonSend<Main>,
    mut armed: NonSendMut<Routines>,
    items: Query<Ticking>,
    mut subscribers: NonSendMut<crate::subscribers::Subscribers>,
    mut queue: ResMut<Queue>,
) {
    let now = Instant::now();
    let routine = std::sync::Arc::new(Event::Routine(rsbar_protocol::event::Routine {}));

    for row in &items {
        let every = row.clock.every;
        // Nothing asked for a wake here. An item with updates switched off
        // gives its deadline up rather than keeping one it will not use: the
        // rhythm is the wall clock's, so turning updates back on rejoins it
        // rather than restarting it.
        if every == 0 || row.updates.is_some_and(|updates| !updates.0) {
            armed.0.remove(&row.entity);
            continue;
        }

        let standing = armed.0.get(&row.entity);
        let due = match standing {
            // First sight of an item that wants updating: run it *now*. A
            // config that draws a clock means it to say the time as soon as
            // the item appears, not up to a period later — `SketchyBar` runs
            // an item's script when it is added, for the same reason. The
            // alignment starts from the boundary after this one, so the first
            // update is immediate and every one after it is on the second.
            None => true,
            Some(standing) => now >= standing.at,
        };
        // Its frequency moved under it. The deadline it holds is the wrong
        // one, but that is not a reason to *run*: retuning is not an update.
        let retune = standing.is_none_or(|standing| standing.every != every);
        if let Some(standing) = standing.filter(|_| due) {
            tracing::trace!(
                item = %row.name.0,
                late = ?now.saturating_duration_since(standing.at),
                "a routine came due"
            );
        }
        if due || retune {
            let at = next_boundary(every, now);
            armed.0.insert(
                row.entity,
                Armed {
                    at,
                    every,
                    _registration: crate::runloop::wake_at(main.0, at, pass_waker(main.0)),
                },
            );
        }
        if !due {
            continue;
        }

        // A client holding a port takes the routine itself. Checked before
        // the script, and before asking whether there *is* one: an item that
        // only exists to be updated by a Lua callback has no script at all,
        // and gating on one meant its routine went nowhere.
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

/// Rebuilds the shaped text of any item whose text or font moved, across the
/// shared runtime rather than one line at a time on this thread.
///
/// This is the cache's whole invalidation strategy: `Changed` says which items
/// need it, so it cannot go stale without someone having changed something.
///
/// `Changed` is only the coarse first pass, though. It fires for any field of
/// a run -- a colour, a padding, a y-offset -- and none of those shape
/// anything, so a `--set clock icon.y_offset=5` used to reach `Font::resolve`
/// twice and clone four strings before the by-value comparison inside [`Text`]
/// declined to rebuild the line. [`Cache::refresh_changed`] makes the second
/// test itself, so nothing here has to ask twice.
fn reshape(changed: Reshaped, mut cache: NonSendMut<Cache>) {
    cache.refresh_changed(
        changed
            .iter()
            .map(|(entity, icon, label)| (entity, &icon.0, &label.0)),
    );
}

#[cfg(test)]
mod idle_tests {
    //! Idle at zero, asserted rather than sampled.
    //!
    //! `ps` and `top` are entitlement-blocked here and would answer the wrong
    //! question anyway: idle CPU says nothing about whether a wake was
    //! *scheduled*. [`crate::runloop::armed`] says exactly what the one timer
    //! stands armed for, and [`super::passes`] says what a stretch of real
    //! time actually cost in schedule runs. Both are exact.
    //!
    //! The app here is the daemon's own — `ecs::build`, parked in `APP` the
    //! way [`super::run`] parks it — with nothing stubbed. Drawing is a no-op
    //! only because `Panels::default()` has no displays in it.

    use super::{APP, Inbox, PASSES, build, pass, passes};
    use crate::components::{Order, Routine, bundle};
    use crate::runloop::armed;
    use crate::sources::Registry;
    use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
    use rsbar_protocol::{ItemName, Position};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    /// Runs this test alone, however many threads libtest gave the binary.
    ///
    /// Every test here measures real time against a real run loop, with
    /// margins of 10 and 25 milliseconds. Six of them pumping at once on a
    /// debug build is enough scheduling jitter to blow those margins — and a
    /// wake that was late because five other run loops wanted the same core
    /// says nothing about whether the reactor armed the right deadline, which
    /// is the only thing being asked. So they take turns. Poisoning is
    /// ignored: one test failing its assertion must not turn the other five
    /// into a different failure that hides it.
    fn alone() -> std::sync::MutexGuard<'static, ()> {
        static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
        TURN.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Lets the run loop take its turns for `seconds`, the way the daemon's
    /// own parked loop would.
    fn pump(seconds: f64) {
        // SAFETY: `kCFRunLoopDefaultMode` is a `'static` constant, and this is
        // the thread that owns the run loop being pumped.
        unsafe { CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, seconds, false) };
    }

    /// How far into the current second the wall clock is.
    fn into_second() -> Duration {
        Duration::from_nanos(u64::from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos(),
        ))
    }

    /// Waits until a little past the next whole second.
    ///
    /// Not cosmetic: a pump started at `x.999` has its second fire on its own
    /// edge, and the count would be two or three depending on the scheduler.
    /// Starting just inside a second puts both fires well within the window.
    fn align() {
        /// Far enough inside the second that neither fire lands on the pump's
        /// own edge.
        const MARGIN: Duration = Duration::from_millis(100);

        std::thread::sleep(Duration::from_secs(1).saturating_sub(into_second()) + MARGIN);
    }

    /// The daemon's app, with `items` items of the given update frequency.
    ///
    /// Frequency zero is the requirement's bar: a static block of colour,
    /// which is what an item is before anything asks it to update. No script
    /// either — a script running would signal the pool's waker, and this is
    /// counting wakes.
    fn daemon(items: usize, every: u32) -> bevy_app::App {
        let mut app = build(
            crate::runloop::main_thread(),
            Inbox(std::rc::Rc::new(crate::ipc::Requests::default())),
            crate::bar::Settings::default(),
            crate::bar::Panels::default(),
            Registry::new(
                crate::runloop::main_thread(),
                crate::config::shared(),
                super::waker(crate::runloop::main_thread()),
            ),
            crate::script::Runner::start(1),
            crate::config::shared(),
            "test".to_owned(),
        );
        for i in 0..items {
            let name = ItemName::new(format!("item{i}")).expect("a valid name");
            let order = Order(u32::try_from(i).expect("a small count"));
            let entity = app
                .world_mut()
                .spawn(bundle(name, Position::Left, order))
                .id();
            app.world_mut()
                .entity_mut(entity)
                .insert(Routine { every, elapsed: 0 });
        }
        app
    }

    /// Parks the app the way [`super::run`] does, first pass included.
    fn park(mut app: bevy_app::App) {
        app.finish();
        app.cleanup();
        APP.with(|slot| *slot.borrow_mut() = Some(app));
        PASSES.with(|passes| passes.borrow_mut().clear());
        pass();
    }

    /// Lets go of the app, so a test cannot leave one parked for the next.
    fn unpark() {
        drop(APP.with(|slot| slot.borrow_mut().take()));
        assert_eq!(armed(), None, "the app took its deadlines with it");
    }

    /// **The requirement.** A bar of one static block of colour arms no timer
    /// and costs no wakes at all — not a cheap tick, none.
    #[test]
    fn a_static_bar_arms_nothing_and_costs_no_passes() {
        let _alone = alone();
        park(daemon(1, 0));
        assert_eq!(
            armed(),
            None,
            "a bar nothing asked to update holds no timer"
        );

        let before = passes().len();
        pump(2.0);
        assert_eq!(
            passes().len(),
            before,
            "two seconds of a static bar cost {} pass(es); it must cost none",
            passes().len() - before
        );
        assert_eq!(armed(), None, "and still nothing is armed");
        unpark();
    }

    /// One second is one deadline, at the next whole second — not one second
    /// from whenever the item happened to be added.
    #[test]
    fn a_one_second_item_arms_the_next_whole_second() {
        let _alone = alone();
        park(daemon(1, 1));
        let deadline = armed().expect("an item asking to update arms a deadline");
        let left = deadline.saturating_duration_since(Instant::now());
        let boundary = Duration::from_secs(1).saturating_sub(into_second());
        assert!(
            left.abs_diff(boundary) < Duration::from_millis(10),
            "armed {left:?} out; the next whole second is {boundary:?} out"
        );
        unpark();
    }

    /// The benchmark: a clock ticking every second wakes exactly once per
    /// second, on the second, and never in between.
    #[test]
    fn a_one_second_clock_wakes_exactly_once_a_second_on_the_second() {
        let _alone = alone();
        align();
        park(daemon(1, 1));

        let before = passes().len();
        pump(2.0);
        let woke = &passes()[before..];
        assert_eq!(
            woke.len(),
            2,
            "two seconds of a one-second clock must be two passes, not {}",
            woke.len()
        );
        for at in woke {
            let off = Duration::from_nanos(u64::from(at.subsec_nanos()));
            let late = off.min(Duration::from_secs(1).saturating_sub(off));
            assert!(
                late < Duration::from_millis(25),
                "a pass landed {late:?} off the second"
            );
        }
        unpark();
    }

    /// Several items on the same frequency come due together, and cost one
    /// pass between them — which is what the run loop source in the middle is
    /// for.
    #[test]
    fn items_sharing_a_frequency_share_a_wake() {
        let _alone = alone();
        align();
        park(daemon(8, 1));
        assert!(armed().is_some(), "eight items, and a deadline for them");

        let before = passes().len();
        pump(2.0);
        assert_eq!(
            passes().len() - before,
            2,
            "eight items on the same second must not cost eight passes"
        );
        unpark();
    }

    /// An item that wants updating runs when it is added, and then on the
    /// boundary — not up to a whole period after it appeared.
    ///
    /// The alignment is what a clock needs to stay right; running at once is
    /// what it needs to *be* right the moment it is drawn. A bar coming up
    /// with a blank clock for the rest of the second is what this stops.
    #[test]
    fn a_new_routine_runs_at_once_and_then_only_on_its_boundary() {
        use bevy_ecs::system::RunSystemOnce as _;

        let _alone = alone();

        let mut world = bevy_ecs::world::World::new();
        world.insert_non_send(super::Main(crate::runloop::main_thread()));
        world.insert_non_send(super::Routines::default());
        world.insert_non_send(crate::subscribers::Subscribers::default());
        world.init_resource::<super::Queue>();

        let name = ItemName::new("clock").expect("a valid name");
        let entity = world.spawn(bundle(name, Position::Right, Order(0))).id();
        world.entity_mut(entity).insert((
            Routine {
                every: 60,
                elapsed: 0,
            },
            crate::components::Script(std::sync::Arc::from("/usr/bin/true")),
        ));

        world
            .run_system_once(super::routines)
            .expect("a valid system");
        assert_eq!(
            world.resource::<super::Queue>().0.len(),
            1,
            "an item added with an update frequency runs straight away"
        );
        world.resource_mut::<super::Queue>().0.clear();

        let deadline = armed().expect("and is armed for the boundary after that");
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(
            left <= Duration::from_mins(1),
            "armed {left:?} out, which is past the next minute boundary"
        );

        world
            .run_system_once(super::routines)
            .expect("a valid system");
        assert!(
            world.resource::<super::Queue>().0.is_empty(),
            "and does not run again until that boundary arrives"
        );
        drop(world);
        assert_eq!(armed(), None);
    }

    /// The other half of the requirement: an item going away takes its wake
    /// with it, so a bar emptied by a reload is back to no timer at all.
    #[test]
    fn despawning_the_last_item_disarms_the_reactor() {
        let _alone = alone();
        let mut app = daemon(1, 1);
        let entity = app
            .world_mut()
            .query_filtered::<bevy_ecs::entity::Entity, bevy_ecs::prelude::With<crate::components::Item>>()
            .iter(app.world())
            .next()
            .expect("the item was spawned");
        park(app);
        assert!(armed().is_some());

        APP.with(|slot| {
            let mut slot = slot.borrow_mut();
            let app = slot.as_mut().expect("the app is parked");
            app.world_mut().despawn(entity);
        });
        pass();
        // The despawn is also a repaint, and a repaint inside the frame the
        // last present opened is a deferred one -- which is itself a deadline.
        // Pumping past the boundary lets that one serve and go, leaving only
        // the question this is asking.
        pump(0.2);
        assert_eq!(armed(), None, "the last item took the timer with it");
        unpark();
    }
}

#[cfg(test)]
mod frame_tests {
    //! The frame gate, driven by a clock the test advances by hand.
    //!
    //! The draw is stubbed — it is the only part that needs a window server —
    //! but every gate around it is the schedule's own, so what these check is
    //! the wiring rather than a model of it. `examples/frame_budget.rs` runs
    //! the same harness for the numbers.
    //!
    //! [`super::open_frame`] itself is the one system substituted, because all
    //! it does is read the clock: [`FrameBudget::open`] takes the instant
    //! explicitly for exactly this reason, so the gate under test here is the
    //! real one and only its source of "now" is the test's.

    use super::{
        FrameBudget, close_frame, presenting, presenting_bar, remember_bar, remember_popups,
    };
    use crate::bar::Settings;
    use crate::components::{Label, Order, bundle};
    use crate::layout::{DirtyItems, ForceRepaint, clear_force_repaint, needs_repaint};
    use crate::popup::needs_repaint_popups;
    use bevy_ecs::prelude::*;
    use bevy_ecs::schedule::Schedule;
    use rsbar_protocol::{ItemName, Position};
    use std::time::{Duration, Instant};

    const FRAME: Duration = Duration::from_micros(16_667);
    const GAP: Duration = Duration::from_micros(200);
    /// What is left of a frame after a [`GAP`] has gone by inside it.
    const REST: Duration = Duration::from_micros(16_467);

    /// The wake's instant, moved by the test rather than by the world.
    #[derive(Resource)]
    struct Clock(Instant);

    /// [`super::open_frame`] with the test's clock in place of the real one.
    fn open_frame(mut budget: ResMut<FrameBudget>, clock: Res<Clock>) {
        budget.open(clock.0);
    }

    #[derive(Resource, Default)]
    struct Draws {
        count: usize,
        damage: Vec<usize>,
    }

    fn draw(dirty: DirtyItems, mut draws: ResMut<Draws>) {
        let damaged = dirty.iter().count();
        draws.count += 1;
        draws.damage.push(damaged);
    }

    struct Bar {
        world: World,
        schedule: Schedule,
        items: Vec<Entity>,
    }

    impl Bar {
        fn new(items: usize) -> Self {
            let mut world = World::new();
            world.insert_resource(Settings::default());
            world.insert_resource(ForceRepaint::default());
            world.insert_resource(Draws::default());
            world.insert_resource(Clock(Instant::now()));
            world.insert_resource(FrameBudget::new(FRAME));

            let items = (0..items)
                .map(|i| {
                    let name = ItemName::new(format!("item{i}")).expect("a valid name");
                    let order = Order(u32::try_from(i).expect("a small count"));
                    world.spawn(bundle(name, Position::Left, order)).id()
                })
                .collect();

            let mut schedule = Schedule::default();
            schedule.add_systems(
                (
                    needs_repaint.pipe(remember_bar),
                    needs_repaint_popups.pipe(remember_popups),
                    open_frame,
                    draw.run_if(presenting_bar),
                    clear_force_repaint.run_if(presenting),
                    close_frame,
                )
                    .chain(),
            );

            let mut bar = Self {
                world,
                schedule,
                items,
            };
            // Spawning marked everything changed. Let that first pass draw,
            // the way the real bar's first pass does, then start counting.
            bar.wake(GAP);
            bar.world.resource_mut::<Draws>().count = 0;
            bar.world.resource_mut::<Draws>().damage.clear();
            bar
        }

        fn wake(&mut self, gap: Duration) {
            let mut clock = self.world.resource_mut::<Clock>();
            clock.0 += gap;
            self.schedule.run(&mut self.world);
        }

        fn touch(&mut self, which: usize) {
            let item = self.items[which % self.items.len()];
            let mut item = self.world.entity_mut(item);
            let mut label = item.get_mut::<Label>().expect("a label");
            label.0.string = format!("{}", label.0.string.len() + 1);
        }

        fn draws(&self) -> usize {
            self.world.resource::<Draws>().count
        }

        fn deferred(&self) -> Option<Duration> {
            self.world.resource::<FrameBudget>().deferred()
        }
    }

    #[test]
    fn a_burst_inside_one_frame_costs_one_draw() {
        let mut bar = Bar::new(8);
        for i in 0..50 {
            bar.touch(i);
            bar.wake(GAP);
        }
        assert_eq!(bar.draws(), 0, "nothing drew inside the frame");
        let left = bar.deferred().expect("a draw is outstanding");
        bar.wake(left);
        assert_eq!(bar.draws(), 1, "and one draw covered the lot");
    }

    /// The failure mode: a change 200us into a frame is skipped now, so it
    /// has to happen at the boundary even though nothing changes in between.
    #[test]
    fn a_deferred_draw_still_happens_with_nothing_new_to_prompt_it() {
        let mut bar = Bar::new(1);
        bar.touch(0);
        bar.wake(GAP);
        assert_eq!(bar.draws(), 0);
        bar.wake(bar.deferred().expect("a draw is outstanding"));
        assert_eq!(bar.draws(), 1);
        assert_eq!(bar.deferred(), None, "and nothing is left outstanding");
    }

    /// Damage is what the deferral could quietly lose: the draw at the
    /// boundary must carry every item touched since the last one, not just
    /// the last wake's.
    #[test]
    fn the_draw_at_the_boundary_carries_every_item_touched_since_the_last_one() {
        let mut bar = Bar::new(3);
        for i in 0..3 {
            bar.touch(i);
            bar.wake(GAP);
        }
        bar.wake(bar.deferred().expect("a draw is outstanding"));
        assert_eq!(
            bar.world.resource::<Draws>().damage,
            vec![3],
            "one draw, three damaged items"
        );
    }

    /// A forced repaint deferred to the boundary must still be a forced one
    /// there — cleared on the pass that skipped it, it would arrive as an
    /// ordinary repaint and redraw less than it was asked to.
    #[test]
    fn a_forced_repaint_survives_the_frame_it_was_deferred_by() {
        let mut bar = Bar::new(1);
        bar.world.resource_mut::<ForceRepaint>().0 = true;
        bar.wake(GAP);
        assert_eq!(bar.draws(), 0);
        assert!(
            bar.world.resource::<ForceRepaint>().0,
            "the force flag was cleared by a pass that did not draw"
        );
        bar.wake(bar.deferred().expect("a draw is outstanding"));
        assert_eq!(bar.draws(), 1);
        assert!(
            !bar.world.resource::<ForceRepaint>().0,
            "and cleared by the one that did"
        );
    }

    /// Idle is the whole point of the daemon: nothing dirty, nothing armed,
    /// so the run loop sleeps until something asks it not to.
    #[test]
    fn an_idle_pass_arms_no_wake() {
        let mut bar = Bar::new(4);
        for _ in 0..5 {
            bar.wake(FRAME);
            assert_eq!(bar.deferred(), None);
        }
        assert_eq!(bar.draws(), 0);
    }

    /// **A frame that was missed is dropped, not made up.** Five intervals
    /// passing in one step is one present, and the next boundary is one
    /// interval after *that* present rather than after the boundary that was
    /// missed. The alternative is a burst of catch-up draws putting the same
    /// pixels on the screen five times, which is what a stutter is.
    #[test]
    fn a_long_gap_costs_one_present_and_re_phases_from_it() {
        let mut bar = Bar::new(1);
        bar.touch(0);
        // The machine slept, or a modal held the loop, or a pass ran long.
        bar.wake(FRAME * 5);
        assert_eq!(bar.draws(), 1, "one present, not five");
        assert_eq!(bar.deferred(), None, "and nothing outstanding behind it");

        // The next frame is a full interval from the present, not from the
        // boundary that went by while nothing was running.
        bar.touch(0);
        bar.wake(GAP);
        assert_eq!(bar.draws(), 1, "still inside the frame that just opened");
        assert_eq!(
            bar.deferred(),
            Some(REST),
            "the boundary is measured from the present, not from a missed one"
        );
        bar.wake(REST);
        assert_eq!(bar.draws(), 2);
    }

    /// A change arriving after the frame has passed draws on the wake that
    /// brought it — the cap costs no latency when nothing is racing it.
    #[test]
    fn a_change_after_a_quiet_frame_draws_at_once() {
        let mut bar = Bar::new(1);
        bar.wake(FRAME);
        bar.touch(0);
        bar.wake(GAP);
        assert_eq!(bar.draws(), 1);
        assert_eq!(bar.deferred(), None);
    }
}

#[cfg(test)]
mod sweep_tests {
    use super::collect_belongings;
    use crate::components::{Order, Run, bundle};
    use crate::shaping::Cache;
    use bevy_ecs::prelude::*;
    use rsbar_protocol::style::Color;
    use rsbar_protocol::{ItemName, Position};

    /// The leak the sweep exists to stop: a despawn is the only signal, and
    /// no despawn site has to remember anything for the `CTLine` to go.
    #[test]
    fn a_despawned_item_stops_holding_its_shaped_text() {
        let mut world = World::new();
        world.insert_non_send(Cache::default());
        world.insert_non_send(crate::alias::Captures::default());
        world.insert_non_send(crate::subscribers::Subscribers::default());
        world.insert_non_send(super::Routines::default());
        let sweep = world.register_system(collect_belongings);

        let name = ItemName::new("one").expect("a valid name");
        let entity = world.spawn(bundle(name, Position::Left, Order(0))).id();
        let run = Run::new("Menlo:Regular:13", Color::WHITE);
        world.non_send_mut::<Cache>().refresh(entity, &run, &run);
        assert!(
            world.non_send::<Cache>().get(entity).is_some(),
            "the item shaped something to begin with"
        );

        world.despawn(entity);
        world.run_system(sweep).expect("a valid system");
        assert!(
            world.non_send::<Cache>().get(entity).is_none(),
            "the shaped text outlived the item that owned it"
        );
    }
}

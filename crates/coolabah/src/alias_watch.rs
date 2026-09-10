//! Turning an Accessibility notification on an aliased item's owning
//! application into "re-capture this one now".
//!
//! One [`AXObserver`] per owning pid, registered on **every element a
//! notification about a status item could plausibly come from**: the
//! application's own top-level element, its `AXExtrasMenuBar`, and each item
//! hanging off that bar including one level of `BentoBox` grandchildren.
//!
//! Breadth is not caution here, it is the difference between working and not:
//! registering on the application element alone measured **127 notifications
//! over two minutes, and not one of them was about a watched item**, while
//! the same targets registered per the tree above do receive them. An AX
//! notification does not reliably travel from a descendant up to the
//! application element, and a status item is a child, or grandchild, of the
//! extras bar. So the registration follows the tree.
//!
//! What the probe *did* settle, and still holds: `AXMenuBarItem` rejects
//! some notifications outright with `kAXErrorNotificationUnsupported`, so a
//! refusal on one target is ordinary and only a pid that refuses *every*
//! target and name is a failure.
//!
//! **Which entity (if any) an element is for is decided once, when it is
//! registered — not re-derived on every notification.** Each target is
//! registered with its own refcon naming that entity (see
//! [`skylight::ax::Observer::watch_matched`]), so `on_notification` recovers
//! it directly and reads nothing off the element at all. The match itself is
//! the same check this module always used, just moved from the callback to
//! [`register_targets`]/[`match_target`], run once per element instead of
//! once per notification:
//!
//! - the element must *be* one of this pid's watched items, which is true if
//!   **either** its on-screen frame is within [`FRAME_MATCH_RADIUS`] of where
//!   the window server last put that alias's window, **or** its
//!   `AXDescription` (falling back to `AXTitle`) is that alias's name exactly
//!   *and* its role is one [`named_item`] trusts.
//!
//! A per-notification `AXUIElementGetPid` check is not enough on its own:
//! `ax_observer_probe` once caught this kind of observer receiving a
//! callback whose *element* belonged to a wholly unrelated pid — another
//! app's still-mounted popover text — despite being registered for Control
//! Centre. A per-registration refcon is immune to that by construction:
//! `on_notification` never looks at the delivered element, only at what its
//! own registration was told it was.
//!
//! The frame is the strong half and the name is the weak one, which is the
//! reverse of how this started. Measured live against the real config, 127
//! notifications over two minutes: 117 of them came from elements nowhere
//! near the menu bar — Amphetamine's own popover text at (559, 420), 450
//! points below it — and every one is thrown out by the frame alone. The
//! role check used to stand in *front* of all this as a hard gate, and
//! discarded 86 notifications before the frame was ever read, every one of
//! them from a pid that had a visible, watched alias. It is now only a
//! tie-breaker on a name match; see [`named_item`].
//!
//! Matching on the frame is also what closes the one gap this module used to
//! admit to. An alias whose name carries the `(n)` duplicate suffix from
//! [`disambiguate_duplicates`](super::disambiguate_duplicates) can never
//! match by identity, because the real `AXDescription` never carries that
//! suffix — but five windows all called `Item-0` are still in five different
//! places, and a place is enough. It is the same fallback
//! [`crate::extras::find_extras_child`] already presses such an item with,
//! and the same tolerance, for the same reason: an element's `AXFrame` and
//! its window's bounds are measured by two different subsystems and disagree
//! by a few points. The alternative — a periodic re-capture for exactly
//! those items — is the polling this daemon is removing.
//!
//! A miss is silent and harmless, which is why both checks err toward
//! discarding. Marking an entity dirty only ever makes *that entity's own*
//! [`Captures::refresh`](super::Captures::refresh) run sooner; it never hands
//! one item's notification to another item's repaint, so a wrong or
//! ambiguous match costs an extra capture, never a wrong picture. A miss
//! costs a stale item until the next notification that does match.
//!
//! Marking dirty also wakes the run loop. Without that a posted entity would
//! sit there until some unrelated thing ran a pass, which on an idle bar is
//! never — a notification nobody acts on is not a push.
//!
//! # Creating an observer is off the main thread too
//!
//! Not only the callback. `PidObserver::create`'s own AX walk —
//! `ax::application`, `ax::extras_menu_bar` with a timeout, `ax::children`
//! and a notification registration per target — is a string of synchronous
//! cross-process Mach RPCs measured at ~29 ms per pid, and a moved or
//! added alias's [`PidObserver::resync`] repeats the registration half of
//! that. Both now run on [`crate::pool::sources`]'s own thread, in
//! [`PID_OBSERVERS`], reached through [`create_on_sources`] and
//! [`resync_on_sources`]. [`AxWatch::sync_one`] never waits on either: it
//! hands the pid to a task (see [`watch`]) and writes straight into
//! [`Context::watched`] — the same `Mutex` that thread reads when it gets
//! around to registering, so nothing asked for while a create or resync is
//! in flight is lost, only picked up a moment later than it would be
//! synchronously.

use bevy_ecs::entity::Entity;
use objc2_application_services::AXUIElement;
use objc2_core_foundation::{CFArray, CFRetained, CFRunLoop, CGRect, Type};
use skylight::ax::{self, Trusted, attribute};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, hash_map::Entry};
use std::sync::{Arc, Mutex, PoisonError};

/// What this module asks each target to report.
///
/// **This list is measured, not chosen.** `examples/ax_observer_probe.rs`
/// registers forty names and its own comment admits it never established
/// whether the breadth was doing any work; this module then shipped two
/// names with nothing recording why. Both are now settled, by registering
/// the probe's list live against the real config for 150 seconds (still
/// available as `COOLABAH_AX_WIDE=1`, see [`WIDE_NOTIFICATIONS`]) and counting
/// what actually arrived:
///
/// | notification | deliveries |
/// |---|---|
/// | `AXValueChanged` | 88 |
/// | `AXTitleChanged` | 3 |
/// | `AXRowCountChanged` | 2 |
/// | *the other 23 names* | 0 |
///
/// So the two this started with were right, and `AXRowCountChanged` is the
/// one addition the measurement earned. Registering all twenty-six costs the
/// same wall time (~29 ms per pid) but accepts thirteen times as many
/// registrations for traffic that never comes.
///
/// Re-run the measurement rather than editing this by eye: `COOLABAH_AX_WIDE=1`
/// with `COOLABAH_LOG=coolabah=trace`, then count `notification="..."` in the log.
const NOTIFICATIONS: &[&str] = &["AXValueChanged", "AXTitleChanged", "AXRowCountChanged"];

/// Everything `examples/ax_observer_probe.rs` asks for, for a run that is
/// trying to find out *which* notification a real item actually sends.
///
/// Behind `COOLABAH_AX_WIDE=1` rather than always on, because the answer is
/// meant to end up in [`NOTIFICATIONS`] as a measured list, not to be paid
/// for on every start-up: registering is a cross-process call per
/// (target, name) pair, and this list is twenty times longer.
const WIDE_NOTIFICATIONS: &[&str] = &[
    "AXValueChanged",
    "AXTitleChanged",
    "AXUIElementDestroyed",
    "AXCreated",
    "AXResized",
    "AXMoved",
    "AXLayoutChanged",
    "AXSelectedChildrenChanged",
    "AXSelectedChildrenMoved",
    "AXRowCountChanged",
    "AXMenuOpened",
    "AXMenuClosed",
    "AXMenuItemSelected",
    "AXElementBusyChanged",
    "AXAnnouncementRequested",
    "AXFocusedUIElementChanged",
    "AXApplicationActivated",
    "AXApplicationDeactivated",
    "AXApplicationHidden",
    "AXApplicationShown",
    "AXWindowCreated",
    "AXWindowMoved",
    "AXWindowResized",
    "AXHelpTagCreated",
    "AXLoadComplete",
    "AXUnitsChanged",
];

/// Which notification names to register. See [`WIDE_NOTIFICATIONS`].
fn notification_names() -> &'static [&'static str] {
    if std::env::var_os(name!(env "AX_WIDE")).is_some() {
        WIDE_NOTIFICATIONS
    } else {
        NOTIFICATIONS
    }
}

/// What identifies one watched item inside its owning application.
///
/// Both halves, because neither is enough on its own. The name is exact and
/// usually right; it can never match an item whose window title carries the
/// `(n)` duplicate suffix, since the real `AXDescription` never carries it.
/// The frame is what such an item does have that is unique — two windows
/// called `Item-0` sit in different places — and it is the same fallback
/// [`crate::extras::find_extras_child`] already uses to press one.
struct Watched {
    name: String,
    /// Where the window server last put this item's window. `None` until the
    /// alias has resolved one.
    bounds: Option<CGRect>,
}

/// What one owning application's observer needs at hand when a notification
/// arrives: who it was created for, and which entities under it are watching
/// for what.
///
/// `Send + Sync` on every field, with nothing to assert: `pid` is a plain
/// `i32`, the map is behind a `Mutex`, and `shared` is an `Arc` whose own
/// `Waker` carries the one `unsafe impl` this needs anywhere. That is what
/// lets [`AxWatch`] hold the same `Arc<Context>` the sources thread's
/// [`PidObserver`] does — see the module doc.
struct Context {
    pid: i32,
    watched: Mutex<HashMap<Entity, Watched>>,
    shared: Arc<Shared>,
}

/// Where a notified entity is posted, and how the main thread is told to
/// look.
///
/// No demultiplexing left to do here: the per-element refcon already names
/// which entity a notification was about (see the module doc), so posting
/// straight to a [`skylight::callback::Relay`] is the entity's own stream,
/// not a shared set someone sorts out later — [`AxWatch`] holds the other
/// end, [`skylight::callback::Events`], directly.
///
/// The waker arrives at construction rather than at `poll`, which is the wrong
/// way round for a future and the right way round for this: what consumes it is
/// a bevy run condition, and a schedule has no `poll` to be handed a waker in.
/// When that consumer becomes a future, this type and the waker with it go
/// away.
struct Shared {
    relay: Arc<skylight::callback::Relay<Entity>>,
    /// A notification is only half of a push: something has to run the
    /// schedule that acts on it. Without this a posted entity would sit
    /// there until the next unrelated wake, which on an idle bar is never.
    wake: crate::runloop::Waker,
}

impl Shared {
    fn mark(&self, entities: impl IntoIterator<Item = Entity>) {
        let mut posted = 0;
        for entity in entities {
            posted += usize::from(self.relay.post(entity));
        }
        tracing::debug!(posted, "posted a dirty alias; waking the run loop");
        self.wake.wake();
    }
}

thread_local! {
    /// Every pid currently being watched, keyed by pid — the `!Send` half of
    /// the arrangement. Touched only from [`crate::pool::sources`]'s own
    /// thread, through [`create_on_sources`], [`resync_on_sources`] and
    /// [`drop_on_sources`].
    static PID_OBSERVERS: RefCell<HashMap<i32, PidObserver>> = RefCell::new(HashMap::new());
}

/// One pid's live registration.
///
/// Dropping this drops the [`AXObserver`], which — per its own documented
/// contract — removes its run loop source automatically; nothing else here
/// needs to.
struct PidObserver {
    // Declared first so it drops first: the source comes off the run loop
    // before the observer that produced it is released.
    scheduled_source: crate::runloop::Undo,
    // Holds the context the callback is handed, and deregisters every
    // notification it took as it goes.
    observer: ax::Observer<Arc<Context>, Option<Entity>>,
    /// Every element [`register_targets`] matched against, kept so
    /// [`PidObserver::resync`] can redo the match without re-walking the AX
    /// tree — only the targets' own attributes are re-read, not their
    /// parentage.
    targets: Vec<CFRetained<AXUIElement>>,
}

impl PidObserver {
    /// Registers an observer for `pid`, matching every target against
    /// whatever `context.watched` already holds.
    ///
    /// Always called on [`crate::pool::sources`]'s own thread — see
    /// [`create_on_sources`], the only caller. This method itself has no
    /// opinion about that; it is just never asked to run anywhere else
    /// again, which is the whole point: this AX walk plus a notification
    /// registration per target is measured at ~29 ms per pid, all
    /// synchronous cross-process Mach RPCs, and none of it belongs on the
    /// thread that draws.
    ///
    /// `context.watched` is read fresh, here, rather than trusted from
    /// whenever the caller built it — an entity added to the same pid while
    /// this was still queued (see [`AxWatch::sync_one`]'s `Occupied` arm,
    /// reached once this pid already has an entry) lands in the same
    /// `Mutex` and is matched on this, the very first registration, with no
    /// separate resync needed.
    ///
    /// # Errors
    ///
    /// [`skylight::Error::NotTrusted`] without the Accessibility grant, which
    /// is a fact about this process and not about `pid` — a caller must not
    /// blame the application for it. [`skylight::Error::CreateObserver`] and
    /// [`skylight::Error::Notification`] are the application's own refusals,
    /// and mean this pid has to fall back to the poll.
    fn create(pid: i32, context: Arc<Context>, here: &CFRunLoop) -> skylight::Result<Self> {
        let access = Trusted::get()?;
        let mut observer = ax::Observer::create(access, pid, context, on_notification)?;

        let began = std::time::Instant::now();
        let names = notification_names();

        // The application element, the extras bar, and every status item
        // hanging off it. All three, because an AX notification does not
        // reliably travel from a descendant up to the application: a status
        // item is a child -- or, for a `BentoBox`-hosted module, a
        // grandchild -- of `AXExtrasMenuBar`, and registering only on the
        // application is why this heard nothing. Measured live before the
        // change: 127 notifications over two minutes, not one of them from a
        // watched item; `examples/ax_observer_probe.rs`, which registers on
        // all of these, does receive them from the same items.
        let app = ax::application(access, pid);
        let extras = ax::extras_menu_bar(access, pid, crate::extras::AX_TIMEOUT);
        let mut targets: Vec<CFRetained<AXUIElement>> = vec![app];
        if let Some(extras) = extras {
            if let Some(children) = ax::children(&extras) {
                collect_targets(&children, NEST, &mut targets);
            }
            targets.push(extras);
        }

        let refused = register_targets(&mut observer, &targets, names);
        if observer.watching() == 0 {
            return Err(refused.unwrap_or(skylight::Error::NoMenuBar));
        }

        let scheduled = crate::runloop::scheduled(here, observer.run_loop_source());
        tracing::debug!(
            pid,
            targets = targets.len(),
            names = names.len(),
            registered = observer.watching(),
            took = ?began.elapsed(),
            "registered an AX observer for aliased items owned by this pid"
        );
        Ok(Self {
            scheduled_source: scheduled,
            observer,
            targets,
        })
    }

    /// Redoes the registration-time match for every already-known target,
    /// against the watched set as it stands right now.
    ///
    /// **The one real risk of matching at registration instead of at
    /// delivery**: a match baked into a refcon can go stale where the old
    /// per-notification match could not, because that one re-read `watched`
    /// fresh every time. Two things go stale — an alias that moves (its
    /// `bounds` changes) and a second alias added under a pid this observer
    /// already watches (a target that matched nothing, or matched the wrong
    /// entity, at the first alias's registration) — and [`AxWatch::sync_one`]
    /// asks for this, through [`resync_on_sources`], exactly when either
    /// happens.
    ///
    /// Rebuilds the observer rather than patching individual registrations:
    /// `AXObserverAddNotification` has no way to change an already-registered
    /// refcon, so a changed match needs a remove-then-add regardless, and
    /// there is no cheaper way to know which targets are affected than to
    /// re-read them all. What this *does* reuse is `self.targets` — the AX
    /// tree itself is not walked again, only each target's own attributes —
    /// and `self.observer`'s own `Arc<Context>`, the exact allocation
    /// [`AxWatch`] writes `watched` into, so there is nothing to clone.
    ///
    /// Leaves the old observer in place on any failure -- a stale match is
    /// recoverable at the next real change; an application that stops
    /// answering mid-resync is not a reason to stop watching it.
    fn resync(&mut self, here: &CFRunLoop) {
        let Ok(access) = Trusted::get() else {
            return;
        };
        let context = Arc::clone(self.observer.context());
        let pid = context.pid;
        let Ok(mut observer) = ax::Observer::create(access, pid, context, on_notification) else {
            return;
        };
        register_targets(&mut observer, &self.targets, notification_names());
        if observer.watching() == 0 {
            return;
        }
        let scheduled = crate::runloop::scheduled(here, observer.run_loop_source());
        tracing::debug!(
            pid,
            targets = self.targets.len(),
            registered = observer.watching(),
            "re-matched an AX observer's targets after a watched alias moved"
        );
        self.scheduled_source = scheduled;
        self.observer = observer;
    }
}

/// Builds and registers one pid's [`PidObserver`], on
/// [`crate::pool::sources`]'s own thread — see [`watch`], the only caller.
///
/// Self-corrects against the one race this shape admits: `context.watched`
/// may be empty by the time this runs, because the entity that made the pid
/// worth watching was forgotten before creation finished (`AxWatch::detach`
/// writes straight into the same `Mutex`, with nothing to wait on). Rather
/// than have `detach` coordinate with a create it did not ask for, a
/// newly-built observer over an empty set is simply dropped instead of kept
/// — [`drop_on_sources`] would have removed it a moment later regardless.
fn create_on_sources(
    pid: i32,
    context: &Arc<Context>,
    here: &CFRunLoop,
) -> Result<(), skylight::Error> {
    let observer = PidObserver::create(pid, Arc::clone(context), here)?;
    if context
        .watched
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_empty()
    {
        return Ok(());
    }
    PID_OBSERVERS.with_borrow_mut(|observers| {
        observers.insert(pid, observer);
    });
    Ok(())
}

/// Re-matches one pid's observer, on [`crate::pool::sources`]'s own thread.
///
/// A pid with nothing in [`PID_OBSERVERS`] is a no-op rather than an error.
/// [`watch`]'s task only ever asks for this after its own create finished
/// (the two share one `Loop`, which drains its errands in order, so a
/// resync this task queues cannot run ahead of the insert its own create
/// made), so the case this actually guards is a race with
/// [`AxWatch::detach`]'s [`drop_on_sources`] -- a resync already queued when
/// the last entity for this pid was forgotten.
fn resync_on_sources(pid: i32, here: &CFRunLoop) {
    PID_OBSERVERS.with_borrow_mut(|observers| {
        if let Some(observer) = observers.get_mut(&pid) {
            observer.resync(here);
        }
    });
}

/// Drops one pid's observer, if it has one, on
/// [`crate::pool::sources`]'s own thread.
fn drop_on_sources(pid: i32) {
    PID_OBSERVERS.with_borrow_mut(|observers| {
        observers.remove(&pid);
    });
}

/// Which entity (if any) `target` is, against the given `watched` set. Reads
/// `target`'s own attributes — four `AXUIElementCopyAttributeValue` RPCs,
/// this is the cost `on_notification` used to pay on every delivery, now
/// paid once per element, here — **before** taking `watched`'s lock, and
/// deliberately: that `Mutex` is the one [`AxWatch::sync_one`] and
/// [`AxWatch::detach`] lock from the main thread on their hot, settled
/// path, and holding it across a handful of cross-process calls would just
/// relocate the stall this module exists to remove into lock contention.
/// See [`named_item`] for why the role only guards a name match rather than
/// gating the whole thing.
fn match_target(target: &AXUIElement, watched: &Mutex<HashMap<Entity, Watched>>) -> Option<Entity> {
    let role = attribute::<String>(target, "AXRole");
    let frame = ax::frame(target);
    let description = attribute::<String>(target, "AXDescription");
    let title = attribute::<String>(target, "AXTitle");
    let identity = [description.as_deref(), title.as_deref()];
    let by_name = named_item(role.as_deref());

    let watched = watched.lock().unwrap_or_else(PoisonError::into_inner);
    watched.iter().find_map(|(&entity, item)| {
        (near(frame, item.bounds) || (by_name && identity.contains(&Some(item.name.as_str()))))
            .then_some(entity)
    })
}

/// Matches every target against `observer`'s current watched set and
/// registers each one, under every name, tagged with what it was found to
/// be — see [`match_target`]. `None` targets still get registered: a
/// notification from one is what drives [`on_notification`]'s pid-wide
/// fallback, which is only reachable if the element that fired is itself
/// being watched.
///
/// Answers the last refusal, if any, so a caller with nothing registered at
/// all can report why.
fn register_targets(
    observer: &mut ax::Observer<Arc<Context>, Option<Entity>>,
    targets: &[CFRetained<AXUIElement>],
    names: &[&str],
) -> Option<skylight::Error> {
    // Cloned once, up front, so `observer` is free to be borrowed mutably by
    // `watch_matched` below -- an owned `Arc` rather than a borrow through
    // `observer.context()` held across the loop.
    let context = Arc::clone(observer.context());
    let mut refused = None;
    for target in targets {
        let matched = match_target(target, &context.watched);
        for name in names {
            if let Err(err) = observer.watch_matched(target, name, matched) {
                refused = Some(err);
            }
        }
    }
    refused
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "the shape `ax::Observer` calls back with"
)]
fn on_notification(notified: ax::Notified<'_, Arc<Context>, Option<Entity>>) {
    let ax::Notified {
        notification,
        context,
        matched,
        ..
    } = notified;

    if let Some(entity) = *matched {
        tracing::info!(
            pid = context.pid,
            %notification,
            ?entity,
            "an AX notification matched an aliased item; marking it dirty"
        );
        context.shared.mark([entity]);
        return;
    }

    // This element was matched to nothing at registration, but the
    // notification still identifies an *application*, and this observer
    // exists only because that application owns aliases someone is drawing.
    // That is enough to re-read all of them.
    //
    // Measured, not assumed. Amphetamine sends 198 notifications over 100
    // seconds and not one comes from its status item: 87 are
    // `AXValueChanged` on the `AXStaticText` of its popover countdown at
    // (559, 420), the rest its menu, its text field and the application
    // element. Registering more names does not help -- the wide set catches
    // seven and none arrives from menu bar height. The element is silent
    // while the icon it draws changes every minute, so an element-precise
    // filter leaves that alias stale forever.
    //
    // Unresolved aliases fall back the same way, and are the case that
    // matters most: one whose first capture lost a race with its
    // application publishing the window has no bounds, so the frame check
    // can never match it at registration, and its element's title is its
    // own live state rather than its name, so the name check cannot either.
    // Re-resolving is exactly what a capture does.
    //
    // What keeps this from being a firehose is what happens after rather
    // than what is let through: marking dirty only schedules an entity's
    // own capture, and `Captures::refresh` compares the captured digest, so
    // a notification about something unrelated costs one capture and no
    // repaint at all.
    let entities: Vec<Entity> = context
        .watched
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .keys()
        .copied()
        .collect();
    if entities.is_empty() {
        tracing::trace!(
            pid = context.pid,
            %notification,
            "an AX notification arrived for a pid with no aliases at all"
        );
        return;
    }
    tracing::debug!(
        pid = context.pid,
        %notification,
        aliases = entities.len(),
        "an AX notification named no watched element; re-reading this pid's aliases"
    );
    context.shared.mark(entities);
}

/// How far below `AXExtrasMenuBar` a status item can be and still be
/// registered on directly.
///
/// One level, for the same reason [`crate::extras`] descends one: Control
/// Centre groups Wi-Fi, Bluetooth and friends into a single `BentoBox`
/// element whose modules are *grandchildren* of the extras bar, and those
/// are exactly the items a config aliases as `Item-0(n)`.
const NEST: u32 = 1;

/// Collects the elements to register on: each child, and one level of
/// grandchildren.
fn collect_targets(children: &CFArray, depth: u32, out: &mut Vec<CFRetained<AXUIElement>>) {
    for i in 0..children.count() {
        let Some(child) = ax::array_element(children, i) else {
            continue;
        };
        out.push(child.retain());
        if depth > 0
            && let Some(grandchildren) = ax::children(child)
        {
            collect_targets(&grandchildren, depth - 1, out);
        }
    }
}

/// Whether an element's role is one a *name* match can be trusted on.
///
/// The two roles a real status item reports, measured live: an
/// `AXExtrasMenuBar` child is `AXMenuBarItem` — for Control Centre's own
/// hosted modules and for a separate process's status item alike — while the
/// descendant a Control Centre-hosted item's value change actually lands on
/// is `AXButton` instead.
///
/// This used to be a gate in front of the whole match, and that was wrong.
/// Measured over 182 live notifications with the real config: **86 were
/// discarded here**, every one of them from a pid that had a visible,
/// watched alias, and not one of them ever reached the frame check that
/// could have identified it. The role is a weak signal — this module's own
/// doc already records that several real items do not report `AXButton` —
/// while an element sitting within [`FRAME_MATCH_RADIUS`] of a watched
/// item's menu bar window, in that item's own process, *is* that item
/// whatever it calls itself.
///
/// So the frame decides on its own, and the role only guards a *name* match,
/// which is where it earns its place: a label inside a still-mounted popover
/// can carry the same `AXDescription` as the item it belongs to, and nothing
/// but the role tells those two apart. A popover cannot collide by frame —
/// it hangs below the menu bar, far outside the radius.
fn named_item(role: Option<&str>) -> bool {
    matches!(role, Some("AXButton" | "AXMenuBarItem"))
}

/// How far the notified element's own frame may sit from the alias's window
/// and still be taken for the same item.
///
/// The same tolerance [`crate::extras::find_extras_child`] presses with, and
/// for the same reason: an `AXExtrasMenuBar` element's frame and its window's
/// bounds are measured by two different subsystems and disagree by a few
/// points. Wrong here is cheap — a spurious re-capture of one item, never a
/// wrong picture — and right here is the only thing a `(n)`-suffixed alias
/// has to go on.
const FRAME_MATCH_RADIUS: f64 = 20.0;

fn near(frame: Option<CGRect>, bounds: Option<CGRect>) -> bool {
    let (Some(frame), Some(bounds)) = (frame, bounds) else {
        return false;
    };
    crate::alias::point_distance(
        crate::alias::rect_center(frame),
        crate::alias::rect_center(bounds),
    ) <= FRAME_MATCH_RADIUS
}

/// One pid's registration from [`AxWatch`]'s own side.
///
/// Owns nothing `!Send` — the `AXObserver` and everything under it live on
/// the sources thread, in [`PID_OBSERVERS`]. What this holds is the shared
/// [`Context`] (the same allocation that thread's [`PidObserver`] reads, so
/// [`AxWatch::sync_one`] and [`AxWatch::detach`] can lock `watched` directly
/// with no cross-thread hop for the common, settled path) and the means to
/// ask for more.
struct Watching {
    context: Arc<Context>,
    /// Wakes [`watch`]'s resync loop. Never touched before creation
    /// finishes — [`AxWatch::sync_one`] only reaches a `Watching` once its
    /// pid already has an entry, and the create still in flight reads
    /// `watched` fresh when it registers (see [`create_on_sources`]), so a
    /// change made before that happens is picked up there instead.
    resync: Arc<tokio::sync::Notify>,
    /// Dropping this — which only happens once the last entity for this pid
    /// is forgotten — aborts whichever of create or the resync loop is
    /// running. The sources-thread work already queued when that happens
    /// still runs to completion; it is [`create_on_sources`]'s own
    /// emptiness check and [`AxWatch::detach`]'s explicit
    /// [`drop_on_sources`] that make that harmless rather than this abort.
    _task: crate::pool::Task,
}

/// One pid's create having failed, crossing back from
/// [`crate::pool::sources`]'s thread to [`AxWatch::apply_results`].
///
/// Only ever sent for a real failure — a successful create has nothing to
/// report, since [`AxWatch`] already wrote what it knows into the shared
/// [`Context`] before asking, and a resync never fails outward (see
/// [`PidObserver::resync`]'s own doc).
struct PidResult {
    pid: i32,
    failed: skylight::Error,
}

/// Spawns the task that owns one pid's whole lifecycle: create, then answer
/// every resync request until dropped.
///
/// One persistent task rather than one spawned per event, so a second entity
/// arriving under a pid that is still being created can never race it with a
/// competing resync — `sync_one` only ever has to say "again", through
/// `resync`, never "start over".
fn watch(pid: i32, context: Arc<Context>, results: crate::pool::Sender<PidResult>) -> Watching {
    let resync = Arc::new(tokio::sync::Notify::new());
    let woken = Arc::clone(&resync);
    let kept = Arc::clone(&context);
    let task = crate::pool::owned(async move {
        let created = crate::pool::sources()
            .call(move |here| create_on_sources(pid, &context, here))
            .await;
        match created {
            Ok(Ok(())) => loop {
                woken.notified().await;
                let _ = crate::pool::sources()
                    .call(move |here| resync_on_sources(pid, here))
                    .await;
            },
            Ok(Err(failed)) => results.send(PidResult { pid, failed }),
            // The sources loop is gone, which only happens as the process
            // is shutting down -- nothing left to tell.
            Err(crate::runloop::Stopped) => {}
        }
    });
    Watching {
        context: kept,
        resync,
        _task: task,
    }
}

/// Every aliased entity's live AX registration, by the pid it currently
/// believes owns it.
pub struct AxWatch {
    watching: HashMap<i32, Watching>,
    entity_pid: HashMap<Entity, i32>,
    /// Pids that already refused a registration, so a config with an item
    /// this module cannot observe does not retry `AXObserverCreate` every
    /// time it is looked at.
    failed: HashSet<i32>,
    /// Where a create's failure lands, since the task that finds out is
    /// never the one holding `self`. Drained opportunistically, at the top
    /// of [`AxWatch::sync_one`] — the frequent, `&mut self` call every
    /// aliased entity already makes each pass, so a failure lands in
    /// [`AxWatch::failed`] within a pass or two of happening rather than
    /// needing a dedicated poll.
    results: crate::pool::Handoff<PidResult>,
    /// The receiving end of every `Context`'s [`Shared::relay`] — one stream
    /// for the whole config, since that is what a bevy run condition and
    /// [`AxWatch::take_dirty_set`] both want to drain from, but no demux:
    /// each item posted to it is already the one entity a notification named.
    dirty: skylight::callback::Events<Entity>,
    shared: Arc<Shared>,
}

impl AxWatch {
    /// The waker is what turns a notification into a pass; see [`Shared`].
    #[must_use]
    pub fn new(wake: crate::runloop::Waker) -> Self {
        let (relay, dirty) = skylight::callback::relay::<Entity>();
        Self {
            watching: HashMap::new(),
            entity_pid: HashMap::new(),
            failed: HashSet::new(),
            results: crate::pool::Handoff::new(wake.clone()),
            dirty,
            shared: Arc::new(Shared { relay, wake }),
        }
    }

    /// Whether any notification has matched since the last
    /// [`AxWatch::take_dirty_set`] — no lock, for a run condition.
    #[must_use]
    pub fn any_dirty(&self) -> bool {
        !self.dirty.is_empty()
    }

    /// Every entity a notification has named since the last look.
    ///
    /// The point of handing back *which* entities rather than answering
    /// "something changed": a notification is about one status item, and the
    /// only thing that should be re-captured because of it is that item. The
    /// alternative — walk every alias and ask each whether it was the one —
    /// makes a single clock tick cost every alias in the config.
    ///
    /// A duplicate here — the same entity posted twice before this drains —
    /// costs nothing: `Captor`'s own queue is a set for exactly this reason.
    pub fn take_dirty_set(&mut self) -> Vec<Entity> {
        let mut taken = Vec::new();
        while let Some(entity) = self.dirty.ready() {
            taken.push(entity);
        }
        taken
    }

    /// Applies whatever [`watch`]'s tasks have reported failed since the
    /// last look. See [`AxWatch::results`].
    fn apply_results(&mut self) {
        if !self.results.ready() {
            return;
        }
        for PidResult { pid, failed } in self.results.take() {
            self.watching.remove(&pid);
            // A missing grant is about this process, not this pid, so it
            // does not condemn the application to the poll forever: the
            // grant can arrive later, and the next sync retries.
            if !matches!(failed, skylight::Error::NotTrusted) {
                self.failed.insert(pid);
            }
            self.entity_pid.retain(|_, owner| *owner != pid);
            tracing::debug!(pid, %failed, "falling back to the poll for this application");
        }
    }

    /// Keeps one entity's registration current: which pid it should be
    /// watched under, and what name identifies it there.
    ///
    /// `pid` is `None` until the alias has resolved a window at least once —
    /// there is nothing to observe before that, so this is a no-op.
    ///
    /// A fresh pid seeds a [`Context`] with this one entity and hands it to
    /// [`watch`], which creates the observer asynchronously (see the module
    /// doc); an existing pid gaining a new entity, or an existing entity's
    /// name or bounds actually changing, wakes that pid's resync loop. An
    /// unchanged sync — the common case, reached on every resolve pass for a
    /// settled alias — does neither: it is a lock, a comparison, and a
    /// store, all on `self.watching`'s own `Context`, no cross-thread hop.
    pub fn sync_one(
        &mut self,
        entity: Entity,
        name: &str,
        pid: Option<i32>,
        bounds: Option<CGRect>,
    ) {
        self.apply_results();

        let previous = self.entity_pid.get(&entity).copied();
        if previous != pid {
            if let Some(old) = previous {
                self.detach(entity, old);
            }
            if let Some(pid) = pid {
                self.entity_pid.insert(entity, pid);
            } else {
                self.entity_pid.remove(&entity);
            }
        }

        let Some(pid) = pid else { return };
        if self.failed.contains(&pid) {
            return;
        }

        match self.watching.entry(pid) {
            Entry::Vacant(slot) => {
                // Seeded, not empty: `entity` is why this pid is being
                // watched at all, and `create_on_sources` matches every
                // target against whatever is in this map when it runs. An
                // empty map here would leave every target unmatched even
                // for the one alias this observer exists for.
                let mut seed = HashMap::new();
                seed.insert(
                    entity,
                    Watched {
                        name: name.to_owned(),
                        bounds,
                    },
                );
                let context = Arc::new(Context {
                    pid,
                    watched: Mutex::new(seed),
                    shared: Arc::clone(&self.shared),
                });
                slot.insert(watch(pid, context, self.results.sender()));
            }
            Entry::Occupied(slot) => {
                let watching = slot.get();
                // Only when it actually differs: this is reached for every
                // alias that gets looked at, and waking a resync on an
                // unchanged sync would pay the per-element attribute reads
                // this module exists to avoid paying on every notification.
                // Exact equality is safe, not just convenient: `Alias::bounds`
                // is only ever reassigned alongside a fresh `window_id` (see
                // `Alias::resolve`), so the same window hands back the
                // identical `CGRect` every call -- there is no sub-point
                // jitter here to round away.
                let moved = {
                    let mut watched = watching
                        .context
                        .watched
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    match watched.get_mut(&entity) {
                        Some(held) if held.name == name => {
                            let moved = held.bounds != bounds;
                            held.bounds = bounds;
                            moved
                        }
                        _ => {
                            watched.insert(
                                entity,
                                Watched {
                                    name: name.to_owned(),
                                    bounds,
                                },
                            );
                            true
                        }
                    }
                };
                if moved {
                    watching.resync.notify_one();
                }
            }
        }
    }

    fn detach(&mut self, entity: Entity, pid: i32) {
        // Whatever this entity already posted to `self.dirty` is left to
        // drain on its own -- a queue cannot un-post one entry, and a stale
        // one costs nothing: `refresh_aliases` looks it up in the live
        // query, and an entity that left has nothing there to find.
        let Some(watching) = self.watching.get(&pid) else {
            return;
        };
        let empty = {
            let mut watched = watching
                .context
                .watched
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            watched.remove(&entity);
            watched.is_empty()
        };
        if empty {
            // Drops the task (aborting whichever of create or the resync
            // loop is running) and asks the sources thread to drop the
            // observer, if it ever got one -- see `drop_on_sources`.
            self.watching.remove(&pid);
            crate::pool::sources().run_on(move |_here| drop_on_sources(pid));
            return;
        }
        // Stale, not just imprecise: a target the last registration matched
        // to `entity` still carries that refcon, and `on_notification`
        // marks it dirty without ever consulting `watched` again. Harmless
        // while `entity` just moved to a different name here -- the write
        // above already fixed that -- but wrong if it left because its pid
        // changed or it was forgotten: this observer would keep marking an
        // entity it no longer has anything to do with. Only reached when
        // other entities remain, so this is bounded by removals, not by
        // notifications.
        watching.resync.notify_one();
    }

    /// Forgets everything about one entity, tearing down its owning
    /// observer too if nothing else still watches it.
    pub fn forget(&mut self, entity: Entity) {
        if let Some(pid) = self.entity_pid.remove(&entity) {
            self.detach(entity, pid);
        }
    }
}

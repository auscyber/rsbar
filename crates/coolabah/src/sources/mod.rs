//! Where events come from.
//!
//! Each system facility the bar reacts to — the workspace, the power source,
//! the audio device — is a [`Source`]. The [`Registry`] owns them as boxes, so
//! the daemon never has to know which framework is behind a given event.
//!
//! **Every source registers on the main thread, against the run loop the app is
//! already pumping.** There is no thread per source: it turned out none of them
//! wanted one.
//!
//! `NSWorkspace` and `CGDisplayRegisterReconfigurationCallback` *must* be
//! registered on the main thread — register elsewhere and the callback is
//! accepted and then silently never fires. `IOKit`'s power notification is a
//! `CFRunLoopSource`, which the main run loop serves as happily as any other.
//! And `CoreAudio` and `notify` each run a thread of their own and deliver from
//! it, so a run loop of ours did nothing for them at all.
//!
//! A callback that arrives on a framework's own thread is still fine: it hands
//! the event to an [`Emitter`], which is `Send`, and wakes the main run loop.
//!
//! **The invariant: an event is either delivered or still queued with a wake
//! pending — never dropped, and nothing ever spins or blocks waiting for it.**
//!
//! The channel is `tokio::sync::mpsc::unbounded`, which is waker-based: a
//! producer hands the value over and wakes whoever is waiting, rather than
//! parking a thread or being polled on a timer. Unbounded is the only shape
//! that satisfies all three halves of the invariant at once, and the producers
//! are what decide that. They are OS callbacks — a notification block, a
//! `CoreAudio` listener, an `IOKit` run loop source — which can neither await
//! nor afford to block, because a callback that blocks stalls the framework
//! that called it. A bounded `blocking_send` would park a callback thread; a
//! bounded `try_send` would drop on a full queue, which is data loss. Tokio
//! documents `UnboundedSender::send` as never requiring any form of waiting,
//! and therefore usable from sync and async code alike; the receiving half
//! registers a waker and returns `Pending`, so nothing spins either.
//!
//! Unbounded is not unwatched: a growing backlog is logged, see [`HIGH_WATER`],
//! and the drain itself is bounded per source, see [`DRAIN_BUDGET`].
//!
//! The channel is not only for sources. Some events have no framework behind
//! them and can only originate on the main thread — a click lands on a window
//! the main thread owns, and `--trigger` arrives through it — so [`Emitter`]
//! is deliberately a plain cloneable sink that the main thread holds one of
//! too. Everything downstream then sees one ordered stream of events, whatever
//! produced them.
//!
//! Sources start lazily. Observing audio has a real cost, and a bar whose
//! config never mentions `volume_changed` should not pay it, so the registry
//! starts a source the first time an item subscribes to something it provides.

pub mod accessibility;
pub mod brightness;
pub mod config;
pub mod displays;
pub mod media;
pub mod mouse;
pub mod notifications;
pub mod observers;
pub mod power;
pub mod spaces;
pub mod volume;
pub mod wifi;

use crate::protocol::{Event, Kind};
use bevy_ecs::entity::Entity;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// How deep a source's backlog may get before it is worth saying so. Not a
/// cap: nothing is refused or dropped here, and the events past it are
/// delivered like any others. A backlog this deep means a producer is running
/// faster than the daemon drains, which is a bug to see in the log rather than
/// one to hide by throwing events away.
const HIGH_WATER: usize = 256;

/// How many events one source hands over per drain before the next source gets
/// its turn.
///
/// The queue is unbounded; the *pass* is not. This is what keeps a burst from
/// holding the main thread, and what makes the visit order irrelevant to
/// fairness: no source can spend another's turn, and whatever is left over
/// re-signals so the next wake continues where this one stopped.
const DRAIN_BUDGET: usize = 32;

/// The ready-set bit for events this process produces itself — a click, or
/// `--trigger` — which have no source behind them.
const LOCAL_BIT: u64 = 1 << (u64::BITS - 1);

/// Names a source. Carried alongside every event it produces, so a stray
/// emission can be traced back to what made it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceId(pub &'static str);

impl std::fmt::Display for SourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// The end a source's callbacks write into.
///
/// Two halves, and both are necessary — see the module doc for why the queue
/// is unbounded. There is deliberately no async variant of
/// [`send`](Emitter::send): an unbounded send never has anything to wait for,
/// so the same call is correct from a callback and from a task.
///
/// The waker is what actually gets the event looked at. A channel send wakes a
/// *task*, and this daemon has no executor waiting on one: its runner is asleep
/// in `CFRunLoopRunInMode`, which only a run loop source can interrupt. Without
/// the wake, an event from a source thread would sit in the queue until the
/// next routine tick — up to a second of latency on a volume change. Signalling
/// the main run loop turns a send into "look at this now".
///
/// Sources that register on the main thread are already inside the run loop
/// when they fire, so waking is redundant there — but it is also free, and
/// making every producer identical is worth more than saving a signal.
#[derive(Clone)]
pub struct Emitter {
    queue: tokio::sync::mpsc::UnboundedSender<Event>,
    waker: crate::runloop::Waker,
    /// Which sources have something queued, shared by all of them.
    ready: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// This source's bit in that set.
    bit: u64,
}

impl Emitter {
    /// Queues an event and wakes the app to drain it.
    ///
    /// Never blocks, never drops, and never has to be awaited. The only way
    /// this does not deliver is a receiver that has gone — the source was torn
    /// down and a callback its framework will not let us unregister fired
    /// anyway — and then there is nobody to deliver to. That is the whole of
    /// the "is anyone still listening" question, answered by the channel
    /// itself rather than by a lock guarding a slot.
    pub fn send(&self, event: Event) {
        if let Err(err) = self.queue.send(event) {
            tracing::trace!(%err, "a source outlived its feed; nothing is listening");
            return;
        }
        // Says which source to look at, so waking does not mean asking every
        // one of them whether it was them.
        self.ready
            .fetch_or(self.bit, std::sync::atomic::Ordering::Release);
        self.waker.wake();
    }

    /// Whether anything is still draining what this writes into.
    ///
    /// For a source whose framework cannot unregister a callback — `spaces`'
    /// notify procs — where a closed channel is the only honest "stop
    /// listening" signal there is.
    #[must_use]
    pub fn is_listening(&self) -> bool {
        !self.queue.is_closed()
    }
}

impl std::fmt::Debug for Emitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Emitter")
    }
}

/// A running source: the events it produces, and what keeps it alive.
///
/// This is the unit the rest of the daemon deals in. It is identified, it is
/// pollable, and dropping it stops the source — rather than an install call
/// with a side effect and an opaque token to file away somewhere.
///
/// Dropping it does both halves of stopping, because it owns both: the
/// [`Task`](crate::pool::Task) goes, which deregisters — a source's future
/// owns whatever [`skylight::callback::Callback`] it registered, so dropping
/// the task drops that too — and the receiver goes, which closes the channel.
/// The second is what tells a callback that could not be deregistered —
/// `spaces`' `SkyLight` notify procs — that there is no longer anyone to
/// report to. No source has to be told to stop; the one owner dropping is the
/// whole of it.
pub struct Feed {
    id: SourceId,
    events: tokio::sync::mpsc::UnboundedReceiver<Event>,
    /// Kept so a source being adjusted writes into the same queue it already
    /// does, rather than being handed a second one nothing reads.
    emit: Emitter,
    /// Whether this source's backlog has already been reported. One line as a
    /// backlog forms, not one per event that follows it.
    warned: bool,
    /// Dropped on whichever thread it was built on, which is the thread that
    /// registered — see [`Source::run`]. `None` for [`Feed::manual`], which
    /// nothing registered.
    #[allow(dead_code, reason = "held only so dropping the feed drops it too")]
    task: Option<crate::pool::Task>,
}

impl Feed {
    /// Wraps a running source's task and its receiver.
    fn new(
        id: SourceId,
        events: tokio::sync::mpsc::UnboundedReceiver<Event>,
        emit: Emitter,
        task: Option<crate::pool::Task>,
    ) -> Self {
        Self {
            id,
            events,
            emit,
            warned: false,
            task,
        }
    }

    /// A feed nothing registered for — one the caller writes into directly.
    ///
    /// Some events have no framework behind them: a click lands on a window the
    /// main thread owns, and `--trigger` arrives over IPC. They join the same
    /// stream as everything else rather than being a special case downstream.
    #[must_use]
    pub fn manual(
        id: SourceId,
        waker: crate::runloop::Waker,
        ready: std::sync::Arc<std::sync::atomic::AtomicU64>,
        bit: u64,
    ) -> (Emitter, Self) {
        let (queue, events) = tokio::sync::mpsc::unbounded_channel();
        let emit = Emitter {
            queue,
            waker,
            ready,
            bit,
        };
        (emit.clone(), Self::new(id, events, emit, None))
    }

    #[must_use]
    pub fn id(&self) -> SourceId {
        self.id
    }

    /// Another handle on the sink this source's callbacks already write into.
    fn emitter(&self) -> Emitter {
        self.emit.clone()
    }

    /// Takes the next event if one is queued, without waiting.
    ///
    /// What the `CFRunLoop`-driven runner uses: it drains on each wake rather
    /// than awaiting.
    pub fn try_next(&mut self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    /// Whether anything is still queued — and, once per backlog, a line
    /// saying so when it has grown past [`HIGH_WATER`].
    ///
    /// Both halves of "queued with a wake pending": the caller re-signals on
    /// a `true`, and a backlog that keeps growing is visible rather than
    /// silent. The flag resets as the backlog clears, so a source that goes
    /// deep twice is reported twice.
    fn backlogged(&mut self) -> bool {
        let queued = self.events.len();
        if queued >= HIGH_WATER {
            if !self.warned {
                self.warned = true;
                tracing::warn!(
                    source = %self.id,
                    queued,
                    "an event source is producing faster than the daemon drains"
                );
            }
        } else {
            self.warned = false;
        }
        queued > 0
    }

    /// Waits for the next event, or `None` once the source has stopped.
    ///
    /// Unused by the current runner, but this is what makes a producer's send
    /// a wakeup rather than a write into a void — and it is the shape an
    /// executor would poll.
    pub async fn next(&mut self) -> Option<Event> {
        self.events.recv().await
    }
}

impl futures_lite::Stream for Feed {
    type Item = Event;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Event>> {
        self.events.poll_recv(cx)
    }
}

impl std::fmt::Debug for Feed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Feed")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// Why a source could not start.
///
/// Named causes rather than a formatted string, so a caller can tell a missing
/// config file from a framework refusing a registration without matching on
/// prose. Every one of these is structural — a source that fails this way fails
/// the same way every time, which is why nothing retries.
#[derive(Debug, thiserror::Error)]
pub enum Cause {
    /// The config file the watcher would follow does not exist.
    #[error("there is no config file")]
    NoConfigFile,
    /// A path with no parent to watch. Watching the directory rather than the
    /// file is deliberate — editors save by renaming over the original.
    #[error("{0} has no parent directory to watch")]
    NoParentDirectory(std::path::PathBuf),
    /// The filesystem watcher refused.
    #[error("could not watch the filesystem: {0}")]
    Watch(#[from] notify::Error),
    /// There is no audio output to observe.
    #[error("there is no default output device")]
    NoOutputDevice,
    /// `CoreAudio` refused a property listener.
    #[error("CoreAudio refused a listener (status {0})")]
    CoreAudio(i32),
    /// `CoreGraphics` refused a display reconfiguration callback.
    #[error("CoreGraphics refused the callback ({0:?})")]
    CoreGraphics(objc2_core_graphics::CGError),
    /// `IOKit` would not create the power notification source.
    #[error("IOKit refused a notification source")]
    IoKit,
    /// A source that must be on the main thread was started elsewhere.
    #[error("this source has to be registered on the main thread")]
    NotMainThread,
    /// `DisplayServicesCanChangeBrightness` says the display has no
    /// brightness control to observe.
    #[error("this display has no brightness control")]
    NoBrightnessControl,
    /// `SCDynamicStore` refused to create the store, set its notification
    /// keys, or hand back a run loop source.
    #[error("SCDynamicStore refused the registration")]
    DynamicStore,
    /// `MediaRemote`'s now-playing calls are entitlement-gated as of macOS
    /// 15.3 and silently produce nothing for a process without one.
    #[error("MediaRemote is entitlement-blocked on this OS")]
    MediaRemoteBlocked,
}

/// A source that could not start, and which one.
#[derive(Debug, thiserror::Error)]
#[error("could not start the {id} event source: {cause}")]
pub struct StartError {
    pub id: SourceId,
    #[source]
    pub cause: Cause,
}

impl StartError {
    #[must_use]
    pub fn new(id: SourceId, cause: Cause) -> Self {
        Self { id, cause }
    }
}

/// One system facility, turned into events.
pub trait Source: Send {
    /// Identifies this source in logs, errors and thread names.
    fn id(&self) -> SourceId;

    /// The events this source can produce. The registry matches a subscription
    /// against this to decide what to start.
    fn provides(&self) -> Vec<Kind>;

    /// Whether this has to run whether or not anything subscribed.
    ///
    /// Lazy by default, which is the answer for anything an item asks for: a
    /// bar that never mentions the volume should not be paying `CoreAudio` to
    /// watch it. Eager is for what the bar itself depends on — the displays it
    /// draws on, the config file it reloads from — where there is no
    /// subscription to wait for because the daemon is the subscriber. An eager
    /// source is asked for everything it provides.
    fn eager(&self) -> bool {
        false
    }

    /// Registers what it takes to produce `wanted`, and hands back the future
    /// that runs it.
    ///
    /// `wanted` is a non-empty subset of [`provides`](Source::provides): the
    /// events something is actually waiting for. Observing more than this is
    /// not free — four `NSWorkspace` observers for an item that only asked
    /// about the front application is three notifications nobody reads — so a
    /// source registers against `wanted` rather than against everything it is
    /// capable of. A source whose events all come off one facility is free to
    /// install it whole; that is its business, not the registry's.
    ///
    /// **The returned future owns its own registration.** Whatever platform
    /// teardown the future must run at the end — dropping a
    /// [`skylight::callback::Callback`], stopping a `notify` watcher — lives
    /// *inside* it, held across every `.await`, rather than beside it in a
    /// second value. Dropping the [`Task`](crate::pool::Task) this returns is
    /// therefore the whole of stopping a source: it deregisters and stops
    /// consuming in one motion, in the order the future's own drop glue says,
    /// with nothing left for the registry to hold separately.
    ///
    /// Where the future runs is decided by whether it is `Send`, not by
    /// policy: [`crate::pool::spawn`]/[`crate::pool::owned`] for one that is,
    /// [`crate::runloop::owned`] for one that must be on the main thread. A
    /// callback-only source with no stream to drain still returns a future —
    /// one that holds the registration and never resolves, so dropping the
    /// task is still what tears it down.
    ///
    /// Called again with a different set when demand changes, having dropped
    /// the previous [`Task`](crate::pool::Task) first. That is how a reload
    /// that adds a subscription gets the observer it needs: the registry does
    /// not ask a running source for more, it re-registers it against the new
    /// set — unless [`update`](Source::update) says it already adjusted in
    /// place.
    ///
    /// Called on the main thread, with the app's run loop current.
    ///
    /// # Errors
    ///
    /// Returns [`StartError`] if the underlying framework refuses. A source
    /// that cannot start is not asked again: these fail for structural reasons,
    /// not transient ones.
    fn run(
        &mut self,
        wanted: &BTreeSet<Kind>,
        cx: Registering,
    ) -> Result<crate::pool::Task, StartError>;

    /// Adjusts a running source to a new set of requests, in place.
    ///
    /// Called instead of tearing the source down and calling
    /// [`run`](Source::run) again, so a source holding one observer per event
    /// only touches the ones that came or went — the four `NSWorkspace`
    /// observers do not all get deregistered and reinstalled because a fifth
    /// item started asking about sleep. A source that does this signals its
    /// own running future — over a channel it kept a sender for — rather than
    /// reaching into a registration the registry hands back, because there is
    /// no such handle any more: the future owns itself.
    ///
    /// The default says it cannot, which is the honest answer for a source
    /// whose events all come off one facility: there is nothing to adjust,
    /// because every event it provides is already being produced. The registry
    /// then leaves the running future exactly as it is.
    ///
    /// # Errors
    ///
    /// Returns [`StartError`] if the change is refused, which stops the source.
    fn update(&mut self, wanted: &BTreeSet<Kind>, cx: &mut Registering) -> Result<(), StartError> {
        let _ = (wanted, cx);
        Ok(())
    }
}

/// The run loops a source can put a callback on, and which is which.
///
/// Two, because most sources do not need the main thread and should not be on
/// it: an `IOKit` or `SCDynamicStore` source is an ordinary `CoreFoundation`
/// source and fires on whatever loop something pumps, so putting it on the one
/// that composites the bar means every power event competes with a frame.
#[derive(Clone)]
pub struct Loops {
    /// Proof of the main thread, and its loop. Wanted by the few sources whose
    /// facility answers no other thread — Carbon's event dispatcher.
    main: skylight::MainThread,
}

impl Loops {
    /// The only loop this owns. The other one belongs to
    /// [`crate::pool`](crate::pool::run_loop), which is where a thread that
    /// hosts run loop sources belongs — see its module note.
    fn new(main: skylight::MainThread) -> Self {
        Self { main }
    }
}

/// What a source is handed when it registers.
///
/// A context rather than a bare [`Emitter`] because registering needs more than
/// a sink: it needs whatever this source shares with the others.
pub struct Registering {
    id: SourceId,
    emit: Emitter,
    /// Proof of the thread the registry runs on.
    ///
    /// A `Source` is a trait, so `run` cannot grow an argument — but the
    /// context it is handed can carry one. That is what lets a source that
    /// installs a CoreFoundation observer say so in the type system instead of
    /// checking the thread and refusing.
    ///
    /// Handed to every source, but **wanted by few**: see
    /// [`Registering::main_thread`].
    loops: Loops,
}

impl Registering {
    /// Proof that this is the thread the run loop turns on.
    ///
    /// For a source whose facility only talks to that thread — pointer events,
    /// window server notifications.
    #[must_use]
    pub fn main_thread(&self) -> &skylight::MainThread {
        &self.loops.main
    }

    /// Registers a window server notification for this source.
    ///
    /// The handler is an ordinary Rust function taking its own state and a
    /// [`skylight::ffi::Event`] — no `extern "C"`, no `void*`, and no cast:
    /// the context pointer is derived from `state` here and recovered as the
    /// same type on the way back. See [`skylight::ffi::NotifyProcedure`].
    ///
    /// # Errors
    ///
    /// [`StartError`] if the window server declines, so a `run` can
    /// return it as-is.
    ///
    /// The state goes in as an [`Arc`](std::sync::Arc); the window server holds
    /// only a weak reference to it, so the returned guard is what keeps the
    /// procedure live. **Dropping the guard makes it inert** — there is no call
    /// that takes a procedure back, so that is as close as the platform gets to
    /// deregistering, and it is safe to do while a callback is running. See
    /// [`skylight::register_notify`].
    ///
    /// A source that wants its procedure to outlive its registration keeps the
    /// guard inside its own running future; one that does not can drop it.
    pub fn notify<S: Send + Sync + 'static>(
        &self,
        proc: extern "C-unwind" fn(
            u32,
            *mut std::ffi::c_void,
            usize,
            *mut std::ffi::c_void,
            skylight::ConnectionId,
        ),
        event: u32,
        state: std::sync::Arc<S>,
    ) -> Result<skylight::callback::Callback<S>, StartError> {
        skylight::register_notify(proc, event, state).map_err(|err| match err {
            skylight::Error::Notify(status) => {
                StartError::new(self.id, Cause::CoreGraphics(status))
            }
            other => {
                tracing::warn!(%other, "unexpected error registering a notification");
                StartError::new(self.id, Cause::NotMainThread)
            }
        })
    }

    /// The sink this source's callbacks write into.
    #[must_use]
    pub fn emitter(&self) -> Emitter {
        self.emit.clone()
    }
}

/// Where an event is delivered.
///
/// One variant, kept as an enum: a future event scoped some other way — by
/// display, say — has somewhere to go without every call site changing shape.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Target {
    /// Wherever it happened, to whoever is watching.
    All,
}

/// What a scoped claim is bound to: the item holding it, and — for a claim on
/// the pointer over that item — the rectangle it was standing on when the
/// claim was taken.
///
/// The rectangle is part of the claim's *identity*, not a field hanging off
/// it. An item that moves therefore holds a different claim: the new
/// rectangle's claim is taken and the old one released down the same
/// refcounted path as every other claim, with nothing anywhere comparing where
/// the item used to be. See [`crate::tracking`], which is the only thing that
/// binds an area in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Watched {
    pub item: Entity,
    pub area: Option<crate::tracking::Area>,
}

impl Watched {
    /// An item's claim on an event, wherever it happens to be.
    #[must_use]
    pub fn item(item: Entity) -> Self {
        Self { item, area: None }
    }

    /// An item's claim on the pointer over one rectangle.
    #[must_use]
    pub fn over(item: Entity, area: crate::tracking::Area) -> Self {
        Self {
            item,
            area: Some(area),
        }
    }
}

/// A claim on one event, held by whatever depends on it.
///
/// Holding it is what keeps the source registered for that event, and it is
/// what puts the holder on the list an occurrence is delivered to — so wanting
/// something and receiving it cannot get out of step, and letting go stops
/// both.
///
/// Delivery is a push to exactly the holders, not something they poll for. The
/// consumers here are items serviced in one pass of the schedule, so a queue
/// each would be buffering nobody waits on, and finding the work would mean
/// walking every item on every tick to discover that nothing happened.
///
/// The claim *is* the reference count. Every holder of the same event shares
/// one [`Claim`], and the registry keeps only a [`Weak`](std::sync::Weak) to
/// it, so the event is wanted for exactly as long as a handle exists with no
/// counter to keep in step. A second holder clones — an atomic increment —
/// rather than allocating a second claim, and the last handle to go frees it
/// once.
///
/// Dropping only records the release. Deregistering has to happen on the main
/// thread with the run loop current, and a `Drop` can run anywhere, so the
/// registry settles it on the next pass.
#[derive(Debug)]
pub struct Watch {
    kind: Kind,
    owner: Entity,
    claim: std::sync::Arc<Claim>,
}

impl Watch {
    /// The event this is a claim on.
    #[must_use]
    pub fn kind(&self) -> &Kind {
        &self.kind
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.claim.forget(self.owner);
    }
}

/// The thing a [`Watch`] is a handle on: one event being wanted, and who
/// wants it.
///
/// Never held by the registry, only pointed at weakly. Its destructor running
/// is what "nothing wants this any more" means.
#[derive(Debug)]
struct Claim {
    /// Who an occurrence goes to. A `std::sync::Mutex` because it is touched
    /// from `Watch`'s destructor, which cannot await.
    dependents: std::sync::Mutex<Vec<Entity>>,
    /// Set when this changes, so the registry knows to look without walking
    /// every source on every pass.
    moved: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Bumped when this changes, for a reader that has to ask more than once.
    ///
    /// `moved` above is *taken* by the settle pass, which is right for the one
    /// consumer that acts on it and then has nothing left to do. The tracked
    /// areas projected out of the claim map are a second consumer, on its own
    /// schedule, and two readers racing for one flag would mean whichever
    /// asked first swallowed the other's news. A counter is read without
    /// clearing, so each keeps its own place in it.
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Claim {
    fn remember(&self, who: Entity) {
        if let Ok(mut dependents) = self.dependents.lock()
            && !dependents.contains(&who)
        {
            dependents.push(who);
        }
    }

    fn forget(&self, who: Entity) {
        if let Ok(mut dependents) = self.dependents.lock() {
            dependents.retain(|held| *held != who);
        }
    }

    /// Adds who depends on this to `into`, without allocating one to hand back.
    fn append_dependents(&self, into: &mut Vec<Entity>) {
        if let Ok(dependents) = self.dependents.lock() {
            into.extend_from_slice(&dependents);
        }
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.moved.store(true, std::sync::atomic::Ordering::Release);
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }
}

/// The claims currently alive, by event and target.
///
/// Keyed by `Kind<Watched>`, not the wire-shaped `Kind` a caller hands in —
/// [`take`](Claims::take) binds `who` into it first. An unscoped kind (most
/// of them) does not carry `who` at all, so every holder of, say,
/// `volume_changed` still lands on the one shared key; a scoped kind does
/// carry it, so item A's `mouse.entered` and item B's are different keys
/// with no `Target` needed to tell them apart — see `Target`'s own doc.
///
/// [`Watched`] carries a rectangle as well as an item, which is what makes an
/// item moving a change of *key*: its old claim has no holder left and dies,
/// its new one is taken, and [`areas`](Claims::areas) reads the whole set of
/// rectangles back off these keys rather than off a second list of them.
#[derive(Debug, Default)]
struct Claims {
    live: HashMap<(Kind<Watched>, Target), std::sync::Weak<Claim>>,
    moved: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Bumped by every claim taken and every claim that dies — see
    /// [`Claim::generation`].
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// What `generation` read as when the areas were last projected.
    projected: u64,
    areas: crate::tracking::Areas,
}

impl Claims {
    /// A handle on `kind` at `target` for `who`, sharing the one claim if it
    /// is already alive.
    fn take(&mut self, who: Watched, kind: &Kind, target: &Target) -> Watch {
        let scoped = kind.clone().map(|()| who);
        let key = (scoped, target.clone());
        if let Some(existing) = self.live.get(&key).and_then(std::sync::Weak::upgrade) {
            existing.remember(who.item);
            return Watch {
                kind: kind.clone(),
                owner: who.item,
                claim: existing,
            };
        }

        let claim = std::sync::Arc::new(Claim {
            dependents: std::sync::Mutex::new(vec![who.item]),
            moved: std::sync::Arc::clone(&self.moved),
            generation: std::sync::Arc::clone(&self.generation),
        });
        self.live.insert(key, std::sync::Arc::downgrade(&claim));
        self.mark();
        Watch {
            kind: kind.clone(),
            owner: who.item,
            claim,
        }
    }

    /// Everything that depends on this occurrence.
    ///
    /// `event.kind()` can only ever be the wire-shaped `Kind<()>` — a payload
    /// never says which item it happened to, that is decided by hit geometry
    /// — so a scoped kind's real, entity-bound claim can never be reached
    /// from here. [`Entity::PLACEHOLDER`] stands in for the entity such a
    /// lookup cannot supply; it is not, and can never become, a real claim's
    /// key, so this correctly finds nothing for a scoped kind rather than
    /// guessing at one. That is by design: `mouse.entered` and its siblings
    /// are delivered by direct hit-test dispatch (see `ecs.rs`), not through
    /// this map — the same way a click already was before this existed.
    fn dependents_into(&self, event: &Event, target: &Target, into: &mut Vec<Entity>) {
        into.clear();
        let kind = event.kind().map(|()| Watched::item(Entity::PLACEHOLDER));
        let key = (kind, target.clone());
        if let Some(claim) = self.live.get(&key).and_then(std::sync::Weak::upgrade) {
            claim.append_dependents(into);
        }
    }

    #[cfg(test)]
    fn dependents(&self, event: &Event, target: &Target) -> Vec<Entity> {
        let mut into = Vec::new();
        self.dependents_into(event, target, &mut into);
        into
    }

    fn mark(&self) {
        self.moved.store(true, std::sync::atomic::Ordering::Release);
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    /// The rectangles claimed right now, taken again if any claim has been
    /// taken or has died since the last ask.
    ///
    /// The projection *is* the answer to "which tracked areas are there" —
    /// there is no set of areas kept beside this map to disagree with it — and
    /// a pass where no claim moved reads one atomic and reports nothing
    /// changed.
    fn areas(&mut self) -> &crate::tracking::Areas {
        let Self {
            live,
            generation,
            projected,
            areas,
            ..
        } = self;
        let now = generation.load(std::sync::atomic::Ordering::Acquire);
        if now == *projected {
            areas.hold();
            return areas;
        }
        *projected = now;
        areas.refresh(
            live.iter()
                .filter(|(_, claim)| claim.strong_count() > 0)
                .filter_map(|((kind, _), _)| kind.item().and_then(|watched| watched.area)),
        );
        areas
    }

    /// Whether any claim was taken or died since the last ask.
    fn moved(&self) -> bool {
        self.moved.swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    /// The events still claimed, forgetting the ones whose claim has died.
    ///
    /// Erases what a scoped kind carries — the entity, and the rectangle a
    /// tracked one is bound over: a source registers against *which events*,
    /// not *whose* or *where* — the mouse source starts because someone,
    /// anyone, wants `mouse.entered`, not once per item that does, and an item
    /// moving changes no source's registration at all.
    fn wanted(&mut self) -> BTreeSet<Kind> {
        self.live.retain(|_, claim| claim.strong_count() > 0);
        self.live
            .keys()
            .map(|(kind, _)| kind.clone().map(|_| ()))
            .collect()
    }
}

/// Starts a source, giving back the feed it produces.
///
/// Registration happens here, on the caller's thread, which is the main one.
/// Whatever the source hands back is held inside the feed and dropped when the
/// feed is — deregistering on the same thread that registered.
///
/// # Errors
///
/// Returns [`StartError`] if the framework refuses.
fn start(
    source: &mut dyn Source,
    wanted: &BTreeSet<Kind>,
    waker: &crate::runloop::Waker,
    ready: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    bit: u64,
    loops: Loops,
) -> Result<Feed, StartError> {
    let id = source.id();
    let (queue, events) = tokio::sync::mpsc::unbounded_channel();
    let emit = Emitter {
        queue,
        waker: waker.clone(),
        ready: std::sync::Arc::clone(ready),
        bit,
    };
    let cx = Registering {
        id,
        emit: emit.clone(),
        loops,
    };
    let task = source.run(wanted, cx)?;
    Ok(Feed::new(id, events, emit, Some(task)))
}

/// Holds every source and starts them on demand.
///
/// Sources start lazily. Observing audio has a real cost, and a bar whose
/// config never mentions `volume_changed` should not pay it, so a source starts
/// the first time an item subscribes to something it provides.
pub struct Registry {
    /// Every source, by name. Keyed rather than listed so that starting one is
    /// a lookup, and so that a source is started once however many of the
    /// events it provides get asked for.
    sources: HashMap<SourceId, Registered>,
    /// Which sources produce a given event.
    ///
    /// Many-valued on purpose. A kind is not guaranteed a single provider —
    /// per-item pointer events will come from sources indexed by the entity
    /// they watch — so this maps to a list rather than pretending otherwise.
    providers: HashMap<Kind, Vec<SourceId>>,
    /// Every claim currently out, however many items hold a handle on each.
    claims: Claims,
    /// How many sources are marked stale — still registered, wanted by
    /// nothing, and one pass away from being taken down. Counted rather than
    /// looked for, so [`settle`](Registry::settle) can tell whether a pass has
    /// anything to sweep without walking every source to find out.
    stale: usize,
    /// The two run loops a source may register on. See [`Loops`].
    loops: Loops,
    /// The bridged half of [`events`](Registry::events), projected for the
    /// source that observes them. Never written directly — see
    /// [`publish_declared`](Registry::publish_declared).
    declared: notifications::Declared,
    /// Every event a config has declared, bridged or not, and whether the
    /// config being read right now still wants it.
    events: BTreeMap<crate::protocol::EventName, Declaration>,
    /// Which sources have something queued. A source sets its bit when it
    /// sends, so a wake says *which* one to look at rather than starting a
    /// walk of all of them — and [`drain`](Registry::drain) sets it again for
    /// a source it stopped short of, which is the "still queued with a wake
    /// pending" half of the invariant.
    ready: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Context the sources share, built on first ask.
    /// Events with no framework behind them — a click, or `--trigger`.
    manual: Feed,
    emit: Emitter,
    /// Handed to every source so its events wake the app rather than waiting
    /// for the next tick.
    waker: crate::runloop::Waker,
}

/// A custom event a config declared.
///
/// A declaration with no notification registers nothing — `--trigger` is its
/// only source — but it is still what makes the name deliberate rather than a
/// typo. The mark is what the config bracket sweeps against; see
/// [`Registry::begin_config`].
struct Declaration {
    /// Which distributed notification it is bridged from, if any.
    notification: Option<crate::protocol::NotificationName>,
    /// Whether the config being read right now has declared it again.
    liveness: Liveness,
}

/// Whether a running source is still wanted, or only waiting to be taken
/// down.
///
/// Also what marks a [`Declaration`] across a config bracket, where `Stale`
/// means "the config being read has not mentioned this" rather than "one pass
/// from being taken down". The mark and [`revive`](Liveness::revive) are the
/// shared half; [`evict`](Liveness::evict)'s grace pass is the source's own.
///
/// A source is not stopped the moment its last claim goes. An item that moves
/// gives up the claim keyed to where it was and takes one keyed to where it
/// is now, and the two need not land in the same pass — a plain reference
/// count reaches zero in between and tears down a registration that is about
/// to be asked for again. Installing a Carbon handler or a `CoreAudio`
/// listener costs real time, and doing it twice in quick succession is racy
/// rather than merely wasteful.
///
/// So the first pass that finds a source unwanted only marks it [`Stale`],
/// leaving it registered, and the next one takes it down; a claim taken in
/// between revives it in place, with its registration and whatever state that
/// holds untouched. This is the discipline the item world already uses for a
/// config reload — mark everything, then sweep whatever is still marked (see
/// [`Stale`](crate::components::Stale)) — applied to sources.
///
/// [`Stale`]: Liveness::Stale
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    /// Something claims at least one event it provides.
    Wanted,
    /// Nothing did, as of the last pass. Still registered, and one pass away
    /// from not being.
    Stale,
}

/// What a pass that finds a running source unwanted did about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Evicting {
    /// Newly unwanted. Left registered and marked, to be looked at again.
    Marked,
    /// Unwanted last pass too, so it came down this one.
    Swept,
    /// It was not running; there was nothing to take down.
    Idle,
}

impl Liveness {
    /// Moves a running source one pass closer to being taken down, saying
    /// what that pass did.
    fn evict(&mut self) -> Evicting {
        match *self {
            Self::Wanted => {
                *self = Self::Stale;
                Evicting::Marked
            }
            Self::Stale => {
                // Back to `Wanted`, which is what a source that is not running
                // reads as: the mark belongs to a registration, and this pass
                // is the end of one.
                *self = Self::Wanted;
                Evicting::Swept
            }
        }
    }

    /// Notes that something wants this again, answering whether that rescued
    /// a source the next pass would otherwise have taken down.
    fn revive(&mut self) -> bool {
        std::mem::replace(self, Self::Wanted) == Self::Stale
    }
}

/// A source, and its feed once it is running.
struct Registered {
    source: Box<dyn Source>,
    feed: Option<Feed>,
    /// Everything this source can produce.
    provides: BTreeSet<Kind>,
    /// What it is serving right now, so a change of demand is detectable.
    serving: BTreeSet<Kind>,
    /// Whether anything still wants it, or it is only registered until the
    /// next pass. See [`Liveness`].
    liveness: Liveness,
    /// Whether this source is eager — see [`Source::eager`].
    pinned: bool,
    /// A source that refused to start is not asked again. These fail for
    /// structural reasons — a missing config file, a framework saying no — so a
    /// second attempt fails identically.
    failed: bool,
    /// This source's bit in the ready set.
    bit: u64,
}

impl Registered {
    /// Everything still claimed that this source provides.
    fn demand(&self, claimed: &BTreeSet<Kind>) -> BTreeSet<Kind> {
        if self.pinned {
            return self.provides.clone();
        }
        self.provides.intersection(claimed).cloned().collect()
    }

    /// What this pass does about a source nothing wants any more.
    ///
    /// Marks a live one and leaves it registered; takes down one that was
    /// already marked. See [`Liveness`] for why there is a pass in between.
    fn evict(&mut self) -> Evicting {
        if self.feed.is_none() {
            return Evicting::Idle;
        }
        let outcome = self.liveness.evict();
        if outcome == Evicting::Swept {
            // Dropping the feed drops the registration, which deregisters —
            // on this thread, which is the one that registered — and the
            // receiver, which closes the channel and so silences whatever
            // could not be deregistered.

            self.feed = None;
            self.serving.clear();
        }
        outcome
    }
}

impl Registry {
    /// Builds the registry over the sources this build knows about.
    ///
    /// # Panics
    ///
    /// If this build has more event sources than there are bits in the ready
    /// set, which would make two of them indistinguishable.
    #[must_use]
    #[skylight::main_thread]
    pub fn new(config: crate::config::Shared, waker: crate::runloop::Waker) -> Self {
        let declared = notifications::Declared::default();
        let sources: Vec<Box<dyn Source>> = vec![
            Box::new(notifications::Notifications::new(declared.clone())),
            Box::new(displays::Displays),
            Box::new(config::Watcher { config }),
            Box::new(power::Power),
            Box::new(mouse::Mouse::default()),
            Box::new(volume::Volume),
            Box::new(brightness::Brightness),
            Box::new(wifi::Wifi),
            Box::new(spaces::Spaces),
            Box::new(media::Media),
            Box::new(accessibility::Accessibility),
        ];

        let mut providers: HashMap<Kind, Vec<SourceId>> = HashMap::new();
        let mut registered = HashMap::with_capacity(sources.len());
        // One bit each, and the top one for the events this process produces
        // itself. A build with more sources than bits would silently share
        // them, so it is worth failing here instead.
        assert!(
            sources.len() < u64::BITS as usize,
            "more event sources than bits in the ready set"
        );
        let ready = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        for (index, source) in sources.into_iter().enumerate() {
            let id = source.id();
            let provided: BTreeSet<Kind> = source.provides().into_iter().collect();
            for kind in &provided {
                providers.entry(kind.clone()).or_default().push(id);
            }
            registered.insert(
                id,
                Registered {
                    pinned: source.eager(),
                    provides: provided,
                    source,
                    feed: None,
                    serving: BTreeSet::new(),
                    liveness: Liveness::Wanted,
                    failed: false,
                    bit: 1 << index,
                },
            );
        }

        let (emit, manual) = Feed::manual(
            SourceId("local"),
            waker.clone(),
            std::sync::Arc::clone(&ready),
            LOCAL_BIT,
        );
        Self {
            loops: Loops::new(skylight::MainThread::of(&proof)),
            sources: registered,
            providers,
            declared,
            events: BTreeMap::new(),
            claims: Claims::default(),
            stale: 0,
            ready,
            manual,
            emit,
            waker,
        }
    }

    /// Declares a custom event, and bridges it when it names a notification.
    ///
    /// A declaration is not a subscription: nothing starts here unless
    /// something already claimed the event, and nothing needs to — the
    /// registry re-reconciles the notifications source so an item that
    /// subscribed before the declaration arrived gets its observer now. That
    /// order is the normal one for a config, which subscribes items as it
    /// builds them and may declare the event anywhere.
    ///
    /// Declaring one already declared clears its mark, which is what carries
    /// it across a config bracket — see [`begin_config`](Registry::begin_config).
    pub fn declare_event(
        &mut self,
        name: crate::protocol::EventName,
        notification: Option<crate::protocol::NotificationName>,
    ) {
        if let Some(notification) = &notification {
            tracing::debug!(
                %name, %notification, "declared a custom event bridged from a notification"
            );
        } else {
            tracing::debug!(%name, "declared a custom event");
        }
        self.events
            .entry(name)
            .and_modify(|declaration| {
                declaration.notification.clone_from(&notification);
                declaration.liveness.revive();
            })
            .or_insert(Declaration {
                notification,
                liveness: Liveness::Wanted,
            });
        self.republish();
    }

    /// Marks every declaration, so the config being read now says which it
    /// still wants by declaring it again.
    ///
    /// Paired with [`end_config`](Registry::end_config). The same
    /// mark-then-sweep `--begin`/`--end` already runs over items, and that
    /// [`Liveness`] already runs over sources — a third use of one discipline
    /// rather than a third mechanism.
    pub fn begin_config(&mut self) {
        for declaration in self.events.values_mut() {
            declaration.liveness = Liveness::Stale;
        }
    }

    /// Drops every declaration the config just read did not mention.
    ///
    /// The only way a declaration goes away: there is no `--remove event`. A
    /// config that renames an event would otherwise leave the old name
    /// advertised for the life of the daemon, and a bridged one would keep an
    /// entry that installs a real observer the moment anything claims it.
    pub fn end_config(&mut self) {
        let swept: Vec<crate::protocol::EventName> = self
            .events
            .iter()
            .filter(|(_, declaration)| declaration.liveness == Liveness::Stale)
            .map(|(name, _)| name.clone())
            .collect();
        if swept.is_empty() {
            return;
        }
        for name in swept {
            tracing::debug!(%name, "the config stopped declaring this custom event");
            self.events.remove(&name);
        }
        self.republish();
    }

    /// Pushes the declarations back out to everything that mirrors them.
    ///
    /// [`events`](Registry::events) is where a declaration lives; the shared
    /// [`Declared`](notifications::Declared) map, the source's `provides` and
    /// the `providers` index are all projections of it. Rebuilt through one
    /// call rather than patched at each site, because three statements that
    /// have to agree are three statements that can stop agreeing — which is
    /// what this used to be.
    fn republish(&mut self) {
        if let Ok(mut declared) = self.declared.lock() {
            declared.clear();
            declared.extend(self.events.iter().filter_map(|(name, declaration)| {
                declaration
                    .notification
                    .clone()
                    .map(|notification| (name.clone(), notification))
            }));
        }
        // Nothing starts here that was not already claimed: reconciling is how
        // an item that subscribed *before* the declaration arrived gets its
        // observer now, which is the normal order for a config.
        let id = SourceId("notifications");
        self.refresh_provides(id);
        self.reconcile(id);
    }

    /// Re-reads what a source serves and rebuilds both indexes from it.
    ///
    /// [`Source::provides`] is the truth — for the notifications source, the
    /// fixed rows plus whatever a config has declared — and both
    /// [`Registered::provides`] and [`providers`](Registry::providers) are
    /// caches of it. Caches worth keeping: `demand` intersects against
    /// `provides` for every source on every settle pass, and asking the source
    /// each time would mean a `Vec` allocation and a mutex per source per
    /// pass, for an answer that changes only when a config declares
    /// something.
    fn refresh_provides(&mut self, id: SourceId) {
        let Self {
            sources, providers, ..
        } = self;
        let Some(entry) = sources.get_mut(&id) else {
            return;
        };
        let now: BTreeSet<Kind> = entry.source.provides().into_iter().collect();
        let was = std::mem::replace(&mut entry.provides, now);
        let now = &entry.provides;
        for kind in was.difference(now) {
            if let Some(holders) = providers.get_mut(kind) {
                holders.retain(|held| *held != id);
                if holders.is_empty() {
                    providers.remove(kind);
                }
            }
        }
        for kind in now.difference(&was) {
            let holders = providers.entry(kind.clone()).or_default();
            if !holders.contains(&id) {
                holders.push(id);
            }
        }
    }

    /// Every custom event a config has declared, bridged or not.
    pub fn declared_events(&self) -> impl Iterator<Item = &crate::protocol::EventName> {
        self.events.keys()
    }

    /// Which sources produce this event, if any.
    ///
    /// The one answer to "who is behind this event", so nothing outside has to
    /// keep a second copy of the mapping to drift out of step with the source
    /// table — which changes whenever sources are regrouped.
    #[must_use]
    pub fn providers_of(&self, kind: &Kind) -> &[SourceId] {
        self.providers.get(kind).map_or(&[], Vec::as_slice)
    }

    /// Whether a source is currently registered with its framework.
    #[must_use]
    pub fn running(&self, id: SourceId) -> bool {
        self.sources.get(&id).is_some_and(|e| e.feed.is_some())
    }

    /// Exactly what a source is registered for right now.
    ///
    /// Empty when it is not running. This is what a subscription actually
    /// bought, as opposed to what the source is capable of.
    #[must_use]
    pub fn registered_for(&self, id: SourceId) -> BTreeSet<Kind> {
        self.sources
            .get(&id)
            .map(|e| e.serving.clone())
            .unwrap_or_default()
    }

    /// A sink for events this process observes outside any source.
    #[must_use]
    pub fn emitter(&self) -> Emitter {
        self.emit.clone()
    }

    /// Starts the sources that declared themselves eager.
    ///
    /// Everything else waits for an item to want it. Nothing releases these —
    /// they answer the daemon's own needs, and the daemon is always here.
    pub fn start_eager(&mut self) {
        let eager: Vec<SourceId> = self
            .sources
            .iter()
            .filter(|(_, entry)| entry.pinned)
            .map(|(id, _)| *id)
            .collect();
        for id in eager {
            self.reconcile(id);
        }
    }

    /// Records everything `who` needs, releasing whatever it needed before.
    ///
    /// This is the whole lifecycle: a source starts when the first item wants
    /// an event it provides, and stops when the last one stops wanting it. An
    /// item that drops `volume_changed` from its subscriptions stops the audio
    /// listener, and a config reload that removes the item entirely does too.
    /// Claims an event for as long as the returned [`Watch`] lives.
    ///
    /// Claiming an event already claimed hands back a handle on the same one
    /// rather than a second of it. The source is actually started, or stopped,
    /// by [`settle`](Registry::settle) on the next pass, because registering
    /// and deregistering have to happen on the main thread while a `Drop` can
    /// run anywhere.
    #[must_use]
    pub fn watch(&mut self, who: Entity, kind: &Kind) -> Watch {
        self.watch_at(who, kind, &Target::All)
    }

    /// [`watch`](Registry::watch), against an explicit [`Target`] rather than
    /// always [`Target::All`].
    ///
    /// A scoped kind — `mouse.entered` and its siblings — does not need this
    /// to reach one item and not another; the entity is already bound into
    /// the kind itself when the claim is taken. This exists for whatever a
    /// future event scopes some other way.
    #[must_use]
    pub fn watch_at(&mut self, who: Entity, kind: &Kind, target: &Target) -> Watch {
        if !self.providers.contains_key(kind) {
            tracing::debug!(%kind, "nothing provides this event");
        }
        self.claims.take(Watched::item(who), kind, target)
    }

    /// [`watch`](Registry::watch), over the rectangle `who` occupies.
    ///
    /// The rectangle is part of the claim, so an item that moves does not
    /// adjust anything: it takes the claim on where it is now, and the claim
    /// on where it was dies with the handle it replaced. Registering the new
    /// rectangle and dropping the old one is then the ordinary refcounted
    /// path, and [`tracked_areas`](Registry::tracked_areas) is where it
    /// surfaces.
    #[must_use]
    pub fn watch_area(&mut self, who: Entity, kind: &Kind, area: crate::tracking::Area) -> Watch {
        self.claims
            .take(Watched::over(who, area), kind, &Target::All)
    }

    /// The rectangles items have claimed the pointer over, and which displays'
    /// sets have moved since this was last asked.
    pub fn tracked_areas(&mut self) -> &crate::tracking::Areas {
        self.claims.areas()
    }

    /// Everything that depends on this occurrence.
    ///
    /// The whole routing decision, and it costs nothing when nobody is
    /// watching: the claim knows its holders, so there is no set of items to
    /// walk and no queue to poll. A pass where nothing arrived does no work.
    #[must_use]
    pub fn dependents(&self, event: &Event, target: &Target) -> Vec<Entity> {
        let mut into = Vec::new();
        self.dependents_into(event, target, &mut into);
        into
    }

    /// The same, into a buffer the caller keeps between events.
    pub fn dependents_into(&self, event: &Event, target: &Target, into: &mut Vec<Entity>) {
        self.claims.dependents_into(event, target, into);
    }

    /// Claims several events at once.
    pub fn watch_all(&mut self, who: Entity, kinds: impl IntoIterator<Item = Kind>) -> Vec<Watch> {
        kinds
            .into_iter()
            .map(|kind| self.watch(who, &kind))
            .collect()
    }

    /// Brings every source into line with the claims currently out.
    ///
    /// Cheap when nothing moved, which is almost every pass: a flag says
    /// whether any claim was taken or dropped since last time. A pass also
    /// runs while anything is stale, because a source marked last pass is
    /// taken down by this one and no claim need move for that to be due —
    /// see [`Liveness`].
    pub fn settle(&mut self) {
        // Asked first and unconditionally: reading the flag clears it, so a
        // pass that runs only to sweep must not leave a real change unseen.
        let moved = self.claims.moved();
        if !moved && self.stale == 0 {
            return;
        }
        let claimed = self.claims.wanted();
        let ids: Vec<SourceId> = self.sources.keys().copied().collect();
        for id in ids {
            self.reconcile_against(id, &claimed);
        }
    }

    /// Brings one source into line with what is now wanted from it.
    ///
    /// Three outcomes: nothing wants it and it stops, the set it is registered
    /// against is already right and it is left alone, or the set has changed
    /// and it is registered again against the new one. That last case is the
    /// point — it is how a reload that adds a subscription gets an observer
    /// for it, rather than relying on the source having quietly installed one
    /// nobody asked for.
    fn reconcile(&mut self, id: SourceId) {
        let claimed = self.claims.wanted();
        self.reconcile_against(id, &claimed);
    }

    fn reconcile_against(&mut self, id: SourceId, claimed: &BTreeSet<Kind>) {
        let Self {
            sources,
            waker,
            ready,
            stale,
            loops,
            ..
        } = self;
        let loops = loops.clone();
        let Some(entry) = sources.get_mut(&id) else {
            return;
        };
        let demand = entry.demand(claimed);

        if demand.is_empty() {
            match entry.evict() {
                Evicting::Marked => {
                    *stale += 1;
                    tracing::debug!(source = %id, "nothing wants this event source; holding it a pass");
                }
                Evicting::Swept => {
                    *stale = stale.saturating_sub(1);
                    tracing::debug!(source = %id, "stopped event source; nothing wants it");
                }
                Evicting::Idle => {}
            }
            return;
        }

        if entry.liveness.revive() {
            *stale = stale.saturating_sub(1);
            tracing::debug!(source = %id, "kept an event source that was a pass from stopping");
        }

        if entry.feed.is_some() && entry.serving == demand {
            return;
        }
        if entry.failed {
            return;
        }

        // Already running, only the request set moved: hand it the new set and
        // let it change what it needs to. Tearing it down would deregister
        // observers that were already right, and lose whatever state they had.
        if let Some(feed) = entry.feed.as_mut() {
            let mut cx = Registering {
                id,
                emit: feed.emitter(),
                loops: loops.clone(),
            };
            match entry.source.update(&demand, &mut cx) {
                Ok(()) => {
                    tracing::debug!(source = %id, wants = ?demand, "adjusted event source");
                    entry.serving = demand;
                }
                Err(err) => {
                    entry.failed = true;
                    entry.feed = None;
                    entry.serving.clear();
                    tracing::warn!(%err, "event source refused a change");
                }
            }
            return;
        }

        match start(
            entry.source.as_mut(),
            &demand,
            waker,
            ready,
            entry.bit,
            loops,
        ) {
            Ok(feed) => {
                tracing::debug!(source = %id, wants = ?demand, "registered event source");
                entry.feed = Some(feed);
                entry.serving = demand;
            }
            Err(err) => {
                entry.failed = true;
                entry.serving.clear();
                tracing::warn!(%err, "event source unavailable");
            }
        }
    }

    /// Takes what is queued across every signalled feed, without waiting.
    ///
    /// Bounded, deliberately: each source hands over at most [`DRAIN_BUDGET`]
    /// events and anything left re-signals its source and wakes the loop, so
    /// the pass returns to the run loop instead of following a burst wherever
    /// it goes. Nothing is lost by stopping early — the events are still
    /// queued, and the wake is already pending — and no source can spend
    /// another's turn, which is what makes the visit order a matter of
    /// ordering rather than of starvation.
    ///
    /// Each emission is tagged with the source that produced it, so an event
    /// arriving from somewhere unexpected is traceable rather than anonymous.
    pub fn drain(&mut self, mut on_event: impl FnMut(SourceId, Event)) {
        let Self {
            sources,
            manual,
            ready,
            waker,
            ..
        } = self;
        // Which sources actually have something. Taken once and cleared, so a
        // wake looks at whatever woke us rather than asking every source
        // whether it was them.
        let signalled = ready.swap(0, std::sync::atomic::Ordering::AcqRel);
        if signalled == 0 {
            return;
        }
        let feeds = sources
            .values_mut()
            .filter(|entry| entry.bit & signalled != 0)
            .filter_map(|entry| entry.feed.as_mut());
        let local = (signalled & LOCAL_BIT != 0).then_some(manual);
        // Which sources still have something after their turn. Re-signalled
        // together at the end, so one wake covers all of them.
        let mut left = 0;
        for feed in feeds.chain(local) {
            let id = feed.id();
            for _ in 0..DRAIN_BUDGET {
                let Some(event) = feed.try_next() else { break };
                // Handed straight on rather than collected. One list of
                // everything every source produced is an allocation per pass
                // for a consumer that only walks it once.
                on_event(id, event);
            }
            if feed.backlogged() {
                left |= feed.emit.bit;
            }
        }
        // Only on a real backlog. Re-signalling with nothing queued is the
        // spin this exists to avoid.
        if left != 0 {
            ready.fetch_or(left, std::sync::atomic::Ordering::Release);
            waker.wake();
        }
    }
}

#[cfg(test)]
mod claim_tests {
    use super::{Claims, Kind, Target, Watched};
    use crate::protocol::Event;
    use crate::protocol::event::SystemWoke;
    use crate::tracking::Area;
    use bevy_ecs::entity::Entity;
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use std::collections::BTreeSet;
    use std::sync::Arc;

    fn item(index: u32) -> Entity {
        Entity::from_raw_u32(index).expect("a valid entity index")
    }

    fn holder(index: u32) -> Watched {
        Watched::item(item(index))
    }

    /// An item at `x` on display 1, the width of a small item.
    fn at(index: u32, x: f64) -> Watched {
        Watched::over(
            item(index),
            Area::new(
                1,
                CGRect::new(CGPoint::new(x, 0.0), CGSize::new(40.0, 24.0)),
            ),
        )
    }

    #[test]
    fn a_second_claim_on_one_event_is_a_handle_on_the_first() {
        let mut claims = Claims::default();
        let first = claims.take(holder(1), &Kind::SystemWoke, &Target::All);
        let second = claims.take(holder(2), &Kind::SystemWoke, &Target::All);
        assert!(
            Arc::ptr_eq(&first.claim, &second.claim),
            "one claim, two handles — not two claims"
        );
    }

    #[test]
    fn an_event_stays_wanted_until_the_last_handle_goes() {
        let mut claims = Claims::default();
        let first = claims.take(holder(1), &Kind::SystemWoke, &Target::All);
        let second = claims.take(holder(2), &Kind::SystemWoke, &Target::All);

        drop(first);
        assert_eq!(claims.wanted(), BTreeSet::from([Kind::SystemWoke]));

        drop(second);
        assert!(claims.wanted().is_empty());
    }

    #[test]
    fn a_claim_taken_again_after_dying_is_a_fresh_one() {
        let mut claims = Claims::default();
        drop(claims.take(holder(1), &Kind::SystemWoke, &Target::All));
        assert!(claims.wanted().is_empty());

        let revived = claims.take(holder(1), &Kind::SystemWoke, &Target::All);
        assert_eq!(claims.wanted(), BTreeSet::from([Kind::SystemWoke]));
        drop(revived);
    }

    #[test]
    fn an_occurrence_goes_to_everything_that_depends_on_it() {
        let mut claims = Claims::default();
        let first = claims.take(holder(1), &Kind::SystemWoke, &Target::All);
        let second = claims.take(holder(2), &Kind::SystemWoke, &Target::All);

        // Order is not meaningful — dependents are a set, and `Entity` does
        // not order by index anyway.
        let woke = Event::SystemWoke(SystemWoke {});
        let both = claims.dependents(&woke, &Target::All);
        assert_eq!(both.len(), 2);
        assert!(both.contains(&item(1)) && both.contains(&item(2)));

        drop(first);
        assert_eq!(claims.dependents(&woke, &Target::All), vec![item(2)]);
        drop(second);
        assert!(claims.dependents(&woke, &Target::All).is_empty());
    }

    #[test]
    fn an_event_nobody_asked_for_reaches_nobody() {
        // The idle case: no claim, no dependents, no work — without walking
        // a single item to find that out.
        let claims = Claims::default();
        let woke = Event::SystemWoke(SystemWoke {});
        assert!(claims.dependents(&woke, &Target::All).is_empty());
    }

    #[test]
    fn two_items_claiming_the_same_scoped_kind_get_separate_claims() {
        // `mouse.clicked` is `@scoped` — the entity `take` was called with is
        // baked into the claim's key, so item 1's claim and item 2's are two
        // different `Claim`s sharing nothing, with no `Target` needed to
        // separate them.
        let mut claims = Claims::default();
        let mine = claims.take(holder(1), &Kind::MouseClicked(()), &Target::All);
        let theirs = claims.take(holder(2), &Kind::MouseClicked(()), &Target::All);
        assert!(
            !Arc::ptr_eq(&mine.claim, &theirs.claim),
            "a scoped kind must not share one item's claim with another's"
        );
    }

    #[test]
    fn a_scoped_kind_still_shares_one_claim_for_the_same_item() {
        let mut claims = Claims::default();
        let first = claims.take(holder(1), &Kind::MouseClicked(()), &Target::All);
        let second = claims.take(holder(1), &Kind::MouseClicked(()), &Target::All);
        assert!(Arc::ptr_eq(&first.claim, &second.claim));
    }

    #[test]
    fn wanted_erases_the_item_a_scoped_kind_carries() {
        // The source registers against *which events*, not *whose* — two
        // different items claiming `mouse.clicked` must still add up to one
        // entry a source's `provides()` can match against.
        let mut claims = Claims::default();
        let _mine = claims.take(holder(1), &Kind::MouseClicked(()), &Target::All);
        let _theirs = claims.take(holder(2), &Kind::MouseClicked(()), &Target::All);
        assert_eq!(claims.wanted(), BTreeSet::from([Kind::MouseClicked(())]));
    }

    #[test]
    fn a_scoped_event_is_not_reachable_through_dependents() {
        // `Event::kind()` cannot say which item a click landed on — that is
        // decided by hit geometry, not carried in the payload — so this can
        // never find a scoped kind's real, entity-bound claim. Delivery for
        // these goes through direct hit-test dispatch instead; see
        // `ecs.rs::route_pointer`.
        let mut claims = Claims::default();
        let _mine = claims.take(holder(1), &Kind::MouseClicked(()), &Target::All);
        let pressed = Kind::MouseClicked(()).into_event();
        assert!(claims.dependents(&pressed, &Target::All).is_empty());
    }

    #[test]
    fn dropping_a_handle_tells_the_registry_to_look() {
        let mut claims = Claims::default();
        let watch = claims.take(holder(1), &Kind::SystemWoke, &Target::All);
        assert!(claims.moved(), "taking one is a change");
        assert!(!claims.moved(), "and asking clears it");

        drop(watch);
        assert!(claims.moved(), "so is the last one going");
    }

    #[test]
    fn an_item_that_moved_is_holding_a_different_claim() {
        // The whole point: the rectangle is the claim's identity, so a move
        // is not an adjustment to anything, it is one claim dying and another
        // being taken.
        let mut claims = Claims::default();
        let was = claims.take(at(1, 0.0), &Kind::MouseEntered(()), &Target::All);
        let now = claims.take(at(1, 60.0), &Kind::MouseEntered(()), &Target::All);
        assert!(!Arc::ptr_eq(&was.claim, &now.claim));

        drop(was);
        assert_eq!(claims.areas().on(1).count(), 1, "only where it is now");
    }

    #[test]
    fn an_item_that_stayed_put_holds_the_claim_it_already_had() {
        let mut claims = Claims::default();
        let first = claims.take(at(1, 0.0), &Kind::MouseEntered(()), &Target::All);
        let again = claims.take(at(1, 0.0), &Kind::MouseEntered(()), &Target::All);
        assert!(Arc::ptr_eq(&first.claim, &again.claim));
    }

    #[test]
    fn an_area_claim_is_not_the_same_as_a_bare_one() {
        let mut claims = Claims::default();
        let bare = claims.take(holder(1), &Kind::MouseClicked(()), &Target::All);
        let over = claims.take(at(1, 0.0), &Kind::MouseClicked(()), &Target::All);
        assert!(!Arc::ptr_eq(&bare.claim, &over.claim));
    }

    #[test]
    fn wanted_erases_the_rectangle_a_claim_is_taken_over() {
        // A source registers against *which events*: an item moving must not
        // look to it like a change of demand at all.
        let mut claims = Claims::default();
        let _here = claims.take(at(1, 0.0), &Kind::MouseEntered(()), &Target::All);
        let _there = claims.take(at(2, 60.0), &Kind::MouseEntered(()), &Target::All);
        assert_eq!(claims.wanted(), BTreeSet::from([Kind::MouseEntered(())]));
    }

    #[test]
    fn the_tracked_areas_are_read_back_off_the_claims() {
        let mut claims = Claims::default();
        assert!(claims.areas().changed().is_empty(), "nothing claimed yet");

        let here = claims.take(at(1, 0.0), &Kind::MouseEntered(()), &Target::All);
        assert_eq!(claims.areas().changed(), [1]);
        assert_eq!(claims.areas().on(1).count(), 1);
        assert!(
            claims.areas().changed().is_empty(),
            "asking twice is not a change"
        );

        drop(here);
        assert_eq!(claims.areas().changed(), [1], "the rectangle went with it");
        assert_eq!(claims.areas().on(1).count(), 0);
    }
}

#[cfg(test)]
mod eviction_tests {
    use super::{Evicting, Liveness};

    #[test]
    fn a_source_survives_the_pass_its_last_claim_goes() {
        let mut liveness = Liveness::Wanted;
        assert_eq!(liveness.evict(), Evicting::Marked);
        assert_eq!(liveness, Liveness::Stale, "marked, not taken down");
    }

    #[test]
    fn the_next_pass_takes_a_stale_source_down() {
        let mut liveness = Liveness::Wanted;
        liveness.evict();
        assert_eq!(liveness.evict(), Evicting::Swept);
    }

    #[test]
    fn a_claim_taken_before_the_sweep_revives_it() {
        // The whole point: an item that moves drops one claim and takes
        // another, and the two need not land in the same pass. The
        // registration has to still be there when the second one arrives.
        let mut liveness = Liveness::Wanted;
        assert_eq!(liveness.evict(), Evicting::Marked);
        assert!(liveness.revive(), "that was a rescue");
        assert_eq!(liveness, Liveness::Wanted);
        // And the grace pass starts over, rather than the source coming down
        // on the next one because it was once marked.
        assert_eq!(liveness.evict(), Evicting::Marked);
    }

    #[test]
    fn wanting_a_source_that_was_never_marked_is_not_a_rescue() {
        // Every pass over a live source calls this, so it has to be free of
        // meaning when nothing was pending.
        let mut liveness = Liveness::Wanted;
        assert!(!liveness.revive());
        assert_eq!(liveness, Liveness::Wanted);
    }
}

#[cfg(test)]
mod declaration_tests {
    use super::{Registry, SourceId};
    use crate::protocol::EventName;

    /// A registry with nothing started: a waker needs a run loop and a test
    /// thread has one, and no source registers until something claims it.
    fn registry() -> Registry {
        Registry::new(
            crate::runloop::main_thread(),
            crate::config::shared(),
            crate::runloop::Waker::install(crate::runloop::main_thread(), || {}),
        )
    }

    #[test]
    fn an_event_bridged_from_a_notification_gains_a_provider() {
        let mut registry = registry();
        let name: EventName = "demo".parse().unwrap();
        registry.declare_event(name.clone(), Some("com.example.thing".parse().unwrap()));

        let id = SourceId("notifications");
        assert_eq!(
            registry.providers.get(&name.kind()).map(Vec::as_slice),
            Some([id].as_slice()),
            "the bridge is what can serve this kind"
        );
        assert!(registry.sources[&id].provides.contains(&name.kind()));
        assert!(!registry.running(id), "declaring is not claiming");
    }

    #[test]
    fn an_event_the_config_fires_itself_has_no_provider() {
        // `--trigger` is its only source, so there is nothing to register
        // against — but the name is still declared.
        let mut registry = registry();
        let name: EventName = "demo".parse().unwrap();
        registry.declare_event(name.clone(), None);

        assert!(!registry.providers.contains_key(&name.kind()));
        assert!(registry.declared_events().any(|held| *held == name));
    }

    #[test]
    fn a_config_that_stops_declaring_an_event_retires_it() {
        // The only way one goes away: there is no `--remove event`. Without
        // the sweep the notifications source keeps advertising a kind nothing
        // can trigger, and keeps a bridge entry that would install a real
        // observer the moment anything claimed it.
        let mut registry = registry();
        let name: EventName = "demo".parse().unwrap();
        registry.declare_event(name.clone(), Some("com.example.thing".parse().unwrap()));

        registry.begin_config();
        registry.end_config();

        let id = SourceId("notifications");
        assert!(registry.declared_events().next().is_none());
        assert!(!registry.providers.contains_key(&name.kind()));
        assert!(!registry.sources[&id].provides.contains(&name.kind()));
        assert!(
            registry.declared.lock().unwrap().is_empty(),
            "and the source is no longer told to bridge it"
        );
    }

    #[test]
    fn a_config_that_declares_it_again_keeps_it() {
        let mut registry = registry();
        let name: EventName = "demo".parse().unwrap();
        registry.declare_event(name.clone(), Some("com.example.thing".parse().unwrap()));

        registry.begin_config();
        registry.declare_event(name.clone(), Some("com.example.thing".parse().unwrap()));
        registry.end_config();

        assert_eq!(
            registry.providers.get(&name.kind()).map(Vec::as_slice),
            Some([SourceId("notifications")].as_slice()),
            "declaring it again is what clears the mark"
        );
    }

    #[test]
    fn a_renamed_event_leaves_nothing_of_the_old_one_behind() {
        let mut registry = registry();
        let was: EventName = "old".parse().unwrap();
        let now: EventName = "new".parse().unwrap();
        registry.declare_event(was.clone(), Some("com.example.thing".parse().unwrap()));

        registry.begin_config();
        registry.declare_event(now.clone(), Some("com.example.thing".parse().unwrap()));
        registry.end_config();

        assert!(!registry.providers.contains_key(&was.kind()));
        assert!(registry.providers.contains_key(&now.kind()));
        let declared = registry.declared.lock().unwrap();
        assert_eq!(declared.len(), 1, "one bridge, not two");
        assert!(declared.contains_key(&now));
    }

    #[test]
    fn the_fixed_rows_survive_a_config_bracket() {
        // The sweep is over declarations, not over what the source is made
        // of: `front_app_switched` is not a declaration and must not be
        // swept with them.
        let mut registry = registry();
        registry.begin_config();
        registry.end_config();

        assert_eq!(
            registry
                .providers
                .get(&crate::protocol::Kind::FrontAppSwitched)
                .map(Vec::as_slice),
            Some([SourceId("notifications")].as_slice())
        );
    }
}

#[cfg(test)]
mod delivery_tests {
    use super::{Feed, Registry, SourceId};
    use crate::protocol::Event;
    use crate::protocol::event::SystemWoke;
    use std::sync::atomic::Ordering;

    /// A burst big enough to overrun any queue a bounded channel would have.
    const BURST: usize = 1000;

    fn registry() -> Registry {
        Registry::new(
            crate::runloop::main_thread(),
            crate::config::shared(),
            crate::runloop::Waker::install(crate::runloop::main_thread(), || {}),
        )
    }

    fn woke() -> Event {
        Event::SystemWoke(SystemWoke {})
    }

    /// Drains until nothing more is queued, counting what came out. That is
    /// what the run loop does: a drain that stops leaves its source signalled,
    /// and the wake brings us straight back here.
    fn drain_all(registry: &mut Registry) -> usize {
        let mut seen = 0;
        loop {
            let before = seen;
            registry.drain(|_, _| seen += 1);
            if seen == before {
                break;
            }
        }
        seen
    }

    #[test]
    fn a_burst_from_one_source_loses_nothing() {
        let mut registry = registry();
        let emit = registry.emitter();
        for _ in 0..BURST {
            emit.send(woke());
        }

        // One pass takes its budget and no more, and what it left is still
        // signalled: queued, with a wake pending.
        let mut first = 0;
        registry.drain(|_, _| first += 1);
        assert_eq!(first, super::DRAIN_BUDGET);
        assert!(
            registry.ready.load(Ordering::Acquire) & super::LOCAL_BIT != 0,
            "the rest of the burst is still asking to be looked at"
        );

        assert_eq!(drain_all(&mut registry) + first, BURST);
        assert_eq!(
            registry.ready.load(Ordering::Acquire),
            0,
            "nothing left signalled once everything has been delivered"
        );
    }

    #[test]
    fn an_event_emitted_mid_drain_still_arrives() {
        let mut registry = registry();
        let emit = registry.emitter();
        emit.send(woke());

        let mut seen = 0;
        registry.drain(|_, _| {
            seen += 1;
            if seen == 1 {
                emit.send(woke());
            }
        });
        let delivered = seen + drain_all(&mut registry);
        assert_eq!(delivered, 2, "an event emitted mid-pass is not lost");
        assert_eq!(registry.ready.load(Ordering::Acquire), 0);
    }

    #[test]
    fn a_chatty_source_does_not_starve_the_ones_behind_it() {
        let mut registry = registry();
        let quiet = SourceId("power");
        let bit = registry.sources[&quiet].bit;
        let (other, feed) = Feed::manual(
            quiet,
            registry.waker.clone(),
            std::sync::Arc::clone(&registry.ready),
            bit,
        );
        registry
            .sources
            .get_mut(&quiet)
            .expect("a known source")
            .feed = Some(feed);

        let chatty = registry.emitter();
        for _ in 0..BURST {
            chatty.send(woke());
            other.send(woke());
        }

        let mut counts = std::collections::HashMap::new();
        registry.drain(|id, _| *counts.entry(id).or_insert(0usize) += 1);
        assert!(
            counts.get(&quiet).is_some_and(|count| *count > 0),
            "one drain reaches both sources: {counts:?}"
        );
        assert!(counts.get(&SourceId("local")).is_some_and(|c| *c > 0));
    }
}

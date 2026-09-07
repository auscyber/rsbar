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
//! The channel is `tokio::sync::mpsc`, which is waker-based: a producer's
//! `try_send` hands the value over and wakes whoever is waiting, rather than
//! parking a thread or being polled on a timer. That matters because the
//! producers are OS callbacks — a notification block, a `CoreAudio` listener,
//! an `IOKit` run loop source — and a callback that blocks is a callback that
//! stalls the framework that called it. `try_send` never blocks: a full queue
//! drops the event, which is the right trade when the alternative is wedging
//! the window server's notification thread.
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

pub mod config;
pub mod displays;
pub mod mouse;
pub mod observers;
pub mod power;
pub mod volume;
pub mod workspace;

use rsbar_protocol::{Event, Kind};
use std::collections::{BTreeSet, HashMap};
use std::ffi::c_void;

/// How many events may be queued before the oldest producer starts losing
/// them. A burst this deep means the daemon is not draining, which is a bug
/// rather than a backlog to absorb.
const QUEUE_DEPTH: usize = 256;

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
/// Two halves, and both are necessary.
///
/// The queue is `tokio::sync::mpsc`: `try_send` never blocks, and a full queue
/// drops. That matters because every producer is an OS callback — a
/// notification block, a `CoreAudio` listener, an `IOKit` run loop source — and
/// a callback that blocks stalls the framework that called it.
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
    queue: tokio::sync::mpsc::Sender<Event>,
    waker: crate::runloop::Waker,
}

impl Emitter {
    /// Queues an event and wakes the app to drain it.
    ///
    /// Never blocks and never fails loudly: a full queue means the daemon is
    /// not draining, which is a bug to see in the log rather than a reason to
    /// stall a system callback.
    pub fn send(&self, event: Event) {
        match self.queue.try_send(event) {
            Ok(()) => self.waker.wake(),
            Err(err) => tracing::warn!(%err, "dropping an event; the queue is full"),
        }
    }
}

impl std::fmt::Debug for Emitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Emitter")
    }
}

/// Whatever keeps a source registered with its framework.
///
/// Deliberately opaque: its only contract is `Drop`, which deregisters. It is
/// held inside the [`Feed`] rather than handed back to a caller, so a source
/// cannot outlive the thing you poll it through.
pub type Registration = Box<dyn std::any::Any>;

/// A running source: the events it produces, and what keeps it alive.
///
/// This is the unit the rest of the daemon deals in. It is identified, it is
/// pollable, and dropping it stops the source — rather than an install call
/// with a side effect and an opaque token to file away somewhere.
pub struct Feed {
    id: SourceId,
    events: tokio::sync::mpsc::Receiver<Event>,
    /// Kept so a source being adjusted writes into the same queue it already
    /// does, rather than being handed a second one nothing reads.
    emit: Emitter,
    /// Dropped on whichever thread owns this feed, which is the thread that
    /// registered. Never read.
    registration: Registration,
}

impl Feed {
    /// Wraps a registration and its receiver.
    fn new(
        id: SourceId,
        events: tokio::sync::mpsc::Receiver<Event>,
        emit: Emitter,
        registration: Registration,
    ) -> Self {
        Self {
            id,
            events,
            emit,
            registration,
        }
    }

    /// A feed nothing registered for — one the caller writes into directly.
    ///
    /// Some events have no framework behind them: a click lands on a window the
    /// main thread owns, and `--trigger` arrives over IPC. They join the same
    /// stream as everything else rather than being a special case downstream.
    #[must_use]
    pub fn manual(id: SourceId, waker: crate::runloop::Waker) -> (Emitter, Self) {
        let (queue, events) = tokio::sync::mpsc::channel(QUEUE_DEPTH);
        let emit = Emitter { queue, waker };
        (emit.clone(), Self::new(id, events, emit, Box::new(())))
    }

    #[must_use]
    pub fn id(&self) -> SourceId {
        self.id
    }

    /// The registration, so a running source can be adjusted in place rather
    /// than stopped and started again.
    fn registration_mut(&mut self) -> &mut Registration {
        &mut self.registration
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

    /// Waits for the next event, or `None` once the source has stopped.
    ///
    /// Unused by the current runner, but this is what makes the producers'
    /// `try_send` a wakeup rather than a write into a void — and it is the
    /// shape an executor would poll.
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
    /// A source was handed back a registration it did not produce, which can
    /// only be the registry pairing them up wrongly.
    #[error("the registration does not belong to this source")]
    MismatchedRegistration,
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

    /// Registers what it takes to produce `wanted`, and nothing more.
    ///
    /// `wanted` is a non-empty subset of [`provides`](Source::provides): the
    /// events something is actually waiting for. Observing more than this is
    /// not free — four `NSWorkspace` observers for an item that only asked
    /// about the front application is three notifications nobody reads — so a
    /// source registers against `wanted` rather than against everything it is
    /// capable of. A source whose events all come off one facility is free to
    /// install it whole; that is its business, not the registry's.
    ///
    /// Called again with a different set when demand changes, having dropped
    /// the previous [`Registration`] first. That is how a reload that adds a
    /// subscription gets the observer it needs: the registry does not ask a
    /// running source for more, it re-registers it against the new set.
    ///
    /// Called on the main thread, with the app's run loop current. The returned
    /// [`Registration`] is dropped on that same thread, which is what
    /// deregisters. A source whose framework delivers from a thread of its own
    /// is free to do so — [`Emitter`] is `Send` and wakes the run loop.
    ///
    /// # Errors
    ///
    /// Returns [`StartError`] if the underlying framework refuses. A source
    /// that cannot start is not asked again: these fail for structural reasons,
    /// not transient ones.
    fn register(
        &mut self,
        wanted: &BTreeSet<Kind>,
        cx: &mut Registering<'_>,
    ) -> Result<Registration, StartError>;

    /// Adjusts a running registration to a new set of requests.
    ///
    /// Called instead of tearing the source down and building it again, so a
    /// source holding one observer per event only touches the ones that came
    /// or went — the four `NSWorkspace` observers do not all get deregistered
    /// and reinstalled because a fifth item started asking about sleep.
    ///
    /// The default says it cannot, which is the honest answer for a source
    /// whose events all come off one facility: there is nothing to adjust,
    /// because every event it provides is already being produced. The registry
    /// then leaves the registration exactly as it is.
    ///
    /// # Errors
    ///
    /// Returns [`StartError`] if the change is refused, which stops the source.
    fn update(
        &mut self,
        wanted: &BTreeSet<Kind>,
        current: &mut Registration,
        cx: &mut Registering<'_>,
    ) -> Result<(), StartError> {
        let _ = (wanted, current, cx);
        Ok(())
    }
}

/// What a source is handed when it registers.
///
/// A context rather than a bare [`Emitter`] because registering needs more than
/// a sink: it needs whatever this source shares with the others.
pub struct Registering<'a> {
    id: SourceId,
    emit: Emitter,
    shared: &'a mut Shared,
}

impl Registering<'_> {
    /// The sink this source's callbacks write into.
    #[must_use]
    pub fn emitter(&self) -> Emitter {
        self.emit.clone()
    }

    /// Context this source shares with every other one that asks for it.
    ///
    /// For what more than one source needs and none should own twice — a thread
    /// serving several observers, a connection, a subscription upstream. Built
    /// on first ask and handed out by [`Arc`](std::sync::Arc) after that, which
    /// matters most for the sources that are indexed by an entity: one per item
    /// on the bar, all wanting the same thing behind them.
    ///
    /// Held only by its users. The registry keeps a [`Weak`](std::sync::Weak),
    /// so when the last source holding one stops, the context goes with it and
    /// the next ask builds a fresh one.
    ///
    /// # Errors
    ///
    /// Returns [`StartError`] if the context cannot be built, which fails the
    /// registration that asked for it.
    pub fn shared<T: Context>(&mut self) -> Result<std::sync::Arc<T>, StartError> {
        self.shared
            .get_or_create::<T>()
            .map_err(|cause| StartError::new(self.id, cause))
    }
}

/// Something more than one source needs, built once and shared.
pub trait Context: std::any::Any + Send + Sync {
    /// Builds it. Called on the main thread, on the first ask.
    ///
    /// # Errors
    ///
    /// Returns the [`Cause`] the source that asked will fail with.
    fn create() -> Result<Self, Cause>
    where
        Self: Sized;
}

/// The shared contexts currently alive, by type.
#[derive(Default)]
struct Shared(HashMap<std::any::TypeId, std::sync::Weak<dyn std::any::Any + Send + Sync>>);

impl Shared {
    fn get_or_create<T: Context>(&mut self) -> Result<std::sync::Arc<T>, Cause> {
        let key = std::any::TypeId::of::<T>();
        if let Some(live) = self.0.get(&key).and_then(std::sync::Weak::upgrade) {
            // Keyed by the type it was stored under, so this is the same type.
            if let Ok(shared) = live.downcast::<T>() {
                return Ok(shared);
            }
        }
        let made = std::sync::Arc::new(T::create()?);
        let weak = std::sync::Arc::downgrade(&(made.clone() as std::sync::Arc<_>));
        self.0.insert(key, weak);
        Ok(made)
    }
}

/// State a C callback is handed a pointer to.
///
/// Every framework here takes a `void*` and gives it back on each callback.
/// This owns that state and every conversion it needs, so no source has to
/// write a cast, store a raw pointer, or reason about a reference count.
///
/// Ownership is a plain [`Arc`](std::sync::Arc): the C side is given a
/// *borrowed* pointer, not a count, and this value keeps the state alive.
/// Dropping it frees — so it must outlive the deregistration, which a struct
/// holding one alongside its registration gets for free: Rust runs
/// `Drop::drop` before dropping fields, so a destructor that deregisters has
/// already run by the time the state goes.
///
/// A framework that can still be running a callback after deregistration
/// returns must say so and call [`CallbackState::leak`] instead.
///
pub struct CallbackState<T: Payload>(std::sync::Arc<T>);

/// The states a C callback can be handed a pointer to.
///
/// Sealed, because the set is small and known: an [`Emitter`] for the sources
/// that only need to report something happened, and the volume source's
/// listener state, which also has to remember which device it is watching.
/// Adding a third is a deliberate act here rather than an accident at a call
/// site.
///
/// `Send + Sync` is a requirement, not an assumption: `CoreAudio` calls back on
/// a thread of its own, so state that was not safe to share would be a data
/// race rather than a compile error.
pub trait Payload: Send + Sync + sealed::Sealed {}

pub(crate) mod sealed {
    pub trait Sealed {}
}

impl sealed::Sealed for Emitter {}
impl Payload for Emitter {}

impl<T: Payload> CallbackState<T> {
    pub fn new(value: T) -> Self {
        Self(std::sync::Arc::new(value))
    }

    /// The state, for reading outside a callback.
    #[must_use]
    pub fn get(&self) -> &T {
        &self.0
    }

    /// Runs `f` with the pointer to hand the framework.
    ///
    /// Scoped deliberately: the pointer is valid for as long as this value
    /// lives, and passing it through a closure is what stops a caller storing
    /// one that outlives it.
    pub fn with_ptr<R>(&self, f: impl FnOnce(*mut c_void) -> R) -> R {
        f(std::sync::Arc::as_ptr(&self.0).cast_mut().cast::<c_void>())
    }

    /// The same, from a reference recovered inside a callback.
    ///
    /// A callback that needs to re-register — the audio device changing under
    /// the volume listener — has the state but not the original pointer. It is
    /// the same address either way.
    pub fn with_ptr_of<R>(value: &T, f: impl FnOnce(*mut c_void) -> R) -> R {
        f(std::ptr::from_ref(value).cast_mut().cast::<c_void>())
    }

    /// Recovers the state inside a callback.
    ///
    /// # Safety
    ///
    /// `context` must be a pointer this type produced for a `CallbackState`
    /// of the same `T` that is still alive. A null pointer is handled rather
    /// than dereferenced, because a framework may call back with one during
    /// teardown.
    pub unsafe fn recover<'a>(context: *mut c_void) -> Option<&'a T> {
        // SAFETY: the caller guarantees provenance, type and liveness.
        unsafe { context.cast::<T>().as_ref() }
    }

    /// Another handle to the same state.
    #[must_use]
    pub fn clone_of(state: &Self) -> Self {
        Self(std::sync::Arc::clone(&state.0))
    }

    /// Gives up ownership, so the state is never freed.
    ///
    /// For a framework that can still be running a callback after
    /// deregistration returns, where freeing would be a use-after-free that
    /// shows up as a rare crash on quit. One small allocation, deliberately
    /// kept.
    pub fn leak(self) {
        let _ = std::sync::Arc::into_raw(self.0);
    }
}

/// A live claim on one event.
///
/// The claim *is* the reference count. Every item wanting the same event holds
/// a handle on one shared [`Claim`], and the registry keeps only a [`Weak`] to
/// it — so the event is wanted for exactly as long as a handle exists, with no
/// counter to keep in step with reality. A second demander clones rather than
/// allocating a second claim, and the last handle to go frees it once.
///
/// Dropping only records the release. Deregistering has to happen on the main
/// thread with the run loop current, and a `Drop` can run anywhere, so the
/// registry settles the change on its next pass.
#[derive(Debug, Clone)]
pub struct Watch(
    /// Held for its destructor and nothing else. Never read: what it *is* is
    /// the claim, and letting go of it is the whole interface.
    #[allow(dead_code, reason = "the value is the reference, not something read")]
    std::sync::Arc<Claim>,
);

/// The thing a [`Watch`] is a handle on: one event being wanted.
///
/// Never held by the registry, only pointed at weakly. Its destructor running
/// is what "nothing wants this any more" means.
#[derive(Debug)]
struct Claim {
    /// Set when this dies, so the registry knows to look without walking every
    /// source on every pass.
    moved: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.moved.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// The claims currently alive, by event.
#[derive(Debug, Default)]
struct Claims {
    live: HashMap<Kind, std::sync::Weak<Claim>>,
    moved: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Claims {
    /// A handle on `kind`, sharing the one claim if it is already alive.
    fn take(&mut self, kind: &Kind) -> Watch {
        if let Some(existing) = self.live.get(kind).and_then(std::sync::Weak::upgrade) {
            return Watch(existing);
        }
        let claim = std::sync::Arc::new(Claim {
            moved: std::sync::Arc::clone(&self.moved),
        });
        self.live
            .insert(kind.clone(), std::sync::Arc::downgrade(&claim));
        self.mark();
        Watch(claim)
    }

    fn mark(&self) {
        self.moved.store(true, std::sync::atomic::Ordering::Release);
    }

    /// Whether any claim was taken or died since the last ask.
    fn moved(&self) -> bool {
        self.moved.swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    /// The events still claimed, forgetting the ones whose claim has died.
    fn wanted(&mut self) -> BTreeSet<Kind> {
        self.live.retain(|_, claim| claim.strong_count() > 0);
        self.live.keys().cloned().collect()
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
    shared: &mut Shared,
    waker: &crate::runloop::Waker,
) -> Result<Feed, StartError> {
    let id = source.id();
    let (queue, events) = tokio::sync::mpsc::channel(QUEUE_DEPTH);
    let emit = Emitter {
        queue,
        waker: waker.clone(),
    };
    let mut cx = Registering {
        id,
        emit: emit.clone(),
        shared,
    };
    let registration = source.register(wanted, &mut cx)?;
    Ok(Feed::new(id, events, emit, registration))
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
    /// Context the sources share, built on first ask.
    shared: Shared,
    /// Events with no framework behind them — a click, or `--trigger`.
    manual: Feed,
    emit: Emitter,
    /// Handed to every source so its events wake the app rather than waiting
    /// for the next tick.
    waker: crate::runloop::Waker,
}

/// A source, and its feed once it is running.
struct Registered {
    source: Box<dyn Source>,
    feed: Option<Feed>,
    /// Everything this source can produce.
    provides: BTreeSet<Kind>,
    /// What it is serving right now, so a change of demand is detectable.
    serving: BTreeSet<Kind>,
    /// Kept running regardless of who wants it: the bar's own geometry and its
    /// config file are not anybody's subscription. An eager source is asked
    /// for everything it provides.
    pinned: bool,
    /// A source that refused to start is not asked again. These fail for
    /// structural reasons — a missing config file, a framework saying no — so a
    /// second attempt fails identically.
    failed: bool,
}

impl Registered {
    /// Everything still claimed that this source provides.
    fn demand(&self, claimed: &BTreeSet<Kind>) -> BTreeSet<Kind> {
        if self.pinned {
            return self.provides.clone();
        }
        self.provides.intersection(claimed).cloned().collect()
    }
}

impl Registry {
    /// Builds the registry over the sources this build knows about.
    #[must_use]
    pub fn new(config: crate::config::Shared, waker: crate::runloop::Waker) -> Self {
        let sources: Vec<Box<dyn Source>> = vec![
            Box::new(workspace::Workspace),
            Box::new(displays::Displays),
            Box::new(config::Watcher { config }),
            Box::new(power::Power),
            Box::new(mouse::Mouse),
            Box::new(volume::Volume),
        ];

        let mut providers: HashMap<Kind, Vec<SourceId>> = HashMap::new();
        let mut registered = HashMap::with_capacity(sources.len());
        for source in sources {
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
                    failed: false,
                },
            );
        }

        let (emit, manual) = Feed::manual(SourceId("local"), waker.clone());
        Self {
            sources: registered,
            providers,
            claims: Claims::default(),
            shared: Shared::default(),
            manual,
            emit,
            waker,
        }
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
    pub fn watch(&mut self, kind: &Kind) -> Watch {
        if !self.providers.contains_key(kind) {
            tracing::debug!(%kind, "nothing provides this event");
        }
        self.claims.take(kind)
    }

    /// Claims several events at once.
    pub fn watch_all(&mut self, kinds: impl IntoIterator<Item = Kind>) -> Vec<Watch> {
        kinds.into_iter().map(|kind| self.watch(&kind)).collect()
    }

    /// Brings every source into line with the claims currently out.
    ///
    /// Cheap when nothing moved, which is almost every pass: a flag says
    /// whether any claim was taken or dropped since last time.
    pub fn settle(&mut self) {
        if !self.claims.moved() {
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
            shared,
            waker,
            ..
        } = self;
        let Some(entry) = sources.get_mut(&id) else {
            return;
        };
        let demand = entry.demand(claimed);

        if demand.is_empty() {
            if entry.feed.is_some() {
                // Dropping the feed drops the registration, which deregisters —
                // on this thread, which is the one that registered.
                tracing::debug!(source = %id, "stopped event source; nothing wants it");
                entry.feed = None;
                entry.serving.clear();
            }
            return;
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
                shared,
            };
            match entry
                .source
                .update(&demand, feed.registration_mut(), &mut cx)
            {
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

        match start(entry.source.as_mut(), &demand, shared, waker) {
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

    /// Takes everything queued across every running feed, without waiting.
    ///
    /// Each emission is tagged with the source that produced it, so an event
    /// arriving from somewhere unexpected is traceable rather than anonymous.
    pub fn drain(&mut self) -> Vec<(SourceId, Event)> {
        let mut drained = Vec::new();
        let feeds = self.sources.values_mut().filter_map(|e| e.feed.as_mut());
        for feed in feeds.chain(std::iter::once(&mut self.manual)) {
            let id = feed.id();
            while let Some(event) = feed.try_next() {
                drained.push((id, event));
            }
        }
        drained
    }
}

#[cfg(test)]
mod claim_tests {
    use super::{Claims, Kind};
    use std::collections::BTreeSet;
    use std::sync::Arc;

    #[test]
    fn a_second_claim_on_one_event_is_a_handle_on_the_first() {
        let mut claims = Claims::default();
        let first = claims.take(&Kind::SystemWoke);
        let second = claims.take(&Kind::SystemWoke);
        assert!(
            Arc::ptr_eq(&first.0, &second.0),
            "one claim, two handles — not two claims"
        );
    }

    #[test]
    fn an_event_stays_wanted_until_the_last_handle_goes() {
        let mut claims = Claims::default();
        let first = claims.take(&Kind::SystemWoke);
        let second = claims.take(&Kind::SystemWoke);

        drop(first);
        assert_eq!(claims.wanted(), BTreeSet::from([Kind::SystemWoke]));

        drop(second);
        assert!(claims.wanted().is_empty());
    }

    #[test]
    fn a_claim_taken_again_after_dying_is_a_fresh_one() {
        let mut claims = Claims::default();
        drop(claims.take(&Kind::SystemWoke));
        assert!(claims.wanted().is_empty());

        let revived = claims.take(&Kind::SystemWoke);
        assert_eq!(claims.wanted(), BTreeSet::from([Kind::SystemWoke]));
        drop(revived);
    }

    #[test]
    fn dropping_a_handle_tells_the_registry_to_look() {
        let mut claims = Claims::default();
        let watch = claims.take(&Kind::SystemWoke);
        assert!(claims.moved(), "taking one is a change");
        assert!(!claims.moved(), "and asking clears it");

        drop(watch);
        assert!(claims.moved(), "so is the last one going");
    }
}

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

use bevy_ecs::entity::Entity;
use rsbar_protocol::{Event, Kind};
use std::collections::{HashMap, HashSet};
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
    /// Dropped on whichever thread owns this feed, which is the thread that
    /// registered. Never read.
    _registration: Registration,
}

impl Feed {
    /// Wraps a registration and its receiver.
    fn new(
        id: SourceId,
        events: tokio::sync::mpsc::Receiver<Event>,
        registration: Registration,
    ) -> Self {
        Self {
            id,
            events,
            _registration: registration,
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
        (
            Emitter { queue, waker },
            Self::new(id, events, Box::new(())),
        )
    }

    #[must_use]
    pub fn id(&self) -> SourceId {
        self.id
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
    /// subscription to wait for because the daemon is the subscriber.
    fn eager(&self) -> bool {
        false
    }

    /// Registers observers that write into `emit`.
    ///
    /// Called on the main thread, with the app's run loop current. The returned
    /// [`Registration`] is dropped on that same thread, which is what
    /// deregisters. A source whose framework delivers from a thread of its own
    /// is free to do so — [`Emitter`] is `Send` and wakes the run loop.
    ///
    /// # Errors
    ///
    /// Returns [`StartError`] if the underlying framework refuses. A source
    /// that cannot start is dropped rather than retried: these fail for
    /// structural reasons, not transient ones.
    fn register(&mut self, emit: Emitter) -> Result<Registration, StartError>;
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

/// Starts a source, giving back the feed it produces.
///
/// Registration happens here, on the caller's thread, which is the main one.
/// Whatever the source hands back is held inside the feed and dropped when the
/// feed is — deregistering on the same thread that registered.
///
/// # Errors
///
/// Returns [`StartError`] if the framework refuses.
pub fn start(source: &mut dyn Source, waker: &crate::runloop::Waker) -> Result<Feed, StartError> {
    let id = source.id();
    let (queue, events) = tokio::sync::mpsc::channel(QUEUE_DEPTH);
    let registration = source.register(Emitter {
        queue,
        waker: waker.clone(),
    })?;
    Ok(Feed::new(id, events, registration))
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
    /// What each item is currently keeping alive.
    ///
    /// The reference count, held as the set it was derived from rather than a
    /// number: replacing an item's subscriptions has to release exactly what it
    /// used to want, which a count cannot tell you.
    held: HashMap<Entity, HashSet<SourceId>>,
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
    /// The items keeping it alive. Emptying this stops the source.
    users: HashSet<Entity>,
    /// Kept running regardless of who wants it: the bar's own geometry and its
    /// config file are not anybody's subscription.
    pinned: bool,
    /// A source that refused to start is not asked again. These fail for
    /// structural reasons — a missing config file, a framework saying no — so a
    /// second attempt fails identically.
    failed: bool,
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
            for kind in source.provides() {
                providers.entry(kind).or_default().push(id);
            }
            registered.insert(
                id,
                Registered {
                    pinned: source.eager(),
                    source,
                    feed: None,
                    users: HashSet::new(),
                    failed: false,
                },
            );
        }

        let (emit, manual) = Feed::manual(SourceId("local"), waker.clone());
        Self {
            sources: registered,
            providers,
            held: HashMap::new(),
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
            self.reconcile(id, None);
        }
    }

    /// Records everything `who` needs, releasing whatever it needed before.
    ///
    /// This is the whole lifecycle: a source starts when the first item wants
    /// an event it provides, and stops when the last one stops wanting it. An
    /// item that drops `volume_changed` from its subscriptions stops the audio
    /// listener, and a config reload that removes the item entirely does too.
    pub fn holds(&mut self, who: Entity, kinds: impl IntoIterator<Item = Kind>) {
        let mut wanted = HashSet::new();
        let mut touched = Vec::new();
        for kind in kinds {
            if let Some(ids) = self.providers.get(&kind) {
                wanted.extend(ids.iter().copied());
                touched.push(kind);
            } else {
                tracing::debug!(%kind, "nothing provides this event");
            }
        }

        let previously = self.held.insert(who, wanted.clone()).unwrap_or_default();
        for id in previously.difference(&wanted) {
            if let Some(entry) = self.sources.get_mut(id) {
                entry.users.remove(&who);
            }
        }
        for id in &wanted {
            if let Some(entry) = self.sources.get_mut(id) {
                entry.users.insert(who);
            }
        }
        if wanted.is_empty() {
            self.held.remove(&who);
        }

        for id in previously.union(&wanted).copied().collect::<Vec<_>>() {
            self.reconcile(id, touched.first());
        }
    }

    /// Releases everything an item was keeping alive, because it is gone.
    pub fn release(&mut self, who: Entity) {
        let Some(held) = self.held.remove(&who) else {
            return;
        };
        for id in held {
            if let Some(entry) = self.sources.get_mut(&id) {
                entry.users.remove(&who);
            }
            self.reconcile(id, None);
        }
    }

    /// Brings one source into line with whether anything still wants it.
    fn reconcile(&mut self, id: SourceId, because: Option<&Kind>) {
        let Self { sources, waker, .. } = self;
        let Some(entry) = sources.get_mut(&id) else {
            return;
        };
        let wanted = entry.pinned || !entry.users.is_empty();

        match (wanted, entry.feed.is_some()) {
            (true, false) if !entry.failed => match start(entry.source.as_mut(), waker) {
                Ok(feed) => {
                    tracing::debug!(source = %id, kind = ?because, "started event source");
                    entry.feed = Some(feed);
                }
                Err(err) => {
                    entry.failed = true;
                    tracing::warn!(%err, "event source unavailable");
                }
            },
            // Dropping the feed drops the registration, which deregisters — on
            // this thread, which is the one that registered.
            (false, true) => {
                tracing::debug!(source = %id, "stopped event source; nothing wants it");
                entry.feed = None;
            }
            _ => {}
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

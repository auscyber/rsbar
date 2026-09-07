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
pub mod power;
pub mod volume;
pub mod workspace;

use rsbar_protocol::Event;

/// How many events may be queued before the oldest producer starts losing
/// them. A burst this deep means the daemon is not draining, which is a bug
/// rather than a backlog to absorb.
const QUEUE_DEPTH: usize = 256;

/// Something happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Emission {
    pub event: Event,
    /// Whatever context the event carries — an app name, a volume percentage.
    /// Reaches a script as `RSBAR_INFO`.
    pub info: Option<String>,
}

impl Emission {
    #[must_use]
    pub fn new(event: Event, info: Option<String>) -> Self {
        Self { event, info }
    }
}

/// Names a source. Carried on every event it produces, so a stray emission can
/// be traced back to what made it.
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
    queue: tokio::sync::mpsc::Sender<Emission>,
    waker: crate::runloop::Waker,
}

impl Emitter {
    /// Queues an event and wakes the app to drain it.
    ///
    /// Never blocks and never fails loudly: a full queue means the daemon is
    /// not draining, which is a bug to see in the log rather than a reason to
    /// stall a system callback.
    pub fn send(&self, emission: Emission) {
        match self.queue.try_send(emission) {
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
    events: tokio::sync::mpsc::Receiver<Emission>,
    /// Dropped on whichever thread owns this feed, which is the thread that
    /// registered. Never read.
    _registration: Registration,
}

impl Feed {
    /// Wraps a registration and its receiver.
    fn new(
        id: SourceId,
        events: tokio::sync::mpsc::Receiver<Emission>,
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
    pub fn try_next(&mut self) -> Option<Emission> {
        self.events.try_recv().ok()
    }

    /// Waits for the next event, or `None` once the source has stopped.
    ///
    /// Unused by the current runner, but this is what makes the producers'
    /// `try_send` a wakeup rather than a write into a void — and it is the
    /// shape an executor would poll.
    pub async fn next(&mut self) -> Option<Emission> {
        self.events.recv().await
    }
}

impl futures_lite::Stream for Feed {
    type Item = Emission;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Emission>> {
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

#[derive(Debug, thiserror::Error)]
#[error("could not start the {name} event source: {reason}")]
pub struct StartError {
    /// Named `name` rather than `source`: thiserror reserves a field called
    /// `source` for an underlying error, which this is not.
    pub name: &'static str,
    pub reason: String,
}

/// One system facility, turned into events.
pub trait Source: Send {
    /// Identifies this source in logs, errors and thread names.
    fn id(&self) -> SourceId;

    /// The events this source can produce. The registry matches a subscription
    /// against this to decide what to start.
    fn provides(&self) -> Vec<Event>;

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

/// Starts a source, giving back the feed it produces.
///
/// Registration happens here, on the caller's thread, which is the main one.
/// Whatever the source hands back is held inside the feed and dropped when the
/// feed is — deregistering on the same thread that registered.
///
/// # Errors
///
/// Returns [`StartError`] if the framework refuses.
pub fn start(
    mut source: Box<dyn Source>,
    waker: &crate::runloop::Waker,
) -> Result<Feed, StartError> {
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
    entries: Vec<Entry>,
    /// Events with no framework behind them — a click, or `--trigger`.
    manual: Feed,
    emit: Emitter,
    /// Handed to every source so its events wake the app rather than waiting
    /// for the next tick.
    waker: crate::runloop::Waker,
}

struct Entry {
    source: Option<Box<dyn Source>>,
    provides: Vec<Event>,
    id: SourceId,
    feed: Option<Feed>,
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
            Box::new(volume::Volume),
        ];

        let entries = sources
            .into_iter()
            .map(|source| Entry {
                id: source.id(),
                provides: source.provides(),
                source: Some(source),
                feed: None,
            })
            .collect();

        let (emit, manual) = Feed::manual(SourceId("local"), waker.clone());
        Self {
            entries,
            manual,
            emit,
            waker,
        }
    }

    /// A sink for events this process observes outside any source.
    #[must_use]
    pub fn emitter(&self) -> Emitter {
        self.emit.clone()
    }

    /// Starts the sources worth running whether or not anything subscribed.
    ///
    /// The workspace notifications are five observers on one notification
    /// centre. Displays and the config are here not because anything subscribed
    /// but because the bar's own geometry and contents depend on them.
    pub fn start_eager(&mut self) {
        self.ensure(&Event::FrontAppSwitched);
        self.ensure(&Event::DisplayChanged);
        self.ensure(&Event::ConfigReloaded);
    }

    /// Starts whatever provides `event`, if it is not running already.
    ///
    /// Cheap to call repeatedly: a subscription change runs this over every
    /// event an item asked for.
    pub fn ensure(&mut self, event: &Event) {
        for entry in &mut self.entries {
            if entry.feed.is_some() || !entry.provides.contains(event) {
                continue;
            }
            let Some(source) = entry.source.take() else {
                continue;
            };
            match start(source, &self.waker) {
                Ok(feed) => {
                    tracing::debug!(source = %entry.id, %event, "started event source");
                    entry.feed = Some(feed);
                }
                Err(err) => tracing::warn!(%err, "event source unavailable"),
            }
        }
    }

    /// Starts everything needed for a set of subscriptions at once.
    pub fn ensure_all<'a>(&mut self, events: impl IntoIterator<Item = &'a Event>) {
        for event in events {
            self.ensure(event);
        }
    }

    /// Takes everything queued across every running feed, without waiting.
    ///
    /// Each emission is tagged with the source that produced it, so an event
    /// arriving from somewhere unexpected is traceable rather than anonymous.
    pub fn drain(&mut self) -> Vec<(SourceId, Emission)> {
        let mut drained = Vec::new();
        let feeds = self.entries.iter_mut().filter_map(|e| e.feed.as_mut());
        for feed in feeds.chain(std::iter::once(&mut self.manual)) {
            let id = feed.id();
            while let Some(emission) = feed.try_next() {
                drained.push((id, emission));
            }
        }
        drained
    }
}

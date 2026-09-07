//! Where events come from.
//!
//! Each system facility the bar reacts to — the workspace, the power source,
//! the audio device — is a [`Source`]. The [`Registry`] owns them as boxes, so
//! the daemon never has to know which framework is behind a given event.
//!
//! **A source runs on its own thread, and reaches the rest of the daemon only
//! through an async channel.** The frameworks behind these are all callback
//! driven and all want a run loop, so each source gets one of its own. Nothing
//! a source does can stall drawing, and a source that wedges takes only its own
//! thread with it.
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

use objc2_core_foundation::{CFRetained, CFRunLoop};
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

/// The end anything producing events writes to.
///
/// Cloneable and `Send`: source threads hold one each, and so does the main
/// thread for the events only it can observe.
pub type Emitter = tokio::sync::mpsc::Sender<Emission>;

/// The end the schedule drains.
///
/// A newtype rather than the bare receiver because `tokio`'s takes `&mut self`
/// to receive, while the draining system holds the inbox immutably. The
/// `try_lock` is uncontended by construction — exactly one thread ever drains —
/// so it is a borrow adapter, not synchronisation.
#[derive(Debug)]
pub struct Events(tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Emission>>);

impl Events {
    fn new(receiver: tokio::sync::mpsc::Receiver<Emission>) -> Self {
        Self(tokio::sync::Mutex::new(receiver))
    }

    /// Takes the next event if one is queued.
    ///
    /// Returns `Err` when the queue is empty, so a caller drains with
    /// `while let Ok(event) = events.try_recv()`.
    ///
    /// # Errors
    ///
    /// Returns [`tokio::sync::mpsc::error::TryRecvError`] when nothing is
    /// queued, or when every sender has been dropped.
    pub fn try_recv(&self) -> Result<Emission, tokio::sync::mpsc::error::TryRecvError> {
        use tokio::sync::mpsc::error::TryRecvError;
        let Ok(mut receiver) = self.0.try_lock() else {
            // Someone else is draining; from this caller's point of view there
            // is nothing to take right now.
            return Err(TryRecvError::Empty);
        };
        receiver.try_recv()
    }

    /// Waits for the next event.
    ///
    /// Unused by the `CFRunLoop`-driven runner, which drains on each wake, but
    /// this is the shape an executor would poll — and it is what makes the
    /// producers' `try_send` a wakeup rather than a write into a void.
    pub async fn recv(&self) -> Option<Emission> {
        self.0.lock().await.recv().await
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
///
/// `install` runs on the source's own thread, with a run loop already current.
/// It registers whatever observers it needs and returns; the thread then pumps
/// the run loop until the channel closes. Observers are torn down by dropping
/// whatever `install` returns, which happens on that same thread.
pub trait Source: Send {
    /// A name for logs, errors and the thread.
    fn name(&self) -> &'static str;

    /// The events this source can produce. The registry matches a subscription
    /// against this to decide what to start.
    fn provides(&self) -> Vec<Event>;

    /// Whether this source must be installed on the main thread.
    ///
    /// `NSWorkspace`'s notification centre is one: an observer registered from
    /// any other thread is accepted and then silently never fires. Such a
    /// source does not need a run loop of its own either — it uses the main
    /// one, which the app's runner is already pumping.
    ///
    /// Everything else gets its own thread, so nothing it does can stall
    /// drawing.
    fn needs_main_thread(&self) -> bool {
        false
    }

    /// Registers observers against the current thread's run loop.
    ///
    /// The returned value is kept alive for as long as the source runs, and
    /// dropped on this thread when it stops — which is what deregisters.
    ///
    /// # Errors
    ///
    /// Returns [`StartError`] if the underlying framework refuses. A source
    /// that cannot start is dropped rather than retried: these fail for
    /// structural reasons, not transient ones.
    fn install(&mut self, emit: Emitter) -> Result<Box<dyn std::any::Any>, StartError>;
}

/// A run loop belonging to another thread, so it can be stopped from this one.
struct RemoteRunLoop(CFRetained<CFRunLoop>);

// SAFETY: `CFRunLoopStop` is one of the two calls `CoreFoundation` documents as
// safe from any thread, and it is all this exposes.
unsafe impl Send for RemoteRunLoop {}

/// A source that is running, and the means to stop it.
enum Running {
    /// Installed here. The observers are dropped when this is, which is on the
    /// same thread that registered them.
    Inline(#[allow(dead_code)] Box<dyn std::any::Any>),
    /// Installed on a thread of its own.
    Threaded {
        run_loop: Option<RemoteRunLoop>,
        thread: Option<std::thread::JoinHandle<()>>,
    },
}

impl Drop for Running {
    fn drop(&mut self) {
        let Self::Threaded { run_loop, thread } = self else {
            return;
        };
        // Stopping the run loop is what lets the thread fall out of its loop
        // and drop the observers on the thread that registered them.
        if let Some(run_loop) = run_loop.take() {
            run_loop.0.stop();
        }
        if let Some(thread) = thread.take() {
            let _ = thread.join();
        }
    }
}

/// Holds every source and starts them on demand.
pub struct Registry {
    emit: Emitter,
    entries: Vec<Entry>,
}

struct Entry {
    source: Option<Box<dyn Source>>,
    provides: Vec<Event>,
    name: &'static str,
    running: Option<Running>,
}

impl Registry {
    /// Builds the registry over the sources this build knows about, and hands
    /// back the stream every one of them writes into.
    #[must_use]
    pub fn new(config: crate::config::Shared) -> (Self, Events) {
        // Bounded: a producer outrunning the daemon is misbehaving, and an
        // unbounded queue would turn that into unbounded memory. Producers use
        // `try_send` and drop on a full queue rather than blocking, because
        // every one of them is on a callback the system wants back promptly.
        let (emit, events) = tokio::sync::mpsc::channel(QUEUE_DEPTH);

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
                name: source.name(),
                provides: source.provides(),
                source: Some(source),
                running: None,
            })
            .collect();

        (Self { emit, entries }, Events::new(events))
    }

    /// A sink for events this process observes outside any source — clicks,
    /// and anything a client triggers.
    #[must_use]
    pub fn emitter(&self) -> Emitter {
        self.emit.clone()
    }

    /// Starts the sources cheap enough to run whether or not anything
    /// subscribed. The workspace notifications are five observers on one
    /// notification centre; nothing else is in that class.
    pub fn start_eager(&mut self) {
        self.ensure(&Event::FrontAppSwitched);
        // Not because anything subscribed, but because the bar's own geometry
        // depends on it: a monitor appearing has to reach the panels whether or
        // not a config ever mentions `display_changed`.
        self.ensure(&Event::DisplayChanged);
        // Likewise: the config drives everything, so it is watched whether or
        // not anything subscribed to hearing about it.
        self.ensure(&Event::ConfigReloaded);
    }

    /// Starts whatever provides `event`, if it is not running already.
    ///
    /// Cheap to call repeatedly: a subscription change runs this over every
    /// event an item asked for.
    pub fn ensure(&mut self, event: &Event) {
        for entry in &mut self.entries {
            if entry.running.is_some() || !entry.provides.contains(event) {
                continue;
            }
            let Some(mut source) = entry.source.take() else {
                continue;
            };
            // A main-thread source installs here, on the caller's thread, and
            // uses the run loop the app is already pumping. Everything else
            // gets a thread of its own.
            let started = if source.needs_main_thread() {
                source.install(self.emit.clone()).map(Running::Inline)
            } else {
                spawn(source, self.emit.clone())
            };
            match started {
                Ok(running) => {
                    tracing::debug!(source = entry.name, %event, "started event source");
                    entry.running = Some(running);
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
}

/// Runs a source on its own thread and waits for it to say whether it started.
fn spawn(mut source: Box<dyn Source>, emit: Emitter) -> Result<Running, StartError> {
    let name = source.name();
    // The thread reports back once, so a framework refusing to register is a
    // synchronous error to the caller rather than a silently dead thread.
    // A one-shot: the thread reports once and never again.
    let (ready, started) = tokio::sync::oneshot::channel();

    let thread = std::thread::Builder::new()
        .name(format!("rsbar-source-{name}"))
        .spawn(move || {
            // A run loop with no input sources finishes immediately, and
            // `CFRunLoop::run` returns rather than blocking — which would drop
            // the observers below and silently kill the source. Not every
            // framework adds a source of its own: notify and CoreAudio both
            // run threads of their own and add nothing here. So give the run
            // loop one unconditionally; it is also what `stop` interrupts.
            let keepalive = crate::runloop::Waker::install(|| {});

            let observers = match source.install(emit) {
                Ok(observers) => {
                    let run_loop = CFRunLoop::current().expect("no run loop on a fresh thread");
                    if ready.send(Ok(RemoteRunLoop(run_loop))).is_err() {
                        return;
                    }
                    observers
                }
                Err(err) => {
                    let _ = ready.send(Err(err));
                    return;
                }
            };

            // Returns when someone stops this run loop, which is how the
            // registry shuts a source down.
            CFRunLoop::run();

            // Explicit, so it is obvious the teardown happens here — on the
            // thread that registered — and not wherever the registry lives.
            drop(observers);
            drop(keepalive);
        })
        .map_err(|err| StartError {
            name,
            reason: err.to_string(),
        })?;

    // `blocking_recv` panics inside a tokio runtime. There is none here — this
    // daemon's executor is the CFRunLoop-driven Bevy runner — and this is the
    // registry's thread, not a task.
    match started.blocking_recv() {
        Ok(Ok(run_loop)) => Ok(Running::Threaded {
            run_loop: Some(run_loop),
            thread: Some(thread),
        }),
        Ok(Err(err)) => Err(err),
        Err(_) => Err(StartError {
            name,
            reason: "the source thread died on startup".to_owned(),
        }),
    }
}

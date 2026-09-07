//! Where events come from.
//!
//! Each system facility the bar reacts to — the workspace, the power source,
//! the audio device — is a [`Source`]. The [`Registry`] owns them as boxes, so
//! the daemon never has to know which framework is behind a given event.
//!
//! **A source runs on its own thread, and reaches the rest of the daemon only
//! through an async channel.** The frameworks behind these are all callback
//! driven and all want a run loop, so each source gets one of its own; what
//! comes back out is a channel the tokio runtime awaits alongside everything
//! else. Nothing a source does can stall drawing, and a source that wedges
//! takes only its own thread with it.
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

pub mod power;
pub mod volume;
pub mod workspace;

use objc2_core_foundation::{CFRetained, CFRunLoop};
use rsbar_protocol::Event;

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

/// The end the runtime reads.
pub type Events = tokio::sync::mpsc::Receiver<Emission>;

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
struct Running {
    _name: &'static str,
    run_loop: Option<RemoteRunLoop>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        // Stopping the run loop is what lets the thread fall out of its loop
        // and drop the observers on the thread that registered them.
        if let Some(run_loop) = self.run_loop.take() {
            run_loop.0.stop();
        }
        if let Some(thread) = self.thread.take() {
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
    pub fn new() -> (Self, Events) {
        // Bounded: a producer outrunning the daemon is misbehaving, and an
        // unbounded queue would turn that into unbounded memory. Producers use
        // `try_send` and drop on a full queue rather than blocking, because
        // every one of them is on a callback the system wants back promptly.
        let (emit, events) = tokio::sync::mpsc::channel(256);

        let sources: Vec<Box<dyn Source>> = vec![
            Box::new(workspace::Workspace),
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

        (Self { emit, entries }, events)
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
            let Some(source) = entry.source.take() else {
                continue;
            };
            match spawn(source, self.emit.clone()) {
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
    let (ready, started) = std::sync::mpsc::sync_channel(1);

    let thread = std::thread::Builder::new()
        .name(format!("rsbar-source-{name}"))
        .spawn(move || {
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
        })
        .map_err(|err| StartError {
            name,
            reason: err.to_string(),
        })?;

    match started.recv() {
        Ok(Ok(run_loop)) => Ok(Running {
            _name: name,
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

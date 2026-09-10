//! The IPC [`Dispatcher`]: talks to a running `coolabah` over its Mach
//! service.
//!
//! `Request::Subscribe` already replaces an item's subscriptions when sent
//! plainly; sent through [`SendPort::subscribe_blocking`] it additionally
//! hands the daemon a port to push matching [`Event`]s to. See this crate's
//! top-level docs for the daemon-side change that requires, and
//! [`crate::dispatch`] for the trait this implements.
//!
//! # The port lives on a thread of its own
//!
//! It did not use to: every operation here was `async-mach-ports`' own
//! `async` one, awaited straight from the Lua thread, and nothing was parked
//! anywhere. That is no longer possible from *this* side of the seam.
//! [`Dispatcher`]'s futures are `Send` (mlua's `send` feature — see
//! [`crate::dispatch`]), and `async-mach-ports`' receive future is not: it
//! holds a `&Interest`, and `Interest` keeps its armed flag in a `Cell`. A
//! future that borrows one therefore cannot cross a thread boundary even
//! though nothing ever makes it.
//!
//! So the Mach side moves to one thread, [`Worker`], and this type talks to it
//! over channels that *are* `Send`. That thread is one of the "each of these
//! exists to be parked in exactly one place" kind: it holds no lock, owns no
//! state past the job in its hands, and a config awaiting a reply is awaiting
//! a `oneshot` rather than a port.
//!
//! **This is a workaround, not the design.** Making `Interest::armed` an
//! `AtomicBool` in `async-mach-ports` — it is only ever touched by the one
//! task polling it — makes the original all-`async`, no-thread version
//! compile as-is, and this whole module goes back to four lines of `await`.
//! [`crate::direct`], which is what the daemon actually uses, has no port and
//! no thread either way.

use std::pin::Pin;
use std::sync::OnceLock;
use std::sync::mpsc::{Sender as Chute, channel};
use std::task::{Context, Poll};

use crate::protocol::wire::{MessagePack, Sender};
use crate::protocol::{Event, ItemName, Kind, Request, Response, service_name};
use async_mach_ports::{RecvPort as _, SendPort as _};
use futures_lite::Stream;

use crate::dispatch::{BoxFuture, BoxedEventStream, Dispatcher};
use crate::error::{ApiError, Result};

/// One thing for the worker to do, and where its answer goes.
enum Job {
    Call {
        request: Request,
        answer: tokio::sync::oneshot::Sender<Result<Response>>,
    },
    Subscribe {
        request: Request,
        answer: tokio::sync::oneshot::Sender<Result<Events>>,
    },
}

/// The events one subscription has pushed and the Lua thread has not read.
///
/// Unbounded for the same reason the daemon's own subscriber queues are: the
/// thread draining the port must never wait on the thread handling callbacks,
/// and a config that is mid-callback when three events land should see all
/// three rather than the last.
type Events = tokio::sync::mpsc::UnboundedReceiver<Event>;

pub struct IpcDispatcher {
    service: String,
    /// Started on the first request rather than in [`Self::new`]: building a
    /// dispatcher is what `require("coolabah")` does, and a host that only ever
    /// reads `coolabah._VERSION` should not cost a thread.
    ///
    /// `Mutex<Option<..>>` inside a `OnceLock` would be one lock too many —
    /// the channel's sending end is `Sync` and cloning it is all anyone wants.
    worker: OnceLock<Chute<Job>>,
}

impl IpcDispatcher {
    /// Connects using `COOLABAH_SERVICE`, or the built-in default — the same
    /// resolution `coolabah-cli` uses.
    #[must_use]
    pub fn new() -> Self {
        Self::with_service(service_name())
    }

    #[must_use]
    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            worker: OnceLock::new(),
        }
    }

    /// Hands `job` to the worker, starting it if this is the first one.
    fn submit(&self, job: Job) -> Result<()> {
        let chute = self
            .worker
            .get_or_init(|| Worker::start(self.service.clone()));
        chute.send(job).map_err(|_| ApiError::NotRunning)
    }
}

impl Default for IpcDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

/// What the failed half of a `oneshot` means here.
///
/// The worker only drops an answer by exiting, and it only exits when this
/// dispatcher is gone — so from a caller's point of view it is the same thing
/// as never having reached the daemon at all.
fn lost() -> ApiError {
    ApiError::NotRunning
}

impl Dispatcher for IpcDispatcher {
    fn call(&self, request: Request) -> BoxFuture<'_, Result<Response>> {
        Box::pin(async move {
            let (answer, waiting) = tokio::sync::oneshot::channel();
            self.submit(Job::Call { request, answer })?;
            waiting.await.map_err(|_| lost())?
        })
    }

    fn subscribe(
        &self,
        name: ItemName,
        events: Vec<Kind>,
    ) -> BoxFuture<'_, Result<BoxedEventStream>> {
        Box::pin(async move {
            let (answer, waiting) = tokio::sync::oneshot::channel();
            self.submit(Job::Subscribe {
                request: Request::Subscribe { name, events },
                answer,
            })?;
            let queue = waiting.await.map_err(|_| lost())??;
            Ok(Box::pin(EventStream(queue)) as BoxedEventStream)
        })
    }
}

/// The one thread that touches a Mach port.
struct Worker;

impl Worker {
    /// Starts it, and hands back the end jobs go in.
    fn start(service: String) -> Chute<Job> {
        let (chute, jobs) = channel::<Job>();
        let spawned = std::thread::Builder::new()
            .name("coolabah-lua-ipc".into())
            .spawn(move || {
                // Ends when the dispatcher is dropped, which drops the last
                // sending end.
                while let Ok(job) = jobs.recv() {
                    match job {
                        Job::Call { request, answer } => {
                            drop(answer.send(call(&service, &request)));
                        }
                        Job::Subscribe { request, answer } => {
                            drop(answer.send(subscribe(&service, &request)));
                        }
                    }
                }
            });
        if let Err(err) = spawned {
            tracing::error!(%err, "could not start the IPC thread");
        }
        chute
    }
}

/// One round trip. Reconnects per call rather than holding a `Sender`, which
/// is what lets a config keep working across a daemon restart — the send
/// right a previous lookup handed back names a port that no longer exists.
fn call(service: &str, request: &Request) -> Result<Response> {
    let sender = Sender::<Request>::connect(service, MessagePack)?;
    match sender.call_blocking::<Response>(request)? {
        Response::Error(message) => Err(ApiError::Rejected(message)),
        other => Ok(other),
    }
}

/// Opens one subscription and starts draining it.
///
/// No reply travels with this send (the extra port takes the reply port's
/// place on the wire), so a rejected subscription can only show up as the
/// stream ending with nothing ever having arrived on it.
fn subscribe(service: &str, request: &Request) -> Result<Events> {
    let sender = Sender::<Request>::connect(service, MessagePack)?;
    let receiver = sender.subscribe_blocking::<Event>(request)?;
    let (queued, events) = tokio::sync::mpsc::unbounded_channel();

    // A thread per subscription, parked in the port's own receive: the same
    // shape, for the same reason, as the daemon's per-subscriber drain
    // thread. It cannot go on the worker thread above, which has to stay free
    // to answer the next `coolabah.set`.
    let spawned = std::thread::Builder::new()
        .name("coolabah-lua-events".into())
        .spawn(move || {
            loop {
                match receiver.recv_blocking() {
                    Ok(delivery) => {
                        // The reader is gone, so nothing wants the port either.
                        if queued.send(delivery.value).is_err() {
                            return;
                        }
                    }
                    Err(async_mach_ports::Error::PeerGone) => return,
                    // About this one message, not the channel.
                    Err(err) => tracing::warn!(%err, "dropping an undecodable event"),
                }
            }
        });
    if let Err(err) = spawned {
        tracing::error!(%err, "could not start an event thread");
        return Err(ApiError::NotRunning);
    }
    Ok(events)
}

/// Adapts the drain thread's queue into the plain `Stream<Item = Event>`
/// [`Dispatcher::subscribe`] promises, ending when the daemon is gone.
struct EventStream(Events);

impl Stream for EventStream {
    type Item = Event;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Event>> {
        self.0.poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::IpcDispatcher;
    use crate::dispatch::Dispatcher;

    /// The whole reason this module was rewritten: mlua's `send` feature
    /// makes every registered callback `Send`, so the dispatcher one holds
    /// has to be.
    #[test]
    fn the_ipc_dispatcher_can_be_shared_across_threads() {
        fn assert<T: Send + Sync>() {}
        assert::<IpcDispatcher>();
        assert::<std::sync::Arc<dyn Dispatcher>>();
    }

    /// Nothing is spawned until something is actually asked for — a host that
    /// only reads `coolabah._VERSION` should not cost a thread.
    #[test]
    fn no_thread_starts_until_the_first_request() {
        let dispatcher = IpcDispatcher::with_service(concat!(name!(service), ".test.unused"));
        assert!(
            dispatcher.worker.get().is_none(),
            "constructing a dispatcher started a thread"
        );
    }
}

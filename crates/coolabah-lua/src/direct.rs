//! The no-IPC [`Dispatcher`]: for Lua hosted *by* the daemon.
//!
//! [`crate::ipc::IpcDispatcher`] exists because the config is in another
//! process. When it is not — when the `Lua` state belongs to `coolabah` itself —
//! every part of that is waste: a [`Request`] is `MessagePack`-encoded, copied
//! into a Mach message, carried through the kernel to a port the daemon's own
//! run loop is watching, decoded, and applied; then a [`Response`] makes the
//! same trip back. Both ends are the same process. Nothing about that crossing
//! is load-bearing.
//!
//! This module is the crossing with all of that taken out. A [`Request`]
//! *value* goes into a queue, and the daemon's request pass takes it and calls
//! `requests::apply` on it directly. An [`Event`] *value* comes back the same
//! way. There is no wire format anywhere in this file, no service name, no
//! port, and no `async_mach_ports` import — that absence is the whole point,
//! and a `use` of any of them here would mean the change had been undone.
//!
//! # The two halves
//!
//! [`Channel`] is one object with two faces, both cheap to clone:
//!
//! * [`DirectDispatcher`] is what [`crate::api::install`] is given, and is a
//!   [`Dispatcher`] like any other, so the Lua table built over it is the
//!   identical table — same verbs, same arguments, same errors.
//! * [`Channel::drain`] and [`Channel::deliver`] are the daemon's side: drain
//!   the queued calls in the pass that has `&mut World`, answer each one, and
//!   push an item's events to whoever subscribed.
//!
//! # Waiting without blocking, on either thread
//!
//! A [`Call`] carries a [`tokio::sync::oneshot`] sender, and `call` awaits the
//! receiving end. Nothing parks: the Lua task suspends and the thread it was
//! on goes back to whatever else it had. That works whether the host runs Lua
//! on the run loop's own `LocalExecutor` — the reply lands in the same pass
//! that applied the request, and the wake is a run loop signal — or on a
//! thread of its own, where the wake crosses threads and costs one futex.
//! Neither shape is baked in here, which is what lets the daemon choose.
//!
//! [`Channel::notify`] is the other half of that: queueing a call has to bring
//! a pass about, or a request would sit there until something unrelated woke
//! the daemon. It is a callback rather than a call into the app for the same
//! reason [`crate::ipc`]'s daemon-side counterpart takes one — the queue
//! should know nothing about what drains it.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use crate::protocol::{Event, ItemName, Kind, Request, Response};
use futures_lite::Stream;

use crate::dispatch::{BoxFuture, BoxedEventStream, Dispatcher};
use crate::error::{ApiError, Result};

/// One queued request, and where its answer goes.
///
/// The daemon gets this out of [`Channel::drain`], applies `request`, and
/// calls [`Call::answer`]. Answering consumes it, which is the same shape as
/// the Mach `Reply` it replaces and true for the same reason: one request has
/// exactly one answer.
pub struct Call {
    pub request: Request,
    answer: tokio::sync::oneshot::Sender<Response>,
}

impl Call {
    /// Hands `response` back to the config that asked.
    ///
    /// A dropped answer is not an error: it means the config stopped waiting
    /// — a reload took its `Lua` down mid-request — which is the same thing a
    /// client exiting mid-call means over a port.
    pub fn answer(self, response: Response) {
        if self.answer.send(response).is_err() {
            tracing::debug!("a config stopped waiting for its answer");
        }
    }
}

/// Where one subscribed item's events are queued for the config.
///
/// Unbounded, like the daemon's own per-subscriber queue and for the identical
/// reason: [`Channel::deliver`] runs on the thread that draws, so it must
/// never wait on a config that is mid-callback, and dropping the event instead
/// would lose the `mouse.exited` that had to follow a `mouse.entered`.
type Feed = tokio::sync::mpsc::UnboundedSender<Event>;

/// What both halves share. Not built directly — see [`Channel::new`].
#[derive(Default)]
struct Shared {
    queued: Mutex<Vec<Call>>,
    feeds: Mutex<HashMap<ItemName, Feed>>,
}

/// The seam between a config and the daemon hosting it.
///
/// Cloning is an [`Arc`] bump; every clone is the same channel.
#[derive(Clone)]
pub struct Channel {
    shared: Arc<Shared>,
    /// Asks the host for a pass. See the module docs.
    notify: Arc<dyn Fn() + Send + Sync>,
}

impl Channel {
    /// A channel that calls `notify` whenever a request is queued.
    #[must_use]
    pub fn new(notify: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            shared: Arc::default(),
            notify: Arc::new(notify),
        }
    }

    /// The end a `Lua` state is built over.
    #[must_use]
    pub fn dispatcher(&self) -> DirectDispatcher {
        DirectDispatcher {
            channel: self.clone(),
        }
    }

    /// Takes everything queued since the last drain, oldest first.
    ///
    /// Everything, not one: a config load is hundreds of requests, and the
    /// pass should see them as one update rather than one each — the same
    /// reason [`crate::ipc`]'s daemon-side queue exists at all.
    ///
    /// The lock is released before the caller touches what it got, because
    /// applying a request can re-enter the host and a config can queue more.
    #[must_use]
    pub fn drain(&self) -> Vec<Call> {
        let mut queued = self
            .shared
            .queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut queued)
    }

    /// How many calls are waiting. Only the tests ask.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.shared
            .queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Pushes one event to the config, if it subscribed to this item.
    ///
    /// Reports whether it was taken, matching the daemon's own
    /// `Subscribers::push`: an item a config is handling itself must not also
    /// have its shell script forked. A config that has gone is forgotten here
    /// rather than anywhere else, so its item goes back to being nobody's.
    pub fn deliver(&self, item: &ItemName, event: &Event) -> bool {
        let mut feeds = self
            .shared
            .feeds
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(feed) = feeds.get(item) else {
            return false;
        };
        if feed.send(event.clone()).is_ok() {
            return true;
        }
        feeds.remove(item);
        false
    }

    /// Whether anything is subscribed to `item` — what a host checks before
    /// building an event it would otherwise throw away.
    #[must_use]
    pub fn is_subscribed(&self, item: &ItemName) -> bool {
        self.shared
            .feeds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(item)
    }

    /// Drops every subscription, so a config being replaced stops receiving
    /// before its successor starts.
    pub fn forget_all(&self) {
        self.shared
            .feeds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// Queues `request` and asks for a pass, handing back what to await.
    fn submit(&self, request: Request) -> tokio::sync::oneshot::Receiver<Response> {
        let (answer, waiting) = tokio::sync::oneshot::channel();
        self.shared
            .queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Call { request, answer });
        // After the push, never before: the pass this asks for must not arrive
        // to find the queue still empty.
        (self.notify)();
        waiting
    }

    /// Registers `item`'s feed, replacing whatever it was.
    ///
    /// Replacing rather than adding mirrors [`Request::Subscribe`], which
    /// replaces an item's subscriptions rather than adding to them.
    fn open(&self, item: ItemName) -> tokio::sync::mpsc::UnboundedReceiver<Event> {
        let (feed, events) = tokio::sync::mpsc::unbounded_channel();
        self.shared
            .feeds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(item, feed);
        events
    }
}

/// The [`Dispatcher`] a daemon-hosted `Lua` state is installed over.
#[derive(Clone)]
pub struct DirectDispatcher {
    channel: Channel,
}

impl Dispatcher for DirectDispatcher {
    fn call(&self, request: Request) -> BoxFuture<'_, Result<Response>> {
        let waiting = self.channel.submit(request);
        Box::pin(async move {
            // The host is gone -- the app exited while a config was mid-call.
            // Nothing distinguishes that from a daemon that was never there,
            // and a config should be told the same thing either way.
            match waiting.await.map_err(|_| ApiError::NotRunning)? {
                Response::Error(message) => Err(ApiError::Rejected(message)),
                other => Ok(other),
            }
        })
    }

    fn subscribe(
        &self,
        name: ItemName,
        events: Vec<Kind>,
    ) -> BoxFuture<'_, Result<BoxedEventStream>> {
        // Opened before the request is queued, so an event the daemon emits
        // while applying the subscription has somewhere to land.
        let queue = self.channel.open(name.clone());
        let waiting = self.channel.submit(Request::Subscribe { name, events });
        Box::pin(async move {
            match waiting.await.map_err(|_| ApiError::NotRunning)? {
                Response::Error(message) => Err(ApiError::Rejected(message)),
                _ => Ok(Box::pin(EventStream(queue)) as BoxedEventStream),
            }
        })
    }
}

/// The queue, as the plain `Stream<Item = Event>` [`Dispatcher::subscribe`]
/// promises. Ends when the daemon drops the feed.
struct EventStream(tokio::sync::mpsc::UnboundedReceiver<Event>);

impl Stream for EventStream {
    type Item = Event;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Event>> {
        self.0.poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::{Channel, Dispatcher as _};
    use crate::protocol::{Event, ItemName, Kind, Request, Response, event};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn name(text: &str) -> ItemName {
        ItemName::new(text).expect("a valid item name")
    }

    /// A runtime for the one thing these tests await: the reply to a call the
    /// test itself answers.
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
    }

    #[test]
    fn a_request_reaches_the_host_as_a_value_and_its_answer_comes_back() {
        let channel = Channel::new(|| {});
        let dispatcher = channel.dispatcher();

        let waiting = runtime().block_on(async {
            let pending = dispatcher.call(Request::UpdateAll);
            // The host's side of the pass: take what was queued, answer it.
            let mut drained = channel.drain();
            assert_eq!(drained.len(), 1, "the request was queued");
            let call = drained.pop().expect("one call");
            assert_eq!(call.request, Request::UpdateAll, "unencoded, as a value");
            call.answer(Response::Ok);
            pending.await
        });

        assert!(matches!(waiting, Ok(Response::Ok)));
    }

    #[test]
    fn queueing_a_request_asks_the_host_for_a_pass() {
        let woken = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&woken);
        let channel = Channel::new(move || {
            counted.fetch_add(1, Ordering::Relaxed);
        });
        let dispatcher = channel.dispatcher();

        let _first = dispatcher.call(Request::UpdateAll);
        let _second = dispatcher.call(Request::Reload);

        assert_eq!(woken.load(Ordering::Relaxed), 2, "one wake per request");
        assert_eq!(channel.pending(), 2, "and both are waiting for one pass");
    }

    /// The point of draining rather than popping: a config load is hundreds of
    /// requests and the pass should see them together.
    #[test]
    fn a_burst_of_requests_drains_in_one_go() {
        let channel = Channel::new(|| {});
        let dispatcher = channel.dispatcher();

        let pending: Vec<_> = (0..32)
            .map(|_| dispatcher.call(Request::UpdateAll))
            .collect();
        assert_eq!(channel.drain().len(), 32, "nothing was lost on the way in");
        assert!(channel.drain().is_empty(), "and the queue is empty after");
        drop(pending);
    }

    #[test]
    fn an_error_response_reaches_the_config_as_a_rejection() {
        let channel = Channel::new(|| {});
        let dispatcher = channel.dispatcher();

        let answered = runtime().block_on(async {
            let pending = dispatcher.call(Request::UpdateAll);
            channel
                .drain()
                .pop()
                .expect("one call")
                .answer(Response::Error("no item named `nope`".into()));
            pending.await
        });

        let err = answered.expect_err("an error response is an error");
        assert!(err.to_string().contains("no item named `nope`"), "{err}");
    }

    #[test]
    fn a_subscribed_item_receives_events_pushed_straight_to_it() {
        use futures_lite::StreamExt as _;

        let channel = Channel::new(|| {});
        let dispatcher = channel.dispatcher();
        let item = name("clock");

        let seen = runtime().block_on(async {
            let opening = dispatcher.subscribe(item.clone(), vec![Kind::Forced]);
            channel
                .drain()
                .pop()
                .expect("the subscribe")
                .answer(Response::Ok);
            let mut stream = opening.await.expect("a stream");

            assert!(
                channel.deliver(&item, &Event::Forced(event::Forced {})),
                "a subscribed item takes its event"
            );
            stream.next().await
        });

        assert!(matches!(seen, Some(Event::Forced(_))));
    }

    #[test]
    fn an_event_for_an_item_nobody_subscribed_to_is_not_taken() {
        let channel = Channel::new(|| {});
        assert!(
            !channel.deliver(&name("nobody"), &Event::Forced(event::Forced {})),
            "so the daemon runs that item's script instead"
        );
    }

    /// A config being replaced must stop receiving before its successor
    /// starts, or the old callbacks run against the new bar.
    #[test]
    fn forgetting_every_subscription_sends_events_back_to_the_scripts() {
        let channel = Channel::new(|| {});
        let dispatcher = channel.dispatcher();
        let item = name("clock");

        runtime().block_on(async {
            let opening = dispatcher.subscribe(item.clone(), vec![Kind::Forced]);
            channel
                .drain()
                .pop()
                .expect("the subscribe")
                .answer(Response::Ok);
            drop(opening.await.expect("a stream"));
        });

        assert!(channel.is_subscribed(&item));
        channel.forget_all();
        assert!(!channel.is_subscribed(&item));
        assert!(!channel.deliver(&item, &Event::Forced(event::Forced {})));
    }
}

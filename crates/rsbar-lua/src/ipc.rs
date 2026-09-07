//! The IPC [`Dispatcher`]: talks to a running `rsbard` over its Mach
//! service, using `async-mach-ports`' async operations throughout — nothing
//! here parks a thread.
//!
//! `Request::Subscribe` already replaces an item's subscriptions when sent
//! plainly; sent through [`SendPort::subscribe`] it additionally hands the
//! daemon a port to push matching [`Event`]s to. See this crate's top-level
//! docs for the daemon-side change that requires, and [`crate::dispatch`] for
//! the trait this implements.

use std::pin::Pin;
use std::task::{Context, Poll};

use async_mach_ports::{SendPort, Sender};
use futures_lite::Stream;
use rsbar_protocol::{Event, ItemName, Kind, Request, Response, service_name};

use crate::dispatch::{BoxedEventStream, Dispatcher, LocalBoxFuture};
use crate::error::{ApiError, Result};

pub struct IpcDispatcher {
    service: String,
}

impl IpcDispatcher {
    /// Connects using `RSBAR_SERVICE`, or the built-in default — the same
    /// resolution `rsbar-cli` uses.
    #[must_use]
    pub fn new() -> Self {
        Self {
            service: service_name(),
        }
    }

    #[must_use]
    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }
}

impl Default for IpcDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Dispatcher for IpcDispatcher {
    fn call(&self, request: Request) -> LocalBoxFuture<'_, Result<Response>> {
        Box::pin(async move {
            // A bootstrap lookup, not a round trip to the daemon: local IPC
            // to launchd, done synchronously exactly as `rsbar-cli` does it.
            let sender = Sender::<Request>::connect(&self.service)?;
            match sender.call::<Response>(&request).await? {
                Response::Error(message) => Err(ApiError::Rejected(message)),
                other => Ok(other),
            }
        })
    }

    fn subscribe(
        &self,
        name: ItemName,
        events: Vec<Kind>,
    ) -> LocalBoxFuture<'_, Result<BoxedEventStream>> {
        Box::pin(async move {
            let sender = Sender::<Request>::connect(&self.service)?;
            let request = Request::Subscribe { name, events };
            // No reply travels with this send (the extra port takes the
            // reply port's place on the wire), so a rejected subscription
            // can only show up as the stream ending with nothing ever having
            // arrived on it.
            let receiver = sender.subscribe::<Event>(&request).await?;
            Ok(Box::pin(EventStream {
                receiver,
                done: false,
            }) as BoxedEventStream)
        })
    }
}

/// Adapts `Receiver<Event>` (a stream of `Result<Delivery<Event>>` that never
/// ends on its own) into a plain `Stream<Item = Event>` that ends when the
/// daemon is gone, decoding failures skipped rather than treated as the end.
struct EventStream {
    receiver: async_mach_ports::Receiver<Event>,
    done: bool,
}

impl Stream for EventStream {
    type Item = Event;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Event>> {
        if self.done {
            return Poll::Ready(None);
        }
        loop {
            match Pin::new(&mut self.receiver).poll_next(cx) {
                Poll::Ready(Some(Ok(delivery))) => return Poll::Ready(Some(delivery.value)),
                Poll::Ready(Some(Err(async_mach_ports::Error::PeerGone)) | None) => {
                    self.done = true;
                    return Poll::Ready(None);
                }
                Poll::Ready(Some(Err(err))) => {
                    tracing::warn!(%err, "dropping an undecodable event");
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

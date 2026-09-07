//! The one seam between the API and its host.
//!
//! [`crate::api::install`] is written entirely against [`Dispatcher`], and
//! entirely `async`: nothing here blocks a thread waiting on a reply. The IPC
//! implementation ([`crate::ipc::IpcDispatcher`]) is this crate's; an
//! embedded implementation, calling straight into the daemon's own request
//! handling with no IPC at all, is the daemon's to write against the same
//! trait. Both make the exact same Lua table behave identically from a
//! script's point of view.
//!
//! Neither method requires `Send`: a call always originates from, and its
//! future is always polled on, the single thread hosting the `Lua` state —
//! Lua's C API is not reentrant across threads, so there is never a second
//! thread that could need to touch a [`Dispatcher`]. This is also why the
//! futures are boxed as local (`dyn Future<Output = _> + '_`, not `+ Send`):
//! it lets an implementation hold non-`Send` state (an `Rc`, a borrowed
//! connection) with no wrapper required.

use std::future::Future;
use std::pin::Pin;

use futures_lite::Stream;
use rsbar_protocol::{Event, ItemName, Kind, Request, Response};

use crate::error::Result;

/// A future tied to the dispatcher it came from, not required to be `Send`.
pub type LocalBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// What [`Dispatcher::subscribe`] hands back: every [`Event`] the daemon
/// pushes for that item, in order, for as long as the subscription lasts.
pub type BoxedEventStream = Pin<Box<dyn Stream<Item = Event>>>;

pub trait Dispatcher {
    /// Sends `request` and waits for the daemon's answer, without blocking
    /// the calling thread while it waits.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::ApiError::Rejected`] if the daemon understood
    /// the request but refused it, and a transport error if it could not be
    /// delivered at all.
    fn call(&self, request: Request) -> LocalBoxFuture<'_, Result<Response>>;

    /// Registers `name` for `events`, replacing whatever it was previously
    /// subscribed to (mirrors [`Request::Subscribe`]), and resolves to a
    /// stream of the matching events pushed to it from then on.
    ///
    /// Called once per item, when [`crate::events::Registry::run`] starts —
    /// not per `item:subscribe` call, which only records interest locally
    /// until the loop opens the stream with the full, final set of kinds
    /// that item cares about.
    ///
    /// # Errors
    ///
    /// Same as [`Self::call`].
    fn subscribe(
        &self,
        name: ItemName,
        events: Vec<Kind>,
    ) -> LocalBoxFuture<'_, Result<BoxedEventStream>>;
}

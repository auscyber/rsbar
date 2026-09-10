//! The one seam between the API and its host.
//!
//! [`crate::api::install`] is written entirely against [`Dispatcher`], and
//! entirely `async`: nothing here blocks a thread waiting on a reply. The IPC
//! implementation ([`crate::ipc::IpcDispatcher`]) is this crate's, and so is
//! the no-IPC one ([`crate::direct::DirectDispatcher`]) a host that *is* the
//! daemon uses instead. Both make the exact same Lua table behave identically
//! from a script's point of view.
//!
//! # Why `Send`, and what it does not mean
//!
//! mlua is built here with `feature = "send"`, so a registered callback must
//! be `Send` and a `UserData` must be `Send + Sync`. A [`Dispatcher`] is
//! reached from both, so it inherits the requirement, and its futures are
//! [`BoxFuture`] rather than local.
//!
//! That is not a claim that anything here runs concurrently. Lua's C API is
//! not reentrant and mlua guards the VM with a reentrant mutex, so exactly one
//! thread is ever inside a chunk. What `Send` buys is that the *whole* Lua
//! state, the dispatcher included, can live on a thread the host chooses —
//! which for the daemon means a thread that is not the one that draws.
//! Concurrency comes from the work a config waits on (a subprocess, a timer,
//! the next event) being off that thread, not from two chunks running at once.

use std::future::Future;
use std::pin::Pin;

use crate::protocol::{Event, ItemName, Kind, Request, Response};
use futures_lite::Stream;

use crate::error::Result;

/// A future tied to the dispatcher it came from.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What [`Dispatcher::subscribe`] hands back: every [`Event`] the daemon
/// pushes for that item, in order, for as long as the subscription lasts.
pub type BoxedEventStream = Pin<Box<dyn Stream<Item = Event> + Send>>;

pub trait Dispatcher: Send + Sync {
    /// Sends `request` and waits for the daemon's answer, without blocking
    /// the calling thread while it waits.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::ApiError::Rejected`] if the daemon understood
    /// the request but refused it, and a transport error if it could not be
    /// delivered at all.
    fn call(&self, request: Request) -> BoxFuture<'_, Result<Response>>;

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
    ) -> BoxFuture<'_, Result<BoxedEventStream>>;
}

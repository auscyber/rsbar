//! Where a pushed [`Event`] turns into a Lua callback call.
//!
//! `item:subscribe(kind, callback)` only ever touches [`Registry::subscribe`]
//! — it records the callback. Opening every item's channel and actually
//! dispatching events is [`Registry::run`], called once by `coolabah.run()`.
//!
//! Every item's stream is merged into one [`SelectAll`] and driven from a
//! single `async fn` — no threads, no cross-thread channel. That is only
//! possible because `Dispatcher` is `async`-all-the-way-down: awaiting N
//! streams concurrently on one thread is what an executor is for, and both
//! dispatchers' streams are already waker-driven, so this costs nothing while
//! idle.
//!
//! The callback map is behind a [`Mutex`] rather than a `RefCell` because
//! mlua's `send` feature makes every registered callback `Send`, so the
//! registry an `item:subscribe` closure holds has to be too. It is never
//! contended: the VM runs one thread at a time.
//!
//! A script that calls `item:subscribe` for a *new* item only after
//! `coolabah.run()` is already looping will not see it take effect — the item's
//! channel is opened once, from the snapshot [`Registry::run`] takes at the
//! start. Registering every subscription before calling `coolabah.run()` is the
//! supported shape, matching how such a config is written in practice.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use crate::protocol::{Event, ItemName, Kind};
use futures_util::stream::{SelectAll, StreamExt as _};
use mlua::{Function, Lua};

use crate::convert::event_to_table;
use crate::dispatch::Dispatcher;

#[derive(Default)]
struct ItemSubs {
    callbacks: HashMap<Kind, Function>,
}

/// One item's tagged event stream, boxed so every item's stream can share one
/// [`SelectAll`] despite each closure in [`Registry::run`] having its own type.
type TaggedStream = Pin<Box<dyn futures_lite::Stream<Item = (ItemName, Event)> + Send>>;

pub struct Registry {
    dispatcher: Arc<dyn Dispatcher>,
    items: Mutex<HashMap<ItemName, ItemSubs>>,
}

impl Registry {
    #[must_use]
    pub fn new(dispatcher: Arc<dyn Dispatcher>) -> Self {
        Self {
            dispatcher,
            items: Mutex::new(HashMap::new()),
        }
    }

    /// Records `callback` for `item`/`kind`, replacing any earlier callback
    /// registered for that same pair.
    pub fn subscribe(&self, item: &ItemName, kind: Kind, callback: Function) {
        self.items
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(item.clone())
            .or_default()
            .callbacks
            .insert(kind, callback);
    }

    /// Opens every registered item's channel, then dispatches events to
    /// callbacks until every channel has ended — in practice, until the
    /// daemon exits. Returns immediately, doing nothing, if nothing was ever
    /// subscribed.
    ///
    /// A callback that raises a Lua error is logged (`tracing::error!`) and
    /// the loop keeps going — the same policy the daemon already applies to
    /// a failing shell script, and for the same reason: one bad tick should
    /// not take the whole bar down.
    ///
    /// # Errors
    ///
    /// Returns a Lua error only if building an event's payload table fails,
    /// which would mean a bug in this crate rather than in a script.
    pub async fn run(&self, lua: &Lua) -> mlua::Result<()> {
        let snapshot: Vec<(ItemName, Vec<Kind>)> = self
            .items
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(item, subs)| (item.clone(), subs.callbacks.keys().cloned().collect()))
            .collect();

        let mut streams: SelectAll<TaggedStream> = SelectAll::new();
        for (item, kinds) in snapshot {
            match self.dispatcher.subscribe(item.clone(), kinds).await {
                Ok(stream) => {
                    let tagged = stream.map(move |event| (item.clone(), event));
                    streams.push(Box::pin(tagged));
                }
                Err(err) => tracing::error!(%item, %err, "could not subscribe"),
            }
        }

        while let Some((item, event)) = streams.next().await {
            let kind = event.kind();
            let callback = self
                .items
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&item)
                .and_then(|subs| subs.callbacks.get(&kind).cloned());
            let Some(callback) = callback else { continue };

            let table = event_to_table(lua, &item, &event)?;
            if let Err(err) = callback.call_async::<()>(table).await {
                tracing::error!(%item, event = %kind, %err, "a subscribed callback failed");
            }
        }
        Ok(())
    }
}

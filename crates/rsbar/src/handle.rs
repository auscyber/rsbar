//! Applying one request to the bar.
//!
//! Kept out of the schedule so it can be called directly by a test without
//! standing up an app.

use crate::bar::Bar;
use crate::script::Job;
use crate::sources::Registry;
use bevy_ecs::prelude::MessageWriter;
use rsbar_protocol::{ItemName, Query, Request, Response};

/// Applies `request`, returning the answer and any scripts it set off.
pub fn apply(
    bar: &mut Bar,
    sources: &mut Registry,
    request: Request,
    exit: &mut MessageWriter<bevy_app::AppExit>,
) -> (Response, Vec<Job>) {
    let found = |ok: bool, name: &ItemName| {
        if ok {
            Response::Ok
        } else {
            Response::Error(format!("no item named `{name}`"))
        }
    };

    match request {
        Request::SetBar(patch) => {
            tracing::debug!(?patch, "set bar");
            let response = match bar.apply(&patch) {
                Ok(()) => Response::Ok,
                Err(err) => Response::Error(err.to_string()),
            };
            (response, Vec::new())
        }
        Request::AddItem { name, position } => {
            tracing::debug!(%name, ?position, "add item");
            bar.add_item(name, position);
            (Response::Ok, Vec::new())
        }
        Request::SetItem { name, patch } => {
            tracing::trace!(%name, ?patch, "set item");
            (found(bar.set_item(&name, &patch), &name), Vec::new())
        }
        Request::RemoveItem(name) => {
            tracing::debug!(%name, "remove item");
            (found(bar.remove_item(&name), &name), Vec::new())
        }
        Request::Subscribe { name, events } => {
            tracing::debug!(%name, ?events, "subscribe");
            // Subscribing is the point at which a config decides what this
            // process actually observes, so it is what starts a source.
            sources.ensure_all(&events);
            (found(bar.subscribe(&name, events), &name), Vec::new())
        }
        Request::Trigger { event, info } => {
            tracing::debug!(%event, ?info, "trigger");
            (Response::Ok, bar.jobs_for(&event, info.as_deref()))
        }
        Request::UpdateAll => (Response::Ok, bar.all_jobs()),
        Request::Query(Query::Bar) => (Response::Bar(Box::new(bar.state())), Vec::new()),
        Request::Query(Query::Items) => (
            Response::Items(bar.items().iter().map(crate::item::Item::state).collect()),
            Vec::new(),
        ),
        Request::Query(Query::Item(name)) => {
            let response = match bar.item(&name) {
                Some(item) => Response::Item(Box::new(item.state())),
                None => Response::Error(format!("no item named `{name}`")),
            };
            (response, Vec::new())
        }
        Request::Shutdown => {
            tracing::info!("shutting down");
            exit.write(bevy_app::AppExit::Success);
            (Response::Ok, Vec::new())
        }
    }
}

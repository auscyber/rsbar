//! Builds the `rsbar` table. Host-agnostic and entirely `async`: every verb
//! goes through the injected [`Dispatcher`] and awaits it rather than
//! blocking, so this module is identical whether it ends up wired to
//! [`crate::ipc::IpcDispatcher`] or to an embedded implementation living in
//! the daemon.
//!
//! A plain Lua script calling `rsbar.bar({...})` does not need to know any of
//! this — mlua drives the whole chunk as a coroutine (see the bin runner's
//! `exec_async`), so an ordinary call syntax is enough for it to yield to the
//! executor while its request is in flight.

use std::rc::Rc;

use mlua::{Function, Lua, Table, UserData, UserDataMethods, Value};
use rsbar_protocol::{ItemName, Query, Request, Response};

use crate::convert::{
    bar_patch_from_table, bar_state_to_table, item_name_from_str, item_patch_from_table,
    item_state_to_table, kinds_from_value, position_from_str,
};
use crate::dispatch::Dispatcher;
use crate::events::Registry;

/// One item, as handed back by `rsbar.add`. Cheap to hold: cloning just
/// bumps the two `Rc`s.
#[derive(Clone)]
struct Item {
    name: ItemName,
    dispatcher: Rc<dyn Dispatcher>,
    registry: Rc<Registry>,
}

impl UserData for Item {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("name", |_, this, ()| Ok(this.name.as_str().to_string()));

        methods.add_async_method("set", |_, this, patch: Table| {
            let dispatcher = Rc::clone(&this.dispatcher);
            let name = this.name.clone();
            async move {
                let patch = item_patch_from_table(&patch)?;
                expect_ok(
                    dispatcher
                        .call(Request::SetItem {
                            name,
                            patch: Box::new(patch),
                        })
                        .await,
                )
            }
        });

        methods.add_async_method("remove", |_, this, ()| {
            let dispatcher = Rc::clone(&this.dispatcher);
            let name = this.name.clone();
            async move { expect_ok(dispatcher.call(Request::RemoveItem(name)).await) }
        });

        methods.add_async_method("query", |lua, this, ()| {
            let dispatcher = Rc::clone(&this.dispatcher);
            let name = this.name.clone();
            async move {
                match dispatcher.call(Request::Query(Query::Item(name))).await? {
                    Response::Item(state) => item_state_to_table(&lua, &state),
                    other => Err(unexpected(&other)),
                }
            }
        });

        // item:subscribe("volume_changed", function(event) ... end)
        // item:subscribe({"volume_changed", "brightness_changed"}, fn)
        // Purely local bookkeeping — see `crate::events` for why opening the
        // channel waits for `rsbar.run()`.
        methods.add_method(
            "subscribe",
            |_, this, (events, callback): (Value, Function)| {
                for kind in kinds_from_value(&events)? {
                    this.registry.subscribe(&this.name, kind, callback.clone());
                }
                Ok(())
            },
        );
    }
}

fn expect_ok(response: crate::error::Result<Response>) -> mlua::Result<()> {
    match response? {
        Response::Ok => Ok(()),
        other => Err(unexpected(&other)),
    }
}

fn unexpected(response: &Response) -> mlua::Error {
    mlua::Error::RuntimeError(format!("rsbar: unexpected reply {response:?}"))
}

/// Builds the `rsbar` table, wiring every verb to `dispatcher`.
///
/// # Errors
///
/// Returns an error if any Lua table/function creation or assignment fails.
// Taking ownership is the point: every host constructs its `Dispatcher` and
// hands it off once, here, rather than this crate borrowing one it does not
// control the lifetime of.
#[allow(clippy::needless_pass_by_value)]
pub fn install(lua: &Lua, dispatcher: Rc<dyn Dispatcher>) -> mlua::Result<Table> {
    let rsbar = lua.create_table()?;
    let registry = Rc::new(Registry::new(Rc::clone(&dispatcher)));

    rsbar.set("bar", bar_fn(lua, &dispatcher)?)?;
    rsbar.set("add", add_fn(lua, &dispatcher, &registry)?)?;
    rsbar.set("remove", remove_fn(lua, &dispatcher)?)?;
    rsbar.set("trigger", trigger_fn(lua, &dispatcher)?)?;
    rsbar.set(
        "update_all",
        verb_fn(lua, &dispatcher, || Request::UpdateAll)?,
    )?;
    rsbar.set("reload", verb_fn(lua, &dispatcher, || Request::Reload)?)?;
    rsbar.set("shutdown", verb_fn(lua, &dispatcher, || Request::Shutdown)?)?;
    // `sbar.begin_config()` / `sbar.end_config()` in the SketchyBar config
    // this mirrors: a config brackets its own declarations in these, so
    // whatever it stops mentioning between the two is swept on `end_config`
    // rather than the client having to compute a diff.
    rsbar.set(
        "begin_config",
        verb_fn(lua, &dispatcher, || Request::BeginConfig)?,
    )?;
    rsbar.set(
        "end_config",
        verb_fn(lua, &dispatcher, || Request::EndConfig)?,
    )?;
    rsbar.set("query", query_table(lua, &dispatcher)?)?;

    // `sbar.event_loop()` is the name the reference config actually calls;
    // `run` is kept as a shorter alias for the same function.
    let event_loop = run_fn(lua, &registry)?;
    rsbar.set("event_loop", event_loop.clone())?;
    rsbar.set("run", event_loop)?;

    rsbar.set("_VERSION", env!("CARGO_PKG_VERSION"))?;

    Ok(rsbar)
}

fn bar_fn(lua: &Lua, dispatcher: &Rc<dyn Dispatcher>) -> mlua::Result<Function> {
    let dispatcher = Rc::clone(dispatcher);
    lua.create_async_function(move |_, patch: Table| {
        let dispatcher = Rc::clone(&dispatcher);
        async move {
            let patch = bar_patch_from_table(&patch)?;
            expect_ok(dispatcher.call(Request::SetBar(patch)).await)
        }
    })
}

fn add_fn(
    lua: &Lua,
    dispatcher: &Rc<dyn Dispatcher>,
    registry: &Rc<Registry>,
) -> mlua::Result<Function> {
    let dispatcher = Rc::clone(dispatcher);
    let registry = Rc::clone(registry);
    lua.create_async_function(
        move |_, (name, position, patch): (String, Option<String>, Option<Table>)| {
            let dispatcher = Rc::clone(&dispatcher);
            let registry = Rc::clone(&registry);
            async move {
                let name = item_name_from_str(&name)?;
                let position = position
                    .map(|p| position_from_str(&p))
                    .transpose()?
                    .unwrap_or_default();

                expect_ok(
                    dispatcher
                        .call(Request::AddItem {
                            name: name.clone(),
                            position,
                        })
                        .await,
                )?;

                if let Some(patch) = patch {
                    let patch = item_patch_from_table(&patch)?;
                    expect_ok(
                        dispatcher
                            .call(Request::SetItem {
                                name: name.clone(),
                                patch: Box::new(patch),
                            })
                            .await,
                    )?;
                }

                Ok(Item {
                    name,
                    dispatcher,
                    registry,
                })
            }
        },
    )
}

fn remove_fn(lua: &Lua, dispatcher: &Rc<dyn Dispatcher>) -> mlua::Result<Function> {
    let dispatcher = Rc::clone(dispatcher);
    lua.create_async_function(move |_, name: String| {
        let dispatcher = Rc::clone(&dispatcher);
        async move {
            let name = item_name_from_str(&name)?;
            expect_ok(dispatcher.call(Request::RemoveItem(name)).await)
        }
    })
}

fn trigger_fn(lua: &Lua, dispatcher: &Rc<dyn Dispatcher>) -> mlua::Result<Function> {
    let dispatcher = Rc::clone(dispatcher);
    lua.create_async_function(move |_, (name, data): (String, Option<Value>)| {
        let dispatcher = Rc::clone(&dispatcher);
        async move {
            let kind = crate::convert::kind_from_str(&name)?;
            let mut event = kind.into_event();
            if let (rsbar_protocol::Event::Custom(custom), Some(data)) = (&mut event, data) {
                custom.data = lua_value_to_json(&data)?;
            }
            expect_ok(dispatcher.call(Request::Trigger(event)).await)
        }
    })
}

/// A zero-argument verb that always issues the same [`Request`] shape (built
/// fresh per call, since `Request` is not `Clone`-worth-storing here).
fn verb_fn(
    lua: &Lua,
    dispatcher: &Rc<dyn Dispatcher>,
    request: impl Fn() -> Request + 'static,
) -> mlua::Result<Function> {
    let dispatcher = Rc::clone(dispatcher);
    lua.create_async_function(move |_, ()| {
        let dispatcher = Rc::clone(&dispatcher);
        let request = request();
        async move { expect_ok(dispatcher.call(request).await) }
    })
}

fn query_table(lua: &Lua, dispatcher: &Rc<dyn Dispatcher>) -> mlua::Result<Table> {
    let query = lua.create_table()?;

    query.set("bar", {
        let dispatcher = Rc::clone(dispatcher);
        lua.create_async_function(move |lua, ()| {
            let dispatcher = Rc::clone(&dispatcher);
            async move {
                match dispatcher.call(Request::Query(Query::Bar)).await? {
                    Response::Bar(state) => bar_state_to_table(&lua, &state),
                    other => Err(unexpected(&other)),
                }
            }
        })?
    })?;

    query.set("items", {
        let dispatcher = Rc::clone(dispatcher);
        lua.create_async_function(move |lua, ()| {
            let dispatcher = Rc::clone(&dispatcher);
            async move {
                match dispatcher.call(Request::Query(Query::Items)).await? {
                    Response::Items(states) => {
                        let table = lua.create_table()?;
                        for (index, state) in states.iter().enumerate() {
                            table.set(index + 1, item_state_to_table(&lua, state)?)?;
                        }
                        Ok(table)
                    }
                    other => Err(unexpected(&other)),
                }
            }
        })?
    })?;

    query.set("item", {
        let dispatcher = Rc::clone(dispatcher);
        lua.create_async_function(move |lua, name: String| {
            let dispatcher = Rc::clone(&dispatcher);
            async move {
                let name = item_name_from_str(&name)?;
                match dispatcher.call(Request::Query(Query::Item(name))).await? {
                    Response::Item(state) => item_state_to_table(&lua, &state),
                    other => Err(unexpected(&other)),
                }
            }
        })?
    })?;

    Ok(query)
}

fn run_fn(lua: &Lua, registry: &Rc<Registry>) -> mlua::Result<Function> {
    let registry = Rc::clone(registry);
    lua.create_async_function(move |lua, ()| {
        let registry = Rc::clone(&registry);
        async move { registry.run(&lua).await }
    })
}

/// A small, deliberately incomplete Lua-value-to-`Json` for `rsbar.trigger`'s
/// free-form payload. Tables become objects; sequences are not distinguished
/// from them, since a triggered event's data is read by key, not iterated.
fn lua_value_to_json(value: &Value) -> mlua::Result<rsbar_protocol::Json> {
    use rsbar_protocol::Json;
    Ok(match value {
        Value::Nil => Json::Null,
        Value::Boolean(b) => Json::Bool(*b),
        Value::Integer(i) => Json::Int(*i),
        Value::Number(n) => Json::Float(*n),
        Value::String(s) => Json::String(s.to_str()?.to_string()),
        Value::Table(table) => {
            let mut fields = Vec::new();
            for pair in table.clone().pairs::<String, Value>() {
                let (key, value) = pair?;
                fields.push((key, lua_value_to_json(&value)?));
            }
            Json::Object(fields)
        }
        other => {
            return Err(mlua::Error::RuntimeError(format!(
                "cannot turn a {} into trigger data",
                other.type_name()
            )));
        }
    })
}

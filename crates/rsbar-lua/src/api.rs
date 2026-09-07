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

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use mlua::{Function, Lua, Table, UserData, UserDataMethods, Value};
use rsbar_protocol::{ItemName, ItemPatch, Query, Request, Response};

use crate::convert::{
    bar_patch_from_table, bar_state_to_table, deep_merge, item_name_from_str,
    item_patch_from_table, item_position_from_table, item_state_to_table, kinds_from_value,
    pattern_from_name,
};
use crate::dispatch::Dispatcher;
use crate::error::ApiError;
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
    // What `rsbar.default(...)` last stored — merged into every `add` from
    // then on. `Cell`/`RefCell`, not `tokio::sync`: this is plain single-
    // threaded bookkeeping local to the one thread hosting the `Lua` state,
    // never touched across an `.await`, so there is nothing here for an
    // async-aware lock to buy.
    let defaults: Rc<RefCell<Option<Table>>> = Rc::new(RefCell::new(None));
    // Names the next anonymous bracket (`rsbar.add("bracket", {members}, {})`,
    // with no name of its own).
    let bracket_counter = Rc::new(Cell::new(0_u64));

    rsbar.set("bar", bar_fn(lua, &dispatcher)?)?;
    rsbar.set(
        "add",
        add_fn(lua, &dispatcher, &registry, &defaults, &bracket_counter)?,
    )?;
    rsbar.set("set", set_fn(lua, &dispatcher)?)?;
    rsbar.set("default", default_fn(lua, &defaults)?)?;
    rsbar.set("exec", exec_fn(lua)?)?;
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

async fn add_item(
    dispatcher: &Rc<dyn Dispatcher>,
    name: ItemName,
    position: rsbar_protocol::Position,
) -> mlua::Result<()> {
    expect_ok(dispatcher.call(Request::AddItem { name, position }).await)
}

async fn set_item(
    dispatcher: &Rc<dyn Dispatcher>,
    name: ItemName,
    patch: ItemPatch,
) -> mlua::Result<()> {
    expect_ok(
        dispatcher
            .call(Request::SetItem {
                name,
                patch: Box::new(patch),
            })
            .await,
    )
}

/// Every item's name, right now — the snapshot `rsbar.set` and bracket
/// membership resolve a `/pattern/` against, since the protocol itself has no
/// concept of matching several items by one name.
async fn query_item_names(dispatcher: &Rc<dyn Dispatcher>) -> crate::error::Result<Vec<ItemName>> {
    match dispatcher.call(Request::Query(Query::Items)).await? {
        Response::Items(states) => Ok(states.into_iter().map(|state| state.name).collect()),
        other => Err(ApiError::UnexpectedResponse(other)),
    }
}

fn value_to_string(value: &Value, what: &str) -> mlua::Result<String> {
    match value {
        Value::String(s) => Ok(s.to_str()?.to_string()),
        other => Err(mlua::Error::RuntimeError(format!(
            "expected {what} to be a string, got {}",
            other.type_name()
        ))),
    }
}

fn value_to_opt_table(value: &Value, what: &str) -> mlua::Result<Option<Table>> {
    match value {
        Value::Nil => Ok(None),
        Value::Table(table) => Ok(Some(table.clone())),
        other => Err(mlua::Error::RuntimeError(format!(
            "expected {what} to be a table, got {}",
            other.type_name()
        ))),
    }
}

/// `opts` layered over whatever `rsbar.default` last stored, deep-merged so
/// only the keys either side actually named end up in the result — see
/// [`deep_merge`]. Neither `defaults` nor `opts` is consumed: both may be
/// reused by later `add` calls (`defaults`) or still be owned by the caller
/// (`opts`, borrowed).
fn merged_opts(
    lua: &Lua,
    defaults: &RefCell<Option<Table>>,
    opts: Option<&Table>,
) -> mlua::Result<Table> {
    match (defaults.borrow().as_ref(), opts) {
        (Some(base), Some(overlay)) => deep_merge(lua, base, overlay),
        (Some(base), None) => Ok(base.clone()),
        (None, Some(overlay)) => Ok(overlay.clone()),
        (None, None) => lua.create_table(),
    }
}

/// A bracket's members: literal item names, or `/pattern/`s expanded against
/// [`query_item_names`] — resolved once per `add("bracket", ...)` call, not
/// per member, so a bracket with several patterns costs one query rather than
/// one per pattern.
async fn resolve_members(
    dispatcher: &Rc<dyn Dispatcher>,
    members: &Value,
) -> mlua::Result<Vec<ItemName>> {
    let Value::Table(members) = members else {
        return Err(mlua::Error::RuntimeError(format!(
            "bracket members must be a table of item names, got {}",
            members.type_name()
        )));
    };

    let mut names = Vec::new();
    let mut every_name: Option<Vec<ItemName>> = None;
    for entry in members.clone().sequence_values::<mlua::LuaString>() {
        let raw = entry?.to_str()?.to_string();
        match pattern_from_name(&raw)? {
            Some(pattern) => {
                if every_name.is_none() {
                    every_name = Some(query_item_names(dispatcher).await?);
                }
                let matched = every_name
                    .as_ref()
                    .unwrap()
                    .iter()
                    .filter(|name| pattern.is_match(name.as_str()));
                let before = names.len();
                names.extend(matched.cloned());
                if names.len() == before {
                    tracing::warn!(pattern = %raw, "bracket member pattern matched no items");
                }
            }
            None => names.push(item_name_from_str(&raw)?),
        }
    }
    Ok(names)
}

/// `rsbar.add(kind, name, ...)` — `SketchyBar`'s own calling convention, kept
/// deliberately close to it rather than to a different shape rsbar might
/// otherwise have preferred, since the whole point is that a config only
/// changes its `require`. `kind` decides how the rest of the arguments are
/// read:
///
/// * `"item"`, `"alias"` — `add(kind, name, opts?)`. `opts.position` picks the
///   bucket (default `left`); an `"alias"` additionally mirrors `name` itself
///   as the menu-bar item to shadow, unless `opts.alias` already says so.
/// * `"bracket"` — `add("bracket", name, members, opts?)`, or, anonymously,
///   `add("bracket", members, opts?)` (`SketchyBar`'s own sugar for "I don't
///   need to name this bracket").
/// * `"event"` — `add("event", name)`. Declares nothing server-side (there is
///   no request that could); only checks `name` is a name `subscribe`/
///   `trigger` will accept later. Returns `nil`.
/// * anything else — logged by name and treated as a plain item, since a kind
///   rsbar does not model (`"slider"`, so far) still deserves *something* to
///   `:set`/`:subscribe` against rather than aborting the whole config.
fn add_fn(
    lua: &Lua,
    dispatcher: &Rc<dyn Dispatcher>,
    registry: &Rc<Registry>,
    defaults: &Rc<RefCell<Option<Table>>>,
    bracket_counter: &Rc<Cell<u64>>,
) -> mlua::Result<Function> {
    let dispatcher = Rc::clone(dispatcher);
    let registry = Rc::clone(registry);
    let defaults = Rc::clone(defaults);
    let bracket_counter = Rc::clone(bracket_counter);
    lua.create_async_function(
        move |lua, (kind, arg2, arg3, arg4): (String, Value, Value, Value)| {
            let dispatcher = Rc::clone(&dispatcher);
            let registry = Rc::clone(&registry);
            let defaults = Rc::clone(&defaults);
            let bracket_counter = Rc::clone(&bracket_counter);
            async move {
                if kind == "event" {
                    let name = value_to_string(&arg2, "event name")?;
                    crate::convert::kind_from_str(&name)?;
                    return Ok(None);
                }

                let (name_str, opts_value, members_value): (String, Value, Option<Value>) =
                    if kind == "bracket" {
                        match &arg2 {
                            Value::Table(_) => {
                                let n = bracket_counter.get() + 1;
                                bracket_counter.set(n);
                                (format!("bracket.{n}"), arg3.clone(), Some(arg2.clone()))
                            }
                            Value::String(s) => {
                                (s.to_str()?.to_string(), arg4.clone(), Some(arg3.clone()))
                            }
                            other => {
                                return Err(mlua::Error::RuntimeError(format!(
                                    "add(\"bracket\", ...)'s second argument must be a name or a table of members, got {}",
                                    other.type_name()
                                )));
                            }
                        }
                    } else {
                        if kind != "item" && kind != "alias" {
                            tracing::error!(
                                kind = %kind,
                                "unknown add kind; treating it as a plain item"
                            );
                        }
                        let name = value_to_string(&arg2, "item name")?;
                        // A kind rsbar does not model may carry its options
                        // somewhere other than the third argument (a
                        // slider's width sits there instead) — use whichever
                        // of the two trailing arguments is a table, and name
                        // the one that got skipped.
                        let opts = match (&arg3, &arg4) {
                            (Value::Table(_), _) => arg3.clone(),
                            (_, Value::Table(_)) => {
                                if !matches!(arg3, Value::Nil) {
                                    tracing::error!(
                                        kind = %kind,
                                        argument = ?arg3,
                                        "ignoring an argument rsbar has nowhere to put for this add kind"
                                    );
                                }
                                arg4.clone()
                            }
                            _ => Value::Nil,
                        };
                        (name, opts, None)
                    };

                let item_name = item_name_from_str(&name_str)?;
                let opts_table = value_to_opt_table(&opts_value, "add options")?;
                let merged = merged_opts(&lua, &defaults, opts_table.as_ref())?;
                let position = item_position_from_table(&merged)?;

                add_item(&dispatcher, item_name.clone(), position).await?;

                let mut patch = item_patch_from_table(&merged)?;
                if kind == "alias" && patch.alias.is_none() {
                    patch.alias = Some(name_str.clone());
                }
                if let Some(members_value) = members_value {
                    patch.members = Some(resolve_members(&dispatcher, &members_value).await?);
                }
                set_item(&dispatcher, item_name.clone(), patch).await?;

                Ok(Some(Item {
                    name: item_name,
                    dispatcher,
                    registry,
                }))
            }
        },
    )
}

/// `rsbar.set(name, opts)` — sets an item by name rather than through the
/// handle `rsbar.add` returned, which is what lets one module (`paneru_bar`)
/// reach into items another module (`items/left.lua`, `wm.lua`) created. Also
/// accepts a `/pattern/` name, resolved the same way a bracket's members are.
fn set_fn(lua: &Lua, dispatcher: &Rc<dyn Dispatcher>) -> mlua::Result<Function> {
    let dispatcher = Rc::clone(dispatcher);
    lua.create_async_function(move |_, (name, patch): (String, Table)| {
        let dispatcher = Rc::clone(&dispatcher);
        async move {
            let patch = item_patch_from_table(&patch)?;
            match pattern_from_name(&name)? {
                Some(pattern) => {
                    let targets = query_item_names(&dispatcher).await?;
                    let matched: Vec<_> = targets
                        .into_iter()
                        .filter(|target| pattern.is_match(target.as_str()))
                        .collect();
                    if matched.is_empty() {
                        tracing::warn!(pattern = %name, "rsbar.set matched no items");
                    }
                    for target in matched {
                        set_item(&dispatcher, target, patch.clone()).await?;
                    }
                    Ok(())
                }
                None => set_item(&dispatcher, item_name_from_str(&name)?, patch).await,
            }
        }
    })
}

/// `rsbar.default(opts)` — properties merged into every `add` from this point
/// on (see [`merged_opts`]), replacing whatever an earlier call stored.
/// Implemented entirely client-side: there is no request that could apply
/// this on the daemon's behalf, and there does not need to be one, since it
/// only ever affects what `add` sends at the moment an item is created.
fn default_fn(lua: &Lua, defaults: &Rc<RefCell<Option<Table>>>) -> mlua::Result<Function> {
    let defaults = Rc::clone(defaults);
    lua.create_function(move |_, opts: Table| {
        *defaults.borrow_mut() = Some(opts);
        Ok(())
    })
}

/// `rsbar.exec(command, callback?)` — runs `command` through `sh -c` and, if
/// given, calls `callback` with its stdout once it exits. Purely local: no
/// `Dispatcher` involved, since this never talks to the daemon at all.
///
/// Real `SketchyBar`'s `exec` is fire-and-forget — the calling script keeps
/// running immediately while the command executes in the background, and the
/// callback lands later. This awaits the whole thing (spawn, run, callback)
/// before returning instead, since the whole config already runs as one Lua
/// coroutine with no scheduler of its own to hand a detached task to. That
/// keeps every observable ordering the same as a config that never notices
/// the difference (a lookup before continuing), at the cost of a slower
/// config load or event callback when the command is itself slow. Making
/// this truly concurrent would mean a `tokio::task::LocalSet` in the host
/// binary (`bin/rsbar_lua.rs`) `spawn_local`-ing it instead — worth doing if
/// a config turns out to depend on `exec` actually running in the background.
fn exec_fn(lua: &Lua) -> mlua::Result<Function> {
    lua.create_async_function(
        move |_, (command, callback): (String, Option<Function>)| async move {
            match tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&command)
                .output()
                .await
            {
                Ok(output) => {
                    if !output.status.success() {
                        tracing::warn!(
                            %command,
                            status = %output.status,
                            stderr = %String::from_utf8_lossy(&output.stderr),
                            "rsbar.exec exited non-zero"
                        );
                    }
                    if let Some(callback) = callback {
                        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                        if let Err(err) = callback.call_async::<()>(stdout).await {
                            tracing::error!(%command, %err, "an rsbar.exec callback failed");
                        }
                    }
                }
                Err(err) => tracing::error!(%command, %err, "rsbar.exec could not run the command"),
            }
            Ok(())
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

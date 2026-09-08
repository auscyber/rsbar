//! Builds the `rsbar` table. Host-agnostic and entirely `async`: every verb
//! goes through the injected [`Dispatcher`] and awaits it rather than
//! blocking, so this module is identical whether it ends up wired to
//! [`crate::ipc::IpcDispatcher`] or to an embedded implementation living in
//! the daemon.
//!
//! A plain Lua script calling `rsbar.bar({...})` does not need to know any of
//! this — mlua drives the whole chunk as a coroutine (see the bin runner's
//! `exec_async`), so an ordinary call syntax is enough for it to yield to the
//! executor while its request is in flight. That includes the metamethods:
//! `item.popup.drawing = true` awaits its `Request::Set` from inside
//! `__newindex`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use mlua::{FromLua, Function, Lua, MetaMethod, Table, UserData, UserDataMethods, Value};
use rsbar_protocol::{
    ComponentKind, ItemName, ItemPatch, Position, Query, Relative, Request, Response, Selector,
};

use crate::convert::{
    bar_patch_from_table, bar_state_to_table, deep_merge, item_name_from_str, item_patch_at,
    item_patch_from_table, item_position_from_table, item_state_to_table, kinds_from_value,
    selector_from_name,
};
use crate::dispatch::Dispatcher;
use crate::events::Registry;

/// The groups indexed on the way to a leaf: `item.popup.background` carries
/// `["popup", "background"]`.
///
/// Nothing here checks a segment against a list of legal keys, and that is
/// the point — the very same `item_patch_from_table` that reads a `:set{...}`
/// table decides what a path means, so there is no second spelling of
/// [`ItemPatch`]'s shape to keep in step with it.
#[derive(Clone, Default)]
struct PropertyPath(Vec<Box<str>>);

impl PropertyPath {
    fn child(&self, key: &str) -> Self {
        let mut segments = self.0.clone();
        segments.push(key.into());
        Self(segments)
    }
}

/// What indexing an item hands back: the item, plus the path so far.
///
/// `item.popup.drawing = true` and `item:set{ popup = { drawing = true } }`
/// put the identical [`Request::Set`] on the wire — see [`item_patch_at`].
#[derive(Clone)]
struct Property {
    name: ItemName,
    dispatcher: Arc<dyn Dispatcher>,
    path: PropertyPath,
}

impl Property {
    /// The one thing both this and [`Item`]'s own `__newindex` do.
    async fn assign(
        lua: Lua,
        dispatcher: Arc<dyn Dispatcher>,
        name: ItemName,
        groups: PropertyPath,
        key: &str,
        value: Value,
    ) -> mlua::Result<()> {
        let patch = item_patch_at(&lua, &groups.0, key, value)?;
        set_item(&dispatcher, name, patch).await
    }
}

impl UserData for Property {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Index, |_, this, key: String| {
            Ok(Self {
                name: this.name.clone(),
                dispatcher: Arc::clone(&this.dispatcher),
                path: this.path.child(&key),
            })
        });

        methods.add_async_meta_method(
            MetaMethod::NewIndex,
            |lua, this, (key, value): (String, Value)| {
                let (dispatcher, name, path) = (
                    Arc::clone(&this.dispatcher),
                    this.name.clone(),
                    this.path.clone(),
                );
                async move { Self::assign(lua, dispatcher, name, path, &key, value).await }
            },
        );
    }
}

/// An item method that sends one [`Request`] naming the item and returns
/// nothing — the [`request_fn`] of the handle side.
macro_rules! item_verbs {
    ($methods:ident, $($verb:literal($arg:ident : $ty:ty) => |$name:ident| $request:expr),* $(,)?) => {
        $($methods.add_async_method($verb, |_, this, $arg: $ty| {
            let dispatcher = Arc::clone(&this.dispatcher);
            let $name = this.name.clone();
            let request: mlua::Result<Request> = $request;
            async move { expect_ok(dispatcher.call(request?).await) }
        });)*
    };
}

/// One item, as handed back by `rsbar.add`. Cheap to hold: cloning just
/// bumps the two `Arc`s.
#[derive(Clone)]
struct Item {
    name: ItemName,
    dispatcher: Arc<dyn Dispatcher>,
    registry: Arc<Registry>,
}

impl UserData for Item {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("name", |_, this, ()| Ok(this.name.as_str().to_string()));

        // Each of these is one request built from the item's own name and the
        // method's argument, so that is all the macro asks for.
        //
        // `push` is one more sample for a graph — `--push` in `SketchyBar`'s
        // own vocabulary, and `sbar.push` in `SbarLua`'s.
        item_verbs! { methods,
            "set"(patch: Table) => |name| item_patch_from_table(&patch)
                .map(|patch| Request::Set(Selector::Name(name), Box::new(patch))),
            "remove"(_unused: ()) => |name| Ok(Request::Remove(Selector::Name(name))),
            "push"(value: f32) => |name| Ok(Request::Push { name, value }),
        }

        methods.add_async_method("query", |lua, this, ()| {
            let dispatcher = Arc::clone(&this.dispatcher);
            let name = this.name.clone();
            async move {
                let response = dispatcher.call(Request::Query(Query::Item(name))).await?;
                response_to_table(&lua, &response)
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

        // Property access. mlua checks methods before either of these, so
        // `item:set{...}` and `item.label = "x"` live on the same handle.
        //
        // Indexing hands back a [`Property`] whatever the key, rather than a
        // known-namespaces list: reading a *leaf* would mean a query round
        // trip per access (`:query()` is how a config reads a live value), so
        // the only thing a read can usefully be is a longer path.
        methods.add_meta_method(MetaMethod::Index, |_, this, key: String| {
            Ok(Property {
                name: this.name.clone(),
                dispatcher: Arc::clone(&this.dispatcher),
                path: PropertyPath::default().child(&key),
            })
        });

        methods.add_async_meta_method(
            MetaMethod::NewIndex,
            |lua, this, (key, value): (String, Value)| {
                let (dispatcher, name) = (Arc::clone(&this.dispatcher), this.name.clone());
                async move {
                    Property::assign(lua, dispatcher, name, PropertyPath::default(), &key, value)
                        .await
                }
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

/// Everything `rsbar.add` needs to keep between calls. One value rather than
/// four parameters, since every one of them is an `Arc` the closure clones
/// anyway.
#[derive(Clone)]
struct AddContext {
    dispatcher: Arc<dyn Dispatcher>,
    registry: Arc<Registry>,
    /// What `rsbar.default(...)` last stored — merged into every `add` from
    /// then on. A plain [`Mutex`], not `tokio::sync`: this is bookkeeping
    /// bookkeeping local to the one thread hosting the `Lua` state, never
    /// touched across an `.await`, so there is nothing here for an
    /// async-aware lock to buy.
    defaults: Arc<Mutex<Option<Table>>>,
    /// Names the next unnamed add of each kind — `bracket.1`, `item.2`.
    anonymous: Arc<Mutex<HashMap<String, u64>>>,
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
pub fn install(lua: &Lua, dispatcher: Arc<dyn Dispatcher>) -> mlua::Result<Table> {
    let rsbar = lua.create_table()?;
    let registry = Arc::new(Registry::new(Arc::clone(&dispatcher)));
    let context = AddContext {
        dispatcher: Arc::clone(&dispatcher),
        registry: Arc::clone(&registry),
        defaults: Arc::new(Mutex::new(None)),
        anonymous: Arc::new(Mutex::new(HashMap::new())),
    };

    install_requests(lua, &rsbar, &dispatcher)?;

    rsbar.set("add", add_fn(lua, &context)?)?;
    rsbar.set("default", default_fn(lua, &context.defaults)?)?;
    rsbar.set("exec", exec_fn(lua)?)?;
    rsbar.set("subscribe", subscribe_fn(lua, &registry)?)?;
    rsbar.set("animate", animate_fn(lua)?)?;
    rsbar.set("delay", delay_fn(lua)?)?;
    rsbar.set("hotload", hotload_fn(lua)?)?;
    rsbar.set("set_bar_name", set_bar_name_fn(lua)?)?;

    // `sbar.event_loop()` is the name the reference config actually calls;
    // `run` is kept as a shorter alias for the same function.
    let event_loop = run_fn(lua, &registry)?;
    rsbar.set("event_loop", event_loop.clone())?;
    rsbar.set("run", event_loop)?;

    rsbar.set("_VERSION", env!("CARGO_PKG_VERSION"))?;

    Ok(rsbar)
}

/// Every verb that ends in one or more requests, which is most of them: what
/// differs between `rsbar.remove` and `rsbar.reorder` is only which
/// [`Request`]s their arguments name, so each is a line rather than a builder.
fn install_requests(
    lua: &Lua,
    rsbar: &Table,
    dispatcher: &Arc<dyn Dispatcher>,
) -> mlua::Result<()> {
    macro_rules! verb {
        ($build:expr) => {
            request_fn(lua, dispatcher, $build)
        };
    }
    rsbar.set(
        "bar",
        verb!(|patch: Table| Ok(vec![Request::SetBar(bar_patch_from_table(&patch)?)]))?,
    )?;
    rsbar.set(
        "set",
        verb!(|(name, patch): (String, Table)| {
            Ok(vec![Request::Set(
                selector_from_name(&name)?,
                Box::new(item_patch_from_table(&patch)?),
            )])
        })?,
    )?;
    rsbar.set(
        "remove",
        verb!(|names: Value| {
            names_from_value(&names, "rsbar.remove")?
                .iter()
                .map(|name| Ok(Request::Remove(selector_from_name(name)?)))
                .collect()
        })?,
    )?;
    rsbar.set(
        "trigger",
        verb!(|(name, data): (String, Option<Value>)| {
            Ok(vec![Request::Trigger(event_from_lua(
                &name,
                data.as_ref(),
            )?)])
        })?,
    )?;
    rsbar.set(
        "push",
        verb!(|(name, value): (String, f32)| {
            Ok(vec![Request::Push {
                name: item_name_from_str(&name)?,
                value,
            }])
        })?,
    )?;
    // `rsbar.move("chevron", "after", "front_app")` — what the reference
    // config otherwise shells out to `sketchybar --move` for.
    rsbar.set(
        "move",
        verb!(|(name, relative, reference): (String, String, String)| {
            Ok(vec![Request::Move {
                name: item_name_from_str(&name)?,
                relative: relative_from_str(&relative)?,
                reference: item_name_from_str(&reference)?,
            }])
        })?,
    )?;
    rsbar.set(
        "reorder",
        verb!(|names: Value| {
            let names = names_from_value(&names, "rsbar.reorder")?
                .iter()
                .map(|name| Ok(item_name_from_str(name)?))
                .collect::<mlua::Result<Vec<_>>>()?;
            Ok(vec![Request::Reorder(names)])
        })?,
    )?;
    rsbar.set("update_all", verb!(|()| Ok(vec![Request::UpdateAll]))?)?;
    // `SbarLua` spells the same thing `update`; both are kept so neither a
    // config nor rsbar's own docs have to change.
    rsbar.set("update", verb!(|()| Ok(vec![Request::UpdateAll]))?)?;
    rsbar.set("reload", verb!(|()| Ok(vec![Request::Reload]))?)?;
    rsbar.set("shutdown", verb!(|()| Ok(vec![Request::Shutdown]))?)?;
    // `sbar.begin_config()` / `sbar.end_config()` in the SketchyBar config
    // this mirrors: a config brackets its own declarations in these, so
    // whatever it stops mentioning between the two is swept on `end_config`
    // rather than the client having to compute a diff.
    rsbar.set("begin_config", verb!(|()| Ok(vec![Request::BeginConfig]))?)?;
    rsbar.set("end_config", verb!(|()| Ok(vec![Request::EndConfig]))?)?;

    let query = lua.create_table()?;
    query.set("bar", query_fn(lua, dispatcher, |()| Ok(Query::Bar))?)?;
    query.set("items", query_fn(lua, dispatcher, |()| Ok(Query::Items))?)?;
    query.set(
        "item",
        query_fn(lua, dispatcher, |name: String| {
            Ok(Query::Item(item_name_from_str(&name)?))
        })?,
    )?;
    rsbar.set("query", query)?;

    Ok(())
}

/// A verb that turns its Lua arguments into requests and expects `Ok` back.
fn request_fn<A, B>(lua: &Lua, dispatcher: &Arc<dyn Dispatcher>, build: B) -> mlua::Result<Function>
where
    A: mlua::FromLuaMulti + 'static,
    B: Fn(A) -> mlua::Result<Vec<Request>> + Send + 'static,
{
    let dispatcher = Arc::clone(dispatcher);
    lua.create_async_function(move |_, args: A| {
        let dispatcher = Arc::clone(&dispatcher);
        let requests = build(args);
        async move {
            for request in requests? {
                expect_ok(dispatcher.call(request).await)?;
            }
            Ok(())
        }
    })
}

/// As [`request_fn`], for the verbs that read something back rather than
/// expecting `Ok`.
fn query_fn<A, B>(lua: &Lua, dispatcher: &Arc<dyn Dispatcher>, build: B) -> mlua::Result<Function>
where
    A: mlua::FromLuaMulti + 'static,
    B: Fn(A) -> mlua::Result<Query> + Send + 'static,
{
    let dispatcher = Arc::clone(dispatcher);
    lua.create_async_function(move |lua, args: A| {
        let dispatcher = Arc::clone(&dispatcher);
        let query = build(args);
        async move {
            let response = dispatcher.call(Request::Query(query?)).await?;
            response_to_table(&lua, &response)
        }
    })
}

/// Whatever a query answered, as the table a config reads.
fn response_to_table(lua: &Lua, response: &Response) -> mlua::Result<Table> {
    match response {
        Response::Bar(state) => bar_state_to_table(lua, state),
        Response::Item(state) => item_state_to_table(lua, state),
        Response::Items(states) => {
            let table = lua.create_table()?;
            for (index, state) in states.iter().enumerate() {
                table.set(index + 1, item_state_to_table(lua, state)?)?;
            }
            Ok(table)
        }
        other => Err(unexpected(other)),
    }
}

/// `rsbar.trigger("demo", { VAR = "Test" })` — a built-in carries its own
/// payload; only a custom event takes variables.
fn event_from_lua(name: &str, data: Option<&Value>) -> mlua::Result<rsbar_protocol::Event> {
    let mut event = crate::convert::kind_from_str(name)?.into_event();
    if let (rsbar_protocol::Event::Custom(custom), Some(data)) = (&mut event, data) {
        custom.vars = crate::convert::vars_from_value(data)?;
    }
    Ok(event)
}

fn relative_from_str(text: &str) -> mlua::Result<Relative> {
    match text {
        "before" => Ok(Relative::Before),
        "after" => Ok(Relative::After),
        other => Err(mlua::Error::RuntimeError(format!(
            "rsbar.move takes \"before\" or \"after\", got \"{other}\""
        ))),
    }
}

async fn add_item(
    dispatcher: &Arc<dyn Dispatcher>,
    name: ItemName,
    position: Position,
) -> mlua::Result<()> {
    expect_ok(
        dispatcher
            .call(Request::Add(ComponentKind::Item { name, position }))
            .await,
    )
}

/// `Request::Add`, degrading to a plain item if the daemon rejects the kind
/// outright — a `Response::Error` reaches this as
/// [`crate::error::ApiError::Rejected`], the [`Dispatcher`]'s own way of
/// turning "recognised, but I cannot draw one yet" into an error rather than
/// letting that abort the whole config over one component it cannot draw.
async fn add_component(dispatcher: &Arc<dyn Dispatcher>, kind: ComponentKind) -> mlua::Result<()> {
    let (name, position) = kind.placement();
    let (name, position) = (name.clone(), position);
    match dispatcher.call(Request::Add(kind.clone())).await {
        Ok(Response::Ok) => Ok(()),
        Ok(other) => Err(unexpected(&other)),
        Err(crate::error::ApiError::Rejected(message)) => {
            tracing::error!(
                ?kind,
                %message,
                "the daemon cannot draw this component yet; adding a plain item instead"
            );
            add_item(dispatcher, name, position).await
        }
        Err(err) => Err(err.into()),
    }
}

async fn set_item(
    dispatcher: &Arc<dyn Dispatcher>,
    name: ItemName,
    patch: ItemPatch,
) -> mlua::Result<()> {
    expect_ok(
        dispatcher
            .call(Request::Set(Selector::Name(name), Box::new(patch)))
            .await,
    )
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
    defaults: &Mutex<Option<Table>>,
    opts: Option<&Table>,
) -> mlua::Result<Table> {
    let stored = defaults.lock().unwrap_or_else(PoisonError::into_inner);
    match (stored.as_ref(), opts) {
        (Some(base), Some(overlay)) => deep_merge(lua, base, overlay),
        (Some(base), None) => Ok(base.clone()),
        (None, Some(overlay)) => Ok(overlay.clone()),
        (None, None) => lua.create_table(),
    }
}

/// A bracket's members: literal item names, or `/pattern/`s — turned into
/// [`Selector`]s and sent as-is, since only the daemon has a live item list
/// to resolve a pattern against without racing a config that is still adding
/// items (see [`Selector`]'s own doc comment).
fn members_from_value(members: &Value) -> mlua::Result<Vec<Selector>> {
    let Value::Table(members) = members else {
        return Err(mlua::Error::RuntimeError(format!(
            "bracket members must be a table of item names, got {}",
            members.type_name()
        )));
    };

    members
        .clone()
        .sequence_values::<mlua::LuaString>()
        .map(|entry| {
            let raw = entry?.to_str()?.to_string();
            raw.parse::<Selector>()
                .map_err(|err| mlua::Error::RuntimeError(err.to_string()))
        })
        .collect()
}

/// The one argument that genuinely differs between `add` kinds: what sits
/// between the (optional) name and the options table.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Extra {
    /// `add(kind, name?, opts?)`.
    None,
    /// `add("bracket", name?, {members}, opts?)`.
    Members,
    /// `add("slider"|"graph", name?, width, opts?)`.
    Width,
}

impl Extra {
    /// Whether `value` could be this kind's own extra argument — which is how
    /// the named form is told from `SketchyBar`'s anonymous sugar without
    /// asking each kind separately.
    fn claims(self, value: &Value) -> bool {
        match self {
            Self::None => matches!(value, Value::Nil | Value::Table(_)),
            Self::Members => matches!(value, Value::Table(_)),
            Self::Width => matches!(value, Value::Integer(_) | Value::Number(_)),
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Self::None => "an options table",
            Self::Members => "a table of members",
            Self::Width => "a width",
        }
    }
}

/// What a kind builds, and how it reads its arguments. `event` is here rather
/// than beside the table because it is the one kind that draws nothing and
/// returns nothing — a difference in *result*, not just in arguments.
enum Build {
    Component {
        extra: Extra,
        /// Position and members are both passed because a kind takes one or
        /// the other; the constructor drops whichever it does not use.
        component: fn(ItemName, Position, Vec<Selector>) -> ComponentKind,
        /// `None` once the daemon can draw one. Until then, what it cannot
        /// do — logged as recognised rather than as a config's typo.
        gap: Option<&'static str>,
        /// `add("alias", "Owner,Name")` names the menu bar item it mirrors:
        /// the name *is* the target, so it fills in `alias` itself.
        mirrors_its_own_name: bool,
    },
    Event,
}

/// One row per kind `--add` accepts, so adding a kind is adding a row rather
/// than another branch through [`AddContext::add`].
struct KindSpec {
    name: &'static str,
    build: Build,
}

const ITEM: KindSpec = KindSpec {
    name: "item",
    build: Build::Component {
        extra: Extra::None,
        component: |name, position, _| ComponentKind::Item { name, position },
        gap: None,
        mirrors_its_own_name: false,
    },
};

const KINDS: &[KindSpec] = &[
    ITEM,
    KindSpec {
        name: "alias",
        build: Build::Component {
            extra: Extra::None,
            component: |name, position, _| ComponentKind::Alias { name, position },
            gap: None,
            mirrors_its_own_name: true,
        },
    },
    KindSpec {
        name: "bracket",
        build: Build::Component {
            extra: Extra::Members,
            // A bracket takes no position: its frame comes from its members.
            component: |name, _, members| ComponentKind::Bracket { name, members },
            gap: None,
            mirrors_its_own_name: false,
        },
    },
    KindSpec {
        name: "space",
        build: Build::Component {
            extra: Extra::None,
            component: |name, position, _| ComponentKind::Space { name, position },
            gap: Some("nothing draws a space's own selected highlight yet"),
            mirrors_its_own_name: false,
        },
    },
    KindSpec {
        name: "graph",
        build: Build::Component {
            extra: Extra::Width,
            component: |name, position, _| ComponentKind::Graph { name, position },
            gap: Some("nothing plots a graph yet"),
            mirrors_its_own_name: false,
        },
    },
    KindSpec {
        name: "slider",
        build: Build::Component {
            extra: Extra::Width,
            component: |name, position, _| ComponentKind::Slider { name, position },
            gap: Some("nothing draws a slider's track or knob yet"),
            mirrors_its_own_name: false,
        },
    },
    KindSpec {
        name: "event",
        build: Build::Event,
    },
];

/// Six rows, walked once per `add` at config load — a lookup table keyed by
/// the kind would only be a second copy of the names already in [`KINDS`].
fn spec_for(kind: &str) -> Option<&'static KindSpec> {
    KINDS.iter().find(|spec| spec.name == kind)
}

/// `add("event", name)` declares a custom event; a third argument bridges it
/// from that `NSDistributedNotificationCenter` name, so any process on the
/// machine posting it fires the event here.
async fn add_event(
    dispatcher: &Arc<dyn Dispatcher>,
    name: &Value,
    notification: &Value,
) -> mlua::Result<()> {
    let name = crate::convert::event_name_from_str(&value_to_string(name, "event name")?)?;
    let notification = match notification {
        Value::Nil => None,
        other => Some(crate::convert::notification_name_from_str(
            &value_to_string(other, "notification name")?,
        )?),
    };
    expect_ok(
        dispatcher
            .call(Request::AddEvent { name, notification })
            .await,
    )
}

impl AddContext {
    /// `<kind>.1`, `<kind>.2`, ... — `SketchyBar`'s own sugar for "I don't
    /// need to name this one", counted per kind so a config's brackets stay
    /// `bracket.1`, `bracket.2` however many items were added between them.
    fn anonymous_name(&self, kind: &str) -> String {
        let mut counters = self
            .anonymous
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let counter = counters.entry(kind.to_owned()).or_default();
        *counter += 1;
        format!("{kind}.{counter}")
    }

    /// `rsbar.add(kind, name?, ...)` — see [`add_fn`] for the vocabulary.
    async fn add(
        &self,
        lua: &Lua,
        kind: &str,
        (arg2, arg3, arg4): (Value, Value, Value),
    ) -> mlua::Result<Option<Item>> {
        let spec = spec_for(kind).unwrap_or_else(|| {
            tracing::error!(%kind, "unknown add kind; treating it as a plain item");
            &ITEM
        });
        let Build::Component {
            extra,
            component,
            gap,
            mirrors_its_own_name,
        } = spec.build
        else {
            add_event(&self.dispatcher, &arg2, &arg3).await?;
            return Ok(None);
        };
        if let Some(gap) = gap {
            tracing::error!(
                %kind,
                "recognised, but {gap}; adding it as a `ComponentKind` the daemon can at least track"
            );
        }

        // The name is optional for every kind: whether the second argument is
        // one is decided by whether the kind's own extra argument claims it.
        let (name, extra_value, opts_value) = match arg2 {
            Value::String(ref s) => (s.to_str()?.to_string(), arg3, arg4),
            ref other if extra.claims(other) => (self.anonymous_name(kind), arg2, arg3),
            other => {
                return Err(mlua::Error::RuntimeError(format!(
                    "add(\"{kind}\", ...)'s second argument must be a name or {}, got {}",
                    extra.describe(),
                    other.type_name()
                )));
            }
        };
        // With nothing between the name and the options, the extra slot *is*
        // the options table — except for an unknown kind, which may carry a
        // positional argument rsbar has nowhere to put.
        let opts_value = match extra {
            Extra::None => trailing_opts(kind, extra_value.clone(), opts_value),
            _ => opts_value,
        };

        let item_name = item_name_from_str(&name)?;
        let merged = merged_opts(
            lua,
            &self.defaults,
            value_to_opt_table(&opts_value, "add options")?.as_ref(),
        )?;
        let position = item_position_from_table(&merged)?;
        let members = match extra {
            Extra::Members => members_from_value(&extra_value)?,
            _ => Vec::new(),
        };

        add_component(
            &self.dispatcher,
            component(item_name.clone(), position, members.clone()),
        )
        .await?;

        let mut patch = item_patch_from_table(&merged)?;
        if mirrors_its_own_name && patch.alias.is_none() {
            patch.alias = Some(name);
        }
        if extra == Extra::Members {
            // Sent again on the patch rather than left on the kind alone: the
            // daemon keeps only the literal names off a `ComponentKind`, and a
            // config's brackets are written as `/pattern/`s.
            patch.members = Some(members);
        }
        if extra == Extra::Width {
            // `add("slider", name, 100, ...)`'s positional width. Nothing
            // draws a slider yet, so this lands on the item's own width —
            // still the space the config asked for, rather than dropped.
            // Width lives under `geometry` on the patch even though a config
            // writes it flat, so the sub-patch is created if the options
            // table said nothing geometric at all.
            let geometry = patch.geometry.get_or_insert_default();
            if geometry.width.is_none() {
                geometry.width = Some(f64::from_lua(extra_value, lua)?);
            }
        }
        set_item(&self.dispatcher, item_name.clone(), patch).await?;

        Ok(Some(Item {
            name: item_name,
            dispatcher: Arc::clone(&self.dispatcher),
            registry: Arc::clone(&self.registry),
        }))
    }
}

/// Whichever of the two trailing arguments is a table, naming the one that
/// got skipped — for a kind rsbar does not model, whose options may not be
/// the third argument.
fn trailing_opts(kind: &str, first: Value, second: Value) -> Value {
    match (&first, &second) {
        (Value::Table(_), _) => first,
        (_, Value::Table(_)) => {
            if !matches!(first, Value::Nil) {
                tracing::error!(
                    %kind,
                    argument = ?first,
                    "ignoring an argument rsbar has nowhere to put for this add kind"
                );
            }
            second
        }
        _ => Value::Nil,
    }
}

/// `rsbar.add(kind, name?, ...)` — `SketchyBar`'s own calling convention,
/// kept deliberately close to it rather than to a different shape rsbar might
/// otherwise have preferred, since the whole point is that a config only
/// changes its `require`. Every kind is a row in [`KINDS`]; what differs
/// between them is which arguments they take ([`Extra`]) and which
/// [`ComponentKind`] they build, both of which are data on that row rather
/// than a branch here.
///
/// * `"item"`, `"alias"`, `"space"` — `add(kind, name?, opts?)`.
///   `opts.position` picks the bucket (default `left`), or hangs the item off
///   another item's popup with `position = "popup.<owner>"`. An `"alias"`
///   additionally mirrors `name` itself as the menu-bar item to shadow,
///   unless `opts.alias` already says so.
/// * `"bracket"` — `add("bracket", name?, members, opts?)`.
/// * `"graph"`, `"slider"` — `add(kind, name?, width, opts?)`.
/// * `"event"` — `add("event", name)` declares a custom event this config
///   fires itself; `add("event", name, notification)` also bridges it from an
///   `NSDistributedNotificationCenter` name, so any process on the machine
///   posting that notification fires the event here. A built-in's name is
///   refused: it is already defined. Returns `nil`.
/// * anything else — a config's typo, most likely; logged by name and treated
///   as a plain item all the same, rather than aborting the config over one
///   bad `add`.
///
/// A kind whose name is left out is named `<kind>.<n>` — `SketchyBar`'s own
/// sugar, most often written for a bracket nothing else refers to.
fn add_fn(lua: &Lua, context: &AddContext) -> mlua::Result<Function> {
    let context = context.clone();
    lua.create_async_function(
        move |lua, (kind, arg2, arg3, arg4): (String, Value, Value, Value)| {
            let context = context.clone();
            async move { context.add(&lua, &kind, (arg2, arg3, arg4)).await }
        },
    )
}

/// `rsbar.default(opts)` — properties merged into every `add` from this point
/// on (see [`merged_opts`]), replacing whatever an earlier call stored.
/// Implemented entirely client-side: there is no request that could apply
/// this on the daemon's behalf, and there does not need to be one, since it
/// only ever affects what `add` sends at the moment an item is created.
fn default_fn(lua: &Lua, defaults: &Arc<Mutex<Option<Table>>>) -> mlua::Result<Function> {
    let defaults = Arc::clone(defaults);
    lua.create_function(move |_, opts: Table| {
        *defaults.lock().unwrap_or_else(PoisonError::into_inner) = Some(opts);
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
                // A config shells out to `rsbard` — `--subscribe`, `--move`,
                // `--query` — so that name has to resolve. This process ships
                // beside the daemon, so its own directory is where to find
                // it; front, so a stale copy elsewhere cannot shadow it.
                .env("PATH", child_path())
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

/// `PATH` with this binary's own directory on the front, for anything a
/// config's `exec` runs.
fn child_path() -> std::ffi::OsString {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let Some(directory) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
    else {
        return inherited;
    };

    let mut path = std::ffi::OsString::from(directory);
    if !inherited.is_empty() {
        path.push(":");
        path.push(&inherited);
    }
    path
}

/// One name, or a list of them — `SbarLua`'s `sbar.remove` takes either.
fn names_from_value(value: &Value, what: &str) -> mlua::Result<Vec<String>> {
    match value {
        Value::String(s) => Ok(vec![s.to_str()?.to_string()]),
        Value::Table(names) => names
            .clone()
            .sequence_values::<mlua::LuaString>()
            .map(|entry| Ok(entry?.to_str()?.to_string()))
            .collect(),
        other => Err(mlua::Error::RuntimeError(format!(
            "{what} takes an item name or a table of them, got {}",
            other.type_name()
        ))),
    }
}

/// `rsbar.subscribe(item, events, callback)` — the same registration
/// `item:subscribe` does, for a config holding the name rather than the
/// handle.
fn subscribe_fn(lua: &Lua, registry: &Arc<Registry>) -> mlua::Result<Function> {
    let registry = Arc::clone(registry);
    lua.create_function(
        move |_, (item, events, callback): (Value, Value, Function)| {
            let name = match &item {
                Value::UserData(data) => data.borrow::<Item>()?.name.clone(),
                other => item_name_from_str(&value_to_string(other, "an item name")?)?,
            };
            for kind in kinds_from_value(&events)? {
                registry.subscribe(&name, kind, callback.clone());
            }
            Ok(())
        },
    )
}

/// `rsbar.animate(curve, duration, function() ... end)` — runs the body
/// straight through. rsbar has no animation to hand the curve to, so the sets
/// inside land at once instead of over `duration` ticks; the config still
/// works, which is the point, and the difference is visible rather than
/// silent.
fn animate_fn(lua: &Lua) -> mlua::Result<Function> {
    lua.create_async_function(
        move |_, (curve, duration, body): (String, u32, Function)| async move {
            tracing::warn!(
                %curve,
                duration,
                "rsbar cannot animate yet; applying this block's changes at once"
            );
            body.call_async::<()>(()).await
        },
    )
}

/// `rsbar.delay(seconds, function() ... end)`.
fn delay_fn(lua: &Lua) -> mlua::Result<Function> {
    lua.create_async_function(move |_, (seconds, body): (f64, Function)| async move {
        tokio::time::sleep(std::time::Duration::from_secs_f64(seconds.max(0.0))).await;
        body.call_async::<()>(()).await
    })
}

/// `rsbar.hotload(true)` — accepted and ignored. The daemon watches its own
/// configured file either way, so there is nothing for a config to turn on.
fn hotload_fn(lua: &Lua) -> mlua::Result<Function> {
    lua.create_function(move |_, enabled: bool| {
        tracing::debug!(
            enabled,
            "rsbar reloads its own config; hotload is always on"
        );
        Ok(())
    })
}

/// `rsbar.set_bar_name(name)` — refused rather than silently talking to the
/// wrong daemon: which bar this connects to is `RSBAR_SERVICE`, fixed when
/// the dispatcher was built.
fn set_bar_name_fn(lua: &Lua) -> mlua::Result<Function> {
    lua.create_function(move |_, name: String| {
        tracing::error!(
            %name,
            "rsbar picks its daemon from RSBAR_SERVICE, not from the config; ignoring"
        );
        Ok(())
    })
}

fn run_fn(lua: &Lua, registry: &Arc<Registry>) -> mlua::Result<Function> {
    let registry = Arc::clone(registry);
    lua.create_async_function(move |lua, ()| {
        let registry = Arc::clone(&registry);
        async move { registry.run(&lua).await }
    })
}

#[cfg(test)]
mod tests {
    use super::{Dispatcher, install};
    use crate::dispatch::{BoxFuture, BoxedEventStream};
    use mlua::Lua;
    use rsbar_protocol::{
        BoolChange, ComponentKind, ItemName, ItemPatch, Kind, Position, Request, Response, Selector,
    };
    use std::sync::{Arc, Mutex};

    /// Answers every request `Ok` and keeps it, so a test can assert on what
    /// a Lua call actually put on the wire rather than on what it returned.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<Request>>);

    impl Dispatcher for Recorder {
        fn call(&self, request: Request) -> BoxFuture<'_, crate::error::Result<Response>> {
            self.0.lock().expect("no other holder").push(request);
            Box::pin(async { Ok(Response::Ok) })
        }

        fn subscribe(
            &self,
            _name: ItemName,
            _events: Vec<Kind>,
        ) -> BoxFuture<'_, crate::error::Result<BoxedEventStream>> {
            unreachable!("nothing here opens an event stream")
        }
    }

    /// Runs `script` against a fresh `rsbar` table and hands back both the
    /// chunk's result and everything the dispatcher saw.
    fn run(script: &str) -> (mlua::Result<()>, Vec<Request>) {
        let lua = Lua::new();
        let recorder = Arc::new(Recorder::default());
        let table = install(&lua, Arc::clone(&recorder) as Arc<dyn Dispatcher>).unwrap();
        lua.globals().set("rsbar", table).unwrap();
        // `enable_all`, so `rsbar.delay` has a timer to sleep on.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(lua.load(script).exec_async());
        let requests = recorder.0.lock().expect("no other holder").clone();
        (result, requests)
    }

    /// The requests `script` produced, failing the test if it did not run.
    fn requests(script: &str) -> Vec<Request> {
        let (result, requests) = run(script);
        result.unwrap();
        requests
    }

    fn name(text: &str) -> ItemName {
        ItemName::new(text).unwrap()
    }

    /// Every `Set` in `requests`, in order — the shape most of these tests
    /// assert on, since an `add` always leaves an `Add` in front of one.
    fn patches(requests: &[Request]) -> Vec<&ItemPatch> {
        requests
            .iter()
            .filter_map(|request| match request {
                Request::Set(_, patch) => Some(&**patch),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn adding_an_event_declares_one_the_config_fires_itself() {
        assert_eq!(
            requests(r#"rsbar.add("event", "demo")"#),
            vec![Request::AddEvent {
                name: "demo".parse().unwrap(),
                notification: None,
            }]
        );
    }

    #[test]
    fn adding_an_event_with_a_notification_bridges_it() {
        assert_eq!(
            requests(r#"rsbar.add("event", "demo", "com.example.thing")"#),
            vec![Request::AddEvent {
                name: "demo".parse().unwrap(),
                notification: Some("com.example.thing".parse().unwrap()),
            }]
        );
    }

    #[test]
    fn adding_a_built_in_event_is_an_error_rather_than_a_second_one() {
        let (result, requests) = run(r#"rsbar.add("event", "volume_changed")"#);
        assert!(result.is_err(), "a built-in is already defined");
        assert!(requests.is_empty(), "and nothing reached the daemon");
    }

    // --- the metatable ----------------------------------------------------

    #[test]
    fn assigning_a_property_sends_the_same_patch_a_set_would() {
        let through_metatable = requests(
            r#"local item = rsbar.add("item", "clock")
               item.label = "12:00""#,
        );
        let through_set = requests(
            r#"local item = rsbar.add("item", "clock")
               item:set({ label = "12:00" })"#,
        );
        assert_eq!(through_metatable, through_set);
        assert_eq!(
            patches(&through_metatable).last().unwrap().label,
            Some(rsbar_protocol::RunPatch {
                text: Some("12:00".into()),
                ..Default::default()
            })
        );
    }

    #[test]
    fn a_popups_drawing_is_reachable_through_the_metatable() {
        // The headline case: `item.popup.drawing = true` is one `Request::Set`
        // carrying `popup.drawing`, not a namespace object the daemon sees.
        let requests = requests(
            r#"local item = rsbar.add("item", "volume")
               item.popup.drawing = true"#,
        );
        let patch = patches(&requests).last().copied().unwrap();
        assert_eq!(
            patch.popup.as_ref().unwrap().drawing,
            Some(BoolChange::True)
        );
        assert_eq!(
            requests.last().unwrap(),
            &Request::Set(Selector::Name(name("volume")), Box::new(patch.clone()))
        );
    }

    #[test]
    fn a_popup_can_be_toggled_through_the_metatable_too() {
        // What a click script does: only the daemon can resolve `toggle`.
        let requests = requests(
            r#"local item = rsbar.add("item", "volume")
               item.popup.drawing = "toggle""#,
        );
        assert_eq!(
            patches(&requests)
                .last()
                .unwrap()
                .popup
                .as_ref()
                .unwrap()
                .drawing,
            Some(BoolChange::Toggle)
        );
    }

    #[test]
    fn a_namespace_keeps_accumulating_the_path() {
        let requests = requests(
            r#"local item = rsbar.add("item", "clock")
               item.icon.color = 0xffff0000
               item.background.corner_radius = 6
               item.icon.font.size = 14
               item.popup.background.border_width = 1"#,
        );
        let patches = patches(&requests);
        assert_eq!(
            patches[1].icon.as_ref().unwrap().color,
            Some(rsbar_protocol::Color(0xffff_0000))
        );
        assert_eq!(
            patches[2]
                .geometry
                .as_ref()
                .unwrap()
                .background
                .as_ref()
                .unwrap()
                .corner_radius,
            Some(6.0)
        );
        assert_eq!(
            patches[3]
                .icon
                .as_ref()
                .unwrap()
                .font
                .as_ref()
                .unwrap()
                .size
                .to_string(),
            "14"
        );
        assert_eq!(
            patches[4]
                .popup
                .as_ref()
                .unwrap()
                .background
                .as_ref()
                .unwrap()
                .border_width,
            Some(1.0)
        );
    }

    #[test]
    fn a_property_assignment_only_touches_what_it_named() {
        // The daemon marks a component dirty just by being handed one, so an
        // assignment must not synthesise the halves it said nothing about.
        let requests = requests(
            r#"local item = rsbar.add("item", "clock")
               item.popup.drawing = true"#,
        );
        let patch = patches(&requests).last().copied().unwrap();
        assert!(patch.icon.is_none());
        assert!(patch.label.is_none());
        assert!(patch.geometry.is_none());
    }

    #[test]
    fn methods_and_property_assignment_live_on_the_same_handle() {
        let requests = requests(
            r#"local item = rsbar.add("item", "clock")
               assert(item:name() == "clock")
               item.label = "hi"
               item:set({ drawing = true })
               item:subscribe("front_app_switched", function() end)
               item:remove()"#,
        );
        assert!(matches!(requests.last().unwrap(), Request::Remove(_)));
    }

    // --- the generic add --------------------------------------------------

    #[test]
    fn every_kind_reaches_the_daemon_as_its_own_component() {
        let added: Vec<ComponentKind> = requests(
            r#"rsbar.add("item", "an_item")
               rsbar.add("alias", "Control Centre,FocusModes")
               rsbar.add("bracket", "a_bracket", { "an_item" })
               rsbar.add("space", "a_space")
               rsbar.add("graph", "a_graph", 50)
               rsbar.add("slider", "a_slider", 100)"#,
        )
        .into_iter()
        .filter_map(|request| match request {
            Request::Add(kind) => Some(kind),
            _ => None,
        })
        .collect();

        assert_eq!(
            added,
            vec![
                ComponentKind::Item {
                    name: name("an_item"),
                    position: Position::Left,
                },
                ComponentKind::Alias {
                    name: name("Control Centre,FocusModes"),
                    position: Position::Left,
                },
                ComponentKind::Bracket {
                    name: name("a_bracket"),
                    members: vec![Selector::Name(name("an_item"))],
                },
                ComponentKind::Space {
                    name: name("a_space"),
                    position: Position::Left,
                },
                ComponentKind::Graph {
                    name: name("a_graph"),
                    position: Position::Left,
                },
                ComponentKind::Slider {
                    name: name("a_slider"),
                    position: Position::Left,
                },
            ]
        );
    }

    #[test]
    fn an_alias_mirrors_the_menu_bar_item_it_is_named_after() {
        let requests = requests(r#"rsbar.add("alias", "Control Centre,FocusModes")"#);
        assert_eq!(
            patches(&requests)[0].alias.as_deref(),
            Some("Control Centre,FocusModes")
        );
    }

    #[test]
    fn an_explicit_alias_wins_over_the_items_own_name() {
        let requests =
            requests(r#"rsbar.add("alias", "focus", { alias = "Control Centre,FocusModes" })"#);
        assert_eq!(
            patches(&requests)[0].alias.as_deref(),
            Some("Control Centre,FocusModes")
        );
    }

    #[test]
    fn an_unnamed_add_is_counted_per_kind() {
        let names: Vec<String> = requests(
            r#"rsbar.add("bracket", { "a" })
               rsbar.add("item", {})
               rsbar.add("bracket", { "b" })"#,
        )
        .iter()
        .filter_map(|request| match request {
            Request::Add(kind) => Some(kind.placement().0.to_string()),
            _ => None,
        })
        .collect();
        assert_eq!(names, vec!["bracket.1", "item.1", "bracket.2"]);
    }

    #[test]
    fn a_brackets_members_ride_on_the_patch_so_patterns_survive() {
        // The daemon keeps only literal names off a `ComponentKind`, and the
        // reference config's menu bracket is written as a `/pattern/`.
        let requests = requests(r#"rsbar.add("bracket", { "/menu\\..*/" }, {})"#);
        assert_eq!(
            patches(&requests)[0].members,
            Some(vec![Selector::Pattern("menu\\..*".into())])
        );
    }

    #[test]
    fn a_sliders_positional_width_becomes_the_items_width() {
        let requests = requests(
            r#"rsbar.add("slider", "volume_options", 100, { position = "popup.volume" })"#,
        );
        assert_eq!(
            patches(&requests)[0].geometry.as_ref().unwrap().width,
            Some(100.0)
        );
    }

    #[test]
    fn a_popup_child_hangs_off_the_item_that_owns_the_popup() {
        let requests = requests(
            r#"rsbar.add("alias", "Fantastical,Fantastical", { position = "popup.overflow" })"#,
        );
        assert_eq!(
            requests[0],
            Request::Add(ComponentKind::Alias {
                name: name("Fantastical,Fantastical"),
                position: Position::Popup(name("overflow")),
            })
        );
    }

    #[test]
    fn an_unknown_kind_is_still_added_as_a_plain_item() {
        let requests = requests(r#"rsbar.add("widget", "thing", { label = "hi" })"#);
        assert_eq!(
            requests[0],
            Request::Add(ComponentKind::Item {
                name: name("thing"),
                position: Position::Left,
            })
        );
    }

    #[test]
    fn a_second_argument_that_is_neither_a_name_nor_the_kinds_own_is_refused() {
        let (result, requests) = run(r#"rsbar.add("bracket", 42, {})"#);
        assert!(result.is_err(), "42 is neither a name nor members");
        assert!(requests.is_empty());
    }

    #[test]
    fn defaults_are_merged_into_every_add() {
        let requests = requests(
            r#"rsbar.default({ icon = { color = 0xffff0000 } })
               rsbar.add("item", "clock", { label = "12:00" })"#,
        );
        let patch = patches(&requests)[0];
        assert_eq!(
            patch.icon.as_ref().unwrap().color,
            Some(rsbar_protocol::Color(0xffff_0000))
        );
        assert_eq!(patch.label.as_ref().unwrap().text.as_deref(), Some("12:00"));
    }

    // --- the rest of the surface -----------------------------------------

    #[test]
    fn a_graph_takes_samples_by_name_and_through_its_handle() {
        assert_eq!(
            requests(
                r#"local graph = rsbar.add("graph", "cpu", 50)
                   graph:push(0.5)
                   rsbar.push("cpu", 0.25)"#
            )
            .into_iter()
            .filter(|request| matches!(request, Request::Push { .. }))
            .collect::<Vec<_>>(),
            vec![
                Request::Push {
                    name: name("cpu"),
                    value: 0.5,
                },
                Request::Push {
                    name: name("cpu"),
                    value: 0.25,
                },
            ]
        );
    }

    #[test]
    fn moving_and_reordering_are_reachable_without_shelling_out() {
        assert_eq!(
            requests(
                r#"rsbar.move("chevron", "after", "front_app")
                   rsbar.reorder({ "chevron", "front_app" })"#
            ),
            vec![
                Request::Move {
                    name: name("chevron"),
                    relative: rsbar_protocol::Relative::After,
                    reference: name("front_app"),
                },
                Request::Reorder(vec![name("chevron"), name("front_app")]),
            ]
        );
    }

    #[test]
    fn set_by_name_also_takes_sketchybars_pattern_shorthand() {
        // How one module reaches into items another created — the reference
        // config's `sbar.set("/menu\\..*/", { drawing = false })`.
        let requests = requests(r#"rsbar.set("/menu\\..*/", { drawing = false })"#);
        assert!(matches!(
            &requests[0],
            Request::Set(Selector::Pattern(pattern), _) if pattern == "menu\\..*"
        ));
    }

    #[test]
    fn remove_takes_one_name_or_a_list_of_them() {
        assert_eq!(
            requests(
                r#"rsbar.remove("clock")
                   rsbar.remove({ "a", "/menu\\..*/" })"#
            ),
            vec![
                Request::Remove(Selector::Name(name("clock"))),
                Request::Remove(Selector::Name(name("a"))),
                Request::Remove(Selector::Pattern("menu\\..*".into())),
            ]
        );
    }

    #[test]
    fn an_animated_block_still_applies_its_changes() {
        let requests = requests(
            r#"local item = rsbar.add("item", "clock")
               rsbar.animate("sin", 30, function()
                   item.label = "12:00"
               end)"#,
        );
        assert_eq!(
            patches(&requests)
                .last()
                .unwrap()
                .label
                .as_ref()
                .unwrap()
                .text
                .as_deref(),
            Some("12:00")
        );
    }

    #[test]
    fn a_delayed_block_runs_after_the_wait() {
        let requests = requests(
            r#"local item = rsbar.add("item", "clock")
               rsbar.delay(0, function() item.label = "later" end)"#,
        );
        assert_eq!(
            patches(&requests)
                .last()
                .unwrap()
                .label
                .as_ref()
                .unwrap()
                .text
                .as_deref(),
            Some("later")
        );
    }

    #[test]
    fn subscribing_by_name_reaches_the_same_registry_the_handle_does() {
        // Purely local until `rsbar.run()`, so the assertion is that neither
        // spelling raises and neither puts anything on the wire.
        assert_eq!(
            requests(
                r#"local item = rsbar.add("item", "clock")
                   rsbar.subscribe(item, "front_app_switched", function() end)
                   rsbar.subscribe("clock", { "space_change" }, function() end)"#
            )
            .into_iter()
            .filter(|request| matches!(request, Request::Subscribe { .. }))
            .count(),
            0
        );
    }
}

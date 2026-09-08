//! Runs a real `SketchyBar` config through the Lua API and prints the
//! requests it produces, without a daemon anywhere near it.
//!
//! The check this exists for is the one no unit test makes: that every shape
//! a config actually writes still reads. Point it at a config directory —
//! `cargo run -p rsbar-lua --example replay_config -- ~/dendritic/sketchybar`
//! — and read the requests, and the warnings, that come out.

use std::sync::{Arc, Mutex};

use futures_lite::stream;
use mlua::Lua;
use rsbar_lua::dispatch::{BoxFuture, BoxedEventStream, Dispatcher};
use rsbar_protocol::{ItemName, Kind, Request, Response};

/// Answers every request the way a daemon with nothing in it would, and keeps
/// what it was asked.
#[derive(Default)]
struct Recorder(Mutex<Vec<Request>>);

impl Dispatcher for Recorder {
    fn call(&self, request: Request) -> BoxFuture<'_, rsbar_lua::Result<Response>> {
        self.0.lock().expect("no other holder").push(request);
        Box::pin(async { Ok(Response::Ok) })
    }

    fn subscribe(
        &self,
        _name: ItemName,
        _events: Vec<Kind>,
    ) -> BoxFuture<'_, rsbar_lua::Result<BoxedEventStream>> {
        Box::pin(async { Ok(Box::pin(stream::empty()) as BoxedEventStream) })
    }
}

fn main() -> mlua::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Extra directories after the first are search paths only: a real config
    // usually pulls a module or two (a generated `colors`) from somewhere
    // else on `package.path`.
    let mut args = std::env::args().skip(1);
    let root = args.next().expect("a config directory");
    let extra: Vec<String> = args.collect();

    // Everything the real host does, for the same reasons — see
    // `src/bin/rsbar_lua.rs`: an unsafe state so a config's own `require` can
    // reach a native module, this crate's `require` so a module can yield at
    // its top level, and one coroutine on a current-thread runtime so an
    // `item:set{...}` reads as an ordinary call while awaiting underneath.
    let lua = unsafe { Lua::unsafe_new() };
    let recorder = Arc::new(Recorder::default());
    let rsbar = rsbar_lua::api::install(&lua, recorder.clone())?;
    rsbar_lua::require::install(&lua)?;

    // A config written for SketchyBar reaches the API under its own name, by
    // `require` as well as as a global.
    lua.globals().set("sbar", rsbar.clone())?;
    lua.globals().set("sketchybar", rsbar.clone())?;
    let loaded: mlua::Table = lua.globals().get::<mlua::Table>("package")?.get("loaded")?;
    loaded.set("sketchybar", rsbar.clone())?;
    loaded.set("rsbar", rsbar)?;
    // The config's own directory ends up first: an extra path is a fallback
    // for a module the config gets from elsewhere, never a shadow for one it
    // ships itself.
    for dir in extra.iter().rev().chain(std::iter::once(&root)) {
        lua.load(format!(
            "package.path = '{dir}/?.lua;{dir}/?/init.lua;' .. package.path"
        ))
        .exec()?;
    }

    let script = std::fs::read_to_string(format!("{root}/init.lua"))
        .or_else(|_| std::fs::read_to_string(format!("{root}/sketchybarrc")))
        .expect("a config to run");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime");
    let outcome = runtime.block_on(lua.load(&script).set_name("config").exec_async());

    let requests = recorder.0.lock().expect("no other holder");
    println!("\n{} requests", requests.len());
    for request in requests.iter() {
        println!("{request:?}");
    }
    outcome
}

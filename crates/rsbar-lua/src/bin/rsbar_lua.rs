//! Runs a `.lua` config against a running `rsbard`.
//!
//! Hosts a vendored `LuaJIT` itself and seeds `package.loaded.rsbar` before
//! loading the script, so `require("rsbar")` finds the module already there
//! — no dynamic loading, so there is only ever one Lua runtime involved. See
//! `Cargo.toml` for why that matters and when the `cdylib`/`module` build is
//! used instead.
//!
//! The whole script runs as one Lua coroutine (`exec_async`) on a
//! current-thread Tokio runtime, so `rsbar.bar(...)`, `item:set(...)` and
//! `rsbar.run()` all read as ordinary calls in the config while actually
//! awaiting their dispatcher underneath.

use std::process::ExitCode;
use std::rc::Rc;

use mlua::Lua;
use rsbar_lua::{IpcDispatcher, api};

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("RSBAR_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: rsbar-lua <config.lua>");
        return ExitCode::FAILURE;
    };

    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("{path}: {err}");
            return ExitCode::FAILURE;
        }
    };

    let lua = Lua::new();
    if let Err(err) = seed_module(&lua) {
        eprintln!("rsbar: {err}");
        return ExitCode::FAILURE;
    }

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("could not start the runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(lua.load(&source).set_name(&path).exec_async()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn seed_module(lua: &Lua) -> mlua::Result<()> {
    let dispatcher: Rc<dyn rsbar_lua::Dispatcher> = Rc::new(IpcDispatcher::new());
    let table = api::install(lua, dispatcher)?;

    let package: mlua::Table = lua.globals().get("package")?;
    let loaded: mlua::Table = package.get("loaded")?;
    loaded.set("rsbar", table)
}

//! Runs a `.lua` config against a running `rsbard`.
//!
//! Hosts a vendored `LuaJIT` itself and seeds `package.loaded.rsbar` before
//! loading the script, so `require("rsbar")` finds the module already there
//! — no dynamic loading, so there is only ever one Lua runtime involved. See
//! `Cargo.toml` for why that matters and when the `cdylib`/`module` build is
//! used instead.
//!
//! `Lua::unsafe_new`, not the safe default: a real config's own `require` may
//! reach a native module (`package.cpath`/`loadlib`), which mlua's safe mode
//! disables outright — see `rsbar_lua::require`.
//!
//! The whole script runs as one Lua coroutine (`exec_async`) on a
//! current-thread Tokio runtime, so `rsbar.bar(...)`, `item:set(...)` and
//! `rsbar.run()` all read as ordinary calls in the config while actually
//! awaiting their dispatcher underneath.

use std::path::Path;
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

    let lua = unsafe { Lua::unsafe_new() };
    if let Err(err) = seed_module(&lua) {
        eprintln!("rsbar: {err}");
        return ExitCode::FAILURE;
    }
    // This bin owns the whole Lua state (no foreign host sharing it), so it
    // can safely replace `require` with one that lets a config's own
    // `require("bar")`-style module calls yield through an async `rsbar`
    // call at their top level — see `rsbar_lua::require` for why the builtin
    // cannot.
    if let Err(err) = rsbar_lua::require::install(&lua) {
        eprintln!("rsbar: {err}");
        return ExitCode::FAILURE;
    }
    if let Err(err) = configure_package_paths(&lua, Path::new(&path)) {
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

/// Seeds `CONFIG_DIR`, if the environment does not already carry one, to the
/// config's own directory — the same thing real `SketchyBar`'s `hotload.c`
/// sets before running a config script — and appends its Lua/native module
/// locations to `package.path`/`package.cpath`. A config is free to overwrite
/// either afterwards; this only fills in the default a bare-bones one never
/// bothered to set for itself.
fn configure_package_paths(lua: &Lua, script: &Path) -> mlua::Result<()> {
    let dir = script.parent().unwrap_or(Path::new("."));
    if std::env::var_os("CONFIG_DIR").is_none() {
        // Safety: single-threaded, and still before the runtime (let alone
        // the config) starts — nothing else can be reading the environment
        // concurrently yet.
        unsafe { std::env::set_var("CONFIG_DIR", dir) };
    }
    let dir = dir.display();

    let package: mlua::Table = lua.globals().get("package")?;
    let path: String = package.get("path")?;
    package.set("path", format!("{path};{dir}/?.lua;{dir}/?/init.lua"))?;
    let cpath: String = package.get("cpath")?;
    package.set("cpath", format!("{cpath};{dir}/?.so;{dir}/?.dylib"))?;
    Ok(())
}

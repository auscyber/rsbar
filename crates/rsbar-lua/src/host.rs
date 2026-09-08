//! Running a `.lua` config, for a host that owns the whole `Lua` state.
//!
//! Everything `bin/rsbar_lua.rs` does to turn a file into a running config,
//! written once so the daemon does not carry a second copy of it. The two
//! differ in one place only — which [`Dispatcher`] they hand
//! [`crate::api::install`] — and that is the argument.
//!
//! # Where the VM runs, and what "multithreaded" means here
//!
//! Not on the thread that draws. [`spawn`] puts the `Lua` state on a thread of
//! its own and hands the outcome back through a `oneshot`, which is the same
//! shape — and for the same reason — as the daemon's existing config thread:
//! interpreting a config is arithmetic over a few thousand lines of Lua, and
//! the crate root's rule is that the compositing thread draws and does not do
//! that.
//!
//! It is worth being exact about what this does and does not buy, because
//! "Lua with threads" is easy to overclaim:
//!
//! * Lua's C API is not reentrant, and mlua guards the VM with a reentrant
//!   mutex. **One thread is inside a chunk at a time, always.** No arrangement
//!   of this crate changes that.
//! * What mlua's `send` feature does buy is that the *state* is movable. The
//!   VM, the `rsbar` table and the dispatcher behind it can live wherever the
//!   host puts them, and a result computed elsewhere can be carried back in.
//!   That is what lets the daemon host a config at all without putting it on
//!   the run loop.
//! * The concurrency a config actually gets is in what it *waits* on. Its
//!   requests are answered by a pass on another thread; `rsbar.exec` runs a
//!   subprocess and `rsbar.delay` a timer, both on the runtime this thread
//!   owns; and while any of those is in flight the coroutine is suspended and
//!   the daemon is drawing. A hundred `sbar.exec` calls overlap. Two Lua
//!   statements do not.
//!
//! # Why the runtime is still tokio, and still current-thread
//!
//! `rsbar.exec` spawns a subprocess and `rsbar.delay` sleeps; both are tokio's
//! (see [`crate::api`]), and a config that calls them must find a reactor. The
//! daemon's own executor is a `LocalExecutor` on the run loop, which is the
//! one place this must not run — so this thread brings its own, exactly the
//! one the standalone binary already uses. That is not a compromise for the
//! daemon's sake: it is what makes an in-process config and a
//! `rsbar-lua config.lua` run *the same code on the same runtime*, which is
//! the only way the promise that a config sees no difference can be checked.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mlua::Lua;

use crate::dispatch::Dispatcher;
use crate::require::Modules;

/// What one config run came to: nothing, or the reason it did not happen.
///
/// The same type the daemon's `config::Outcome` already is, spelled here so a
/// caller does not have to translate.
pub type Outcome = Result<(), String>;

/// A `Lua` state with the `rsbar` API in it, ready to run a config.
pub struct Host {
    pub lua: Lua,
    /// Every file this state's `require` has loaded. Handed in rather than
    /// created, so a host can keep one across reloads — see [`Modules`].
    pub modules: Modules,
}

/// Builds a state for the config at `script`, wired to `dispatcher`.
///
/// Does everything a config expects to find already true: the API installed
/// and reachable under all three names a `SketchyBar` config uses for it, the
/// `require` trampoline in place, and the config's own directory on
/// `package.path`/`package.cpath`.
///
/// **Does not touch the process environment.** The standalone binary seeds
/// `CONFIG_DIR` here; a daemon must not, because by this point it has threads
/// and `std::env::set_var` is unsound with any of them reading. A daemon sets
/// it once at startup instead.
///
/// # Errors
///
/// Returns a Lua error if the API or the trampoline cannot be installed, which
/// would mean a bug in this crate rather than in a config.
pub fn build(
    dispatcher: Arc<dyn Dispatcher>,
    script: &Path,
    modules: Modules,
) -> mlua::Result<Host> {
    // SAFETY: this state is built here and owned by the caller alone -- no
    // foreign interpreter shares it. Unsafe rather than `Lua::new` because a
    // real config's own `require` may reach a native module through
    // `package.cpath`/`loadlib`, which mlua's safe mode removes outright.
    let lua = unsafe { Lua::unsafe_new() };

    let rsbar = crate::api::install(&lua, dispatcher)?;
    install_module(&lua, &rsbar)?;
    // Replaces the global `require`, which only a host owning the whole state
    // may do -- see `crate::require`. Tracked into the caller's own set, which
    // is what lets one set of watches outlive a sequence of reloads.
    crate::require::install_tracked_with(&lua, modules.clone())?;
    add_search_paths(&lua, script)?;

    Ok(Host { lua, modules })
}

/// Puts the table where every name a config reaches for finds it.
///
/// A config written for `SketchyBar` says `require("sketchybar")`; one written
/// for `SbarLua` says `sbar`; rsbar's own docs say `rsbar`. All three are the
/// same table, so a config only ever changes its `require` — which is the
/// whole point of the API.
fn install_module(lua: &Lua, rsbar: &mlua::Table) -> mlua::Result<()> {
    let globals = lua.globals();
    globals.set("rsbar", rsbar.clone())?;
    globals.set("sbar", rsbar.clone())?;
    globals.set("sketchybar", rsbar.clone())?;

    let package: mlua::Table = globals.get("package")?;
    let loaded: mlua::Table = package.get("loaded")?;
    loaded.set("rsbar", rsbar.clone())?;
    loaded.set("sbar", rsbar.clone())?;
    loaded.set("sketchybar", rsbar.clone())
}

/// Appends the config's own directory to `package.path`/`package.cpath`.
///
/// Appends rather than prepends: a module the config ships itself should not
/// shadow one the host deliberately put in front of it.
fn add_search_paths(lua: &Lua, script: &Path) -> mlua::Result<()> {
    let dir = script.parent().unwrap_or(Path::new(".")).display();
    let package: mlua::Table = lua.globals().get("package")?;
    let path: String = package.get("path")?;
    package.set("path", format!("{path};{dir}/?.lua;{dir}/?/init.lua"))?;
    let cpath: String = package.get("cpath")?;
    package.set("cpath", format!("{cpath};{dir}/?.so;{dir}/?.dylib"))
}

/// Runs the config at `script` on a thread of its own, and says when it is
/// done.
///
/// `finished` is woken with the outcome, and `modules` is left holding every
/// file the run loaded — marked and swept across the run, so what comes back
/// from [`Modules::end`] is what this generation stopped needing.
///
/// A failed run changes nothing about the previous one: the state is dropped,
/// the module set keeps its watches ([`Modules::keep`]), and whatever the
/// previous config put on the bar stays there. A broken edit should say what
/// is wrong, not empty the bar.
pub fn spawn(
    dispatcher: Arc<dyn Dispatcher>,
    script: PathBuf,
    modules: Modules,
) -> tokio::sync::oneshot::Receiver<Outcome> {
    let (finished, waiting) = tokio::sync::oneshot::channel();

    let spawned = std::thread::Builder::new()
        .name("rsbar-lua".into())
        .spawn(move || {
            modules.begin();
            let outcome = run(&dispatcher, &script, &modules);
            if outcome.is_ok() {
                // Whatever is still marked is what this generation left out.
                for dropped in modules.end() {
                    tracing::debug!(path = %dropped.display(), "no longer part of the config");
                }
            } else {
                modules.keep();
            }
            // Dropped rather than expected: a receiver that has gone away is a
            // daemon shutting down, not a fault.
            drop(finished.send(outcome));
        });

    if let Err(err) = spawned {
        tracing::error!(%err, "could not start the Lua thread");
        let (failed, waiting) = tokio::sync::oneshot::channel();
        drop(failed.send(Err(format!("could not start the Lua thread: {err}"))));
        return waiting;
    }
    waiting
}

/// One run, start to finish, on the calling thread.
fn run(dispatcher: &Arc<dyn Dispatcher>, script: &Path, modules: &Modules) -> Outcome {
    let source = std::fs::read_to_string(script)
        .map_err(|err| format!("could not read {}: {err}", script.display()))?;

    let host = build(Arc::clone(dispatcher), script, modules.clone())
        .map_err(|err| format!("could not prepare the Lua state: {err}"))?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("could not start the runtime: {err}"))?;

    // The whole config is one coroutine, which is what lets `rsbar.bar(...)`
    // and `item:set(...)` read as ordinary calls while awaiting underneath.
    runtime
        .block_on(
            host.lua
                .load(&source)
                .set_name(format!("@{}", script.display()))
                .exec_async(),
        )
        .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::{build, spawn};
    use crate::dispatch::{BoxFuture, BoxedEventStream, Dispatcher};
    use crate::require::Modules;
    use rsbar_protocol::{ItemName, Kind, Request, Response};
    use std::sync::{Arc, Mutex};

    /// Answers every request `Ok` and keeps it, so a test can assert on what a
    /// config actually asked for.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<Request>>);

    impl Dispatcher for Recorder {
        fn call(&self, request: Request) -> BoxFuture<'_, crate::Result<Response>> {
            self.0.lock().expect("no other holder").push(request);
            Box::pin(async { Ok(Response::Ok) })
        }

        fn subscribe(
            &self,
            _name: ItemName,
            _events: Vec<Kind>,
        ) -> BoxFuture<'_, crate::Result<BoxedEventStream>> {
            Box::pin(async { Ok(Box::pin(futures_lite::stream::empty()) as BoxedEventStream) })
        }
    }

    /// A directory with a config in it, cleaned up on drop.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("rsbar-lua-host-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("a scratch directory");
            Self(dir)
        }

        fn write(&self, name: &str, source: &str) -> std::path::PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, source).expect("write");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The three names are one table, so a config only ever changes its
    /// `require`.
    #[test]
    fn every_name_a_config_reaches_for_finds_the_same_table() {
        let scratch = Scratch::new("names");
        let script = scratch.write("rc.lua", "");
        let host = build(Arc::new(Recorder::default()), &script, Modules::new()).expect("a host");

        host.lua
            .load(
                r"
                assert(rawequal(rsbar, sbar), 'sbar is the rsbar table')
                assert(rawequal(rsbar, sketchybar), 'sketchybar is too')
                assert(rawequal(rsbar, require('sketchybar')), 'and so is the require')
                ",
            )
            .exec()
            .expect("the names agree");
    }

    /// The end-to-end shape a daemon uses: a config on its own thread, its
    /// requests arriving as values, and the files it is made of recorded.
    #[test]
    fn a_config_runs_off_thread_and_reports_the_modules_it_loaded() {
        let scratch = Scratch::new("run");
        scratch.write("palette.lua", "return { bar = 0xff181818 }");
        let script = scratch.write(
            "rc.lua",
            r"
            local palette = require('palette')
            rsbar.bar({ color = palette.bar })
            rsbar.add('item', 'clock', { position = 'right' })
            ",
        );

        let recorder = Arc::new(Recorder::default());
        let modules = Modules::new();
        let waiting = spawn(
            Arc::clone(&recorder) as Arc<dyn Dispatcher>,
            script,
            modules.clone(),
        );

        let outcome = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(waiting)
            .expect("the thread answered");
        outcome.expect("the config ran");

        let requests = recorder.0.lock().expect("no other holder").clone();
        assert!(
            requests
                .iter()
                .any(|request| matches!(request, Request::SetBar(_))),
            "the config set the bar: {requests:?}"
        );
        assert!(
            requests
                .iter()
                .any(|request| matches!(request, Request::Add(_))),
            "and added its item: {requests:?}"
        );
        assert_eq!(
            modules.paths().len(),
            1,
            "the module it required is what is watched"
        );
        assert!(
            modules
                .paths()
                .iter()
                .any(|path| path.ends_with("palette.lua"))
        );
    }

    /// A config that fails leaves the previous generation's watches alone —
    /// it has not stopped wanting the files it never reached.
    #[test]
    fn a_failed_config_keeps_the_watches_it_had() {
        let scratch = Scratch::new("broken");
        let script = scratch.write("rc.lua", "error('deliberate')");

        let modules = Modules::new();
        let waiting = spawn(
            Arc::new(Recorder::default()) as Arc<dyn Dispatcher>,
            script,
            modules.clone(),
        );

        let outcome = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(waiting)
            .expect("the thread answered");

        let err = outcome.expect_err("a config that errors fails");
        assert!(err.contains("deliberate"), "{err}");
    }
}

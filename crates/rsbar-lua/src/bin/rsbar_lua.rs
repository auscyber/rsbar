//! Runs a `.lua` config against a running `rsbard`.
//!
//! Almost nothing happens here. [`rsbar_lua::host`] is what turns a file into
//! a running config — the `Lua` state, the API under all three names a config
//! reaches for, the `require` trampoline, the search paths, the runtime — and
//! this binary's whole contribution is choosing the [`IpcDispatcher`] and
//! seeding `CONFIG_DIR`.
//!
//! That is deliberate, and it is what the promise rests on: a config the
//! daemon hosts in-process and a config run through here go through the *same*
//! host code on the *same* runtime, differing only in whether a request
//! becomes a Mach message or a value in a queue. There is no second
//! implementation to drift.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use rsbar_lua::{IpcDispatcher, host, require::Modules};

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
    let path = PathBuf::from(path);

    seed_config_dir(&path);

    let dispatcher: Arc<dyn rsbar_lua::Dispatcher> = Arc::new(IpcDispatcher::new());
    // On a thread of its own even here, where nothing else needs this one:
    // it is the same call the daemon makes, so this binary exercises the path
    // that matters rather than a shortcut past it.
    let waiting = host::spawn(dispatcher, path, Modules::new());

    let runtime = match tokio::runtime::Builder::new_current_thread().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("could not start the runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(waiting) {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(err)) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
        Err(_) => {
            eprintln!("the config thread went away without answering");
            ExitCode::FAILURE
        }
    }
}

/// Seeds `CONFIG_DIR` to the config's own directory — the same thing real
/// `SketchyBar`'s `hotload.c` sets before running a config script. A config is
/// free to overwrite it afterwards; this only fills in the default a
/// bare-bones one never bothered to set for itself.
///
/// Here rather than in [`host::build`] because only this process can honestly
/// do it: a daemon has threads by the time it runs a config, and
/// `std::env::set_var` is unsound with any of them reading the environment.
fn seed_config_dir(script: &Path) {
    if std::env::var_os("CONFIG_DIR").is_some() {
        return;
    }
    let dir = script.parent().unwrap_or(Path::new("."));
    // SAFETY: still single-threaded -- before the runtime, before the config
    // thread, and before the config itself -- so nothing else can be reading
    // the environment concurrently yet.
    unsafe { std::env::set_var("CONFIG_DIR", dir) };
}

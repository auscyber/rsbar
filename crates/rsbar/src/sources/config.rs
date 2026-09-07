//! Watching the config file.
//!
//! A source like any other, so a config change reaches the app through the same
//! stream everything else does.

use crate::config::{self, Debounce, Shared};
use crate::sources::{Emission, Emitter, Source, StartError};
use notify::{RecursiveMode, Watcher as _};
use rsbar_protocol::Event;
use std::time::Duration;

/// How long one save is allowed to keep producing events.
const DEBOUNCE: Duration = Duration::from_millis(250);

pub struct Watcher {
    pub config: Shared,
}

impl Source for Watcher {
    fn name(&self) -> &'static str {
        "config"
    }

    fn provides(&self) -> Vec<Event> {
        vec![Event::ConfigReloaded]
    }

    fn install(&mut self, emit: Emitter) -> Result<Box<dyn std::any::Any>, StartError> {
        let Some(path) = self.config.blocking_read().path.clone() else {
            return Err(StartError {
                name: "config",
                reason: "there is no config file".to_owned(),
            });
        };

        // Watch the directory, not the file. Editors save by writing a
        // temporary file and renaming it over the original, which replaces the
        // inode a file watch is holding — the watch survives and never fires
        // again.
        let directory = path
            .parent()
            .ok_or_else(|| StartError {
                name: "config",
                reason: format!("{} has no parent directory", path.display()),
            })?
            .to_path_buf();

        let watched = path.clone();
        let mut debounce = Debounce::new(DEBOUNCE);
        let mut observer =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                let Ok(event) = result else { return };
                if !event.paths.iter().any(|p| touches(p, &watched)) {
                    return;
                }
                if !debounce.admit() {
                    return;
                }
                let _ = emit.try_send(Emission::new(Event::ConfigReloaded, None));
            })
            .map_err(|err| StartError {
                name: "config",
                reason: err.to_string(),
            })?;

        observer
            .watch(&directory, RecursiveMode::NonRecursive)
            .map_err(|err| StartError {
                name: "config",
                reason: err.to_string(),
            })?;

        tracing::info!(path = %path.display(), "watching config");
        Ok(Box::new(observer))
    }
}

/// Whether a reported path is the config.
///
/// Compares the file name as well as the whole path, because an atomic rename
/// is reported against the temporary file the editor used, not the target.
fn touches(reported: &std::path::Path, config: &std::path::Path) -> bool {
    reported == config || reported.file_name() == config.file_name()
}

/// Re-runs the config on a thread of its own, recording the outcome.
///
/// **Not on the calling thread.** A config script drives the CLI, and the CLI
/// waits for the daemon to answer — so running it where the daemon's schedules
/// run deadlocks it against its own config. A thread also means a slow config
/// does not stall drawing.
///
/// A failed run leaves the previous generation in place: a broken edit should
/// not tear down a working bar, it should say what is wrong and change nothing.
pub fn reload(config: &Shared, service: String) {
    let path = {
        let guard = config.blocking_read();
        guard.path.clone()
    };
    let Some(path) = path else { return };

    config.blocking_write().running = true;
    let owned = Shared::clone(config);

    let spawned = std::thread::Builder::new()
        .name("rsbar-config".into())
        .spawn(move || {
            let result = config::run(&path, &service);
            let mut guard = owned.blocking_write();
            // Cleared on both paths, and before the branch, so a failure cannot
            // leave the app waiting forever on a run that already finished.
            guard.running = false;
            match result {
                Ok(()) => {
                    guard.generation += 1;
                    guard.last_error = None;
                    tracing::info!(generation = guard.generation, "config reloaded");
                }
                Err(err) => {
                    tracing::error!(%err, "config failed; keeping the previous one");
                    guard.last_error = Some(err);
                }
            }
        });

    if let Err(err) = spawned {
        tracing::error!(%err, "could not spawn the config thread");
        config.blocking_write().running = false;
    }
}

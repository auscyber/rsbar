//! Watching the config file.
//!
//! A source like any other, so a config change reaches the app through the same
//! stream everything else does.

use crate::config::{self, Debounce, Outcome, Shared};
use crate::protocol::event::ConfigReload;
use crate::protocol::{Event, Kind};
use crate::sources::{Cause, Registering, Source, SourceId, StartError};
use notify::{RecursiveMode, Watcher as _};
use std::collections::BTreeSet;
use std::time::Duration;

/// How long one save is allowed to keep producing events.
const DEBOUNCE: Duration = Duration::from_millis(250);

pub struct Watcher {
    pub config: Shared,
}

impl Source for Watcher {
    fn id(&self) -> SourceId {
        SourceId("config")
    }

    fn provides(&self) -> Vec<Kind> {
        vec![Kind::ConfigReloaded]
    }

    fn eager(&self) -> bool {
        true
    }

    fn run(
        &mut self,
        _wanted: &BTreeSet<Kind>,
        cx: Registering,
    ) -> Result<crate::pool::Task, StartError> {
        let emit = cx.emitter();
        let Some(path) = self.config.blocking_read().path.clone() else {
            return Err(StartError::new(self.id(), Cause::NoConfigFile));
        };

        // Watch the directory, not the file. Editors save by writing a
        // temporary file and renaming it over the original, which replaces the
        // inode a file watch is holding — the watch survives and never fires
        // again.
        let directory = path
            .parent()
            .ok_or_else(|| StartError::new(self.id(), Cause::NoParentDirectory(path.clone())))?
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
                emit.send(Event::ConfigReloaded(ConfigReload {}));
            })
            .map_err(|err| StartError::new(SourceId("config"), Cause::from(err)))?;

        observer
            .watch(&directory, RecursiveMode::NonRecursive)
            .map_err(|err| StartError::new(SourceId("config"), Cause::from(err)))?;

        tracing::info!(path = %path.display(), "watching config");

        // The watcher emits straight from its own callback, so this future's
        // only job is to outlive it: dropping the `Task` drops `Watching`,
        // which stops the watch (off-thread -- see `Watching::drop`).
        // `notify::RecommendedWatcher` is `Send`, so this needs no thread of
        // its own the way a `!Send` `Callback`-holding source does.
        let watching = Watching(Some(observer));
        Ok(crate::pool::owned(async move {
            let _watching = watching;
            std::future::pending::<()>().await;
        }))
    }
}

/// The config watcher, torn down off the thread that drops it.
///
/// `FsEventWatcher::drop` calls `stop`, which **joins its `FSEvents` thread** —
/// and that join is the entire cost of this daemon's shutdown: sampled under
/// SIGTERM, 2604 of 2604 main-thread samples were `pthread_join` beneath it,
/// for between 2 and 11 seconds depending on how long `FSEvents` feels like
/// taking.
///
/// Nothing waits on that tidying. So it happens on a thread of its own: at exit
/// the process goes without it, and on a genuine restart it completes
/// concurrently rather than stalling the pass that replaced the watcher.
///
/// `notify`'s `macos_kqueue` feature avoids the `FSEvents` thread entirely and
/// was tried here, but its directory watch leaks a file descriptor on almost
/// every save -- `remove_watch` in `kqueue.rs` walks the removed path with
/// `WalkDir` to unregister it, which errors on a path that is already gone
/// and swallows that error, so the fd is never closed. Rejected; this stays
/// on `FSEvents`.
struct Watching(Option<notify::RecommendedWatcher>);

impl Drop for Watching {
    fn drop(&mut self) {
        if let Some(watcher) = self.0.take() {
            // If the thread cannot be spawned, the closure -- and `watcher`
            // inside it -- never left this thread, and drops here: the
            // blocking join this thread exists to avoid, but it still stops.
            if std::thread::Builder::new()
                .name("coolabah-config-stop".to_owned())
                .spawn(move || drop(watcher))
                .is_err()
            {
                tracing::warn!(
                    "could not hand the config watcher's teardown to a thread of its own"
                );
            }
        }
    }
}

/// Whether a reported path is the config.
///
/// Compares the file name as well as the whole path, because the path a save
/// is reported against is not reliably the one being watched: `FSEvents`
/// resolves through symlinks (`/tmp` against `/private/tmp` is the usual way
/// to see this) and can report a coalesced parent. The name is what survives
/// both.
///
/// It does not catch an editor's atomic rename by the *temporary* file's name
/// -- `coolabahrc.tmp1234` shares no name with `coolabahrc` -- and is not meant to:
/// the rename that follows lands on the real path, which the first comparison
/// matches.
fn touches(reported: &std::path::Path, config: &std::path::Path) -> bool {
    reported == config || reported.file_name() == config.file_name()
}

/// Re-runs the config as a task of its own, and says when it is done.
///
/// **Not on the calling thread.** A config script drives the CLI, and the CLI
/// waits for the daemon to answer — so running it where the daemon's schedules
/// run deadlocks it against its own config. A task on the shared runtime also
/// means a slow config does not stall drawing.
///
/// A failed run leaves the previous generation in place: a broken edit should
/// not tear down a working bar, it should say what is wrong and change nothing.
///
/// # The result comes back, rather than being left somewhere to be found
///
/// This used to set a `running` flag the app read on every tick. That is a
/// poll, and once there was no tick to carry it, it was a poll with nothing
/// driving it: a reload settled only when some unrelated wake happened by, and
/// a config whose last request changed no pixels left the items it dropped on
/// the screen for good.
///
/// The receiver says the same thing directly and carries the answer with it,
/// and `wake` is what makes it an event rather than something to be checked
/// for: the reload task sends, then signals, so the pass that reads the
/// receiver is the pass that signal caused.
///
/// **Nothing here touches the lock on the calling thread.** Even reading the
/// path is left to the reload task: `tokio::sync::RwLock` queues writers
/// fairly, so a read taken on the compositing thread while the previous run's
/// task holds the write guard parks the run loop in the kernel. Short window,
/// wrong thread, no reason to be there at all.
pub fn reload(
    config: &Shared,
    service: String,
    wake: &crate::runloop::Waker,
) -> tokio::sync::oneshot::Receiver<Outcome> {
    let owned = Shared::clone(config);
    let (finished, waiting) = tokio::sync::oneshot::channel();
    let woken = wake.clone();

    crate::pool::spawn(async move {
        let path = owned.read().await.path.clone();
        let result = match path {
            Some(path) => config::run(&path, &service).await,
            // Legitimate: a daemon driven entirely over the CLI has no
            // config to re-run. Reported as a failure because that is
            // what it means to the caller -- nothing was replaced, so
            // nothing the old config put there should go.
            None => Err("there is no config file to run".to_owned()),
        };
        let mut guard = owned.write().await;
        match &result {
            Ok(()) => {
                guard.generation += 1;
                guard.last_error = None;
                tracing::info!(generation = guard.generation, "config reloaded");
            }
            Err(err) => {
                tracing::error!(%err, "config failed; keeping the previous one");
                guard.last_error = Some(err.clone());
            }
        }
        drop(guard);
        // Dropped rather than expected: a receiver that has gone away is
        // a daemon shutting down, not a fault.
        drop(finished.send(result));
        // Sent first, so the pass this asks for finds the answer already
        // there rather than one turn of the loop later.
        woken.wake();
    });

    waiting
}

#[cfg(test)]
mod tests {
    use super::touches;
    use notify::{RecursiveMode, Watcher as _};
    use std::sync::mpsc;
    use std::time::Duration;

    /// Guards the one thing this watcher exists for: write a temp file, then
    /// `rename` it over the target with nothing in between -- an editor's
    /// atomic save -- and check the directory watch still reports it.
    ///
    /// `macos_kqueue` was tried here and rejected: it does catch the rename,
    /// but leaks a file descriptor on almost every save (~1/save measured for
    /// a rename; a plain unlink takes the same `remove_watch` path and leaks
    /// the same way) — see the doc on [`super::Watching`]. This runs against
    /// the default `FSEvents` backend, which does not have that problem.
    ///
    /// Drops the watcher off-thread, same as [`Watching`] does in the real
    /// source, so at least teardown is not what makes this slow.
    ///
    /// Ignored: `FSEvents` itself takes seconds to deliver each save in this
    /// environment, even with `latency: 0.0` — measured at ~3s/generation,
    /// nothing here waits on it artificially. Run explicitly with
    /// `cargo test -p coolabah --lib sources::config -- --ignored`.
    #[test]
    #[ignore = "several real seconds of FSEvents latency per save; run with --ignored"]
    fn an_atomic_rename_over_save_is_caught() {
        let dir = std::env::temp_dir().join(format!(
            "coolabah-config-watch-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Decoy files, so a backend that (mis)reports "some untracked file
        // appeared" against the directory rather than the actual mover has
        // something else to report instead of the config by accident.
        for name in ["a", "b", "c"] {
            std::fs::write(dir.join(format!("{name}.txt")), name).unwrap();
        }

        let config = dir.join(name!(rc));
        std::fs::write(&config, "generation 0").unwrap();

        let (tx, rx) = mpsc::channel::<std::path::PathBuf>();
        let target = config.clone();
        let mut watcher =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                let Ok(event) = result else { return };
                for path in &event.paths {
                    if touches(path, &target) {
                        let _ = tx.send(path.clone());
                    }
                }
            })
            .expect("a watcher");
        watcher
            .watch(&dir, RecursiveMode::NonRecursive)
            .expect("the directory is watchable");

        for generation in 1..=3u32 {
            let temp = dir.join(format!("{}.tmp{generation}", name!(rc)));
            std::fs::write(&temp, format!("generation {generation}")).unwrap();
            std::fs::rename(&temp, &config).unwrap();

            let seen = rx.recv_timeout(Duration::from_secs(5));
            assert!(
                seen.is_ok(),
                "generation {generation}: no event matched the config within 5s"
            );
        }

        // Off-thread and not joined: see the doc above.
        std::thread::spawn(move || drop(watcher));
        std::fs::remove_dir_all(&dir).ok();
    }
}

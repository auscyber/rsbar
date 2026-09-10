//! Noticing that the Accessibility grant arrived, without being restarted.
//!
//! The system's own prompt tells the user to quit and reopen the
//! application. Two things here make that unnecessary:
//!
//! 1. **Being told.** The TCC database is a file, and a write to it is the
//!    signal that the answer may have changed. `AXIsProcessTrusted` re-reads
//!    that database on every call, so the watch says *when* to ask and the
//!    ask gives the answer.
//! 2. **Being replaced.** A grant can land and the API still answer
//!    `kAXErrorAPIDisabled`, which is the case the prompt is actually about:
//!    this image cannot use the permission it now has. The last resort is to
//!    replace it — `exec` with the same argv, environment and working
//!    directory, so the pid and everything holding it survive and only the
//!    image changes. Once, guarded by a marker in that very environment, so a
//!    process that comes back still disabled does not spin.
//!
//! There used to be a third: a 2 s timer re-asking `AXIsProcessTrusted`
//! whether or not anything had changed. It is gone, and nothing replaces it.
//! A permission the user grants by hand is not a thing to poll for — it is a
//! file being written, which is what the watch is. Where the watch cannot be
//! set up, this source reports the grant it saw at start and nothing more
//! until the daemon is restarted; that is said once, at registration, rather
//! than paid for sixty times a minute forever.

use crate::protocol::event::AccessibilityChange;
use crate::protocol::{Event, Kind};
use crate::sources::{Emitter, Registering, Source, SourceId, StartError};
use skylight::ax;
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Set in the environment of the process `replace` starts, so the one attempt
/// this makes stays one attempt.
const RESTARTED: &str = name!(env "AX_RESTARTED");

/// Whether the grant is there and this process can actually use it.
///
/// [`skylight::ax::usable`]'s `NoFrontApp` is not an answer either way — it
/// means there was nothing to ask against — so it counts as trusted, which is
/// what [`skylight::ax::trusted`] already said.
#[must_use]
#[skylight::main_thread]
pub fn usable() -> bool {
    !matches!(
        ax::usable(proof),
        Err(skylight::Error::NotTrusted | skylight::Error::ApiDisabled)
    )
}

/// Where the TCC database lives, both copies of it.
///
/// The user's own is the one an Accessibility grant is written to; the system
/// one is included because a managed machine's grant is written there
/// instead. Neither is read — only watched.
fn tcc_directories() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from("/Library/Application Support/com.apple.TCC")];
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(home).join("Library/Application Support/com.apple.TCC"));
    }
    paths
}

/// Replaces this process with a fresh one: same program, same argv, same
/// environment, same working directory.
///
/// `exec` rather than spawn-and-exit, so there is no window with two of them
/// and no new pid — whatever is supervising this one, launchd included, sees
/// the same process it started, and the sockets and locks it holds are the
/// ones it keeps.
///
/// Only ever reached when the grant is present and the API is disabled
/// anyway, and only once: the new image is handed [`RESTARTED`] and refuses
/// to do this again.
///
/// # Errors
///
/// Returns the reason the replacement did not happen. On success it does not
/// return at all.
fn replace() -> std::io::Error {
    use std::os::unix::process::CommandExt;

    let program = match std::env::current_exe() {
        Ok(path) => path,
        Err(err) => return err,
    };
    let mut command = std::process::Command::new(program);
    command.args(std::env::args_os().skip(1));
    command.env(RESTARTED, "1");
    command.exec()
}

/// Whether this process may still replace itself.
fn may_replace() -> bool {
    std::env::var_os(RESTARTED).is_none()
}

/// The registration: the TCC watch, when one could be had.
struct Watching {
    files: Option<notify::RecommendedWatcher>,
}

impl Drop for Watching {
    /// Drops the watcher **off this thread**.
    ///
    /// `notify`'s `FSEvents` backend runs a `CFRunLoop` on a thread of its own,
    /// and `FsEventWatcher::drop` stops it by joining that thread — which
    /// `FSEvents` can take seconds to answer. Sampled under `SIGTERM`, every
    /// main-thread sample was `pthread_join` beneath this drop, for between two
    /// and eleven seconds, and the daemon is being asked to quit.
    ///
    /// Nothing waits on the result, so the join can happen anywhere. The same
    /// trade [`crate::sources::config`]'s watcher makes, for the same reason.
    fn drop(&mut self) {
        let Some(watcher) = self.files.take() else {
            return;
        };
        if std::thread::Builder::new()
            .name("coolabah-tcc-stop".to_owned())
            .spawn(move || drop(watcher))
            .is_err()
        {
            tracing::warn!("could not hand the TCC watcher's teardown to a thread of its own");
        }
    }
}

/// The grant, reported as an event.
pub struct Accessibility;

/// Reads the grant and reports it if it moved, replacing this process if the
/// grant is present but unusable.
///
/// **On the main thread**, and now enforced rather than described:
/// [`ax::usable`] asks `NSWorkspace` for the frontmost application, so it takes
/// main-thread proof and will not compile without it. This used to run in the
/// `notify` crate's own watcher thread.
fn check(main: &skylight::MainThread, last: &std::sync::atomic::AtomicBool, emit: &Emitter) {
    use std::sync::atomic::Ordering;

    if matches!(ax::usable(main), Err(skylight::Error::ApiDisabled)) && may_replace() {
        tracing::warn!(
            "the Accessibility grant is recorded but this process cannot use it; replacing this \
             image in place"
        );
        let err = replace();
        tracing::error!(%err, "could not replace this process; a manual restart is needed");
        return;
    }

    let now = usable(main);
    if last.swap(now, Ordering::Relaxed) != now {
        tracing::info!(trusted = now, "the Accessibility grant changed");
        emit.send(Event::AccessibilityChanged(AccessibilityChange {
            trusted: now,
        }));
    }
}

/// Answers every poke from the watcher, on the thread `AppKit` needs.
///
/// The grant is one bit, so a burst of TCC writes is one question: the batch is
/// taken and discarded, and the answer read once.
async fn answer(
    main: skylight::MainThread,
    mut changes: skylight::callback::Events<()>,
    emit: Emitter,
) {
    let last = std::sync::atomic::AtomicBool::new(usable(&main));
    while changes.batch(BURST).await.is_some() {
        check(&main, &last, &emit);
    }
}

/// How many pokes one look at the grant answers. A TCC write touches several
/// files; the grant does not change several times.
const BURST: usize = 32;

impl Source for Accessibility {
    fn id(&self) -> SourceId {
        SourceId("accessibility")
    }

    fn provides(&self) -> Vec<Kind> {
        vec![Kind::AccessibilityChanged]
    }

    fn run(
        &mut self,
        _wanted: &BTreeSet<Kind>,
        cx: Registering,
    ) -> Result<crate::pool::Task, StartError> {
        use notify::Watcher;

        let emit = cx.emitter();

        // The watcher's thread reads nothing and decides nothing: it says a
        // file under TCC changed, and the answer is worked out on the thread
        // that is allowed to ask AppKit.
        let (relay, changes) = skylight::callback::relay::<()>();
        let poke = std::sync::Arc::clone(&relay);

        let files = notify::recommended_watcher(move |_| {
            // The event says a file under TCC changed; it never says what the
            // answer now is, so the answer is asked for rather than inferred.
            poke.post(());
        })
        .ok()
        .and_then(|mut watcher| {
            let watched = tcc_directories()
                .into_iter()
                .filter(|dir| {
                    watcher
                        .watch(dir, notify::RecursiveMode::NonRecursive)
                        .is_ok()
                })
                .count();
            (watched > 0).then_some(watcher)
        });

        if files.is_none() {
            // Said once, and only here. This is the whole mechanism now, so a
            // machine that will not let this process watch TCC gets the grant
            // as it stood at start and no further word about it -- which is
            // worth a line in the log, and is not worth a timer.
            tracing::warn!(
                "could not watch the TCC directories; a grant made from now on will not be \
                 noticed until this daemon is restarted"
            );
        }

        // `Watching` and `relay` now live inside the future itself: dropping
        // the `Task` drops both, which stops the watcher (off-thread -- see
        // `Watching::drop`) and ends `answer` by dropping the last `Relay`.
        let watching = Watching { files };
        Ok(crate::runloop::owned(
            cx.main_thread(),
            move |main| async move {
                let _watching = watching;
                let _relay = relay;
                answer(main, changes, emit).await;
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{RESTARTED, may_replace, tcc_directories};

    #[test]
    fn the_replacement_marker_is_what_stops_a_second_one() {
        // The test process is not marked, so it would be allowed one.
        assert_eq!(may_replace(), std::env::var_os(RESTARTED).is_none());
    }

    #[test]
    fn both_tcc_directories_are_watched_when_there_is_a_home() {
        let dirs = tcc_directories();
        assert!(dirs.iter().any(|d| d.starts_with("/Library")));
        if std::env::var_os("HOME").is_some() {
            assert_eq!(dirs.len(), 2);
        }
    }
}

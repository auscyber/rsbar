//! Ending the daemon the way the rest of the system asks it to.
//!
//! `launchctl bootout`, `timeout`, a terminal's Ctrl-C and a profiler ending
//! its target all say the same thing: `SIGTERM` or `SIGINT`. Carbon's event
//! loop runs on the main thread, but the script and Accessibility worker
//! threads are just as eligible to take a process-directed signal, so without
//! a handler installed the outcome depended on which thread caught it: a
//! process that kept running, or one killed with its window server windows
//! still mapped.
//!
//! Handled through `tokio::signal::unix`, which installs a real `sigaction`
//! rather than blocking the signal and parking a thread in `sigwait`. That
//! sounds like the unsafe choice — a handler runs on whatever thread the
//! kernel picks, mid-instruction — but the handler `tokio` installs
//! (`signal-hook-registry`'s `action`) does exactly two things, both
//! async-signal-safe: set an atomic flag and write one byte to a pipe. No
//! lock, no allocator, no `CoreFoundation` state, so it is safe no matter
//! which thread the kernel interrupts. Everything that actually reacts to the
//! signal happens later, as ordinary async code on [`crate::pool`], nowhere
//! near a signal context.
//!
//! Because nothing here blocks the signal on any thread, `sigaction`'s
//! handler is process-wide regardless of which thread installs it — no
//! "install before any other thread starts" ordering to get right — and it
//! overrides an inherited `SIG_IGN` (a backgrounded job's disposition) the
//! same as it overrides the default.
//!
//! From there it does one thing: signal a run loop source. Everything about
//! how this daemon stops lives on the other side of that, in
//! [`crate::ecs::exit_waker`] — the exit arrives as an ordinary run loop
//! callout, on the main thread, and leaves by the same path a `--exit` request
//! leaves by. Nothing here knows what teardown involves, and none of it runs
//! on this thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use tokio::signal::unix::{SignalKind, signal};

/// Nothing has asked yet. Not a valid signal number, so it cannot be confused
/// for one.
const NOT_ASKED: i32 = 0;

/// A request to stop, from the system rather than from a config.
///
/// Cheap to clone and safe to read from anywhere.
#[derive(Clone, Debug)]
pub struct Stop {
    asked: Arc<AtomicI32>,
}

impl Stop {
    /// Starts listening for `SIGINT` and `SIGTERM`.
    ///
    /// `SIGQUIT` is deliberately left alone: it is the one a person sends when
    /// they want a core dump of a wedged process, and turning it into a tidy
    /// exit would take that away.
    ///
    /// # Errors
    ///
    /// Whatever registering either signal handler returned; neither is being
    /// waited for in that case.
    pub fn listen(wake: crate::runloop::Waker) -> std::io::Result<Self> {
        // `signal` needs a runtime in scope to register with; the waiting
        // itself happens in the tasks below, on the runtime proper.
        let _entered = crate::pool::handle().enter();
        let (interrupt_kind, terminate_kind) = (SignalKind::interrupt(), SignalKind::terminate());
        let interrupt = signal(interrupt_kind)?;
        let terminate = signal(terminate_kind)?;

        let asked = Arc::new(AtomicI32::new(NOT_ASKED));
        watch(interrupt, interrupt_kind, &asked, wake.clone());
        watch(terminate, terminate_kind, &asked, wake);

        Ok(Self { asked })
    }

    /// Which signal asked this daemon to stop, if one has.
    ///
    /// `Relaxed` is enough: the number is the whole message and publishes no
    /// other state.
    #[must_use]
    pub fn asked(&self) -> Option<i32> {
        match self.asked.load(Ordering::Relaxed) {
            NOT_ASKED => None,
            raw => Some(raw),
        }
    }
}

/// Waits for one signal and records it, once.
///
/// A second signal, from this or the other kind, has nothing left to say: the
/// flag is already set and the main thread is already on its way out. The
/// `compare_exchange` is what makes that true even with both signals racing
/// -- only the one that actually wins gets to wake anything.
fn watch(
    mut source: tokio::signal::unix::Signal,
    kind: SignalKind,
    asked: &Arc<AtomicI32>,
    wake: crate::runloop::Waker,
) {
    let asked = Arc::clone(asked);
    crate::pool::spawn(async move {
        source.recv().await;
        let raw = kind.as_raw_value();
        if asked
            .compare_exchange(NOT_ASKED, raw, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            tracing::info!(signal = raw, "asked to stop");
            // Half of a stop is being told; the other half is something
            // running that acts on it. On an idle bar no pass is coming on
            // its own.
            wake.wake();
        }
    });
}

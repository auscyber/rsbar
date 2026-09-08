//! Exclusive use of the process's one window server connection.
//!
//! # Why a lock and not a thread
//!
//! Every call here used to demand the main thread, on the reading that the
//! window server answers only for it. It does not — it answers any thread.
//! What a connection cannot take is two overlapping *sequences*:
//! `SLSDisableUpdate` and `SLSReenableUpdate` bracket a batch of window
//! changes, and a second thread re-enabling compositing halfway through one
//! ends it early. That is a mutual-exclusion problem, and modelling it as
//! thread-affinity pinned work to the thread that composites for no reason.
//!
//! Half of it was already understood: `window.rs` held a `SHARED_CALLS` mutex
//! across the capture and screen-rectangle round trips, because "nothing
//! documents those as safe to make concurrently on one connection". But it
//! covered only the calls made off the main thread. The main thread's own
//! window mutations took nothing, so a capture on a worker could and did
//! overlap them. One lock, covering everything, is what closes that.
//!
//! # Why it is asynchronous
//!
//! Because the thing most likely to be holding it is a window capture, which
//! is one round trip of about twenty milliseconds. A blocking take would stop
//! the thread that composites dead for that long. An `await` does not: the
//! frame's window work waits, the run loop carries on, and nothing is parked
//! in the kernel. That is also why no `blocking_lock` appears here — the main
//! thread has a `LocalSet` of its own precisely so its work can be a future.

use crate::ffi;

/// The connection, established once.
///
/// `SLSMainConnectionID` is asked for on the main thread — the one call here
/// that genuinely wants it, since it is what hands back the process's
/// connection in the first place. What comes back is a plain number that any
/// thread may carry.
static CONNECTION: std::sync::OnceLock<ffi::ConnectionId> = std::sync::OnceLock::new();

/// Whose turn it is.
static ALONE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Establishes the connection if nothing has yet, and answers it.
pub fn establish(proof: impl crate::MainThreadProof) -> ffi::ConnectionId {
    *CONNECTION.get_or_init(|| {
        // SAFETY: takes no arguments beyond the proof, which is the thread it
        // wanted.
        unsafe { ffi::SLSMainConnectionID(&proof) }
    })
}

/// The connection, for a caller that already knows it was established.
///
/// # Panics
///
/// Panics if nothing has asked for it yet, which cannot happen once the bar
/// has a window: [`crate::Window::new`] is the first thing to ask.
pub(crate) fn established() -> ffi::ConnectionId {
    *CONNECTION
        .get()
        .expect("the window server connection is made before anything uses it")
}

/// Takes the connection until the returned value is dropped.
///
/// Held for the length of a frame's window work rather than one call: that is
/// the granularity the batch needs, and taking it per call would make
/// `batched` — which holds it across everything inside — deadlock against
/// itself.
pub async fn acquire() -> Connected {
    Connected {
        id: established(),
        _alone: ALONE.lock().await,
    }
}

/// Proof that the connection is this caller's for the moment.
///
/// `'static` because the lock behind it is: a frame can put one in the world
/// for the length of a pass and take it out again, which a borrowed guard
/// could not do.
pub struct Connected {
    id: ffi::ConnectionId,
    _alone: tokio::sync::MutexGuard<'static, ()>,
}

impl Connected {
    /// The connection this is exclusive use of.
    #[must_use]
    pub(crate) const fn id(&self) -> ffi::ConnectionId {
        self.id
    }
}

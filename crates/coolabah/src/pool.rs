//! The one runtime everything off the main thread runs on.
//!
//! The crate root states the rule: the main thread draws, and nothing on it may
//! wait. This is the other side of that sentence. Every piece of this daemon
//! that computes, waits, or watches shares the runtime here, so a new piece of
//! it does not arrive with a thread pool of its own and a hand-rolled way of
//! getting its answer back.
//!
//! Bevy's schedule is single-threaded on the run loop thread and has to be:
//! `NonSend`, `CFRetained` and `MainThreadMarker` are what the world holds. So
//! everything else shares the runtime here, and [`Handoff`] is the seam — a
//! worker leaves a result and wakes the run loop, which runs the schedule.
//!
//! # Which of the two to use
//!
//! * [`spawn`] for work that **awaits** — a subprocess, a socket, a channel, a
//!   sleep. It shares [`width`] threads with every other such task, so there
//!   may be thousands of these.
//! * [`blocking`] for work that **parks** — a cross-process
//!   `AXUIElementCopyAttributeValue`, a window server capture. These get a
//!   thread each for the duration, from a pool that is empty when nothing is
//!   blocked, so a caller that wants a bound on how many run at once must say
//!   so itself.
//!
//! The distinction is not "slow versus fast", it is **awaiting versus parking**.
//! Parking work on [`spawn`] holds a core that had other tasks to run.
//!
//! # What does not belong here at all
//!
//! Anything main-thread-only. It will not compile: a `skylight::Window`, a
//! `MainThreadMarker` and a `CFRetained` are all `!Send`, so the bounds below
//! reject them. Main-thread work goes to [`crate::runloop::spawn`], which polls
//! on the thread that draws and hands the task its proof.
//!
//! # A run loop for the platform's own sources
//!
//! A `CFRunLoopSource` only fires while scheduled on a `CFRunLoop` some thread
//! is parked in, so hosting them costs a thread that no amount of async
//! removes. It is a runtime resource like the workers, which is why
//! [`run_loop`] is here rather than in the source registry.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

/// One worker per core, less the one that draws.
///
/// The main thread is not counted: it is busy compositing, and a runtime that
/// oversubscribed it would take time from the frame it is trying to make.
fn threads() -> usize {
    std::thread::available_parallelism()
        .map_or(4, std::num::NonZero::get)
        .saturating_sub(1)
        .max(1)
}

/// The runtime, built on first use and never taken down.
///
/// Never dropped, and that is deliberate twice over. A daemon that has spawned
/// one task will spawn more, and the workers cost nothing while the queue is
/// empty — each is parked in the scheduler's own wait rather than spinning on
/// it. And dropping a tokio runtime *joins its blocking threads*, which is a
/// wait on the thread doing the dropping; a daemon told to quit should be gone,
/// not joining a cross-process Accessibility call that will time out in its own
/// time. A leaked runtime exits with the process.
static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads())
        .thread_name("coolabah-worker")
        .enable_all()
        .build()
        .expect("a runtime; the process is out of resources if not")
});

/// The run loop that hosts the platform's own sources.
///
/// One thread, parked in `CFRunLoopRun`, started the first time a source asks
/// and never taken down. It costs nothing while nothing fires, and a callback
/// on it should return almost immediately — see the module note.
///
/// Off the main thread deliberately. A `CFRunLoopSource` scheduled on the main
/// loop competes with compositing for the thread that draws, and CoreFoundation
/// gives a callback no way to say "not now".
#[must_use]
pub fn run_loop() -> objc2_core_foundation::CFRetained<objc2_core_foundation::CFRunLoop> {
    sources().run_loop()
}

/// The thread that hosts the platform's own sources, for work that has to
/// happen *on* it.
///
/// [`run_loop`] is enough to schedule a `CFRunLoopSource`. This is for the rest:
/// [`Pumped::run_on`](crate::runloop::Loop::run_on) reaches a thread parked in
/// `CFRunLoopRun`, and [`Pumped::spawn`](crate::runloop::Loop::spawn) builds a
/// `!Send` future there and lets that loop's own waker poll it. A source whose
/// state cannot leave the thread its callbacks arrive on — an `AXObserver`, a
/// `CFRetained` anything — keeps it there instead of asking to be believed.
#[must_use]
pub fn sources() -> &'static crate::runloop::Loop {
    /// Started on first use, so a daemon whose config registers no
    /// `CoreFoundation` source never starts the thread at all.
    static SOURCES: LazyLock<crate::runloop::Loop> =
        LazyLock::new(|| crate::runloop::Loop::start("coolabah-sources"));
    &SOURCES
}

/// The runtime handle, for the places that need to hand one to something else.
pub fn handle() -> &'static tokio::runtime::Handle {
    RUNTIME.handle()
}

/// Runs `future` off the main thread.
///
/// For work that **awaits**. The returned handle detaches on drop — the task
/// keeps running — so a caller that wants to cancel must keep it and call
/// [`abort`](tokio::task::JoinHandle::abort).
pub fn spawn<T: Send + 'static>(
    future: impl Future<Output = T> + Send + 'static,
) -> tokio::task::JoinHandle<T> {
    RUNTIME.spawn(future)
}

/// Runs `work` on a thread that is allowed to park in it.
///
/// For the calls with no async form: `AXUIElementCopyAttributeValue` is a
/// synchronous cross-process Mach RPC and the window server's capture is
/// another, and no amount of runtime gets either of them to yield. What this
/// does get is a thread only for as long as one is actually blocked, out of a
/// pool shared with every other such call and reaped when it goes idle —
/// instead of a fixed set held for the life of the process.
///
/// The pool is not bounded in any way a caller should rely on. Work that could
/// fan out to a hundred applications takes a
/// [`Semaphore`](tokio::sync::Semaphore) and says how many at once.
pub fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> tokio::task::JoinHandle<T> {
    RUNTIME.spawn_blocking(work)
}

/// A running task, stopped when this handle is dropped.
///
/// The difference from a bare [`JoinHandle`](tokio::task::JoinHandle), which
/// detaches: this is for a future something *owns*. An item owns the futures
/// watching on its behalf, and a live [`Source`](crate::sources::Source) owns
/// the one future that both consumes its callbacks and holds whatever platform
/// registration feeds them — so despawning the item, or stopping the source,
/// must stop them. Holding a `Task` is how that is said, and there is nothing
/// to remember to call.
///
/// `abort` is a request, not a synchronous stop: the task ends at its next
/// await point. A source whose platform teardown must run synchronously, on
/// the thread that registered — Carbon's `RemoveEventHandler`, `AppKit`'s
/// `removeObserver` — keeps that thread-bound registration inside the future
/// itself (see [`crate::runloop::owned`]), so aborting still tears it down on
/// the right thread; nothing here has to know that.
pub struct Task(Handle);

enum Handle {
    /// The ordinary case: the task exists, so its handle does.
    Now(tokio::task::AbortHandle),
    /// Spawned on another thread, so the handle arrives after this does.
    ///
    /// [`crate::runloop::Loop::spawn`] hands work to a thread parked in
    /// `CFRunLoopRun`; the task is not created until that loop gets a turn, but
    /// the caller needs something to hold *now* — a `Source` returns one from a
    /// synchronous `register`. So the slot is filled in when the work lands,
    /// and a drop before then leaves a cancellation for it to find.
    Later(std::sync::Arc<Deferred>),
}

/// One [`Deferred`]'s state, all of it behind the one lock [`Deferred`] holds
/// -- see that type's own doc for why a separate flag beside the handle used
/// to be wrong.
#[derive(Default)]
enum Slot {
    #[default]
    Waiting,
    Arrived(tokio::task::AbortHandle),
    Dropped,
}

/// A single lock around both halves of "has the task arrived, and has the
/// owner given up on it" -- deliberately one `Mutex` and not a handle plus a
/// separate flag.
///
/// Two fields checked and set independently used to be exactly that: a
/// `dropped: AtomicBool` beside `handle: Mutex<Option<AbortHandle>>`, each
/// read-then-write its own step. [`Deferred::arrived`] would load `dropped`
/// as false, [`Task::drop`] would store it true and find no handle to abort,
/// and only then would `arrived` store the handle -- ordered so neither side
/// ever saw the other's write, and a task nothing owned any more kept
/// running forever, on another thread, holding whatever platform teardown it
/// was built to release. Both steps now happen under the same lock, so there
/// is no window between them for the other side to land in.
#[derive(Default)]
pub(crate) struct Deferred {
    slot: Mutex<Slot>,
}

impl Deferred {
    /// Called on the far thread once the task exists.
    pub(crate) fn arrived(&self, handle: tokio::task::AbortHandle) {
        let mut slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        match *slot {
            Slot::Dropped => {
                // Given up on before it started.
                drop(slot);
                handle.abort();
            }
            Slot::Waiting | Slot::Arrived(_) => *slot = Slot::Arrived(handle),
        }
    }
}

impl Task {
    /// Takes ownership of an already-spawned task.
    #[must_use]
    pub fn owns<T>(spawned: &tokio::task::JoinHandle<T>) -> Self {
        Self(Handle::Now(spawned.abort_handle()))
    }

    /// A handle for a task that does not exist yet.
    pub(crate) fn deferred() -> (Self, std::sync::Arc<Deferred>) {
        let shared = std::sync::Arc::<Deferred>::default();
        (Self(Handle::Later(std::sync::Arc::clone(&shared))), shared)
    }

    /// Whether the task has finished on its own. A task that has not started
    /// answers `false`.
    #[must_use]
    pub fn finished(&self) -> bool {
        match &self.0 {
            Handle::Now(handle) => handle.is_finished(),
            Handle::Later(shared) => {
                match &*shared.slot.lock().unwrap_or_else(PoisonError::into_inner) {
                    Slot::Arrived(handle) => handle.is_finished(),
                    Slot::Waiting | Slot::Dropped => false,
                }
            }
        }
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        match &self.0 {
            Handle::Now(handle) => handle.abort(),
            Handle::Later(shared) => {
                // Read and written under the one lock, so there is no window
                // for `arrived` to land in between "nothing here yet" and
                // "too late to matter" -- see `Deferred`'s own doc.
                let mut slot = shared.slot.lock().unwrap_or_else(PoisonError::into_inner);
                if let Slot::Arrived(handle) = std::mem::replace(&mut *slot, Slot::Dropped) {
                    handle.abort();
                }
            }
        }
    }
}

impl std::fmt::Debug for Task {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Task")
            .field("finished", &self.finished())
            .finish()
    }
}

/// Runs `future` off the main thread, for as long as the returned [`Task`]
/// lives.
///
/// [`spawn`] where the caller does not care when the work stops; this where it
/// does.
pub fn owned<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> Task {
    Task::owns(&spawn(future))
}

/// How many workers the runtime has, for a diagnostic that wants to say.
#[must_use]
pub fn width() -> usize {
    threads()
}

/// Where a worker leaves results and the main thread collects them.
///
/// **The seam between the two schedulers**, written once. Three parts, and each
/// is load-bearing:
///
/// 1. a mutex holding what has finished, so a result outlives the task that
///    produced it;
/// 2. an [`AtomicBool`] beside it saying whether there is anything, because a
///    run condition asks every pass and **must not take a lock a worker
///    holds** — that is a stall on the thread that draws, waiting on a thread
///    that is hashing an image;
/// 3. the run loop [`Waker`](crate::runloop::Waker), because on an idle bar no
///    pass is coming on its own and a finished result would sit there.
pub struct Handoff<T> {
    held: Arc<Mutex<Vec<T>>>,
    landed: Arc<AtomicBool>,
    wake: crate::runloop::Waker,
}

impl<T> Handoff<T> {
    /// A new handoff, waking `wake` when something lands.
    #[must_use]
    pub fn new(wake: crate::runloop::Waker) -> Self {
        Self {
            held: Arc::new(Mutex::new(Vec::new())),
            landed: Arc::new(AtomicBool::new(false)),
            wake,
        }
    }

    /// Whether anything has landed, without taking the lock.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.landed.load(Ordering::Acquire)
    }

    /// Everything that has landed since the last ask.
    ///
    /// The flag is cleared *under the lock*, so a result pushed between the
    /// take and the clear is not lost — it would otherwise sit there with the
    /// flag saying there was nothing.
    pub fn take(&self) -> Vec<T> {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let taken = std::mem::take(&mut *held);
        self.landed.store(false, Ordering::Release);
        drop(held);
        taken
    }

    /// A handle a task can send results through.
    #[must_use]
    pub fn sender(&self) -> Sender<T> {
        Sender {
            held: Arc::clone(&self.held),
            landed: Arc::clone(&self.landed),
            wake: self.wake.clone(),
        }
    }
}

/// The worker's half of a [`Handoff`].
///
/// `Send`, and cheap to clone: several tasks may hold one, and each outlives
/// the [`Handoff`] itself if it has to — a result landing after the collector
/// is gone is written into an `Arc` nothing else reads, rather than into freed
/// memory.
pub struct Sender<T> {
    held: Arc<Mutex<Vec<T>>>,
    landed: Arc<AtomicBool>,
    wake: crate::runloop::Waker,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            held: Arc::clone(&self.held),
            landed: Arc::clone(&self.landed),
            wake: self.wake.clone(),
        }
    }
}

impl<T> Sender<T> {
    /// Leaves `value` for the main thread and brings a pass about.
    ///
    /// The flag is set before the wake, so the pass this causes cannot arrive
    /// to find the flag still false.
    pub fn send(&self, value: T) {
        self.send_all([value]);
    }

    /// The same, for several at once: one lock and one wake rather than each.
    pub fn send_all(&self, values: impl IntoIterator<Item = T>) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let before = held.len();
        held.extend(values);
        if held.len() == before {
            return;
        }
        drop(held);
        self.landed.store(true, Ordering::Release);
        self.wake.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::{Handoff, blocking, spawn, width};

    fn handoff() -> Handoff<u32> {
        Handoff::new(crate::runloop::Waker::install(
            crate::runloop::main_thread(),
            || {},
        ))
    }

    /// Waits for `ready` without blocking a runtime worker: this is the test
    /// thread, which is not one.
    fn settle(mut ready: impl FnMut() -> bool) {
        for _ in 0..500 {
            if ready() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn the_runtime_leaves_a_thread_for_the_one_that_draws() {
        let cores = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        assert!(
            width() < cores || cores == 1,
            "one core is left for drawing"
        );
        assert!(width() >= 1, "there is always somewhere to put work");
    }

    #[test]
    fn nothing_is_ready_until_something_lands() {
        let handoff = handoff();
        assert!(!handoff.ready());
        assert!(handoff.take().is_empty());
    }

    #[test]
    fn a_result_sent_off_thread_is_collected_on_this_one() {
        let handoff = handoff();
        let sender = handoff.sender();
        spawn(async move { sender.send(7) });

        settle(|| handoff.ready());
        assert!(handoff.ready(), "the task never landed");
        assert_eq!(handoff.take(), vec![7]);
        assert!(!handoff.ready(), "taking clears the flag");
    }

    #[test]
    fn several_senders_land_in_one_place() {
        let handoff = handoff();
        for value in 0..4u32 {
            let sender = handoff.sender();
            spawn(async move { sender.send(value) });
        }

        settle(|| handoff.held.lock().expect("no other holder").len() == 4);
        let mut landed = handoff.take();
        landed.sort_unstable();
        assert_eq!(landed, vec![0, 1, 2, 3]);
    }

    /// The distinction the module is built on: parking work must not sit on a
    /// worker, and there must be somewhere for it that a full worker set does
    /// not starve.
    #[test]
    fn work_that_parks_runs_even_with_every_worker_busy() {
        let held = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        for _ in 0..width() {
            let held = std::sync::Arc::clone(&held);
            spawn(async move {
                while held.load(std::sync::atomic::Ordering::Acquire) {
                    tokio::task::yield_now().await;
                }
            });
        }

        let handoff = handoff();
        let sender = handoff.sender();
        blocking(move || sender.send(11));

        settle(|| handoff.ready());
        held.store(false, std::sync::atomic::Ordering::Release);
        assert!(handoff.ready(), "a blocking task waited on the workers");
        assert_eq!(handoff.take(), vec![11]);
    }
}

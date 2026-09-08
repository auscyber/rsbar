//! The run loop, as this daemon's reactor.
//!
//! Drawing, and every window server call behind it, belongs to the thread
//! running the `CFRunLoop`. Work that arrives anywhere else — an IPC request,
//! a finished script — has to be handed over rather than acted on.
//!
//! CoreFoundation already has the three primitives a reactor needs, and this
//! module is a safe owner for each of them:
//!
//! * [`Waker`] — a manual source something signals from another thread, and
//!   the handover for work that has no port or file behind it.
//! * [`wake_at`] — the deadline reactor, for a wake that is due at a *time*.
//!   Each deadline is its own `tokio::time::sleep_until`, and with nothing due
//!   there is nothing sleeping. There is no way here to ask for a *repeating*
//!   wake, because nothing in this daemon is allowed to want one.
//! * [`MachSource`] — a Mach port the run loop receives on itself, which is
//!   how IPC arrives without a thread parked on `mach_msg` anywhere.
//! * [`spawn`] — the daemon's async runtime: a `tokio::task::LocalSet` whose
//!   one driver is a run loop source, so a task is polled on the thread that
//!   owns the windows and the process sleeps whenever no task can move.
//!
//! The first and last wrap a `CFRunLoop*Context`: a `void*`, a retain and a
//! release for it, and a callback handed it back. [`Handler`] is that shape,
//! written once — so the `unsafe` lives here rather than at every call site
//! that wants a wakeup.

use objc2::MainThreadMarker;
use objc2_core_foundation::{
    CFIndex, CFMachPort, CFMachPortContext, CFRetained, CFRunLoop, CFRunLoopMode, CFRunLoopSource,
    CFRunLoopSourceContext, kCFRunLoopCommonModes,
};
use std::ffi::c_void;

/// A handle a worker thread can use to wake the main run loop.
///
/// Signals coalesce: several wakes before the run loop gets a turn produce one
/// call of the handler, so the handler must drain whatever queued up rather
/// than assume one item per call.
#[derive(Clone)]
pub struct Waker {
    source: CFRetained<CFRunLoopSource>,
    run_loop: CFRetained<CFRunLoop>,
}

// SAFETY: `CFRunLoopSource::signal` and `CFRunLoop::wake_up` are the two calls
// CoreFoundation documents as safe from any thread, and they are all this
// exposes. The handler itself never leaves the thread that installed it.
unsafe impl Send for Waker {}
unsafe impl Sync for Waker {}

/// Source ordering relative to other sources; nothing here competes.
const ORDER: CFIndex = 0;

/// The run loop modes everything here registers in.
///
/// Common modes, so a handler still runs while a menu is tracking or a window
/// is being resized. Read once, here, rather than at each of the call sites
/// that need it — [`scheduled`]'s included.
pub(crate) fn common_modes() -> Option<&'static CFRunLoopMode> {
    /// Read once. It is a constant CoreFoundation exports, so re-reading the
    /// extern static at each of the dozen-odd registration sites bought
    /// nothing but a dozen-odd `unsafe` blocks.
    ///
    /// `LazyLock` rather than a `const`: an extern static cannot be read in
    /// constant context. `&'static CFRunLoopMode` is a `CFStringRef` to
    /// immutable, immortal storage — safe to share, which is what makes this
    /// sound in a `static`.
    struct Modes(Option<&'static CFRunLoopMode>);

    // SAFETY: the referent is an immutable CoreFoundation constant that lives
    // for the process; nothing here can mutate it, so sharing it across
    // threads and initialising it on any of them are both fine.
    unsafe impl Sync for Modes {}
    // SAFETY: as above.
    unsafe impl Send for Modes {}

    static COMMON: std::sync::LazyLock<Modes> = std::sync::LazyLock::new(|| {
        // SAFETY: a `'static` constant CoreFoundation exports; unsafe to read
        // only because it is an extern static.
        Modes(unsafe { kCFRunLoopCommonModes })
    });

    COMMON.0
}

/// What a run loop callout may be written as.
///
/// The run loop only ever calls back on the thread its registration was made
/// on, and [`Waker::install`] will not make one without a
/// [`MainThreadMarker`]. So by the time a handler runs, the proof already
/// exists — the only question is whether the handler wants to see it.
///
/// This is how it is offered without being passed: two impls, one for a
/// handler that takes nothing and one for a handler that takes the marker.
/// `Marker` exists only to keep them disjoint, which is the same trick
/// `bevy_ecs` uses to let a system declare its arguments instead of
/// receiving a context object.
///
/// ```ignore
/// Waker::install(mtm, || pass());                       // does not care
/// Waker::install(mtm, |mtm: MainThreadMarker| draw(mtm)); // asked for it
/// ```
///
/// A closure that wants the marker has to name its type, as a bevy system's
/// parameters do: with two impls in scope there is nothing else for inference
/// to go on.
pub trait Callback<Marker>: 'static {
    /// Runs the handler, on the thread `mtm` is proof of.
    fn call(&self, mtm: MainThreadMarker);
}

/// [`Callback`] disjointness tag: a handler that takes nothing.
#[doc(hidden)]
pub struct TakesNothing;

/// [`Callback`] disjointness tag: a handler that takes the marker.
#[doc(hidden)]
pub struct TakesMain;

impl<F: Fn() + 'static> Callback<TakesNothing> for F {
    fn call(&self, _mtm: MainThreadMarker) {
        self();
    }
}

impl<F: Fn(MainThreadMarker) + 'static> Callback<TakesMain> for F {
    fn call(&self, mtm: MainThreadMarker) {
        self(mtm);
    }
}

/// The same arrangement as [`Callback`], for a callout that carries a message.
///
/// Implemented for `Fn(&[u8])` and `Fn(&[u8], MainThreadMarker)`, so an IPC
/// handler asks for the proof by writing a second parameter or says nothing
/// and is not given one.
pub trait MessageCallback<Marker>: 'static {
    /// Runs the handler on the thread `mtm` is proof of.
    fn call(&self, message: &[u8], mtm: MainThreadMarker);
}

impl<F: Fn(&[u8]) + 'static> MessageCallback<TakesNothing> for F {
    fn call(&self, message: &[u8], _mtm: MainThreadMarker) {
        self(message);
    }
}

impl<F: Fn(&[u8], MainThreadMarker) + 'static> MessageCallback<TakesMain> for F {
    fn call(&self, message: &[u8], mtm: MainThreadMarker) {
        self(message, mtm);
    }
}

/// A marker for a test, which cannot honestly have one.
///
/// **Test-only, and the only place in this crate that assumes rather than is
/// told.** In the daemon the marker comes from `main` and is passed by value,
/// so nothing anywhere checks a thread and nothing can panic for being on the
/// wrong one. libtest, though, runs each test on a thread it spawned, so a
/// test that wants to touch a run loop has no honest marker to be had — and
/// the alternative is that no test may touch one at all.
///
/// What a test does with it is install a source on its *own* thread's run
/// loop, which CoreFoundation permits from anywhere.
#[cfg(test)]
#[must_use]
pub fn main_thread() -> MainThreadMarker {
    // SAFETY: not sound in general, which is why it is behind `cfg(test)`.
    MainThreadMarker::new().unwrap_or_else(|| unsafe { MainThreadMarker::new_unchecked() })
}

/// How many messages every [`MachSource`] callout has taken.
///
/// One counter for the process, in the trampoline, so it covers every source
/// and needs nothing from any handler. What it answers is whether the port is
/// delivering at all — a client that reports success while the bar does not
/// change is either not being heard or not being understood, and these two
/// look identical from the outside.
static DELIVERED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many IPC messages this process has been handed.
#[must_use]
pub fn messages_delivered() -> u64 {
    DELIVERED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The handler CoreFoundation is pointed at, and the reference count it keeps
/// of it.
///
/// A namespace rather than a value: nothing ever holds one of these. What it
/// carries is `F`, so the four trampolines below — leak, borrow, retain,
/// release — agree about the type behind the `void*` without any of them being
/// told.
///
/// A run loop source and a Mach port take the same shape of context — a
/// `void*`, a retain and a release for it, and a callback handed it back — and
/// each used to carry its own character-for-character copy of the three
/// trampolines that shape needs. They live here once instead. The count is
/// CoreFoundation's to move; the box is reclaimed when it returns to zero,
/// which is what stops a detached handler leaking.
/// The handler CoreFoundation is pointed at.
///
/// `skylight::callback::Retained`, not a scheme of its own: CoreFoundation's
/// context structs carry a `retain`/`release` pair and call them, so the right
/// model is to hand over a strong reference and let CoreFoundation manage one.
/// That module has both models and says which to pick from the shape of the C
/// struct being filled in.
type Handler<F> = skylight::callback::Retained<F>;

/// Builds a manual run loop source whose context is `handler`.
///
/// **The only place a `CFRunLoopSourceContext` is written.** Three call sites
/// used to fill one in by hand — two wakers and a keep-alive — and each carried
/// its own `unsafe` block, its own retain/release pair and its own chance to
/// mismatch the two. The context is typed by construction here: the `info`
/// pointer is a [`Handler<F>`] and the retain, release and callout are the ones
/// that agree with it, so a caller supplies only the handler and the trampoline
/// that reads it back.
fn manual_source<F: 'static>(
    handler: F,
    perform: unsafe extern "C-unwind" fn(*mut c_void),
) -> CFRetained<CFRunLoopSource> {
    let info = Handler::hand_over(handler);
    let mut context = CFRunLoopSourceContext {
        version: 0,
        info,
        retain: Some(Handler::<F>::retain),
        release: Some(Handler::<F>::release),
        copyDescription: None,
        equal: None,
        hash: None,
        schedule: None,
        cancel: None,
        perform: Some(perform),
    };

    // SAFETY: `context` outlives the call and CoreFoundation copies it; `info`
    // is the box just leaked, and the retain/release pair is the one built for
    // that box's type.
    let Some(source) = (unsafe { CFRunLoopSource::new(None, ORDER, &raw mut context) }) else {
        // CoreFoundation never took a reference, so nothing will ever release
        // one -- give ours up here rather than leak on the way to the panic.
        // SAFETY: `info` is the allocation `leak` made and ours is the only
        // reference to it.
        unsafe { Handler::<F>::give_up(info) };
        panic!("failed to create a run loop source");
    };

    // Creation retained it, so the source now owns the handler and will
    // release it when it is destroyed. Ours would otherwise be the reference
    // that never goes.
    // SAFETY: `info` is the same allocation, and this drops the reference
    // `leak` created rather than the one CoreFoundation just took.
    unsafe { Handler::<F>::give_up(info) };
    source
}

impl Waker {
    /// Installs a manual source on the current thread's run loop.
    ///
    /// The handler runs in all common modes, so it still fires while a menu is
    /// tracking or a window is being resized.
    ///
    /// Taking the marker is what makes the callout's own thread known: see
    /// [`Callback`], which lets a handler ask for it back by writing a
    /// parameter, or say nothing and not be given one.
    ///
    /// # Panics
    ///
    /// Panics if the current thread has no run loop, or if CoreFoundation
    /// declines to create the source — neither is recoverable.
    pub fn install<M, F: Callback<M>>(proof: impl skylight::MainThreadProof, handler: F) -> Self {
        unsafe extern "C-unwind" fn perform<M, F: Callback<M>>(info: *mut c_void) {
            // SAFETY: `info` is the pointer installed below, still retained.
            let handler = unsafe { Handler::<F>::borrow(info) };
            // SAFETY: `install` took a `MainThreadMarker`, and CoreFoundation
            // calls a source out only on the run loop it was added to -- the
            // one belonging to the thread that proved it. So this callout is
            // on that thread, and the marker is not being invented, only
            // carried across a C boundary that cannot hold it.
            handler.call(unsafe { MainThreadMarker::new_unchecked() });
        }

        let source = manual_source(handler, perform::<M, F>);
        // From the proof, not from `CFRunLoop::current()`: the proof knows
        // which loop it belongs to, and a source added to a loop nobody pumps
        // never fires and reports nothing.
        let run_loop = skylight::MainThreadProof::run_loop(&proof);
        run_loop.add_source(Some(&source), common_modes());

        Self { source, run_loop }
    }

    /// The same, on a run loop that is not this thread's.
    ///
    /// For a [`Loop`] turned by a thread of its own. Two differences
    /// follow from that, and both are in the signature: the handler must be
    /// `Send`, because it will run on that thread; and it is handed **no
    /// main-thread marker**, because it is not on the main thread and saying
    /// otherwise would be a lie the rest of this crate believes.
    ///
    /// # Panics
    ///
    /// Panics if CoreFoundation declines the source, which is not recoverable.
    pub fn install_on<F: Fn(&CFRunLoop) + Send + 'static>(
        run_loop: &CFRunLoop,
        handler: F,
    ) -> Self {
        unsafe extern "C-unwind" fn perform<F: Fn(&CFRunLoop) + Send + 'static>(info: *mut c_void) {
            // CoreFoundation calls a source out on the thread turning the loop
            // it was added to, so `current` is that loop. Learned here, where
            // it is a local fact, rather than captured -- a
            // `CFRetained<CFRunLoop>` is `!Send` and the handler is not.
            let Some(here) = CFRunLoop::current() else {
                return;
            };
            // SAFETY: `info` is the pointer installed below, still retained.
            (unsafe { Handler::<F>::borrow(info) })(&here);
        }

        let source = manual_source(handler, perform::<F>);
        run_loop.add_source(Some(&source), common_modes());

        Self {
            source,
            run_loop: CFRetained::from(run_loop),
        }
    }

    /// Schedules the handler and wakes the run loop so it runs promptly.
    pub fn wake(&self) {
        self.source.signal();
        self.run_loop.wake_up();
    }

    /// The same wake-up, as a [`std::task::Waker`].
    ///
    /// This is the bridge between the two meanings of the word. A
    /// [`std::task::Waker`] says "poll this task again" and leaves *how* to an
    /// executor; a [`Waker`] is that how, for the one executor this daemon has
    /// — the run loop on the main thread. Handing a future the result lets it
    /// be woken from any thread and polled where the window server calls are
    /// legal, without the future knowing anything about `CoreFoundation`.
    ///
    /// Cheap to hand out: the returned waker holds a reference to the same run
    /// loop source, so cloning one is a reference count rather than another
    /// registration.
    #[must_use]
    pub fn to_std(&self) -> std::task::Waker {
        let owned = std::sync::Arc::new(self.clone());
        // SAFETY: `raw` is built from an `Arc<Self>` and paired with `VTABLE`,
        // whose four functions all treat the pointer as exactly that. `Self`
        // is `Send + Sync`, which `std::task::Waker` requires.
        unsafe { std::task::Waker::from_raw(raw(owned)) }
    }
}

/// A [`RawWaker`] over an `Arc<Waker>`, which is what [`VTABLE`] expects.
fn raw(owned: std::sync::Arc<Waker>) -> std::task::RawWaker {
    std::task::RawWaker::new(std::sync::Arc::into_raw(owned).cast::<()>(), &VTABLE)
}

/// The four operations `std::task::Waker` needs, over an `Arc<Waker>`.
///
/// Each takes the pointer [`raw`] produced. `clone` and `wake_by_ref` leave
/// the caller's reference intact; `wake` and `drop` consume it — the contract
/// `RawWakerVTable` documents, and the reason the two pairs differ.
static VTABLE: std::task::RawWakerVTable =
    std::task::RawWakerVTable::new(clone_waker, wake_waker, wake_waker_by_ref, drop_waker);

unsafe fn clone_waker(data: *const ()) -> std::task::RawWaker {
    // SAFETY: `data` came from `Arc::into_raw`; this adds a reference for the
    // clone without touching the one the caller still holds.
    unsafe { std::sync::Arc::increment_strong_count(data.cast::<Waker>()) };
    std::task::RawWaker::new(data, &VTABLE)
}

unsafe fn wake_waker(data: *const ()) {
    // SAFETY: `data` came from `Arc::into_raw` and this consumes it, per the
    // vtable's contract for `wake`.
    let owned = unsafe { std::sync::Arc::from_raw(data.cast::<Waker>()) };
    owned.wake();
}

unsafe fn wake_waker_by_ref(data: *const ()) {
    // SAFETY: as above, but the reference is the caller's and stays theirs, so
    // the pointer is borrowed rather than reclaimed.
    let owned =
        unsafe { std::mem::ManuallyDrop::new(std::sync::Arc::from_raw(data.cast::<Waker>())) };
    owned.wake();
}

unsafe fn drop_waker(data: *const ()) {
    // SAFETY: `data` came from `Arc::into_raw` and is being given back.
    unsafe { std::sync::Arc::decrement_strong_count(data.cast::<Waker>()) };
}

/// A Mach port the run loop receives on, so nothing else has to.
///
/// The readiness half of the reactor, and the one place CoreFoundation does
/// more than tell you something happened. `CFMachPortCreateWithPort` adds a
/// receive right to the run loop as a native source: the run loop's own
/// `mach_msg` takes the message and hands it straight to the handler, on the
/// thread that owns the windows. So there is no thread parked on the port and
/// none awaiting it either — which is what `SketchyBar` does, and the reason
/// this daemon has no IPC thread and no kqueue reactor behind it.
///
/// **The handler is given the message, not a copy of its contents.**
/// CoreFoundation does not destroy what it delivers: the reply right, any
/// carried right, and the out-of-line payload are all still live, and a
/// handler that does not consume them strands them. `Receiver::decode_message`
/// is what consumes them.
pub struct MachSource {
    /// Both are kept for the same reason: releasing either would take the port
    /// off the run loop, and the handler's box would go with it.
    _port: CFRetained<CFMachPort>,
    source: CFRetained<CFRunLoopSource>,
    run_loop: CFRetained<CFRunLoop>,
}

impl MachSource {
    /// Installs `port` on the current thread's run loop.
    ///
    /// Registered in all common modes, like everything else here, so a request
    /// is still answered while a menu is tracking.
    ///
    /// Returns `None` if CoreFoundation declines the port — which in practice
    /// means something already wrapped it — or if the current thread has no
    /// run loop.
    ///
    /// Takes the marker for the same reason [`Waker::install`] does, and hands
    /// it back the same way: a message is delivered by the run loop it was
    /// registered on, so the handler is on the thread that proved it and can
    /// ask for that proof by writing a second parameter.
    ///
    /// # Safety
    ///
    /// `port` must name a receive right that outlives the returned value, and
    /// nothing else may receive on it. Two reactors on one port do not share
    /// the messages; they race for each one.
    pub unsafe fn install<M, F: MessageCallback<M>>(
        proof: impl skylight::MainThreadProof,
        port: async_mach_ports::mach_port_t,
        handler: F,
    ) -> Option<Self> {
        /// What `CFMachPort` calls when the port has a message.
        ///
        /// The run loop has already done the `mach_msg`, so what arrives here
        /// is the message itself and there is nothing left in the kernel's
        /// queue to receive. It is passed on as bytes and nothing here looks
        /// inside it: the layout, the trailer and the out-of-line memory are
        /// `async-mach-ports`' business, and the handler hands them back to
        /// that crate -- so the wire format never crosses this boundary and
        /// nothing has to be released by hand.
        unsafe extern "C-unwind" fn deliver<M, F: MessageCallback<M>>(
            _port: *mut CFMachPort,
            message: *mut c_void,
            len: CFIndex,
            info: *mut c_void,
        ) {
            let Ok(len) = usize::try_from(len) else {
                return;
            };
            if message.is_null() || len == 0 {
                return;
            }
            // SAFETY: CoreFoundation hands over a message of `len` bytes that
            // stays valid for the call.
            let bytes = unsafe { std::slice::from_raw_parts(message.cast::<u8>(), len) };

            // Counted and said before the handler runs, so a request that fails
            // to decode still shows as delivery: what this answers is "did the
            // port hand us something", which is the question when a client says
            // it sent a command and the bar did not change.
            let seen = DELIVERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            tracing::trace!(bytes = len, delivered = seen, "an IPC message arrived");
            // SAFETY: `info` is the pointer installed below, still retained.
            let handler = unsafe { Handler::<F>::borrow(info) };
            // SAFETY: as `Waker::install`'s own callout: `install` took a
            // marker and CoreFoundation delivers only on the run loop the
            // port was registered on.
            handler.call(bytes, unsafe { MainThreadMarker::new_unchecked() });
        }

        let info = Handler::hand_over(handler);
        let mut context = CFMachPortContext {
            version: 0,
            info,
            retain: Some(Handler::<F>::retain),
            release: Some(Handler::<F>::release),
            copyDescription: None,
        };
        // CoreFoundation sets this when it refuses the port -- something else
        // already wrapped it. `Boolean` is a `u8` whose name
        // `objc2-core-foundation` does not export.
        let mut orphaned: u8 = 0;

        // SAFETY: `context` outlives the call and CoreFoundation copies it;
        // `deliver` matches the callback it is installed as; the caller
        // guarantees `port` is a live receive right.
        let wrapped = unsafe {
            CFMachPort::with_port(
                None,
                port,
                Some(deliver::<M, F>),
                &raw mut context,
                &raw mut orphaned,
            )
        };
        let Some(wrapped) = wrapped else {
            if orphaned != 0 {
                // SAFETY: nothing retained the box, so this is the only owner.
                unsafe { Handler::<F>::give_up(info) };
            }
            return None;
        };

        let source = CFMachPort::new_run_loop_source(None, Some(&wrapped), ORDER)?;
        let run_loop = skylight::MainThreadProof::run_loop(&proof);
        run_loop.add_source(Some(&source), common_modes());

        Some(Self {
            _port: wrapped,
            source,
            run_loop,
        })
    }
}

impl Drop for MachSource {
    /// Takes the port off the run loop, and nothing more.
    ///
    /// Deliberately not `CFMachPortInvalidate`: the receive right belongs to
    /// whoever handed it to [`MachSource::install`], and invalidating is how a
    /// `CFMachPort` would claim it. Removing the source is enough to stop the
    /// deliveries, which is all this owns.
    fn drop(&mut self) {
        self.run_loop
            .remove_source(Some(&self.source), common_modes());
    }
}

thread_local! {
    /// Every deadline currently armed on this thread, so [`armed`] can say.
    ///
    /// Keyed by a serial rather than by the `Instant`, since two deadlines can
    /// land on the same instant. The value is what [`armed`] reports.
    static ARMED: std::cell::RefCell<std::collections::BTreeMap<u64, std::time::Instant>> =
        const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
    static NEXT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A wake registered with the reactor, cancelled when this is dropped.
///
/// Held rather than detached: a caller that stops caring must take its
/// deadline out of the set, or a task keeps running for a wake nobody wants.
/// Dropping this is the only way to do that, and the only thing that has to
/// happen — so a map keyed by entity disarms an item by losing its entry,
/// with no cancellation call for a despawn site to forget.
pub struct Registration {
    id: u64,
    /// Aborts the sleeping task on drop.
    _task: crate::pool::Task,
    /// The reactor is a thread local, so a registration dropped on another
    /// thread would cancel nothing and quietly leave a task running for a
    /// wake nobody wants. A raw pointer makes that a compile error rather
    /// than a convention: this is `!Send`, so anything holding one has to be
    /// `NonSend` too.
    thread_bound: std::marker::PhantomData<*const ()>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        // `try_with`: a registration outlived by nothing but the thread's own
        // teardown would otherwise abort the process on the way out, and a
        // thread that is being destroyed has already forgotten everything
        // this would have told it.
        let _ = ARMED.try_with(|armed| armed.borrow_mut().remove(&self.id));
    }
}

/// Asks to wake `waker` at `deadline`.
///
/// The door for a caller that is not a future — the run loop's own "there is
/// nothing to do until then", which is what replaces a periodic tick. Backed
/// by a `tokio::time::sleep_until` on a task of its own, polled the same way
/// every other main-thread task is: see [`spawn`].
#[must_use]
#[skylight::main_thread]
pub fn wake_at(deadline: std::time::Instant, waker: std::task::Waker) -> Registration {
    let id = NEXT.with(|next| {
        let id = next.get();
        next.set(id + 1);
        id
    });
    ARMED.with(|armed| armed.borrow_mut().insert(id, deadline));

    let task = owned(proof, move |_main| async move {
        tokio::time::sleep_until(deadline.into()).await;
        // Taken out before waking, so a pass this wake causes does not see
        // its own deadline as still armed.
        let _ = ARMED.try_with(|armed| armed.borrow_mut().remove(&id));
        waker.wake();
    });
    Registration {
        id,
        _task: task,
        thread_bound: std::marker::PhantomData,
    }
}

/// The earliest deadline still armed on this thread, or `None` when nothing
/// is.
///
/// Exposed so the invariant can be *asserted* rather than observed: measuring
/// idle CPU says nothing about whether a wake was scheduled, and this says it
/// exactly.
#[must_use]
pub fn armed() -> Option<std::time::Instant> {
    ARMED.with(|armed| armed.borrow().values().min().copied())
}

/// `!Send` tasks, polled by the run loop on the thread that draws.
///
/// [`crate::pool`] is the other half, for `Send` futures. Everything tokio
/// offers for driving a `LocalSet` parks the thread, and this thread is
/// Carbon's — so it is polled by hand from a run loop source whose
/// [`std::task::Waker`] signals that same source. An idle bar signals nothing.
mod tasks {
    use super::Waker;
    use objc2_core_foundation::CFRunLoop;
    use std::cell::RefCell;
    use std::future::Future;
    use std::pin::Pin;

    /// What the run loop drives, once something has asked for it.
    struct Driver {
        /// Boxed and pinned because `poll` needs `Pin<&mut LocalSet>` and this
        /// outlives every callback that polls it.
        local: Pin<Box<tokio::task::LocalSet>>,
        /// The run loop source whose callback is [`drive`], and the
        /// [`std::task::Waker`] over it.
        ///
        /// That waker is the load-bearing half. `LocalSet::poll` registers *the
        /// waker it was polled with*, so it has to be one that signals this
        /// source — a noop waker would register a wake that goes nowhere, and
        /// the run loop would sleep through every task ever spawned.
        source: Waker,
        woken: std::task::Waker,
        /// This thread's place in the runtime, so a local task may use tokio's
        /// timers and await work on the workers.
        _entered: tokio::runtime::EnterGuard<'static>,
    }

    thread_local! {
        /// Leaked, and that is the point: dropping an `EnterGuard` reaches into
        /// tokio's own thread-local context, and a thread-local destructor
        /// cannot rely on another thread-local still being alive. Dropping this
        /// at thread exit aborted the process with "cannot access a Thread
        /// Local Storage value during or after destruction".
        ///
        /// The driver lives as long as the thread does anyway, so what is
        /// stored is a reference and TLS teardown drops nothing.
        static DRIVER: RefCell<Option<&'static mut Driver>> = const { RefCell::new(None) };
    }

    /// Builds this thread's driver against `run_loop`, once.
    ///
    /// Any run loop, not just the main one. A `CFRunLoopSource` becomes a
    /// [`std::task::Waker`], and a `LocalSet` polled by that waker is an
    /// executor for whichever thread is turning that loop — so a source that
    /// needs a run loop of its own gets its futures polled on the same thread
    /// its callbacks arrive on, woken the same way, with nothing crossing.
    fn started_on(run_loop: &CFRunLoop) {
        DRIVER.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_some() {
                return;
            }
            let local = Box::pin(tokio::task::LocalSet::new());
            // Never completes, so the set is never empty and `poll` never
            // answers `Ready`. See the module note.
            local.spawn_local(std::future::pending::<()>());
            let source = Waker::install_on(run_loop, |_here| drive());
            let woken = source.to_std();
            *slot = Some(Box::leak(Box::new(Driver {
                local,
                source,
                woken,
                _entered: crate::pool::handle().enter(),
            })));
        });
    }

    /// Runs a `!Send` future on **this** thread's run loop.
    ///
    /// For a thread that turns a loop of its own. The main thread's callers
    /// want [`spawn`] instead, which additionally hands the future its proof.
    ///
    /// # Panics
    ///
    /// If this thread has no `CoreFoundation` run loop, which cannot happen on
    /// a thread that is running one.
    pub fn spawn_here<F: Future<Output = ()> + 'static>(future: F) -> tokio::task::JoinHandle<()> {
        let run_loop = CFRunLoop::current().expect("this thread turns a run loop");
        started_on(&run_loop);
        let spawned = DRIVER.with(|slot| {
            let slot = slot.borrow();
            let driver = slot.as_ref().expect("built on the line above");
            driver.local.spawn_local(future)
        });
        signal();
        spawned
    }

    /// Polls the local tasks, and asks for another turn if work remains.
    fn drive() {
        let again = DRIVER.with(|slot| {
            // Re-entered: a task pumped the run loop and the driver's source
            // came round again. The outer poll is still in progress and will
            // re-signal for whatever this one would have done.
            let Ok(mut slot) = slot.try_borrow_mut() else {
                return false;
            };
            let Some(driver) = slot.as_mut() else {
                return false;
            };
            let woken = driver.woken.clone();
            let polled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                driver
                    .local
                    .as_mut()
                    .poll(&mut std::task::Context::from_waker(&woken))
            }));
            match polled {
                // Cannot happen: the keep-alive task holds the set open. If it
                // somehow did, polling again would be undefined, so stop.
                Ok(std::task::Poll::Ready(())) => {
                    tracing::error!("the local task set finished; nothing more will be polled");
                    slot.take();
                    false
                }
                Ok(std::task::Poll::Pending) => false,
                Err(_) => {
                    tracing::error!("the task driver panicked; carrying on");
                    true
                }
            }
        });
        if again {
            signal();
        }
    }

    /// Asks the run loop for a turn.
    fn signal() {
        DRIVER.with(|slot| {
            if let Some(driver) = slot.borrow().as_ref() {
                driver.source.wake();
            }
        });
    }

    /// Runs a task on this thread, handing it the proof.
    ///
    /// A closure rather than a future, so the task is built *here* and handed
    /// its [`MainThread`](skylight::MainThread) — `!Send` and unforgeable, held
    /// across every await. That makes the future `!Send` by construction; an
    /// `impl Future` bound would merely *allow* a main-thread task, since there
    /// is no way to require "not `Send`".
    ///
    /// The returned handle detaches on drop. Keep it to await or abort.
    #[skylight::main_thread]
    pub fn spawn<F: Future<Output = ()> + 'static>(
        task: impl FnOnce(skylight::MainThread) -> F,
    ) -> tokio::task::JoinHandle<()> {
        started_on(&skylight::MainThreadProof::run_loop(&proof));
        let spawned = DRIVER.with(|slot| {
            let slot = slot.borrow();
            let driver = slot
                .as_ref()
                .expect("`started` built the driver on the line above");
            driver
                .local
                .spawn_local(task(skylight::MainThread::of(&proof)))
        });
        // `LocalSet` wakes its registered waker on spawn, which is this
        // thread's driver only once something has polled it. Signalling as well
        // costs nothing and covers the first spawn, before anything has.
        signal();
        spawned
    }

    /// Runs a task on this thread for as long as the returned handle lives.
    ///
    /// [`spawn`] where nothing owns the task; this where something does — an
    /// item holding the futures watching on its behalf, so that despawning it
    /// stops them, with no teardown system to write and none to forget.
    ///
    /// ```ignore
    /// // in a system, with `main: NonSend<Main>`
    /// commands.entity(item).insert(Tasks(vec![
    ///     runloop::owned(main.0, |main| async move { .. }),
    /// ]));
    /// ```
    #[skylight::main_thread]
    pub fn owned<F: Future<Output = ()> + 'static>(
        task: impl FnOnce(skylight::MainThread) -> F,
    ) -> crate::pool::Task {
        crate::pool::Task::owns(&spawn(proof, task))
    }

    /// Whether this thread has a driver at all, for the tests that care that
    /// one is not built until something asks.
    #[cfg(test)]
    pub fn running() -> bool {
        DRIVER.with(|slot| slot.borrow().is_some())
    }
}

/// Adds `source` to `run_loop`, and hands back how to take it off again.
///
/// **The one place a run loop source is scheduled.** Every source that gets one
/// — `IOKit`'s power notification, `SCDynamicStore`'s network one, an
/// `AXObserver` — used to carry its own two-field struct and its own
/// destructor; this is that, once.
///
/// The teardown stays on the thread that registered, which is where it belongs
/// and where the registration is dropped. It is the source's *future* that
/// moves, and that holds only the stream.
///
/// `impl AsRef<CFRunLoop>` so a `skylight::MainThread` can be handed over
/// as-is — the proof knows its own loop, and a caller should not have to say so.
pub fn scheduled(run_loop: impl AsRef<CFRunLoop>, source: CFRetained<CFRunLoopSource>) -> Undo {
    run_loop.as_ref().schedule(source)
}

/// Putting a source on a run loop, and taking it off again.
///
/// A method on the loop rather than a function taking one: an errand is handed
/// the `&CFRunLoop` it is running on, and what it wants to say next is
/// `here.schedule(source)`.
pub trait Schedule {
    /// Adds `source`, until the returned [`Undo`] is dropped.
    fn schedule(&self, source: CFRetained<CFRunLoopSource>) -> Undo;
}

impl Schedule for CFRunLoop {
    fn schedule(&self, source: CFRetained<CFRunLoopSource>) -> Undo {
        let run_loop: CFRetained<CFRunLoop> = CFRetained::from(self);
        run_loop.add_source(Some(&source), common_modes());
        Undo(Some(Box::new(move || {
            run_loop.remove_source(Some(&source), common_modes());
        })))
    }
}

impl Schedule for Loop {
    /// On this loop, whichever thread asks.
    fn schedule(&self, source: CFRetained<CFRunLoopSource>) -> Undo {
        self.turning.schedule(source)
    }
}

/// The main thread's loop, which can hand out proof of it.
///
/// A [`Loop`] cannot: it is `Send + Sync` because it lives in a static and is
/// reached from anywhere, and `MainThread` is neither, so one cannot be stored
/// in it. What *can* be stored is the fact that this loop is the main one —
/// and a callout on a run loop happens on the thread turning that loop, so an
/// errand running here is on the main thread by construction.
///
/// That is what this wraps. [`MainLoop::run_on`] and [`MainLoop::spawn`] hand
/// the work its `MainThread`, so a worker on [`crate::pool`] can push
/// window-server work back to the thread that draws without the receiving end
/// having to ask for proof it cannot get.
pub struct MainLoop(Loop);

impl MainLoop {
    /// Wraps the main thread's loop. The proof is not kept — only the fact.
    #[must_use]
    pub fn new(proof: &impl skylight::MainThreadProof) -> Self {
        Self(Loop::main(proof))
    }

    /// Runs `work` on the main thread, handing it the proof.
    pub fn run_on(&self, work: impl FnOnce(skylight::MainThread) + Send + 'static) {
        self.0.run_on(move |_| work(Self::proof()));
    }

    /// Asks the main thread for something and waits for the answer.
    ///
    /// The work is handed a `MainThreadMarker` rather than a [`MainThread`],
    /// which is what makes a `#[skylight::main_thread]` function passable
    /// straight through — those take `impl MainThreadProof + Copy`, and a
    /// `MainThread` holds a `CFRetained` so it is not `Copy`:
    ///
    /// ```ignore
    /// let apps = main.call(extras::running_apps).await?;
    /// ```
    ///
    /// A closure works the same way where the proof is not the only argument,
    /// or is not first: `main.call(move |mtm| Window::new(frame, mtm))`.
    ///
    /// The shape a worker wants: `AppKit`'s running-application list, a window
    /// server read, anything that will only answer on the thread that draws.
    /// The work is handed proof, and the *answer* comes back — so the caller
    /// can be a task on [`crate::pool`] and be moved between workers while it
    /// waits, because only `T` crosses and `T` is `Send`.
    ///
    /// What the work touches need not be: the `NSArray`, the `MainThreadMarker`
    /// and anything else main-thread-bound stays there and never leaves.
    ///
    /// # Errors
    ///
    /// [`Stopped`] if the loop went away before it ran the work.
    pub async fn call<T: Send + 'static>(
        &self,
        work: impl FnOnce(objc2::MainThreadMarker) -> T + Send + 'static,
    ) -> Result<T, Stopped> {
        self.0
            .call(move |_| work(skylight::MainThreadProof::marker(&Self::proof())))
            .await
    }

    /// Runs a `!Send` future on the main thread and awaits its answer.
    ///
    /// [`MainLoop::call`] is for work that finishes in one go;
    /// [`MainLoop::spawn`] runs a future there but hands nothing back. This is
    /// the two together, and the shape most main-thread work actually wants:
    /// `async move` over there, holding the proof and whatever `!Send` things
    /// it needs across every await, with only the answer coming home.
    ///
    /// ```ignore
    /// let title = main.run(|mtm| async move {
    ///     let window = Window::new(frame, *mtm);
    ///     window.settle().await;
    ///     window.title()           // `String`, so it can come back
    /// }).await?;
    /// ```
    ///
    /// # Errors
    ///
    /// [`Stopped`] if the loop went away before the future finished.
    pub async fn run<T: Send + 'static, F: Future<Output = T> + 'static>(
        &self,
        make: impl FnOnce(skylight::MainThread) -> F + Send + 'static,
    ) -> Result<T, Stopped> {
        let (answer, wait) = tokio::sync::oneshot::channel();
        // Held until the answer arrives. Dropping it would abort the very work
        // being waited on, which is what discarding it here used to do.
        let _running = self.spawn(move |proof| async move {
            // Ignored on purpose: a caller that stopped waiting dropped the
            // receiving end, and the work is done either way.
            drop(answer.send(make(proof).await));
        });
        wait.await.map_err(|_| Stopped)
    }

    /// Builds a future on the main thread and runs it there, for as long as the
    /// returned handle lives.
    ///
    /// The future may be `!Send` — it is built where it will be polled — and it
    /// is handed proof, so it can hold a `MainThreadMarker` across every await.
    #[must_use = "dropping the handle stops the task; bind it for as long as the work should run"]
    pub fn spawn<F: Future<Output = ()> + 'static>(
        &self,
        make: impl FnOnce(skylight::MainThread) -> F + Send + 'static,
    ) -> crate::pool::Task {
        self.0.spawn(move |_| make(Self::proof()))
    }

    /// Mints the proof, on the thread an errand of this loop runs on.
    fn proof() -> skylight::MainThread {
        // SAFETY: only reachable from an errand of a `Loop::main`, which
        // `MainLoop::new` took main-thread proof to build --- and
        // CoreFoundation runs a source's callout on the thread turning that
        // loop. So this *is* the main thread; the marker is carried across a C
        // boundary that cannot hold it, not invented.
        let marker = unsafe { objc2::MainThreadMarker::new_unchecked() };
        skylight::MainThread::of(&marker)
    }
}

/// The main thread's loop, once it has been published.
///
/// A `LazyLock` cannot do this: building one takes main-thread proof, and a
/// static initialiser runs wherever it is first touched. So it is set once, on
/// the main thread, by [`publish_main`] — and read from anywhere, because
/// [`MainLoop`] is `Send + Sync` and everything reachable through it either
/// crosses safely or is minted on the far side.
static MAIN: std::sync::OnceLock<MainLoop> = std::sync::OnceLock::new();

/// Publishes the main thread's loop. Idempotent.
#[skylight::main_thread]
pub fn publish_main() -> &'static MainLoop {
    MAIN.get_or_init(|| MainLoop::new(&proof))
}

/// Runs a main-thread-only function on the main thread and awaits its answer.
///
/// The short way to reach [`MainLoop::call`] without finding the loop first:
///
/// ```ignore
/// let apps = runloop::on_main(extras::running_apps).await?;
/// ```
///
/// Awaitable from anywhere — a worker, another loop — because only `T` crosses
/// back and `T` is `Send`. What the work touches need not be.
///
/// # Errors
///
/// [`Stopped`] if the main loop has not been published yet, or went away
/// before it ran the work. Both mean the same thing to a caller: no answer is
/// coming, and it is a fact about the process rather than about the work.
pub async fn on_main<T: Send + 'static>(
    work: impl FnOnce(objc2::MainThreadMarker) -> T + Send + 'static,
) -> Result<T, Stopped> {
    match main_loop() {
        Some(main) => main.call(work).await,
        None => Err(Stopped),
    }
}

/// Runs `async move` work on the main thread and awaits its answer.
///
/// [`on_main`]'s counterpart for work that has to await. The future is built on
/// the main thread and may be `!Send` — holding a `MainThread`, a `CFRetained`,
/// an `Rc` across its awaits — while the caller is an ordinary `Send` task that
/// can be moved between workers while it waits.
///
/// # Errors
///
/// [`Stopped`] if the main loop has not been published yet, or went away before
/// the future finished.
pub async fn on_main_async<T: Send + 'static, F: Future<Output = T> + 'static>(
    make: impl FnOnce(skylight::MainThread) -> F + Send + 'static,
) -> Result<T, Stopped> {
    match main_loop() {
        Some(main) => main.run(make).await,
        None => Err(Stopped),
    }
}

/// The main thread's loop, for work that has to happen there.
///
/// `None` before [`publish_main`] has run, which is a fact about start-up
/// order rather than about the caller — answered rather than panicked over, so
/// a worker that is early simply does not queue anything.
#[must_use]
pub fn main_loop() -> Option<&'static MainLoop> {
    MAIN.get()
}

impl std::ops::Deref for MainLoop {
    type Target = Loop;

    fn deref(&self) -> &Loop {
        &self.0
    }
}

/// Something to undo, done when this is dropped.
///
/// A `Box<dyn FnOnce()>` looks like it would do — and it does not, which is
/// what this exists to prevent: dropping a boxed closure drops it, it does not
/// call it. Storing one as a teardown and trusting `Drop` is a teardown that
/// silently never runs, which is exactly what happened to this module's run
/// loop sources.
pub struct Undo(Option<Box<dyn FnOnce()>>);

impl Undo {
    /// Undoes it now rather than at the end of the scope.
    pub fn now(self) {
        drop(self);
    }
}

impl Drop for Undo {
    fn drop(&mut self) {
        if let Some(undo) = self.0.take() {
            undo();
        }
    }
}

impl std::fmt::Debug for Undo {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Undo").finish_non_exhaustive()
    }
}

pub use tasks::{owned, spawn};

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    /// Carbon's own main loop. Never returns until `QuitApplicationEventLoop`.
    fn RunApplicationEventLoop();
    fn QuitApplicationEventLoop();
}

thread_local! {
    /// The right to end the loop, while one is running here.
    static QUIT: std::cell::Cell<Option<AppLoop>> = const { std::cell::Cell::new(None) };
}

/// Carbon's application event loop: entered once, left once.
///
/// The pairing is inverted from every other guard in this crate.
/// `RunApplicationEventLoop` does not return until `QuitApplicationEventLoop`
/// is called from a callback several frames below it, so a value whose *drop*
/// quit would run after the quit rather than cause it — the wrong order to be
/// a guard. What can still be made unforgeable is the quit: [`AppLoop::enter`]
/// mints the one token that reaches that call and parks it where a callback
/// can find it, [`AppLoop::take`] is the only way to get it, and [`AppLoop::quit`]
/// consumes it. So the loop cannot be ended twice, cannot be ended by anything
/// that did not enter it, and there is no bare `QuitApplicationEventLoop` left
/// to call by hand.
pub struct AppLoop(());

impl AppLoop {
    /// Parks this thread in the loop until [`AppLoop::quit`] ends it.
    ///
    /// The only thing that dispatches a Carbon event to the handlers installed
    /// on the event dispatcher target — which is how a click on the bar
    /// arrives. It runs the same `CFRunLoop` underneath, so every source and
    /// timer works exactly as it would under `CFRunLoopRunInMode`.
    pub fn enter() {
        QUIT.with(|slot| slot.set(Some(Self(()))));
        // SAFETY: called on the main thread, which owns the run loop and every
        // window on it. Returns once the parked token has been used.
        unsafe { RunApplicationEventLoop() };
        // Nothing may quit a loop that already returned.
        QUIT.with(std::cell::Cell::take);
    }

    /// The token, if a loop is running on this thread and nothing has taken it.
    #[must_use]
    pub fn take() -> Option<Self> {
        QUIT.with(std::cell::Cell::take)
    }

    /// Ends the loop [`AppLoop::enter`] is parked in.
    pub fn quit(self) {
        // SAFETY: a token exists only between `enter` parking on this thread
        // and the loop returning, so this ends a loop that is running.
        unsafe { QuitApplicationEventLoop() };
    }
}

#[cfg(test)]
mod task_tests {
    //! The executor, and the one thing about it that can fail silently.

    use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
    use std::cell::Cell;
    use std::rc::Rc;

    /// Lets the run loop take its turns, so a signalled driver gets to poll.
    fn pump(seconds: f64) {
        // SAFETY: `kCFRunLoopDefaultMode` is a `'static` constant, and this is
        // the thread that owns the run loop being pumped.
        unsafe { CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, seconds, false) };
    }

    /// Nothing until something asks: a thread that never spawns builds no
    /// executor and installs no source.
    #[test]
    fn no_executor_exists_until_a_task_wants_one() {
        assert!(!super::tasks::running());
        let ran = Rc::new(Cell::new(false));
        let flag = Rc::clone(&ran);
        super::spawn(super::main_thread(), |_main| async move { flag.set(true) });
        assert!(super::tasks::running());
        pump(0.2);
        assert!(ran.get(), "a spawned task runs on the run loop");
    }

    /// A task spawned while the driver is parked still runs.
    ///
    /// Covered twice over: [`super::spawn`] signals the source itself, which
    /// is what makes the *first* ever spawn work — the executor's own notify
    /// has no sleeper to reach before anything has been polled. So this
    /// passes even with the retention below broken, and the test that
    /// actually guards that is the cross-thread one.
    #[test]
    fn spawning_into_a_parked_driver_wakes_the_run_loop() {
        // A first task, run to completion, so the driver has polled and then
        // parked on an empty queue.
        super::spawn(super::main_thread(), |_main| async {});
        pump(0.2);

        let ran = Rc::new(Cell::new(false));
        let flag = Rc::clone(&ran);
        super::spawn(super::main_thread(), |_main| async move { flag.set(true) });
        pump(0.5);
        assert!(
            ran.get(),
            "a task spawned into a parked driver never woke the run loop"
        );
    }

    /// **The silent failure this driver is shaped around.**
    ///
    /// A task already waiting on something another thread will produce. The
    /// wake has nothing to do with `spawn`: it arrives through the waker the
    /// executor is holding, which is the one the *stored* `tick()` future put
    /// in its sleeper list. Dropping that future takes it back out — so a
    /// driver that built a fresh `tick()` per callback would sleep through
    /// this wake and every one like it, with the work sitting in the queue
    /// and nothing at all to see. Verified by breaking it: the assertion
    /// fails, rather than the test hanging, because the pump is bounded.
    #[test]
    fn a_task_waiting_on_another_thread_is_woken_here() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u32>();
        let got = Rc::new(Cell::new(0));
        let slot = Rc::clone(&got);
        super::spawn(super::main_thread(), |_main| async move {
            if let Some(value) = rx.recv().await {
                slot.set(value);
            }
        });

        // Parked: the task is pending on the channel, the executor is pending
        // on the task, and nothing here can move it.
        pump(0.2);
        assert_eq!(got.get(), 0, "nothing arrived yet");

        std::thread::spawn(move || tx.send(7).expect("the receiver is alive"))
            .join()
            .expect("the sending thread finished");
        pump(0.5);
        assert_eq!(got.get(), 7, "the wake crossed threads and landed here");
    }

    /// A panicking task must not unwind into CoreFoundation, and must not
    /// take the executor with it.
    #[test]
    fn a_panicking_task_is_caught_and_the_rest_still_run() {
        super::spawn(super::main_thread(), |_main| async {
            panic!("a task went wrong")
        });
        let ran = Rc::new(Cell::new(false));
        let flag = Rc::clone(&ran);
        super::spawn(super::main_thread(), |_main| async move { flag.set(true) });
        pump(0.5);
        assert!(ran.get(), "the executor survived a task panicking");
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::{armed, wake_at};
    use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    /// Lets the run loop take a turn, so a task that is due gets to poll.
    fn pump(seconds: f64) {
        // SAFETY: `kCFRunLoopDefaultMode` is a `'static` constant, and this is
        // the thread that owns the run loop being pumped.
        unsafe { CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, seconds, false) };
    }

    /// Pumps in short bursts until `done`, or `timeout` has passed.
    ///
    /// A tokio timer firing crosses more hops to reach the run loop than a
    /// `CFRunLoopTimer` did — its own driver thread, then this task's waker,
    /// then the run loop source — so a single fixed sleep is the wrong tool.
    fn wait_for(timeout: Duration, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + timeout;
        while !done() && Instant::now() < deadline {
            pump(0.02);
        }
    }

    /// A waker that only counts, for the tests that care about arming rather
    /// than about what the wake then does.
    fn counting(count: &Arc<AtomicU32>) -> std::task::Waker {
        struct Count(Arc<AtomicU32>);
        impl std::task::Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        std::task::Waker::from(Arc::new(Count(Arc::clone(count))))
    }

    /// The requirement, stated as an assertion: nothing due, nothing armed.
    #[test]
    fn nothing_due_arms_nothing() {
        assert_eq!(armed(), None, "the reactor starts empty");

        let woken = Arc::new(AtomicU32::new(0));
        let deadline = Instant::now() + Duration::from_mins(1);
        let registration = wake_at(super::main_thread(), deadline, counting(&woken));
        assert_eq!(
            armed(),
            Some(deadline),
            "one registration is one armed deadline"
        );

        drop(registration);
        assert_eq!(
            armed(),
            None,
            "and taking it away takes the deadline with it"
        );
        assert_eq!(woken.load(Ordering::Relaxed), 0, "nothing fired");
    }

    /// Two deadlines still report as one — the earlier — and the later takes
    /// over once the earlier one goes.
    #[test]
    fn the_earliest_deadline_is_the_one_armed() {
        let woken = Arc::new(AtomicU32::new(0));
        let now = Instant::now();
        let soon = now + Duration::from_mins(1);
        let later = now + Duration::from_mins(5);

        let far = wake_at(super::main_thread(), later, counting(&woken));
        assert_eq!(armed(), Some(later));
        let near = wake_at(super::main_thread(), soon, counting(&woken));
        assert_eq!(armed(), Some(soon), "the earlier deadline wins");

        drop(near);
        assert_eq!(armed(), Some(later), "re-armed rather than left early");
        drop(far);
        assert_eq!(armed(), None);
    }

    /// A deadline that arrives wakes its waker once and then stops existing,
    /// so a wake cannot repeat and an empty set cannot keep anything armed.
    #[test]
    fn a_deadline_that_arrives_wakes_once_and_disarms() {
        let woken = Arc::new(AtomicU32::new(0));
        let deadline = Instant::now() + Duration::from_millis(50);
        let registration = wake_at(super::main_thread(), deadline, counting(&woken));
        assert!(armed().is_some());

        wait_for(Duration::from_secs(2), || {
            woken.load(Ordering::Relaxed) >= 1
        });
        assert_eq!(woken.load(Ordering::Relaxed), 1, "woken exactly once");
        assert_eq!(armed(), None, "and nothing is left armed");

        pump(0.2);
        assert_eq!(woken.load(Ordering::Relaxed), 1, "and it does not repeat");
        drop(registration);
    }
}

#[cfg(test)]
mod tests {
    use super::Waker;
    use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Lets the run loop take a turn, so a signalled source gets to run.
    fn pump() {
        // SAFETY: `kCFRunLoopDefaultMode` is a `'static` constant, and this is
        // the thread that owns the run loop being pumped.
        unsafe { CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, 0.05, false) };
    }

    #[test]
    fn a_std_waker_wakes_the_run_loop() {
        let ran = Arc::new(AtomicU32::new(0));
        let counted = Arc::clone(&ran);
        let waker = Waker::install(super::main_thread(), move || {
            counted.fetch_add(1, Ordering::Relaxed);
        });

        let std_waker = waker.to_std();
        assert_eq!(ran.load(Ordering::Relaxed), 0, "nothing runs before a wake");

        std_waker.wake_by_ref();
        pump();
        assert_eq!(ran.load(Ordering::Relaxed), 1);

        // A clone is the same source, not a second registration.
        let cloned = std_waker.clone();
        drop(std_waker);
        cloned.wake();
        pump();
        assert_eq!(
            ran.load(Ordering::Relaxed),
            2,
            "the clone outlives the original"
        );
    }

    #[test]
    fn a_std_waker_survives_the_thread_it_was_made_on_being_the_only_holder() {
        // The point of the bridge: woken from elsewhere, run here. A waker
        // handed to another thread must keep the source alive by itself.
        let ran = Arc::new(AtomicU32::new(0));
        let counted = Arc::clone(&ran);
        let waker = Waker::install(super::main_thread(), move || {
            counted.fetch_add(1, Ordering::Relaxed);
        });
        let std_waker = waker.to_std();
        drop(waker);

        std::thread::spawn(move || std_waker.wake())
            .join()
            .expect("the waking thread finished");
        pump();
        assert_eq!(ran.load(Ordering::Relaxed), 1);
    }
}

/// A run loop on a thread of its own.
///
/// For the CoreFoundation sources that have no business on the main thread.
/// `IOKit`'s power notification and `SCDynamicStore`'s network one are ordinary
/// run loop sources: they will fire on whatever loop they are added to, and
/// putting them on the one that composites the bar means every power event and
/// every network change is a callout competing with a frame.
///
/// # Why a whole thread
///
/// A `CFRunLoop` belongs to a thread — there is no way to pump one from
/// somewhere else — so a loop that is not the main one needs a thread that does
/// nothing but pump it. That is not the same as the worker pool in
/// [`crate::pool`]: a pool thread takes tasks and finishes them, this one is
/// parked in CoreFoundation waiting to be called back, which is what a source
/// needs and what a pool cannot offer.
///
/// # Getting the answer back
///
/// Nothing here does. A source scheduled on one of these runs its callback on
/// this thread, and that callback hands its result over the way every other
/// off-main thing does — through the `Emitter` it was registered with, which
/// wakes the main run loop. So a source moving here changes which thread its
/// callback runs on and nothing else about how it reports.
pub struct Loop {
    /// The loop itself. Named for what it is rather than repeating the type.
    turning: CFRetained<CFRunLoop>,
    /// Signalled to nothing, and never removed: a `CFRunLoop` with no sources
    /// returns immediately from `run`, so without this the thread would fall
    /// straight out of the loop it was started to turn.
    ///
    /// `None` for the main thread's loop, which Carbon keeps turning.
    _keep_alive: Option<CFRetained<CFRunLoopSource>>,
    /// Work handed over from another thread, waiting to run on this one.
    ///
    /// A channel rather than a `Mutex<Vec<_>>`: the sending side takes no lock
    /// at all, so an errand that hands over another cannot wait on a lock the
    /// drain is holding. That hazard used to be avoided by draining out from
    /// under the lock — correct, but only for as long as someone remembered.
    errands: tokio::sync::mpsc::UnboundedSender<Errand>,
    /// Signals the source that drains [`Loop::errands`].
    ring: Waker,
}

/// One piece of work to run on a loop's own thread, handed that loop.
///
/// The loop is passed rather than looked up because the errand is running *on*
/// it — that is the whole point of having been handed over — and because it is
/// what `add_source` needs. A `CFRetained<CFRunLoop>` is `!Send`, so it cannot
/// be captured on the way here; it is supplied on arrival instead.
type Errand = Box<dyn FnOnce(&CFRunLoop) + Send>;

/// A [`Loop`] stopped before it could answer a [`Loop::call`].
///
/// The errand may or may not have run; what is certain is that no answer is
/// coming. In this daemon the loops outlive everything that calls them, so a
/// caller seeing this is shutting down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the run loop stopped before it answered")]
pub struct Stopped;

// SAFETY: `CFRunLoop` is one of the few CoreFoundation types Apple documents as
// thread-safe, and what this exposes is the subset that is: adding a source,
// waking the loop, and stopping it. The loop is pumped only by the thread
// `start` spawned, and nothing here hands out anything that thread also touches.
unsafe impl Send for Loop {}
unsafe impl Sync for Loop {}

impl Loop {
    /// Starts a thread pumping a run loop of its own, named `name`.
    ///
    /// Blocks until that thread has a loop to hand back, which is one thread
    /// spawn and no more: the point of the wait is that a caller can schedule
    /// on the returned loop immediately.
    ///
    /// # Panics
    ///
    /// Panics if the thread cannot be spawned, or if CoreFoundation declines
    /// the keep-alive source — the process is out of resources either way.
    #[must_use]
    pub fn start(name: &str) -> Self {
        let (ready, wait) = std::sync::mpsc::sync_channel::<Handoff>(1);
        std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let run_loop = CFRunLoop::current().expect("a new thread has a run loop");
                let keep_alive = keep_alive();
                run_loop.add_source(Some(&keep_alive), common_modes());

                // The loop and the source both belong to this thread until the
                // handoff; after it, the loop is shared and the source is only
                // ever read.
                if ready
                    .send(Handoff {
                        run_loop: run_loop.clone(),
                        keep_alive,
                    })
                    .is_err()
                {
                    return;
                }

                CFRunLoop::run();
            })
            .expect("a run loop thread; the process is out of resources if not");

        let Handoff {
            run_loop,
            keep_alive,
        } = wait
            .recv()
            .expect("the run loop thread hands its loop back");

        Self::over(run_loop, Some(keep_alive))
    }

    /// The main thread's loop.
    ///
    /// The same type as [`Loop::start`]'s, and deliberately: a run loop is a
    /// run loop, and everything below — the errand queue, the waker, the
    /// executor polled by it — is identical. All that differs is who turns it.
    /// This one is turned by Carbon, on a thread that already exists, so there
    /// is no thread to start and no keep-alive to add.
    ///
    /// What that buys is the direction that used to be missing: `run_on` and
    /// `spawn` on *this* loop reach the compositing thread from anywhere, which
    /// is how a worker hands back something only the main thread may touch.
    #[must_use]
    pub fn main(proof: &impl skylight::MainThreadProof) -> Self {
        Self::over(skylight::MainThreadProof::run_loop(proof), None)
    }

    /// The half both constructors share.
    fn over(
        run_loop: CFRetained<CFRunLoop>,
        keep_alive: Option<CFRetained<CFRunLoopSource>>,
    ) -> Self {
        let (errands, waiting) = tokio::sync::mpsc::unbounded_channel::<Errand>();
        // Only the drain touches it, and only on this loop's own thread, so the
        // lock is never contended -- it is here because the handler must be
        // `Send` and `try_recv` wants `&mut`.
        let waiting = std::sync::Mutex::new(waiting);
        let ring = Waker::install_on(&run_loop, move |here| {
            let mut waiting = waiting
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while let Ok(errand) = waiting.try_recv() {
                errand(here);
            }
        });

        Self {
            turning: run_loop,
            _keep_alive: keep_alive,
            errands,
            ring,
        }
    }

    /// Runs `work` on this loop's own thread, handing it that loop.
    ///
    /// The way to reach a thread parked in `CFRunLoopRun`: leave the work where
    /// its loop will find it and wake the loop. The `&CFRunLoop` is what
    /// `add_source` wants, so a source can be built and scheduled in one place
    /// without asking which loop it is on.
    pub fn run_on(&self, work: impl FnOnce(&CFRunLoop) + Send + 'static) {
        // Dropped if the loop has gone; there is nowhere to run it and nothing
        // to tell. `call` reports that as `Stopped` because it has a channel
        // to say it on.
        drop(self.errands.send(Box::new(work)));
        self.ring.wake();
    }

    /// Runs `work` on this loop's thread and waits for what it answers.
    ///
    /// The awaiting side can be anywhere — a task on [`crate::pool`]'s workers,
    /// or on another loop — because only `T` crosses back, and `T` is `Send`.
    /// What the work *touches* need not be: it may build, use and keep `!Send`
    /// values on that thread and hand back a summary. An `AXObserver` stays
    /// where its run loop is; a `Result` comes home.
    ///
    /// Nothing blocks on either side. The caller is a future that yields until
    /// the answer arrives, and the loop runs the errand between its own
    /// callouts — so a task can interleave: ask this loop for something, await
    /// it, do work on a worker, ask again.
    ///
    /// # Errors
    ///
    /// [`Stopped`] if the loop went away before it ran the work, which is the
    /// one thing that can go wrong here and is worth telling apart from
    /// whatever `T` reports about the work itself.
    pub async fn call<T: Send + 'static>(
        &self,
        work: impl FnOnce(&CFRunLoop) -> T + Send + 'static,
    ) -> Result<T, Stopped> {
        let (answer, wait) = tokio::sync::oneshot::channel();
        self.run_on(move |here| {
            // Ignored on purpose: a caller that stopped waiting dropped the
            // receiving end, and the work has already been done either way.
            drop(answer.send(work(here)));
        });
        wait.await.map_err(|_| Stopped)
    }

    /// Builds a future on this loop's thread and runs it there.
    ///
    /// **`make` is `Send`; the future it returns need not be.** That is the
    /// whole of the trick, and the reason this exists rather than a `spawn`
    /// taking a future: a future holding a `CFRetained` or an `AXUIElement`
    /// cannot cross a thread, but the recipe for one can. So the `!Send` value
    /// is built where it will live.
    ///
    /// What polls it is this loop's own [`Waker`], turned into a
    /// [`std::task::Waker`] — the same arrangement the main thread has, on a
    /// different loop. A source that needs a run loop of its own therefore gets
    /// its callbacks and its futures on one thread, woken by one mechanism,
    /// with nothing sent anywhere.
    ///
    /// **The returned handle owns the task.** Dropping it stops the work, so a
    /// discarded one cancels immediately — hence `must_use`.
    #[must_use = "dropping the handle stops the task; bind it for as long as the work should run"]
    pub fn spawn<F: Future<Output = ()> + 'static>(
        &self,
        make: impl FnOnce(&CFRunLoop) -> F + Send + 'static,
    ) -> crate::pool::Task {
        let (task, arriving) = crate::pool::Task::deferred();
        self.run_on(move |here| {
            arriving.arrived(tasks::spawn_here(make(here)).abort_handle());
        });
        task
    }

    /// The loop, for [`scheduled`].
    #[must_use]
    pub fn run_loop(&self) -> CFRetained<CFRunLoop> {
        self.turning.clone()
    }
}

impl Drop for Loop {
    fn drop(&mut self) {
        // The thread falls out of `run` and ends. Sources still scheduled on the
        // loop stop firing, which is what dropping this means.
        self.turning.stop();
    }
}

/// What the pumping thread hands back once it has a loop.
struct Handoff {
    run_loop: CFRetained<CFRunLoop>,
    keep_alive: CFRetained<CFRunLoopSource>,
}

// SAFETY: as `Loop`'s own impl -- the loop is thread-safe, and the source
// crosses once, before anything else can reach it.
unsafe impl Send for Handoff {}

/// A source that exists only so the loop has something to wait on.
///
/// Never signalled, so `never` is never called; it exists because a source
/// needs a callout and a loop needs a source.
fn keep_alive() -> CFRetained<CFRunLoopSource> {
    unsafe extern "C-unwind" fn never(_info: *mut c_void) {}

    manual_source((), never)
}

#[cfg(test)]
mod pumped_tests {
    use super::Loop;

    /// The loop has to be *running*, not merely created: a source scheduled on
    /// a loop nobody pumps never fires, which is the failure this type exists
    /// to prevent.
    #[test]
    fn a_background_loop_runs_what_is_scheduled_on_it() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let pumped = Loop::start("rsbar-test-loop");
        let fired = std::sync::Arc::new(AtomicBool::new(false));

        // A source of the ordinary kind, scheduled from *this* thread onto the
        // other one's loop -- which is the whole arrangement.
        let flag = std::sync::Arc::clone(&fired);
        let waker = super::Waker::install_on(&pumped.run_loop(), move |_here| {
            flag.store(true, Ordering::Release);
        });
        waker.wake();

        for _ in 0..500 {
            if fired.load(Ordering::Acquire) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            fired.load(Ordering::Acquire),
            "the background loop never ran it"
        );
    }

    /// The third place a future can be polled: a loop of its own.
    ///
    /// The daemon has exactly three, and they differ only in what they will
    /// accept — the main loop and this one take `!Send` futures because each is
    /// one thread turning one `LocalSet`; [`crate::pool`] takes `Send` ones and
    /// moves them between workers. This proves the middle one, which is the
    /// only one with no other coverage.
    ///
    /// The future holds an `Rc`, so it is `!Send` and could not have been sent
    /// here — it is *built* here, which is what [`Loop::spawn`] takes a factory
    /// for.
    #[test]
    fn a_background_loop_polls_a_future_built_on_its_own_thread() {
        let pumped = Loop::start("rsbar-test-spawn");
        let (tx, rx) = std::sync::mpsc::channel();

        let _running = pumped.spawn(move |_here| {
            let held = std::rc::Rc::new(7u32);
            async move {
                // Awaiting at all is the point: a future that never yields
                // proves only that the closure ran.
                tokio::task::yield_now().await;
                let _ = tx.send(*held);
            }
        });

        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(5)),
            Ok(7),
            "the loop never polled the future it was given"
        );
    }

    /// A pool task interleaving with a loop: ask, await, work, ask again.
    ///
    /// The point is that neither side blocks. The `!Send` state stays on the
    /// loop's thread for the whole exchange — proven by keeping it in a
    /// thread-local `Rc`, which could not have been sent anywhere — while the
    /// task doing the asking is an ordinary `Send` future on the workers.
    #[test]
    fn a_pool_task_interleaves_with_a_loop_without_either_blocking() {
        use std::cell::RefCell;
        use std::rc::Rc;

        thread_local! {
            /// `!Send`, so it can only ever live on the loop's own thread.
            static TALLY: RefCell<Option<Rc<RefCell<Vec<u32>>>>> = const { RefCell::new(None) };
        }

        fn note(value: u32) -> usize {
            TALLY.with(|slot| {
                let mut slot = slot.borrow_mut();
                let held = slot.get_or_insert_with(|| Rc::new(RefCell::new(Vec::new())));
                held.borrow_mut().push(value);
                held.borrow().len()
            })
        }

        let pumped = std::sync::Arc::new(Loop::start("rsbar-test-call"));
        let on_loop = std::sync::Arc::clone(&pumped);

        let seen = crate::pool::handle().block_on(async move {
            let mut counts = Vec::new();
            for value in 1..=3u32 {
                // Ask the loop, and yield until it answers.
                counts.push(on_loop.call(move |_here| note(value)).await);
                // Work that belongs on a worker, between asks.
                tokio::task::yield_now().await;
            }
            counts
        });

        assert_eq!(
            seen,
            vec![Ok(1), Ok(2), Ok(3)],
            "the loop kept its own state across three interleaved calls"
        );
    }

    /// A value crosses all three executors and comes home, checked at each hop.
    ///
    /// The whole model in one path: start on a worker, ask the main thread for
    /// something only `AppKit` will answer, hand that to the run loop on its own
    /// thread, and carry the result back to a worker. Every hop asserts *which*
    /// thread it ran on, so a future silently polled in the wrong place fails
    /// here rather than at four in the morning.
    ///
    /// One test rather than four because [`super::publish_main`] fills a
    /// process-wide `OnceLock` and libtest gives each test its own thread — a
    /// second test that published would address a loop nobody is pumping.
    #[test]
    fn a_value_crosses_all_three_executors_and_comes_home() {
        super::publish_main(super::main_thread());
        let sources = Loop::start("rsbar-test-round-trip");
        let main_thread = std::thread::current().id();
        let (tx, rx) = std::sync::mpsc::channel();

        // Starts on a worker of the multi-threaded runtime.
        crate::pool::spawn(async move {
            let started_on = std::thread::current().id();

            // 1. `AppKit`, which answers only on the thread that draws. The
            //    `NSArray` never leaves it; the count does.
            let asked = super::on_main(|mtm| {
                (
                    crate::extras::running_apps(mtm).len(),
                    std::thread::current().id(),
                )
            })
            .await;

            // 2. Hand it to the loop on its own thread, and work there.
            let carried = match asked {
                Ok((apps, on_main)) => sources
                    .call(move |_here| (apps * 2, on_main, std::thread::current().id()))
                    .await
                    .ok(),
                Err(_) => None,
            };

            // 3. Home to a worker.
            let _ = tx.send(carried.map(|(doubled, on_main, on_loop)| {
                (
                    doubled,
                    on_main,
                    on_loop,
                    started_on,
                    std::thread::current().id(),
                )
            }));
        });

        let mut landed = None;
        for _ in 0..800 {
            // SAFETY: `kCFRunLoopDefaultMode` is a `'static` constant and this
            // is the thread that owns the loop being pumped.
            unsafe {
                objc2_core_foundation::CFRunLoop::run_in_mode(
                    objc2_core_foundation::kCFRunLoopDefaultMode,
                    0.01,
                    false,
                )
            };
            if let Ok(got) = rx.try_recv() {
                landed = Some(got);
                break;
            }
        }

        let (doubled, on_main, on_loop, started_on, came_home) = landed
            .expect("the round trip never finished")
            .expect("a hop failed");

        assert!(doubled > 0, "AppKit reported no running applications");
        assert_eq!(
            on_main, main_thread,
            "AppKit did not run on the main thread"
        );
        assert_ne!(
            on_loop, main_thread,
            "the loop's work ran on the main thread"
        );
        assert_ne!(on_loop, started_on, "the loop's work ran on a worker");
        assert_ne!(started_on, main_thread, "the worker was the main thread");
        assert_ne!(came_home, main_thread, "it came home to the main thread");
    }

    /// Work handed to a loop from another thread runs on that loop's thread.
    #[test]
    fn an_errand_runs_on_the_loop_that_was_given_it() {
        let pumped = Loop::start("rsbar-test-errand");
        let (tx, rx) = std::sync::mpsc::channel();

        pumped.run_on(move |_here| {
            let _ = tx.send(std::thread::current().name().map(str::to_owned));
        });

        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(5)),
            Ok(Some("rsbar-test-errand".to_owned())),
            "the errand ran somewhere else"
        );
    }
}

//! State handed to a C callback, and got back safely.
//!
//! Every private-API registration on this platform is the same shape: it takes
//! a `void*` you will be handed back, and calls you on a thread and at a time of
//! its choosing. `SLSRegisterNotifyProc`, `IOPSNotificationCreateRunLoopSource`,
//! `SCDynamicStoreCreate`, `AudioObjectAddPropertyListener`,
//! `CGDisplayRegisterReconfigurationCallback`.
//!
//! Two things are easy to get wrong, and both are use-after-free:
//!
//! * the trampoline must reach the state **without moving a reference count** —
//!   it is borrowing the registration's pointer, not taking it;
//! * it must hold the state for the length of the call, so the owner may drop
//!   the registration at any moment, including *during* a callback.
//!
//! [`Callback`] is both, once. A source keeps one; dropping it deregisters and
//! then releases the state, in that order.

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::{Arc, Weak};

/// A registered C callback: how to undo it, and the state it was given.
///
/// # What dropping it does
///
/// Runs the teardown the registration returned, then releases the state. That
/// order matters: stop new callbacks first, then let go of what they would have
/// read. A callback already *running* is safe either way — [`Callback::with`]
/// holds the state for the length of the call.
///
/// For an API whose teardown cannot report whether it worked —
/// `SLSRemoveNotifyProc` answers `kCGErrorSuccess` whether or not it matched
/// anything — releasing the state is what the guarantee rests on regardless:
/// the callback goes **inert**, the trampoline finds nothing, and the handler
/// is never entered.
pub struct Callback<S> {
    /// The only strong reference. Dropping it is what makes a callback inert.
    state: Arc<S>,
    /// What the registration returned as its own teardown. `None` once run, so
    /// [`Callback::stop`] and `Drop` cannot run it twice.
    ///
    /// Not `Send`, and deliberately not made so: these capture CoreFoundation
    /// objects and raw context pointers belonging to the thread that
    /// registered, and that thread is where the teardown has to run anyway. A
    /// source's *future* is the part that moves, and it holds only the stream
    /// — which is `Send` whenever its events are. Nothing has to be asserted.
    stop: Option<Box<dyn FnOnce()>>,
    /// The `Weak` handed to the registration, as the pointer it was given.
    context: *const S,
    /// Whether dropping takes that reference back. False only for
    /// [`Callback::outliving`].
    reclaim: bool,
}

impl<S> Callback<S> {
    /// Registers a callback and returns the one value to keep.
    ///
    /// `install` is handed the context pointer to give the C API, and returns
    /// how to take the registration back — usually a closure capturing whatever
    /// the API needs to match on. Everything else happens here: the pointer, the
    /// reference rules, the tracing, and reclaiming on refusal.
    ///
    /// ```ignore
    /// let watch = Callback::new(state, |context| {
    ///     // SAFETY: the context outlives the registration; `Callback` owns it.
    ///     let source = unsafe { IOPSNotificationCreateRunLoopSource(REPORT, context) };
    ///     let source = NonNull::new(source).ok_or(Cause::IoKit)?;
    ///     // SAFETY: the Create convention hands back a +1 reference.
    ///     let source: CFRetained<CFRunLoopSource> = unsafe { CFRetained::from_raw(source) };
    ///     run_loop.add_source(Some(&source), common_modes());
    ///     Ok(move || run_loop.remove_source(Some(&source), common_modes()))
    /// })?;
    /// ```
    ///
    /// # The pointer is a `Weak`
    ///
    /// Deliberately, and never reclaimed: the weak count keeps the allocation's
    /// control block alive, so the pointer stays valid to *reconstruct* for as
    /// long as the API might use it — which for several of these is the life of
    /// the process. The strong count stays in this value, so the state itself
    /// goes when this does.
    ///
    /// # Errors
    ///
    /// Whatever `install` fails with. The weak reference is taken back first, so
    /// a refused registration leaks nothing.
    pub fn new<Stop, E>(
        state: Arc<S>,
        install: impl FnOnce(*mut c_void) -> Result<Stop, E>,
    ) -> Result<Self, E>
    where
        Stop: FnOnce() + 'static,
    {
        Self::install(state, install, true)
    }

    /// [`Callback::new`] for a registration that can still fire after its
    /// teardown has run.
    ///
    /// The weak reference is *not* taken back, so the control block outlives
    /// this and a late callback finds a pointer it can still reconstruct: it
    /// upgrades to `None` and does nothing. The cost is that allocation, for
    /// the life of the process — which is the price of the guarantee, and only
    /// worth paying where the guarantee is actually needed.
    ///
    /// One caller: `SLSRegisterNotifyProc`. `SLSRemoveNotifyProc` reports
    /// success whether or not it matched anything, so there is no way to know
    /// the procedure is really gone.
    ///
    /// # Errors
    ///
    /// Whatever `install` fails with.
    pub fn outliving<Stop, E>(
        state: Arc<S>,
        install: impl FnOnce(*mut c_void) -> Result<Stop, E>,
    ) -> Result<Self, E>
    where
        Stop: FnOnce() + 'static,
    {
        Self::install(state, install, false)
    }

    fn install<Stop, E>(
        state: Arc<S>,
        install: impl FnOnce(*mut c_void) -> Result<Stop, E>,
        reclaim: bool,
    ) -> Result<Self, E>
    where
        Stop: FnOnce() + 'static,
    {
        let context = Weak::into_raw(Arc::downgrade(&state))
            .cast_mut()
            .cast::<c_void>();

        match install(context) {
            Ok(stop) => {
                tracing::debug!(callback = context.addr(), "registered a callback");
                Ok(Self {
                    state,
                    stop: Some(Box::new(stop)),
                    context: context.cast::<S>().cast_const(),
                    reclaim,
                })
            }
            Err(err) => {
                tracing::debug!(callback = context.addr(), "a registration was refused");
                // SAFETY: the pointer came from `Weak::into_raw` above and the
                // failed registration did not keep it.
                drop(unsafe { Weak::from_raw(context.cast::<S>()) });
                Err(err)
            }
        }
    }

    /// Runs the handler with the state behind `context`, if it is still there.
    ///
    /// **What a trampoline calls.** Three things happen, each easy to get wrong
    /// alone:
    ///
    /// 1. the `Weak` is reconstructed and put straight back with
    ///    [`ManuallyDrop`] — the pointer is *borrowed*, so dropping it would give
    ///    up a count this call never owned and the next callout would find a
    ///    freed control block;
    /// 2. it is upgraded, taking a strong reference for the length of `f` — so
    ///    the owner may drop the [`Callback`] mid-callback and the state still
    ///    outlives the call;
    /// 3. a null pointer, or state already gone, answers `None` rather than
    ///    dereferencing. Frameworks do call back during teardown.
    ///
    /// # Safety
    ///
    /// `context` must be null, or a pointer [`Callback::new`] handed to its
    /// `install` for this same `S`.
    pub unsafe fn with<R>(context: *mut c_void, f: impl FnOnce(&S) -> R) -> Option<R> {
        if context.is_null() {
            return None;
        }
        // SAFETY: the caller guarantees the pointer; the registration holds its
        // weak count, so reconstructing is valid even if the state has gone.
        let weak = ManuallyDrop::new(unsafe { Weak::from_raw(context.cast::<S>()) });
        let state = weak.upgrade()?;
        tracing::trace!(callback = context.addr(), "a callback fired");
        Some(f(&state))
    }

    /// Another handle on the state, for the registering side.
    #[must_use]
    pub fn state(&self) -> Arc<S> {
        Arc::clone(&self.state)
    }

    /// Deregisters now rather than at drop.
    ///
    /// For a caller that must sequence teardown — deregister, then do something
    /// that would have raced with a callback. Idempotent.
    pub fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            tracing::debug!("deregistering a callback");
            stop();
        }
    }
}

impl<S> Drop for Callback<S> {
    fn drop(&mut self) {
        self.stop();
        if self.reclaim {
            // SAFETY: `context` came from `Weak::into_raw` in `install` and
            // went to exactly one registration, which `stop` above has just
            // taken down -- an authoritative teardown, on the thread delivery
            // happens on, so nothing is inside `with` holding a reconstructed
            // copy and nothing can arrive to make one. Leaving it behind cost
            // 48 bytes a registration for the life of the process.
            drop(unsafe { Weak::from_raw(self.context) });
        }
        // `state` goes next, on its own: released after the teardown, never
        // before it.
    }
}

impl<S> std::fmt::Debug for Callback<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Callback")
            .field("live", &self.stop.is_some())
            .finish_non_exhaustive()
    }
}

/// State for an API that manages the context's lifetime itself.
///
/// The other model, and the distinction is real. [`Callback`] is for APIs that
/// take a pointer and never mention it again — they cannot say when they are
/// done, so the state is kept by the handle and the context is a `Weak` that is
/// never reclaimed.
///
/// CoreFoundation is not like that: a `CFRunLoopSourceContext`, a
/// `CFMachPortContext` and their kin carry **`retain` and `release` callbacks**
/// and call them. So hand over a strong reference and let it manage one, which
/// leaks nothing at all.
///
/// Pick by looking at the C struct being filled in: a `retain`/`release` pair
/// means this one.
pub struct Retained<F>(std::marker::PhantomData<F>);

impl<F: 'static> Retained<F> {
    /// Hands `handler` over, as one strong reference.
    #[must_use]
    pub fn hand_over(handler: F) -> *mut c_void {
        Arc::into_raw(Arc::new(handler)).cast_mut().cast::<c_void>()
    }

    /// Borrows the handler behind a context pointer.
    ///
    /// # Safety
    ///
    /// `context` must come from [`Retained::hand_over`] for this same `F` and
    /// still be retained; the borrow must not outlive the call.
    pub unsafe fn borrow<'a>(context: *mut c_void) -> &'a F {
        // SAFETY: the caller guarantees provenance, type and liveness.
        unsafe { &*context.cast::<F>() }
    }

    /// Gives up one strong reference — to hand ownership over once the API has
    /// retained, or to reclaim the handler when it never did.
    ///
    /// # Safety
    ///
    /// `context` must come from [`Retained::hand_over`] for this same `F`, with
    /// a reference the caller is entitled to drop.
    pub unsafe fn give_up(context: *mut c_void) {
        // SAFETY: the caller guarantees provenance and the reference.
        drop(unsafe { Arc::from_raw(context.cast::<F>()) });
    }

    /// The `retain` callback for the C struct.
    ///
    /// # Safety
    ///
    /// The API calls this only with the pointer it was handed, while it still
    /// holds a reference.
    #[expect(
        clippy::must_use_candidate,
        reason = "CoreFoundation calls this; the return is the contract"
    )]
    pub unsafe extern "C-unwind" fn retain(context: *const c_void) -> *const c_void {
        // SAFETY: as above -- a live `Arc<F>` allocation.
        unsafe { Arc::increment_strong_count(context.cast::<F>()) };
        context
    }

    /// The `release` callback for the C struct.
    ///
    /// # Safety
    ///
    /// As [`Retained::retain`]. At the last reference the handler is dropped.
    pub unsafe extern "C-unwind" fn release(context: *const c_void) {
        // SAFETY: as above.
        unsafe { Arc::decrement_strong_count(context.cast::<F>()) };
    }
}

/// The handler's end of a [`relay`]: somewhere to put an event and return.
///
/// This is what a trampoline's state usually *is*, once the source it belongs
/// to has been written as a future.
pub struct Relay<T> {
    /// Written once, when the relay is made. There is no reopening a relay, so
    /// there is nothing to guard: `post` reaches the sender straight from a C
    /// callout, taking no lock in a place with no way to wait.
    sending: tokio::sync::mpsc::UnboundedSender<T>,
}

impl<T> Relay<T> {
    /// Hands one event to whoever is listening.
    ///
    /// Never waits and never fails for want of room, which is what makes it
    /// callable from a place with no way to wait and nothing to return an error
    /// to. `false` means nothing is listening any more.
    pub fn post(&self, event: T) -> bool {
        self.sending.send(event).is_ok()
    }

    /// Whether anything is still listening, without posting.
    #[must_use]
    pub fn listening(&self) -> bool {
        !self.sending.is_closed()
    }
}

impl<T> std::fmt::Debug for Relay<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Relay")
            .field("listening", &self.listening())
            .finish()
    }
}

/// The task's end of a [`relay`]: a [`Stream`](futures_core::Stream) of what
/// the callback posted.
pub struct Events<T> {
    events: tokio::sync::mpsc::UnboundedReceiver<T>,
}

impl<T> Events<T> {
    /// Whether anything is queued, without taking it — a run condition's
    /// question, so it must not need `&mut self` to ask.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// An event that is already here, without waiting for one.
    pub fn ready(&mut self) -> Option<T> {
        self.events.try_recv().ok()
    }

    /// The next event, or `None` once the registration behind it is gone.
    ///
    /// `None` is the ordinary end of a source's future: the [`Callback`] was
    /// dropped, so the [`Relay`] went with it and nothing more can arrive. A
    /// `while let Some(..)` loop over this therefore *is* the registration's
    /// lifetime, with no separate shutdown signal to plumb.
    pub async fn next(&mut self) -> Option<T> {
        self.events.recv().await
    }

    /// The next event and everything already queued behind it, up to `most`.
    ///
    /// For a source that would rather answer a burst once than once per
    /// callback — twelve `AirPort` sub-key changes for one join, or
    /// `CoreGraphics` announcing a reconfiguration per display. Waits for the
    /// first and takes the rest without waiting, so it never delays an event to
    /// see whether another is coming.
    pub async fn batch(&mut self, most: usize) -> Option<Vec<T>> {
        let mut taken = Vec::with_capacity(1);
        taken.push(self.next().await?);
        while taken.len() < most {
            match self.ready() {
                Some(event) => taken.push(event),
                None => break,
            }
        }
        Some(taken)
    }
}

impl<T> futures_core::Stream for Events<T> {
    type Item = T;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<T>> {
        self.events.poll_recv(context)
    }
}

impl<T> std::fmt::Debug for Events<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Events").finish_non_exhaustive()
    }
}

/// Turns a C callback into a stream of events.
///
/// A callback cannot hold anything between calls, so a source that remembers
/// one thing had to put it behind the context pointer under a lock — taken from
/// a place with nowhere to `.await`. A future holds it in a local instead. The
/// callback reads what is valid only for the length of the call (a `CFArray`, a
/// Carbon `EventRef`, a `SCDynamicStore` handle) into an owned `T` and posts
/// *that*; the future does the rest.
///
/// Where that future runs is decided by whether it is `Send` and nothing else,
/// so the wrong choice does not compile. One relay delivers in order, and
/// nothing is dropped while anything is listening.
///
/// The queue is `tokio::sync::mpsc`'s: a sender that never blocks and never
/// fails for want of room is exactly what a C callback needs, and it is not
/// worth writing again.
#[must_use]
pub fn relay<T>() -> (std::sync::Arc<Relay<T>>, Events<T>) {
    let (sending, events) = tokio::sync::mpsc::unbounded_channel();
    (std::sync::Arc::new(Relay { sending }), Events { events })
}

/// Declares the C entry point for a handler.
///
/// The handler stays an **ordinary function**, written above and formatted by
/// `rustfmt` like any other. This only builds the `extern "C-unwind"` shim that
/// stands in front of it, from the argument list the C API actually has.
///
/// ```ignore
/// fn report(state: &Relay<State>) {
///     // ordinary code
/// }
///
/// skylight::trampoline!(REPORT = report(@ state: &Relay<State>));
/// ```
///
/// The arguments are the C callback's own, in its order, with `@` marking the
/// one that is the `void*` — write the same names the handler uses. So `RECONFIGURED` below is
/// `extern "C-unwind" fn(CGDirectDisplayID, CGDisplayChangeSummaryFlags,
/// *mut c_void)`:
///
/// ```ignore
/// skylight::trampoline!(RECONFIGURED = reconfigured(
///     id: CGDirectDisplayID,
///     flags: CGDisplayChangeSummaryFlags,
///     @ state: &Relay<Reported>,
/// ));
/// ```
///
/// # What it does for you
///
/// * the **ABI** — `C-unwind`, because a panic crossing back into a framework
///   through a plain `extern "C"` frame is undefined behaviour;
/// * the **state recovery**, through [`Callback::with`], with the reference
///   rules and the tracing;
/// * doing **nothing** when the registration has been dropped.
///
/// # Returning a value
///
/// Say what it returns and what a dropped registration answers. That value is
/// required rather than `Default::default()` because the right one is per API
/// and getting it wrong is silent: Carbon's `noErr` would claim a dead handler
/// *handled* the event and swallow it.
///
/// ```ignore
/// skylight::trampoline!(
///     OBSERVE = observe(call: *mut c_void, event: EventRef, @ state: &Relay<()>)
///         -> OsStatus; dropped EVENT_NOT_HANDLED
/// );
/// ```
///
/// # Passing the context on
///
/// Add `+ context` and the handler takes the raw pointer as a final argument.
/// `CoreAudio` needs it: `AudioObjectRemovePropertyListener` matches on the
/// context, and a listener that re-registers from inside its own callback has
/// nowhere else to get one.
#[macro_export]
macro_rules! trampoline {
    ($entry:ident = $handler:ident ( $($args:tt)* )) => {
        $crate::trampoline!(@build $entry = $handler($($args)*) -> (); dropped (); plain);
    };

    ($entry:ident = $handler:ident ( $($args:tt)* ) -> $ret:ty; dropped $absent:expr) => {
        $crate::trampoline!(@build $entry = $handler($($args)*) -> $ret; dropped $absent; plain);
    };

    ($entry:ident = $handler:ident ( $($args:tt)* ) + context -> $ret:ty; dropped $absent:expr) => {
        $crate::trampoline!(@build $entry = $handler($($args)*) -> $ret; dropped $absent; context);
    };

    (
        @build $entry:ident = $handler:ident (
            $($arg:ident : $arg_ty:ty,)* @ $state:ident : & $state_ty:ty $(,)?
        ) -> $ret:ty; dropped $absent:expr; $pass:ident
    ) => {
        #[doc = concat!("The C entry point for [`", stringify!($handler), "`].")]
        const $entry: extern "C-unwind" fn($($arg_ty,)* *mut ::std::ffi::c_void) -> $ret = {
            extern "C-unwind" fn shim(
                $($arg: $arg_ty,)* context: *mut ::std::ffi::c_void
            ) -> $ret {
                // SAFETY: `context` is null or the pointer this callback was
                // registered with. `with` does every reference rule, traces the
                // fire, and answers `None` once the registration is gone.
                unsafe {
                    $crate::callback::Callback::<$state_ty>::with(context, |$state| {
                        $crate::trampoline!(@call $handler $pass ($($arg),*) $state context)
                    })
                }
                .unwrap_or($absent)
            }
            shim
        };
    };

    (@call $handler:ident plain ($($arg:ident),*) $state:ident $context:ident) => {
        $handler($($arg,)* $state)
    };
    (@call $handler:ident context ($($arg:ident),*) $state:ident $context:ident) => {
        $handler($($arg,)* $state, $context)
    };
}

#[cfg(test)]
mod tests {
    use super::Callback;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn bump(state: &AtomicU32) {
        state.fetch_add(1, Ordering::AcqRel);
    }

    fn answer(state: &AtomicU32) -> u32 {
        state.load(Ordering::Acquire)
    }

    /// A registration whose teardown reports that it ran.
    fn watching(undone: &Arc<AtomicU32>) -> Callback<AtomicU32> {
        let undone = Arc::clone(undone);
        Callback::new(Arc::new(AtomicU32::new(0)), |_context| {
            Ok::<_, ()>(move || {
                undone.fetch_add(1, Ordering::AcqRel);
            })
        })
        .expect("the registration was accepted")
    }

    #[test]
    fn dropping_a_registration_tears_it_down_once_and_then_releases_the_state() {
        let undone = Arc::new(AtomicU32::new(0));
        let watch = watching(&undone);

        assert_eq!(undone.load(Ordering::Acquire), 0, "not yet");
        drop(watch);
        assert_eq!(undone.load(Ordering::Acquire), 1, "dropping did it");
    }

    #[test]
    fn stopping_early_is_idempotent_and_drop_does_not_repeat_it() {
        let undone = Arc::new(AtomicU32::new(0));
        let mut watch = watching(&undone);

        watch.stop();
        watch.stop();
        drop(watch);
        assert_eq!(undone.load(Ordering::Acquire), 1);
    }

    #[test]
    fn a_refused_registration_gives_the_state_back_rather_than_leaking_it() {
        let state = Arc::new(AtomicU32::new(0));
        let refused = Callback::new(Arc::clone(&state), |_context| {
            Err::<fn(), _>("the API refused")
        });

        assert!(refused.is_err());
        assert_eq!(
            Arc::strong_count(&state),
            1,
            "the state is back to this test's own reference"
        );
    }

    #[test]
    fn a_trampoline_reaches_the_state_and_survives_being_used_many_times() {
        let mut context = std::ptr::null_mut();
        let watch = Callback::new(Arc::new(AtomicU32::new(0)), |ctx| {
            context = ctx;
            Ok::<_, ()>(|| {})
        })
        .expect("accepted");

        for _ in 0..1_000 {
            // A count moved here would show as a freed control block long
            // before the last iteration.
            BUMP(context);
        }
        assert_eq!(watch.state().load(Ordering::Acquire), 1_000);
    }

    #[test]
    fn a_dropped_registration_does_nothing_and_answers_what_was_declared() {
        // `outliving`, because that is the whole of what this asserts: a
        // registration that may still fire after teardown keeps its control
        // block, so the late call finds a pointer it can reconstruct and
        // upgrade to `None`. `Callback::new` reclaims instead, and calling a
        // trampoline after dropping one of those is a use-after-free rather
        // than a defined answer.
        let mut context = std::ptr::null_mut();
        let watch = Callback::outliving(Arc::new(AtomicU32::new(7)), |ctx| {
            context = ctx;
            Ok::<_, ()>(|| {})
        })
        .expect("accepted");

        assert_eq!(ANSWER(context), 7);
        drop(watch);
        assert_eq!(
            ANSWER(context),
            404,
            "a dead handler answers the declared value, never one meaning success"
        );
    }

    #[test]
    fn a_null_context_is_nothing_rather_than_a_dereference() {
        assert_eq!(ANSWER(std::ptr::null_mut()), 404);
    }

    crate::trampoline!(BUMP = bump(@ state: &AtomicU32));
    crate::trampoline!(ANSWER = answer(@ state: &AtomicU32) -> u32; dropped 404);
}

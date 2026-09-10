//! Main-thread proof: where it comes from, how it travels, and how to send
//! work to the thread it is proof of.
//!
//! **Nothing here checks a thread at runtime.** Being on the main thread is a
//! fact the compiler carries from one place, and a call that cannot show it
//! does not compile.
//!
//! It starts at [`macro@crate::main`], which mints the process's one marker in
//! `fn main` — where Rust already guarantees the thread, so there is nothing
//! to ask. From there it travels two ways:
//!
//! - **by value**, as an argument, which is what [`macro@crate::main_thread`]
//!   adds to a signature and binds in a body;
//! - **through a run loop callout**, because a source registered on the main
//!   thread is only ever called out on it.
//!
//! A [`crate::Window`] used to be the third way — built with proof and `!Send`,
//! so every method on it carried its own. It no longer does: what a method on
//! a window needs is exclusive use of the one connection, which is
//! [`crate::Connected`], acquired with [`crate::acquire`] rather than shown as
//! a type. The two that remain proof-only — making a window, and
//! `without_implicit_animations`, which touches Core Animation's own
//! main-thread-only transaction state — are the ones
//! [`macro@crate::main_thread`] still writes proof into without it being
//! spelled out:
//!
//! ```no_run
//! use objc2_core_foundation::CGRect;
//! use skylight::Window;
//!
//! #[skylight::main_thread]
//! fn open(frame: CGRect) -> skylight::Result<Window> {
//!     let window = Window::new(frame)?;              // supplied by the attribute
//!     skylight::without_implicit_animations(|| {});  // supplied too
//!     Ok(window)
//!     // Everything else on `window` takes a `skylight::Connected` — see
//!     // `skylight::connection` for why that is a lock to acquire rather
//!     // than proof this attribute could hand over.
//! }
//!
//! #[skylight::main(also(open))]
//! fn main() {
//!     // No marker written: `also(open)` supplies it, and the binding itself
//!     // has a generated name nothing can say. `pass` is how a body that
//!     // needs to *hold* it asks.
//!     let _ = open(CGRect::default());
//! }
//! ```
//!
//! A worker cannot call any of it — not because it would panic, but because it
//! has no proof to pass:
//!
//! ```compile_fail,E0277
//! #[skylight::main_thread]
//! fn twice(of: u32) -> u32 {
//!     of * 2
//! }
//!
//! twice((), 21);
//! ```
//!
//! And it costs nothing: proof is a zero-sized type. Measured with
//! `cargo run --release -p skylight --example proof_cost`, an annotated call
//! is a plain call.

use objc2::MainThreadMarker;
use std::ffi::c_void;

/// A field that keeps its struct on the main thread.
///
/// Zero-sized, and neither `Send` nor `Sync` — so a struct holding one is
/// neither either, and a value of it cannot leave the thread that built it.
/// That is the invariant [`macro@crate::MainThreadOnly`] derives an impl from,
/// and it checks for.
#[derive(Clone, Copy, Debug, Default)]
pub struct OnlyOnMain(core::marker::PhantomData<*const ()>);

impl OnlyOnMain {
    /// The one value, for a constructor that has already shown its proof.
    pub const NEW: Self = Self(core::marker::PhantomData);
}

/// Proof of the main thread, holding the run loop that belongs to it.
///
/// The obvious way to name a run loop for a CoreFoundation registration is
/// `CFRunLoop::current()`, which is the wrong way: it means "whichever thread
/// called me", so a registration made on a thread nobody pumps **fails
/// silently** — the source never fires, and the symptom is a bar that has
/// simply stopped. So the loop is read once, on the thread it belongs to
/// (only [`MainThread::new`] can), and travels by value with the proof: a
/// caller holding one of these cannot get the loop wrong.
///
/// Cheap to clone: the loop is a `CFRetained`, so a clone is one retain.
#[derive(Clone, Debug)]
pub struct MainThread {
    marker: MainThreadMarker,
    run_loop: objc2_core_foundation::CFRetained<objc2_core_foundation::CFRunLoop>,
    /// What makes this `!Send`, and so what the derive above checks.
    _main: OnlyOnMain,
}

impl MainThread {
    /// Reads the current thread's run loop and keeps it beside the proof.
    ///
    /// The one place `CFRunLoop::current()` is correct: the marker says the
    /// current thread is the main one, so the loop it returns is the main one.
    ///
    /// # Panics
    ///
    /// Panics if CoreFoundation has no run loop for this thread, which cannot
    /// happen on the main thread of a process that has one.
    #[must_use]
    pub fn new(marker: MainThreadMarker) -> Self {
        let run_loop =
            objc2_core_foundation::CFRunLoop::current().expect("the main thread has a run loop");
        Self {
            marker,
            run_loop,
            _main: OnlyOnMain::NEW,
        }
    }

    /// The same, from proof that already has a loop — or from a bare marker,
    /// in which case the loop is read once here.
    ///
    /// For anything that has to *keep* the proof rather than pass it straight
    /// on: a registry that starts sources later, a future that re-arms.
    #[must_use]
    pub fn of(proof: &impl MainThreadProof) -> Self {
        Self {
            marker: proof.marker(),
            run_loop: proof.run_loop(),
            _main: OnlyOnMain::NEW,
        }
    }

    /// The `objc2` marker, for the APIs that take one.
    #[must_use]
    pub fn marker(&self) -> MainThreadMarker {
        self.marker
    }

    /// The main thread's run loop, without asking which thread this is.
    #[must_use]
    pub fn run_loop(&self) -> &objc2_core_foundation::CFRunLoop {
        &self.run_loop
    }
}

/// The run loop, by reference.
///
/// `Deref` is spent on the marker — that is the conversion a caller wants ten
/// times as often — so the loop comes out through `AsRef` instead. Anything
/// taking `impl AsRef<CFRunLoop>` accepts a `MainThread` directly, which is
/// what `crate` and `coolabah`'s registration calls do, so a caller passes `mtm`
/// and not `mtm.run_loop()`.
impl AsRef<objc2_core_foundation::CFRunLoop> for MainThread {
    fn as_ref(&self) -> &objc2_core_foundation::CFRunLoop {
        &self.run_loop
    }
}

/// The marker, by deref — `*mtm` for an `objc2` API that takes it by value,
/// and method calls straight through.
impl std::ops::Deref for MainThread {
    type Target = MainThreadMarker;

    fn deref(&self) -> &Self::Target {
        &self.marker
    }
}

// SAFETY: `MainThread` holds an `OnlyOnMain`, so it is neither `Send` nor
// `Sync`, and its only constructor takes a marker -- it cannot exist off the
// main thread.
unsafe impl MainThreadProof for MainThread {
    fn marker(&self) -> MainThreadMarker {
        self.marker
    }

    /// The loop it already has, rather than a question about this thread.
    fn run_loop(&self) -> objc2_core_foundation::CFRetained<objc2_core_foundation::CFRunLoop> {
        self.run_loop.clone()
    }
}

/// Proof that the caller is on the thread the window server talks to.
///
/// The point of this being a trait rather than a `MainThreadMarker`
/// parameter is the error message. A missing argument gets you "this function
/// takes 2 arguments but 1 was supplied", which says nothing about threads;
/// an unsatisfied bound gets you the note below, which says what is actually
/// wrong and how to fix it.
///
/// Two things are proof today, both markers rather than values with their own
/// state: [`MainThreadMarker`], which cannot be obtained off the main thread,
/// and [`MainThread`], which wraps one with a run loop already read off it.
/// [`crate::Window`] used to be a third, but no longer is — see this module's
/// own doc comment for why that moved to a lock instead.
///
/// # Safety
///
/// Unsafe to implement, safe to use — the invariant belongs to whoever adds a
/// type to that list, not to the callers who then trust it. Implement this only
/// for a type that **cannot exist on any other thread**, and say why. `!Send`
/// plus a constructor that took proof is the argument [`MainThread`] makes; a
/// type that is merely *usually* on the main thread does not qualify, and the
/// window server calls this authorises would be reached from a worker on the
/// day it was not.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not proof that this is the main thread",
    label = "not proof of the main thread",
    note = "the window server only answers the thread its run loop turns on, so this call needs to \
            show it is there",
    note = "add `#[skylight::main_thread]` to the enclosing function, which binds `mtm` and checks \
            once on entry -- or pass something that already carries the proof, such as a \
            `skylight::MainThread`"
)]
pub unsafe trait MainThreadProof {
    /// The main thread's run loop.
    ///
    /// Defaulted, and correct as a default: holding proof is what makes
    /// `CFRunLoop::current()`'s "whichever thread called me" the main one.
    /// [`MainThread`] overrides it only for cost and honesty — it already has
    /// the loop, so it neither asks nor has an `expect` that could fire.
    ///
    /// # Panics
    ///
    /// The default panics if this thread has no run loop, which a thread
    /// holding main-thread proof cannot be.
    fn run_loop(&self) -> objc2_core_foundation::CFRetained<objc2_core_foundation::CFRunLoop> {
        objc2_core_foundation::CFRunLoop::current().expect("the main thread has a run loop")
    }

    /// The marker behind the proof.
    ///
    /// For handing on: a caller holding a [`MainThread`] can read the marker
    /// off it and pass that on its own, rather than checking the thread again
    /// for something it has already proved.
    fn marker(&self) -> MainThreadMarker;
}

// SAFETY: `MainThreadMarker` is objc2's own proof of exactly this, and is
// neither `Send` nor `Sync`.
unsafe impl MainThreadProof for MainThreadMarker {
    fn marker(&self) -> MainThreadMarker {
        *self
    }
}

// SAFETY: a reference reaches no thread its referent could not.
unsafe impl<T: MainThreadProof + ?Sized> MainThreadProof for &T {
    fn marker(&self) -> MainThreadMarker {
        (**self).marker()
    }
}

/// [`MainOnly`] disjointness tag: work that does not want the marker.
#[doc(hidden)]
pub struct TakesNothing;

/// [`MainOnly`] disjointness tag: work that wants the marker.
#[doc(hidden)]
pub struct TakesMain;

/// Work that belongs on the main thread, however it got written.
///
/// Implemented for both `FnOnce()` and `FnOnce(MainThreadMarker)`, so a
/// closure asks for the proof by writing a parameter or says nothing and is
/// not given one. `Marker` exists only to keep the two impls disjoint.
///
/// A closure is also how several arguments travel: capture them.
///
/// ```ignore
/// let frame = bar_frame();
/// (move |mtm: MainThreadMarker| Window::new(frame, mtm)).now();
/// ```
pub trait MainOnly<Marker> {
    /// What the work produces.
    type Output;

    /// Runs it, given proof.
    fn run(self, mtm: MainThreadMarker) -> Self::Output;

    /// Sends it to the main thread and returns immediately.
    ///
    /// The one way to reach the main thread from somewhere else. It does not
    /// wait, and there is deliberately no variant that does: a worker parked
    /// on the thread that composites is the failure this whole arrangement
    /// exists to prevent. If the answer is needed back, send it the way
    /// everything else does — a channel, and a wake.
    ///
    /// Queued on the main dispatch queue, which the main run loop services.
    /// Called from the main thread it still queues rather than running
    /// inline, so it cannot re-enter whatever is already running there.
    ///
    /// ```
    /// use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
    /// use skylight::MainOnly;
    /// use std::sync::atomic::{AtomicBool, Ordering};
    ///
    /// static LANDED: AtomicBool = AtomicBool::new(false);
    ///
    /// // A worker cannot do main-thread work, but it can send some.
    /// std::thread::spawn(|| {
    ///     (|| LANDED.store(true, Ordering::Release)).dispatch();
    /// })
    /// .join()
    /// .unwrap();
    ///
    /// // Nothing runs until the main thread turns its run loop, which is the
    /// // whole point: the hand-over waits for the main thread to be free.
    /// let mode = unsafe { kCFRunLoopDefaultMode };
    /// for _ in 0..100 {
    ///     if LANDED.load(Ordering::Acquire) {
    ///         break;
    ///     }
    ///     CFRunLoop::run_in_mode(mode, 0.05, true);
    /// }
    /// assert!(LANDED.load(Ordering::Acquire), "the work never reached the main thread");
    /// ```
    fn dispatch(self)
    where
        Self: Sized + Send + 'static,
    {
        dispatch_to_main(move |mtm| {
            self.run(mtm);
        });
    }
}

impl<R, F: FnOnce() -> R> MainOnly<TakesNothing> for F {
    type Output = R;

    fn run(self, _mtm: MainThreadMarker) -> R {
        self()
    }
}

impl<R, F: FnOnce(MainThreadMarker) -> R> MainOnly<TakesMain> for F {
    type Output = R;

    fn run(self, mtm: MainThreadMarker) -> R {
        self(mtm)
    }
}

// `dispatch_get_main_queue()` is an inline function in C; what it returns is
// the address of this symbol. Taking it is the documented way to name the main
// queue from a language without the header.
unsafe extern "C" {
    static _dispatch_main_q: c_void;
    fn dispatch_async_f(
        queue: *mut c_void,
        context: *mut c_void,
        work: unsafe extern "C-unwind" fn(*mut c_void),
    );
}

/// Queues `f` on the main thread's dispatch queue.
fn dispatch_to_main(f: impl FnOnce(MainThreadMarker) + Send + 'static) {
    unsafe extern "C-unwind" fn run(context: *mut c_void) {
        // SAFETY: the box `dispatch_to_main` leaked, handed back exactly once
        // -- libdispatch runs a queued function one time and then forgets it.
        let f = unsafe { Box::from_raw(context.cast::<Boxed>()) };
        // SAFETY: this is the main queue's own thread, which is the main
        // thread; the marker is being carried across a C boundary that cannot
        // hold it, not invented.
        f(unsafe { MainThreadMarker::new_unchecked() });
    }

    type Boxed = Box<dyn FnOnce(MainThreadMarker) + Send>;
    let boxed: Boxed = Box::new(f);
    let context = Box::into_raw(Box::new(boxed)).cast::<c_void>();

    // SAFETY: `_dispatch_main_q` is a live process-wide object for the life of
    // the process, `context` is the box above and is reclaimed by `run`, and
    // `run` matches the callback type.
    unsafe {
        dispatch_async_f(
            std::ptr::addr_of!(_dispatch_main_q).cast_mut().cast(),
            context,
            run,
        );
    }
}

/// What the macros refuse, as tests.
///
/// These live here rather than beside the macros, because `skylight-macros` is
/// a proc-macro crate and cannot depend on the crate whose types its output
/// names. `compile_fail` doctests are the closest thing to a unit test for "the
/// compiler must reject this", and rejecting these is the whole point of the
/// arrangement — so they are checked rather than described.
///
/// # A worker cannot call a main-thread function
///
/// Not because it panics — because it has nothing to pass:
///
/// ```compile_fail,E0277
/// #[skylight::main_thread]
/// fn only_here() {}
///
/// std::thread::spawn(|| only_here(())).join().unwrap();
/// ```
///
/// # A worker cannot be given the proof either
///
/// [`MainThread`] holds a retained run loop and an [`OnlyOnMain`], so it is not
/// `Send` and the closure cannot capture it:
///
/// ```compile_fail,E0277
/// #[skylight::main_thread]
/// fn only_here() {}
///
/// #[skylight::main(pass)]
/// fn main() {
///     let proof = mtm;
///     std::thread::spawn(move || only_here(&proof)).join().unwrap();
/// }
/// ```
///
/// Nor a bare marker, for the same reason one level down. Note the binding:
/// `let _ = mtm` would not capture it at all — a discard pattern is not a use —
/// so the test has to actually keep it, which is the honest shape anyway:
///
/// ```compile_fail,E0277
/// fn main() {
///     let mtm = objc2::MainThreadMarker::new().unwrap();
///     std::thread::spawn(move || {
///         let _kept = mtm;
///     });
/// }
/// ```
///
/// # And the generated binding cannot be named
///
/// Without `pass`, there is no `mtm` in scope to smuggle anywhere:
///
/// ```compile_fail,E0425
/// #[skylight::main]
/// fn main() {
///     let _ = mtm;
/// }
/// ```
///
/// # What a worker can do
///
/// Send the work to the thread that may do it. This one runs:
///
/// ```
/// use skylight::MainOnly;
/// use std::sync::atomic::{AtomicBool, Ordering};
///
/// #[skylight::main_thread]
/// fn only_here(flag: &'static AtomicBool) {
///     flag.store(true, Ordering::Release);
/// }
///
/// static RAN: AtomicBool = AtomicBool::new(false);
///
/// std::thread::spawn(|| {
///     // The closure takes the proof the main thread will have, so the call is
///     // legal there and nowhere else.
///     (|mtm: objc2::MainThreadMarker| only_here(mtm, &RAN)).dispatch();
/// })
/// .join()
/// .unwrap();
///
/// let mode = unsafe { objc2_core_foundation::kCFRunLoopDefaultMode };
/// for _ in 0..100 {
///     if RAN.load(Ordering::Acquire) {
///         break;
///     }
///     objc2_core_foundation::CFRunLoop::run_in_mode(mode, 0.05, true);
/// }
/// assert!(RAN.load(Ordering::Acquire), "the work never reached the main thread");
/// ```
pub mod refusals {}

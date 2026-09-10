//! The Accessibility API, as far as this crate wraps it.
//!
//! Here rather than in a caller for the same reason the window server calls
//! are: it is a C API that reports failure as an opaque code, needs a
//! permission the user grants out of band, and hands back objects with no
//! static type. All three are this crate's job, and all three report through
//! the one [`crate::Error`] the rest of it uses -- an Accessibility failure
//! and a window server failure reach a caller the same way.
//!
//! What is *not* here is any policy about which elements matter. Finding the
//! status item behind a particular menu bar window, or deciding how far an
//! element's frame may sit from a window's before they are the same thing, is
//! a caller's business.
//!
//! The grant is a value, not a convention: [`Trusted`] is the only way to
//! obtain an `AXUIElement` through this module, and the only way to obtain
//! one of those is to have asked the system and been told yes. See its own
//! doc for why the proof is demanded where elements enter rather than at
//! every read.

use crate::callback::Callback;
use crate::cf::FromCF;
use crate::error::{Error, Result};
use objc2_app_kit::NSWorkspace;
use objc2_application_services::{
    AXError, AXIsProcessTrusted, AXIsProcessTrustedWithOptions, AXObserver, AXUIElement,
    kAXTrustedCheckOptionPrompt,
};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFRetained, CFRunLoopSource, CFString, CFType, CGPoint,
    CGRect, CGSize, Type,
};
use std::ptr::NonNull;
use std::time::Duration;

/// Turns the Accessibility API's success-or-code convention into a `Result`.
fn ok(err: AXError) -> std::result::Result<(), AXError> {
    if err == AXError::Success {
        Ok(())
    } else {
        Err(err)
    }
}

/// Whether this process may read other applications' Accessibility trees.
///
/// Every call below that needs the grant checks this first, so a missing one
/// is [`Error::NotTrusted`] and not a wrong-looking empty answer. Without it
/// the whole API succeeds while reporting that every application has no menu
/// bar and no status items, which is indistinguishable from the truth on a
/// machine that really has none.
///
/// # Errors
///
/// [`Error::NotTrusted`] if the grant is missing. [`request`] asks for it.
pub fn trusted() -> Result<()> {
    // SAFETY: no arguments.
    if unsafe { AXIsProcessTrusted() } {
        Ok(())
    } else {
        Err(Error::NotTrusted)
    }
}

/// Proof that this process may read other applications' Accessibility trees.
///
/// The only way to hold one is to have asked and been told yes, and every
/// call below that hands back an `AXUIElement` -- [`application`],
/// [`menu_bar_children`], [`extras_menu_children_within`], [`Observer`] --
/// wants one first. Gating the *acquisition* of elements is what makes gating
/// each individual read unnecessary: an `AXUIElement` is itself the
/// capability, so the proof is demanded once, where an element enters the
/// program, rather than restated at each of the dozen reads that follow.
///
/// A zero-sized `Copy` witness, so passing it costs nothing and a worker
/// thread can carry it -- the grant is a fact about the process, not about
/// the thread that noticed it.
///
/// It is proof of a past check, not a lease. System Settings can revoke the
/// grant while one is held, and then every call made through it starts
/// answering `kAXErrorAPIDisabled` -- which reaches a caller as the same
/// `None` an application with nothing to say produces. That rules out
/// *forgetting* to check, not the user changing their mind mid-walk.
/// [`Trust`] is what notices the change and says so.
#[derive(Debug, Clone, Copy)]
pub struct Trusted(());

impl Trusted {
    /// Asks the system, and hands back the proof if the answer is yes.
    ///
    /// # Errors
    ///
    /// [`Error::NotTrusted`] if the grant is missing. [`request`] asks the
    /// user for it; [`Trust`] watches for it arriving later.
    pub fn get() -> Result<Self> {
        trusted().map(|()| Self(()))
    }

    /// The same, for a caller that treats an absent grant as an empty answer
    /// rather than a failure.
    #[must_use]
    pub fn now() -> Option<Self> {
        Self::get().ok()
    }
}

/// Whether the grant is not merely recorded but actually usable.
///
/// [`trusted`] reads the database; the API itself is a separate question. A
/// process that was already running when the grant landed can be trusted and
/// still have every call answered `kAXErrorAPIDisabled` -- which is the whole
/// truth behind the system prompt telling the user to restart the
/// application. This asks the API instead of the database: one real attribute
/// read against another process, whose `APIDisabled` is the exact code that
/// means "this image has to be replaced".
///
/// # Errors
///
/// [`Error::NotTrusted`] when the grant is simply absent, and
/// [`Error::ApiDisabled`] when it is there but this process cannot use it.
/// [`Error::NoFrontApp`] when there is no other process to ask against, which
/// says nothing either way.
#[crate::main_thread]
pub fn usable() -> Result<()> {
    let access = Trusted::get()?;
    let (pid, _) = frontmost_application(proof)?;
    let app = application(access, pid);
    let attr = CFString::from_str("AXRole");
    let mut value: *const CFType = std::ptr::null();
    // SAFETY: `attr` is live for the call and `value` is a valid out-pointer.
    let err = unsafe { app.copy_attribute_value(&attr, NonNull::from(&mut value)) };
    if !value.is_null() {
        // SAFETY: a non-null result carries a +1 reference, dropped here --
        // only the error code is wanted.
        drop(unsafe { CFRetained::<CFType>::from_raw(NonNull::new_unchecked(value.cast_mut())) });
    }
    if err == AXError::APIDisabled {
        return Err(Error::ApiDisabled);
    }
    Ok(())
}

/// Prompts the user for Accessibility access if it is not already granted,
/// answering whether it is granted now.
///
/// The prompt is the system's own, and the answer arrives long after this
/// returns: a user who grants it is granting it to a process that has already
/// been told no.
#[must_use]
pub fn request() -> bool {
    // SAFETY: reads a `'static` extern constant.
    let key = unsafe { kAXTrustedCheckOptionPrompt };
    let options = CFDictionary::from_slices(&[key], &[CFBoolean::new(true)]);
    // SAFETY: `options` maps the documented prompt key to a `CFBoolean`,
    // exactly as `AXIsProcessTrustedWithOptions` expects.
    unsafe { AXIsProcessTrustedWithOptions(Some(options.as_opaque())) }
}

/// The Accessibility grant, watched for as long as this is held.
///
/// The system's own prompt says the application must be restarted, and for an
/// application that reads the grant once at launch it must. It need not be:
/// `AXIsProcessTrusted` re-reads the TCC database on every call, so a grant
/// made minutes after this process started is visible to the same process
/// immediately. Polling this on a timer is what turns "quit and reopen" into
/// "the bar starts resolving owners a moment after you tick the box".
///
/// Prompting is separate from polling, and happens at most once: the prompt
/// is modal-ish and a second one after the user has already dismissed it is
/// noise.
#[derive(Debug, Default)]
pub struct Trust {
    granted: bool,
    prompted: bool,
}

impl Trust {
    /// Reads the grant once, up front.
    #[must_use]
    pub fn new() -> Self {
        Self {
            granted: trusted().is_ok(),
            prompted: false,
        }
    }

    /// Whether the grant was there as of the last [`Trust::poll`].
    #[must_use]
    pub const fn granted(&self) -> bool {
        self.granted
    }

    /// The proof, re-read now rather than as of the last [`Trust::poll`].
    ///
    /// The grant can be revoked from System Settings at any moment, so a
    /// watcher's cached answer is a report and this is a permission.
    #[must_use]
    pub fn access(&self) -> Option<Trusted> {
        Trusted::now()
    }

    /// Asks the user for the grant, the first time this is called on an
    /// untrusted process. Later calls do nothing, so this is safe to call
    /// from the same failure path every time it is hit.
    pub fn prompt(&mut self) {
        if self.granted || self.prompted {
            return;
        }
        self.prompted = true;
        self.granted = request();
    }

    /// Re-reads the grant, answering `Some` only when it changed.
    ///
    /// `Some(true)` is a caller's cue to redo whatever it gave up on while
    /// untrusted; `Some(false)` means the user revoked it, which System
    /// Settings allows at any time.
    pub fn poll(&mut self) -> Option<bool> {
        let now = trusted().is_ok();
        (now != self.granted).then(|| {
            self.granted = now;
            now
        })
    }
}

/// One application's Accessibility element, the root everything else about it
/// hangs off.
///
/// A dead pid is not an error here -- the element is created regardless, and
/// every attribute read through it simply answers nothing.
///
/// Takes the grant because this is where an element enters the program: an
/// untrusted process gets `kAXErrorAPIDisabled` from every read through it,
/// which is indistinguishable from an application that publishes nothing.
#[must_use]
pub fn application(_access: Trusted, pid: i32) -> CFRetained<AXUIElement> {
    // SAFETY: `AXUIElementCreateApplication` accepts any pid and always
    // returns a +1 element.
    unsafe { AXUIElement::new_application(pid) }
}

/// Which process an element belongs to.
///
/// `None` when Accessibility will not say, which is what a stale element
/// looks like -- one whose application has quit since it was handed over.
#[must_use]
pub fn pid_of(element: &AXUIElement) -> Option<i32> {
    let mut pid: i32 = 0;
    // SAFETY: `pid` is a valid out-pointer.
    let status = unsafe { element.pid(NonNull::from(&mut pid)) };
    ok(status).ok().map(|()| pid)
}

/// The attribute `name` of `element`, if it is there and is a `T`.
///
/// An absent attribute is `None`, not an error: an application publishing no
/// `AXTitle` is ordinary, and most callers here read several names in turn
/// and take the first that answers.
///
/// The type is asked for rather than returned untyped, because an attribute's
/// name says nothing about what the application actually put under it. Every
/// caller has to check anyway, and one that forgot would be holding a
/// `CFRetained<CFType>` it could only guess about; naming `T` makes the guess
/// the caller's stated expectation and a wrong one a `None`.
pub fn attribute<T: FromCF>(element: &AXUIElement, name: &str) -> Option<T> {
    let attr = CFString::from_str(name);
    let mut value: *const CFType = std::ptr::null();
    // SAFETY: `attr` is a live `CFString` for the call's duration, and
    // `value` is a valid out-pointer.
    let err = unsafe { element.copy_attribute_value(&attr, NonNull::from(&mut value)) };
    ok(err).ok()?;
    let ptr = NonNull::new(value.cast_mut())?;
    // SAFETY: a non-null result from `AXUIElementCopyAttributeValue` carries
    // a +1 reference, which transfers to `CFRetained`.
    let value = unsafe { CFRetained::<CFType>::from_raw(ptr) };
    T::from_cf(&value)
}

/// The first of `names` this element answers.
///
/// `AXVisibleChildren` then `AXChildren` is the pairing this exists for: the
/// visible set is the one a caller wants and the one plenty of applications
/// do not publish.
pub fn first_attribute<T: FromCF>(element: &AXUIElement, names: &[&str]) -> Option<T> {
    names.iter().find_map(|name| attribute(element, name))
}

/// An element's children, visible ones for preference.
pub fn children(element: &AXUIElement) -> Option<CFRetained<CFArray>> {
    first_attribute(element, &["AXVisibleChildren", "AXChildren"])
}

/// One element of an Accessibility array, if it is there and is an element.
pub fn array_element(array: &CFArray, i: isize) -> Option<&AXUIElement> {
    crate::cf::Array::new(array).get::<AXUIElement>(i)
}

/// An element's on-screen rect.
///
/// `AXFrame` when the application publishes one, and otherwise the
/// `AXPosition`/`AXSize` pair that predates it -- plenty of elements still
/// only answer the older two.
pub fn frame(element: &AXUIElement) -> Option<CGRect> {
    if let Some(frame) = attribute::<CGRect>(element, "AXFrame") {
        return Some(frame);
    }
    let position = attribute::<CGPoint>(element, "AXPosition")?;
    let size = attribute::<CGSize>(element, "AXSize")?;
    Some(CGRect::new(position, size))
}

/// `AXCancel` then `AXPress`, exactly the sequence `SketchyBar`'s own menu
/// helper performs -- the cancel first dismisses any menu already open, so
/// the press reliably opens this one rather than sometimes toggling it shut.
///
/// # Errors
///
/// [`Error::Action`] if the element itself refused the press. The grant is
/// already proven by the caller's [`Trusted`], which is what it took to
/// obtain `element` in the first place.
pub fn press(_access: Trusted, element: &AXUIElement) -> Result<()> {
    let cancel = CFString::from_str("AXCancel");
    // SAFETY: `cancel` is a live `CFString` for the call's duration. Whether
    // there was anything to cancel is not interesting.
    let _ = unsafe { element.perform_action(&cancel) };
    std::thread::sleep(Duration::from_millis(1));
    let press = CFString::from_str("AXPress");
    // SAFETY: `press` is a live `CFString` for the call's duration.
    ok(unsafe { element.perform_action(&press) }).map_err(Error::Action)
}

/// The frontmost application's pid and name.
///
/// # Errors
///
/// [`Error::NoFrontApp`] when nothing is frontmost, which includes the moment
/// after the last window of the last application closes.
#[crate::main_thread]
pub fn frontmost_application() -> Result<(i32, String)> {
    let app = NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .ok_or(Error::NoFrontApp)?;
    let pid = app.processIdentifier();
    if pid <= 0 {
        return Err(Error::NoFrontApp);
    }
    let name = app
        .localizedName()
        .map_or_else(|| pid.to_string(), |name| name.to_string());
    Ok((pid, name))
}

/// One application's own top-level menu bar (`AXMenuBar`) -- the Apple menu
/// and its File/Edit/View/... titles -- as opposed to `AXExtrasMenuBar`, its
/// status items, which is a different attribute entirely.
///
/// # Errors
///
/// [`Error::NoMenuBar`] when the application genuinely publishes none.
pub fn menu_bar_children(access: Trusted, pid: i32) -> Result<CFRetained<CFArray>> {
    let app = application(access, pid);
    let menu_bar =
        attribute::<CFRetained<AXUIElement>>(&app, "AXMenuBar").ok_or(Error::NoMenuBar)?;
    children(&menu_bar).ok_or(Error::NoMenuBar)
}

/// How long a cross-process Accessibility call on `element` may take before
/// it is abandoned, in place of the API's own default.
///
/// That default is around six seconds, per element, and every read here is a
/// synchronous Mach round trip into another process: one application that has
/// stopped answering costs the caller six seconds of nothing. A short timeout
/// turns "hung" into "absent", which is a state every caller here already
/// handles -- an application that publishes no status items and one that
/// declines to say are the same answer.
///
/// Set on an application element, it applies to every element obtained
/// through it, so one call per application covers the whole walk.
///
/// No restore, and none needed: every caller here sets it on an application
/// element it created moments earlier and drops moments later, so nothing
/// outlives it to be left mis-configured. The one call that *would* need
/// undoing is on the system-wide element, which changes the default for the
/// whole process; nothing here makes it.
pub fn set_messaging_timeout(element: &AXUIElement, timeout: Duration) {
    // A timeout is a `float` of seconds; menu bar work is measured in
    // milliseconds, so the precision loss is far below anything meaningful.
    #[allow(clippy::cast_possible_truncation)]
    let seconds = timeout.as_secs_f32();
    // SAFETY: `element` is live for the call, and any non-negative timeout is
    // accepted -- zero would mean "back to the default", which is why callers
    // pass a real duration.
    let _ = unsafe { element.set_messaging_timeout(seconds) };
}

/// One application's `AXExtrasMenuBar` element -- the container its status
/// items hang off.
///
/// Wanted in its own right, not only as a step towards [`children`]: an
/// `AXObserver` registered on the container hears about its items, which a
/// registration on the application element does not reliably do.
///
/// `timeout` bounds every call made through this application's element (see
/// [`set_messaging_timeout`]). `None` for an application that publishes no
/// status items, which is the common case, and for one that did not answer
/// within `timeout`.
#[must_use]
pub fn extras_menu_bar(
    access: Trusted,
    pid: i32,
    timeout: Duration,
) -> Option<CFRetained<AXUIElement>> {
    let app = application(access, pid);
    set_messaging_timeout(&app, timeout);
    attribute::<CFRetained<AXUIElement>>(&app, "AXExtrasMenuBar")
}

/// An application with no status items is `None`, which is the common case --
/// most applications have none, and so does one that did not answer within
/// `timeout`.
#[must_use]
pub fn extras_menu_children_within(
    access: Trusted,
    pid: i32,
    timeout: Duration,
) -> Option<CFRetained<CFArray>> {
    let extras = extras_menu_bar(access, pid, timeout)?;
    children(&extras)
}

/// The same, under the Accessibility API's own default timeout -- around six
/// seconds, per element.
///
/// Nothing in the daemon calls this: a walk of every running application
/// under that default measured 2.6 s here, of which one unresponsive
/// application was 1.5 s. It is kept as the *baseline* that number is
/// measured against — `coolabah/examples/alias_probe.rs` times both forms side
/// by side, and an argument for a timeout that cannot be re-run is an
/// argument nobody can check.
#[must_use]
pub fn extras_menu_children(access: Trusted, pid: i32) -> Option<CFRetained<CFArray>> {
    let app = application(access, pid);
    let extras = attribute::<CFRetained<AXUIElement>>(&app, "AXExtrasMenuBar")?;
    children(&extras)
}

/// What an observer's callback is handed.
///
/// The element and the notification are borrowed for the call only —
/// Accessibility owns them and they are gone when it returns — while the
/// context is the caller's own, alive for as long as the observer is.
///
/// `matched` is what [`Observer::watch_matched`] decided about *this*
/// registration's element, back when it was registered — `()` for a plain
/// [`Observer::watch`], which has nothing to say.
pub struct Notified<'a, T, M = ()> {
    pub element: &'a AXUIElement,
    pub notification: &'a CFString,
    pub context: &'a T,
    pub matched: &'a M,
}

/// One registration's own payload, at the address the framework is handed.
///
/// `AXObserverAddNotification` takes a `void*` *per registration*, not one
/// for the whole observer — this is what makes that pointer typed and exact
/// rather than a cast a caller writes by hand. `context` and `on` are the
/// same for every registration on one [`Observer`] (cloned in, not shared by
/// reference, so each registration's payload is self-contained); `matched`
/// is the one thing that differs per element, decided once here instead of
/// guessed at every delivery.
struct Registered<T, M> {
    context: std::sync::Arc<T>,
    on: fn(Notified<'_, T, M>),
    matched: M,
}

/// The C callback, one per `(T, M)`, recovering the registration the caller
/// made.
///
/// `extern "C-unwind"` because a panic crossing back into Accessibility is
/// undefined either way; unwinding at least aborts with the panic's message
/// rather than corrupting the frame silently.
extern "C-unwind" fn deliver<T, M>(
    _observer: NonNull<AXObserver>,
    element: NonNull<AXUIElement>,
    notification: NonNull<CFString>,
    refcon: *mut std::ffi::c_void,
) {
    // SAFETY: both are live for the duration of the callback.
    let (element, notification) = unsafe { (element.as_ref(), notification.as_ref()) };
    // SAFETY: the refcon is the never-reclaimed `Weak<Registered<T, M>>` that
    // `Observer::<T, M>::watch`/`watch_matched` made for this exact
    // registration, and its `(T, M)` is this `(T, M)` because only those
    // methods, on an observer created with `deliver::<T, M>`, register it.
    // `with` does the reference rules and answers `None` once the
    // registration has gone -- which is what makes a delivery already in
    // flight safe while another thread drops it.
    let _ = unsafe {
        Callback::<Registered<T, M>>::with(refcon, |registered| {
            (registered.on)(Notified {
                element,
                notification,
                context: &registered.context,
                matched: &registered.matched,
            });
        })
    };
}

/// One application's Accessibility observer, and what it was registered to
/// tell.
///
/// The point of the type parameters is the `void*`. `AXObserverAddNotification`
/// takes a raw context pointer, which is how a callback reaches anything at
/// all, and every hand-written use of it is a cast out and a cast back with
/// nothing checking the two agree. Here the context is given once, at
/// creation, and arrives at the handler as a `&T` — the cast happens in one
/// place, for one type, and a caller never writes either half.
///
/// `M` is per-registration rather than per-observer: [`Observer::watch_matched`]
/// gives each element its own refcon, so the callback recovers exactly what
/// that element was decided to be at registration time and never has to
/// guess again. Plain [`Observer::watch`] is `M = ()`, the shape this had
/// before there was anything per-element to say.
///
/// Dropping this removes every notification it registered and releases the
/// observer, which is what takes its run loop source with it.
pub struct Observer<T, M = ()> {
    /// One [`Callback`] per registration — each owns its own teardown
    /// (`AXObserverRemoveNotification`) and its own payload, released in
    /// that order by `Callback`'s own `Drop`. Nothing here has to sequence
    /// that by hand.
    watching: Vec<Callback<Registered<T, M>>>,
    handle: CFRetained<AXObserver>,
    /// The canonical `T`, cloned into every [`Registered`] so a callback
    /// never borrows through this directly.
    context: std::sync::Arc<T>,
    on: fn(Notified<'_, T, M>),
}

impl<T, M> Observer<T, M> {
    /// Creates an observer for `pid`, whose notifications reach `on` with
    /// `context`.
    ///
    /// Nothing is observed yet — [`Observer::watch`] and
    /// [`Observer::watch_matched`] say what. The observer delivers on
    /// whatever run loop [`Observer::run_loop_source`] is added to, so a
    /// caller that never adds it hears nothing.
    ///
    /// # Errors
    ///
    /// [`Error::CreateObserver`] when the framework refuses.
    pub fn create(
        _access: Trusted,
        pid: i32,
        context: T,
        on: fn(Notified<'_, T, M>),
    ) -> Result<Self> {
        let mut out: *mut AXObserver = std::ptr::null_mut();
        // SAFETY: `out` is a valid out-pointer, and `deliver::<T, M>` has
        // `AXObserverCallback`'s signature — a safe `extern "C-unwind" fn`
        // coerces to the unsafe function-pointer type the slot expects.
        let status =
            unsafe { AXObserver::create(pid, Some(deliver::<T, M>), NonNull::from(&mut out)) };
        ok(status).map_err(Error::CreateObserver)?;
        let out = NonNull::new(out).ok_or(Error::CreateObserver(status))?;
        // SAFETY: a non-null result from `AXObserverCreate` carries a +1
        // reference, which transfers to `CFRetained`.
        let observer = unsafe { CFRetained::from_raw(out) };
        Ok(Self {
            watching: Vec::new(),
            handle: observer,
            context: std::sync::Arc::new(context),
            on,
        })
    }

    /// Asks `element` to report `notification`, tagging this registration
    /// with `matched` — recovered exactly as given, in [`Notified::matched`],
    /// whenever this element reports.
    ///
    /// For a caller that has already worked out, once, what (if anything)
    /// this element is; see [`Observer::watch`] for one that has not.
    ///
    /// Takes `matched` by value rather than asking for an `Arc` already made:
    /// the `Arc<Registered<T, M>>` this wraps it in is what the refcon has to
    /// be, and building it here — rather than trusting one handed in — is
    /// what lets a refused registration hand `matched` back unleaked, through
    /// [`Callback::new`]'s own refusal path.
    ///
    /// # Errors
    ///
    /// [`Error::Notification`] if the application refuses this one, which it
    /// may do per notification — a caller asking for several is expected to
    /// tolerate some being declined.
    pub fn watch_matched(
        &mut self,
        element: &AXUIElement,
        notification: &str,
        matched: M,
    ) -> Result<()> {
        let name = CFString::from_str(notification);
        let registered = std::sync::Arc::new(Registered {
            context: std::sync::Arc::clone(&self.context),
            on: self.on,
            matched,
        });
        let handle = self.handle.retain();
        let target = element.retain();
        let callback = Callback::new(registered, move |refcon| {
            // SAFETY: `target` and `name` are live for the call, and `refcon`
            // is the `Weak` this `Callback` manages — valid to reconstruct
            // for as long as the process lives, whether or not the state
            // behind it is still there.
            let status = unsafe { handle.add_notification(&target, &name, refcon) };
            ok(status).map_err(Error::Notification)?;
            Ok(move || {
                // SAFETY: the same pair, registered just above and removed
                // exactly once, whether from here or from `Observer::drop`.
                let _ = unsafe { handle.remove_notification(&target, &name) };
            })
        })?;
        self.watching.push(callback);
        Ok(())
    }

    /// The source to add to a run loop, which is what makes it deliver.
    #[must_use]
    pub fn run_loop_source(&self) -> CFRetained<CFRunLoopSource> {
        // SAFETY: the observer outlives the source it hands back, and this is
        // exactly what `AXObserverGetRunLoopSource` is for.
        unsafe { self.handle.run_loop_source() }
    }

    /// How many notifications are actually registered.
    #[must_use]
    pub fn watching(&self) -> usize {
        self.watching.len()
    }

    /// The context the callback sees, for a caller that also needs it.
    #[must_use]
    pub fn context(&self) -> &T {
        &self.context
    }
}

impl<T> Observer<T, ()> {
    /// Asks `element` to report `notification`, with nothing to tag it —
    /// [`Observer::watch_matched`] with `matched: ()`. The shape this had
    /// before a registration could name what it was for.
    ///
    /// # Errors
    ///
    /// [`Error::Notification`] if the application refuses this one, which it
    /// may do per notification — a caller asking for several is expected to
    /// tolerate some being declined.
    pub fn watch(&mut self, element: &AXUIElement, notification: &str) -> Result<()> {
        self.watch_matched(element, notification, ())
    }
}

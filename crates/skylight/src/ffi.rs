//! Raw declarations for the private window server API.
//!
//! Signatures are cross-checked against three independent reverse-engineerings:
//! yabai's `src/misc/extern.h`, `SketchyBar`'s `src/misc/extern.h`, and
//! `hs._asm.undocumented.spaces`. Nothing here is in a public SDK header, so
//! every signature is a claim about an ABI Apple may change.

use crate::WindowTags;
use objc2_core_foundation::{CFArray, CFNumber, CFString, CFType, CGRect};
use objc2_core_graphics::{CGContext, CGDirectDisplayID, CGError, CGImage};
use std::ffi::{c_int, c_void};

/// A window server connection. `int` on the C side; the width matters because
/// it is passed by value in the first argument register.
///
/// A plain number, `Send` and `Copy`. It used to carry [`crate::OnlyOnMain`]
/// and to be documented as "main-thread-only, and so is every call that takes
/// one" — which was never the platform's rule. The window server answers any
/// thread. What a connection actually needs is *exclusive* use, and for a
/// reason thread-affinity does not describe: `SLSDisableUpdate` and
/// `SLSReenableUpdate` bracket a whole batch of window changes, so a second
/// thread re-enabling compositing in the middle of one breaks it. Sequences
/// have to be atomic; individual calls do not have to be on any particular
/// thread.
///
/// So exclusivity is a lock, not a marker: [`crate::Connected`] is the proof
/// that this connection is yours for the moment, and every safe wrapper takes
/// one. This raw id is what those wrappers pass on to the `unsafe` calls
/// below.
///
/// `repr(transparent)` is load-bearing: this has to lower to exactly the `int`
/// the window server takes by value.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(c_int);

impl ConnectionId {
    /// Wraps a raw number. For tests, which call trampolines the way the
    /// window server does and have to supply the argument it passes.
    #[cfg(test)]
    pub(crate) const fn new(id: c_int) -> Self {
        Self(id)
    }

    /// Wraps a raw number the window server itself handed back as a
    /// connection id — [`crate::sys::window::SLSGetWindowOwner`]'s out
    /// parameter, which names another window's owner rather than this
    /// process's own connection. That id is not exclusive-use in the sense
    /// [`crate::Connected`] is; it is only ever a value to pass on to a
    /// further read, such as `SLSConnectionGetPID`.
    #[must_use]
    pub const fn from_raw(id: c_int) -> Self {
        Self(id)
    }
}

/// A window server window number — the same value `NSWindow.windowNumber` and
/// `kCGWindowNumber` report.
pub type WindowId = u32;

/// A mission control space. Opaque; only equality and lookup are meaningful.
pub type SpaceId = u64;

/// Width of the tag bitset every tag call takes, in **bits**, not bytes.
/// Passing 8 here is the classic way to silently set the wrong tags.
///
/// Derived from [`WindowTags`] rather than written as 64, so the two cannot
/// disagree. The assertion is the conversion's safety net: `TryFrom` is not a
/// const trait yet, and a bitset wider than a `c_int` can name would be a
/// nonsense the window server should never be handed.
pub const TAG_BITS: c_int = {
    const BITS: usize = size_of::<WindowTags>() * 8;
    assert!(
        BITS <= c_int::MAX as usize,
        "the tag bitset outgrew a C int"
    );
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "the assertion above is the check"
    )]
    {
        BITS as c_int
    }
};

/// `SLSOrderWindow` modes.
pub const ORDER_BELOW: c_int = -1;
pub const ORDER_OUT: c_int = 0;
pub const ORDER_ABOVE: c_int = 1;

bitflags::bitflags! {
    /// What kind of space `SLSSpaceGetType` reports.
    ///
    /// The values are bits rather than an enumeration: an ordinary user space
    /// is none of them set, and the two the window server distinguishes sit a
    /// bit apart. Held as flags so an unknown bit from a future release reads
    /// as "not a kind we know" rather than falling out of a `match` — the
    /// question every caller here asks is `contains(FULLSCREEN)`, not "which
    /// of exactly three".
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    #[repr(transparent)]
    pub struct SpaceType: c_int {
        /// A system space -- Dashboard, and whatever else Apple puts there.
        ///
        const SYSTEM = 2;
        /// A space holding one fullscreen application.
        const FULLSCREEN = 4;
    }
}

impl SpaceType {
    /// An ordinary space: no bit set.
    pub const USER: Self = Self::empty();

    /// Whether this is a space a bar should stay out of the way of.
    #[must_use]
    pub const fn is_fullscreen(self) -> bool {
        self.contains(Self::FULLSCREEN)
    }
}

/// The callback shape every `SLSRegister*NotifyProc` takes.
///
/// `C-unwind` because a panic crossing back into the window server through a
/// plain `extern "C"` frame is undefined behaviour. `unsafe` because calling
/// one by hand carries a contract the window server keeps for you: the context
/// pointer has to be the one its registration was given, of the type the
/// handler expects. [`NotifyProcedure`] is how one is produced without having
/// to think about either.
pub type NotifyProc = unsafe extern "C-unwind" fn(
    event: u32,
    data: *mut c_void,
    len: usize,
    context: *mut c_void,
    cid: ConnectionId,
);

/// One notification, as the window server delivered it.
///
/// The raw callback's arguments as one value, with the pointer-and-length pair
/// already a slice.
#[derive(Clone, Copy, Debug)]
pub struct Event<'a> {
    /// Which notification this is.
    pub id: u32,
    /// The payload, empty when there is none.
    pub payload: &'a [u8],
    /// The connection it came in on.
    ///
    /// A [`ConnectionId`], not a number: the window server calls back on the
    /// thread that registered, which is the main thread — so this *is* proof of
    /// it, and a handler can pass it straight to the calls that need one.
    pub connection: ConnectionId,
}

/// How many notifications every trampoline has taken.
///
/// One counter for the process. Delivery is what goes quiet — a notification
/// the window server stops sending looks exactly like one nothing subscribed
/// to, and [`SLSRemoveNotifyProc`] reports success either way — so a number
/// that only goes up is what tells those apart.
static RECEIVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many window server notifications this process has been handed.
#[must_use]
pub fn notifications_received() -> u64 {
    RECEIVED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Declares the entry point for a window server notify procedure.
///
/// [`crate::trampoline!`] cannot serve these: the raw callback's five arguments
/// become one [`Event`], which is a shape of its own. This is that shape, once,
/// so no source builds it.
///
/// ```ignore
/// fn space_changed(state: &Relay<()>, event: skylight::ffi::Event<'_>) { .. }
///
/// skylight::notify!(SPACE_CHANGED = space_changed(&Relay<()>));
/// ```
#[macro_export]
macro_rules! notify {
    ($entry:ident = $handler:ident ( & $state:ty )) => {
        #[doc = concat!("The notify entry point for [`", stringify!($handler), "`].")]
        const $entry: extern "C-unwind" fn(
            u32,
            *mut ::std::ffi::c_void,
            usize,
            *mut ::std::ffi::c_void,
            $crate::ConnectionId,
        ) = {
            extern "C-unwind" fn shim(
                id: u32,
                data: *mut ::std::ffi::c_void,
                len: usize,
                context: *mut ::std::ffi::c_void,
                connection: $crate::ConnectionId,
            ) {
                let payload = if data.is_null() || len == 0 {
                    &[][..]
                } else {
                    // SAFETY: the window server hands over `len` readable bytes
                    // at `data` for the duration of this call.
                    unsafe { ::std::slice::from_raw_parts(data.cast::<u8>(), len) }
                };
                $crate::ffi::count_delivery(id, payload.len());

                // SAFETY: `context` is null or the pointer this procedure was
                // registered with; `with` does every reference rule.
                unsafe {
                    $crate::callback::Callback::<$state>::with(context, |state| {
                        $handler(
                            state,
                            $crate::ffi::Event {
                                id,
                                payload,
                                connection,
                            },
                        );
                    })
                };
            }
            shim
        };
    };
}

/// Counts a delivery. Called by the shim `SLSRegisterNotifyProc` handlers are
/// declared with, before the handler runs — so one that bails early still shows
/// as delivery.
#[doc(hidden)]
pub fn count_delivery(id: u32, bytes: usize) {
    RECEIVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    tracing::trace!(event = id, bytes, "window server notification");
}

// The window server answers these only for the thread it talks to: making,
// mutating, ordering and releasing windows, and suspending compositing. The
// attribute makes each declaration private and puts a proof-taking wrapper of
// the same name in front of it, so `ffi::SLSOrderWindow(proof, ..)` is the
// only way to reach one -- from inside this crate as much as outside.
#[link(name = "SkyLight", kind = "framework")]
#[skylight_macros::main_thread_ffi]
unsafe extern "C" {
    // ---- connection -------------------------------------------------------

    pub fn SLSMainConnectionID() -> ConnectionId;

    /// Suspends compositing until the matching re-enable. Bracket a multi-window
    /// mutation with these or the user sees each step land separately.
    pub fn SLSDisableUpdate(cid: ConnectionId) -> CGError;

    pub fn SLSReenableUpdate(cid: ConnectionId) -> CGError;

    // ---- window lifecycle -------------------------------------------------

    /// Creates a window owned by this connection, with no `NSWindow` behind it.
    ///
    /// `opaque_shape` empty means every pixel is alpha-blended. `tags` is an
    /// in/out bitset of [`TAG_BITS`] bits. `x`/`y` place the window; `region`
    /// describes its shape, conventionally anchored at the origin.
    pub fn SLSNewWindowWithOpaqueShapeAndContext(
        cid: ConnectionId,
        backing: c_int,
        region: *mut CFType,
        opaque_shape: *mut CFType,
        options: c_int,
        tags: *mut u64,
        x: f32,
        y: f32,
        tag_bits: c_int,
        out_wid: *mut WindowId,
        context: *mut c_void,
    ) -> CGError;

    pub fn SLSReleaseWindow(cid: ConnectionId, wid: WindowId) -> CGError;

    // ---- window geometry and appearance -----------------------------------

    /// Moves *and* resizes: the offset rides in `x`/`y`, the shape in `region`.
    pub fn SLSSetWindowShape(
        cid: ConnectionId,
        wid: WindowId,
        x: f32,
        y: f32,
        region: *mut CFType,
    ) -> CGError;

    /// Backing scale. Must agree with the `contentsScale` of whatever draws
    /// into the window, or the result is soft on a retina display.
    pub fn SLSSetWindowResolution(cid: ConnectionId, wid: WindowId, scale: f64) -> CGError;

    pub fn SLSSetWindowAlpha(cid: ConnectionId, wid: WindowId, alpha: f32) -> CGError;

    pub fn SLSSetWindowOpacity(cid: ConnectionId, wid: WindowId, opaque: bool) -> CGError;

    pub fn SLSSetWindowLevel(cid: ConnectionId, wid: WindowId, level: c_int) -> CGError;

    /// Ordering *within* a level, which `SLSSetWindowLevel` alone cannot express.
    pub fn SLSSetWindowSubLevel(cid: ConnectionId, wid: WindowId, sublevel: c_int) -> CGError;

    pub fn SLSOrderWindow(
        cid: ConnectionId,
        wid: WindowId,
        mode: c_int,
        relative_to: WindowId,
    ) -> CGError;

    pub fn SLSSetWindowBackgroundBlurRadius(
        cid: ConnectionId,
        wid: WindowId,
        radius: c_int,
    ) -> CGError;

    pub fn SLSSetWindowProperty(
        cid: ConnectionId,
        wid: WindowId,
        key: *mut CFString,
        value: *mut CFType,
    ) -> CGError;

    pub fn SLSWindowSetShadowProperties(wid: WindowId, properties: *mut CFType) -> CGError;

    pub fn SLSSetWindowTags(
        cid: ConnectionId,
        wid: WindowId,
        tags: *mut u64,
        tag_bits: c_int,
    ) -> CGError;

    pub fn SLSClearWindowTags(
        cid: ConnectionId,
        wid: WindowId,
        tags: *mut u64,
        tag_bits: c_int,
    ) -> CGError;

    /// Publishes what was drawn. Without this the context's pixels never reach
    /// the screen.
    pub fn SLSFlushWindowContentRegion(
        cid: ConnectionId,
        wid: WindowId,
        dirty: *mut c_void,
    ) -> CGError;

    // ---- mouse tracking ---------------------------------------------------

    pub fn SLSAddTrackingRect(cid: ConnectionId, wid: WindowId, rect: CGRect) -> CGError;

    pub fn SLSRemoveAllTrackingAreas(cid: ConnectionId, wid: WindowId) -> CGError;

    pub fn SLSCopyActiveMenuBarDisplayIdentifier(cid: ConnectionId) -> *mut CFString;
}

// The rest: calls the window server answers on any thread.
//
// Not an absence of an opinion -- the split is the opinion. Reading a window's
// picture and its screen rectangle is what the capture pool does, on four
// worker threads, and that is where 115ms of every second went when it was
// done inline. Moving these into the block above would put it back.
#[link(name = "SkyLight", kind = "framework")]
unsafe extern "C" {

    // ---- regions ----------------------------------------------------------

    pub fn CGSNewRegionWithRect(rect: *const CGRect, region: *mut *mut CFType) -> CGError;

    pub fn CGRegionCreateEmptyRegion() -> *mut CFType;

    /// Reading a window's tags back needs this query-and-iterate pair; there
    /// is no single-window getter in any of yabai, `rift` or `paneru`.
    /// Cross-checked against yabai's `window_tags()` (`window.c`) and `rift`'s
    /// `sys::skylight` bindings of the same four symbols.
    pub fn SLSWindowQueryWindows(
        cid: ConnectionId,
        windows: *mut CFArray<CFNumber>,
        count: c_int,
    ) -> *mut CFType;

    pub fn SLSWindowQueryResultCopyWindows(query: *mut CFType) -> *mut CFType;

    pub fn SLSWindowIteratorAdvance(iterator: *mut CFType) -> bool;

    pub fn SLSWindowIteratorGetCount(iterator: *mut CFType) -> c_int;

    pub fn SLSWindowIteratorGetTags(iterator: *mut CFType) -> u64;

    // ---- drawing ----------------------------------------------------------

    /// A drawing context for a window that has no view hierarchy. Caller owns
    /// the result and must `CFRelease` it.
    pub fn SLWindowContextCreate(
        cid: ConnectionId,
        wid: WindowId,
        options: *mut CFType,
    ) -> *mut CGContext;

    // ---- spaces and displays ----------------------------------------------

    pub fn SLSGetActiveSpace(cid: ConnectionId) -> SpaceId;

    pub fn SLSCopyManagedDisplaySpaces(cid: ConnectionId) -> *mut CFArray;

    pub fn SLSManagedDisplayGetCurrentSpace(cid: ConnectionId, uuid: *mut CFString) -> SpaceId;

    pub fn SLSSpaceGetType(cid: ConnectionId, sid: SpaceId) -> SpaceType;

    pub fn SLSGetDisplayMenubarHeight(display: u32, out_height: *mut u32) -> CGError;

    // ---- notifications ----------------------------------------------------

    pub fn SLSRegisterNotifyProc(callback: NotifyProc, event: u32, context: *mut c_void)
    -> CGError;

    /// Undoes [`SLSRegisterNotifyProc`]. Exported but undocumented — no
    /// header gives its signature, so this one was read off the arm64 code in
    /// the shared cache.
    ///
    /// It takes all three of registration's arguments, and the third is not
    /// decoration: `event` selects a hash bucket, and the entry erased from it
    /// is the first whose *proc and context both* match, comparing
    /// `entry + 0x00` against argument 0 and `entry + 0x08` against argument 2.
    /// Passing only two arguments therefore matches nothing, because the third
    /// register holds whatever the caller happened to leave there.
    ///
    /// The returned `CGError` cannot tell you which happened: found and
    /// not-found converge on the same `mov w0, #0`, so this always answers
    /// `kCGErrorSuccess`. Callers get their guarantee from releasing the state,
    /// not from this.
    pub fn SLSRemoveNotifyProc(callback: NotifyProc, event: u32, context: *mut c_void) -> CGError;

    pub fn SLSRegisterConnectionNotifyProc(
        cid: ConnectionId,
        callback: NotifyProc,
        event: u32,
        context: *mut c_void,
    ) -> CGError;

    /// Asks the window server to deliver per-window notifications (order,
    /// move, resize, ...) for windows this connection does not own. Without
    /// this, `SLSRegisterConnectionNotifyProc` sees only this process's own
    /// windows for those event types. Cross-checked against yabai's
    /// `event_loop.c` and `rift`'s `window_notify.rs`.
    pub fn SLSRequestNotificationsForWindows(
        cid: ConnectionId,
        window_list: *const WindowId,
        window_count: c_int,
    ) -> CGError;

    // ---- capture ----------------------------------------------------------

    /// Renders a window's current contents into a `CGImage`.
    ///
    /// `wid` is a *pointer to a 64-bit* window id even though window ids are
    /// 32-bit, because the call takes a list. `rect` may be `CGRectNull` for
    /// the whole window. Needs Screen Recording; without it the out-pointer is
    /// left null rather than an error being returned.
    pub fn SLSCaptureWindowsContentsToRectWithOptions(
        cid: ConnectionId,
        wid: *const u64,
        meh: bool,
        rect: CGRect,
        options: u32,
        out_image: *mut *mut CGImage,
    );

    /// A window's rect in screen coordinates, which is the only reliable
    /// source of its true size — the window list's bounds can disagree.
    pub fn SLSGetScreenRectForWindow(cid: ConnectionId, wid: WindowId, out: *mut CGRect)
    -> CGError;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub fn CFRelease(cf: *mut CFType);
}

/// The shape `DisplayServices` calls back with when a display's brightness
/// moves: a `CFNotificationCallback` whose "observer" slot carries the
/// `uint32_t` passthrough from registration rather than a pointer.
pub type BrightnessCallback = extern "C-unwind" fn(
    center: *mut c_void,
    display: CGDirectDisplayID,
    name: *mut c_void,
    sender: *const c_void,
    info: *mut c_void,
);

// Brightness, which no public framework exposes for an arbitrary display.
// Same standing as everything above: cross-checked against `SketchyBar`'s
// `src/misc/extern.h`, and not in any SDK header.
#[link(name = "DisplayServices", kind = "framework")]
unsafe extern "C" {
    pub fn DisplayServicesGetBrightness(
        display: CGDirectDisplayID,
        brightness: *mut f32,
    ) -> CGError;
    pub fn DisplayServicesSetBrightness(display: CGDirectDisplayID, brightness: f32) -> CGError;

    /// Not an error code despite the C declaration: any non-zero return means
    /// "yes". Verified on this machine's main display, which returns `1`.
    pub fn DisplayServicesCanChangeBrightness(display: CGDirectDisplayID) -> i32;

    pub fn DisplayServicesRegisterForBrightnessChangeNotifications(
        display: CGDirectDisplayID,
        passthrough: u32,
        callback: BrightnessCallback,
    ) -> CGError;
    pub fn DisplayServicesUnregisterForBrightnessChangeNotifications(
        display: CGDirectDisplayID,
        passthrough: u32,
    ) -> CGError;
}

#[cfg(test)]
mod notify_tests {
    use super::{Event, notifications_received};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// The received counter is one number for the process, so two tests
    /// asserting on it cannot run at once. Held for the length of each.
    static ALONE: Mutex<()> = Mutex::new(());

    /// What the handler below was given, so a test can look at it afterwards.
    struct Seen {
        id: AtomicU32,
        payload: Mutex<Vec<u8>>,
    }

    fn record(state: &Seen, event: Event<'_>) {
        state.id.store(event.id, Ordering::Release);
        *state.payload.lock().expect("no other holder") = event.payload.to_vec();
    }

    /// The trampoline can be called the way the window server calls it, which
    /// is the whole of what `NotifyProcedure` generates: no framework needed to
    /// test that the context comes back as the right type and the payload as a
    /// slice.
    #[test]
    fn a_handler_is_given_its_own_state_and_the_payload_as_bytes() {
        let _alone = ALONE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = Seen {
            id: AtomicU32::new(0),
            payload: Mutex::new(Vec::new()),
        };
        crate::notify!(RECORD = record(&Seen));
        let proc = RECORD;

        let mut bytes = [7u8, 8, 9];
        let before = notifications_received();
        // Exactly the call the window server makes: the context is a live
        // `Seen`, and `bytes` outlives the call.
        {
            proc(
                1401,
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                std::ptr::from_ref(&state).cast_mut().cast(),
                super::ConnectionId::new(0),
            );
        }

        assert_eq!(state.id.load(Ordering::Acquire), 1401);
        assert_eq!(
            *state.payload.lock().expect("no other holder"),
            vec![7, 8, 9]
        );
        assert_eq!(
            notifications_received(),
            before + 1,
            "the trampoline counts what it was handed"
        );
    }

    /// A framework calling back during teardown, which must not dereference.
    #[test]
    fn a_null_context_is_counted_and_dropped_rather_than_read() {
        let _alone = ALONE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::notify!(RECORD2 = record(&Seen));
        let proc = RECORD2;
        let before = notifications_received();
        // A null context is exactly what this is here to survive.
        {
            proc(
                1401,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                super::ConnectionId::new(0),
            );
        }
        assert_eq!(notifications_received(), before + 1);
    }
}

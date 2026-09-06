//! Raw declarations for the private window server API.
//!
//! Signatures are cross-checked against three independent reverse-engineerings:
//! yabai's `src/misc/extern.h`, `SketchyBar`'s `src/misc/extern.h`, and
//! `hs._asm.undocumented.spaces`. Nothing here is in a public SDK header, so
//! every signature is a claim about an ABI Apple may change.

use objc2_core_foundation::{CFArray, CFString, CFType, CGRect};
use objc2_core_graphics::{CGContext, CGError};
use std::ffi::{c_int, c_void};

/// A window server connection. `int` on the C side; the width matters because
/// it is passed by value in the first argument register.
pub type ConnectionId = c_int;

/// A window server window number — the same value `NSWindow.windowNumber` and
/// `kCGWindowNumber` report.
pub type WindowId = u32;

/// A mission control space. Opaque; only equality and lookup are meaningful.
pub type SpaceId = u64;

/// Width of the tag bitset every tag call takes, in **bits**, not bytes.
/// Passing 8 here is the classic way to silently set the wrong tags.
pub const TAG_BITS: c_int = 64;

/// `SLSOrderWindow` modes.
pub const ORDER_BELOW: c_int = -1;
pub const ORDER_OUT: c_int = 0;
pub const ORDER_ABOVE: c_int = 1;

/// `SLSSpaceGetType` results.
pub const SPACE_TYPE_USER: c_int = 0;
pub const SPACE_TYPE_SYSTEM: c_int = 2;
pub const SPACE_TYPE_FULLSCREEN: c_int = 4;

/// The callback shape every `SLSRegister*NotifyProc` takes.
/// `C-unwind` because a panic crossing back into the window server through a
/// plain `extern "C"` frame is undefined behaviour.
pub type NotifyProc = extern "C-unwind" fn(
    event: u32,
    data: *mut c_void,
    len: usize,
    context: *mut c_void,
    cid: ConnectionId,
);

#[link(name = "SkyLight", kind = "framework")]
unsafe extern "C" {
    // ---- connection -------------------------------------------------------

    pub fn SLSMainConnectionID() -> ConnectionId;

    /// Suspends compositing until the matching re-enable. Bracket a multi-window
    /// mutation with these or the user sees each step land separately.
    pub fn SLSDisableUpdate(cid: ConnectionId) -> CGError;
    pub fn SLSReenableUpdate(cid: ConnectionId) -> CGError;

    // ---- regions ----------------------------------------------------------

    pub fn CGSNewRegionWithRect(rect: *const CGRect, region: *mut *mut CFType) -> CGError;
    pub fn CGRegionCreateEmptyRegion() -> *mut CFType;

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

    // ---- drawing ----------------------------------------------------------

    /// A drawing context for a window that has no view hierarchy. Caller owns
    /// the result and must `CFRelease` it.
    pub fn SLWindowContextCreate(
        cid: ConnectionId,
        wid: WindowId,
        options: *mut CFType,
    ) -> *mut CGContext;

    /// Publishes what was drawn. Without this the context's pixels never reach
    /// the screen.
    pub fn SLSFlushWindowContentRegion(
        cid: ConnectionId,
        wid: WindowId,
        dirty: *mut c_void,
    ) -> CGError;

    // ---- spaces and displays ----------------------------------------------

    pub fn SLSGetActiveSpace(cid: ConnectionId) -> SpaceId;
    pub fn SLSCopyManagedDisplaySpaces(cid: ConnectionId) -> *mut CFArray;
    pub fn SLSManagedDisplayGetCurrentSpace(cid: ConnectionId, uuid: *mut CFString) -> SpaceId;
    pub fn SLSSpaceGetType(cid: ConnectionId, sid: SpaceId) -> c_int;
    pub fn SLSCopyActiveMenuBarDisplayIdentifier(cid: ConnectionId) -> *mut CFString;
    pub fn SLSGetDisplayMenubarHeight(display: u32, out_height: *mut u32) -> CGError;

    // ---- notifications ----------------------------------------------------

    pub fn SLSRegisterNotifyProc(callback: NotifyProc, event: u32, context: *mut c_void)
    -> CGError;
    pub fn SLSRegisterConnectionNotifyProc(
        cid: ConnectionId,
        callback: NotifyProc,
        event: u32,
        context: *mut c_void,
    ) -> CGError;

    // ---- mouse tracking ---------------------------------------------------

    pub fn SLSAddTrackingRect(cid: ConnectionId, wid: WindowId, rect: CGRect) -> CGError;
    pub fn SLSRemoveAllTrackingAreas(cid: ConnectionId, wid: WindowId) -> CGError;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub fn CFRelease(cf: *mut CFType);
}

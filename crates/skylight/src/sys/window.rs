//! Asking the window server about windows — this process's and everyone
//! else's.
//!
//! [`crate::ffi`] can *make* a window and read its tags and its picture. What
//! it cannot do is answer the questions a bar asks about windows it does not
//! own: which ones are on a space, what level they sit at, whether one is
//! ordered in at all. That is what is here.
//!
//! The window-list query is the centre of it, because it is what
//! `SketchyBar`'s own `space_windows` items are built on
//! (`src/app_windows.c`): [`SLSCopyWindowsWithOptionsAndTags`] for the
//! window ids on a set of spaces, then [`crate::ffi::SLSWindowQueryWindows`]
//! and the iterator to read each one's properties in a single round trip
//! rather than one call per window per property.

use crate::ffi::{ConnectionId, WindowId};
use objc2_application_services::{AXError, AXUIElement};
use objc2_core_foundation::{CFArray, CFNumber, CFString, CFType, CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGError;
use std::ffi::c_int;

/// What [`SLSCopyWindowsWithOptionsAndTags`] should count as a window.
///
/// Not a bitfield anyone has decoded — these are the two values every
/// reference passes and nothing else, so they are named as the two choices
/// they are. `0x2` and `0x7` are what yabai's `space_window_list_for_connection`
/// passes for `include_minimized` false and true respectively (`window.c`), and
/// `SketchyBar`'s `app_windows.c` passes `0x2`.
pub mod window_list {
    /// Windows that are on screen. What a bar showing the current space wants.
    pub const ON_SCREEN: u32 = 0x2;
    /// The same, plus minimized windows.
    pub const INCLUDING_MINIMIZED: u32 = 0x7;
}

/// The `selector` [`SLSCopySpacesForWindows`] takes.
///
/// `0x7` is the only value any reference passes — yabai's `window_space_id`,
/// `JankyBorders`' `window.h` and `rift`'s `window_spaces` all pass it
/// literally, none of them names it, and none says what the bits mean. Named
/// here as the one value known to work rather than left as a magic number at a
/// call site.
pub const ALL_SPACES_SELECTOR: c_int = 0x7;

bitflags::bitflags! {
    /// What [`SLSWindowIteratorGetAttributes`] reports, as far as anyone has
    /// decoded it.
    ///
    /// One bit, because one bit is all any reference names. `SketchyBar`'s
    /// `iterator_window_suitable` (`src/app_windows.c`) requires
    /// `attributes & 0x2` of a window before it will treat it as a real
    /// application window, alongside a tag test; `rift`'s
    /// `iterator_window_suitable` reads the attributes and then ignores them,
    /// with a comment saying an earlier version required "attribute/high-bit
    /// hints". Nobody has written down what the rest are.
    ///
    /// `from_bits_retain` is therefore the only correct way to build one: the
    /// unknown bits are not spare, they are undocumented, and a set that
    /// truncated them would silently lose whatever the window server actually
    /// said.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    #[repr(transparent)]
    pub struct WindowAttributes: u64 {
        /// Set on ordinary application windows and clear on the window
        /// server's own furniture -- shadows, tooltips, the drag image. What
        /// it *means* is not known; what it does is separate the two, which is
        /// why `SketchyBar` tests it.
        const APPLICATION_WINDOW = 0x2;
    }
}

// Reads. Nothing in this block changes window server state -- each one asks a
// question and writes the answer through an out-pointer or returns a +1
// CoreFoundation object -- so they take a `ConnectionId` and need no
// proof, on the same footing as `SLSGetActiveSpace` and
// `SLSGetScreenRectForWindow` in `ffi`'s second block.
//
// That matters for more than tidiness here: the alias machinery walks
// Accessibility on a worker pool, and correlating an `AXUIElement` with a
// window server window (`_AXUIElementGetWindow`, below) is exactly the kind of
// thing that walk wants to do without hopping to the main thread first.
#[link(name = "SkyLight", kind = "framework")]
unsafe extern "C" {
    // ---- one window at a time ---------------------------------------------

    /// A window's frame in screen coordinates.
    ///
    /// Distinct from [`crate::ffi::SLSGetScreenRectForWindow`], which this
    /// crate already has and which reports the *inked* rectangle. Declared by
    /// yabai (`SLSGetWindowBounds`), `JankyBorders` and `rift` (which spells it
    /// `CGSGetWindowBounds` -- both symbols exist; see the note on the `CGS`
    /// prefix in [`crate::sys`]).
    pub fn SLSGetWindowBounds(cid: ConnectionId, wid: WindowId, frame: *mut CGRect) -> CGError;

    /// A window's level, which is how a bar tells "something is covering me"
    /// from "the desktop is showing through".
    ///
    /// **The references disagree on the out-pointer's width.** yabai and
    /// `rift` both say `int`; `JankyBorders` says `int64_t`
    /// (`misc/extern.h:53`). `c_int` here follows the two-to-one majority and
    /// matches [`crate::ffi::SLSSetWindowLevel`]'s `c_int` on the other side of
    /// the same property, but a caller should treat a level that looks like
    /// garbage as evidence that `JankyBorders` is the one that is right.
    pub fn SLSGetWindowLevel(cid: ConnectionId, wid: WindowId, level: *mut c_int) -> CGError;

    /// A window's alpha. Cross-checked against yabai's `SLSGetWindowAlpha`.
    pub fn SLSGetWindowAlpha(cid: ConnectionId, wid: WindowId, alpha: *mut f32) -> CGError;

    /// Whether a window is ordered in — on screen, as opposed to merely
    /// existing.
    ///
    /// The out-pointer is one byte. yabai and `rift` say `uint8_t*`,
    /// `JankyBorders` says `bool*`; those are the same width and layout on
    /// this platform, so the disagreement is cosmetic. `u8` rather than `bool`
    /// because a `bool` holding anything but 0 or 1 is undefined behaviour in
    /// Rust and nothing here promises the window server writes only those.
    pub fn SLSWindowIsOrderedIn(cid: ConnectionId, wid: WindowId, ordered: *mut u8) -> CGError;

    /// The connection that owns a window. Half of "which process is this
    /// window", the other half being [`SLSConnectionGetPID`] — which is the
    /// route `SketchyBar` takes in `app_windows.c` and the reason both are
    /// here rather than just one.
    pub fn SLSGetWindowOwner(cid: ConnectionId, wid: WindowId, owner: *mut c_int) -> CGError;

    /// The process behind a connection.
    ///
    /// `pid` is a `pid_t*`, which is `int32_t*` on this platform; spelt
    /// `c_int` because this crate does not depend on `libc` and the two are
    /// the same type. Cross-checked against yabai's and `SketchyBar`'s
    /// identical declarations.
    pub fn SLSConnectionGetPID(cid: ConnectionId, pid: *mut c_int) -> CGError;

    /// Which spaces a window is on — more than one for a sticky window, which
    /// is how yabai's `window_is_sticky` answers that question.
    ///
    /// `selector` takes [`ALL_SPACES_SELECTOR`]. Its type is the one place
    /// `rift` and yabai differ: `rift` says `u32`, yabai and `JankyBorders`
    /// say `int`. Same width, so it cannot matter; `c_int` follows the two
    /// that agree.
    pub fn SLSCopySpacesForWindows(
        cid: ConnectionId,
        selector: c_int,
        windows: *mut CFArray<CFNumber>,
    ) -> *mut CFArray<CFNumber>;

    /// The windows attached to a window — a sheet, a drawer, a popover's
    /// shadow. What a bar with a popup would use to find out what it has
    /// hanging off it. Cross-checked against yabai's `SLSCopyAssociatedWindows`.
    pub fn SLSCopyAssociatedWindows(cid: ConnectionId, wid: WindowId) -> *mut CFArray<CFNumber>;

    /// Which display a window is mostly on, by the window server's UUID
    /// naming — the same naming [`crate::display::active_menu_bar`] converts
    /// from. Cross-checked against yabai's `SLSCopyManagedDisplayForWindow`.
    pub fn SLSCopyManagedDisplayForWindow(cid: ConnectionId, wid: WindowId) -> *mut CFString;

    /// One of a window's properties by name, the read half of
    /// [`crate::ffi::SLSSetWindowProperty`]. Cross-checked against yabai's
    /// `SLSCopyWindowProperty`.
    pub fn SLSCopyWindowProperty(
        cid: ConnectionId,
        wid: WindowId,
        property: *mut CFString,
        value: *mut *mut CFType,
    ) -> CGError;

    // ---- many windows at once ---------------------------------------------

    /// The window ids on a set of spaces, filtered by tags.
    ///
    /// `owner` is a *connection* id, not a pid, and `0` means every
    /// connection — which is what both yabai and `SketchyBar` pass when they
    /// want a space's whole window list. `options` takes one of
    /// [`window_list`]'s two values. `set_tags` and `clear_tags` are in/out
    /// bitsets of [`crate::WindowTags`]: a window is returned only if it has
    /// every tag in the first and none in the second, and passing zero for
    /// both asks for no filtering.
    ///
    /// Declared identically by yabai, `SketchyBar` and `rift`, which is as
    /// much agreement as anything private here gets.
    pub fn SLSCopyWindowsWithOptionsAndTags(
        cid: ConnectionId,
        owner: u32,
        spaces: *mut CFArray<CFNumber>,
        options: u32,
        set_tags: *mut u64,
        clear_tags: *mut u64,
    ) -> *mut CFArray<CFNumber>;

    // ---- the query iterator, past what `ffi` already binds ----------------
    //
    // `ffi` has `Advance`, `GetCount` and `GetTags`, which is what reading a
    // window's tags back needs. These are the rest of the properties one pass
    // over the iterator can answer, so a caller that wants a window's level
    // and pid and frame makes one round trip instead of four.

    /// Cross-checked against yabai, `SketchyBar` and `JankyBorders`, all
    /// identical.
    pub fn SLSWindowIteratorGetWindowID(iterator: *mut CFType) -> u32;

    /// Zero for a top-level window. Every reference tests exactly that —
    /// `parent_wid == 0` is the first clause of both `SketchyBar`'s and
    /// `rift`'s "is this a real window" filter.
    pub fn SLSWindowIteratorGetParentID(iterator: *mut CFType) -> u32;

    /// Cross-checked against yabai and `JankyBorders`.
    pub fn SLSWindowIteratorGetLevel(iterator: *mut CFType) -> c_int;

    /// See [`WindowAttributes`] for the one bit anyone has decoded.
    /// Cross-checked against yabai and `SketchyBar`.
    pub fn SLSWindowIteratorGetAttributes(iterator: *mut CFType) -> u64;

    /// **`rift` is the only reference that names this.** Neither yabai,
    /// `SketchyBar` nor `JankyBorders` declares it; each reaches a window's pid
    /// the long way, through [`SLSGetWindowOwner`] and [`SLSConnectionGetPID`].
    /// The symbol does exist — confirmed by `dlsym` on this machine — so the
    /// name is real; the return type is `rift`'s claim alone. Prefer the two
    /// calls above where being sure matters.
    pub fn SLSWindowIteratorGetPID(iterator: *mut CFType) -> c_int;

    /// **`rift`-only, like [`SLSWindowIteratorGetPID`]**, and the riskier of
    /// the two: it returns a `CGRect` by value, which is a four-double
    /// structure return, so a wrong guess about the shape is not a wrong
    /// number but a corrupt stack. [`SLSGetWindowBounds`] answers the same
    /// question through an out-pointer and three references declare it.
    pub fn SLSWindowIteratorGetBounds(iterator: *mut CFType) -> CGRect;

    /// **`rift`-only.** [`SLSGetWindowAlpha`] is the cross-checked way to ask.
    pub fn SLSWindowIteratorGetAlpha(iterator: *mut CFType) -> f32;

    /// A window's minimum, maximum and current size, as the window server
    /// records them.
    ///
    /// **`rift`-only**, and it reports zeroes for many windows — `rift`'s
    /// `WindowQuery::constraints` falls back to
    /// [`SLSPackagesGetWindowConstraints`] when all four of min and max come
    /// back zero, so a caller here should expect to do the same.
    pub fn SLSWindowIteratorGetConstraints(
        iterator: *mut CFType,
        min: *mut CGSize,
        max: *mut CGSize,
        current: *mut CGSize,
    ) -> CGError;

    /// The fallback for [`SLSWindowIteratorGetConstraints`], by window id.
    /// **`rift`-only.**
    pub fn SLSPackagesGetWindowConstraints(
        cid: ConnectionId,
        wid: WindowId,
        min: *mut CGSize,
        max: *mut CGSize,
        current: *mut CGSize,
    ) -> CGError;

    // ---- the pointer ------------------------------------------------------

    /// Where the pointer is, without an event to read it off.
    ///
    /// `mouse` learns about the pointer from the window server's tracking
    /// rectangles, which say *that* it crossed a boundary. This says where it
    /// is at an arbitrary moment, which is what `SketchyBar`'s
    /// `display_active_display_uuid` needs when
    /// [`space::SLSGetSpaceManagementMode`](crate::sys::space::SLSGetSpaceManagementMode)
    /// says displays do not have separate spaces: with one shared space the
    /// active display is the one the pointer is on, and there is no other way
    /// to ask. Cross-checked against yabai and `SketchyBar`.
    pub fn SLSGetCurrentCursorLocation(cid: ConnectionId, point: *mut CGPoint) -> CGError;

    /// The window under a point, and the connection that owns it.
    ///
    /// The three unnamed integers are a walk direction, not decoration:
    /// `(0, 1, 0)` starts at the top, and `(wid, -1, 0)` continues *below* an
    /// already-found window, which is how `rift`'s
    /// `is_point_occluded_by_external_window` skips past its own windows. For
    /// a bar the use is the same question inverted — is anything on top of me.
    /// Cross-checked against yabai's identical declaration (which also returns
    /// `OSStatus` rather than `CGError`; both are `int32_t`).
    pub fn SLSFindWindowAndOwner(
        cid: ConnectionId,
        start_wid: c_int,
        direction: c_int,
        zero: c_int,
        screen_point: *mut CGPoint,
        window_point: *mut CGPoint,
        wid: *mut WindowId,
        owner: *mut c_int,
    ) -> c_int;
}

// The window server answers this one only for its own thread: it changes a
// window's appearance. It takes a `ConnectionId` first, so the attribute leaves
// it as declared -- having one to pass is already the proof.
#[link(name = "SkyLight", kind = "framework")]
#[skylight_macros::main_thread_ffi]
unsafe extern "C" {
    /// [`crate::ffi::SLSSetWindowBackgroundBlurRadius`] with the blur's
    /// *style* as well as its radius.
    ///
    /// What the extra integer selects is not documented anywhere; yabai
    /// exposes it as an opaque number and so does `rift`'s
    /// `CgsWindow::set_blur`. Cross-checked against yabai's
    /// `SLSSetWindowBackgroundBlurRadiusStyle`.
    pub fn SLSSetWindowBackgroundBlurRadiusStyle(
        cid: ConnectionId,
        wid: WindowId,
        radius: c_int,
        style: c_int,
    ) -> CGError;
}

// Accessibility, not the window server. Reads an element's window server id,
// which is the one thing `crate::ax` cannot do and the only way to correlate
// the two namings: an `AXUIElement` for a window and the `WindowId` every call
// above takes are the same window with no other bridge between them.
//
// Any thread, on purpose. The alias machinery walks Accessibility on a worker
// pool, and this is a plain attribute read on an element that walk is already
// holding -- requiring proof here would mean hopping to the main thread in the
// middle of it for no gain. Cross-checked against yabai's
// `_AXUIElementGetWindow` and `rift`'s `WindowServerId: TryFrom<&AXUIElement>`,
// which agree exactly; it lives in `HIServices`, reached through the
// `ApplicationServices` umbrella (confirmed by `dladdr` on this machine).
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    pub fn _AXUIElementGetWindow(element: *mut AXUIElement, wid: *mut WindowId) -> AXError;
}

#[cfg(test)]
mod tests {
    use super::WindowAttributes;

    /// The bit `SketchyBar` tests, at the value it tests it at. A set that
    /// truncated the undocumented bits would make this pass and still lose
    /// what the window server said, so the second half is the real assertion.
    #[test]
    fn an_attribute_set_keeps_the_bits_nobody_has_decoded() {
        assert_eq!(WindowAttributes::APPLICATION_WINDOW.bits(), 0x2);
        let unknown = WindowAttributes::from_bits_retain(0x8000_0000_0000_0002);
        assert!(unknown.contains(WindowAttributes::APPLICATION_WINDOW));
        assert_eq!(unknown.bits(), 0x8000_0000_0000_0002);
    }
}

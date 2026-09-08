//! Spaces, past the three calls [`crate::ffi`] already makes.
//!
//! `ffi` can name the active space, list the spaces per display and ask what
//! kind a space is — which is everything the `spaces` source needs today. What
//! is here is the rest of the vocabulary: enumerating spaces without going
//! through the per-display dictionary, a space's own name, and the one system
//! setting that changes what "the active space" even means.

use crate::ffi::{ConnectionId, SpaceId};
use objc2_core_foundation::{CFArray, CFNumber, CFString};
use std::ffi::c_int;

bitflags::bitflags! {
    /// Which spaces [`CGSCopySpaces`] should return.
    ///
    /// Names and values from `NUIKit/CGSInternal`'s `CGSSpace.h`, which is the
    /// oldest of the reverse-engineerings and the only one that names these at
    /// all, cross-checked against `rift`'s `CGSSpaceMask` — the two agree on
    /// every bit `CGSInternal` has.
    ///
    /// `rift` names one bit `CGSInternal` does not: [`Self::INCLUDE_OS`] at
    /// `1 << 3`. Nothing else names it and there is no live evidence for it
    /// here, so it is carried with that said out loud rather than presented as
    /// established.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    #[repr(transparent)]
    pub struct SpaceMask: c_int {
        /// The space that is current on each display.
        const INCLUDE_CURRENT = 1 << 0;
        /// The spaces that are not current.
        const INCLUDE_OTHERS = 1 << 1;
        /// Spaces the user made, as opposed to the system's own.
        const INCLUDE_USER = 1 << 2;
        /// The system's own spaces — a fullscreen app's, Dashboard's.
        ///
        /// **`rift`-only**; absent from `CGSInternal`. `1 << 3` is what `rift`
        /// says and nothing contradicts it, but nothing confirms it either.
        const INCLUDE_OS = 1 << 3;
        /// Restrict the answer to spaces currently on screen.
        const VISIBLE = 1 << 16;
    }
}

impl SpaceMask {
    /// Every user space on every display, current or not — what a bar drawing
    /// a space indicator wants.
    ///
    /// `CGSInternal`'s `kCGSAllSpacesMask` and `rift`'s `ALL_SPACES` agree on
    /// this one exactly.
    pub const ALL: Self = Self::INCLUDE_USER
        .union(Self::INCLUDE_OTHERS)
        .union(Self::INCLUDE_CURRENT);

    /// The same, restricted to what is on screen. `CGSInternal`'s
    /// `KCGSAllVisibleSpacesMask`.
    pub const ALL_VISIBLE: Self = Self::ALL.union(Self::VISIBLE);

    /// The space current on each display. Both references agree
    /// (`kCGSCurrentSpaceMask`).
    pub const CURRENT: Self = Self::INCLUDE_USER.union(Self::INCLUDE_CURRENT);

    // Deliberately absent: a `kCGSOtherSpacesMask` equivalent. `CGSInternal`
    // spells it `INCLUDE_OTHERS | INCLUDE_CURRENT` and `rift` spells it
    // `INCLUDE_USER | INCLUDE_OTHERS` -- a mask named "other spaces" that
    // includes the current one is either a typo in `CGSInternal` or a fact
    // about the window server, and there is no way to tell from here. A caller
    // that wants one can write the combination it means.
}

/// What [`SLSGetSpaceManagementMode`] reports when System Settings' *Displays
/// have separate Spaces* is on.
///
/// Cross-checked two ways, and it is the *behaviour* that is cross-checked
/// rather than the number: yabai refuses to start unless the call returns this
/// (`yabai.c`), and `SketchyBar` uses it to decide how to find the active
/// display — with separate spaces it asks
/// [`crate::ffi::SLSCopyActiveMenuBarDisplayIdentifier`], and without them it
/// falls back to whichever display the pointer is on
/// (`display_active_display_uuid`, `src/display.c`). No reference names the
/// other values.
pub const SEPARATE_SPACES_PER_DISPLAY: c_int = 1;

// All reads, so all on a `ConnectionId` and none needing proof -- the
// same standing `SLSGetActiveSpace` and `SLSSpaceGetType` already have in
// `ffi`'s second block. None of these mutates anything: the two `Copy` calls
// hand back a +1 CoreFoundation object the caller owns, and the third returns
// a setting.
#[link(name = "SkyLight", kind = "framework")]
unsafe extern "C" {
    /// Every space matching `mask`, as an array of `CFNumber` space ids.
    ///
    /// The one way to enumerate spaces that does not go through
    /// [`crate::ffi::SLSCopyManagedDisplaySpaces`]' nested dictionaries — and
    /// the only way to ask "which spaces are visible right now" in one call,
    /// through [`SpaceMask::VISIBLE`].
    ///
    /// Named `CGS` rather than `SLS` because that is the name both references
    /// use and the name that exists: **both** `CGSCopySpaces` and
    /// `SLSCopySpaces` resolve on this machine, which is the usual pattern for
    /// this API — the framework was renamed and kept the old exports. Declared
    /// by `CGSInternal` (`CFArrayRef CGSCopySpaces(CGSConnectionID, CGSSpaceMask)`)
    /// and by `rift`, which agree; neither yabai nor `SketchyBar` declares it,
    /// so this is the one call here with no window-manager-independent check
    /// beyond `CGSInternal` itself.
    pub fn CGSCopySpaces(cid: ConnectionId, mask: SpaceMask) -> *mut CFArray<CFNumber>;

    /// A space's name, if it has one.
    ///
    /// Not the number Mission Control shows — this is the window server's own
    /// identifier string. Cross-checked against yabai's `SLSSpaceCopyName` and
    /// `CGSInternal`'s `CGSSpaceCopyName`, which agree.
    pub fn SLSSpaceCopyName(cid: ConnectionId, sid: SpaceId) -> *mut CFString;

    /// Which display a space belongs to, by the window server's UUID naming.
    ///
    /// The inverse of [`crate::ffi::SLSManagedDisplayGetCurrentSpace`], and the
    /// call that answers "did the space that just changed belong to the display
    /// this panel is on" without walking the whole display-spaces dictionary.
    /// Cross-checked against yabai's and `SketchyBar`'s identical declarations.
    pub fn SLSCopyManagedDisplayForSpace(cid: ConnectionId, sid: SpaceId) -> *mut CFString;

    /// Whether each display has its own set of spaces.
    ///
    /// See [`SEPARATE_SPACES_PER_DISPLAY`] for what the answer means and why a
    /// bar cares. Cross-checked against yabai and `SketchyBar`, which declare
    /// it identically — `SketchyBar` in `sketchybar.c` rather than its
    /// `extern.h`.
    pub fn SLSGetSpaceManagementMode(cid: ConnectionId) -> c_int;
}

#[cfg(test)]
mod tests {
    use super::SpaceMask;

    /// The composites are the references' own arithmetic, so they are worth
    /// pinning: `kCGSAllSpacesMask` is 0b111 and adding `CGSSpaceVisible` puts
    /// bit 16 on top of it.
    #[test]
    fn the_space_masks_both_references_agree_on_have_the_values_they_agree_on() {
        assert_eq!(SpaceMask::ALL.bits(), 0b111);
        assert_eq!(SpaceMask::ALL_VISIBLE.bits(), 0b111 | (1 << 16));
        assert_eq!(SpaceMask::CURRENT.bits(), 0b101);
    }
}

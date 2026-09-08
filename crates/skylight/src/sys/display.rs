//! The furniture a bar has to share the screen with: the menu bar it hides
//! under, the Dock it must not sit on top of, and the window server's own
//! naming of displays.
//!
//! [`crate::display`] answers what `CoreGraphics` will say about a display —
//! its bounds, whether it is built in, how fast it refreshes, its brightness.
//! None of that says how much of the display is already taken. That is what is
//! here, and all of it is private.
//!
//! # Display reconfiguration
//!
//! Nothing is declared for it, deliberately. `rift` re-declares
//! `CGDisplayRegisterReconfigurationCallback` and its flags by hand
//! (`sys/skylight.rs`); both are **public, documented `CoreGraphics` API** that
//! `objc2-core-graphics` already binds as
//! [`CGDisplayRegisterReconfigurationCallback`](objc2_core_graphics::CGDisplayRegisterReconfigurationCallback)
//! and [`CGDisplayChangeSummaryFlags`](objc2_core_graphics::CGDisplayChangeSummaryFlags),
//! so re-declaring them would only add a second place to be wrong.
//!
//! **And `rift`'s copy is wrong.** Its `DisplayReconfigFlags` puts `ENABLED` at
//! `0x40`, `DISABLED` at `0x80`, `MIRROR` at `0x100` and `UNMIRROR` at `0x200`.
//! Apple's `CGDisplayConfiguration.h` (lines 218-222 of the SDK on this
//! machine) has `kCGDisplayEnabledFlag = 1 << 8`, `kCGDisplayDisabledFlag =
//! 1 << 9`, `kCGDisplayMirrorFlag = 1 << 10` and `kCGDisplayUnMirrorFlag =
//! 1 << 11` — so `rift` reads a display being *mirrored* as one being enabled,
//! and an unmirror as a disable, and never sees a real mirror change at all.
//! The bits it shares with the header (`BEGIN_CONFIGURATION`, `MOVED`,
//! `SET_MAIN`, `SET_MODE`, `ADD`, `REMOVE`, `DESKTOP_SHAPE_CHANGED`) are
//! correct. The test at the bottom of this file pins the header's values, so
//! this note cannot rot into a claim about something that changed.

use objc2_core_foundation::{CFArray, CFString, CGPoint, CGRect};
use objc2_core_graphics::CGError;
use std::ffi::c_int;

use crate::ffi::{ConnectionId, SpaceId};

/// Where the Dock is, as [`CoreDockGetOrientationAndPinning`] reports it.
///
/// Values from yabai's `display_manager.h`, which is the only reference that
/// names them (`DOCK_ORIENTATION_BOTTOM 2`, `LEFT 3`, `RIGHT 4`) and uses them
/// in `display.c` to decide which edge of a display the Dock eats. `1` is
/// presumably "top", which macOS has not allowed since the Public Beta;
/// nothing names it, so nothing here claims it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DockOrientation {
    Bottom,
    Left,
    Right,
}

impl DockOrientation {
    /// The orientation `orientation` names, or `None` for a value no reference
    /// has seen — which is the honest answer rather than a guess, since being
    /// wrong about this means putting a bar under the Dock.
    #[must_use]
    pub const fn from_raw(orientation: c_int) -> Option<Self> {
        match orientation {
            2 => Some(Self::Bottom),
            3 => Some(Self::Left),
            4 => Some(Self::Right),
            _ => None,
        }
    }
}

/// Whether the Dock hides itself, so a bar at that edge has the space back.
///
/// Safe because the call takes nothing and returns a byte — see
/// [`CoreDockGetAutoHideEnabled`] for why it is declared as one.
#[must_use]
pub fn dock_is_autohidden() -> bool {
    // SAFETY: no arguments, and the return is read as a byte rather than a
    // `bool`, so no value it can produce is unsound.
    unsafe { CoreDockGetAutoHideEnabled() != 0 }
}

/// Which edge the Dock is on, and how it is pinned along that edge.
///
/// `None` for an orientation no reference names — see
/// [`DockOrientation::from_raw`]. The pinning is passed through as the raw
/// integer because nothing names *those* values either.
#[must_use]
pub fn dock_orientation() -> (Option<DockOrientation>, c_int) {
    let mut orientation = 0;
    let mut pinning = 0;
    // SAFETY: two live, valid `c_int` out-pointers, which is all the call
    // takes; it returns nothing.
    unsafe { CoreDockGetOrientationAndPinning(&raw mut orientation, &raw mut pinning) };
    (DockOrientation::from_raw(orientation), pinning)
}

// Reads, so a `ConnectionId` and no proof -- see `crate::sys`'s note on
// the split. The two `Copy` calls hand back a +1 CoreFoundation object the
// caller owns; the rest write through out-pointers.
#[link(name = "SkyLight", kind = "framework")]
unsafe extern "C" {
    // ---- the menu bar -----------------------------------------------------

    /// Whether the menu bar hides itself.
    ///
    /// `enabled` is inverted from what a bar wants to know:
    /// `SketchyBar`'s `display_menu_bar_visible` returns `!status`
    /// (`src/display.c:239`). Cross-checked against yabai's and `SketchyBar`'s
    /// identical declarations.
    pub fn SLSGetMenuBarAutohideEnabled(cid: ConnectionId, enabled: *mut c_int) -> CGError;

    /// The rectangle the menu bar occupies on a space when it is showing.
    ///
    /// **Note the argument order**: the out-pointer comes *first*, before the
    /// connection. That is not a transcription slip — yabai and `SketchyBar`
    /// declare it identically, and `SketchyBar` calls it that way
    /// (`SLSGetRevealedMenuBarBounds(&bounds, g_connection, sid)`).
    ///
    /// `SketchyBar` only calls it on x86: on Apple silicon it takes the menu
    /// bar height from `NSScreen`'s top inset instead, falling back to the
    /// notch height plus six, or 24 points. So this is the *Intel* path of a
    /// question [`crate::ffi::SLSGetDisplayMenubarHeight`] answers differently,
    /// and a caller on this machine should treat it as unverified.
    pub fn SLSGetRevealedMenuBarBounds(
        rect: *mut CGRect,
        cid: ConnectionId,
        sid: SpaceId,
    ) -> CGError;

    // ---- the Dock ---------------------------------------------------------

    /// The Dock's rectangle, and why it is where it is.
    ///
    /// What the `reason` out-parameter means is not documented by any
    /// reference; yabai's `display_manager_dock_rect` reads it into a local and
    /// throws it away. Kept in the signature because it is part of the ABI.
    /// Cross-checked against yabai's identical declaration.
    pub fn SLSGetDockRectWithReason(
        cid: ConnectionId,
        rect: *mut CGRect,
        reason: *mut c_int,
    ) -> CGError;

    // ---- displays, as the window server names them ------------------------

    /// Every display the window server manages, as an array of UUID strings.
    ///
    /// The UUID naming, not `CGDirectDisplayID` — the same strings
    /// [`crate::ffi::SLSCopyActiveMenuBarDisplayIdentifier`] and
    /// [`crate::ffi::SLSManagedDisplayGetCurrentSpace`] deal in, and the array
    /// whose *order* `SketchyBar` uses as a display's arrangement index
    /// (`display_arrangement`, `src/display.c`). Cross-checked against yabai's
    /// and `SketchyBar`'s identical declarations.
    pub fn SLSCopyManagedDisplays(cid: ConnectionId) -> *mut CFArray<CFString>;

    /// Which display a rectangle mostly belongs to.
    ///
    /// How a popup decides which display it is on when its frame straddles
    /// two, and `rift`'s fallback for a window whose space it cannot otherwise
    /// find. Cross-checked against yabai's and `SketchyBar`'s identical
    /// declarations (both spell it `SLS`; `rift` spells the same symbol `CGS`,
    /// and both exports exist).
    pub fn SLSCopyBestManagedDisplayForRect(cid: ConnectionId, rect: CGRect) -> *mut CFString;

    /// The same question for a point — which, with
    /// [`window::SLSGetCurrentCursorLocation`](crate::sys::window::SLSGetCurrentCursorLocation),
    /// is how `SketchyBar` finds the active display when displays do *not* have
    /// separate spaces. Cross-checked against yabai and `SketchyBar`.
    pub fn SLSCopyBestManagedDisplayForPoint(cid: ConnectionId, point: CGPoint) -> *mut CFString;
}

// `CoreDock*` is not the window server: both symbols live in `HIServices`,
// reached through the `ApplicationServices` umbrella -- confirmed by `dladdr`
// on this machine rather than assumed, since yabai reaches them by linking
// Carbon and does not say where they come from.
//
// Any thread: neither takes a connection, and the Dock's own state is not
// per-connection. Nothing in either reference calls them from a particular
// thread.
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// Whether Dock autohide is on.
    ///
    /// Declared as returning `u8`, not `bool`, and the difference is real:
    /// yabai's declaration returns CoreFoundation's `Boolean`, which is an
    /// `unsigned char`, and a `bool` holding anything but 0 or 1 is undefined
    /// behaviour in Rust. `rift` declares it `-> bool` and so takes that on
    /// trust. [`dock_is_autohidden`] is the safe way to ask.
    pub fn CoreDockGetAutoHideEnabled() -> u8;

    /// Which edge the Dock is on, and its pinning along that edge. Returns
    /// nothing; both answers come back through the out-pointers. See
    /// [`dock_orientation`] and [`DockOrientation`].
    pub fn CoreDockGetOrientationAndPinning(orientation: *mut c_int, pinning: *mut c_int);
}

#[cfg(test)]
mod tests {
    use objc2_core_graphics::CGDisplayChangeSummaryFlags as Flags;

    /// The values in Apple's `CGDisplayConfiguration.h`, written out so that
    /// the note in this module's docs about `rift` having four of them wrong
    /// is checked rather than remembered. If `objc2-core-graphics` ever
    /// regenerates these differently, this is what says so.
    #[test]
    fn the_display_reconfiguration_flags_are_the_ones_apples_header_names() {
        assert_eq!(Flags::BeginConfigurationFlag.0, 1 << 0);
        assert_eq!(Flags::MovedFlag.0, 1 << 1);
        assert_eq!(Flags::SetMainFlag.0, 1 << 2);
        assert_eq!(Flags::SetModeFlag.0, 1 << 3);
        assert_eq!(Flags::AddFlag.0, 1 << 4);
        assert_eq!(Flags::RemoveFlag.0, 1 << 5);
        // The four `rift` gets wrong, at 1<<6, 1<<7, 1<<8 and 1<<9.
        assert_eq!(Flags::EnabledFlag.0, 1 << 8);
        assert_eq!(Flags::DisabledFlag.0, 1 << 9);
        assert_eq!(Flags::MirrorFlag.0, 1 << 10);
        assert_eq!(Flags::UnMirrorFlag.0, 1 << 11);
        assert_eq!(Flags::DesktopShapeChangedFlag.0, 1 << 12);
    }

    /// A live check against the Dock's own preferences, which is the only
    /// ground truth there is for a call no header describes.
    ///
    /// `defaults` is the source of truth on purpose: it reads what the user
    /// set, and the private call reads what the Dock is doing about it. They
    /// have to agree. A machine whose `com.apple.dock` has neither key set —
    /// a fresh account, a test runner — is not a failure, so an unreadable
    /// preference skips the comparison rather than failing it.
    #[test]
    fn the_dock_reports_the_orientation_and_autohide_its_preferences_say() {
        use super::{DockOrientation, dock_is_autohidden, dock_orientation};

        let read = |key: &str| {
            std::process::Command::new("/usr/bin/defaults")
                .args(["read", "com.apple.dock", key])
                .output()
                .ok()
                .filter(|out| out.status.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        };

        if let Some(autohide) = read("autohide") {
            assert_eq!(
                dock_is_autohidden(),
                autohide == "1",
                "CoreDockGetAutoHideEnabled disagreed with com.apple.dock autohide"
            );
        }

        let (orientation, _pinning) = dock_orientation();
        if let Some(preferred) = read("orientation") {
            let expected = match preferred.as_str() {
                "bottom" => Some(DockOrientation::Bottom),
                "left" => Some(DockOrientation::Left),
                "right" => Some(DockOrientation::Right),
                _ => return,
            };
            assert_eq!(orientation, expected, "the Dock is not where it says it is");
        } else {
            // No preference set means the default, which is the bottom.
            assert_eq!(orientation, Some(DockOrientation::Bottom));
        }
    }
}

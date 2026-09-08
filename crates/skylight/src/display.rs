//! Facts about a physical display that `CGGetActiveDisplayList` alone does
//! not report.
//!
//! `CGDisplayIsBuiltin`/`CGDisplayIsMain` are public, documented
//! `CoreGraphics` API — not reverse-engineered — but `objc2-core-graphics`
//! only generates them behind feature flags (`CGDirectDisplay` +
//! `CGDisplayConfiguration` + `libc`) this crate did not previously enable.
//! Cross-checked against `rift`'s `sys::skylight` binding of the same two
//! symbols and yabai's `workspace.m`, both of which call them directly.
//!
//! Verified live on this machine (a `Mac16,1` with one built-in Retina
//! display and nothing external attached): both functions report `true` for
//! the sole active display, matching `system_profiler SPDisplaysDataType`'s
//! "Main Display: Yes". **Not verified**: either reporting `false` — that
//! needs a second, external display, which this machine does not have.
//!
//! Brightness is the odd one out, and the reason a display is a *type* here
//! rather than a handful of functions taking an id. `DisplayServices` is
//! private, its getter writes through an out-pointer and its "can this
//! change" answer is an `int` that is not an error code — three different
//! shapes of raw call that every caller otherwise repeats. Naming the display
//! once turns all of them into methods on it.

use crate::error::{Error, Result};
use crate::ffi::{self, ConnectionId, SpaceId};
use objc2_core_foundation::{CFRetained, CFUUID};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayCopyDisplayMode, CGDisplayIsBuiltin, CGDisplayIsMain,
    CGDisplayMode, CGError, CGGetActiveDisplayList, CGMainDisplayID,
};
use std::ptr::NonNull;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// Public API, but not one `objc2-core-graphics` binds: the display-UUID
    /// pair only shows up in `CGDirectDisplay.h`'s less-used corner.
    fn CGDisplayGetDisplayIDFromUUID(uuid: *const CFUUID) -> CGDirectDisplayID;
}

/// The display currently showing the menu bar, and the space active on it.
///
/// The window server names that display by UUID rather than by id — the same
/// UUID round-trip `SketchyBar`'s `display.c` uses to go from a display id to
/// its `SLSManagedDisplayGetCurrentSpace` key — so this is where the two
/// namings meet, and the only place either raw call is made.
///
/// `None` when the window server would not name a display, or names one with
/// no active space, which is what it reports mid-switch.
#[must_use]
pub fn active_menu_bar(cid: ConnectionId) -> Option<(Display, SpaceId)> {
    // SAFETY: `cid` is this process's connection id; the Copy convention hands
    // back a +1 `CFString` this now owns, or null.
    let uuid_string = unsafe {
        let uuid = ffi::SLSCopyActiveMenuBarDisplayIdentifier(cid);
        CFRetained::from_raw(NonNull::new(uuid)?)
    };

    let uuid = CFUUID::from_string(None, Some(&uuid_string))?;
    // SAFETY: a live `CFUUID` for the duration of the call.
    let display = unsafe { CGDisplayGetDisplayIDFromUUID(CFRetained::as_ptr(&uuid).as_ptr()) };

    // SAFETY: `cid` and `uuid_string` are both live for the call.
    let space = unsafe {
        ffi::SLSManagedDisplayGetCurrentSpace(cid, CFRetained::as_ptr(&uuid_string).as_ptr())
    };

    (space != 0).then_some((Display::new(display), space))
}

/// What to assume when a display will not say how fast it refreshes.
///
/// `CGDisplayModeGetRefreshRate` answers 0.0 for a panel whose timing the
/// window server drives itself rather than a mode line, which some built-in
/// ones do — a zero means "would not say", not "never refreshes". This
/// machine's `Mac16,1` panel answers 120.00, so the fallback is for the
/// displays that do not; `examples/refresh_rate.rs` is what asks.
pub const ASSUMED_REFRESH_RATE: f64 = 60.0;

/// Every display currently active, in the window server's order.
///
/// Counted first, then filled: `CGGetActiveDisplayList` answers the count
/// when handed a null buffer, so the caller never picks a fixed maximum and
/// never has to wonder whether a sixteenth display was silently dropped.
///
/// Mirrored displays report the same bounds; they are left in, because each
/// still needs its own window for a bar to appear on both.
pub fn active() -> Result<Vec<Display>> {
    let mut count = 0u32;
    // SAFETY: a null buffer with a zero capacity is the documented form that
    // only reports how many there are; `count` is a valid out-pointer.
    list(unsafe { CGGetActiveDisplayList(0, std::ptr::null_mut(), &raw mut count) })?;

    let mut ids = vec![0 as CGDirectDisplayID; count as usize];
    // SAFETY: `ids` has room for exactly `count` ids, which is what the
    // capacity argument promises, and `count` is a valid out-pointer.
    list(unsafe { CGGetActiveDisplayList(count, ids.as_mut_ptr(), &raw mut count) })?;

    ids.truncate(count as usize);
    Ok(ids.into_iter().map(Display::new).collect())
}

/// A display, by the id `CoreGraphics` names it with.
///
/// A plain wrapper, `Copy` and the same size as the id, so it costs nothing
/// to pass one where the id was passed before.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Display(CGDirectDisplayID);

impl Display {
    #[must_use]
    pub const fn new(id: CGDirectDisplayID) -> Self {
        Self(id)
    }

    /// The display `CGMainDisplayID` names, which is where a bar goes when
    /// nothing has said otherwise.
    #[must_use]
    pub fn main() -> Self {
        Self(CGMainDisplayID())
    }

    #[must_use]
    pub const fn id(self) -> CGDirectDisplayID {
        self.0
    }

    /// Whether this is the display built into the machine, as opposed to
    /// anything external. `SketchyBar`'s notch handling (`bar.c` lines
    /// 196-222 and 437-489) only ever applies to this display — an external
    /// monitor has no notch to avoid.
    #[must_use]
    pub fn is_builtin(self) -> bool {
        CGDisplayIsBuiltin(self.0)
    }

    /// Whether this is the display [`Display::main`] also names.
    #[must_use]
    pub fn is_main(self) -> bool {
        CGDisplayIsMain(self.0)
    }

    /// How many frames a second this display can actually show, falling back
    /// to [`ASSUMED_REFRESH_RATE`] when it will not say.
    ///
    /// Both calls are public `CoreGraphics` and `objc2-core-graphics` already
    /// wraps them safely, so unlike the rest of this module there is no
    /// `unsafe` to justify — the retained mode is released when it drops.
    #[must_use]
    pub fn refresh_rate(self) -> f64 {
        let rate = CGDisplayCopyDisplayMode(self.0)
            .map_or(0.0, |mode| CGDisplayMode::refresh_rate(Some(&mode)));
        if rate > 0.0 {
            rate
        } else {
            ASSUMED_REFRESH_RATE
        }
    }

    /// Whether brightness can be set at all here. A display attached over a
    /// link with no brightness control says no, and so does a virtual one.
    #[must_use]
    pub fn can_change_brightness(self) -> bool {
        // SAFETY: takes a display id by value and returns immediately. The
        // return is a truth value, not an error code -- see the declaration.
        unsafe { ffi::DisplayServicesCanChangeBrightness(self.0) != 0 }
    }

    /// Brightness as a 0.0-1.0 scalar.
    pub fn brightness(self) -> Result<f32> {
        let mut scalar = 0.0f32;
        // SAFETY: `scalar` is a live, valid `f32` out-pointer for the call.
        let status = unsafe { ffi::DisplayServicesGetBrightness(self.0, &raw mut scalar) };
        ok(status).map(|()| scalar)
    }

    /// Sets brightness from a 0.0-1.0 scalar, clamped — `DisplayServices`
    /// accepts anything and a value out of range is a caller's arithmetic
    /// slipping, not something to hand the hardware.
    pub fn set_brightness(self, scalar: f32) -> Result<()> {
        // SAFETY: takes a display id and a float by value.
        ok(unsafe { ffi::DisplayServicesSetBrightness(self.0, scalar.clamp(0.0, 1.0)) })
    }

    /// Calls `callback` whenever this display's brightness moves, until
    /// [`Display::unwatch_brightness`].
    ///
    /// Safe despite being a callback registration, because the passthrough is
    /// not a pointer: `DisplayServices` carries a `uint32_t`, and this fixes
    /// it to the display's own id, so what the callback recovers is the
    /// display and nothing else. There is no lifetime for a caller to get
    /// wrong.
    pub fn watch_brightness(self, callback: ffi::BrightnessCallback) -> Result<()> {
        // SAFETY: takes the display id twice by value and a function pointer.
        ok(unsafe {
            ffi::DisplayServicesRegisterForBrightnessChangeNotifications(self.0, self.0, callback)
        })
    }

    /// Undoes [`Display::watch_brightness`]. `DisplayServices` matches on the
    /// display and the passthrough, both of which are this display's id.
    pub fn unwatch_brightness(self) -> Result<()> {
        // SAFETY: the same display and passthrough the registration used.
        ok(unsafe {
            ffi::DisplayServicesUnregisterForBrightnessChangeNotifications(self.0, self.0)
        })
    }
}

fn ok(status: CGError) -> Result<()> {
    crate::error::ok(status).map_err(Error::Brightness)
}

fn list(status: CGError) -> Result<()> {
    crate::error::ok(status).map_err(Error::DisplayList)
}

/// Whether `id` is the display built into the machine.
#[must_use]
pub fn is_builtin(id: CGDirectDisplayID) -> bool {
    Display::new(id).is_builtin()
}

/// Whether `id` is the display `CGMainDisplayID` also names.
#[must_use]
pub fn is_main(id: CGDirectDisplayID) -> bool {
    Display::new(id).is_main()
}

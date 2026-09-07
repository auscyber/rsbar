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

use objc2_core_graphics::{CGDirectDisplayID, CGDisplayIsBuiltin, CGDisplayIsMain};

/// Whether `id` is the display built into the machine, as opposed to
/// anything external. `SketchyBar`'s notch handling (`bar.c` lines 196-222
/// and 437-489) only ever applies to this display — an external monitor has
/// no notch to avoid.
#[must_use]
pub fn is_builtin(id: CGDirectDisplayID) -> bool {
    CGDisplayIsBuiltin(id)
}

/// Whether `id` is the display `CGMainDisplayID` also names.
#[must_use]
pub fn is_main(id: CGDirectDisplayID) -> bool {
    CGDisplayIsMain(id)
}

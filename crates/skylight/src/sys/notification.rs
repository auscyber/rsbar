//! The window server's notification numbers, named.
//!
//! [`crate::ffi::SLSRegisterNotifyProc`] takes a `u32` and nothing tells you
//! which ones exist. This is the fuller vocabulary, so that a source reaching
//! for one has somewhere to look it up instead of a number copied out of
//! someone else's C.
//!
//! Only the ones a status bar could plausibly want are here. `rift`'s
//! `KnownCGSEvent` names about forty more, mostly its own guesses at internal
//! window-manager transactions whose payloads it says are still under
//! investigation; those are not ported.
//!
//! # How much each of these is worth
//!
//! Three tiers, and each constant says which it is in:
//!
//! * **Registered by a shipping bar or window manager.** `SketchyBar` and yabai
//!   both subscribe from C, so the number is load-bearing in a program people
//!   run. These are as certain as anything private gets.
//! * **Named by two independent reverse-engineerings**, but not registered
//!   here.
//! * **`rift`-only.** Carried because a plausible name is better than a
//!   number, and marked because nothing corroborates it.
//!
//! What no tier gives you is the payload. Only the four numbers `rsbar`'s
//! `spaces` source already uses have had their payloads read here, and only as
//! far as "the first eight bytes are a space id". Four of those — [`SPACE_CHANGED`],
//! [`SPACE_WINDOW_CREATED`], [`SPACE_WINDOW_DESTROYED`] and
//! [`SPACE_WINDOW_BATCH_REASSOCIATED`] — are also spelt out locally in that
//! source; they are repeated here so the vocabulary is in one place.

// ---- displays -------------------------------------------------------------

/// A display is about to sleep. **`rift`-only** (`DisplayWillSleep`).
///
/// For a bar the use is not subtle: a window server window that is drawn into
/// while its display sleeps is work thrown away, and the wake is not an
/// `NSWorkspace` notification — `NSWorkspaceWillSleepNotification` is the
/// *machine* sleeping, which is a different and rarer thing.
pub const DISPLAY_WILL_SLEEP: u32 = 102;

/// A display woke. **`rift`-only** (`DisplayDidWake`).
pub const DISPLAY_DID_WAKE: u32 = 103;

// ---- window capture -------------------------------------------------------

/// Whatever made window capture stop working has stopped.
///
/// **Registered by `SketchyBar`** (`sketchybar.c:217`), and the name describes
/// the only observed *use*, not the window server's own meaning, which no
/// reference states and `rift` does not name at all. `SketchyBar` sets its
/// `g_disable_capture` back to zero here, and its `window_capture` starts
/// answering again.
///
/// Directly relevant to this tree: the alias capture pool has exactly the
/// problem this pair gates, and currently has no way to know about it.
pub const CAPTURE_UNBLOCKED: u32 = 904;

/// Window capture will not work until further notice.
///
/// **Registered by `SketchyBar`**, which on this event sets
/// `g_disable_capture = -1` — indefinitely, unlike the one-second backoff it
/// applies to [`WINDOW_TITLE_CHANGED`] — and returns nothing from
/// `window_capture` until [`CAPTURE_UNBLOCKED`], a space change or an
/// application switch clears it.
pub const CAPTURE_BLOCKED: u32 = 905;

// ---- one window -----------------------------------------------------------
//
// These arrive for windows this connection does not own only after
// `SLSRequestNotificationsForWindows` has asked for them; see that call's own
// note in `ffi`.

/// A window was destroyed. **Registered by yabai** (`yabai.c:329`); `rift`
/// names it `WindowClosed`.
pub const WINDOW_DESTROYED: u32 = 804;

/// A window moved. **Named by `rift`** (`WindowMoved`); yabai reaches the same
/// fact through Accessibility instead.
pub const WINDOW_MOVED: u32 = 806;

/// A window was resized. **`rift`-only** (`WindowResized`).
pub const WINDOW_RESIZED: u32 = 807;

/// A window's stacking order changed. **Registered by yabai**
/// (`yabai.c:326`), which is how it learns another application raised
/// something; `rift` names it `WindowReordered`.
pub const WINDOW_REORDERED: u32 = 808;

/// A window's level changed. **`rift`-only** (`WindowLevelChanged`).
pub const WINDOW_LEVEL_CHANGED: u32 = 811;

/// A window became visible again. **Registered by `SketchyBar`**
/// (`app_windows.c:301`, as half of its `window_hide_handler`).
pub const WINDOW_UNHIDDEN: u32 = 815;

/// A window was hidden. **Registered by `SketchyBar`** (`app_windows.c:305`).
pub const WINDOW_HIDDEN: u32 = 816;

/// A window's title changed.
///
/// **Registered by `SketchyBar`** (`sketchybar.c:221`), and it is worth knowing
/// *why*: not to redraw a title, but because `SketchyBar` treats a title change
/// as a sign that window capture will be wrong for about a second, and backs
/// off (`window_capture`, `src/window.c:353`). `rift` names it
/// `WindowTitleChanged`.
pub const WINDOW_TITLE_CHANGED: u32 = 1322;

// ---- Mission Control ------------------------------------------------------

/// Mission Control was entered. **Registered by yabai** (`yabai.c:323`);
/// `rift` names it `MissionControlEntered`.
pub const MISSION_CONTROL_ENTERED: u32 = 1204;

/// A space transition animation finished. **`rift`-only**
/// (`TransitionDidFinish`).
///
/// The event a bar would use to stop guessing when a switch has settled, which
/// is otherwise a timer.
pub const TRANSITION_DID_FINISH: u32 = 1700;

// ---- spaces ---------------------------------------------------------------

/// A window joined a space. **Registered by `SketchyBar`**
/// (`app_windows.c:293`); `rift` names it `SpaceWindowCreated`. Already used
/// by `rsbar`'s `spaces` source.
///
/// The payload leads with the space id as a `u64`, which is the one payload
/// shape verified in this tree.
pub const SPACE_WINDOW_CREATED: u32 = 1325;

/// A window left a space. **Registered by `SketchyBar`**
/// (`app_windows.c:297`); `rift` names it `SpaceWindowDestroyed`.
pub const SPACE_WINDOW_DESTROYED: u32 = 1326;

/// A space was created. **Registered by both `SketchyBar`
/// (`sketchybar.c:224`, guarded on macOS 13) and yabai (`yabai.c:319`)** —
/// the best-corroborated number here after [`SPACE_CHANGED`].
pub const SPACE_CREATED: u32 = 1327;

/// A space was destroyed. **Registered by both `SketchyBar` and yabai.**
pub const SPACE_DESTROYED: u32 = 1328;

/// Several windows changed space at once. **Named by `rift`
/// (`SpaceWindowBatchReassociated`) and `paneru`**; already used by `rsbar`'s
/// `spaces` source, which reads the leading `u64` as the space id.
pub const SPACE_WINDOW_BATCH_REASSOCIATED: u32 = 1339;

/// The active space changed. **Registered by `SketchyBar` in two places and by
/// every other implementation**; `rift` names it `WorkspaceDidChange`. Already
/// used by `rsbar`'s `spaces` source, and the single most load-bearing number
/// in this file.
pub const SPACE_CHANGED: u32 = 1401;

/// The active space is *about* to change. **`rift`-only**
/// (`WorkspaceWillChange`). The start of the transition [`TRANSITION_DID_FINISH`] ends.
pub const SPACE_WILL_CHANGE: u32 = 1400;

// ---- applications ---------------------------------------------------------

/// The frontmost application changed. **Registered by `SketchyBar`**
/// (`sketchybar.c:220`); `rift` names it `FrontmostApplicationChanged`.
///
/// `rsbar` gets this from `NSWorkspaceDidActivateApplicationNotification`
/// instead, which is public and carries the application object. This is here
/// for completeness, not as an alternative worth switching to.
pub const FRONT_APP_CHANGED: u32 = 1508;

/// Every notification, which the window server accepts as a wildcard.
///
/// **`rift`-only** (`KnownCGSEvent::All`). Useful for finding out what actually
/// fires — which is how several of the numbers above were confirmed elsewhere —
/// and a bad idea in a running bar: it cannot be unregistered, and it is every
/// window event for every window on the machine.
pub const ALL: u32 = 0xFFFF_FFFF;

#[cfg(test)]
mod tests {
    /// `rsbar`'s `spaces` source spells four of these out for itself, and the
    /// two spellings have to agree or one of them is wrong. Written as the
    /// literals that source uses rather than by importing it — `skylight` does
    /// not depend on `rsbar`, and could not — so this is a tripwire for an
    /// edit to either side.
    #[test]
    fn the_four_numbers_the_spaces_source_uses_are_the_ones_it_uses() {
        assert_eq!(super::SPACE_CHANGED, 1401);
        assert_eq!(super::SPACE_WINDOW_CREATED, 1325);
        assert_eq!(super::SPACE_WINDOW_DESTROYED, 1326);
        assert_eq!(super::SPACE_WINDOW_BATCH_REASSOCIATED, 1339);
    }
}

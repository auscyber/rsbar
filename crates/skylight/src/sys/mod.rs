//! Window server calls this crate did not have, ported from `rift`'s
//! `src/sys/` — a window manager's binding layer for the same private API.
//!
//! This is the crate's general layer, and every module in it is `pub`: a
//! window manager reaches for these directly, the way `paneru` would, rather
//! than through anything shaped around a status bar. [`crate::Foreign`] wraps
//! [`window`]'s window-reading calls into a plain `Send` handle for the common
//! case; a caller after something these blocks declare but that type does not
//! cover — a space, a display, a notification — calls the raw declaration
//! itself.
//!
//! Separate from [`crate::ffi`] rather than added to it. `ffi` is what a bar
//! *needs*: the calls behind [`crate::Window`], the capture pool and the two
//! sources that talk to the window server, every one of them reached by
//! something above it. What is here is the second category — calls a status
//! bar plausibly wants and nothing in this tree has asked for yet, alongside
//! calls a window manager wants far more than a bar does — so keeping them
//! apart is what stops "declared" from reading as "used".
//!
//! # Which block a call goes in
//!
//! The same rule [`crate::ffi`] follows: a call that mutates a window this
//! process does not exclusively own the timing of goes in a
//! `#[skylight_macros::main_thread_ffi]` block, and a plain read goes in an
//! ordinary one taking a [`ConnectionId`](crate::ffi::ConnectionId). Each
//! block below says which it is and why. A declaration whose first argument is
//! already a `ConnectionId` is left exactly as declared either way — see
//! [`skylight_macros::main_thread_ffi`]'s own doc comment for what that does
//! and does not prove.
//!
//! Everything read-only here takes a [`ConnectionId`](crate::ffi::ConnectionId)
//! directly rather than [`crate::Connected`]: these are declared, not wrapped,
//! so a caller that already holds a connected guard passes `connected.id()`
//! the same as any other raw call.
//!
//! # Provenance
//!
//! Every signature was cross-checked against at least one reverse-engineering
//! independent of `rift`, from a local checkout rather than from memory:
//! `~/code/yabai/src/misc/extern.h`, `~/code/SketchyBar/src/misc/extern.h`,
//! `~/code/JankyBorders/src/misc/extern.h`, and `NUIKit/CGSInternal`'s
//! `CGSSpace.h`/`CGSWindow.h`. Each declaration names the ones it was checked
//! against, and says so when they disagree.
//!
//! # `CGS` and `SLS` are the same symbols
//!
//! The framework was renamed from `CoreGraphicsServices` to `SkyLight` and kept
//! both sets of exports, so `CGSCopySpaces` and `SLSCopySpaces`,
//! `CGSGetWindowBounds` and `SLSGetWindowBounds`, and the rest of the pairs all
//! resolve. Each declaration below uses whichever prefix its cross-checked
//! references use, which is why the two are mixed; where `rift` and yabai spell
//! the same call differently that is said at the declaration and is not a
//! disagreement about anything.
//!
//! **Every symbol here was also confirmed to exist**, by `dlsym` against the
//! live frameworks on this machine (macOS 25.5, `Mac16,1`): all thirty-six
//! resolved, none missing — including the four of the `SLSWindowIterator`
//! family that only `rift` names, which is what says those names are real
//! rather than transcribed from something older. `dladdr` is also where the
//! `#[link]` names come from: `CoreDock*` and `_AXUIElementGetWindow` are
//! `HIServices`, not `SkyLight`.
//!
//! What none of that gives you is the argument lists. Those remain the
//! references' claims, and a wrong one is a corrupt stack rather than a wrong
//! answer.
//!
//! # What was deliberately not ported
//!
//! `rift` is a window manager, and most of its binding layer is either work a
//! bar must not do or work this tree already does its own way:
//!
//! * **Anything that moves or focuses another application's windows** —
//!   `_SLPSSetFrontProcessWithOptions`, `SLPSPostEventRecordTo`,
//!   `SLSMoveWindow`, `SLSSetWindowTransform`, `SLSProcessAssignToSpace`,
//!   `space_switch.rs` wholesale. A bar reports; it does not rearrange.
//! * **Hotkeys, event taps and haptics** (`hotkey.rs`, `event_tap.rs`,
//!   `haptics.rs`). `mouse` already owns the pointer through the window
//!   server's own tracking rectangles, which is narrower than a global tap.
//! * **`rift`'s executor, timers, dispatch and run loop wrappers**
//!   (`executor.rs`, `timer.rs`, `dispatch.rs`, `run_loop.rs`,
//!   `display_link.rs`). This daemon's runner and frame budget are its own.
//! * **`mach.rs`** — 1,400 lines of hand-written MIG to read a window's
//!   sublevel and talk to another process's window server port. The bar *sets*
//!   its own sublevel and never reads anyone else's.
//! * **`process.rs`** — `GetProcessForPID`/`GetProcessInformation`, deprecated
//!   since 10.9, to find out whether a pid is an XPC service. Nothing a bar
//!   asks. Its `ProcessSerialNumber` is only needed by the front-process calls
//!   above, which are not here either, so the whole Carbon detour goes.
//! * **`axuielement.rs`, `observer.rs`, `app.rs`, `accessibility.rs`** —
//!   [`crate::ax`] is this crate's equivalent and is what the alias machinery
//!   already uses. The one exception is [`window::_AXUIElementGetWindow`],
//!   which `ax` genuinely lacks: see its own note.
//! * **`service.rs`** (launchd), `carbon.rs`, `geometry.rs` (serde shims),
//!   `enhanced_ui.rs`, `power.rs` (`NSProcessInfo` low-power state — an event
//!   this daemon would have to add to its protocol first, which is not in this
//!   change's scope).
//! * **`display_churn.rs`** is the one file whose *idea* was taken rather than
//!   its code: three process-wide atomics tracking whether a reconfiguration
//!   is in flight. `coolabah`'s `sources::displays` keeps the same idea without
//!   the state or the timers: it fingerprints the display layout and reports
//!   only when the fingerprint moves. See that module for why a bar can skip
//!   the settle loop a window manager cannot.

pub mod display;
pub mod notification;
pub mod space;
pub mod window;

//! Spike: does any `SkyLight`/CGS notification fire when a menu bar item's
//! *contents* change, as opposed to its geometry or z-order?
//!
//! Registers every plausibly-relevant `kCGSEvent*` number this investigation
//! turned up — cross-checked against `NUIKit/CGSInternal`'s `CGSEvent.h`,
//! yabai, `rift`, `paneru`, and upstream `SketchyBar`'s own
//! `sketchybar.c`/`window.c` — through both `SLSRegisterConnectionNotifyProc`
//! and the connection-less `SLSRegisterNotifyProc`, and logs every one that
//! actually fires, with a timestamp, while a menu bar clock is on screen. Run
//! it, wait for a minute boundary to pass, and read what printed.
//!
//! Usage: `cargo run -p skylight --example notify_probe -- <owner> <name>`,
//! e.g. `notify_probe "Control Centre" Clock`. Defaults to Control Centre's
//! Clock if no args are given.
//!
//! # Result (macOS 26.5.1, this machine)
//!
//! **Nothing fires, ever, for any of it.** Confirmed four ways:
//! - Watching the Control Centre clock's window across a 130s run (at least
//!   one minute rollover, i.e. a real, visible content change), for all 32
//!   candidates above, registered both ways: silence.
//! - Watching a real Finder window (`owner=Finder`, layer 0) while resizing it
//!   six times live via `osascript`'s `set bounds of window 1` — a genuine,
//!   externally-visible geometry change to a *foreign* window, which is
//!   exactly the yabai/rift/paneru use case `SLSRequestNotificationsForWindows`
//!   exists for: silence, for `kCGSWindowDidMove`/`kCGSWindowDidResize`/
//!   `kCGSWindowDidChangeOrder` alike.
//! - Sanity check: creating a window this process owns, then ordering it
//!   in/out and moving it several times itself (so no cross-process
//!   permission or scoping question applies at all) while registered for the
//!   same window-lifecycle events: also silence.
//! - Cross-checking upstream `SketchyBar` directly: it registers exactly
//!   three of these (904, 905, `kCGSWindowTitleChanged`/1322) for
//!   `system_events`, but the handler only ever sets a `g_disable_capture`
//!   timestamp that makes `window_capture` *skip* a poll for about a second —
//!   never a signal to capture. Even the reference implementation this
//!   project cross-checks against does not attempt to drive an update from
//!   these; it treats them as "something might be mid-transition, don't
//!   trust a capture right now" at most.
//!
//! `SLSRegisterConnectionNotifyProc`, `SLSRegisterNotifyProc`, and
//! `SLSRequestNotificationsForWindows` all report success (`CGError(0)`) —
//! except a handful of the display/server-level events (300, 304, 306, 902,
//! 1000, 1002, 1003), which the connection-less form rejects with
//! `CGError(1000)` while the connection-scoped form accepts; those still
//! never fired via the form that did accept them either. The calls are not
//! erroring; nothing is being delivered. This matches `sources/spaces.rs`'s
//! own caveat that its space-change registration was never observed firing
//! live either — on this macOS build, `SkyLight`'s old private notification
//! stream appears to be non-functional for the specific event numbers every
//! reverse-engineered window manager (yabai, rift, paneru) relies on, not
//! merely silent about window *contents* specifically.

use objc2_core_foundation::{CFDictionary, CFNumber, CFString, CFType};
use objc2_core_graphics::{
    CGWindowListCopyWindowInfo, CGWindowListOption, kCGWindowLayer, kCGWindowName, kCGWindowNumber,
    kCGWindowOwnerName,
};
use skylight::ffi::{self, ConnectionId, WindowId};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

const MENU_BAR_LAYER: i64 = 0x19;

/// Every event number this investigation considered plausible for "a
/// window's contents changed", plus a few window lifecycle ones as a sanity
/// check that registration/delivery works at all.
const CANDIDATES: &[(u32, &str)] = &[
    (300, "kCGSServerConnDirtyScreenNotification"),
    (304, "kCGSServerUpdateDisplayNotification"),
    (306, "kCGSServerUpdateDisplayCompletedNotification"),
    (800, "kCGSWindowIsObscured"),
    (801, "kCGSWindowIsUnobscured"),
    (802, "kCGSWindowIsOrderedIn"),
    (803, "kCGSWindowIsOrderedOut"),
    (806, "kCGSWindowDidMove"),
    (807, "kCGSWindowDidResize"),
    (808, "kCGSWindowDidChangeOrder"),
    (809, "kCGSWindowGeometryDidChange"),
    (810, "kCGSWindowMonitorDataPending"),
    (811, "kCGSWindowDidCreate"),
    (815, "kCGSWindowIsVisible"),
    (816, "kCGSWindowIsInvisible"),
    (902, "kCGSLikelyUnbalancedDisableUpdateNotification"),
    (904, "kCGSConnectionWindowsBecameVisible"),
    (905, "kCGSConnectionWindowsBecameOccluded"),
    (906, "kCGSConnectionWindowModificationsStarted"),
    (907, "kCGSConnectionWindowModificationsStopped"),
    (912, "kCGSWindowBecameVisible"),
    (913, "kCGSWindowBecameOccluded"),
    (1000, "kCGSServerWindowDidCreate"),
    (1002, "kCGSServerWindowOrderDidChange"),
    (1003, "kCGSServerWindowDidTerminate"),
    // Added after reading SketchyBar's own `sketchybar.c`/`window.c`: it
    // registers exactly these three (704, 905, 1322) for `system_events`,
    // but only to set `g_disable_capture` and *skip* a poll for ~1s during a
    // transition — it never treats any of them as "capture now". Worth
    // testing for delivery anyway; a fired event still isn't proof it means
    // "contents changed" without SketchyBar's own corroborating use of it.
    (723, "kCGSEventNotificationKitDefined"),
    (1300, "kCGSServerMenuBarCreated"),
    (1303, "kCGSServerMenuBarDrawingStyleChanged"),
    (1308, "kCGSPackagesStatusBarSpaceChanged"),
    (1322, "kCGSWindowTitleChanged"),
    (1336, "kCGSWindowOrderingGroupChanged"),
    (1338, "kCGSSpaceWindowTransactionCommitted"),
    (1341, "kCGSWindowParentChanged"),
];

extern "C-unwind" fn on_event(
    event: u32,
    data: *mut c_void,
    len: usize,
    _context: *mut c_void,
    _cid: ConnectionId,
) {
    let name = CANDIDATES
        .iter()
        .find(|(id, _)| *id == event)
        .map_or("?", |(_, name)| name);
    let bytes = if data.is_null() || len == 0 {
        Vec::new()
    } else {
        // SAFETY: the window server gave us `len` bytes at `data` for the
        // duration of this callback.
        unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len) }.to_vec()
    };
    println!(
        "[{:?}] event {event} ({name}) len={len} bytes={bytes:02x?}",
        Instant::now()
    );
}

fn find_window(owner_want: &str, name_want: &str) -> Option<(WindowId, i64)> {
    let list = CGWindowListCopyWindowInfo(CGWindowListOption::OptionAll, 0)?;
    for i in 0..list.count() {
        // SAFETY: every element of this array is a CFDictionary.
        let ptr = unsafe { list.value_at_index(i) };
        let ptr = NonNull::new(ptr.cast_mut())?.cast::<CFType>();
        let dict = unsafe { ptr.as_ref() }.downcast_ref::<CFDictionary>()?;

        let layer = dict_i64(dict, unsafe { kCGWindowLayer })?;
        if layer != MENU_BAR_LAYER {
            continue;
        }
        let owner = dict_string(dict, unsafe { kCGWindowOwnerName }).unwrap_or_default();
        let name = dict_string(dict, unsafe { kCGWindowName }).unwrap_or_default();
        if owner == owner_want && name == name_want {
            let id = dict_i64(dict, unsafe { kCGWindowNumber })?;
            return Some((WindowId::try_from(id).ok()?, id));
        }
    }
    None
}

fn dict_value<'a>(dict: &'a CFDictionary, key: &CFString) -> Option<&'a CFType> {
    let ptr = unsafe { dict.value((std::ptr::from_ref(key)).cast()) };
    let ptr = NonNull::new(ptr.cast_mut())?.cast::<CFType>();
    Some(unsafe { ptr.as_ref() })
}

fn dict_string(dict: &CFDictionary, key: &CFString) -> Option<String> {
    Some(
        dict_value(dict, key)?
            .downcast_ref::<CFString>()?
            .to_string(),
    )
}

fn dict_i64(dict: &CFDictionary, key: &CFString) -> Option<i64> {
    dict_value(dict, key)?.downcast_ref::<CFNumber>()?.as_i64()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (owner, name) = if args.len() >= 2 {
        (args[0].clone(), args[1].clone())
    } else {
        ("Control Centre".to_owned(), "Clock".to_owned())
    };

    let Some((wid, wide_id)) = find_window(&owner, &name) else {
        eprintln!("no menu bar window matches owner={owner:?} name={name:?}");
        std::process::exit(1);
    };
    println!("watching owner={owner:?} name={name:?} window={wid} (0x{wide_id:x})");

    // SAFETY: `connection_id` reads the process-wide connection, a plain FFI
    // getter.
    let cid = unsafe { ffi::SLSMainConnectionID() };

    for &(event, name) in CANDIDATES {
        // SAFETY: `on_event` matches `NotifyProc`'s signature; no context is
        // needed since this probe is single-purpose and global anyway.
        let status = unsafe {
            ffi::SLSRegisterConnectionNotifyProc(cid, on_event, event, std::ptr::null_mut())
        };
        if status != objc2_core_graphics::CGError::Success {
            println!("register(connection) {event} ({name}) failed: {status:?}");
        }
        // Also register the connection-less form `spaces.rs` and SketchyBar
        // itself use, in case scoping to our own connection is what's
        // silently dropping delivery.
        // SAFETY: same contract as above, minus the connection argument.
        let status = unsafe { ffi::SLSRegisterNotifyProc(on_event, event, std::ptr::null_mut()) };
        if status != objc2_core_graphics::CGError::Success {
            println!("register(global) {event} ({name}) failed: {status:?}");
        }
    }

    let ids = [wid];
    // SAFETY: `ids` outlives the call.
    let status = unsafe { ffi::SLSRequestNotificationsForWindows(cid, ids.as_ptr(), 1) };
    println!("SLSRequestNotificationsForWindows -> {status:?}");

    println!(
        "pumping for 130s (pid {}); change the mirrored item's contents now",
        std::process::id()
    );
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(130) {
        // SAFETY: plain CoreFoundation call on this thread's run loop.
        unsafe {
            objc2_core_foundation::CFRunLoop::run_in_mode(
                objc2_core_foundation::kCFRunLoopDefaultMode,
                1.0,
                true,
            );
        }
    }
    println!("done");
}

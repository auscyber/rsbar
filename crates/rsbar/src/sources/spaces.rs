//! Space changes, from `SkyLight`'s private notification stream.
//!
//! `SLSRegisterNotifyProc` has no matching unregister call — none of the three
//! independent reverse-engineerings this crate cross-checks (yabai, `rift`,
//! `paneru`) know of one either, and `SketchyBar` never attempts to undo a
//! registration. Once installed, a callback is installed for the rest of the
//! process's life. That is fine for `SketchyBar`, which registers once at
//! startup and never stops, but this registry starts and stops sources lazily
//! as items subscribe and unsubscribe. So this source registers with `SkyLight`
//! at most once ever — guarded by [`REGISTERED`] — and every later
//! `register()` (an item re-subscribing after everything else had dropped it)
//! just repoints [`CURRENT_EMITTER`] at the new sink. Nothing here is
//! per-instance; it could not be, given the framework.
//!
//! # Event numbers
//!
//! None of these are in a public header. Cross-checked against `SketchyBar`'s
//! `sketchybar.c`/`app_windows.c` (which registers `1401` for a space change
//! and `1327`/`1328` for space create/destroy) and `paneru`'s `KnownCGSEvent`
//! (which additionally names `1325`/`1326`/`1339` for window membership).
//! `space_windows_changed` is a membership event, so it watches the latter
//! three, not the space lifecycle ones.
//!
//! # What is and is not verified
//!
//! [`active_display_and_space`] was checked against live ground truth: a spike
//! binary called it right after registering and printed `Some((1, 3))`, which
//! matched both `CGMainDisplayID()` and `defaults read com.apple.spaces`'
//! reported current space (`id64 = 3`) at that moment. The registration calls
//! themselves also succeed (`SLSRegisterNotifyProc` returns `kCGErrorSuccess`
//! for all four events) and nothing crashes.
//!
//! What was **not** observed firing live: this machine has no keyboard
//! shortcut bound to move between spaces (`AppleSymbolicHotKeys` has them
//! disabled), and driving Mission Control through synthetic mouse clicks on
//! its desktop-switcher UI did not reliably land a real switch in this
//! session. Actually triggering a live gesture-based space change — the
//! mechanism recent macOS requires, per `rift`'s `space_switch.rs` — needs a
//! real trackpad swipe or a substantial synthetic-gesture subsystem that was
//! judged out of scope here. So the `1401`/`1325`/`1326`/`1339` callbacks
//! themselves are unverified against a live event, even though the code they
//! call into is.

use crate::sources::{Cause, Registering, Registration, Source, SourceId, StartError};
use objc2_core_foundation::{CFRetained, CFString, CFUUID};
use objc2_core_graphics::{CGDirectDisplayID, CGError};
use rsbar_protocol::event::{SpaceChange, SpaceWindowsChange};
use rsbar_protocol::{Event, Kind};
use skylight::ConnectionId;
use std::collections::BTreeSet;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

/// `WorkspaceDidChange`: the active space changed, on some display.
const SPACE_CHANGED_EVENT: u32 = 1401;
/// `SpaceWindowCreated`: a window joined a space.
const SPACE_WINDOW_CREATED_EVENT: u32 = 1325;
/// `SpaceWindowDestroyed`: a window left a space.
const SPACE_WINDOW_DESTROYED_EVENT: u32 = 1326;
/// `SpaceWindowBatchReassociated`: several windows moved spaces at once.
const SPACE_WINDOW_BATCH_REASSOCIATED_EVENT: u32 = 1339;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// Public API, but not one `objc2-core-graphics` binds: the display-UUID
    /// pair only shows up in `CGDirectDisplay.h`'s less-used corner.
    fn CGDisplayGetDisplayIDFromUUID(uuid: *const CFUUID) -> CGDirectDisplayID;
}

/// The sink this process's one `SkyLight` registration currently writes into,
/// or null while nothing wants these events. Swapped, never freed: a
/// callback already in flight when this changes must not read a freed
/// `Emitter`, and the framework gives no way to wait for one to finish. See
/// the module docs for why this exists instead of a per-registration context.
static CURRENT_EMITTER: AtomicPtr<crate::sources::Emitter> = AtomicPtr::new(std::ptr::null_mut());
/// Whether the `SkyLight` callbacks have been installed yet. Set at most once
/// for the life of the process.
static REGISTERED: AtomicBool = AtomicBool::new(false);

fn set_emitter(emit: crate::sources::Emitter) {
    let boxed = Box::into_raw(Box::new(emit));
    // Deliberately leaked: see the module docs.
    CURRENT_EMITTER.swap(boxed, Ordering::SeqCst);
}

fn clear_emitter() {
    CURRENT_EMITTER.swap(std::ptr::null_mut(), Ordering::SeqCst);
}

fn current_emitter() -> Option<&'static crate::sources::Emitter> {
    // SAFETY: the pointer is either null or was leaked from a `Box` that is
    // never freed, so a non-null read is always valid for `'static`.
    unsafe { CURRENT_EMITTER.load(Ordering::SeqCst).as_ref() }
}

/// The display and space `SLSCopyActiveMenuBarDisplayIdentifier` currently
/// names, following the same UUID round-trip `SketchyBar`'s `display.c` uses
/// to go from a display id to its `SLSManagedDisplayGetCurrentSpace` key.
fn active_display_and_space(cid: ConnectionId) -> Option<(u32, u64)> {
    // SAFETY: `cid` is the process's connection id; the call hands back a +1
    // `CFString` or null.
    let uuid = NonNull::new(unsafe { skylight::ffi::SLSCopyActiveMenuBarDisplayIdentifier(cid) })?;
    // SAFETY: the Copy convention hands back a reference this now owns.
    let uuid_string = unsafe { CFRetained::<CFString>::from_raw(uuid) };

    let uuid = CFUUID::from_string(None, Some(&uuid_string))?;
    // SAFETY: `uuid` is a live `CFUUID` for the duration of the call.
    let display = unsafe { CGDisplayGetDisplayIDFromUUID(CFRetained::as_ptr(&uuid).as_ptr()) };

    // SAFETY: `cid` and `uuid_string` are both live for the call.
    let space = unsafe {
        skylight::ffi::SLSManagedDisplayGetCurrentSpace(
            cid,
            CFRetained::as_ptr(&uuid_string).as_ptr(),
        )
    };

    (space != 0).then_some((display, space))
}

/// The `u64` space id every window-membership payload here leads with, per
/// `rift`'s and `paneru`'s independent reads of the same notifications.
fn leading_space_id(data: *mut c_void, len: usize) -> Option<u64> {
    if data.is_null() || len < size_of::<u64>() {
        return None;
    }
    // SAFETY: `len` was just checked to hold at least one `u64`, and nothing
    // guarantees the window server's alignment of it.
    Some(unsafe { data.cast::<u64>().read_unaligned() })
}

extern "C-unwind" fn space_changed(
    _event: u32,
    _data: *mut c_void,
    _len: usize,
    _context: *mut c_void,
    cid: ConnectionId,
) {
    let Some(emit) = current_emitter() else {
        return;
    };
    let Some((display, space)) = active_display_and_space(cid) else {
        return;
    };
    emit.send(Event::SpaceChanged(SpaceChange { display, space }));
}

extern "C-unwind" fn space_windows_changed(
    _event: u32,
    data: *mut c_void,
    len: usize,
    _context: *mut c_void,
    _cid: ConnectionId,
) {
    let Some(emit) = current_emitter() else {
        return;
    };
    let Some(space) = leading_space_id(data, len) else {
        return;
    };
    emit.send(Event::SpaceWindowsChanged(SpaceWindowsChange { space }));
}

/// Nothing to deregister; see the module docs. Only clears the sink so a
/// callback firing after every subscriber has gone does not try to send into
/// a channel nobody is draining.
struct Watch;

impl Drop for Watch {
    fn drop(&mut self) {
        clear_emitter();
    }
}

pub struct Spaces;

impl Source for Spaces {
    fn id(&self) -> SourceId {
        SourceId("spaces")
    }

    fn provides(&self) -> Vec<Kind> {
        vec![Kind::SpaceChanged, Kind::SpaceWindowsChanged]
    }

    fn register(
        &mut self,
        _wanted: &BTreeSet<Kind>,
        cx: &mut Registering<'_>,
    ) -> Result<Registration, StartError> {
        set_emitter(cx.emitter());

        if !REGISTERED.swap(true, Ordering::SeqCst) {
            for (proc, event) in [
                (
                    space_changed as skylight::ffi::NotifyProc,
                    SPACE_CHANGED_EVENT,
                ),
                (space_windows_changed, SPACE_WINDOW_CREATED_EVENT),
                (space_windows_changed, SPACE_WINDOW_DESTROYED_EVENT),
                (space_windows_changed, SPACE_WINDOW_BATCH_REASSOCIATED_EVENT),
            ] {
                // SAFETY: `proc` matches `NotifyProc`'s signature; `SkyLight`
                // is free to call back from here on, with no context needed —
                // see the module docs for why this is process-global.
                let status = unsafe {
                    skylight::ffi::SLSRegisterNotifyProc(proc, event, std::ptr::null_mut())
                };
                if status != CGError::Success {
                    return Err(StartError::new(self.id(), Cause::CoreGraphics(status)));
                }
            }
        }

        Ok(Box::new(Watch))
    }
}

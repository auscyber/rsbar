//! Displays: the set of them changing, and which one has focus.
//!
//! Two different facts, one event. `CoreGraphics`' reconfiguration callback is
//! the only thing that reports a monitor being plugged in or rearranged, which
//! invalidates every panel's geometry. `NSWorkspace`'s display notification
//! reports focus moving to another display, which is what `SketchyBar`'s
//! `display_change` means. Both are `display_changed`, and both belong to this
//! source — split across two, subscribing to the event started both of them.

use crate::sources::observers::{Observers, ToEvent};
use crate::sources::{CallbackState, Cause, Emitter, Registration, Source, SourceId, StartError};
use objc2_app_kit::NSWorkspace;
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayChangeSummaryFlags, CGDisplayRegisterReconfigurationCallback,
    CGDisplayRemoveReconfigurationCallback,
};
use objc2_foundation::{NSNotification, NSString};
use rsbar_protocol::event::DisplayChange;
use rsbar_protocol::{Event, Kind};
use std::ffi::c_void;

fn display_changed(_: &NSNotification) -> Event {
    Event::DisplayChanged(DisplayChange {})
}

extern "C-unwind" fn reconfigured(
    _display: CGDirectDisplayID,
    flags: CGDisplayChangeSummaryFlags,
    context: *mut c_void,
) {
    // CoreGraphics announces a change twice: once up front carrying only
    // `BeginConfigurationFlag`, and again afterwards carrying what actually
    // happened. There is no matching "end" flag, so the first pass is
    // identified by that flag and skipped — acting on it would read the old
    // display layout.
    if flags.contains(CGDisplayChangeSummaryFlags::BeginConfigurationFlag) {
        return;
    }
    // SAFETY: the registration passed a `CallbackState<Emitter>` pointer.
    if let Some(emit) = unsafe { CallbackState::<Emitter>::recover(context) } {
        // Dropping beats blocking: this is a `CoreGraphics` callback.
        emit.send(Event::DisplayChanged(DisplayChange {}));
    }
}

/// Deregisters on drop.
struct Deregister(CallbackState<Emitter>);

impl Drop for Deregister {
    fn drop(&mut self) {
        self.0.with_ptr(|context| {
            // SAFETY: the same callback and context `register` passed.
            unsafe { CGDisplayRemoveReconfigurationCallback(Some(reconfigured), context) };
        });
    }
}

pub struct Displays;

impl Source for Displays {
    fn id(&self) -> SourceId {
        SourceId("displays")
    }

    fn provides(&self) -> Vec<Kind> {
        vec![Kind::DisplayChanged]
    }

    fn eager(&self) -> bool {
        true
    }

    fn register(&mut self, emit: Emitter) -> Result<Registration, StartError> {
        let state = CallbackState::new(emit);
        let status = state.with_ptr(|context| {
            // SAFETY: the state outlives the registration; see `CallbackState`.
            unsafe { CGDisplayRegisterReconfigurationCallback(Some(reconfigured), context) }
        });
        if status != objc2_core_graphics::CGError::Success {
            return Err(StartError::new(self.id(), Cause::CoreGraphics(status)));
        }

        // `NSWorkspaceActiveDisplayDidChangeNotification` is undocumented and
        // absent from the generated bindings, so it is named by string — the
        // same way SketchyBar reaches it.
        let focus_moved = NSString::from_str("NSWorkspaceActiveDisplayDidChangeNotification");
        let mut observers = Observers::new(NSWorkspace::sharedWorkspace().notificationCenter());
        observers.observe(&focus_moved, state.get(), display_changed as ToEvent);

        Ok(Box::new((Deregister(state), observers)))
    }
}

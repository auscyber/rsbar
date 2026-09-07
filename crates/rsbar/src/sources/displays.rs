//! Displays being added, removed or rearranged.
//!
//! `NSWorkspace` does not report this — its display notification fires when the
//! *active* display changes, not when the set of them does. Plugging in a
//! monitor arrives only here, and it invalidates every panel's geometry.

use crate::sources::{CallbackState, Cause, Emitter, Registration, Source, SourceId, StartError};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayChangeSummaryFlags, CGDisplayRegisterReconfigurationCallback,
    CGDisplayRemoveReconfigurationCallback,
};
use rsbar_protocol::event::DisplayChange;
use rsbar_protocol::{Event, Kind};
use std::ffi::c_void;

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

    fn register(&mut self, emit: Emitter) -> Result<Registration, StartError> {
        let state = CallbackState::new(emit);
        let status = state.with_ptr(|context| {
            // SAFETY: the state outlives the registration; see `CallbackState`.
            unsafe { CGDisplayRegisterReconfigurationCallback(Some(reconfigured), context) }
        });
        if status != objc2_core_graphics::CGError::Success {
            return Err(StartError::new(self.id(), Cause::CoreGraphics(status)));
        }
        Ok(Box::new(Deregister(state)))
    }
}

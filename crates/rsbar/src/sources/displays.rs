//! Displays being added, removed or rearranged.
//!
//! `NSWorkspace` does not report this — its display notification fires when the
//! *active* display changes, not when the set of them does. Plugging in a
//! monitor arrives only here, and it invalidates every panel's geometry.

use crate::sources::{Emission, Emitter, Registration, Source, SourceId, StartError};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayChangeSummaryFlags, CGDisplayRegisterReconfigurationCallback,
    CGDisplayRemoveReconfigurationCallback,
};
use rsbar_protocol::Event;
use std::ffi::c_void;

/// Recovers the emitter a registration was made with.
///
/// # Safety
///
/// `context` must be the pointer `install` passed, on an emitter that is still
/// alive — which is why the one this module makes is leaked.
unsafe fn emitter_from(context: *mut c_void) -> Option<&'static Emitter> {
    // SAFETY: the caller guarantees provenance and liveness.
    unsafe { context.cast::<Emitter>().as_ref() }
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
    // SAFETY: the registration passes the leaked emitter.
    if let Some(emit) = unsafe { emitter_from(context) } {
        // Dropping beats blocking: this is a `CoreGraphics` callback.
        emit.send(Emission::new(Event::DisplayChanged, None));
    }
}

/// Deregisters on drop.
struct Deregister(&'static Emitter);

impl Drop for Deregister {
    fn drop(&mut self) {
        let context = std::ptr::from_ref(self.0).cast_mut().cast::<c_void>();
        // SAFETY: the same callback and context `install` registered.
        unsafe { CGDisplayRemoveReconfigurationCallback(Some(reconfigured), context) };
    }
}

pub struct Displays;

impl Source for Displays {
    fn id(&self) -> SourceId {
        SourceId("displays")
    }

    fn provides(&self) -> Vec<Event> {
        vec![Event::DisplayChanged]
    }

    fn register(&mut self, emit: Emitter) -> Result<Registration, StartError> {
        // Leaked, as in the other sources: the callback dereferences it and
        // nothing waits for one in flight. A source starts once per process.
        let emit: &'static Emitter = Box::leak(Box::new(emit));
        let context = std::ptr::from_ref(emit).cast_mut().cast::<c_void>();
        // SAFETY: `emit` is leaked, so the context outlives the registration.
        let status =
            unsafe { CGDisplayRegisterReconfigurationCallback(Some(reconfigured), context) };
        if status != objc2_core_graphics::CGError::Success {
            return Err(StartError {
                name: "displays",
                reason: format!("CoreGraphics refused the callback ({status:?})"),
            });
        }
        Ok(Box::new(Deregister(emit)))
    }
}

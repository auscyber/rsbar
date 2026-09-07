//! Displays being added, removed or rearranged.
//!
//! `NSWorkspace` does not report this — its display notification fires when the
//! *active* display changes, not when the set of them does. Plugging in a
//! monitor arrives only here, and it invalidates every panel's geometry.

use crate::sources::{Emission, Emitter, Source, StartError};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayChangeSummaryFlags, CGDisplayRegisterReconfigurationCallback,
    CGDisplayRemoveReconfigurationCallback,
};
use rsbar_protocol::Event;
use std::cell::RefCell;
use std::ffi::c_void;

thread_local! {
    /// The callback takes a context pointer, but keeping the emitter here
    /// avoids leaking a box for the life of the process. Thread-local because
    /// CoreGraphics delivers on the thread that registered.
    static EMIT: RefCell<Option<Emitter>> = const { RefCell::new(None) };
}

extern "C-unwind" fn reconfigured(
    _display: CGDirectDisplayID,
    flags: CGDisplayChangeSummaryFlags,
    _context: *mut c_void,
) {
    // CoreGraphics announces a change twice: once up front carrying only
    // `BeginConfigurationFlag`, and again afterwards carrying what actually
    // happened. There is no matching "end" flag, so the first pass is
    // identified by that flag and skipped — acting on it would read the old
    // display layout.
    if flags.contains(CGDisplayChangeSummaryFlags::BeginConfigurationFlag) {
        return;
    }
    EMIT.with_borrow(|emit| {
        if let Some(emit) = emit {
            let _ = emit.try_send(Emission::new(Event::DisplayChanged, None));
        }
    });
}

/// Deregisters on drop.
struct Registration;

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: removing the callback registered in `install`.
        unsafe { CGDisplayRemoveReconfigurationCallback(Some(reconfigured), std::ptr::null_mut()) };
        EMIT.with_borrow_mut(|slot| *slot = None);
    }
}

pub struct Displays;

impl Source for Displays {
    fn name(&self) -> &'static str {
        "displays"
    }

    fn provides(&self) -> Vec<Event> {
        vec![Event::DisplayChanged]
    }

    /// Registered on the main thread so the callback lands there, where the
    /// panels it invalidates are owned.
    fn needs_main_thread(&self) -> bool {
        true
    }

    fn install(&mut self, emit: Emitter) -> Result<Box<dyn std::any::Any>, StartError> {
        EMIT.with_borrow_mut(|slot| *slot = Some(emit));
        // SAFETY: the callback reads only the thread-local set above.
        let status = unsafe {
            CGDisplayRegisterReconfigurationCallback(Some(reconfigured), std::ptr::null_mut())
        };
        if status != objc2_core_graphics::CGError::Success {
            return Err(StartError {
                name: "displays",
                reason: format!("CoreGraphics refused the callback ({status:?})"),
            });
        }
        Ok(Box::new(Registration))
    }
}

//! Power source changes, from `IOKit`.

use crate::sources::{Emission, Emitter, Source, StartError};
use objc2_core_foundation::{CFRetained, CFRunLoop, CFRunLoopSource, CFString, CFType};
use rsbar_protocol::Event;
use std::ffi::c_void;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    /// A run loop source that fires whenever the power source situation
    /// changes. Nothing else in `IOKit` reports this without polling.
    fn IOPSNotificationCreateRunLoopSource(
        callback: extern "C-unwind" fn(*mut c_void),
        context: *mut c_void,
    ) -> *mut CFRunLoopSource;

    fn IOPSCopyPowerSourcesInfo() -> *mut CFType;
    fn IOPSGetProvidingPowerSourceType(snapshot: *mut CFType) -> *const CFString;
}

/// Whether the machine is on mains or on battery right now.
///
/// Reported to scripts as `RSBAR_INFO`, so the spelling is part of the
/// interface: `AC` or `BATTERY`.
#[must_use]
pub fn providing_source() -> String {
    // SAFETY: the snapshot is a +1 `CoreFoundation` object released below, and
    // the type string it hands back is borrowed from it.
    unsafe {
        let snapshot = IOPSCopyPowerSourcesInfo();
        if snapshot.is_null() {
            return "UNKNOWN".to_owned();
        }
        let kind = IOPSGetProvidingPowerSourceType(snapshot);
        let name = if kind.is_null() {
            "UNKNOWN".to_owned()
        } else {
            match (*kind).to_string().as_str() {
                "AC Power" => "AC".to_owned(),
                "Battery Power" => "BATTERY".to_owned(),
                other => other.to_owned(),
            }
        };
        skylight::ffi::CFRelease(snapshot);
        name
    }
}

/// Keeps the notification source installed; dropping it stops the events.
struct Watch {
    source: CFRetained<CFRunLoopSource>,
    run_loop: CFRetained<CFRunLoop>,
}

/// Recovers the emitter a notification was registered with.
///
/// # Safety
///
/// `context` must be the pointer `install` passed, on an emitter that is still
/// alive — which is why the one this module makes is leaked.
unsafe fn emitter_from(context: *mut c_void) -> Option<&'static Emitter> {
    // SAFETY: the caller guarantees provenance and liveness.
    unsafe { context.cast::<Emitter>().as_ref() }
}

impl Watch {
    /// Starts reporting power source changes on the current run loop.
    fn install(emit: &'static Emitter) -> Result<Self, StartError> {
        extern "C-unwind" fn changed(context: *mut c_void) {
            // SAFETY: the registration passes the leaked emitter.
            let Some(emit) = (unsafe { emitter_from(context) }) else {
                return;
            };
            let emission = Emission::new(Event::PowerSourceChanged, Some(providing_source()));
            // Dropping beats blocking: this is an `IOKit` callback.
            let _ = emit.try_send(emission);
        }

        // SAFETY: `emit` is leaked, so the context outlives the registration.
        let context = std::ptr::from_ref(emit).cast_mut().cast::<c_void>();
        let source = unsafe { IOPSNotificationCreateRunLoopSource(changed, context) };
        let Some(source) = std::ptr::NonNull::new(source) else {
            return Err(StartError {
                name: "power",
                reason: "IOKit refused a notification source".to_owned(),
            });
        };
        // SAFETY: the call returns a +1 reference we now own.
        let source = unsafe { CFRetained::from_raw(source) };

        let run_loop = CFRunLoop::current().expect("no run loop on this thread");
        run_loop.add_source(Some(&source), unsafe {
            objc2_core_foundation::kCFRunLoopCommonModes
        });
        Ok(Self { source, run_loop })
    }
}

pub struct Power;

impl Source for Power {
    fn name(&self) -> &'static str {
        "power"
    }

    fn provides(&self) -> Vec<Event> {
        vec![Event::PowerSourceChanged]
    }

    fn install(&mut self, emit: Emitter) -> Result<Box<dyn std::any::Any>, StartError> {
        // Leaked for the same reason the volume source leaks its state: the
        // callback dereferences this, and there is no call that waits for one
        // in flight to finish. A source starts at most once per process.
        let emit: &'static Emitter = Box::leak(Box::new(emit));
        Ok(Box::new(Watch::install(emit)?))
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.run_loop.remove_source(Some(&self.source), unsafe {
            objc2_core_foundation::kCFRunLoopCommonModes
        });
    }
}

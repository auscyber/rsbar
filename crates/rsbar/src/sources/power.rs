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

thread_local! {
    /// The `IOKit` callback carries a context pointer, but keeping the handler
    /// here avoids leaking a box for the life of the process.
    static ON_CHANGE: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

impl Watch {
    /// Starts reporting power source changes on the current run loop.
    fn install<F: Fn() + 'static>(handler: F) -> Result<Self, StartError> {
        extern "C-unwind" fn changed(_context: *mut c_void) {
            ON_CHANGE.with_borrow(|handler| {
                if let Some(handler) = handler {
                    handler();
                }
            });
        }

        ON_CHANGE.with_borrow_mut(|slot| *slot = Some(Box::new(handler)));

        // SAFETY: the callback reads only the thread-local set above.
        let source = unsafe { IOPSNotificationCreateRunLoopSource(changed, std::ptr::null_mut()) };
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
        let watch = Watch::install(move || {
            let emission = Emission::new(Event::PowerSourceChanged, Some(providing_source()));
            // Dropping beats blocking: this is an `IOKit` callback.
            let _ = emit.try_send(emission);
        })?;
        Ok(Box::new(watch))
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.run_loop.remove_source(Some(&self.source), unsafe {
            objc2_core_foundation::kCFRunLoopCommonModes
        });
    }
}

//! Power source changes, from `IOKit`.

use crate::sources::{
    CallbackState, Cause, Emitter, Registering, Registration, Source, SourceId, StartError,
};
use objc2_core_foundation::{CFRetained, CFRunLoop, CFRunLoopSource, CFString, CFType};
use rsbar_protocol::event::PowerChange;
use rsbar_protocol::{Event, Kind, PowerSource};
use std::collections::BTreeSet;
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
#[must_use]
pub fn providing_source() -> PowerSource {
    // SAFETY: the snapshot is a +1 `CoreFoundation` object released below, and
    // the type string it hands back is borrowed from it.
    unsafe {
        let snapshot = IOPSCopyPowerSourcesInfo();
        if snapshot.is_null() {
            return PowerSource::Unknown;
        }
        let kind = IOPSGetProvidingPowerSourceType(snapshot);
        let name = if kind.is_null() {
            PowerSource::Unknown
        } else {
            match (*kind).to_string().as_str() {
                "AC Power" => PowerSource::Ac,
                "Battery Power" => PowerSource::Battery,
                _ => PowerSource::Unknown,
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

impl Watch {
    /// Starts reporting power source changes on the current run loop.
    fn install(state: &CallbackState<Emitter>) -> Result<Self, StartError> {
        extern "C-unwind" fn changed(context: *mut c_void) {
            // SAFETY: the registration passed a `CallbackState<Emitter>` pointer.
            let Some(emit) = (unsafe { CallbackState::<Emitter>::recover(context) }) else {
                return;
            };
            let event = Event::PowerSourceChanged(PowerChange {
                power_source: providing_source(),
            });
            // Dropping beats blocking: this is an `IOKit` callback.
            emit.send(event);
        }

        let source = state.with_ptr(|context| {
            // SAFETY: the state outlives the registration; see `CallbackState`.
            unsafe { IOPSNotificationCreateRunLoopSource(changed, context) }
        });
        let Some(source) = std::ptr::NonNull::new(source) else {
            return Err(StartError::new(SourceId("power"), Cause::IoKit));
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
    fn id(&self) -> SourceId {
        SourceId("power")
    }

    fn provides(&self) -> Vec<Kind> {
        vec![Kind::PowerSourceChanged]
    }

    fn register(
        &mut self,
        _wanted: &BTreeSet<Kind>,
        cx: &mut Registering<'_>,
    ) -> Result<Registration, StartError> {
        let emit = cx.emitter();
        let state = CallbackState::new(emit);
        Ok(Box::new(Watch::install(&state)?))
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.run_loop.remove_source(Some(&self.source), unsafe {
            objc2_core_foundation::kCFRunLoopCommonModes
        });
    }
}

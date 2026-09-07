//! Display brightness changes, from `DisplayServices`.
//!
//! Verified against the real hardware: a spike binary registered exactly this
//! way, a synthetic `NX_KEYTYPE_BRIGHTNESS_*` key event was posted through
//! `CGEventPost` (what the F1/F2 keys actually send), and the callback fired
//! with the display's new value both times.
//!
//! # The passthrough is not a context pointer
//!
//! Every other source in this module hands the framework a pointer to a
//! [`CallbackState`] and recovers it inside the callback. `DisplayServices`
//! will not carry one: `DisplayServicesRegisterForBrightnessChangeNotifications`
//! takes its passthrough as `uint32_t`, not `void*`, and the callback hands it
//! back in that same width — there is no room for a 64-bit pointer. What
//! `SketchyBar` does, and what this does too, is pass the display id itself as
//! the passthrough, so the callback already has the one thing it needs to
//! look up the brightness. The [`Emitter`] then has nowhere to travel through
//! the callback's arguments, so it lives in a process-wide cell instead —
//! sound here only because this source is registered once for the process's
//! one main display, never per-instance.

use crate::sources::{Cause, Emitter, Registering, Registration, Source, SourceId, StartError};
use objc2_core_graphics::{CGDirectDisplayID, CGError, CGMainDisplayID};
use rsbar_protocol::event::BrightnessChange;
use rsbar_protocol::{Event, Kind};
use std::ffi::c_void;

#[link(name = "DisplayServices", kind = "framework")]
unsafe extern "C" {
    fn DisplayServicesRegisterForBrightnessChangeNotifications(
        display: CGDirectDisplayID,
        passthrough: u32,
        callback: Callback,
    ) -> CGError;

    fn DisplayServicesUnregisterForBrightnessChangeNotifications(
        display: CGDirectDisplayID,
        passthrough: u32,
    ) -> CGError;

    fn DisplayServicesGetBrightness(display: CGDirectDisplayID, brightness: *mut f32) -> CGError;

    /// Not an error code despite the C declaration: any non-zero return means
    /// "yes". Verified on the machine's main display, which returns `1`.
    fn DisplayServicesCanChangeBrightness(display: CGDirectDisplayID) -> i32;
}

/// The shape `DisplayServices` actually calls back with: a `CFNotificationCallback`
/// whose "observer" slot is the passthrough value from registration, here the
/// display id rather than a pointer.
type Callback = extern "C-unwind" fn(
    center: *mut c_void,
    display: CGDirectDisplayID,
    name: *mut c_void,
    sender: *const c_void,
    info: *mut c_void,
);

/// Nowhere else for the sink to live; see the module docs.
static EMITTER: tokio::sync::OnceCell<Emitter> = tokio::sync::OnceCell::const_new();

/// A 0.0-1.0 scalar as a whole percentage, following `volume`'s reading of one.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn percent(scalar: f32) -> u8 {
    (scalar * 100.0).round().clamp(0.0, 100.0) as u8
}

fn current_brightness(display: CGDirectDisplayID) -> Option<u8> {
    let mut scalar: f32 = 0.0;
    // SAFETY: `scalar` is a valid f32 out-pointer for the duration of the call.
    let status = unsafe { DisplayServicesGetBrightness(display, &raw mut scalar) };
    (status == CGError::Success).then(|| percent(scalar))
}

extern "C-unwind" fn changed(
    _center: *mut c_void,
    display: CGDirectDisplayID,
    _name: *mut c_void,
    _sender: *const c_void,
    _info: *mut c_void,
) {
    let Some(emit) = EMITTER.get() else {
        return;
    };
    let Some(brightness) = current_brightness(display) else {
        return;
    };
    emit.send(Event::BrightnessChanged(BrightnessChange { brightness }));
}

/// Deregisters on drop.
struct Watch {
    display: CGDirectDisplayID,
}

impl Drop for Watch {
    fn drop(&mut self) {
        // SAFETY: the same display and passthrough the registration used.
        unsafe {
            DisplayServicesUnregisterForBrightnessChangeNotifications(self.display, self.display);
        }
    }
}

pub struct Brightness;

impl Source for Brightness {
    fn id(&self) -> SourceId {
        SourceId("brightness")
    }

    fn provides(&self) -> Vec<Kind> {
        vec![Kind::BrightnessChanged]
    }

    fn register(&mut self, cx: &mut Registering<'_>) -> Result<Registration, StartError> {
        let emit = cx.emitter();
        let display = CGMainDisplayID();

        // SAFETY: `display` is a live display id.
        if unsafe { DisplayServicesCanChangeBrightness(display) } == 0 {
            return Err(StartError::new(self.id(), Cause::NoBrightnessControl));
        }

        // Only ever set once: this source registers on one display for the
        // life of the process. See the module docs.
        let _ = EMITTER.set(emit);

        // SAFETY: `display` is passed back as the passthrough, which is what
        // the callback's `display` parameter recovers it from.
        let status = unsafe {
            DisplayServicesRegisterForBrightnessChangeNotifications(display, display, changed)
        };
        if status != CGError::Success {
            return Err(StartError::new(self.id(), Cause::CoreGraphics(status)));
        }

        Ok(Box::new(Watch { display }))
    }
}

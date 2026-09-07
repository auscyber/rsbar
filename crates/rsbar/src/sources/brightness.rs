//! Display brightness changes, from `DisplayServices`.
//!
//! Verified against the real hardware: `examples/source_probe.rs` registers
//! exactly this way, calls `DisplayServicesSetBrightness` on the main display
//! (the sibling of the getter and the notification this observes) to nudge it
//! up and back down, and the callback fired both times with the display's
//! real new value. Also verified through a stop/start cycle — drop the only
//! claim, take a fresh one, trigger again — which is what caught the sink
//! going stale; see the module-level note on why the cell is repointed.
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
//! the callback's arguments, so it lives in a process-wide cell instead.
//!
//! That cell has to be *repointed*, not just set once: the registry starts and
//! stops this source lazily as items subscribe and unsubscribe, and
//! `DisplayServicesUnregisterForBrightnessChangeNotifications` genuinely
//! deregisters (unlike `SkyLight`'s notify procs), so a later `register()` is
//! a real re-registration, not a formality. A `OnceCell` set only on the first
//! call would leave every later registration's callback writing into a sink
//! whose receiver had already been dropped — the exact "registers, reports
//! success, and never fires" failure this crate watches for. A lock that can
//! be overwritten is what makes each registration's sink the one the callback
//! actually uses.
//!
//! `tokio::sync::Mutex` rather than `std::sync::Mutex`: the crate's rule is
//! `tokio::sync` for locks, and `blocking_lock` is the documented way to use
//! one outside an async context, which a `DisplayServices` callback always is.

use crate::sources::{Cause, Emitter, Registering, Registration, Source, SourceId, StartError};
use objc2_core_graphics::{CGDirectDisplayID, CGError, CGMainDisplayID};
use rsbar_protocol::event::BrightnessChange;
use rsbar_protocol::{Event, Kind};
use std::collections::BTreeSet;
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

/// Nowhere else for the sink to live; see the module docs. Repointed on every
/// registration, not just the first.
static EMITTER: tokio::sync::Mutex<Option<Emitter>> = tokio::sync::Mutex::const_new(None);

/// The last percentage reported, so a callback that fires without the value
/// actually moving — `DisplayServices` does, e.g. auto-brightness settling
/// back on the same rounded percent it started at — does not turn into a
/// script run and a repaint for a number nobody would see change. Reset
/// alongside `EMITTER` on every registration, for the same reason: a stale
/// value here would suppress the first real event after a restart.
static LAST_PERCENT: tokio::sync::Mutex<Option<u8>> = tokio::sync::Mutex::const_new(None);

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
    // Called from `DisplayServices`, never from async code — a blocking lock
    // is the right tool, and the only one available.
    let Some(emit) = EMITTER.blocking_lock().clone() else {
        return;
    };
    let Some(brightness) = current_brightness(display) else {
        return;
    };

    let mut last = LAST_PERCENT.blocking_lock();
    if *last == Some(brightness) {
        return;
    }
    *last = Some(brightness);
    drop(last);

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
        // Not strictly required — the callback is unregistered above — but
        // leaves nothing for a stray late callback to send into a channel
        // whose receiver is gone.
        *EMITTER.blocking_lock() = None;
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

    fn register(
        &mut self,
        _wanted: &BTreeSet<Kind>,
        cx: &mut Registering<'_>,
    ) -> Result<Registration, StartError> {
        let emit = cx.emitter();
        let display = CGMainDisplayID();

        // SAFETY: `display` is a live display id.
        if unsafe { DisplayServicesCanChangeBrightness(display) } == 0 {
            return Err(StartError::new(self.id(), Cause::NoBrightnessControl));
        }

        // Repointed on every registration — see the module docs for why a
        // one-shot `OnceCell` would go stale across a stop/start cycle.
        *EMITTER.blocking_lock() = Some(emit);
        // A stale value here would wrongly suppress the first event after a
        // restart, if the display happens to have settled back on the exact
        // percent it was last seen at.
        *LAST_PERCENT.blocking_lock() = None;

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

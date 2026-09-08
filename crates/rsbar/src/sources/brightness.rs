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
//! Every other source here hands the framework a pointer to its state and
//! reaches it again through the trampoline. `DisplayServices` will not carry
//! one: `DisplayServicesRegisterForBrightnessChangeNotifications` takes its
//! passthrough as `uint32_t`, not `void*`, and the callback hands it back in
//! that same width — no room for a 64-bit pointer. What `SketchyBar` does, and
//! what this does too, is pass the display id itself, so the callback already
//! has the one thing it needs to look up the brightness.
//!
//! So the pointer lives in a static instead of in the framework, and nothing
//! else changes: [`CONTEXT`] holds exactly what the registration produced, the
//! trampoline reads it and goes through
//! [`Callback::with`](skylight::callback::Callback::with) like every other
//! trampoline here, and a callback arriving after the registration has gone
//! finds a null — or state that has been released — and does nothing.
//!
//! # Nothing is remembered in the callback
//!
//! `DisplayServices` fires without the value actually moving — auto-brightness
//! settling back on the same rounded percent it started at — and each of those
//! would otherwise be a script run and a repaint for a number nobody would see
//! change. So something has to remember the last percentage.
//!
//! The callback posts the display id and returns; [`report`] holds the last
//! percentage as a local across its `.await`. Putting it behind the context
//! pointer instead would mean a lock taken from a C callback — it cannot hold
//! anything between calls and has nowhere to `.await`, which rules out the
//! tokio lock and leaves a `std::sync::Mutex` just to compare two integers.

use crate::sources::{Cause, Emitter, Registering, Source, SourceId, StartError};
use objc2_core_graphics::CGDirectDisplayID;
use rsbar_protocol::event::BrightnessChange;
use rsbar_protocol::{Event, Kind};
use skylight::Display;
use skylight::callback::{Callback, Relay};
use std::collections::BTreeSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering};

/// Where the trampoline finds the pointer the framework would not carry.
///
/// Null while nothing is registered. One registration at a time, which is the
/// registry's own contract: it drops the previous running source's `Task`
/// before asking for another, so the store below never overwrites a live one.
static CONTEXT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Reports the brightness changes that actually changed something.
///
/// Ends on its own when the registration goes: the last [`Relay`] goes with
/// it, `next` answers `None`, and this returns.
async fn report(mut changes: skylight::callback::Events<CGDirectDisplayID>, emit: Emitter) {
    let mut last: Option<u8> = None;
    while let Some(display) = changes.next().await {
        let Some(brightness) = current_brightness(Display::new(display)) else {
            continue;
        };
        if last == Some(brightness) {
            continue;
        }
        last = Some(brightness);
        emit.send(Event::BrightnessChanged(BrightnessChange { brightness }));
    }
}

/// A 0.0-1.0 scalar as a whole percentage, following `volume`'s reading of one.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn percent(scalar: f32) -> u8 {
    (scalar * 100.0).round().clamp(0.0, 100.0) as u8
}

fn current_brightness(display: Display) -> Option<u8> {
    display.brightness().ok().map(percent)
}

/// **The one hand-written trampoline in this module tree**, and the reason is a
/// fact about `DisplayServices` rather than a gap in
/// [`skylight::callback::trampoline`]: every builder there recovers the state
/// from a context *argument*, and none of this callback's five arguments is one
/// — the passthrough is a `uint32_t` fixed to the display id. So this reads the
/// pointer from [`CONTEXT`] instead, and everything past that is
/// [`Callback::with`](skylight::callback::Callback::with)'s: the null check, the upgrade that holds the state for the
/// length of the call, and doing nothing for a registration that has gone.
extern "C-unwind" fn changed(
    _center: *mut c_void,
    display: CGDirectDisplayID,
    _name: *mut c_void,
    _sender: *const c_void,
    _info: *mut c_void,
) {
    // SAFETY: the static holds the pointer the live registration produced for
    // its relay, or null once the teardown has cleared it -- and `with`
    // answers `None` for null rather than dereferencing it.
    let _ = unsafe {
        Callback::<Relay<CGDirectDisplayID>>::with(CONTEXT.load(Ordering::Acquire), |relay| {
            relay.post(display);
        })
    };
}

pub struct Brightness;

impl Source for Brightness {
    fn id(&self) -> SourceId {
        SourceId("brightness")
    }

    fn provides(&self) -> Vec<Kind> {
        vec![Kind::BrightnessChanged]
    }

    fn run(
        &mut self,
        _wanted: &BTreeSet<Kind>,
        cx: Registering,
    ) -> Result<crate::pool::Task, StartError> {
        let display = Display::main();
        if !display.can_change_brightness() {
            return Err(StartError::new(self.id(), Cause::NoBrightnessControl));
        }

        let emit = cx.emitter();
        let id = self.id();

        // `!Send` means "build it where it will live", not "put it on the
        // main thread": `DisplayServicesGetBrightness` and the notification
        // it rides are plain `CoreGraphics`, needing no run loop of AppKit's,
        // so the registration is built on `pool::sources()`'s thread rather
        // than the one that draws the bar. The `CONTEXT` store/load pair
        // below is Release/Acquire, which orders correctly across any two
        // threads -- which thread does the storing was never load-bearing.
        //
        // The cost: a registration failure past this point can no longer
        // come back through this `run`'s `Result`, since building it happens
        // after this function has already returned `Ok`. Logged instead —
        // the same choice `wifi` and `power` make.
        Ok(crate::pool::sources().spawn(move |_here| async move {
            let (relay, changes) = skylight::callback::relay::<CGDirectDisplayID>();
            // `DisplayServicesUnregisterForBrightnessChangeNotifications`
            // matches on the display and the passthrough -- both this
            // display's id -- so the display is the whole of what the
            // teardown needs.
            let watch = match Callback::new(relay, |context| {
                // Before the registration, so a notification arriving during
                // the call already has somewhere to report.
                CONTEXT.store(context, Ordering::Release);
                match display.watch_brightness(changed) {
                    Ok(()) => Ok(move || {
                        if let Err(error) = display.unwatch_brightness() {
                            tracing::warn!(%error, "brightness notification would not deregister");
                        }
                        // Leaves a stray late callback with nothing to find.
                        CONTEXT.store(std::ptr::null_mut(), Ordering::Release);
                    }),
                    Err(error) => {
                        CONTEXT.store(std::ptr::null_mut(), Ordering::Release);
                        Err(match error {
                            skylight::Error::Brightness(status) => Cause::CoreGraphics(status),
                            _ => Cause::NoBrightnessControl,
                        })
                    }
                }
            }) {
                Ok(watch) => watch,
                Err(cause) => {
                    let err = StartError::new(id, cause);
                    tracing::error!(%err, "brightness source could not register on its own thread");
                    return;
                }
            };
            let _watch = watch;
            report(changes, emit).await;
        }))
    }
}

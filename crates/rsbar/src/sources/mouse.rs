//! Clicks and scrolls on the bar.
//!
//! Carbon, not a `CGEventTap`. A tap needs the Input Monitoring permission and
//! sees every event in the system; Carbon's queue holds only what was routed to
//! this process, which for a window server window is what landed on the bar.
//! `SketchyBar` reaches them the same way.
//!
//! Two things have to be true before anything arrives. The bar window must be
//! hit-testable — `WindowTags::OPAQUE_FOR_EVENTS`, set the moment something
//! wants a click — and the process must *not* register itself as an
//! `NSApplication`. That last one is backwards from the obvious guess: doing it
//! makes a click terminate the process with a clean exit status, and Carbon
//! delivers perfectly well without it.
//!
//! # Why this pulls rather than installs a handler
//!
//! `InstallEventHandler` is the obvious route and does not work here. The
//! handler only runs for events something has received and sent on, and the
//! thing that normally does that is `RunApplicationEventLoop` — which never
//! returns, so it cannot be this daemon's loop.
//!
//! Pulling from the queue works, but only with a filter. `ReceiveNextEvent`
//! takes a type list; asking for mouse events alone leaves everything else
//! queued for whoever owns it. Draining indiscriminately exits the process,
//! because some of what arrives means quit. And once the event is in hand there
//! is nothing to dispatch it to, which is why there is no handler at all.

use crate::sources::{Cause, Emitter, Registration, Source, SourceId, StartError};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{CGEvent, CGEventField, CGEventFlags};
use rsbar_protocol::event::{MouseClick, Scroll};
use rsbar_protocol::{Event, Kind, Modifiers, MouseButton};
use std::cell::RefCell;
use std::ffi::c_void;

type OsStatus = i32;
type EventRef = *mut c_void;

/// `'mous'`, the four-character code for the mouse event class.
const CLASS_MOUSE: u32 = u32::from_be_bytes(*b"mous");
/// `kEventMouseUp`. A click completes on release, which is the event
/// `SketchyBar` acts on too.
const KIND_MOUSE_UP: u32 = 2;
const KIND_MOUSE_WHEEL_MOVED: u32 = 10;

/// `kEventDurationNoWait`. The runner has already slept; this takes what is
/// there and returns.
const NO_WAIT: f64 = 0.0;

#[repr(C)]
#[derive(Clone, Copy)]
struct EventTypeSpec {
    class: u32,
    kind: u32,
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn ReceiveNextEvent(
        num_types: u32,
        type_list: *const EventTypeSpec,
        timeout: f64,
        pull_event: bool,
        event: *mut EventRef,
    ) -> OsStatus;
    fn ReleaseEvent(event: EventRef);
    fn GetEventKind(event: EventRef) -> u32;
    /// The `CGEvent` behind a Carbon one, which is where the usable accessors
    /// are. Returns a +1 reference.
    fn CopyEventCGEvent(event: EventRef) -> Option<CFRetained<CGEvent>>;
}

thread_local! {
    /// Where pumped events go.
    ///
    /// A thread-local is right here in a way it was not for the audio source:
    /// Carbon's queue belongs to the main thread, and both the pump and the
    /// registration run on it. Nothing else can reach this.
    static EMIT: RefCell<Option<Emitter>> = const { RefCell::new(None) };
}

/// The modifier keys held, from the event's own flags.
fn modifiers_of(event: &CGEvent) -> Modifiers {
    let flags = CGEvent::flags(Some(event));
    let mut modifiers = Modifiers::empty();
    modifiers.set(Modifiers::SHIFT, flags.contains(CGEventFlags::MaskShift));
    modifiers.set(Modifiers::CTRL, flags.contains(CGEventFlags::MaskControl));
    modifiers.set(Modifiers::ALT, flags.contains(CGEventFlags::MaskAlternate));
    modifiers.set(Modifiers::CMD, flags.contains(CGEventFlags::MaskCommand));
    modifiers.set(Modifiers::FN, flags.contains(CGEventFlags::MaskSecondaryFn));
    modifiers
}

/// Turns one Carbon event into ours, if it is one we report.
///
/// Reads through the `CGEvent` the Carbon event wraps rather than through
/// `GetEventParameter`. The parameter route works but is easy to get subtly
/// wrong: `typeHIPoint` is a `CGFloat` pair, so asking for it with a pair of
/// `f32`s reads half a coordinate and silently reports every click at the same
/// place — which looks exactly like broken hit-testing. `CGEventGetLocation`
/// has one meaning.
#[allow(
    clippy::cast_precision_loss,
    reason = "a scroll delta is a small integer"
)]
fn decode(event: EventRef) -> Option<Event> {
    // SAFETY: a live Carbon event for the duration of this call.
    let cg = unsafe { CopyEventCGEvent(event) }?;
    // SAFETY: as above.
    let kind = unsafe { GetEventKind(event) };

    let at = CGEvent::location(Some(&cg));
    let modifiers = modifiers_of(&cg);

    match kind {
        KIND_MOUSE_UP => {
            let button =
                match CGEvent::integer_value_field(Some(&cg), CGEventField::MouseEventButtonNumber)
                {
                    0 => MouseButton::Left,
                    1 => MouseButton::Right,
                    _ => MouseButton::Other,
                };
            Some(Event::MouseClicked(MouseClick {
                button,
                modifiers,
                x: at.x,
                y: at.y,
            }))
        }
        KIND_MOUSE_WHEEL_MOVED => {
            let delta =
                CGEvent::integer_value_field(Some(&cg), CGEventField::ScrollWheelEventDeltaAxis1);
            Some(Event::MouseScrolled(Scroll {
                scroll_delta: delta as f64,
                modifiers,
                x: at.x,
                y: at.y,
            }))
        }
        _ => None,
    }
}

/// Takes the mouse events waiting in Carbon's queue.
///
/// Called by the runner after each wake. A no-op until something has asked for
/// clicks, because nothing registers an emitter until then — so a bar nobody
/// clicks never touches the queue at all.
pub fn pump() {
    let Some(emit) = EMIT.with_borrow(Clone::clone) else {
        return;
    };

    let wanted = [
        EventTypeSpec {
            class: CLASS_MOUSE,
            kind: KIND_MOUSE_UP,
        },
        EventTypeSpec {
            class: CLASS_MOUSE,
            kind: KIND_MOUSE_WHEEL_MOVED,
        },
    ];

    loop {
        let mut event: EventRef = std::ptr::null_mut();
        // SAFETY: `wanted` outlives the call and `event` is a valid
        // out-pointer. The type list is what leaves every other event queued.
        let status = unsafe {
            ReceiveNextEvent(
                u32::try_from(wanted.len()).unwrap_or(0),
                wanted.as_ptr(),
                NO_WAIT,
                true,
                &raw mut event,
            )
        };
        if status != 0 || event.is_null() {
            return;
        }

        if let Some(decoded) = decode(event) {
            emit.send(decoded);
        }
        // SAFETY: received with `pull_event`, so we own it; released once.
        unsafe { ReleaseEvent(event) };
    }
}

/// Stops the pump when the source stops.
struct Registered;

impl Drop for Registered {
    fn drop(&mut self) {
        EMIT.with_borrow_mut(|slot| *slot = None);
    }
}

pub struct Mouse;

impl Source for Mouse {
    fn id(&self) -> SourceId {
        SourceId("mouse")
    }

    fn provides(&self) -> Vec<Kind> {
        vec![
            Kind::MouseClicked,
            Kind::MouseScrolled,
            Kind::MouseScrolledGlobal,
            Kind::MouseEntered,
            Kind::MouseExited,
            Kind::MouseEnteredGlobal,
            Kind::MouseExitedGlobal,
        ]
    }

    fn register(&mut self, emit: Emitter) -> Result<Registration, StartError> {
        if objc2::MainThreadMarker::new().is_none() {
            return Err(StartError::new(self.id(), Cause::NotMainThread));
        }
        EMIT.with_borrow_mut(|slot| *slot = Some(emit));
        Ok(Box::new(Registered))
    }
}

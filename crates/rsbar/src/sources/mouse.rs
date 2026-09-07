//! Clicks and scrolls on the bar.
//!
//! **Incomplete: the handler installs but never fires.** Everything downstream
//! of it works — the bar is hit-testable, the layout is retained, and a click
//! is routed to the one item under it — but the events are not arriving yet.
//! What is established so far:
//!
//! - Clicks on the bar are not delivered as `NSEvent`s. Draining `AppKit`'s queue
//!   yields only an `AppKitDefined` event at startup, never a mouse event, so
//!   an `NSEvent` monitor is not the answer either.
//! - `SketchyBar` gets them through this same Carbon handler, but it calls
//!   `RunApplicationEventLoop`, which drives Carbon's own queue. This daemon
//!   cannot: that call never returns, so it cannot be the app's loop.
//! - Draining Carbon's queue by hand — `ReceiveNextEvent` then
//!   `SendEventToEventTarget` — does deliver, but sending *every* received
//!   event to the dispatcher exits the process, because some of what arrives
//!   means quit. It needs filtering to the mouse class, or a different
//!   mechanism entirely (`SLSRegisterConnectionNotifyProc`, or the tracking
//!   rects `SLSAddTrackingRect` already exposes).
//!
//! Carbon, not a `CGEventTap`. A tap needs the Input Monitoring permission and
//! sees every event in the system; the Carbon dispatcher delivers only what was
//! actually routed to this process, which for a window server window means the
//! clicks that landed on the bar. `SketchyBar` reaches it the same way.
//!
//! The bar window has to be hit-testable for any of this to arrive — see
//! `WindowTags::OPAQUE_FOR_EVENTS` in the panel setup.

use crate::sources::{CallbackState, Cause, Emitter, Registration, Source, SourceId, StartError};
use objc2_core_foundation::CGPoint;
use rsbar_protocol::event::{MouseClick, Scroll};
use rsbar_protocol::{Event, Kind, Modifiers, MouseButton};
use std::ffi::c_void;

type OsStatus = i32;
type EventRef = *mut c_void;
type EventHandlerRef = *mut c_void;
type EventTargetRef = *mut c_void;
type EventHandlerCallRef = *mut c_void;

/// `'mous'`, the four-character code for the mouse event class.
const CLASS_MOUSE: u32 = u32::from_be_bytes(*b"mous");
const KIND_MOUSE_DOWN: u32 = 1;
const KIND_MOUSE_WHEEL_MOVED: u32 = 10;

/// `kEventParamMouseLocation` / `typeHIPoint`, and friends.
const PARAM_MOUSE_LOCATION: u32 = u32::from_be_bytes(*b"mloc");
const PARAM_MOUSE_BUTTON: u32 = u32::from_be_bytes(*b"mbtn");
const PARAM_KEY_MODIFIERS: u32 = u32::from_be_bytes(*b"kmod");
const PARAM_WHEEL_DELTA: u32 = u32::from_be_bytes(*b"whdy");
const TYPE_HI_POINT: u32 = u32::from_be_bytes(*b"hipt");
const TYPE_MOUSE_BUTTON: u32 = u32::from_be_bytes(*b"mbtn");
const TYPE_UINT32: u32 = u32::from_be_bytes(*b"magn");
const TYPE_SINT32: u32 = u32::from_be_bytes(*b"long");

/// Carbon's modifier bits, which are not the same as `NSEvent`'s.
const MOD_SHIFT: u32 = 1 << 9;
const MOD_CONTROL: u32 = 1 << 12;
const MOD_OPTION: u32 = 1 << 11;
const MOD_COMMAND: u32 = 1 << 8;

#[repr(C)]
#[derive(Clone, Copy)]
struct EventTypeSpec {
    class: u32,
    kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct HiPoint {
    x: f32,
    y: f32,
}

type EventHandlerProc =
    extern "C-unwind" fn(EventHandlerCallRef, EventRef, *mut c_void) -> OsStatus;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetEventDispatcherTarget() -> EventTargetRef;
    fn InstallEventHandler(
        target: EventTargetRef,
        handler: EventHandlerProc,
        num_types: u32,
        type_list: *const EventTypeSpec,
        user_data: *mut c_void,
        handler_ref: *mut EventHandlerRef,
    ) -> OsStatus;
    fn RemoveEventHandler(handler_ref: EventHandlerRef) -> OsStatus;
    fn GetEventKind(event: EventRef) -> u32;
    fn GetEventParameter(
        event: EventRef,
        name: u32,
        desired_type: u32,
        actual_type: *mut u32,
        buffer_size: usize,
        actual_size: *mut usize,
        buffer: *mut c_void,
    ) -> OsStatus;
}

/// Reads one parameter off a Carbon event.
///
/// The only `unsafe` in this module's event handling, wrapped once: every
/// caller wants the same "give me a `T` or nothing".
fn parameter<T: Copy + Default>(event: EventRef, name: u32, kind: u32) -> Option<T> {
    let mut value = T::default();
    // SAFETY: `value` is exactly the size passed, and the callee writes at most
    // that much. A mismatched type is reported rather than written.
    let status = unsafe {
        GetEventParameter(
            event,
            name,
            kind,
            std::ptr::null_mut(),
            size_of::<T>(),
            std::ptr::null_mut(),
            (&raw mut value).cast::<c_void>(),
        )
    };
    (status == 0).then_some(value)
}

fn modifiers_of(event: EventRef) -> Modifiers {
    let raw: u32 = parameter(event, PARAM_KEY_MODIFIERS, TYPE_UINT32).unwrap_or(0);
    let mut modifiers = Modifiers::empty();
    modifiers.set(Modifiers::SHIFT, raw & MOD_SHIFT != 0);
    modifiers.set(Modifiers::CTRL, raw & MOD_CONTROL != 0);
    modifiers.set(Modifiers::ALT, raw & MOD_OPTION != 0);
    modifiers.set(Modifiers::CMD, raw & MOD_COMMAND != 0);
    modifiers
}

fn location_of(event: EventRef) -> CGPoint {
    let point: HiPoint = parameter(event, PARAM_MOUSE_LOCATION, TYPE_HI_POINT).unwrap_or_default();
    CGPoint::new(f64::from(point.x), f64::from(point.y))
}

extern "C-unwind" fn dispatch(
    _call: EventHandlerCallRef,
    event: EventRef,
    context: *mut c_void,
) -> OsStatus {
    // SAFETY: the registration passed a `CallbackState<Emitter>` pointer.
    let Some(emit) = (unsafe { CallbackState::<Emitter>::recover(context) }) else {
        // `eventNotHandledErr`, so the event carries on to whoever else wants it.
        return -9874;
    };

    // SAFETY: a live event for the duration of this call.
    let kind = unsafe { GetEventKind(event) };
    let at = location_of(event);
    let modifiers = modifiers_of(event);

    match kind {
        KIND_MOUSE_DOWN => {
            let button = match parameter::<u16>(event, PARAM_MOUSE_BUTTON, TYPE_MOUSE_BUTTON) {
                Some(1) => MouseButton::Left,
                Some(2) => MouseButton::Right,
                _ => MouseButton::Other,
            };
            emit.send(Event::MouseClicked(MouseClick {
                button,
                modifiers,
                x: at.x,
                y: at.y,
            }));
        }
        KIND_MOUSE_WHEEL_MOVED => {
            let delta: i32 = parameter(event, PARAM_WHEEL_DELTA, TYPE_SINT32).unwrap_or(0);
            emit.send(Event::MouseScrolled(Scroll {
                scroll_delta: f64::from(delta),
                modifiers,
                x: at.x,
                y: at.y,
            }));
        }
        _ => {}
    }

    // Not handled, deliberately: the bar reacting to a click should not stop
    // anything else seeing it.
    -9874
}

/// Removes the handler when the source stops.
struct Handler {
    handler: EventHandlerRef,
    _state: CallbackState<Emitter>,
}

impl Drop for Handler {
    fn drop(&mut self) {
        // SAFETY: the reference `register` installed, removed once.
        unsafe { RemoveEventHandler(self.handler) };
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
        let state = CallbackState::new(emit);
        let types = [
            EventTypeSpec {
                class: CLASS_MOUSE,
                kind: KIND_MOUSE_DOWN,
            },
            EventTypeSpec {
                class: CLASS_MOUSE,
                kind: KIND_MOUSE_WHEEL_MOVED,
            },
        ];

        let mut handler: EventHandlerRef = std::ptr::null_mut();
        let status = state.with_ptr(|context| {
            // SAFETY: `types` outlives the call, and the state outlives the
            // handler because `Handler` holds it.
            unsafe {
                InstallEventHandler(
                    GetEventDispatcherTarget(),
                    dispatch,
                    u32::try_from(types.len()).unwrap_or(0),
                    types.as_ptr(),
                    context,
                    &raw mut handler,
                )
            }
        });

        if status != 0 || handler.is_null() {
            return Err(StartError::new(self.id(), Cause::Carbon(status)));
        }
        Ok(Box::new(Handler {
            handler,
            _state: state,
        }))
    }
}

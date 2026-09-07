//! Clicks, scrolls and hover on the bar.
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
//! `mouse.c` (`SketchyBar`'s whole mouse source, 37 lines) installs a handler
//! on `GetEventDispatcherTarget()` with `InstallEventHandler` and chains to
//! `CallNextEventHandler`, rather than pulling from the queue. Tried here,
//! built against the identical `EventTypeSpec` list, and confirmed live
//! against a synthesised click: it never once fired. Not "fired and reached
//! the wrong item" — the handler function itself was never entered, checked
//! by a debug log at its first line. `InstallEventHandler` only registers a
//! handler; something still has to take the event off the queue and send it
//! to that target, and the thing that does that is `RunApplicationEventLoop`
//! — which never returns, so it cannot be this daemon's loop. `SketchyBar`'s
//! `main` calls exactly that function and never returns from it either; this
//! daemon instead ticks its own schedule from `CFRunLoopRunInMode`, which has
//! no such side effect. So a handler on the dispatcher target here would be
//! reachable and correctly torn down and never once called — worse than the
//! bug this was meant to fix, since at least a poll can be observed to run.
//!
//! Pulling from the queue works, but only with a filter. `ReceiveNextEvent`
//! takes a type list; asking for mouse events alone leaves everything else
//! queued for whoever owns it. Draining indiscriminately exits the process,
//! because some of what arrives means quit. And once the event is in hand there
//! is nothing to dispatch it to, which is why there is no handler at all.
//!
//! This is also, confirmed the same way, not what was silently dropping
//! clicks: a synthesised click landed on `pump` and ran its `click_script`
//! immediately, cold process and all, on every attempt — through `ecs.rs`'s
//! runner, which calls `pump` after every `CFRunLoopRunInMode` wake and bounds
//! the wait itself to one second, so a click is never more than one tick from
//! being seen even with no other traffic to piggyback a wake on. Whatever
//! this daemon's reported click failure actually was, it was not this
//! module's delivery mechanism, at least as of the commit this change sits
//! on — see this change's own notes for what else moved recently.
//!
//! # Hover: `kEventMouseMoved` never arrives; tracking rects do
//!
//! Checked live, both ways. With the bar window hit-testable exactly as it
//! already is for clicks, `kEventMouseMoved` is never once delivered to this
//! process's Carbon queue, however long or however far across the bar the
//! real cursor moves — the window server simply does not route plain movement
//! to a window that never asked for it, and there is no tag for "ask for it".
//!
//! `kEventMouseEntered`/`kEventMouseExited` (8/9) are different: `SkyLight`'s
//! `SLSAddTrackingRect` (bound in `skylight::ffi`, otherwise unused before
//! this) registers a rectangle on a window, and crossing *its* boundary — not
//! the window's — delivers exactly one Carbon event, carrying the crossing
//! point. Two adjacent rectangles on the same window each fire independently:
//! moving from one straight into the other was observed to deliver an exit for
//! the first immediately followed by an enter for the second, at the same
//! point. That is real per-region tracking, not "the bar" the way the module
//! doc here once assumed — `SketchyBar` reads the same two Carbon kinds for
//! the same reason, it only looks like a different mechanism because it gives
//! every item its own window and so tracks per-window instead of per-rect.
//!
//! So this module listens for 8/9 and, on either, records where it happened
//! into [`current_position`] — not which item, this module has no layout to
//! ask. `ecs.rs::track_hover` hit-tests that point against the retained
//! layout and turns a change of hit into `mouse.entered`/`mouse.exited`; see
//! its own doc. Both Carbon kinds behave identically here for that purpose: an
//! exit's point is outside whatever rect was left, an enter's is inside
//! whatever was entered, and a re-hit-test tells the difference either way.
//!
//! **What this module cannot do alone**: it only pumps events already
//! *arriving*, and none arrive until something registers a tracking rect over
//! each item that wants hover, kept in step with that item's on-screen frame.
//! That registration belongs wherever the bar's windows are owned and its
//! layout is recomputed, not here — see this change's own notes for exactly
//! what is missing. Until it lands, `mouse.entered`/`mouse.exited` are
//! correctly plumbed end to end but silent, the same as before this change,
//! for a different and now-diagnosed reason.
//!
//! `kEventMouseMoved` stays out of the type list entirely: asking Carbon for
//! an event kind it will never deliver here is not merely useless, a filter
//! entry is also one more comparison every pump — see [`wants_hover`], kept
//! for when a tracking-rect-driven position needs gating the same way.
//!
//! # `kEventMouseDragged` and `kEventMouseScroll`
//!
//! `mouse.c`'s type list also has these. `kEventMouseScroll` (11) is a second,
//! older way a scroll can arrive — `SketchyBar`'s own translation table maps
//! it to the same scrolled event as `kEventMouseWheelMoved`, and its
//! `CGEvent` carries the same delta field `decode` already reads, so it is
//! pulled here on the same terms: gated with scroll, decoded identically.
//! Leaving it out would silently lose scroll input from whatever still sends
//! the legacy kind.
//!
//! `kEventMouseDragged` (6) is not added. There is no `Kind` in
//! `rsbar_protocol` a decoded drag could become, and no drag state lives in
//! this module to make one meaningful — a start point, a threshold, anything
//! a hypothetical `drag_script` would need. Pulling it would only add an
//! event this loop takes off the queue and throws away on every drag over
//! the bar. If drag support is ever wanted, it starts with a protocol `Kind`
//! to decode into, not with adding the Carbon kind to this list.

use crate::sources::{Cause, Emitter, Registering, Registration, Source, SourceId, StartError};
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{CGEvent, CGEventField, CGEventFlags};
use rsbar_protocol::event::{MouseClick, Scroll};
use rsbar_protocol::{Event, Kind, Modifiers, MouseButton};
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::ffi::c_void;

type OsStatus = i32;
type EventRef = *mut c_void;
type EventHandlerProc = unsafe extern "C-unwind" fn(
    call_ref: *mut c_void,
    event: EventRef,
    data: *mut c_void,
) -> OsStatus;

/// `'mous'`, the four-character code for the mouse event class.
const CLASS_MOUSE: u32 = u32::from_be_bytes(*b"mous");
/// `kEventMouseUp`. A click completes on release, which is the event
/// `SketchyBar` acts on too.
const KIND_MOUSE_UP: u32 = 2;
/// `kEventMouseEntered`/`kEventMouseExited`. Only pulled from the queue while
/// [`wants_hover`] is true — see the module doc.
const KIND_MOUSE_ENTERED: u32 = 8;
const KIND_MOUSE_EXITED: u32 = 9;
/// The two Carbon kinds a scroll can arrive as — see the module doc's note on
/// `kEventMouseDragged`/`kEventMouseScroll`.
const KIND_MOUSE_WHEEL_MOVED: u32 = 10;
const KIND_MOUSE_SCROLL: u32 = 11;

#[repr(C)]
#[derive(Clone, Copy)]
struct EventTypeSpec {
    class: u32,
    kind: u32,
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetEventDispatcherTarget() -> *mut c_void;
    fn InstallEventHandler(
        target: *mut c_void,
        handler: EventHandlerProc,
        count: u32,
        list: *const EventTypeSpec,
        user_data: *mut c_void,
        out_ref: *mut *mut c_void,
    ) -> OsStatus;
    fn RemoveEventHandler(handler: *mut c_void) -> OsStatus;
    fn CallNextEventHandler(call_ref: *mut c_void, event: EventRef) -> OsStatus;

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
    /// Whether `mouse.entered`/`mouse.exited` are currently wanted, kept on
    /// this thread the same as `EMIT` — there is exactly one `Mouse` source
    /// for the process's life, so a thread-local is a field on it in
    /// everything but name.
    static WANTS_HOVER: Cell<bool> = const { Cell::new(false) };
    /// Where a tracking rect was last crossed, in global screen coordinates,
    /// or `None` while hover is not wanted at all.
    /// `ecs.rs::track_hover` hit-tests this against the retained layout;
    /// deciding whether the hit *changed* — the actual dedup — happens
    /// there, where the layout is, not here.
    static LAST_CROSSING: Cell<Option<CGPoint>> = const { Cell::new(None) };
}

/// Where a tracking rect was last crossed, for `ecs.rs::track_hover` to
/// hit-test.
///
/// `None` whenever hover is not wanted, so a tick where nothing subscribes to
/// `mouse.entered`/`mouse.exited` does no hit-testing at all.
#[must_use]
pub fn current_position() -> Option<CGPoint> {
    LAST_CROSSING.with(Cell::get)
}

fn wants_hover(wanted: &BTreeSet<Kind>) -> bool {
    wanted.contains(&Kind::MouseEntered(())) || wanted.contains(&Kind::MouseExited(()))
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
        KIND_MOUSE_WHEEL_MOVED | KIND_MOUSE_SCROLL => {
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
/// One event, straight from the dispatcher.
///
/// Installed on `GetEventDispatcherTarget` rather than pulled off the queue
/// with `ReceiveNextEvent`: this daemon runs Carbon's own
/// `RunApplicationEventLoop`, which dispatches an event to the handlers
/// installed on that target and leaves nothing behind for a poll to find.
/// Chains to whatever was there before, so this observes rather than swallows.
unsafe extern "C-unwind" fn handle(
    call_ref: *mut c_void,
    event: EventRef,
    _data: *mut c_void,
) -> OsStatus {
    if let Some(emit) = EMIT.with_borrow(Clone::clone) {
        // SAFETY: a live Carbon event for the duration of this call.
        let kind = unsafe { GetEventKind(event) };
        if kind == KIND_MOUSE_ENTERED || kind == KIND_MOUSE_EXITED {
            // SAFETY: as above.
            if let Some(cg) = unsafe { CopyEventCGEvent(event) } {
                LAST_CROSSING.with(|last| last.set(Some(CGEvent::location(Some(&cg)))));
            }
        } else if let Some(decoded) = decode(event) {
            emit.send(decoded);
        }
    }
    // SAFETY: `call_ref` and `event` are the ones we were handed.
    unsafe { CallNextEventHandler(call_ref, event) }
}

/// Every mouse kind this source decodes.
///
/// Hover joins only while something wants it, so a cursor crossing tracking
/// rects nobody asked about costs nothing. `kEventMouseMoved` is not here at
/// all -- see the module doc.
const WANTED: [EventTypeSpec; 5] = [
    EventTypeSpec {
        class: CLASS_MOUSE,
        kind: KIND_MOUSE_UP,
    },
    EventTypeSpec {
        class: CLASS_MOUSE,
        kind: KIND_MOUSE_WHEEL_MOVED,
    },
    EventTypeSpec {
        class: CLASS_MOUSE,
        kind: KIND_MOUSE_SCROLL,
    },
    EventTypeSpec {
        class: CLASS_MOUSE,
        kind: KIND_MOUSE_ENTERED,
    },
    EventTypeSpec {
        class: CLASS_MOUSE,
        kind: KIND_MOUSE_EXITED,
    },
];

/// The installed handler, removed when this is dropped.
///
/// `NewEventHandlerUPP` is a macro rather than a symbol on 64-bit -- a UPP is
/// just the function pointer there -- so `handle` is passed straight in.
struct Installed {
    handler: *mut c_void,
}

impl Installed {
    fn new(hover: bool) -> Option<Self> {
        let count = if hover { WANTED.len() } else { 3 };
        let mut handler: *mut c_void = std::ptr::null_mut();
        // SAFETY: `handle` matches `EventHandlerProc`, the type list is a
        // `const` that outlives the call, and `handler` is a valid
        // out-pointer.
        let status = unsafe {
            InstallEventHandler(
                GetEventDispatcherTarget(),
                handle,
                u32::try_from(count).unwrap_or(0),
                WANTED.as_ptr(),
                std::ptr::null_mut(),
                &raw mut handler,
            )
        };
        (status == 0 && !handler.is_null()).then_some(Self { handler })
    }
}

impl Drop for Installed {
    fn drop(&mut self) {
        // SAFETY: produced by `new` and removed exactly once. Unlike several
        // other Apple callbacks this daemon registers, this one really can be
        // removed, so it is -- otherwise a source that stops and starts would
        // leak a handler each time.
        unsafe { RemoveEventHandler(self.handler) };
    }
}

/// Removes the handler and forgets the emitter when the source stops.
struct Registered(#[allow(dead_code, reason = "held to keep the handler installed")] Installed);

impl Drop for Registered {
    fn drop(&mut self) {
        EMIT.with_borrow_mut(|slot| *slot = None);
        WANTS_HOVER.with(|w| w.set(false));
        LAST_CROSSING.with(|last| last.set(None));
    }
}

pub struct Mouse;

impl Source for Mouse {
    fn id(&self) -> SourceId {
        SourceId("mouse")
    }

    fn provides(&self) -> Vec<Kind> {
        vec![
            Kind::MouseClicked(()),
            Kind::MouseScrolled(()),
            Kind::MouseScrolledGlobal,
            Kind::MouseEntered(()),
            Kind::MouseExited(()),
            Kind::MouseEnteredGlobal,
            Kind::MouseExitedGlobal,
        ]
    }

    fn register(
        &mut self,
        wanted: &BTreeSet<Kind>,
        cx: &mut Registering<'_>,
    ) -> Result<Registration, StartError> {
        let emit = cx.emitter();
        if objc2::MainThreadMarker::new().is_none() {
            return Err(StartError::new(self.id(), Cause::NotMainThread));
        }
        let hover = wants_hover(wanted);
        let Some(installed) = Installed::new(hover) else {
            return Err(StartError::new(self.id(), Cause::NotMainThread));
        };
        EMIT.with_borrow_mut(|slot| *slot = Some(emit));
        WANTS_HOVER.with(|w| w.set(hover));
        Ok(Box::new(Registered(installed)))
    }

    fn update(
        &mut self,
        wanted: &BTreeSet<Kind>,
        _current: &mut Registration,
        _cx: &mut Registering<'_>,
    ) -> Result<(), StartError> {
        WANTS_HOVER.with(|w| w.set(wants_hover(wanted)));
        if !wants_hover(wanted) {
            LAST_CROSSING.with(|last| last.set(None));
        }
        Ok(())
    }
}

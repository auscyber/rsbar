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
//! # `InstallEventHandler` needs Carbon's own run loop
//!
//! `mouse.c` (`SketchyBar`'s whole mouse source, 37 lines) installs a handler
//! on `GetEventDispatcherTarget()` with `InstallEventHandler` and chains to
//! `CallNextEventHandler` — the same approach used here. It only works
//! because this daemon's run loop is Carbon's own `RunApplicationEventLoop`
//! (see [`crate::runloop`]), which is what actually takes an event off the
//! queue and dispatches it to the handlers on that target. Confirmed live:
//! installed the same way under a plain `CFRunLoopRunInMode` poll first, and
//! the handler was never once entered — not misrouted, never called at all,
//! checked with a debug log at its first line.
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
//! point. That is real per-region tracking, not per-window the way
//! `SketchyBar` gets it — it reads the same two Carbon kinds for the same
//! reason, it only looks like a different mechanism because it gives every
//! item its own window.
//!
//! So this module listens for 8/9 and, on either, records where it happened
//! into [`current_position`] — not which item, this module has no layout to
//! ask. `ecs.rs::track_hover` hit-tests that point against the retained
//! layout and turns a change of hit into `mouse.entered`/`mouse.exited`; see
//! its own doc. Both Carbon kinds behave identically here for that purpose: an
//! exit's point is outside whatever rect was left, an enter's is inside
//! whatever was entered, and a re-hit-test tells the difference either way.
//!
//! This module only pumps events already *arriving*; the tracking rects
//! themselves are registered by [`crate::tracking`], kept in step with each
//! item's on-screen frame as its layout changes.
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
//! `coolabah_protocol` a decoded drag could become, and no drag state lives in
//! this module to make one meaningful — a start point, a threshold, anything
//! a hypothetical `drag_script` would need. Pulling it would only add an
//! event this loop takes off the queue and throws away on every drag over
//! the bar. If drag support is ever wanted, it starts with a protocol `Kind`
//! to decode into, not with adding the Carbon kind to this list.

use crate::protocol::event::{MouseClick, Scroll};
use crate::protocol::{Event, Kind, Modifiers, MouseButton};
use crate::sources::{Cause, Emitter, Registering, Source, SourceId, StartError};
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{CGEvent, CGEventField, CGEventFlags};
use skylight::callback::Callback;
use std::cell::Cell;
use std::collections::BTreeSet;
use std::ffi::c_void;

type OsStatus = i32;
type EventRef = *mut c_void;
/// Carbon's handler shape. Safe, like every other trampoline this daemon
/// hands a framework: the pointers are raw either way, and what makes them
/// readable is [`CarbonEvent`], not the `unsafe` on the signature.
type EventHandlerProc =
    extern "C-unwind" fn(call_ref: *mut c_void, event: EventRef, data: *mut c_void) -> OsStatus;

/// `'mous'`, the four-character code for the mouse event class.
const CLASS_MOUSE: u32 = u32::from_be_bytes(*b"mous");

/// A mouse event kind this source knows how to read.
///
/// A closed set rather than the five bare `u32`s it replaces: the codes are
/// the ABI, but everything above the one `match` that decodes them names the
/// kind, so nothing can compare a kind against a class or against a code
/// this source never asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MouseKind {
    /// `kEventMouseUp`. A click completes on release, which is the event
    /// `SketchyBar` acts on too.
    Up,
    /// `kEventMouseEntered`/`kEventMouseExited`. Only asked for while
    /// [`wants_hover`] is true — see the module doc.
    Entered,
    Exited,
    /// The two kinds a scroll can arrive as — see the module doc's note on
    /// `kEventMouseDragged`/`kEventMouseScroll`.
    WheelMoved,
    Scroll,
}

impl MouseKind {
    const fn code(self) -> u32 {
        match self {
            Self::Up => 2,
            Self::Entered => 8,
            Self::Exited => 9,
            Self::WheelMoved => 10,
            Self::Scroll => 11,
        }
    }

    /// `None` for a kind this source never registered for, which Carbon
    /// should not be delivering but is free to.
    const fn from_code(code: u32) -> Option<Self> {
        match code {
            2 => Some(Self::Up),
            8 => Some(Self::Entered),
            9 => Some(Self::Exited),
            10 => Some(Self::WheelMoved),
            11 => Some(Self::Scroll),
            _ => None,
        }
    }

    const fn spec(self) -> EventTypeSpec {
        EventTypeSpec {
            class: CLASS_MOUSE,
            kind: self.code(),
        }
    }
}

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

/// One Carbon event, borrowed for as long as the handler holding it runs.
///
/// An `EventRef` is a bare `void*` that is only a live event inside the
/// handler Carbon called with it, so every read of one used to be an `unsafe`
/// call at the point of asking — three of them, spread over two functions,
/// each restating the same claim. Naming the borrow once makes that claim
/// where it is actually known, at [`observe`]'s first line, and leaves the
/// reads as ordinary methods.
#[derive(Clone, Copy)]
struct CarbonEvent<'a> {
    event: EventRef,
    handler: std::marker::PhantomData<&'a ()>,
}

impl CarbonEvent<'_> {
    /// # Safety
    ///
    /// `event` must be a live Carbon event for the whole of `'a` — inside an
    /// event handler, that is the handler's own call.
    const unsafe fn new(event: EventRef) -> Self {
        Self {
            event,
            handler: std::marker::PhantomData,
        }
    }

    /// Which mouse event this is, or `None` for a kind this does not decode.
    fn kind(self) -> Option<MouseKind> {
        // SAFETY: live for `'a`, per the constructor's contract.
        MouseKind::from_code(unsafe { GetEventKind(self.event) })
    }

    /// The `CGEvent` behind it, which is where the usable accessors are.
    fn cg(self) -> Option<CFRetained<CGEvent>> {
        // SAFETY: as above; the call hands back the +1 reference `CFRetained`
        // then owns.
        unsafe { CopyEventCGEvent(self.event) }
    }

    /// The raw reference, for the one call that must chain to the next
    /// handler with exactly what this one was given.
    const fn as_raw(self) -> EventRef {
        self.event
    }
}

thread_local! {
    /// Where a tracking rect was last crossed, in global screen coordinates,
    /// or `None` while hover is not wanted at all.
    ///
    /// The one thing here that cannot travel in the callback state: it is read
    /// by [`current_position`], which `ecs.rs::track_hover` calls with no
    /// registration in hand to hit-test against the retained layout — where
    /// the actual dedup, deciding whether the hit *changed*, happens.
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
fn decode(event: CarbonEvent<'_>) -> Option<Event> {
    let cg = event.cg()?;
    let at = CGEvent::location(Some(&cg));
    let modifiers = modifiers_of(&cg);

    match event.kind()? {
        MouseKind::Up => {
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
        MouseKind::WheelMoved | MouseKind::Scroll => {
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

/// `eventNotHandledErr`: what this returns when there is no registration left
/// to report to.
///
/// **Not `noErr`.** A handler that is no longer listening has not handled
/// anything, and saying otherwise would swallow the event rather than let
/// Carbon pass it to whoever is next.
const EVENT_NOT_HANDLED: OsStatus = -9874;

// One event, straight from the dispatcher — see the module doc for why
// installing here only works under Carbon's own event loop.
//
// An ordinary Rust function: the `extern "C-unwind"` entry point, the state
// recovery and the [`EVENT_NOT_HANDLED`] answer for a registration that has
// gone are all `skylight::trampoline!`'s.
//
// Chains to whatever was there before, so this observes rather than swallows —
// which is why the return value is a *call* rather than a constant.
/// What Carbon calls for an event on the dispatcher target.
///
/// **The one callback here whose return value the platform uses**, which is why
/// it is still a callback and not a stream. Carbon decides whether the event
/// carries on to the next handler from what this returns, and a future's answer
/// is by construction available only after the callback has returned — so the
/// chain below stays synchronous. There is nothing else to defer: `decode`
/// already produces the finished event, and sending it cannot block.
fn observe(call_ref: *mut c_void, event: EventRef, emit: &Emitter) -> OsStatus {
    // SAFETY: Carbon calls a handler with a live event, for the length of the
    // call and no longer -- which is the borrow `CarbonEvent` stands for.
    let event = unsafe { CarbonEvent::new(event) };

    match event.kind() {
        Some(MouseKind::Entered | MouseKind::Exited) => {
            if let Some(cg) = event.cg() {
                LAST_CROSSING.with(|last| last.set(Some(CGEvent::location(Some(&cg)))));
            }
        }
        _ => {
            if let Some(decoded) = decode(event) {
                emit.send(decoded);
            }
        }
    }

    // SAFETY: `call_ref` and `event` are the ones we were handed.
    unsafe { CallNextEventHandler(call_ref, event.as_raw()) }
}

/// Every mouse kind this source decodes.
///
/// Hover joins only while something wants it, so a cursor crossing tracking
/// rects nobody asked about costs nothing. `kEventMouseMoved` is not here at
/// all -- see the module doc.
const WANTED: [MouseKind; 5] = [
    MouseKind::Up,
    MouseKind::WheelMoved,
    MouseKind::Scroll,
    // Last, so a registration that does not want hover asks for the leading
    // three and stops -- see `install`.
    MouseKind::Entered,
    MouseKind::Exited,
];

/// How many of [`WANTED`] are asked for when hover is not.
const WITHOUT_HOVER: usize = 3;

/// The same list in the contiguous shape `InstallEventHandler` takes.
const SPECS: [EventTypeSpec; WANTED.len()] = {
    let mut specs = [MouseKind::Up.spec(); WANTED.len()];
    let mut i = 0;
    while i < WANTED.len() {
        specs[i] = WANTED[i].spec();
        i += 1;
    }
    specs
};

/// Installs the handler on Carbon's dispatcher target.
///
/// Takes main-thread proof because `GetEventDispatcherTarget` is the main
/// thread's; the returned handle is what `RemoveEventHandler` takes back.
#[skylight::main_thread]
fn install(hover: bool, proc: EventHandlerProc, context: *mut c_void) -> Option<*mut c_void> {
    // The hover kinds are last in `SPECS`, so not wanting them is a shorter
    // count rather than a second list.
    let wanted = u32::try_from(if hover { SPECS.len() } else { WITHOUT_HOVER })
        .expect("five event kinds fit in a u32");
    let mut handler = std::ptr::null_mut();
    // SAFETY: `SPECS` is `'static`, `wanted` is within it, and `handler` is a
    // valid out-pointer.
    let status = unsafe {
        InstallEventHandler(
            GetEventDispatcherTarget(),
            proc,
            wanted,
            SPECS.as_ptr(),
            context,
            &raw mut handler,
        )
    };
    (status == 0).then_some(handler)
}

skylight::trampoline!(OBSERVE = observe(
    call_ref: *mut c_void,
    event: EventRef,
    @ emit: &Emitter,
) -> OsStatus; dropped EVENT_NOT_HANDLED);

#[derive(Default)]
pub struct Mouse {
    /// Tells the running registration that the wanted kinds moved.
    ///
    /// Carbon is told which event kinds to deliver once, by
    /// `InstallEventHandler`, so a demand change that widens them has to
    /// install again -- the same shape `notifications.rs` uses, for the same
    /// reason.
    adjust: Option<tokio::sync::watch::Sender<bool>>,
}

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

    fn run(
        &mut self,
        wanted: &BTreeSet<Kind>,
        cx: Registering,
    ) -> Result<crate::pool::Task, StartError> {
        // The proof comes from the context rather than from a check: a
        // `Registering` is only built by the registry, on the thread the run
        // loop turns on, so there is nothing to test and nothing to refuse.
        let hover = wants_hover(wanted);

        // The emitter *is* the callback state. Every other source that reports
        // straight from its callback wraps one in a struct with a lock around
        // it; there is nothing here for a lock to protect, because this state
        // is built fresh per registration and read-only afterwards.
        let emit = std::sync::Arc::new(cx.emitter());

        // `RemoveEventHandler` takes exactly what `InstallEventHandler`
        // returned, so the teardown is the handle it gave back. Installed
        // synchronously, here, so a refusal comes back through this `run`
        // rather than surfacing later inside the future below.
        let main = cx.main_thread().clone();
        let installed = Callback::new(std::sync::Arc::clone(&emit), |context| {
            let handler = install(&main, hover, OBSERVE, context).ok_or(Cause::NotMainThread)?;
            Ok(move || {
                // SAFETY: the handle `InstallEventHandler` returned, removed
                // once.
                unsafe { RemoveEventHandler(handler) };
            })
        })
        .map_err(|cause| StartError::new(self.id(), cause))?;

        let (adjust, mut changes) = tokio::sync::watch::channel(hover);
        self.adjust = Some(adjust);
        let id = self.id();

        // `observe` emits straight from Carbon's callback, so this future
        // consumes no stream. It is here to own the registration -- dropping
        // the `Task` drops `installed`, which deregisters -- and to replace it
        // when the wanted kinds move.
        Ok(crate::runloop::owned(
            cx.main_thread(),
            move |_proof| async move {
                let mut installed = Some(installed);
                let mut current = hover;
                while changes.changed().await.is_ok() {
                    let hover = *changes.borrow_and_update();
                    if hover == current {
                        continue;
                    }
                    current = hover;

                    // The old handler goes first. Two handlers on the one
                    // dispatcher target both fire, which would double every
                    // click for as long as both were installed.
                    drop(installed.take());
                    if !hover {
                        LAST_CROSSING.with(|last| last.set(None));
                    }

                    let emit = std::sync::Arc::clone(&emit);
                    match Callback::new(emit, |context| {
                        let handler =
                            install(&main, hover, OBSERVE, context).ok_or(Cause::NotMainThread)?;
                        Ok(move || {
                            // SAFETY: the handle `InstallEventHandler`
                            // returned, removed once.
                            unsafe { RemoveEventHandler(handler) };
                        })
                    }) {
                        Ok(fresh) => installed = Some(fresh),
                        Err(cause) => {
                            // Nothing to hand an error to -- `run` returned
                            // long ago -- and the source is now deaf until the
                            // next change, which is worth saying out loud.
                            let err = StartError::new(id, cause);
                            tracing::error!(%err, "mouse could not re-register for the kinds now wanted");
                        }
                    }
                }
                // The sender goes with the source, which outlives this.
                std::future::pending::<()>().await;
            },
        ))
    }

    fn update(&mut self, wanted: &BTreeSet<Kind>, cx: &mut Registering) -> Result<(), StartError> {
        let _ = cx;
        if let Some(adjust) = &self.adjust {
            // An error means the future has already gone, which only happens
            // once its `Task` is dropped -- and the registry drops that before
            // it calls `update` again.
            let _ = adjust.send(wants_hover(wanted));
        }
        Ok(())
    }
}

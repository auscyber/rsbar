//! Space changes, from `SkyLight`'s private notification stream.
//!
//! `SLSRegisterNotifyProc` has no *documented* unregister call, and none of
//! the three independent reverse-engineerings this crate cross-checks (yabai,
//! `rift`, `paneru`) name one. `SkyLight` exports one anyway:
//! `_SLSRemoveNotifyProc`, the same pairing it gives
//! `_SLSRegisterConnectionNotifyProc`/`_SLSRemoveConnectionNotifyProc`. Its
//! signature is in no header, so it was read off the arm64 code instead — see
//! [`skylight::ffi::SLSRemoveNotifyProc`], which records what the instructions
//! say: three arguments, and an entry is erased only when the proc *and* the
//! context both match.
//!
//! So a registration here is per-run and undone on drop, like every other
//! source. Each `run` builds its own relays, registers against them, and hands
//! the resulting guards to the future; stopping the source drops them, which
//! removes the procedures and then releases the relays.
//!
//! Registering per run is safe to do repeatedly:
//! `skylight/examples/notify_remove.rs` cycles register/remove three times over
//! these same four events and survives, which is what was actually in doubt —
//! this process is the only listener for them, so every stop empties the
//! server's vector and takes a branch that aborts on `kCGErrorInvalidOperation`.
//!
//! What none of it can tell you is whether the removal *took*. Found and
//! not-found both answer `kCGErrorSuccess` — the same probe removes a pair it
//! never registered and still gets success — and on this macOS build the events
//! do not fire at all, which `examples/notify_probe.rs` established for all 32
//! candidate numbers through both registration forms, and which is why this
//! source's own space-change registration has never been observed firing live.
//! With no delivery there is nothing to watch stop. So the guarantee rests
//! where it does for every other source here: the relay goes with the guard,
//! and a procedure whose relay is gone posts into nothing.
//!
//! # Two events, two futures, two threads
//!
//! A window-membership notification carries the space id in its payload, so its
//! future needs nothing but the bytes and runs on the pool. A space change
//! carries nothing useful and must ask the window server which display owns the
//! menu bar — a `ConnectionId`, which is `MainThreadOnly` — so that future is
//! `!Send` and runs on the thread that draws. Both callbacks only read and
//! post.
//!
//! # Event numbers
//!
//! None of these are in a public header. Cross-checked against `SketchyBar`'s
//! `sketchybar.c`/`app_windows.c` (which registers `1401` for a space change
//! and `1327`/`1328` for space create/destroy) and `paneru`'s `KnownCGSEvent`
//! (which additionally names `1325`/`1326`/`1339` for window membership).
//! `space_windows_changed` is a membership event, so it watches the latter
//! three, not the space lifecycle ones.
//!
//! # What is and is not verified
//!
//! [`display::active_menu_bar`] was checked against live ground truth: a spike
//! binary called it right after registering and printed `Some((1, 3))`, which
//! matched both `CGMainDisplayID()` and `defaults read com.apple.spaces`'
//! reported current space (`id64 = 3`) at that moment. The registration calls
//! themselves also succeed (`SLSRegisterNotifyProc` returns `kCGErrorSuccess`
//! for all four events) and nothing crashes.
//!
//! What was **not** observed firing live: this machine has no keyboard
//! shortcut bound to move between spaces (`AppleSymbolicHotKeys` has them
//! disabled), and driving Mission Control through synthetic mouse clicks on
//! its desktop-switcher UI did not reliably land a real switch in this
//! session. Actually triggering a live gesture-based space change — the
//! mechanism recent macOS requires, per `rift`'s `space_switch.rs` — needs a
//! real trackpad swipe or a substantial synthetic-gesture subsystem that was
//! judged out of scope here. So the `1401`/`1325`/`1326`/`1339` callbacks
//! themselves are unverified against a live event, even though the code they
//! call into is.

use crate::protocol::event::{SpaceChange, SpaceWindowsChange};
use crate::protocol::{Event, Kind};
use crate::sources::{Emitter, Registering, Source, SourceId, StartError};
use skylight::callback::{Events, Relay};
use skylight::display;
use std::collections::BTreeSet;

/// `WorkspaceDidChange`: the active space changed, on some display.
const SPACE_CHANGED_EVENT: u32 = 1401;
/// `SpaceWindowCreated`: a window joined a space.
const SPACE_WINDOW_CREATED_EVENT: u32 = 1325;
/// `SpaceWindowDestroyed`: a window left a space.
const SPACE_WINDOW_DESTROYED_EVENT: u32 = 1326;
/// `SpaceWindowBatchReassociated`: several windows moved spaces at once.
const SPACE_WINDOW_BATCH_REASSOCIATED_EVENT: u32 = 1339;

/// The `u64` space id every window-membership payload here leads with, per
/// `rift`'s and `paneru`'s independent reads of the same notifications.
fn leading_space_id(payload: &[u8]) -> Option<u64> {
    let bytes = payload.get(..size_of::<u64>())?;
    Some(u64::from_ne_bytes(bytes.try_into().ok()?))
}

/// An ordinary Rust function: its state and the notification arrive typed, and
/// the `extern "C"` trampoline behind it is `NotifyProcedure`'s.
///
/// The notification says only that the space changed; which display now owns
/// the menu bar is a question for the window server, and [`report_space`] asks
/// it.
fn space_changed(relay: &Relay<()>, _event: skylight::ffi::Event<'_>) {
    relay.post(());
}

fn space_windows_changed(relay: &Relay<u64>, event: skylight::ffi::Event<'_>) {
    let Some(space) = leading_space_id(event.payload) else {
        return;
    };
    relay.post(space);
}

/// Asks the window server which display's menu bar is now active, per change.
async fn report_space(mut changes: Events<()>, emit: Emitter, main: skylight::MainThread) {
    while changes.next().await.is_some() {
        // SAFETY: this thread is the main one, which `main` is proof of, and
        // that is the whole of what the call requires.
        let connection = unsafe { skylight::ffi::SLSMainConnectionID(&main) };
        let Some((display, space)) = display::active_menu_bar(connection) else {
            continue;
        };
        emit.send(Event::SpaceChanged(SpaceChange {
            display: display.id(),
            space,
        }));
    }
}

/// Reports windows moving between spaces. Needs nothing but the id it was
/// handed, so it runs anywhere.
async fn report_windows(mut moved: Events<u64>, emit: Emitter) {
    while let Some(space) = moved.next().await {
        emit.send(Event::SpaceWindowsChanged(SpaceWindowsChange { space }));
    }
}

/// Nothing is kept between runs: the relays, the registrations and the streams
/// are all built by `run` and all die with the task it returns.
pub struct Spaces;

skylight::notify!(SPACE_CHANGED = space_changed(&Relay<()>));
skylight::notify!(SPACE_WINDOWS_CHANGED = space_windows_changed(&Relay<u64>));

impl Source for Spaces {
    fn id(&self) -> SourceId {
        SourceId("spaces")
    }

    fn provides(&self) -> Vec<Kind> {
        vec![Kind::SpaceChanged, Kind::SpaceWindowsChanged]
    }

    fn run(
        &mut self,
        _wanted: &BTreeSet<Kind>,
        cx: Registering,
    ) -> Result<crate::pool::Task, StartError> {
        let (changed, space) = skylight::callback::relay::<()>();
        let (moved, windows) = skylight::callback::relay::<u64>();

        // Registered against this run's own relays, and removed when the
        // guards below go. Any of the four failing takes the whole source down
        // rather than leaving a half-registered one running: the guards
        // already made drop on the way out.
        let watching_space = cx.notify(
            SPACE_CHANGED,
            SPACE_CHANGED_EVENT,
            std::sync::Arc::clone(&changed),
        )?;
        let watching_windows = [
            SPACE_WINDOW_CREATED_EVENT,
            SPACE_WINDOW_DESTROYED_EVENT,
            SPACE_WINDOW_BATCH_REASSOCIATED_EVENT,
        ]
        .into_iter()
        .map(|event| cx.notify(SPACE_WINDOWS_CHANGED, event, std::sync::Arc::clone(&moved)))
        .collect::<Result<Vec<_>, _>>()?;

        let emit_space = cx.emitter();
        let emit_windows = cx.emitter();

        // Folded into one future: the main-thread half holds the pool half and
        // all four guards as locals, so dropping the outer `Task` drops every
        // registration on the thread that made it. `report_space` needs the
        // main thread for `SLSMainConnectionID`; `report_windows` needs nothing
        // and would rather not compete with drawing, so it keeps running on the
        // pool underneath.
        Ok(crate::runloop::owned(
            cx.main_thread(),
            move |main| async move {
                let _watching = (watching_space, watching_windows);
                let _moving = crate::pool::owned(report_windows(windows, emit_windows));
                report_space(space, emit_space, main).await;
            },
        ))
    }
}

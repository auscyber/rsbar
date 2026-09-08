//! `CoreGraphics`' display reconfiguration callback: a monitor plugged in,
//! unplugged or rearranged, which invalidates every panel's geometry.
//!
//! Half of `display_changed`, and the half that is not a notification — so it
//! is the half that cannot live with the [`NSWorkspace`
//! centre](super::notifications), where focus moving to another display is the
//! other half. They used to share a source, back when a source registered
//! everything it could produce the moment it started: subscribing to
//! `display_changed` bought four unwanted `NSWorkspace` observers along with
//! it. `run` is handed `wanted` now, so a second provider for the kind
//! costs exactly the one notification it is for.
//!
//! Eager, because the bar's own geometry depends on it and no config
//! subscribes on the bar's behalf.
//!
//! # This is the whole of what anyone registers
//!
//! Worth saying, because it looks too small. `rift`, which has to keep a tiled
//! layout correct across a monitor being unplugged, registers exactly one thing
//! for this — `CGDisplayRegisterReconfigurationCallback`, in
//! `actor/notification_center.rs` — alongside the `NSWorkspace` notifications
//! this daemon observes in [`super::notifications`]. `SketchyBar`'s
//! `display_begin` (`src/display.c`) is the same one call. There is no window
//! server notification for a display arriving, and no second registration to
//! be missing: what separates a good handler from a bad one is entirely what it
//! does with the flags.
//!
//! # What the flags mean, and why almost all of them count
//!
//! `SketchyBar` acts on four — `Add`, `Remove`, `Moved` and
//! `DesktopShapeChanged` — and it takes only the first that matches, so a
//! resolution change reaches it as `DesktopShapeChanged`. That filter is too
//! narrow here: `SetMain` moves the menu bar to another display, and
//! `Mirror`/`UnMirror` change how many displays a bar needs a panel on, and
//! both change this bar's geometry without setting any of `SketchyBar`'s four.
//! So every flag but `BeginConfigurationFlag` is treated as "the layout may
//! have moved", and the question of whether it *did* is answered by looking,
//! below.
//!
//! (`rift`'s own flag set has four bits at the wrong positions —
//! `Enabled`/`Disabled`/`Mirror`/`UnMirror` are each two bits low — which is
//! why nothing here re-declares them. `skylight::sys::display`'s module docs
//! have the detail and a test pinning Apple's values.)
//!
//! # Coalescing
//!
//! One user action produces a burst: `CoreGraphics` calls back once per display
//! with `BeginConfigurationFlag`, then again per display with what happened,
//! and often a final `DesktopShapeChanged` for display `0`. Every one of those
//! used to become a `display_changed` event, and every event rebuilds every
//! panel — tearing down and remaking window server windows, three or four times
//! for one plug.
//!
//! `rift` answers this with a debounce and a settle loop: it fingerprints the
//! display topology, waits for the fingerprint to repeat and for the window
//! server to go quiet, and only then tells its layout engine
//! (`attempt_finish_display_churn`). The fingerprint is worth having and the
//! wait is not. A window manager must not move windows into a half-updated
//! layout, because a window put in the wrong place stays there; a bar that
//! reframes early is corrected by the next callback in the same burst, since
//! the last callback always carries the settled layout. So this takes the
//! fingerprint and skips the timers: a callback that finds the layout unchanged
//! since the last event reports nothing.
//!
//! What that leaves is at most two events per burst instead of one per
//! callback — one possibly-early, one correct — with no delay added to either.

use skylight::callback::{Callback, Events, Relay};

use crate::sources::{Cause, Emitter, Registering, Source, SourceId, StartError};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayChangeSummaryFlags,
    CGDisplayRegisterReconfigurationCallback, CGDisplayRemoveReconfigurationCallback,
};
use rsbar_protocol::event::DisplayChange;
use rsbar_protocol::{Event, Kind};
use std::collections::BTreeSet;

/// One display, as much of it as changes a bar's geometry.
///
/// The bounds are held as raw bits rather than as `f64`s so that two layouts
/// can be compared for equality without comparing floats — the same thing
/// `rift`'s `fingerprint_displays` does with `to_bits`. Nothing arithmetic ever
/// happens to these; the only question asked of them is "the same as last
/// time?".
///
/// `main` is in here because `SetMainFlag` can arrive with every display's
/// bounds unchanged: the menu bar moved to the other monitor, which moves this
/// bar. A fingerprint of bounds alone would call that no change and swallow it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Placement {
    id: CGDirectDisplayID,
    origin: (u64, u64),
    size: (u64, u64),
    main: bool,
}

/// Every active display, in `CoreGraphics`' order.
///
/// Order is part of it: the displays arriving in a different order is a
/// rearrangement, even if the set is the same.
type Layout = Vec<Placement>;

/// What the layout is right now.
///
/// Empty when `CoreGraphics` will not say — which it does mid-reconfiguration,
/// and which is why "unchanged" and not "non-empty" is the test below: an empty
/// answer differs from the previous non-empty one, so it still reports, and the
/// next callback in the burst corrects it.
fn layout() -> Layout {
    skylight::display::active()
        .unwrap_or_default()
        .into_iter()
        .map(|display| {
            let bounds = CGDisplayBounds(display.id());
            Placement {
                id: display.id(),
                origin: (bounds.origin.x.to_bits(), bounds.origin.y.to_bits()),
                size: (bounds.size.width.to_bits(), bounds.size.height.to_bits()),
                main: display.is_main(),
            }
        })
        .collect()
}

/// The layout as of the last event this source sent.
///
/// A plain field, not a lock. It lives on [`report`]'s stack — one task owns
/// it, holds it across every `.await`, and is the only thing that ever looks at
/// it — so there is nothing to synchronise with.
#[derive(Default)]
struct Reported(Option<Layout>);

impl Reported {
    /// Whether the layout has moved since the last event, recording it if so.
    ///
    /// One call does both halves on purpose: an "is it different" that did not
    /// also store would let two bursts both see a difference and both report
    /// it.
    fn changed_since_last_event(&mut self, now: &Layout) -> bool {
        if self.0.as_ref() == Some(now) {
            return false;
        }
        self.0 = Some(now.clone());
        true
    }
}

/// One reconfiguration callback, as the task sees it.
#[derive(Debug)]
struct Change {
    id: CGDirectDisplayID,
    flags: CGDisplayChangeSummaryFlags,
}

/// How many callbacks of one burst are answered by a single look at the
/// layout.
///
/// The burst is per display and then some — see the module note — so this is
/// generous on purpose. Overshooting costs nothing: [`Events::batch`] takes
/// what is queued and does not wait for more.
const BURST: usize = 64;

/// What `CoreGraphics` calls when the display layout changes.
///
/// Filters and posts. Reading the layout is [`report`]'s, and doing it there
/// rather than here is what makes the coalescing real: one look answers a whole
/// burst instead of one look per callback.
fn reconfigured(
    reconfigured_id: CGDirectDisplayID,
    flags: CGDisplayChangeSummaryFlags,
    relay: &Relay<Change>,
) {
    // CoreGraphics announces a change twice: once up front carrying only
    // `BeginConfigurationFlag`, and again afterwards carrying what actually
    // happened. There is no matching "end" flag, so the first pass is
    // identified by that flag and skipped — acting on it would read the old
    // display layout.
    if flags.contains(CGDisplayChangeSummaryFlags::BeginConfigurationFlag) {
        return;
    }
    relay.post(Change {
        id: reconfigured_id,
        flags,
    });
}

skylight::trampoline!(RECONFIGURED = reconfigured(
    reconfigured_id: CGDirectDisplayID,
    flags: CGDisplayChangeSummaryFlags,
    @ relay: &Relay<Change>,
));

/// Turns bursts of reconfiguration callbacks into the events that earned one.
async fn report(mut changes: Events<Change>, emit: Emitter, mut reported: Reported) {
    while let Some(burst) = changes.batch(BURST).await {
        // Once for the burst, and *after* the whole burst has been taken, so
        // what it reads is the settled layout rather than a half-updated one.
        let now = layout();
        let layout_changed = reported.changed_since_last_event(&now);
        tracing::debug!(
            // The field is not called `display`: that is one of `tracing`'s own
            // field helpers, and the macro resolves the name to the helper
            // wherever it appears.
            id = burst.last().map(|change| change.id),
            flags = burst.last().map(|change| change.flags.0),
            callbacks = burst.len(),
            displays = now.len(),
            changed = layout_changed,
            "a display was reconfigured"
        );
        if layout_changed {
            emit.send(Event::DisplayChanged(DisplayChange {}));
        }
    }
}

pub struct Displays;

impl Source for Displays {
    fn id(&self) -> SourceId {
        SourceId("displays")
    }

    fn provides(&self) -> Vec<Kind> {
        vec![Kind::DisplayChanged]
    }

    fn eager(&self) -> bool {
        true
    }

    fn run(
        &mut self,
        _wanted: &BTreeSet<Kind>,
        cx: Registering,
    ) -> Result<crate::pool::Task, StartError> {
        let emit = cx.emitter();
        let id = self.id();

        // `!Send` means "build it where it will live", not "put it on the
        // main thread": `layout()` goes through `skylight::display::active()`
        // (`CGGetActiveDisplayList`), `CGDisplayBounds` and `CGDisplayIsMain`,
        // none of it gated to the main thread, and
        // `CGDisplayRegisterReconfigurationCallback` wants no thread in
        // particular either -- so the whole registration is built on
        // `pool::sources()`'s thread rather than the one that draws the bar.
        //
        // The cost: a registration failure past this point can no longer
        // come back through this `run`'s `Result`, since building it happens
        // after this function has already returned `Ok`. Logged instead —
        // the same choice `wifi` and `power` make.
        Ok(crate::pool::sources().spawn(move |_here| async move {
            let (relay, changes) = skylight::callback::relay::<Change>();
            // Seeded with the layout as it is now, before the callback is
            // registered, so the first callback after start-up is judged
            // against what the bar was built for rather than against
            // nothing -- otherwise a reconfiguration that changes nothing
            // relevant would still report once.
            let reported = Reported(Some(layout()));

            // `CGDisplayRemoveReconfigurationCallback` matches on the callback
            // *and* the context, so the teardown repeats exactly what went in.
            let watch = match Callback::new(relay, |context| {
                // SAFETY: `context` is the weak reference the callback keeps for as
                // long as the registration lives.
                let status = unsafe {
                    CGDisplayRegisterReconfigurationCallback(Some(RECONFIGURED), context)
                };
                if status != objc2_core_graphics::CGError::Success {
                    return Err(status);
                }
                Ok(move || {
                    // SAFETY: the same callback and context as above.
                    unsafe { CGDisplayRemoveReconfigurationCallback(Some(RECONFIGURED), context) };
                })
            }) {
                Ok(watch) => watch,
                Err(status) => {
                    let err = StartError::new(id, Cause::CoreGraphics(status));
                    tracing::error!(%err, "displays source could not register on its own thread");
                    return;
                }
            };
            let _watch = watch;
            report(changes, emit, reported).await;
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{Placement, Reported, layout};

    fn placement(id: u32, x: f64, main: bool) -> Placement {
        Placement {
            id,
            origin: (x.to_bits(), 0f64.to_bits()),
            size: (1512f64.to_bits(), 982f64.to_bits()),
            main,
        }
    }

    /// The whole point of the fingerprint: a burst of callbacks reporting the
    /// same layout is one event, not four.
    #[test]
    fn a_repeat_of_the_layout_already_reported_is_not_a_change() {
        let mut reported = Reported::default();
        let one = vec![placement(1, 0.0, true)];
        assert!(reported.changed_since_last_event(&one), "nothing seen yet");
        assert!(!reported.changed_since_last_event(&one));
        assert!(!reported.changed_since_last_event(&one));
    }

    /// The menu bar moving to the other display, with both displays' bounds
    /// untouched. `SetMainFlag` arrives for exactly this and a fingerprint of
    /// geometry alone would swallow it.
    #[test]
    fn the_main_display_changing_is_a_change_even_with_the_same_geometry() {
        let mut reported = Reported::default();
        let before = vec![placement(1, 0.0, true), placement(2, 1512.0, false)];
        let after = vec![placement(1, 0.0, false), placement(2, 1512.0, true)];
        assert!(reported.changed_since_last_event(&before));
        assert!(reported.changed_since_last_event(&after));
    }

    /// A display arriving, leaving, and moving — the three the bar has to
    /// reframe for.
    #[test]
    fn a_display_arriving_leaving_or_moving_is_a_change() {
        let mut reported = Reported::default();
        let one = vec![placement(1, 0.0, true)];
        let two = vec![placement(1, 0.0, true), placement(2, 1512.0, false)];
        let moved = vec![placement(1, 0.0, true), placement(2, -1512.0, false)];
        assert!(reported.changed_since_last_event(&one));
        assert!(reported.changed_since_last_event(&two), "arrived");
        assert!(reported.changed_since_last_event(&moved), "moved");
        assert!(reported.changed_since_last_event(&one), "left");
    }

    /// A live read, which is the only thing that says the fingerprint is built
    /// from something real: this machine has at least one active display, and
    /// exactly one of them is the main one.
    #[test]
    fn the_layout_this_machine_reports_has_one_main_display() {
        let now = layout();
        assert!(!now.is_empty(), "no active displays");
        assert_eq!(now.iter().filter(|display| display.main).count(), 1);
    }
}

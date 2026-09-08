//! Which rectangles the window server reports the cursor crossing.
//!
//! The rectangle is part of a claim's identity — an item that wants the
//! pointer holds a [`crate::sources::Watch`] taken over its rectangle — so a
//! moved item holds a *different* claim: the new one is taken and the old one
//! released through the same refcounted claim path every other source uses,
//! rather than anything here remembering where the item used to be.
//!
//! The claim lives on the item as a component, so it goes when the item goes
//! and moves when the item moves, the same reason [`crate::components::Watching`]
//! holds an item's other claims rather than a registry beside the world.
//! Despawning is the tidying.
//!
//! Its own system rather than something the repaint does on the way past,
//! because it answers a different question: an area changes for reasons the
//! layout never sees — an item subscribing to hover after it was placed, or
//! gaining a click script — and stays put through repaints that change
//! everything else about an item, like its colour or its text.
//!
//! `SLSRemoveAllTrackingAreas` takes a window's whole set; there is no call to
//! remove one. So a change means clearing that window and adding back what
//! still applies — which is why [`Areas`] reports *which displays* moved and
//! not merely that something did: a repaint that only recoloured something
//! claims the same rectangles it already had, and touches no window at all.

#![allow(
    clippy::needless_pass_by_value,
    reason = "Bevy system parameters are taken by value by contract"
)]

use crate::bar::Panels;
use crate::components::{ClickScript, Subscriptions};
use crate::ecs::Sources;
use crate::layout::Placements;
use crate::sources::Watch;
use bevy_ecs::prelude::*;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use rsbar_protocol::Kind;
use std::collections::{BTreeSet, HashMap};

/// A rectangle at whole-pixel precision.
///
/// The window server is told rectangles in points, but a claim is *keyed* by
/// one — so it has to hash and compare, which `f64` cannot do meaningfully.
/// Rounding to whole pixels is also the honest resolution of the question
/// being asked: a layout that shifts an item by a third of a point has not
/// moved the region the cursor crosses, and re-registering every tracking rect
/// on the display for it would be pure cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Pixels {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "a float-to-int cast saturates; a bar rectangle is nowhere near i32's range anyway"
)]
fn whole(value: f64) -> i32 {
    value.round() as i32
}

impl Pixels {
    /// The nearest whole-pixel rectangle to one the layout produced.
    #[must_use]
    pub fn of(rect: CGRect) -> Self {
        Self {
            x: whole(rect.origin.x),
            y: whole(rect.origin.y),
            width: whole(rect.size.width),
            height: whole(rect.size.height),
        }
    }

    /// Back in the form the window server takes.
    #[must_use]
    pub fn rect(self) -> CGRect {
        CGRect::new(
            CGPoint::new(f64::from(self.x), f64::from(self.y)),
            CGSize::new(f64::from(self.width), f64::from(self.height)),
        )
    }
}

/// Where an item is, as far as the pointer is concerned: a rectangle on one
/// display.
///
/// Carried inside the claim itself — see [`crate::sources::Watched`] — which
/// is what makes a move a change of claim rather than a change to one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Area {
    pub display: u32,
    pub rect: Pixels,
}

impl Area {
    #[must_use]
    pub fn new(display: u32, rect: CGRect) -> Self {
        Self {
            display,
            rect: Pixels::of(rect),
        }
    }
}

/// The rectangles claimed right now, by display.
///
/// A projection of the live claims, not a second record of them: rebuilt from
/// the claim map whenever one has been taken or dropped, so there is nothing
/// here to keep in step. What it adds is the diff — which displays' sets
/// actually differ from the last time the window server was told — because
/// that is the question `SLSRemoveAllTrackingAreas` forces, and answering it
/// from the claims directly would mean rebuilding every display on any change
/// anywhere.
#[derive(Debug, Default)]
pub struct Areas {
    by_display: HashMap<u32, BTreeSet<Pixels>>,
    changed: Vec<u32>,
}

impl Areas {
    /// The displays whose rectangles are not what they were last projection.
    #[must_use]
    pub fn changed(&self) -> &[u32] {
        &self.changed
    }

    /// Every rectangle claimed on one display.
    pub fn on(&self, display: u32) -> impl Iterator<Item = CGRect> + '_ {
        self.by_display
            .get(&display)
            .into_iter()
            .flatten()
            .map(|rect| rect.rect())
    }

    /// Takes the claimed areas again, recording which displays moved.
    pub(crate) fn refresh(&mut self, claimed: impl Iterator<Item = Area>) {
        let mut next: HashMap<u32, BTreeSet<Pixels>> = HashMap::new();
        for area in claimed {
            next.entry(area.display).or_default().insert(area.rect);
        }
        self.changed.clear();
        for (display, rects) in &next {
            if self.by_display.get(display) != Some(rects) {
                self.changed.push(*display);
            }
        }
        // A display whose last rectangle went has to be told too, and it is
        // not in the new set to be found by the loop above.
        self.changed
            .extend(self.by_display.keys().filter(|d| !next.contains_key(d)));
        self.by_display = next;
    }

    /// Nothing has been claimed or released since the last projection, so
    /// nothing has changed since it either.
    pub(crate) fn hold(&mut self) {
        self.changed.clear();
    }
}

/// The claim this item holds on the pointer over its own rectangle.
///
/// Present only on an item that wants the pointer at all. Dropping it — by
/// [`place`] replacing it, or by the item being despawned — releases the
/// claim, which is what takes the rectangle away.
#[derive(Component, Debug)]
pub struct TrackedArea {
    area: Area,
    watch: Watch,
}

impl TrackedArea {
    /// Whether this is already the claim the item wants, so there is nothing
    /// to take.
    fn is(&self, kind: &Kind, area: Area) -> bool {
        self.area == area && self.watch.kind() == kind
    }
}

/// The pointer event an item's rectangle exists for, if it wants one.
///
/// A click script counts, not just a hover subscription: both are answered
/// against the item's own frame, so both want the window server to know where
/// that frame is. Which of them it is decides the kind the claim is taken on,
/// so that tracking an item never asks a source for an event the item was not
/// going to be sent anyway.
fn pointer_kind(
    subscriptions: Option<&Subscriptions>,
    click: Option<&ClickScript>,
) -> Option<Kind> {
    let subscribed = |kind: &Kind| subscriptions.is_some_and(|subs| subs.0.contains(kind));
    if subscribed(&Kind::MouseEntered(())) {
        return Some(Kind::MouseEntered(()));
    }
    if subscribed(&Kind::MouseExited(())) {
        return Some(Kind::MouseExited(()));
    }
    click.is_some().then_some(Kind::MouseClicked(()))
}

/// Claims the pointer over every item that wants it, where it was just laid
/// out, and releases the claim of anything that no longer does.
pub fn place(
    mut commands: Commands,
    placements: Res<Placements>,
    mut sources: NonSendMut<Sources>,
    interactive: Query<(
        Option<&Subscriptions>,
        Option<&ClickScript>,
        Option<&TrackedArea>,
    )>,
) {
    for placed in placements.panels() {
        for (entity, rect) in &placed.items {
            let Ok((subscriptions, click, current)) = interactive.get(*entity) else {
                continue;
            };
            let Some(kind) = pointer_kind(subscriptions, click) else {
                if current.is_some() {
                    commands.entity(*entity).remove::<TrackedArea>();
                }
                continue;
            };
            let area = Area::new(placed.display, *rect);
            // An item that was repainted without moving is holding exactly the
            // claim it wants, so it keeps it -- and nothing downstream sees a
            // change to react to.
            if current.is_some_and(|held| held.is(&kind, area)) {
                continue;
            }
            let watch = sources.0.watch_area(*entity, &kind, area);
            commands.entity(*entity).insert(TrackedArea { area, watch });
        }
    }
}

/// Tells each display's window about its rectangles, when they have changed.
///
/// Reads the claims rather than the items: an item that was despawned has
/// released its claim, so it is simply not among them, and a display nobody's
/// claim moved on is not touched.
pub fn rebuild(panels: NonSend<Panels>, mut sources: NonSendMut<Sources>) {
    let areas = sources.0.tracked_areas();
    if areas.changed().is_empty() {
        return;
    }
    for panel in panels
        .iter()
        .filter(|panel| areas.changed().contains(&panel.display.id))
    {
        if let Err(err) = panel.window.clear_tracking_rects() {
            tracing::debug!(%err, "could not clear a display's tracked areas");
            continue;
        }
        let mut watched = 0;
        for rect in areas.on(panel.display.id) {
            match panel.window.add_tracking_rect(rect) {
                Ok(()) => watched += 1,
                Err(err) => tracing::debug!(%err, "could not watch an area"),
            }
        }
        tracing::debug!(display = panel.display.id, watched, "tracked areas changed");
    }
}

#[cfg(test)]
mod tests {
    use super::{Area, Areas, Pixels};
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    fn rect(x: f64, width: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, 0.0), CGSize::new(width, 24.0))
    }

    #[test]
    fn a_rectangle_is_claimed_to_the_whole_pixel() {
        // Sub-pixel drift is not a move: it must not make a different claim,
        // or a layout that rounds differently would re-register every
        // tracking rect on the display.
        assert_eq!(
            Area::new(1, rect(10.4, 40.2)),
            Area::new(1, rect(10.0, 40.0))
        );
        assert_ne!(
            Area::new(1, rect(10.6, 40.0)),
            Area::new(1, rect(10.0, 40.0))
        );
        assert_ne!(
            Area::new(1, rect(10.0, 40.0)),
            Area::new(2, rect(10.0, 40.0))
        );
    }

    #[test]
    fn a_rectangle_survives_the_trip_to_the_window_server_and_back() {
        assert_eq!(Pixels::of(rect(10.0, 40.0)).rect(), rect(10.0, 40.0));
    }

    #[test]
    fn only_the_displays_that_moved_are_reported() {
        let mut areas = Areas::default();
        areas.refresh([Area::new(1, rect(0.0, 10.0)), Area::new(2, rect(0.0, 10.0))].into_iter());
        let mut changed = areas.changed().to_vec();
        changed.sort_unstable();
        assert_eq!(changed, vec![1, 2]);

        areas.refresh([Area::new(1, rect(5.0, 10.0)), Area::new(2, rect(0.0, 10.0))].into_iter());
        assert_eq!(areas.changed(), [1], "display 2's set is what it was");

        areas.refresh([Area::new(1, rect(5.0, 10.0)), Area::new(2, rect(0.0, 10.0))].into_iter());
        assert!(areas.changed().is_empty(), "nothing moved, nothing to tell");
    }

    #[test]
    fn a_display_losing_its_last_rectangle_is_still_told() {
        let mut areas = Areas::default();
        areas.refresh(std::iter::once(Area::new(1, rect(0.0, 10.0))));
        areas.refresh(std::iter::empty());
        assert_eq!(areas.changed(), [1]);
        assert_eq!(areas.on(1).count(), 0);
    }
}

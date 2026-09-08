//! Which rectangles the window server reports the cursor crossing.
//!
//! The area lives on the item as a component, so it goes when the item goes
//! and moves when the item moves — the same reason [`crate::components::Watching`]
//! holds an item's source claims rather than a registry keeping a list beside
//! the world. Nothing has to remember to tidy up; despawning is the tidying.
//!
//! Its own system rather than something the repaint does on the way past,
//! because it answers a different question. An area changes for reasons the
//! layout never sees — an item subscribing to hover after it was placed, or
//! gaining a click script — and stays put through repaints that change
//! everything else about an item, like its colour or its text.
//!
//! `SLSRemoveAllTrackingAreas` takes a window's whole set; there is no call to
//! remove one. So a change means clearing that window and adding back what
//! still applies, and the areas are kept as components precisely so the
//! rebuild can be skipped when nothing actually moved — which is every repaint
//! that only recoloured something.

#![allow(
    clippy::needless_pass_by_value,
    reason = "Bevy system parameters are taken by value by contract"
)]

use crate::bar::Panels;
use crate::components::{ClickScript, Subscriptions};
use crate::layout::Placements;
use bevy_ecs::prelude::*;
use objc2_core_foundation::CGRect;
use rsbar_protocol::Kind;

/// The rectangle this item is currently watched on, and the display it is on.
///
/// Present only on an item that wants the pointer at all. Removing it — or
/// despawning the item — is what takes the area away, which [`rebuild`] then
/// notices.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct TrackedArea {
    pub display: u32,
    pub rect: CGRect,
}

/// Whether an item wants to hear about the pointer over it at all.
///
/// A click script counts, not just a hover subscription: both are answered
/// against the item's own frame, so both want the window server to know where
/// that frame is.
fn wants_pointer(subscriptions: Option<&Subscriptions>, click: Option<&ClickScript>) -> bool {
    click.is_some()
        || subscriptions.is_some_and(|subs| {
            subs.0.contains(&Kind::MouseEntered(())) || subs.0.contains(&Kind::MouseExited(()))
        })
}

/// Gives every item that wants the pointer a [`TrackedArea`] matching where it
/// was just laid out, and takes it from anything that no longer does.
pub fn place(
    mut commands: Commands,
    placements: Res<Placements>,
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
            if !wants_pointer(subscriptions, click) {
                if current.is_some() {
                    commands.entity(*entity).remove::<TrackedArea>();
                }
                continue;
            }
            let area = TrackedArea {
                display: placed.display,
                rect: *rect,
            };
            // Written only when it differs, so an item that was repainted
            // without moving does not make the window server rebuild.
            if current != Some(&area) {
                commands.entity(*entity).insert(area);
            }
        }
    }
}

/// Tells each display's window about its areas, when they have changed.
///
/// Rebuilt from the live components rather than from a list kept here, so an
/// item that was despawned is simply not among them.
pub fn rebuild(
    panels: NonSend<Panels>,
    areas: Query<&TrackedArea>,
    moved: Query<&TrackedArea, Changed<TrackedArea>>,
    mut gone: RemovedComponents<TrackedArea>,
) {
    // A removal does not say which display it was on, so it rebuilds all of
    // them -- but an item losing its area is rare, where an item moving is not.
    let all = gone.read().next().is_some();
    let changed: std::collections::BTreeSet<u32> = moved.iter().map(|area| area.display).collect();
    if !all && changed.is_empty() {
        return;
    }

    for panel in panels
        .iter()
        .filter(|p| all || changed.contains(&p.display.id))
    {
        if let Err(err) = panel.window.clear_tracking_rects() {
            tracing::debug!(%err, "could not clear a display's tracked areas");
            continue;
        }
        let mut watched = 0;
        for area in areas.iter().filter(|a| a.display == panel.display.id) {
            match panel.window.add_tracking_rect(area.rect) {
                Ok(()) => watched += 1,
                Err(err) => tracing::debug!(%err, "could not watch an area"),
            }
        }
        tracing::debug!(display = panel.display.id, watched, "tracked areas changed");
    }
}

//! Placing items along the bar, and drawing them.

#![allow(
    clippy::needless_pass_by_value,
    reason = "Bevy system parameters are taken by value by contract"
)]

use crate::bar::{Panels, Settings, fill_rounded_rect};
use crate::components::{Background, Drawing, Icon, Label, Offset, Padding, Placement};
use crate::shaping::Cache;
use bevy_ecs::prelude::*;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use rsbar_protocol::Position;

/// Everything laying out one item needs.
pub type ItemQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static Icon,
        &'static Label,
        &'static Background,
        &'static Padding,
        &'static Offset,
        &'static Placement,
        &'static Drawing,
    ),
>;

/// How wide an item is, padding included.
fn width(cache: &Cache, entity: Entity, icon: &Icon, label: &Label, padding: &Padding) -> f64 {
    let Some(shaped) = cache.get(entity) else {
        return padding.left + padding.right;
    };
    let icon_w = if icon.0.is_empty() {
        0.0
    } else {
        shaped.icon_metrics().width
    };
    let label_w = if label.0.is_empty() {
        0.0
    } else {
        shaped.label_metrics().width
    };
    let between = if icon_w > 0.0 && label_w > 0.0 {
        padding.between
    } else {
        0.0
    };
    padding.left + icon_w + between + label_w + padding.right
}

/// Assigns every drawn item a frame within a panel of `size`.
///
/// The buckets differ on purpose. Left runs left to right. Right and
/// centre-right run right to left, so trailing edges stay pinned as content
/// resizes. The centre group is measured whole before placing, so it stays
/// centred rather than growing from its left edge.
fn place(items: &ItemQuery, cache: &Cache, size: CGSize) -> Vec<(Entity, CGRect)> {
    let widths: Vec<(Entity, Position, f64)> = items
        .iter()
        .filter(|(.., drawing)| drawing.0)
        .map(|(entity, icon, label, _, padding, _, placement, _)| {
            (
                entity,
                placement.0,
                width(cache, entity, icon, label, padding),
            )
        })
        .collect();

    let in_bucket = |bucket: Position| widths.iter().filter(move |(_, p, _)| *p == bucket);
    let group_width = |bucket: Position| in_bucket(bucket).map(|(.., w)| *w).sum::<f64>();

    let mut placed = Vec::with_capacity(widths.len());
    let mut push = |entity, x: f64, w: f64| {
        placed.push((
            entity,
            CGRect::new(CGPoint::new(x, 0.0), CGSize::new(w, size.height)),
        ));
    };

    let mut x = 0.0;
    for (entity, _, w) in in_bucket(Position::Left) {
        push(*entity, x, *w);
        x += w;
    }

    let mut x = size.width;
    for (entity, _, w) in in_bucket(Position::Right)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        x -= w;
        push(*entity, x, *w);
    }

    let centre_start = (size.width - group_width(Position::Center)) / 2.0;
    let mut x = centre_start;
    for (entity, _, w) in in_bucket(Position::Center) {
        push(*entity, x, *w);
        x += w;
    }
    let centre_end = x;

    let mut x = centre_start;
    for (entity, _, w) in in_bucket(Position::CenterLeft)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        x -= w;
        push(*entity, x, *w);
    }

    let mut x = centre_end;
    for (entity, _, w) in in_bucket(Position::CenterRight) {
        push(*entity, x, *w);
        x += w;
    }

    placed
}

/// Lays out and repaints every panel.
///
/// Runs only when something changed — see [`needs_repaint`].
pub fn repaint(
    items: ItemQuery,
    cache: NonSend<Cache>,
    panels: NonSend<Panels>,
    settings: Res<Settings>,
) {
    if settings.hidden {
        return;
    }

    for panel in panels.iter() {
        let size = panel.frame.size;
        // Layout depends on the panel's width, so it is per display.
        let placements = place(&items, &cache, size);

        skylight::draw(panel.window.id(), size, |ctx| {
            fill_rounded_rect(
                ctx,
                CGRect::new(CGPoint::new(0.0, 0.0), size),
                settings.corner_radius,
                settings.color,
            );

            for (entity, frame) in placements {
                let Ok((_, icon, label, background, padding, offset, _, _)) = items.get(entity)
                else {
                    continue;
                };
                let Some(shaped) = cache.get(entity) else {
                    continue;
                };

                if !background.color.is_invisible() {
                    fill_rounded_rect(ctx, frame, background.corner_radius, background.color);
                }

                let mut x = frame.origin.x + padding.left;
                let y = frame.origin.y + offset.0;
                if !icon.0.is_empty() {
                    let w = shaped.icon_metrics().width;
                    let box_ = CGRect::new(CGPoint::new(x, y), CGSize::new(w, frame.size.height));
                    shaped.draw_icon(ctx, box_, icon.0.color);
                    x += w + padding.between;
                }
                if !label.0.is_empty() {
                    let w = shaped.label_metrics().width;
                    let box_ = CGRect::new(CGPoint::new(x, y), CGSize::new(w, frame.size.height));
                    shaped.draw_label(ctx, box_, label.0.color);
                }
            }
        });
    }
}

/// Any item whose on-screen appearance moved. Every component that layout or
/// drawing reads is listed, so adding one to either without adding it here is
/// the one way this can go quietly wrong.
type AnythingVisibleChanged<'w, 's> = Query<
    'w,
    's,
    (),
    Or<(
        Changed<Icon>,
        Changed<Label>,
        Changed<Background>,
        Changed<Padding>,
        Changed<Offset>,
        Changed<Placement>,
        Changed<Drawing>,
    )>,
>;

/// Whether anything that affects what is on screen has moved since the last
/// pass.
///
/// This is what the hand-rolled dirty flag used to be, except that nobody has
/// to remember to set it: a component that changes says so, and one that is
/// added or removed does too.
#[must_use]
pub fn needs_repaint(
    changed: AnythingVisibleChanged,
    mut removed: RemovedComponents<Placement>,
    settings: Res<Settings>,
    repaint: Res<ForceRepaint>,
) -> bool {
    repaint.0 || settings.is_changed() || !changed.is_empty() || removed.read().next().is_some()
}

/// Set when something outside the item world needs the bar redrawn — a display
/// change, say, where nothing about the items moved but their panel did.
#[derive(Resource, Default)]
pub struct ForceRepaint(pub bool);

/// Clears the force flag after a pass, so it means "once" rather than "always".
pub fn clear_force_repaint(mut repaint: ResMut<ForceRepaint>) {
    repaint.0 = false;
}

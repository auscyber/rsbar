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

/// One item, reduced to what layout actually needs.
///
/// Layout is pure arithmetic over this, so it is testable without a World —
/// which matters, because the bucket rules are the part most likely to be got
/// subtly wrong and least likely to be noticed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placed<T> {
    pub id: T,
    pub position: Position,
    pub width: f64,
}

/// Assigns every item an x offset within a bar of `width`.
///
/// The buckets differ on purpose. Left runs left to right. Right and
/// centre-right run right to left, so trailing edges stay pinned as content
/// resizes. The centre group is measured whole before placing, so it stays
/// centred rather than growing from its left edge, and the two centre-adjacent
/// buckets hang off its edges rather than being re-centred with it.
pub fn arrange<T: Copy>(items: &[Placed<T>], width: f64) -> Vec<(T, f64, f64)> {
    let in_bucket = |bucket: Position| items.iter().filter(move |i| i.position == bucket);
    let group_width = |bucket: Position| in_bucket(bucket).map(|i| i.width).sum::<f64>();

    let mut placed = Vec::with_capacity(items.len());

    let mut x = 0.0;
    for item in in_bucket(Position::Left) {
        placed.push((item.id, x, item.width));
        x += item.width;
    }

    let mut x = width;
    for item in in_bucket(Position::Right)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        x -= item.width;
        placed.push((item.id, x, item.width));
    }

    let centre_start = (width - group_width(Position::Center)) / 2.0;
    let mut x = centre_start;
    for item in in_bucket(Position::Center) {
        placed.push((item.id, x, item.width));
        x += item.width;
    }
    let centre_end = x;

    let mut x = centre_start;
    for item in in_bucket(Position::CenterLeft)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        x -= item.width;
        placed.push((item.id, x, item.width));
    }

    let mut x = centre_end;
    for item in in_bucket(Position::CenterRight) {
        placed.push((item.id, x, item.width));
        x += item.width;
    }

    placed
}

/// Gathers the drawn items and hands them to [`arrange`].
fn place(items: &ItemQuery, cache: &Cache, size: CGSize) -> Vec<(Entity, CGRect)> {
    let measured: Vec<Placed<Entity>> = items
        .iter()
        .filter(|(.., drawing)| drawing.0)
        .map(
            |(entity, icon, label, _, padding, _, placement, _)| Placed {
                id: entity,
                position: placement.0,
                width: width(cache, entity, icon, label, padding),
            },
        )
        .collect();

    arrange(&measured, size.width)
        .into_iter()
        .map(|(entity, x, w)| {
            (
                entity,
                CGRect::new(CGPoint::new(x, 0.0), CGSize::new(w, size.height)),
            )
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::{Placed, arrange};
    use rsbar_protocol::Position;

    fn item(id: u8, position: Position, width: f64) -> Placed<u8> {
        Placed {
            id,
            position,
            width,
        }
    }

    /// x offset of one id, for readable assertions.
    fn x_of(placed: &[(u8, f64, f64)], id: u8) -> f64 {
        placed
            .iter()
            .find(|(i, ..)| *i == id)
            .expect("id was placed")
            .1
    }

    #[test]
    fn left_runs_left_to_right_from_the_edge() {
        let placed = arrange(
            &[item(1, Position::Left, 30.0), item(2, Position::Left, 20.0)],
            200.0,
        );
        assert!((x_of(&placed, 1) - 0.0).abs() < 1e-9);
        assert!((x_of(&placed, 2) - 30.0).abs() < 1e-9);
    }

    #[test]
    fn right_pins_its_trailing_edge() {
        let placed = arrange(
            &[
                item(1, Position::Right, 30.0),
                item(2, Position::Right, 20.0),
            ],
            200.0,
        );
        // Last added sits hard against the right edge; the first sits left of it.
        assert!((x_of(&placed, 2) - 180.0).abs() < 1e-9);
        assert!((x_of(&placed, 1) - 150.0).abs() < 1e-9);
    }

    #[test]
    fn right_stays_pinned_when_content_grows() {
        let narrow = arrange(&[item(1, Position::Right, 20.0)], 200.0);
        let wide = arrange(&[item(1, Position::Right, 60.0)], 200.0);
        // The trailing edge is what must not move, not the origin.
        assert!((x_of(&narrow, 1) + 20.0 - 200.0).abs() < 1e-9);
        assert!((x_of(&wide, 1) + 60.0 - 200.0).abs() < 1e-9);
    }

    #[test]
    fn centre_is_centred_as_a_group_not_item_by_item() {
        let placed = arrange(
            &[
                item(1, Position::Center, 40.0),
                item(2, Position::Center, 60.0),
            ],
            200.0,
        );
        // Group is 100 wide, so it starts at 50 and ends at 150.
        assert!((x_of(&placed, 1) - 50.0).abs() < 1e-9);
        assert!((x_of(&placed, 2) - 90.0).abs() < 1e-9);
    }

    #[test]
    fn centre_adjacent_buckets_hang_off_the_centre_group() {
        let placed = arrange(
            &[
                item(1, Position::Center, 40.0),
                item(2, Position::CenterLeft, 10.0),
                item(3, Position::CenterRight, 10.0),
            ],
            200.0,
        );
        // Centre spans 80..120, so its neighbours abut it without re-centring.
        assert!((x_of(&placed, 1) - 80.0).abs() < 1e-9);
        assert!((x_of(&placed, 2) - 70.0).abs() < 1e-9);
        assert!((x_of(&placed, 3) - 120.0).abs() < 1e-9);
    }

    #[test]
    fn buckets_do_not_interfere() {
        let placed = arrange(
            &[
                item(1, Position::Left, 25.0),
                item(2, Position::Center, 50.0),
                item(3, Position::Right, 25.0),
            ],
            200.0,
        );
        assert!((x_of(&placed, 1) - 0.0).abs() < 1e-9);
        assert!((x_of(&placed, 2) - 75.0).abs() < 1e-9);
        assert!((x_of(&placed, 3) - 175.0).abs() < 1e-9);
    }

    #[test]
    fn an_empty_bar_places_nothing() {
        assert!(arrange::<u8>(&[], 200.0).is_empty());
    }
}

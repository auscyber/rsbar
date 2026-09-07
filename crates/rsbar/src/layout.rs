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

/// Where each item ended up, per panel, from the last repaint.
///
/// Retained rather than recomputed: a click has to be answered against what is
/// actually on screen, and re-running layout at click time could disagree with
/// it — an item whose script has since changed its width would move under the
/// cursor between the press and the lookup.
#[derive(Resource, Default)]
pub struct Placements(Vec<PanelPlacements>);

struct PanelPlacements {
    display: u32,
    /// The panel's frame in global screen coordinates, so a click reported
    /// against the desktop can be brought into the panel's own space.
    frame: CGRect,
    items: Vec<(Entity, CGRect)>,
}

impl Placements {
    /// Builds a set directly, for tests.
    #[cfg(test)]
    fn from_parts(display: u32, frame: CGRect, items: Vec<(Entity, CGRect)>) -> Self {
        Self(vec![PanelPlacements {
            display,
            frame,
            items,
        }])
    }

    /// The item at a point given in global screen coordinates.
    ///
    /// Returns the panel's display too, so a handler knows which bar was hit
    /// even when the click landed on empty space.
    #[must_use]
    pub fn hit(&self, point: CGPoint) -> Hit {
        let Some(panel) = self.0.iter().find(|panel| contains(panel.frame, point)) else {
            return Hit::Nothing;
        };
        let local = CGPoint::new(
            point.x - panel.frame.origin.x,
            point.y - panel.frame.origin.y,
        );
        panel
            .items
            .iter()
            .find(|(_, frame)| contains(*frame, local))
            .map_or(
                Hit::Bar {
                    display: panel.display,
                },
                |(entity, _)| Hit::Item {
                    entity: *entity,
                    display: panel.display,
                },
            )
    }
}

/// What a point landed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// An item, which is what a click should be delivered to.
    Item { entity: Entity, display: u32 },
    /// The bar, but not an item — the empty space between them.
    Bar { display: u32 },
    /// Not the bar at all.
    Nothing,
}

/// Half-open on the far edges, so two abutting items cannot both claim a point.
fn contains(rect: CGRect, point: CGPoint) -> bool {
    point.x >= rect.origin.x
        && point.y >= rect.origin.y
        && point.x < rect.origin.x + rect.size.width
        && point.y < rect.origin.y + rect.size.height
}

/// Lays out and repaints every panel.
///
/// Runs only when something changed — see [`needs_repaint`].
pub fn repaint(
    items: ItemQuery,
    cache: NonSend<Cache>,
    panels: NonSend<Panels>,
    settings: Res<Settings>,
    mut placements: ResMut<Placements>,
) {
    placements.0.clear();
    if settings.hidden {
        return;
    }

    // One window server update for every panel, not one per panel. Each
    // `draw` publishes its window as it finishes, so without this a second
    // display shows the previous frame until its own turn comes round — and
    // every panel erases to the background before its items go back down,
    // which is a flash of empty bar if that lands on screen by itself.
    skylight::batched(|| {
        paint_panels(&items, &cache, &panels, &settings, &mut placements);
    });
}

fn paint_panels(
    items: &ItemQuery,
    cache: &Cache,
    panels: &Panels,
    settings: &Settings,
    placements: &mut Placements,
) {
    for panel in panels.iter() {
        let size = panel.frame.size;
        // Layout depends on the panel's width, so it is per display.
        let placed = place(items, cache, size);
        placements.0.push(PanelPlacements {
            display: panel.display.id,
            frame: panel.frame,
            items: placed.clone(),
        });

        skylight::draw(panel.window.id(), size, |ctx| {
            fill_rounded_rect(
                ctx,
                CGRect::new(CGPoint::new(0.0, 0.0), size),
                settings.corner_radius,
                settings.color,
            );

            for (entity, frame) in placed {
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

#[cfg(test)]
mod hit_tests {
    use super::{Hit, Placements};
    use bevy_ecs::prelude::Entity;
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
    }

    fn entity(id: u32) -> Entity {
        Entity::from_raw_u32(id).expect("valid entity id")
    }

    /// A bar 200 wide at the top of a display whose origin is (100, 0), with
    /// two abutting items.
    fn placements() -> Placements {
        Placements::from_parts(
            1,
            rect(100.0, 0.0, 200.0, 30.0),
            vec![
                (entity(1), rect(0.0, 0.0, 50.0, 30.0)),
                (entity(2), rect(50.0, 0.0, 50.0, 30.0)),
            ],
        )
    }

    #[test]
    fn a_click_lands_on_the_item_under_it() {
        // Global (120, 10) is local (20, 10), inside the first item.
        assert_eq!(
            placements().hit(CGPoint::new(120.0, 10.0)),
            Hit::Item {
                entity: entity(1),
                display: 1
            }
        );
    }

    #[test]
    fn the_panel_origin_is_subtracted_before_looking_up() {
        // The same local point on a panel that does not start at zero must not
        // resolve to the same item as an unshifted one would.
        assert_eq!(
            placements().hit(CGPoint::new(20.0, 10.0)),
            Hit::Nothing,
            "a point left of the panel is not on the bar at all"
        );
    }

    #[test]
    fn abutting_items_do_not_both_claim_the_boundary() {
        // Local x = 50 is the first item's far edge and the second's origin.
        assert_eq!(
            placements().hit(CGPoint::new(150.0, 10.0)),
            Hit::Item {
                entity: entity(2),
                display: 1
            },
            "the far edge belongs to the next item, not both"
        );
    }

    #[test]
    fn empty_bar_space_is_the_bar_not_an_item() {
        // Local x = 150 is past both items but still on the panel.
        assert_eq!(
            placements().hit(CGPoint::new(250.0, 10.0)),
            Hit::Bar { display: 1 }
        );
    }

    #[test]
    fn below_the_bar_is_nothing() {
        assert_eq!(placements().hit(CGPoint::new(120.0, 40.0)), Hit::Nothing);
    }

    #[test]
    fn nothing_is_hit_when_the_bar_has_never_been_drawn() {
        assert_eq!(
            Placements::default().hit(CGPoint::new(0.0, 0.0)),
            Hit::Nothing
        );
    }
}

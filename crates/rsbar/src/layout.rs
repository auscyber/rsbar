//! Placing items along the bar, and drawing them.

#![allow(
    clippy::needless_pass_by_value,
    reason = "Bevy system parameters are taken by value by contract"
)]

use crate::alias::Captures;
use crate::bar::{Panels, Settings, fill_rounded_rect};
use crate::components::{
    AliasContent, Background, Drawing, Icon, Label, Offset, Padding, Placement,
};
use crate::shaping::Cache;
use bevy_ecs::prelude::*;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use rsbar_protocol::Position;
use std::collections::{HashMap, HashSet};

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

/// How wide an alias's mirrored image is, or nothing if it is not an alias.
fn alias_width(captures: &Captures, entity: Entity, padding: &Padding) -> Option<f64> {
    let mirrored = captures.get(entity)?;
    Some(padding.left + mirrored.size.width + padding.right)
}

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
fn place(
    items: &ItemQuery,
    cache: &Cache,
    captures: &Captures,
    size: CGSize,
) -> Vec<(Entity, CGRect)> {
    let measured: Vec<Placed<Entity>> = items
        .iter()
        .filter(|(.., drawing)| drawing.0)
        .map(
            |(entity, icon, label, _, padding, _, placement, _)| Placed {
                id: entity,
                position: placement.0,
                // An alias is as wide as what it mirrors; its own text, if it
                // has any, is not drawn.
                width: alias_width(captures, entity, padding)
                    .unwrap_or_else(|| width(cache, entity, icon, label, padding)),
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

/// Everything drawing reads that is not an item: the shaped text, the
/// mirrored images, and the windows to draw into.
///
/// Grouped because they always travel together, and a system taking them
/// separately alongside its queries runs past what is readable.
#[derive(bevy_ecs::system::SystemParam)]
pub struct Surfaces<'w> {
    cache: NonSend<'w, Cache>,
    captures: NonSend<'w, Captures>,
    panels: NonSend<'w, Panels>,
}

/// Lays out and repaints every panel.
///
/// Runs only when something changed — see [`needs_repaint`].
pub fn repaint(
    items: ItemQuery,
    dirty: DirtyItems,
    surfaces: Surfaces,
    settings: Res<Settings>,
    force: Res<ForceRepaint>,
    mut placements: ResMut<Placements>,
) {
    let Surfaces {
        cache,
        captures,
        panels,
    } = surfaces;
    // Taken, not cleared: where every item was last time is what says which
    // pixels an item that has moved left behind.
    let previous = std::mem::take(&mut placements.0);
    if settings.hidden {
        return;
    }

    // A bar-wide change moves everything, so working out what moved is wasted
    // effort — and a forced repaint is by definition one nothing tracked.
    let redraw_everything = force.0 || settings.is_changed();
    let changed: HashSet<Entity> = dirty.iter().collect();

    // Deliberately not wrapped in `skylight::batched`. `SLSDisableUpdate`
    // suspends compositing for the whole display, and holding it across work
    // this open-ended — a draw per panel, per repaint, forever — is how a bar
    // freezes the desktop rather than just itself. `SketchyBar` stopped
    // calling it altogether on macOS 26, and even before that only ever held
    // it across window *geometry* changes, never across drawing.
    //
    // Nothing needs it here any more: a partial repaint no longer erases the
    // panel before putting the items back, so there is no half-drawn state
    // that batching was hiding.
    paint_panels(
        &items,
        &cache,
        &captures,
        &panels,
        &settings,
        &Repaint {
            previous: &previous,
            changed: &changed,
            everything: redraw_everything,
        },
        &mut placements,
    );
}

/// What the last pass left on screen, and what has moved since.
struct Repaint<'a> {
    previous: &'a [PanelPlacements],
    changed: &'a HashSet<Entity>,
    everything: bool,
}

/// The rects of one panel that may look different this pass.
///
/// `None` means the whole panel: either something bar-wide changed, or this
/// panel was not on screen last time and there is no before to compare with.
fn damage(
    panel: &Repaint,
    display: u32,
    frame: CGRect,
    placed: &[(Entity, CGRect)],
) -> Option<Vec<CGRect>> {
    if panel.everything {
        return None;
    }
    let before = panel.previous.iter().find(|p| p.display == display)?;
    // A panel that moved or resized invalidates every position in it.
    if before.frame != frame {
        return None;
    }

    // The usual pass has the same items in the same order — nothing was added
    // or removed, something just changed inside one of them. Diffing that
    // pairwise costs a walk and no allocation, where building a map of every
    // previous placement to look each one up cost more than drawing the item
    // that actually changed.
    if before.items.len() == placed.len()
        && before
            .items
            .iter()
            .zip(placed)
            .all(|((was, _), (now, _))| was == now)
    {
        let mut rects = Vec::new();
        for ((entity, was), (_, now)) in before.items.iter().zip(placed) {
            if was == now {
                if panel.changed.contains(entity) {
                    rects.push(*now);
                }
            } else {
                // Both ends: one to erase, one to draw.
                rects.push(*was);
                rects.push(*now);
            }
        }
        return Some(rects);
    }

    let was: HashMap<Entity, CGRect> = before.items.iter().copied().collect();
    let mut rects = Vec::new();
    for (entity, now) in placed {
        match was.get(entity) {
            // New here: only where it landed needs painting.
            None => rects.push(*now),
            // Moved or resized. Both ends: one to erase, one to draw. This is
            // also what catches an item shifted along by a *neighbour* growing,
            // which no amount of change detection on the item itself would.
            Some(before) if before != now => {
                rects.push(*before);
                rects.push(*now);
            }
            // Sitting still, but repainted itself.
            Some(_) if panel.changed.contains(entity) => rects.push(*now),
            Some(_) => {}
        }
    }
    // Gone: what it used to cover goes back to bar.
    for (entity, before) in &before.items {
        if !placed.iter().any(|(here, _)| here == entity) {
            rects.push(*before);
        }
    }
    Some(rects)
}

fn intersects(a: CGRect, b: CGRect) -> bool {
    a.origin.x < b.origin.x + b.size.width
        && b.origin.x < a.origin.x + a.size.width
        && a.origin.y < b.origin.y + b.size.height
        && b.origin.y < a.origin.y + a.size.height
}

fn paint_panels(
    items: &ItemQuery,
    cache: &Cache,
    captures: &Captures,
    panels: &Panels,
    settings: &Settings,
    pass: &Repaint,
    placements: &mut Placements,
) {
    for panel in panels.iter() {
        let size = panel.frame.size;
        // Layout depends on the panel's width, so it is per display.
        let placed = place(items, cache, captures, size);
        let torn = damage(pass, panel.display.id, panel.frame, &placed);

        // Nothing on this display looks different — the common case on a
        // multi-display bar, where one panel's clock ticks and the rest do
        // not. The placements are still recorded below either way: a click is
        // answered against them, so losing them would stop the bar taking
        // clicks until something moved.
        let unchanged = torn.as_ref().is_some_and(Vec::is_empty);

        let total = placed.len();
        let mut drawn = 0usize;
        if !unchanged {
            skylight::draw_damaged(panel.window.id(), size, torn.as_deref(), |ctx| {
                fill_rounded_rect(
                    ctx,
                    CGRect::new(CGPoint::new(0.0, 0.0), size),
                    settings.corner_radius,
                    settings.color,
                );

                for &(entity, frame) in &placed {
                    // Everything overlapping the damage, not only what changed: the
                    // damaged pixels were cleared, so an untouched item sitting in
                    // them has to go back down too.
                    if let Some(torn) = &torn
                        && !torn.iter().any(|rect| intersects(*rect, frame))
                    {
                        continue;
                    }
                    let Ok((_, icon, label, background, padding, offset, _, _)) = items.get(entity)
                    else {
                        continue;
                    };
                    let Some(shaped) = cache.get(entity) else {
                        continue;
                    };
                    drawn += 1;

                    if !background.color.is_invisible() {
                        fill_rounded_rect(ctx, frame, background.corner_radius, background.color);
                    }

                    // An alias draws what it mirrors, and nothing else.
                    if let Some(captured) = captures.get(entity) {
                        // Centred in the item rather than hung from its top
                        // edge. A menu bar extra is captured at the menu bar's
                        // height, which is not the bar's, so aligning the two
                        // tops sits it visibly higher than the text beside it.
                        let slack = (frame.size.height - captured.size.height) / 2.0;
                        let box_ = CGRect::new(
                            CGPoint::new(
                                frame.origin.x + padding.left,
                                frame.origin.y + offset.0 + slack,
                            ),
                            captured.size,
                        );
                        crate::bar::draw_image(ctx, box_, &captured.image);
                        continue;
                    }

                    let mut x = frame.origin.x + padding.left;
                    let y = frame.origin.y + offset.0;
                    if !icon.0.is_empty() {
                        let w = shaped.icon_metrics().width;
                        let box_ =
                            CGRect::new(CGPoint::new(x, y), CGSize::new(w, frame.size.height));
                        shaped.draw_icon(ctx, box_, icon.0.color);
                        x += w + padding.between;
                    }
                    if !label.0.is_empty() {
                        let w = shaped.label_metrics().width;
                        let box_ =
                            CGRect::new(CGPoint::new(x, y), CGSize::new(w, frame.size.height));
                        shaped.draw_label(ctx, box_, label.0.color);
                    }
                }
            });
        }

        placements.0.push(PanelPlacements {
            display: panel.display.id,
            frame: panel.frame,
            items: placed,
        });

        tracing::debug!(
            display = panel.display.id,
            whole = torn.is_none(),
            rects = torn.as_ref().map_or(0, Vec::len),
            drawn,
            total,
            "repainted"
        );
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
        // The digest of what an alias mirrors. Without this the capture
        // refreshes and the component changes, but nothing asks for a
        // repaint — a mirrored clock sits at the minute it was first drawn.
        Changed<AliasContent>,
    )>,
>;

/// The items that changed since the last repaint, as opposed to whether any
/// did. Same components as [`AnythingVisibleChanged`], because a change that
/// forces a repaint and a change that damages a rect are the same change.
pub type DirtyItems<'w, 's> = Query<
    'w,
    's,
    Entity,
    Or<(
        Changed<Icon>,
        Changed<Label>,
        Changed<Background>,
        Changed<Padding>,
        Changed<Offset>,
        Changed<Placement>,
        Changed<Drawing>,
        // The digest of what an alias mirrors. Without this the capture
        // refreshes and the component changes, but nothing asks for a
        // repaint — a mirrored clock sits at the minute it was first drawn.
        Changed<AliasContent>,
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

#[cfg(test)]
mod damage_tests {
    use super::{PanelPlacements, Repaint, damage, intersects};
    use bevy_ecs::entity::Entity;
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use std::collections::HashSet;

    const DISPLAY: u32 = 1;

    fn entity(index: u32) -> Entity {
        Entity::from_raw_u32(index).expect("a valid entity index")
    }

    fn rect(x: f64, width: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, 0.0), CGSize::new(width, 32.0))
    }

    fn panel() -> CGRect {
        CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1000.0, 32.0))
    }

    fn was(items: Vec<(Entity, CGRect)>) -> Vec<PanelPlacements> {
        vec![PanelPlacements {
            display: DISPLAY,
            frame: panel(),
            items,
        }]
    }

    fn pass<'a>(previous: &'a [PanelPlacements], changed: &'a HashSet<Entity>) -> Repaint<'a> {
        Repaint {
            previous,
            changed,
            everything: false,
        }
    }

    #[test]
    fn a_pass_where_nothing_moved_damages_nothing() {
        let before = was(vec![(entity(1), rect(0.0, 100.0))]);
        let nothing = HashSet::new();
        let torn = damage(
            &pass(&before, &nothing),
            DISPLAY,
            panel(),
            &[(entity(1), rect(0.0, 100.0))],
        );
        assert_eq!(torn, Some(Vec::new()), "no rect should be repainted");
    }

    #[test]
    fn an_item_repainting_in_place_damages_only_itself() {
        let before = was(vec![
            (entity(1), rect(0.0, 100.0)),
            (entity(2), rect(100.0, 50.0)),
        ]);
        let changed = HashSet::from([entity(2)]);
        let torn = damage(
            &pass(&before, &changed),
            DISPLAY,
            panel(),
            &[
                (entity(1), rect(0.0, 100.0)),
                (entity(2), rect(100.0, 50.0)),
            ],
        );
        assert_eq!(torn, Some(vec![rect(100.0, 50.0)]));
    }

    #[test]
    fn an_item_shifted_by_its_neighbour_growing_is_damaged_at_both_ends() {
        // The one that earns this. Change detection says only the first item
        // changed, but the second one moved because of it — damaging only what
        // Bevy called changed would leave the second drawn at its old x as
        // well as its new one.
        let before = was(vec![
            (entity(1), rect(0.0, 100.0)),
            (entity(2), rect(100.0, 50.0)),
        ]);
        let changed = HashSet::from([entity(1)]);
        let torn = damage(
            &pass(&before, &changed),
            DISPLAY,
            panel(),
            &[
                (entity(1), rect(0.0, 140.0)),
                (entity(2), rect(140.0, 50.0)),
            ],
        )
        .expect("a partial repaint");

        assert!(torn.contains(&rect(0.0, 100.0)), "erase the old width");
        assert!(torn.contains(&rect(0.0, 140.0)), "draw the new width");
        assert!(torn.contains(&rect(100.0, 50.0)), "erase where it sat");
        assert!(torn.contains(&rect(140.0, 50.0)), "draw where it sits now");
    }

    #[test]
    fn an_item_that_went_away_leaves_damage_where_it_was() {
        let before = was(vec![
            (entity(1), rect(0.0, 100.0)),
            (entity(2), rect(100.0, 50.0)),
        ]);
        let nothing = HashSet::new();
        let torn = damage(
            &pass(&before, &nothing),
            DISPLAY,
            panel(),
            &[(entity(1), rect(0.0, 100.0))],
        );
        assert_eq!(torn, Some(vec![rect(100.0, 50.0)]));
    }

    #[test]
    fn a_new_item_damages_where_it_landed() {
        let before = was(vec![(entity(1), rect(0.0, 100.0))]);
        let nothing = HashSet::new();
        let torn = damage(
            &pass(&before, &nothing),
            DISPLAY,
            panel(),
            &[
                (entity(1), rect(0.0, 100.0)),
                (entity(2), rect(100.0, 50.0)),
            ],
        );
        assert_eq!(torn, Some(vec![rect(100.0, 50.0)]));
    }

    #[test]
    fn a_panel_that_resized_is_repainted_whole() {
        let before = was(vec![(entity(1), rect(0.0, 100.0))]);
        let nothing = HashSet::new();
        let narrower = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(800.0, 32.0));
        let torn = damage(
            &pass(&before, &nothing),
            DISPLAY,
            narrower,
            &[(entity(1), rect(0.0, 100.0))],
        );
        assert_eq!(torn, None, "every position in it is invalid");
    }

    #[test]
    fn a_panel_with_nothing_before_it_is_repainted_whole() {
        let nothing = HashSet::new();
        let torn = damage(
            &pass(&[], &nothing),
            DISPLAY,
            panel(),
            &[(entity(1), rect(0.0, 100.0))],
        );
        assert_eq!(torn, None, "there is no before to diff against");
    }

    #[test]
    fn a_bar_wide_change_is_repainted_whole() {
        let before = was(vec![(entity(1), rect(0.0, 100.0))]);
        let nothing = HashSet::new();
        let everything = Repaint {
            previous: &before,
            changed: &nothing,
            everything: true,
        };
        assert_eq!(
            damage(
                &everything,
                DISPLAY,
                panel(),
                &[(entity(1), rect(0.0, 100.0))]
            ),
            None
        );
    }

    #[test]
    fn only_overlapping_rects_intersect() {
        assert!(intersects(rect(0.0, 100.0), rect(50.0, 100.0)));
        assert!(!intersects(rect(0.0, 100.0), rect(100.0, 50.0)));
        assert!(!intersects(rect(0.0, 100.0), rect(200.0, 50.0)));
    }
}

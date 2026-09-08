//! Placing items along the bar, and drawing them.

#![allow(
    clippy::needless_pass_by_value,
    reason = "Bevy system parameters are taken by value by contract"
)]

use crate::alias::Captures;
use crate::bar::{Panels, Settings, fill_rounded_rect, stroke_rounded_rect};
use crate::components::{
    AliasContent, Background, Drawing, Graph, Icon, ItemDisplay, Label, Members, Name, Offset,
    Order, Padding, Placement, Slider, Width,
};
use crate::popup::PopupOf;
use crate::shaping::Cache;
use bevy_ecs::prelude::*;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use rsbar_protocol::Position;
use std::collections::{HashMap, HashSet};

/// Everything laying out one item needs.
/// Everything laying out and drawing one item needs.
///
/// Named rather than positional. It is ten columns now, and a tuple at that
/// size is a bug waiting to happen: adding one silently rebinds everything
/// after it, which is exactly how the click script once ended up where the
/// update script belonged.
#[derive(bevy_ecs::query::QueryData)]
pub struct Drawn {
    pub entity: Entity,
    pub icon: &'static Icon,
    pub label: &'static Label,
    pub background: &'static Background,
    pub padding: &'static Padding,
    pub offset: &'static Offset,
    pub placement: &'static Placement,
    pub order: &'static Order,
    pub drawing: &'static Drawing,
    pub width: &'static Width,
    pub display: &'static ItemDisplay,
    pub name: &'static Name,
    /// Set only on a `--add graph` item.
    pub graph: Option<&'static Graph>,
    /// Set only on a `--add slider` item.
    pub slider: Option<&'static Slider>,
    /// Set only on a bracket: the items it draws across.
    pub members: Option<&'static Members>,
    /// Set only on an item living inside a popup, naming the item that hosts
    /// it. Such an item takes no space in the bar's own layout — it is laid
    /// out by [`crate::popup`] instead, against the popup's own surface.
    pub popup_of: Option<&'static PopupOf>,
}

pub type ItemQuery<'w, 's> = Query<'w, 's, Drawn>;

/// An alias's left inset and overall frame width, given its own padding and
/// the width of the ink it is actually going to draw.
///
/// The ink is already trimmed to the window's real content, so the padding a
/// config sets is on top of that, not on top of the window's full (mostly
/// margin) size. A config written against real `SketchyBar`, which has no
/// trimming and expects `padding_left`/`padding_right` to claw back its
/// window's own margin, can therefore ask for a total more negative than the
/// ink itself — measured live, `Control Centre,FocusModes`' `-15`/`-5` against
/// an 18pt-wide trim comes to `-2`. Letting that through would draw the ink
/// `padding.left` short of the frame `alias_width` sized to, spilling onto
/// whatever sits to this item's left. Clamping the slack at zero keeps both
/// numbers agreeing — the frame never shrinks past the ink, and the ink never
/// starts outside the frame it was given — while a config with ordinary,
/// non-negative padding is unaffected.
fn alias_box(padding: &Padding, ink_width: f64) -> (f64, f64) {
    let slack = (padding.left + padding.right).max(0.0);
    let left = padding.left.clamp(0.0, slack);
    (left, ink_width + slack)
}

/// How wide an alias's mirrored image is, or nothing if it is not an alias.
fn alias_width(captures: &Captures, entity: Entity, padding: &Padding) -> Option<f64> {
    let mirrored = captures.get(entity)?;
    // The inked width, not the captured window's. A menu bar extra's window
    // carries the system's own inter-item spacing — on the clock, only 81% of
    // it is ink — and laying that out put a visible gap either side of every
    // alias, twice over between two of them.
    let (_, width) = alias_box(padding, mirrored.trim.size.width);
    Some(width)
}

/// A graph's or a slider's own width, or nothing if the item is neither —
/// `graph_get_length`/`slider_get_length` in `SketchyBar`'s own source. An
/// item is never both, but nothing here enforces that; a config that manages
/// it gets whichever the query happened to find.
fn sandwich_width(graph: Option<&Graph>, slider: Option<&Slider>) -> f64 {
    #[allow(
        clippy::cast_precision_loss,
        reason = "a graph's sample count is nowhere near f64's exact-integer range"
    )]
    graph
        .map(|g| g.capacity as f64)
        .or_else(|| slider.map(|s| s.width))
        .unwrap_or(0.0)
}

/// How wide an item is, padding included.
///
/// Mirrors the draw order in `paint_panels` exactly: each run's own padding is
/// added on top of the item's, so an icon or label that sets it does not drift
/// out of the background this same width sizes. `SketchyBar`'s own item
/// order — icon, then a graph or slider, then label (`bar_item_get_length`,
/// `label_position`) — is why the sandwich segment sits between the two runs
/// rather than after both.
pub(crate) fn width(
    cache: &Cache,
    entity: Entity,
    icon: &Icon,
    label: &Label,
    padding: &Padding,
    graph: Option<&Graph>,
    slider: Option<&Slider>,
) -> f64 {
    let Some(shaped) = cache.get(entity) else {
        return padding.left + padding.right;
    };
    let icon_w = if icon.0.is_empty() {
        0.0
    } else {
        icon.0.padding_left + shaped.icon_metrics().width + icon.0.padding_right
    };
    let sandwich_w = sandwich_width(graph, slider);
    let label_w = if label.0.is_empty() {
        0.0
    } else {
        label.0.padding_left + shaped.label_metrics().width + label.0.padding_right
    };
    let before_sandwich = if icon_w > 0.0 && sandwich_w > 0.0 {
        padding.between
    } else {
        0.0
    };
    let after_sandwich = if (icon_w > 0.0 || sandwich_w > 0.0) && label_w > 0.0 {
        padding.between
    } else {
        0.0
    };
    padding.left + icon_w + before_sandwich + sandwich_w + after_sandwich + label_w + padding.right
}

/// Where an item's background actually sits, `frame` inset by its own padding
/// and, when it sets a fixed height, shrunk and centred within the frame —
/// which is how a pill sits inside a taller bar with a margin above and below.
fn background_rect(frame: CGRect, background: &Background) -> CGRect {
    let height = if background.height > 0.0 {
        background.height.min(frame.size.height)
    } else {
        frame.size.height
    };
    let y = frame.origin.y + (frame.size.height - height) / 2.0;
    let width = (frame.size.width - background.padding_left - background.padding_right).max(0.0);
    let x = frame.origin.x + background.padding_left;
    CGRect::new(CGPoint::new(x, y), CGSize::new(width, height))
}

/// One item, reduced to what layout actually needs.
///
/// Layout is pure arithmetic over this, so it is testable without a World —
/// which matters, because the bucket rules are the part most likely to be got
/// subtly wrong and least likely to be noticed.
#[derive(Debug, Clone, PartialEq)]
pub struct Placed<T> {
    pub id: T,
    pub position: Position,
    pub width: f64,
}

/// Space inside the bar before the first item and after the last.
///
/// Not the same as the bar's own `margin`, which insets the whole window from
/// the screen edge: this insets the items from the window.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BarPadding {
    pub left: f64,
    pub right: f64,
}

/// Assigns every item an x offset within a bar of `width`.
///
/// The buckets differ on purpose. Left runs left to right. Right and
/// centre-right run right to left, so trailing edges stay pinned as content
/// resizes. The centre group is measured whole before placing, so it stays
/// centred rather than growing from its left edge, and the two centre-adjacent
/// buckets hang off its edges rather than being re-centred with it.
///
/// `padding` insets the left and right buckets from the bar's own edges. The
/// centre group is deliberately left out of that — it is centred on the whole
/// bar width regardless, matching `SketchyBar`'s own arithmetic.
pub fn arrange<T: Copy>(
    items: &[Placed<T>],
    width: f64,
    padding: BarPadding,
    notch_width: f64,
) -> Vec<(T, f64, f64)> {
    let in_bucket = |bucket: Position| items.iter().filter(move |i| i.position == bucket);
    let group_width = |bucket: Position| in_bucket(bucket).map(|i| i.width).sum::<f64>();

    let mut placed = Vec::with_capacity(items.len());

    let mut x = padding.left;
    for item in in_bucket(Position::Left) {
        placed.push((item.id, x, item.width));
        x += item.width;
    }

    let mut x = width - padding.right;
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

    // The notch only ever pushes these further out. `SketchyBar` anchors both
    // at the bar's own midpoint (`bar_center_left_first_item_x` in `bar.c`),
    // which would overlap any centre item; rsbar hangs them off the centre
    // group's edges instead, and taking whichever is further from the middle
    // keeps that while still clearing the notch.
    let mut x = centre_start.min(f64::midpoint(width, -notch_width));
    for item in in_bucket(Position::CenterLeft)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        x -= item.width;
        placed.push((item.id, x, item.width));
    }

    let mut x = centre_end.max(f64::midpoint(width, notch_width));
    for item in in_bucket(Position::CenterRight) {
        placed.push((item.id, x, item.width));
        x += item.width;
    }

    placed
}

/// Gathers the drawn items and hands them to [`arrange`].
///
/// `ordinal` is this panel's 1-based position among the active displays —
/// what an item's or the bar's own `display` property is checked against, so
/// an item restricted to another display is simply left out of this panel's
/// list, the same way an undrawn one is.
fn place(
    items: &ItemQuery,
    cache: &Cache,
    captures: &Captures,
    size: CGSize,
    padding: BarPadding,
    notch_width: f64,
    ordinal: u32,
) -> Vec<(Entity, CGRect)> {
    // Sorted, because a query yields archetype order, which is not the order
    // a config added things in and can change when a component is added.
    let mut ordered: Vec<_> = items.iter().collect();
    ordered.sort_unstable_by_key(|row| *row.order);

    let measured: Vec<Placed<Entity>> = ordered
        .into_iter()
        // A bracket takes no space of its own — it is drawn across the items
        // it names, so laying it out alongside them would push them apart by
        // its own width. An item living inside a popup takes no space here
        // either — it is laid out against the popup's own surface, not the
        // bar's.
        .filter(|row| {
            row.drawing.0
                && row.members.is_none()
                && row.popup_of.is_none()
                && row.display.0.matches(ordinal)
        })
        .map(|row| Placed {
            id: row.entity,
            position: row.placement.0.clone(),
            // A fixed width overrides everything else a content measures to,
            // alias included — that is the whole point of a spacer.
            width: row.width.0.unwrap_or_else(|| {
                alias_width(captures, row.entity, row.padding).unwrap_or_else(|| {
                    width(
                        cache,
                        row.entity,
                        row.icon,
                        row.label,
                        row.padding,
                        row.graph,
                        row.slider,
                    )
                })
            }),
        })
        .collect();

    let mut placed: Vec<(Entity, CGRect)> = arrange(&measured, size.width, padding, notch_width)
        .into_iter()
        .map(|(entity, x, w)| {
            (
                entity,
                CGRect::new(CGPoint::new(x, 0.0), CGSize::new(w, size.height)),
            )
        })
        .collect();

    // Brackets go first, so their background lands under the items they span.
    let mut brackets = brackets_over(items, &placed, size, ordinal);
    brackets.append(&mut placed);
    brackets
}

/// Each bracket's frame: the span of the members it names, or nothing when
/// none of them is on screen.
///
/// Membership is by name rather than entity because a config names its members
/// before they necessarily exist, and a reload replaces the items underneath a
/// bracket that outlives them.
fn brackets_over(
    items: &ItemQuery,
    placed: &[(Entity, CGRect)],
    size: CGSize,
    ordinal: u32,
) -> Vec<(Entity, CGRect)> {
    // Indexed once rather than scanned per bracket. Walking every placed item
    // for every bracket, and every name in its member list for each of those,
    // is the one quadratic corner of layout -- and a real config has a dozen
    // brackets over a hundred and forty items.
    let mut frames: HashMap<&rsbar_protocol::ItemName, CGRect> =
        HashMap::with_capacity(placed.len());
    for (entity, frame) in placed {
        if let Ok(row) = items.get(*entity) {
            frames.insert(&row.name.0, *frame);
        }
    }

    let mut out = Vec::new();
    for bracket in items.iter() {
        let Some(members) = bracket.members else {
            continue;
        };
        if !bracket.drawing.0 || bracket.popup_of.is_some() || !bracket.display.0.matches(ordinal) {
            continue;
        }
        let mut span: Option<(f64, f64)> = None;
        for member in &members.0 {
            let Some(frame) = frames.get(member) else {
                continue;
            };
            let (left, right) = (frame.origin.x, frame.origin.x + frame.size.width);
            span = Some(match span {
                Some((l, r)) => (l.min(left), r.max(right)),
                None => (left, right),
            });
        }
        if let Some((left, right)) = span {
            out.push((
                bracket.entity,
                CGRect::new(
                    CGPoint::new(left, 0.0),
                    CGSize::new(right - left, size.height),
                ),
            ));
        }
    }
    out
}

/// Where each item ended up, per panel, from the last repaint.
///
/// Retained rather than recomputed: a click has to be answered against what is
/// actually on screen, and re-running layout at click time could disagree with
/// it — an item whose script has since changed its width would move under the
/// cursor between the press and the lookup.
#[derive(Resource, Default)]
pub struct Placements(Vec<PanelPlacements>);

/// Where one surface's items ended up: a bar panel's, or — reusing the exact
/// same shape — a single popup's. [`damage`] only ever reads `frame` and
/// `items`, so a popup's retained state is one of these too, keyed by its
/// host entity rather than by display; see [`crate::popup`].
pub(crate) struct PanelPlacements {
    pub(crate) display: u32,
    /// The panel's frame in global screen coordinates, so a click reported
    /// against the desktop can be brought into the panel's own space.
    pub(crate) frame: CGRect,
    pub(crate) items: Vec<(Entity, CGRect)>,
}

impl PanelPlacements {
    pub(crate) fn new(display: u32, frame: CGRect, items: Vec<(Entity, CGRect)>) -> Self {
        Self {
            display,
            frame,
            items,
        }
    }
}

impl Placements {
    /// Builds a set directly, for tests.
    #[cfg(test)]
    fn from_parts(display: u32, frame: CGRect, items: Vec<(Entity, CGRect)>) -> Self {
        Self(vec![PanelPlacements::new(display, frame, items)])
    }

    /// The item at a point given in global screen coordinates.
    ///
    /// Returns the panel's display too, so a handler knows which bar was hit
    /// even when the click landed on empty space.
    #[must_use]
    /// Each surface's own placements, for anything that has to talk to the
    /// window they were drawn on.
    pub(crate) fn panels(&self) -> &[PanelPlacements] {
        &self.0
    }

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

    /// Where one item last landed, in global screen coordinates, and which
    /// display's panel it is on.
    ///
    /// What [`crate::popup`] anchors a popup against: a popup hangs off its
    /// host item's own on-screen frame, which only the bar's own layout pass
    /// knows.
    #[must_use]
    pub fn item_frame(&self, entity: Entity) -> Option<(CGRect, u32)> {
        for panel in &self.0 {
            if let Some((_, local)) = panel.items.iter().find(|(id, _)| *id == entity) {
                let global = CGRect::new(
                    CGPoint::new(
                        panel.frame.origin.x + local.origin.x,
                        panel.frame.origin.y + local.origin.y,
                    ),
                    local.size,
                );
                return Some((global, panel.display));
            }
        }
        None
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

/// The rects of one surface that may look different this pass.
///
/// `None` means the whole surface: either something wide enough to move
/// everything changed, or this surface was not on screen last time and there
/// is no before to compare with.
///
/// Takes `before` directly rather than a display to look it up by, so the
/// same function serves both the bar — which looks its previous panel up by
/// display — and a popup, which has exactly one surface and no display to key
/// it by; see [`crate::popup`].
pub(crate) fn damage(
    everything: bool,
    changed: &HashSet<Entity>,
    before: Option<&PanelPlacements>,
    frame: CGRect,
    placed: &[(Entity, CGRect)],
) -> Option<Vec<CGRect>> {
    if everything {
        return None;
    }
    let before = before?;
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
                if changed.contains(entity) {
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
            Some(_) if changed.contains(entity) => rects.push(*now),
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

pub(crate) fn intersects(a: CGRect, b: CGRect) -> bool {
    a.origin.x < b.origin.x + b.size.width
        && b.origin.x < a.origin.x + a.size.width
        && a.origin.y < b.origin.y + b.size.height
        && b.origin.y < a.origin.y + a.size.height
}

/// Draws one item's background, and then its content — an alias's mirrored
/// image, a bracket's nothing, or its own icon and label.
///
/// Returns whether it found shaped text to draw against, so the caller can
/// count what actually got painted.
/// Draws whichever of a graph or a slider sits between the icon and the label.
fn draw_sandwich(ctx: &objc2_core_graphics::CGContext, rect: CGRect, row: &DrawnItem<'_, '_>) {
    if let Some(graph) = row.graph {
        draw_graph(ctx, rect, graph);
    } else if let Some(slider) = row.slider {
        draw_slider(ctx, rect, slider);
    }
}

/// Draws one half of an item's text, shifted by its own `y_offset`.
fn draw_run(
    ctx: &objc2_core_graphics::CGContext,
    shaped: &crate::shaping::Shaped,
    run: &crate::components::Run,
    which: Half,
    at: CGPoint,
    height: f64,
) -> f64 {
    let w = match which {
        Half::Icon => shaped.icon_metrics().width,
        Half::Label => shaped.label_metrics().width,
    };
    let box_ = CGRect::new(
        CGPoint::new(at.x, at.y + run.y_offset),
        CGSize::new(w, height),
    );
    match which {
        Half::Icon => shaped.draw_icon(ctx, box_, run.drawn_color()),
        Half::Label => shaped.draw_label(ctx, box_, run.drawn_color()),
    }
    w
}

/// Which half of an item [`draw_run`] is drawing.
#[derive(Clone, Copy)]
enum Half {
    Icon,
    Label,
}

pub(crate) fn draw_item(
    ctx: &objc2_core_graphics::CGContext,
    cache: &Cache,
    captures: &Captures,
    items: &ItemQuery,
    entity: Entity,
    frame: CGRect,
) -> bool {
    let Ok(row) = items.get(entity) else {
        return false;
    };
    let (icon, label, background, padding, offset, members) = (
        row.icon,
        row.label,
        row.background,
        row.padding,
        row.offset,
        row.members,
    );
    let Some(shaped) = cache.get(entity) else {
        return false;
    };

    let surface = background_rect(frame, background);
    if background.drawing && !background.color.is_invisible() {
        fill_rounded_rect(ctx, surface, background.corner_radius, background.color);
    }
    if background.drawing
        && background.border_width > 0.0
        && !background.border_color.is_invisible()
    {
        stroke_rounded_rect(
            ctx,
            surface,
            background.corner_radius,
            background.border_color,
            background.border_width,
        );
    }

    // A bracket is its background and nothing else — it has no text of its
    // own, and drawing its members' is their job.
    if members.is_some() {
        return true;
    }

    // An alias draws what it mirrors, and nothing else.
    if let Some(captured) = captures.get(entity) {
        // Centred on the ink, not on the captured window, and drawn offset by
        // the trim so the ink lands at the padded origin. The margin still
        // exists in the image, so the draw is clipped to the inked size to
        // keep it off the neighbour. The left inset is [`alias_box`]'s, the
        // same one `alias_width` sized the frame with, so a padding too
        // negative to fit is clamped here exactly as it was there — the ink
        // never starts outside the frame it was laid out in.
        let (left, _) = alias_box(padding, captured.trim.size.width);
        let vertical_slack = (frame.size.height - captured.trim.size.height) / 2.0;
        let ink = CGPoint::new(
            frame.origin.x + left,
            frame.origin.y + offset.0 + vertical_slack,
        );
        let whole = CGRect::new(
            CGPoint::new(
                ink.x - captured.trim.origin.x,
                ink.y - captured.trim.origin.y,
            ),
            captured.size,
        );
        crate::bar::draw_image_clipped(
            ctx,
            whole,
            CGRect::new(ink, captured.trim.size),
            &captured.image,
        );
        return true;
    }

    let mut x = frame.origin.x + padding.left;
    let y = frame.origin.y + offset.0;
    let icon_present = !icon.0.is_empty();
    let sandwich_w = sandwich_width(row.graph, row.slider);
    let sandwich_present = sandwich_w > 0.0;
    let label_present = !label.0.is_empty();
    if icon_present {
        x += icon.0.padding_left;
        let w = draw_run(
            ctx,
            shaped,
            &icon.0,
            Half::Icon,
            CGPoint::new(x, y),
            frame.size.height,
        );
        x += w + icon.0.padding_right;
        if sandwich_present {
            x += padding.between;
        }
    }
    if sandwich_present {
        let box_ = CGRect::new(
            CGPoint::new(x, y),
            CGSize::new(sandwich_w, frame.size.height),
        );
        draw_sandwich(ctx, box_, &row);
        x += sandwich_w;
    }
    if (icon_present || sandwich_present) && label_present {
        x += padding.between;
    }
    if label_present {
        x += label.0.padding_left;
        draw_run(
            ctx,
            shaped,
            &label.0,
            Half::Label,
            CGPoint::new(x, y),
            frame.size.height,
        );
    }
    true
}

/// Strokes (and, if [`Graph::fill`], fills) `graph`'s samples across `rect`,
/// oldest to newest, left to right — `graph_draw` in `SketchyBar`'s own
/// `graph.c`. Each sample is a 0.0-1.0 fraction of `rect`'s own height, its
/// own convention, with `1.0` at the top.
#[allow(
    clippy::cast_precision_loss,
    reason = "a graph is at most a few hundred samples wide"
)]
fn draw_graph(ctx: &objc2_core_graphics::CGContext, rect: CGRect, graph: &Graph) {
    use objc2_core_graphics::CGContext;

    let n = graph.samples.len();
    if n < 2 {
        return;
    }
    let step = rect.size.width / (n - 1) as f64;
    let point = |i: usize| {
        let value = f64::from(graph.samples[i]).clamp(0.0, 1.0);
        CGPoint::new(
            rect.origin.x + step * i as f64,
            rect.origin.y + rect.size.height * (1.0 - value),
        )
    };
    let trace = || {
        CGContext::begin_path(Some(ctx));
        let first = point(0);
        CGContext::move_to_point(Some(ctx), first.x, first.y);
        for i in 1..n {
            let p = point(i);
            CGContext::add_line_to_point(Some(ctx), p.x, p.y);
        }
    };

    CGContext::save_g_state(Some(ctx));
    CGContext::set_line_width(Some(ctx), graph.line_width);
    CGContext::set_rgb_stroke_color(
        Some(ctx),
        graph.line_color.red(),
        graph.line_color.green(),
        graph.line_color.blue(),
        graph.line_color.alpha(),
    );
    trace();
    CGContext::stroke_path(Some(ctx));

    if graph.fill {
        CGContext::set_rgb_fill_color(
            Some(ctx),
            graph.fill_color.red(),
            graph.fill_color.green(),
            graph.fill_color.blue(),
            graph.fill_color.alpha(),
        );
        trace();
        let base = rect.origin.y + rect.size.height;
        let last = point(n - 1);
        CGContext::add_line_to_point(Some(ctx), last.x, base);
        CGContext::add_line_to_point(Some(ctx), point(0).x, base);
        CGContext::close_path(Some(ctx));
        CGContext::fill_path(Some(ctx));
    }
    CGContext::restore_g_state(Some(ctx));
}

/// Draws `slider`'s track, the portion of it filled to
/// [`Slider::percentage`], and its knob — `slider_calculate_bounds`/
/// `slider_draw` in `SketchyBar`'s own `slider.c`.
fn draw_slider(ctx: &objc2_core_graphics::CGContext, rect: CGRect, slider: &Slider) {
    fill_rounded_rect(ctx, rect, 0.0, slider.track_color);

    let filled_w = rect.size.width * f64::from(slider.percentage) / 100.0;
    if filled_w > 0.0 {
        let filled = CGRect::new(rect.origin, CGSize::new(filled_w, rect.size.height));
        fill_rounded_rect(ctx, filled, 0.0, slider.fill_color);
    }

    let knob = crate::text::Text::new(
        slider.knob.string.clone(),
        crate::text::Font::resolve(&slider.knob.font),
    );
    let knob_w = knob.metrics().width;
    let raw_offset = filled_w - knob_w / 2.0;
    let knob_offset = raw_offset.clamp(0.0, (rect.size.width - (knob_w + 1.0)).max(0.0));
    let box_ = CGRect::new(
        CGPoint::new(rect.origin.x + knob_offset, rect.origin.y),
        CGSize::new(knob_w, rect.size.height),
    );
    knob.draw(ctx, box_, slider.knob.color);
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
        let padding = BarPadding {
            left: settings.padding_left,
            right: settings.padding_right,
        };
        // Layout depends on the panel's width and which display it is, so
        // both are per-panel rather than computed once for the whole bar.
        // Only the built-in display has a notch to leave room for.
        let notch = if skylight::is_builtin(panel.display.id) {
            settings.notch_width
        } else {
            0.0
        };
        let placed = place(items, cache, captures, size, padding, notch, panel.ordinal);
        let before = pass.previous.iter().find(|p| p.display == panel.display.id);
        let torn = damage(pass.everything, pass.changed, before, panel.frame, &placed);

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
                    if draw_item(ctx, cache, captures, items, entity, frame) {
                        drawn += 1;
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
///
/// `Without<PopupOf>`, unlike [`DirtyItems`]: an item living inside a popup
/// never lands in the bar's own `placed` list, so nothing it does can ever
/// damage a bar rect — counting its changes here would only wake this system
/// up to compute an empty diff every time a popup's content ticked.
type AnythingVisibleChanged<'w, 's> = Query<
    'w,
    's,
    (),
    (
        Or<(
            Changed<Icon>,
            Changed<Label>,
            Changed<Background>,
            Changed<Padding>,
            Changed<Offset>,
            Changed<Placement>,
            Changed<Order>,
            Changed<Drawing>,
            Changed<Width>,
            Changed<ItemDisplay>,
            Changed<Graph>,
            Changed<Slider>,
            // The digest of what an alias mirrors. Without this the capture
            // refreshes and the component changes, but nothing asks for a
            // repaint — a mirrored clock sits at the minute it was first drawn.
            Changed<AliasContent>,
        )>,
        Without<PopupOf>,
    ),
>;

/// The items that changed since the last repaint, as opposed to whether any
/// did. Same components as [`AnythingVisibleChanged`], because a change that
/// forces a repaint and a change that damages a rect are the same change.
///
/// Deliberately not filtered by [`PopupOf`] the way that query is: this feeds
/// [`damage`]'s per-entity lookup, and [`crate::popup`] runs the very same
/// `damage` against its own placements, so a popup item's change has to
/// survive into this set for its popup to see it.
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
        Changed<Order>,
        Changed<Drawing>,
        Changed<Width>,
        Changed<ItemDisplay>,
        Changed<Graph>,
        Changed<Slider>,
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
    use super::{BarPadding, Placed, arrange};
    use rsbar_protocol::Position;

    fn item(id: u8, position: Position, width: f64) -> Placed<u8> {
        Placed {
            id,
            position,
            width,
        }
    }

    fn no_padding() -> BarPadding {
        BarPadding::default()
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
            no_padding(),
            0.0,
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
            no_padding(),
            0.0,
        );
        // Last added sits hard against the right edge; the first sits left of it.
        assert!((x_of(&placed, 2) - 180.0).abs() < 1e-9);
        assert!((x_of(&placed, 1) - 150.0).abs() < 1e-9);
    }

    #[test]
    fn right_stays_pinned_when_content_grows() {
        let narrow = arrange(&[item(1, Position::Right, 20.0)], 200.0, no_padding(), 0.0);
        let wide = arrange(&[item(1, Position::Right, 60.0)], 200.0, no_padding(), 0.0);
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
            no_padding(),
            0.0,
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
            no_padding(),
            0.0,
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
            no_padding(),
            0.0,
        );
        assert!((x_of(&placed, 1) - 0.0).abs() < 1e-9);
        assert!((x_of(&placed, 2) - 75.0).abs() < 1e-9);
        assert!((x_of(&placed, 3) - 175.0).abs() < 1e-9);
    }

    #[test]
    fn an_empty_bar_places_nothing() {
        assert!(arrange::<u8>(&[], 200.0, no_padding(), 0.0).is_empty());
    }

    #[test]
    fn a_notch_pushes_the_centre_adjacent_buckets_apart() {
        // Only the two centre-adjacent buckets move -- `bar_center_first_item_x`
        // in SketchyBar's own `bar.c` has no notch term, so a centred item
        // stays centred and would sit under the notch either way.
        let items = [
            item(1, Position::CenterLeft, 20.0),
            item(2, Position::CenterRight, 20.0),
        ];

        let without = arrange(&items, 200.0, no_padding(), 0.0);
        assert!((without[0].1 - 80.0).abs() < 1e-9, "left of the middle");
        assert!((without[1].1 - 100.0).abs() < 1e-9, "right of the middle");

        let with = arrange(&items, 200.0, no_padding(), 60.0);
        assert!(
            (with[0].1 - 50.0).abs() < 1e-9,
            "pushed a further half-notch left"
        );
        assert!((with[1].1 - 130.0).abs() < 1e-9, "and a half-notch right");
    }

    #[test]
    fn a_notch_never_pulls_a_bucket_inwards() {
        // The centre-adjacent buckets hang off the centre group's edges here,
        // which is already outside the midpoint. A notch narrower than that
        // group must not drag them back in over it.
        let items = [
            item(1, Position::CenterLeft, 20.0),
            item(2, Position::Center, 100.0),
        ];
        let placed = arrange(&items, 200.0, no_padding(), 10.0);
        assert!(
            (placed[1].1 - 30.0).abs() < 1e-9,
            "still clear of the centre group, not at the midpoint"
        );
    }

    #[test]
    fn bar_padding_insets_left_and_right_from_the_bar_edges() {
        let padding = BarPadding {
            left: 10.0,
            right: 20.0,
        };
        let placed = arrange(
            &[
                item(1, Position::Left, 30.0),
                item(2, Position::Right, 25.0),
            ],
            200.0,
            padding,
            0.0,
        );
        assert!((x_of(&placed, 1) - 10.0).abs() < 1e-9, "left starts inset");
        assert!(
            (x_of(&placed, 2) - (200.0 - 20.0 - 25.0)).abs() < 1e-9,
            "right ends inset"
        );
    }

    #[test]
    fn bar_padding_does_not_move_the_centre_group() {
        let with_padding = arrange(
            &[item(1, Position::Center, 40.0)],
            200.0,
            BarPadding {
                left: 50.0,
                right: 50.0,
            },
            0.0,
        );
        let without = arrange(&[item(1, Position::Center, 40.0)], 200.0, no_padding(), 0.0);
        assert!(
            (x_of(&with_padding, 1) - x_of(&without, 1)).abs() < 1e-9,
            "the centre group is centred on the whole bar, not the padded area"
        );
    }
}

#[cfg(test)]
mod width_tests {
    use super::width;
    use crate::components::{Graph, Icon, Label, Padding, Run};
    use crate::shaping::Cache;
    use bevy_ecs::entity::Entity;
    use rsbar_protocol::style::Color;

    fn entity() -> Entity {
        Entity::from_raw_u32(1).expect("a valid entity index")
    }

    fn padding() -> Padding {
        Padding {
            left: 0.0,
            right: 0.0,
            between: 0.0,
        }
    }

    #[test]
    fn shifting_a_run_does_not_change_what_it_measures() {
        // `y_offset` moves a glyph up or down within the item it is already
        // in. If it fed into `width()` the item would resize as well, and
        // every neighbour would shift with it.
        let cache = Cache::default();
        let id = entity();
        let mut icon = Icon(crate::components::Run::new("Menlo:Bold:15", Color::WHITE));
        icon.0.string = "x".into();
        let label = Label(crate::components::Run::new(
            "Menlo:Regular:13",
            Color::WHITE,
        ));

        let flat = width(&cache, id, &icon, &label, &padding(), None, None);
        icon.0.y_offset = -5.0;
        let shifted = width(&cache, id, &icon, &label, &padding(), None, None);
        assert!((flat - shifted).abs() < 1e-9);
    }

    /// The draw path adds a run's own padding on top of its metrics; layout
    /// must add exactly the same amount, or the background it sizes stops
    /// wrapping the text it was sized for.
    #[test]
    fn a_runs_own_padding_widens_the_item_by_exactly_that_much() {
        let mut cache = Cache::default();
        let id = entity();
        let icon = Icon(Run::new("Menlo:Regular:13", Color::WHITE));
        let mut label = Label(Run::new("Menlo:Regular:13", Color::WHITE));
        label.0.string = "hi".into();
        cache.refresh(id, &icon.0, &label.0);
        let bare = width(&cache, id, &icon, &label, &padding(), None, None);

        let mut padded_label = label.clone();
        padded_label.0.padding_left = 4.0;
        padded_label.0.padding_right = 6.0;
        cache.refresh(id, &icon.0, &padded_label.0);
        let padded = width(&cache, id, &icon, &padded_label, &padding(), None, None);

        assert!(
            (padded - bare - 10.0).abs() < 1e-9,
            "label padding must add exactly to the item's width"
        );
    }

    /// [`Run::is_empty`] already takes a hidden or empty run out of layout —
    /// its own padding must go with it, or an icon-only item gets a phantom
    /// gap where a label with padding but no text would have sat.
    #[test]
    fn a_hidden_runs_own_padding_contributes_nothing() {
        let mut cache = Cache::default();
        let id = entity();
        let mut icon = Icon(Run::new("Menlo:Regular:13", Color::WHITE));
        icon.0.string = "A".into();
        let label = Label(Run::new("Menlo:Regular:13", Color::WHITE));
        cache.refresh(id, &icon.0, &label.0);
        let base = width(&cache, id, &icon, &label, &padding(), None, None);

        let mut padded_label = label.clone();
        padded_label.0.padding_left = 50.0;
        padded_label.0.padding_right = 50.0;
        cache.refresh(id, &icon.0, &padded_label.0);
        let same = width(&cache, id, &icon, &padded_label, &padding(), None, None);

        assert!(
            (same - base).abs() < 1e-9,
            "an empty label takes no space no matter its own padding"
        );
    }

    /// `SketchyBar`'s own item order — icon, then a graph or slider, then
    /// label — so the sandwich segment adds its own width plus one more
    /// `between` gap, on top of whatever the icon and label already claimed.
    #[test]
    fn a_graph_widens_the_item_between_icon_and_label() {
        let mut cache = Cache::default();
        let id = entity();
        let mut icon = Icon(Run::new("Menlo:Regular:13", Color::WHITE));
        icon.0.string = "A".into();
        let mut label = Label(Run::new("Menlo:Regular:13", Color::WHITE));
        label.0.string = "hi".into();
        cache.refresh(id, &icon.0, &label.0);
        let padding = Padding {
            left: 0.0,
            right: 0.0,
            between: 3.0,
        };

        let without_graph = width(&cache, id, &icon, &label, &padding, None, None);
        let graph = Graph::new(20);
        let with_graph = width(&cache, id, &icon, &label, &padding, Some(&graph), None);

        assert!(
            (with_graph - without_graph - 20.0 - 3.0).abs() < 1e-9,
            "the graph's own width plus one more `between` gap must be added"
        );
    }

    /// A graph with nothing either side of it to gap against takes no
    /// `between` space at all.
    #[test]
    fn a_graph_alone_takes_no_between_gap() {
        let mut cache = Cache::default();
        let id = entity();
        let icon = Icon(Run::new("Menlo:Regular:13", Color::WHITE));
        let label = Label(Run::new("Menlo:Regular:13", Color::WHITE));
        cache.refresh(id, &icon.0, &label.0);
        let padding = Padding {
            left: 0.0,
            right: 0.0,
            between: 3.0,
        };

        let graph = Graph::new(20);
        let w = width(&cache, id, &icon, &label, &padding, Some(&graph), None);

        assert!((w - 20.0).abs() < 1e-9);
    }
}

#[cfg(test)]
mod alias_box_tests {
    use super::{Padding, alias_box};

    fn padding(left: f64, right: f64) -> Padding {
        Padding {
            left,
            right,
            between: 0.0,
        }
    }

    /// Ordinary, non-negative padding must land exactly where it always has —
    /// this is the case every alias without a compensating negative padding
    /// hits, and it must not regress.
    #[test]
    fn ordinary_padding_insets_the_ink_by_its_own_amount() {
        let (left, width) = alias_box(&padding(5.0, 5.0), 69.0);
        assert!((left - 5.0).abs() < 1e-9);
        assert!((width - 79.0).abs() < 1e-9);
    }

    /// `Control Centre,FocusModes`' own config, measured live: -15/-5 against
    /// an 18pt-wide trim sums to -2, which would draw the ink 15pt to the left
    /// of a frame only 18pt wide — spilling onto whatever sits to its left.
    /// The frame must never shrink past the ink, and the ink must never start
    /// outside the frame.
    #[test]
    fn padding_too_negative_for_the_ink_is_clamped_to_it_exactly() {
        let (left, width) = alias_box(&padding(-15.0, -5.0), 18.0);
        assert!((left - 0.0).abs() < 1e-9, "no room to inset from at all");
        assert!(
            (width - 18.0).abs() < 1e-9,
            "the frame is exactly the ink, no smaller"
        );
    }

    /// A total that is negative but still fits inside the ink's own width
    /// is not clamped away outright — the frame shrinks, only never past
    /// the ink.
    #[test]
    fn a_small_negative_total_only_shrinks_the_frame_to_the_ink() {
        let (left, width) = alias_box(&padding(-2.0, -2.0), 30.0);
        assert!((left - 0.0).abs() < 1e-9);
        assert!((width - 30.0).abs() < 1e-9);
    }

    /// A very negative left offset, compensated by an equally generous right
    /// one, still must not push the ink left of the frame it was given —
    /// only the *sum* buys back slack, and the left inset itself is clamped
    /// to what that slack actually is.
    #[test]
    fn a_negative_left_offset_never_starts_before_the_frame() {
        let (left, width) = alias_box(&padding(-15.0, 50.0), 18.0);
        assert!(
            (left - 0.0).abs() < 1e-9,
            "the ink starts at the frame origin"
        );
        assert!(
            (width - 53.0).abs() < 1e-9,
            "the sum still buys the frame room"
        );
    }
}

#[cfg(test)]
mod place_tests {
    use super::{BarPadding, ItemQuery, place};
    use crate::alias::Captures;
    use crate::components::{DisplayTarget, ItemDisplay, Order, Width, bundle};
    use crate::shaping::Cache;
    use bevy_ecs::system::SystemState;
    use bevy_ecs::world::World;
    use objc2_core_foundation::CGSize;
    use rsbar_protocol::{ItemName, Position};
    use std::num::NonZeroU32;

    fn size() -> CGSize {
        CGSize::new(500.0, 32.0)
    }

    #[test]
    fn a_fixed_width_overrides_what_the_item_would_otherwise_measure_to() {
        let mut world = World::new();
        let entity = world
            .spawn(bundle(
                ItemName::new("spacer").unwrap(),
                Position::Left,
                Order(0),
            ))
            .id();
        world.entity_mut(entity).insert(Width(Some(5.0)));

        let mut state: SystemState<ItemQuery> = SystemState::new(&mut world);
        let query = state.get(&world).expect("query param is valid");
        let placed = place(
            &query,
            &Cache::default(),
            &Captures::default(),
            size(),
            BarPadding::default(),
            0.0,
            1,
        );

        assert_eq!(placed.len(), 1);
        assert!(
            (placed[0].1.size.width - 5.0).abs() < 1e-9,
            "the fixed width wins over the measured (empty) contents"
        );
    }

    #[test]
    fn an_item_restricted_to_another_display_is_left_off_this_panel() {
        let mut world = World::new();
        let entity = world
            .spawn(bundle(
                ItemName::new("only-two").unwrap(),
                Position::Left,
                Order(0),
            ))
            .id();
        world
            .entity_mut(entity)
            .insert(ItemDisplay(DisplayTarget::Index(
                NonZeroU32::new(2).unwrap(),
            )));

        let mut state: SystemState<ItemQuery> = SystemState::new(&mut world);
        let query = state.get(&world).expect("query param is valid");
        let on_one = place(
            &query,
            &Cache::default(),
            &Captures::default(),
            size(),
            BarPadding::default(),
            0.0,
            1,
        );
        let on_two = place(
            &query,
            &Cache::default(),
            &Captures::default(),
            size(),
            BarPadding::default(),
            0.0,
            2,
        );

        assert!(on_one.is_empty(), "not this panel's display");
        assert_eq!(on_two.len(), 1, "this one is");
    }

    #[test]
    fn an_item_living_inside_a_popup_takes_no_space_in_the_bar() {
        // The local stand-in for `position = "popup.<name>"` — see the module
        // doc on `crate::popup`. Whatever the item's own `Placement` says, a
        // popup item is not one of the bar's five buckets at all.
        let mut world = World::new();
        let host = world
            .spawn(bundle(
                ItemName::new("host").unwrap(),
                Position::Left,
                Order(0),
            ))
            .id();
        let child = world
            .spawn(bundle(
                ItemName::new("in-popup").unwrap(),
                Position::Left,
                Order(1),
            ))
            .id();
        world.entity_mut(child).insert(crate::popup::PopupOf(host));

        let mut state: SystemState<ItemQuery> = SystemState::new(&mut world);
        let query = state.get(&world).expect("query param is valid");
        let placed = place(
            &query,
            &Cache::default(),
            &Captures::default(),
            size(),
            BarPadding::default(),
            0.0,
            1,
        );

        assert_eq!(placed.len(), 1, "only the host is laid out by the bar");
        assert_eq!(placed[0].0, host);
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
    use super::{PanelPlacements, damage, intersects};
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

    fn was(items: Vec<(Entity, CGRect)>) -> PanelPlacements {
        PanelPlacements::new(DISPLAY, panel(), items)
    }

    #[test]
    fn a_pass_where_nothing_moved_damages_nothing() {
        let before = was(vec![(entity(1), rect(0.0, 100.0))]);
        let nothing = HashSet::new();
        let torn = damage(
            false,
            &nothing,
            Some(&before),
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
            false,
            &changed,
            Some(&before),
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
            false,
            &changed,
            Some(&before),
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
            false,
            &nothing,
            Some(&before),
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
            false,
            &nothing,
            Some(&before),
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
            false,
            &nothing,
            Some(&before),
            narrower,
            &[(entity(1), rect(0.0, 100.0))],
        );
        assert_eq!(torn, None, "every position in it is invalid");
    }

    #[test]
    fn a_panel_with_nothing_before_it_is_repainted_whole() {
        let nothing = HashSet::new();
        let torn = damage(
            false,
            &nothing,
            None,
            panel(),
            &[(entity(1), rect(0.0, 100.0))],
        );
        assert_eq!(torn, None, "there is no before to diff against");
    }

    #[test]
    fn a_bar_wide_change_is_repainted_whole() {
        let before = was(vec![(entity(1), rect(0.0, 100.0))]);
        let nothing = HashSet::new();
        assert_eq!(
            damage(
                true,
                &nothing,
                Some(&before),
                panel(),
                &[(entity(1), rect(0.0, 100.0))]
            ),
            None
        );
    }

    #[test]
    fn an_items_own_padding_growing_damages_both_its_old_and_new_rect() {
        // A run's own padding (icon.padding_left, label.padding_right, …)
        // widens the item itself via `width`, not a neighbour — this is the
        // "resized in place" branch of `damage`, exercised without the entity
        // needing to appear in `changed`: the geometry differing is what
        // matters, whatever caused it.
        let before = was(vec![(entity(1), rect(0.0, 100.0))]);
        let nothing = HashSet::new();
        let torn = damage(
            false,
            &nothing,
            Some(&before),
            panel(),
            &[(entity(1), rect(0.0, 108.0))],
        )
        .expect("a partial repaint");
        assert!(torn.contains(&rect(0.0, 100.0)), "erase the old width");
        assert!(torn.contains(&rect(0.0, 108.0)), "draw the new width");
    }

    #[test]
    fn setting_a_background_property_to_its_current_value_damages_nothing() {
        // `patched_background` (in `requests.rs`) refuses to write a patch
        // that matches the component already there, so `Changed<Background>`
        // never fires and the entity never lands in `changed`. None of
        // height, padding or border affect an item's frame, so the geometry
        // compare agrees: nothing to repaint.
        let before = was(vec![(entity(1), rect(0.0, 100.0))]);
        let nothing = HashSet::new();
        let torn = damage(
            false,
            &nothing,
            Some(&before),
            panel(),
            &[(entity(1), rect(0.0, 100.0))],
        );
        assert_eq!(torn, Some(Vec::new()), "no rect should be repainted");
    }

    #[test]
    fn only_overlapping_rects_intersect() {
        assert!(intersects(rect(0.0, 100.0), rect(50.0, 100.0)));
        assert!(!intersects(rect(0.0, 100.0), rect(100.0, 50.0)));
        assert!(!intersects(rect(0.0, 100.0), rect(200.0, 50.0)));
    }
}

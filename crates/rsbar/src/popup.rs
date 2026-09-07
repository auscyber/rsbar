//! A popup: a second window hanging off a bar item, with its own items in it.
//!
//! # Where this lives
//!
//! A popup is *not* folded into [`crate::bar::Panels`]. A panel is one per
//! display, rebuilt when displays change and reframed as a batch whenever a
//! bar-wide setting moves; a popup is one per host item, created the moment
//! that item's `popup.drawing` turns on and destroyed the moment it turns
//! off, sized by what is in it rather than by a display, and anchored to
//! wherever its host currently sits on screen. Folding the two together would
//! mean every bar-wide operation on `Panels` — `rebuild`, `set_blur`,
//! `set_clickable` — would need to start skipping popups, and every popup
//! operation would need to skip panels. Keeping them apart keeps both exactly
//! what they say, at the cost of one more resource for whoever wires the
//! schedule up ([`Popups`], and the damage-tracking state that goes with it,
//! [`PopupPlacements`]).
//!
//! # Its own `Placements`, not the bar's
//!
//! A popup is a distinct window server surface with its own coordinate space,
//! opened and closed independently of the bar and of any other popup. Mixing
//! its items into [`crate::layout::Placements`] would mean one damage pass
//! covering two surfaces that do not share a backing store — clearing a rect
//! in the bar's `Placements` would mean nothing on the popup's window, and
//! [`crate::layout::Placements::hit`] would have no way to say which surface,
//! bar or popup, a point actually belongs to without every caller learning to
//! filter. So this module keeps [`PopupPlacements`]: the same *shape* of
//! retained state as one bar panel — [`crate::layout::PanelPlacements`],
//! reused rather than duplicated — but one per open popup, diffed and drawn
//! with the exact same [`crate::layout::damage`] and [`crate::layout::draw_item`]
//! the bar uses. That reuse is also what makes the hard requirement here
//! checkable: opening a popup can only ever produce damage rects against its
//! own surface, because the bar's [`crate::layout::repaint`] never looks at
//! this module's state and this module never looks at the bar's.
//!
//! # The protocol variant
//!
//! `position = "popup.<name>"` parses into [`rsbar_protocol::Position::Popup`],
//! which `requests.rs::set_item`/`add_item` resolves through the item `Index`
//! and turns into [`PopupOf`] on the item — *instead of* writing that
//! `Position` into the item's own [`crate::components::Placement`], since an
//! item inside a popup is not in any of the bar's five buckets at all.
//!
//! # What is simplified for now
//!
//! No nested popups (an item inside a popup hosting a popup of its own), no
//! alias mirroring inside a popup, and popup content ignores an item's own
//! `display` restriction — it is drawn once, on whichever display its host
//! currently is. `SketchyBar` supports all three; none of the reference
//! config at `~/dendritic/sketchybar/` uses them.

#![allow(
    clippy::needless_pass_by_value,
    reason = "Bevy system parameters are taken by value by contract"
)]

use crate::alias::Captures;
use crate::bar::{Panels, Settings, fill_rounded_rect, stroke_rounded_rect};
use crate::layout::{ItemQuery, PanelPlacements, Placements, damage, draw_item, intersects, width};
use crate::shaping::Cache;
use bevy_ecs::prelude::*;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use rsbar_protocol::Edge;
use rsbar_protocol::style::Color;
use skylight::{Window, WindowTags, level};
use std::collections::{HashMap, HashSet};

pub use rsbar_protocol::{InvalidAlign, PopupAlign};

/// A row's height when a popup does not set one.
const DEFAULT_ROW_HEIGHT: f64 = 25.0;

/// One popup's own drawing properties -- a component on the item that hosts
/// it, not on the items inside it; those instead carry [`PopupOf`].
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct PopupConfig {
    pub drawing: bool,
    /// Rows run left to right instead of stacking top to bottom.
    pub horizontal: bool,
    pub align: PopupAlign,
    /// Above `kCGPopUpMenuWindowLevel`'s neighbourhood, or just above the
    /// system menu bar's own backstop level — the same choice `bar.topmost`
    /// makes for the bar itself, but independent of it: a popup wants to sit
    /// above whatever it hangs off, whether or not the bar underneath does.
    pub topmost: bool,
    /// Each row's height. Zero means [`DEFAULT_ROW_HEIGHT`].
    pub height: f64,
    /// Gap between the host item and the popup.
    pub y_offset: f64,
    pub background: crate::components::Background,
}

impl Default for PopupConfig {
    fn default() -> Self {
        Self {
            drawing: false,
            horizontal: false,
            align: PopupAlign::Left,
            topmost: true,
            height: 0.0,
            y_offset: 0.0,
            background: crate::components::Background {
                drawing: true,
                color: Color(0xe014_1820),
                corner_radius: 9.0,
                height: 0.0,
                padding_left: 0.0,
                padding_right: 0.0,
                border_color: Color::TRANSPARENT,
                border_width: 0.0,
            },
        }
    }
}

impl PopupConfig {
    #[must_use]
    pub fn row_height(&self) -> f64 {
        if self.height > 0.0 {
            self.height
        } else {
            DEFAULT_ROW_HEIGHT
        }
    }

    fn level(&self) -> std::ffi::c_int {
        if self.topmost {
            level::POPUP_MENU
        } else {
            level::BACKSTOP_MENU + 1
        }
    }
}

/// Marks an item as living inside a popup, naming the entity that hosts it —
/// the local stand-in for `position = "popup.<name>"` until the protocol
/// carries [`rsbar_protocol::Position::Popup`]; see the module doc.
///
/// Such an item still has every other component an ordinary item does
/// ([`crate::components::Icon`], `Label`, `Background`, `Order`, …) — this
/// only says which surface lays it out and draws it: the popup's, not the
/// bar's.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PopupOf(pub Entity);

/// A partial update to a [`PopupConfig`], the shape `requests.rs` would
/// deserialize `--set item popup.<key>=<value>` into once `ItemPatch` grows a
/// `popup: Option<PopupPatch>` field. `struct_patch`'s derive is not
/// available in this crate — it is a dependency of `rsbar-protocol`, not of
/// `rsbar` — so [`patched`] merges by hand rather than through that macro.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PopupPatch {
    pub drawing: Option<bool>,
    pub horizontal: Option<bool>,
    pub align: Option<PopupAlign>,
    pub topmost: Option<bool>,
    pub height: Option<f64>,
    pub y_offset: Option<f64>,
    pub background: Option<rsbar_protocol::BackgroundPatch>,
}

/// What the patch would make of this config, or `None` if it would make no
/// difference — the same "apply to a copy, compare, report only a real
/// change" shape `requests.rs::patched` uses for an item's own background,
/// and for the same reason: a script re-setting `popup.height` to the height
/// it already has must not repaint anything.
#[must_use]
pub fn patched(current: &PopupConfig, patch: &PopupPatch) -> Option<PopupConfig> {
    let mut next = *current;
    if let Some(v) = patch.drawing {
        next.drawing = v;
    }
    if let Some(v) = patch.horizontal {
        next.horizontal = v;
    }
    if let Some(v) = patch.align {
        next.align = v;
    }
    if let Some(v) = patch.topmost {
        next.topmost = v;
    }
    if let Some(v) = patch.height {
        next.height = v;
    }
    if let Some(v) = patch.y_offset {
        next.y_offset = v;
    }
    if let Some(bg) = &patch.background {
        next.background = apply_background_patch(next.background, bg);
    }
    (next != *current).then_some(next)
}

fn apply_background_patch(
    mut bg: crate::components::Background,
    patch: &rsbar_protocol::BackgroundPatch,
) -> crate::components::Background {
    if let Some(v) = patch.drawing {
        bg.drawing = v;
    }
    if let Some(v) = patch.color {
        bg.color = v;
    }
    if let Some(v) = patch.corner_radius {
        bg.corner_radius = v;
    }
    if let Some(v) = patch.height {
        bg.height = v;
    }
    if let Some(v) = patch.padding_left {
        bg.padding_left = v;
    }
    if let Some(v) = patch.padding_right {
        bg.padding_right = v;
    }
    if let Some(v) = patch.border_color {
        bg.border_color = v;
    }
    if let Some(v) = patch.border_width {
        bg.border_width = v;
    }
    bg
}

/// One item reduced to what arranging a popup's rows needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PopupPlaced<T> {
    pub id: T,
    pub width: f64,
}

/// Lays out a popup's own items: stacked top to bottom by default, each row
/// as wide as the widest item and as tall as `row_height`; left to right when
/// `horizontal`, each item its own width and every row `row_height` tall.
///
/// Pure and independent of [`crate::layout::arrange`] on purpose — a popup
/// has no left/center/right buckets of its own. `SketchyBar`'s
/// `popup_calculate_bounds` (`popup.c`) just walks `popup->items[]` in order
/// either way; the bucket machinery in `Placement` is what a config still
/// writes on each item, but inside a popup it is not read.
#[must_use]
pub fn arrange_popup<T: Copy>(
    items: &[PopupPlaced<T>],
    horizontal: bool,
    row_height: f64,
) -> (Vec<(T, CGRect)>, CGSize) {
    if items.is_empty() {
        return (Vec::new(), CGSize::new(0.0, 0.0));
    }
    if horizontal {
        let mut x = 0.0;
        let mut placed = Vec::with_capacity(items.len());
        for item in items {
            placed.push((
                item.id,
                CGRect::new(CGPoint::new(x, 0.0), CGSize::new(item.width, row_height)),
            ));
            x += item.width;
        }
        (placed, CGSize::new(x, row_height))
    } else {
        let width = items.iter().fold(0.0_f64, |w, i| w.max(i.width));
        let mut y = 0.0;
        let mut placed = Vec::with_capacity(items.len());
        for item in items {
            placed.push((
                item.id,
                CGRect::new(CGPoint::new(0.0, y), CGSize::new(width, row_height)),
            ));
            y += row_height;
        }
        (placed, CGSize::new(width, y))
    }
}

/// Where a popup of `size` sits, given its host item's own on-screen frame in
/// global screen coordinates.
///
/// Mirrors `popup_calculate_popup_anchor_for_bar_item` in `SketchyBar`'s
/// `popup.c`: `align` places the popup's own left/center/right edge against
/// the host's, and `edge` — the bar's own `Edge`, top or bottom — decides
/// whether the popup hangs below the host or sits above it, the same way a
/// bottom-edge bar flips which side its own items' popups open on.
#[must_use]
pub fn popup_anchor(
    host: CGRect,
    size: CGSize,
    align: PopupAlign,
    edge: Edge,
    y_offset: f64,
    within: Option<f64>,
) -> CGPoint {
    let x = match align {
        PopupAlign::Left => host.origin.x,
        PopupAlign::Center => host.origin.x + (host.size.width - size.width) / 2.0,
        PopupAlign::Right => host.origin.x + host.size.width - size.width,
    };
    // Kept on the display it belongs to. A popup hanging off an item at the
    // right-hand end -- which is where most of them are -- otherwise runs off
    // the edge and loses whatever does not fit, since a window server window
    // is clipped, not moved.
    let x = match within {
        Some(width) if size.width <= width => x.min(width - size.width).max(0.0),
        _ => x,
    };
    let y = match edge {
        Edge::Top => host.origin.y + host.size.height + y_offset,
        Edge::Bottom => host.origin.y - size.height - y_offset,
    };
    CGPoint::new(x, y)
}

/// One window per open popup, keyed by the entity that hosts it.
///
/// `NonSend`, like [`crate::bar::Panels`]: a `Window` wraps a window server
/// handle, which is neither `Send` nor `Sync`.
#[derive(Default)]
pub struct Popups {
    open: HashMap<Entity, Window>,
}

impl Popups {
    /// The window for one popup, creating it — at `frame`, with `scale` and
    /// `config`'s level and background tags — the first time this host has
    /// anything to show.
    ///
    /// # Errors
    ///
    /// Returns the window server's error if a window cannot be created or
    /// configured.
    fn ensure(
        &mut self,
        host: Entity,
        frame: CGRect,
        scale: f64,
        config: &PopupConfig,
    ) -> skylight::Result<&Window> {
        if let std::collections::hash_map::Entry::Vacant(entry) = self.open.entry(host) {
            let window = Window::new(frame)?;
            window.set_scale(scale)?;
            window.set_opaque(false)?;
            window.set_alpha(1.0)?;
            window.set_level(config.level())?;
            window.set_tags(WindowTags::BAR | WindowTags::OPAQUE_FOR_EVENTS)?;
            window.order_above(None)?;
            entry.insert(window);
        }
        Ok(self.open.get(&host).expect("just ensured present"))
    }

    /// Closes and releases the window for one popup, if it has one.
    fn close(&mut self, host: Entity) {
        self.open.remove(&host);
    }

    /// Drops every window whose host is no longer live — the host item was
    /// despawned, or lost its [`PopupConfig`] outright.
    fn retain_live(&mut self, live: &HashSet<Entity>) {
        self.open.retain(|host, _| live.contains(host));
    }
}

/// Where each open popup's rows last landed, for damage tracking — one
/// [`PanelPlacements`] per host, the same shape [`crate::bar::Panels`] keeps
/// per display, reused rather than duplicated (see the module doc).
#[derive(Resource, Default)]
pub struct PopupPlacements(HashMap<Entity, PanelPlacements>);

impl PopupPlacements {
    /// The item, if any, at a point given in global screen coordinates —
    /// checked before [`Placements::hit`] by whoever routes a click, since a
    /// popup floats above the bar and above every other popup that is not
    /// its own ancestor.
    ///
    /// Returns the popup's host alongside the hit, so a click on empty popup
    /// space (as opposed to one of its items) can still be told which popup
    /// it landed in.
    #[must_use]
    pub fn hit(&self, point: CGPoint) -> Option<(Entity, Option<Entity>)> {
        for (host, placement) in &self.0 {
            if !contains(placement.frame, point) {
                continue;
            }
            let local = CGPoint::new(
                point.x - placement.frame.origin.x,
                point.y - placement.frame.origin.y,
            );
            let item = placement
                .items
                .iter()
                .find(|(_, frame)| contains(*frame, local))
                .map(|(entity, _)| *entity);
            return Some((*host, item));
        }
        None
    }
}

fn contains(rect: CGRect, point: CGPoint) -> bool {
    point.x >= rect.origin.x
        && point.y >= rect.origin.y
        && point.x < rect.origin.x + rect.size.width
        && point.y < rect.origin.y + rect.size.height
}

/// Lays out and repaints every open popup.
///
/// Reads the bar's own [`Placements`] to find each host item's current
/// on-screen frame — a popup has no position of its own, only its host's —
/// so this must run after [`crate::layout::repaint`] has updated it for this
/// tick. Gate this on `needs_repaint(..) || needs_repaint_popups(..)`: a
/// popup's anchor moves whenever its host does, even when nothing about the
/// popup itself changed.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "one system doing one whole surface's layout-and-draw pass, matching crate::layout::repaint's own shape"
)]
pub fn repaint_popups(
    items: ItemQuery,
    dirty: crate::layout::DirtyItems,
    hosts: Query<(Entity, &PopupConfig)>,
    cache: NonSend<Cache>,
    captures: NonSend<Captures>,
    panels: NonSend<Panels>,
    settings: Res<Settings>,
    bar_placements: Res<Placements>,
    mut popups: NonSendMut<Popups>,
    mut placements: ResMut<PopupPlacements>,
) {
    let changed: HashSet<Entity> = dirty.iter().collect();
    let live: HashSet<Entity> = hosts
        .iter()
        .filter(|(_, config)| config.drawing)
        .map(|(host, _)| host)
        .collect();

    for (host, config) in &hosts {
        if !config.drawing {
            if placements.0.remove(&host).is_some() {
                popups.close(host);
            }
            continue;
        }

        let Some((host_frame, display)) = bar_placements.item_frame(host) else {
            // The host is not on screen this tick — hidden, undrawn, or
            // restricted off every panel that exists — so neither is its
            // popup.
            if placements.0.remove(&host).is_some() {
                popups.close(host);
            }
            continue;
        };

        let mut children: Vec<_> = items
            .iter()
            .filter(|row| row.popup_of.is_some_and(|of| of.0 == host) && row.drawing.0)
            .collect();
        children.sort_unstable_by_key(|row| *row.order);

        let row_height = config.row_height();
        let measured: Vec<PopupPlaced<Entity>> = children
            .iter()
            .map(|row| PopupPlaced {
                id: row.entity,
                width: row.width.0.unwrap_or_else(|| {
                    width(
                        &cache,
                        row.entity,
                        row.icon,
                        row.label,
                        row.padding,
                        None,
                        None,
                    )
                }),
            })
            .collect();

        let (local, size) = arrange_popup(&measured, config.horizontal, row_height);
        if local.is_empty() {
            if placements.0.remove(&host).is_some() {
                popups.close(host);
            }
            continue;
        }

        let anchor = popup_anchor(
            host_frame,
            size,
            config.align,
            settings.edge,
            config.y_offset,
            panels.width_for(display),
        );
        let frame = CGRect::new(anchor, size);

        let before = placements.0.get(&host);
        let just_opened = before.is_none();
        let torn = damage(false, &changed, before, frame, &local);
        let unchanged = torn.as_ref().is_some_and(Vec::is_empty);

        let scale = panels.scale_for(display);
        let window = match popups.ensure(host, frame, scale, config) {
            Ok(window) => window,
            Err(err) => {
                tracing::error!(?err, "could not open a popup window");
                continue;
            }
        };
        if !just_opened
            && before.is_some_and(|p| p.frame != frame)
            && let Err(err) = window.set_frame(frame)
        {
            tracing::error!(?err, "could not reframe a popup window");
        }

        if !unchanged {
            skylight::draw_damaged(window.id(), size, torn.as_deref(), |ctx| {
                fill_rounded_rect(
                    ctx,
                    CGRect::new(CGPoint::new(0.0, 0.0), size),
                    config.background.corner_radius,
                    config.background.color,
                );
                if config.background.border_width > 0.0
                    && !config.background.border_color.is_invisible()
                {
                    stroke_rounded_rect(
                        ctx,
                        CGRect::new(CGPoint::new(0.0, 0.0), size),
                        config.background.corner_radius,
                        config.background.border_color,
                        config.background.border_width,
                    );
                }
                for &(entity, rect) in &local {
                    if let Some(torn) = &torn
                        && !torn.iter().any(|damaged| intersects(*damaged, rect))
                    {
                        continue;
                    }
                    draw_item(ctx, &cache, &captures, &items, entity, rect);
                }
            });
        }

        placements
            .0
            .insert(host, PanelPlacements::new(display, frame, local));
    }

    placements.0.retain(|host, _| live.contains(host));
    popups.retain_live(&live);
}

/// A popup child whose own draw-affecting components changed — the
/// popup-side counterpart of [`crate::layout::AnythingVisibleChanged`], which
/// excludes these with `Without<PopupOf>` so this is where they are counted
/// instead.
type PopupContentChanged<'w, 's> = Query<
    'w,
    's,
    (),
    (
        Or<(
            Changed<crate::components::Icon>,
            Changed<crate::components::Label>,
            Changed<crate::components::Background>,
            Changed<crate::components::Padding>,
            Changed<crate::components::Offset>,
            Changed<crate::components::Order>,
            Changed<crate::components::Drawing>,
            Changed<crate::components::Width>,
        )>,
        With<PopupOf>,
    ),
>;

/// Any popup-affecting component changed since the last pass: a host's
/// [`PopupConfig`], or one of the components [`PopupContentChanged`] tracks
/// on the items inside it.
///
/// Combine with [`crate::layout::needs_repaint`] at the call site: a popup's
/// anchor also moves whenever its host's bar frame does, which this alone
/// would miss.
#[must_use]
pub fn needs_repaint_popups(
    config_changed: Query<(), Changed<PopupConfig>>,
    content_changed: PopupContentChanged,
    mut removed_config: RemovedComponents<PopupConfig>,
    mut removed_of: RemovedComponents<PopupOf>,
) -> bool {
    !config_changed.is_empty()
        || !content_changed.is_empty()
        || removed_config.read().next().is_some()
        || removed_of.read().next().is_some()
}

#[cfg(test)]
mod align_tests {
    use super::PopupAlign;

    #[test]
    fn the_short_forms_parse_the_same_as_the_long_ones() {
        assert_eq!("left".parse(), Ok(PopupAlign::Left));
        assert_eq!("l".parse(), Ok(PopupAlign::Left));
        assert_eq!("center".parse(), Ok(PopupAlign::Center));
        assert_eq!("centre".parse(), Ok(PopupAlign::Center));
        assert_eq!("right".parse(), Ok(PopupAlign::Right));
        assert!("sideways".parse::<PopupAlign>().is_err());
    }
}

#[cfg(test)]
mod arrange_tests {
    use super::{PopupPlaced, arrange_popup};
    use objc2_core_foundation::CGSize;

    fn item(id: u8, width: f64) -> PopupPlaced<u8> {
        PopupPlaced { id, width }
    }

    fn x_of(placed: &[(u8, objc2_core_foundation::CGRect)], id: u8) -> f64 {
        placed
            .iter()
            .find(|(i, _)| *i == id)
            .expect("id was placed")
            .1
            .origin
            .x
    }

    fn y_of(placed: &[(u8, objc2_core_foundation::CGRect)], id: u8) -> f64 {
        placed
            .iter()
            .find(|(i, _)| *i == id)
            .expect("id was placed")
            .1
            .origin
            .y
    }

    #[test]
    fn an_empty_popup_has_no_size() {
        let (placed, size) = arrange_popup::<u8>(&[], false, 30.0);
        assert!(placed.is_empty());
        assert_eq!(size, CGSize::new(0.0, 0.0));
    }

    #[test]
    fn vertical_rows_stack_top_to_bottom_at_the_widest_items_width() {
        let (placed, size) = arrange_popup(&[item(1, 40.0), item(2, 90.0)], false, 30.0);
        assert!((y_of(&placed, 1) - 0.0).abs() < 1e-9);
        assert!(
            (y_of(&placed, 2) - 30.0).abs() < 1e-9,
            "rows are row_height apart"
        );
        assert!(
            (x_of(&placed, 1) - 0.0).abs() < 1e-9,
            "every row shares one x"
        );
        assert!(
            (size.width - 90.0).abs() < 1e-9,
            "as wide as the widest row"
        );
        assert!((size.height - 60.0).abs() < 1e-9);
    }

    #[test]
    fn horizontal_rows_run_left_to_right_at_their_own_width() {
        let (placed, size) = arrange_popup(&[item(1, 40.0), item(2, 90.0)], true, 30.0);
        assert!((x_of(&placed, 1) - 0.0).abs() < 1e-9);
        assert!((x_of(&placed, 2) - 40.0).abs() < 1e-9);
        assert!(
            (y_of(&placed, 1) - y_of(&placed, 2)).abs() < 1e-9,
            "one shared row"
        );
        assert!((size.width - 130.0).abs() < 1e-9);
        assert!((size.height - 30.0).abs() < 1e-9);
    }
}

#[cfg(test)]
mod anchor_tests {
    use super::{PopupAlign, popup_anchor};
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use rsbar_protocol::Edge;

    fn host() -> CGRect {
        CGRect::new(CGPoint::new(100.0, 0.0), CGSize::new(40.0, 32.0))
    }

    fn popup(w: f64, h: f64) -> CGSize {
        CGSize::new(w, h)
    }

    #[test]
    fn left_align_shares_the_hosts_left_edge() {
        let anchor = popup_anchor(
            host(),
            popup(80.0, 60.0),
            PopupAlign::Left,
            Edge::Top,
            0.0,
            None,
        );
        assert!((anchor.x - 100.0).abs() < 1e-9);
    }

    #[test]
    fn right_align_shares_the_hosts_right_edge() {
        let anchor = popup_anchor(
            host(),
            popup(80.0, 60.0),
            PopupAlign::Right,
            Edge::Top,
            0.0,
            None,
        );
        // Host right edge is 140; the popup's own right edge must land there.
        assert!((anchor.x + 80.0 - 140.0).abs() < 1e-9);
    }

    #[test]
    fn a_popup_stays_on_its_display() {
        // The right-hand end of the bar is where most popups hang from, and a
        // window server window is clipped rather than moved, so anything past
        // the edge is simply lost.
        let host = CGRect::new(CGPoint::new(1900.0, 0.0), CGSize::new(40.0, 32.0));
        let anchor = popup_anchor(
            host,
            popup(200.0, 60.0),
            PopupAlign::Left,
            Edge::Top,
            0.0,
            Some(2000.0),
        );
        assert!((anchor.x - 1800.0).abs() < 1e-9, "{}", anchor.x);

        // One too wide to fit anywhere is left where it was asked for rather
        // than shoved to the left edge, which would only move the clipping.
        let anchor = popup_anchor(
            host,
            popup(3000.0, 60.0),
            PopupAlign::Left,
            Edge::Top,
            0.0,
            Some(2000.0),
        );
        assert!((anchor.x - 1900.0).abs() < 1e-9, "{}", anchor.x);
    }

    #[test]
    fn center_align_centres_over_the_host() {
        let anchor = popup_anchor(
            host(),
            popup(20.0, 60.0),
            PopupAlign::Center,
            Edge::Top,
            0.0,
            None,
        );
        // Host spans 100..140, centred at 120; popup is 20 wide.
        assert!((anchor.x - 110.0).abs() < 1e-9);
    }

    #[test]
    fn a_top_edge_bar_opens_the_popup_below_the_host() {
        let anchor = popup_anchor(
            host(),
            popup(80.0, 60.0),
            PopupAlign::Left,
            Edge::Top,
            0.0,
            None,
        );
        assert!(
            (anchor.y - 32.0).abs() < 1e-9,
            "below the host's own height"
        );
    }

    #[test]
    fn a_bottom_edge_bar_opens_the_popup_above_the_host() {
        let anchor = popup_anchor(
            host(),
            popup(80.0, 60.0),
            PopupAlign::Left,
            Edge::Bottom,
            0.0,
            None,
        );
        assert!(
            (anchor.y + 60.0 - 0.0).abs() < 1e-9,
            "the popup's own bottom meets the host's top"
        );
    }

    #[test]
    fn y_offset_widens_the_gap() {
        let flush = popup_anchor(
            host(),
            popup(80.0, 60.0),
            PopupAlign::Left,
            Edge::Top,
            0.0,
            None,
        );
        let gapped = popup_anchor(
            host(),
            popup(80.0, 60.0),
            PopupAlign::Left,
            Edge::Top,
            5.0,
            None,
        );
        assert!((gapped.y - flush.y - 5.0).abs() < 1e-9);
    }
}

#[cfg(test)]
mod patch_tests {
    use super::{PopupAlign, PopupConfig, PopupPatch, patched};

    #[test]
    fn a_patch_matching_every_current_field_changes_nothing() {
        let current = PopupConfig {
            drawing: true,
            ..PopupConfig::default()
        };
        let patch = PopupPatch {
            drawing: Some(current.drawing),
            horizontal: Some(current.horizontal),
            align: Some(current.align),
            topmost: Some(current.topmost),
            height: Some(current.height),
            y_offset: Some(current.y_offset),
            background: None,
        };
        assert_eq!(patched(&current, &patch), None);
    }

    #[test]
    fn a_patch_moving_one_field_is_reported() {
        let current = PopupConfig::default();
        let patch = PopupPatch {
            align: Some(PopupAlign::Right),
            ..PopupPatch::default()
        };
        let next = patched(&current, &patch).expect("align changed");
        assert_eq!(next.align, PopupAlign::Right);
    }

    #[test]
    fn an_empty_patch_changes_nothing() {
        let current = PopupConfig::default();
        assert_eq!(patched(&current, &PopupPatch::default()), None);
    }
}

#[cfg(test)]
mod placements_tests {
    use super::{PopupPlacements, contains};
    use crate::layout::PanelPlacements;
    use bevy_ecs::entity::Entity;
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    fn entity(id: u32) -> Entity {
        Entity::from_raw_u32(id).expect("a valid entity index")
    }

    #[test]
    fn a_point_outside_every_open_popup_hits_nothing() {
        let placements = PopupPlacements::default();
        assert_eq!(placements.hit(CGPoint::new(0.0, 0.0)), None);
    }

    #[test]
    fn a_point_on_a_row_reports_its_host_and_item() {
        let host = entity(1);
        let row = entity(2);
        let mut placements = PopupPlacements::default();
        placements.0.insert(
            host,
            PanelPlacements::new(
                1,
                CGRect::new(CGPoint::new(100.0, 40.0), CGSize::new(80.0, 30.0)),
                vec![(
                    row,
                    CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(80.0, 30.0)),
                )],
            ),
        );
        assert_eq!(
            placements.hit(CGPoint::new(110.0, 50.0)),
            Some((host, Some(row)))
        );
    }

    #[test]
    fn a_point_on_the_popup_but_off_every_row_reports_no_item() {
        assert!(contains(
            CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(10.0, 10.0)),
            CGPoint::new(5.0, 5.0)
        ));
    }
}

#[cfg(test)]
mod gating_tests {
    use super::{PopupOf, needs_repaint_popups};
    use crate::bar::Settings;
    use crate::components::{Label, Order, bundle};
    use crate::layout::{ForceRepaint, needs_repaint};
    use bevy_ecs::world::World;
    use rsbar_protocol::{ItemName, Position};

    fn world_with_bar_resources() -> World {
        let mut world = World::new();
        world.insert_resource(Settings::default());
        world.insert_resource(ForceRepaint::default());
        world
    }

    /// The hard requirement in one test: content that only a popup shows must
    /// not be why the bar's own repaint system runs at all, let alone what it
    /// repaints. `needs_repaint` is what gates `crate::layout::repaint` in the
    /// real schedule, so this is exactly the check the schedule would make.
    ///
    /// Registered rather than run with `run_system_once`: a one-shot system's
    /// change detection has no "last run" of its own, so its very first call
    /// would see every change since the `World` was created as new — a false
    /// positive this test exists to rule out, not produce. Registering it
    /// gives it a persistent last-run tick, primed by one throwaway call
    /// before the mutation under test, the same as a system that has already
    /// been ticking in the real schedule.
    #[test]
    fn a_popup_items_label_changing_does_not_ask_the_bar_to_repaint() {
        let mut world = world_with_bar_resources();
        let host = world
            .spawn(bundle(
                ItemName::new("host").unwrap(),
                Position::Left,
                Order(0),
            ))
            .id();
        let child = world
            .spawn(bundle(
                ItemName::new("child").unwrap(),
                Position::Left,
                Order(1),
            ))
            .id();
        world.entity_mut(child).insert(PopupOf(host));

        let bar_repaints = world.register_system(needs_repaint);
        let popup_repaints = world.register_system(needs_repaint_popups);
        // Primes both systems' last-run tick past every change spawning made,
        // the same way the real schedule's first tick would.
        world.run_system(bar_repaints).expect("a valid system");
        world.run_system(popup_repaints).expect("a valid system");

        world
            .entity_mut(child)
            .get_mut::<Label>()
            .expect("the bundle carries a label")
            .0
            .string = "hi".into();

        assert!(
            !world.run_system(bar_repaints).expect("a valid system"),
            "the bar's own layout did not move"
        );
        assert!(
            world.run_system(popup_repaints).expect("a valid system"),
            "but the popup showing it did"
        );
    }
}

#[cfg(test)]
mod visual_probe {
    //! Not part of the suite `cargo test -p rsbar` runs — like
    //! `examples/ax_observer_probe.rs` and its neighbours, drawing needs a
    //! real window server, which `harness.rs` deliberately does not fake. Run
    //! with `cargo test -p rsbar --lib popup::visual_probe -- --ignored
    //! --nocapture` and `screencapture` the screen while it sleeps.
    //!
    //! Exercises the same primitives [`super::repaint_popups`] would —
    //! [`super::arrange_popup`], [`super::popup_anchor`],
    //! [`crate::layout::draw_item`], [`crate::bar::fill_rounded_rect`] — by
    //! calling them directly rather than through the full system, since
    //! populating a real [`crate::layout::Placements`] needs a real bar panel
    //! this crate's own visibility rules keep out of reach from here.

    use crate::alias::Captures;
    use crate::components::{Icon, Label, Order, bundle};
    use crate::layout::draw_item;
    use crate::shaping::Cache;
    use bevy_ecs::system::SystemState;
    use bevy_ecs::world::World;
    use objc2_core_foundation::{CGPoint, CGRect};
    use rsbar_protocol::style::Color;
    use rsbar_protocol::{ItemName, Position};

    #[test]
    #[ignore = "opens a real window server window; run manually and screenshot it"]
    fn a_popup_window_actually_renders() {
        let mut world = World::new();
        let mut cache = Cache::default();
        let rows = ["Feedback?", "\u{f8bd}", "\u{f099}", "7"];
        let mut ids = Vec::new();
        for (i, text) in rows.iter().enumerate() {
            let id = world
                .spawn(bundle(
                    ItemName::new(format!("row-{i}")).unwrap(),
                    Position::Left,
                    Order(u32::try_from(i).unwrap()),
                ))
                .id();
            let mut label = Label(crate::components::Run::new("Menlo:Bold:14", Color::WHITE));
            label.0.string = (*text).to_owned();
            world.entity_mut(id).insert(label);
            let icon = world.get::<Icon>(id).unwrap().clone();
            let label = world.get::<Label>(id).unwrap().clone();
            cache.refresh(id, &icon.0, &label.0);
            ids.push(id);
        }

        let padding = world.get::<crate::components::Padding>(ids[0]).copied();
        let row_height = super::DEFAULT_ROW_HEIGHT;
        let measured: Vec<super::PopupPlaced<bevy_ecs::entity::Entity>> = ids
            .iter()
            .map(|&id| super::PopupPlaced {
                id,
                width: crate::layout::width(
                    &cache,
                    id,
                    &world.get::<Icon>(id).unwrap().clone(),
                    &world.get::<Label>(id).unwrap().clone(),
                    &padding.unwrap(),
                    None,
                    None,
                ),
            })
            .collect();
        let (local, size) = super::arrange_popup(&measured, false, row_height);

        let window = skylight::Window::new(CGRect::new(CGPoint::new(80.0, 40.0), size))
            .expect("a window server connection is available when run manually");
        window.set_opaque(false).unwrap();
        window.set_alpha(1.0).unwrap();
        window.set_level(skylight::level::POPUP_MENU).unwrap();
        window
            .set_tags(skylight::WindowTags::BAR | skylight::WindowTags::OPAQUE_FOR_EVENTS)
            .unwrap();

        let mut state: SystemState<crate::layout::ItemQuery> = SystemState::new(&mut world);
        let query = state.get(&world).expect("query param is valid");
        let captures = Captures::default();

        skylight::draw::<()>(window.id(), size, |ctx| {
            crate::bar::fill_rounded_rect(
                ctx,
                CGRect::new(CGPoint::new(0.0, 0.0), size),
                9.0,
                Color(0xe014_1820),
            );
            for &(entity, rect) in &local {
                draw_item(ctx, &cache, &captures, &query, entity, rect);
            }
        });

        window.order_above(None).unwrap();
        eprintln!(
            "popup probe window {:?} at 80,40 size {:?}",
            window.id(),
            size
        );
        std::thread::sleep(std::time::Duration::from_secs(12));
    }
}

//! The bar itself: geometry, the item list, and the draw pass.

use crate::display::{self, Display};
use crate::item::Item;
use crate::script::Job;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGContext;
use rsbar_protocol::style::Color;
use rsbar_protocol::{BarPatch, BarState, Edge, Event, ItemName, ItemPatch, Position};
use skylight::{Window, WindowTags, level};

/// Fills a rectangle whose corners are rounded, falling back to a plain fill
/// when the radius is zero — building a path for a square corner is wasted work
/// on the hot repaint path.
pub fn fill_rounded_rect(ctx: &CGContext, rect: CGRect, radius: f64, color: Color) {
    CGContext::set_rgb_fill_color(
        Some(ctx),
        color.red(),
        color.green(),
        color.blue(),
        color.alpha(),
    );

    let radius = radius
        .min(rect.size.width / 2.0)
        .min(rect.size.height / 2.0);
    if radius <= 0.0 {
        CGContext::fill_rect(Some(ctx), rect);
        return;
    }

    let (x, y) = (rect.origin.x, rect.origin.y);
    let (w, h) = (rect.size.width, rect.size.height);
    CGContext::begin_path(Some(ctx));
    CGContext::move_to_point(Some(ctx), x + radius, y);
    CGContext::add_arc_to_point(Some(ctx), x + w, y, x + w, y + h, radius);
    CGContext::add_arc_to_point(Some(ctx), x + w, y + h, x, y + h, radius);
    CGContext::add_arc_to_point(Some(ctx), x, y + h, x, y, radius);
    CGContext::add_arc_to_point(Some(ctx), x, y, x + w, y, radius);
    CGContext::close_path(Some(ctx));
    CGContext::fill_path(Some(ctx));
}

/// The bar on one display.
struct Panel {
    display: Display,
    window: Window,
    frame: CGRect,
}

pub struct Bar {
    height: f64,
    edge: Edge,
    color: Color,
    margin: f64,
    y_offset: f64,
    corner_radius: f64,
    blur_radius: i32,
    hidden: bool,

    items: Vec<Item>,
    /// One per display. The bar is not one window stretched across the desktop:
    /// with "Displays have separate Spaces" a single window renders on only one
    /// of them, so each display needs its own.
    panels: Vec<Panel>,
    /// Set by any mutation; the draw pass clears it. Repainting on every
    /// request would redraw the whole bar per `--set` in a config run.
    dirty: bool,
}

impl Bar {
    /// Creates the bar on the main display.
    ///
    /// # Errors
    ///
    /// Returns the window server's error if the bar window cannot be created
    /// or configured.
    pub fn new() -> skylight::Result<Self> {
        let mut bar = Self {
            height: 32.0,
            edge: Edge::Top,
            color: Color(0xe014_1820),
            margin: 0.0,
            y_offset: 0.0,
            corner_radius: 0.0,
            blur_radius: 0,
            hidden: false,
            items: Vec::new(),
            panels: Vec::new(),
            dirty: true,
        };
        bar.rebuild_panels()?;
        Ok(bar)
    }

    /// Rebuilds one panel per active display, reusing the window of a display
    /// that is still present so a resize does not flicker the bar away.
    ///
    /// # Errors
    ///
    /// Returns the window server's error if a window cannot be created or
    /// configured.
    pub fn rebuild_panels(&mut self) -> skylight::Result<()> {
        let displays = display::active();
        let mut panels = Vec::with_capacity(displays.len());

        for display in displays {
            let frame = self.frame_for(&display);
            let existing = self
                .panels
                .iter()
                .position(|panel| panel.display.id == display.id);

            let panel = if let Some(index) = existing {
                let mut panel = self.panels.remove(index);
                panel.window.set_frame(frame)?;
                if (panel.display.scale - display.scale).abs() > f64::EPSILON {
                    panel.window.set_scale(display.scale)?;
                }
                panel.display = display;
                panel.frame = frame;
                panel
            } else {
                let window = Window::new(frame)?;
                window.set_scale(display.scale)?;
                window.set_opaque(false)?;
                window.set_alpha(1.0)?;
                window.set_level(level::STATUS)?;
                window.set_tags(WindowTags::BAR | WindowTags::IGNORE_FOR_EVENTS)?;
                if self.blur_radius != 0 {
                    window.set_blur_radius(self.blur_radius)?;
                }
                if !self.hidden {
                    window.order_above(None)?;
                }
                Panel {
                    display,
                    window,
                    frame,
                }
            };
            panels.push(panel);
        }

        for panel in &panels {
            tracing::debug!(
                display = panel.display.id,
                scale = panel.display.scale,
                frame = ?panel.frame,
                "panel"
            );
        }

        // Whatever is left in `self.panels` belongs to a display that is gone;
        // dropping it releases the window.
        self.panels = panels;
        self.dirty = true;
        Ok(())
    }

    fn frame_for(&self, display: &Display) -> CGRect {
        let bounds = display.bounds;
        let y = match self.edge {
            Edge::Top => bounds.origin.y + self.y_offset,
            Edge::Bottom => bounds.origin.y + bounds.size.height - self.height - self.y_offset,
        };
        CGRect::new(
            CGPoint::new(bounds.origin.x + self.margin, y),
            CGSize::new(bounds.size.width - 2.0 * self.margin, self.height),
        )
    }

    /// # Errors
    ///
    /// Returns the window server's error if a geometry or visibility change
    /// is rejected.
    pub fn apply(&mut self, patch: &BarPatch) -> skylight::Result<()> {
        let mut geometry_changed = false;
        if let Some(h) = patch.height {
            self.height = h;
            geometry_changed = true;
        }
        if let Some(e) = patch.edge {
            self.edge = e;
            geometry_changed = true;
        }
        if let Some(m) = patch.margin {
            self.margin = m;
            geometry_changed = true;
        }
        if let Some(y) = patch.y_offset {
            self.y_offset = y;
            geometry_changed = true;
        }
        if let Some(c) = patch.color {
            self.color = Color(c);
        }
        if let Some(r) = patch.corner_radius {
            self.corner_radius = r;
        }
        if let Some(r) = patch.blur_radius {
            self.blur_radius = r;
            for panel in &self.panels {
                panel.window.set_blur_radius(r)?;
            }
        }
        if let Some(hidden) = patch.hidden {
            self.hidden = hidden;
            for panel in &self.panels {
                if hidden {
                    panel.window.order_out()?;
                } else {
                    panel.window.order_above(None)?;
                }
            }
        }

        if geometry_changed {
            // One batch, so a height change lands on every display at once
            // rather than rippling across them.
            let frames: Vec<_> = self
                .panels
                .iter()
                .map(|p| self.frame_for(&p.display))
                .collect();
            skylight::batched(|| -> skylight::Result<()> {
                for (panel, frame) in self.panels.iter_mut().zip(frames) {
                    panel.window.set_frame(frame)?;
                    panel.frame = frame;
                }
                Ok(())
            })?;
        }
        self.dirty = true;
        Ok(())
    }

    pub fn add_item(&mut self, name: ItemName, position: Position) {
        if let Some(existing) = self.items.iter_mut().find(|i| i.name == name) {
            existing.position = position;
        } else {
            self.items.push(Item::new(name, position));
        }
        self.dirty = true;
    }

    /// Returns whether the item existed.
    pub fn set_item(&mut self, name: &ItemName, patch: &ItemPatch) -> bool {
        let Some(item) = self.items.iter_mut().find(|i| &i.name == name) else {
            return false;
        };
        item.apply(patch);
        self.dirty = true;
        true
    }

    pub fn remove_item(&mut self, name: &ItemName) -> bool {
        let before = self.items.len();
        self.items.retain(|i| &i.name != name);
        self.dirty = self.dirty || self.items.len() != before;
        self.items.len() != before
    }

    /// Replaces an item's subscriptions. Returns whether the item existed.
    pub fn subscribe(&mut self, name: &ItemName, events: Vec<Event>) -> bool {
        let Some(item) = self.items.iter_mut().find(|i| &i.name == name) else {
            return false;
        };
        item.subscribe(events);
        true
    }

    /// The scripts to run for `event`. Items without a script are skipped:
    /// subscribing a scriptless item is legal and simply does nothing.
    #[must_use]
    pub fn jobs_for(&self, event: &Event, info: Option<&str>) -> Vec<Job> {
        self.items
            .iter()
            .filter(|item| item.wants(event))
            .filter_map(|item| Self::job(item, event.clone(), info))
            .collect()
    }

    /// Advances every item's routine clock and returns those now due.
    #[must_use]
    pub fn tick(&mut self) -> Vec<Job> {
        let mut due = Vec::new();
        for item in &mut self.items {
            if item.tick()
                && let Some(script) = item.script.clone()
            {
                due.push(Job {
                    item: item.name.clone(),
                    script,
                    sender: Event::Routine,
                    info: None,
                });
            }
        }
        due
    }

    /// Every item's script, regardless of frequency or subscription.
    #[must_use]
    pub fn all_jobs(&self) -> Vec<Job> {
        self.items
            .iter()
            .filter_map(|item| Self::job(item, Event::Forced, None))
            .collect()
    }

    fn job(item: &Item, sender: Event, info: Option<&str>) -> Option<Job> {
        Some(Job {
            item: item.name.clone(),
            script: item.script.clone()?,
            sender,
            info: info.map(ToOwned::to_owned),
        })
    }

    #[must_use]
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    #[must_use]
    pub fn item(&self, name: &ItemName) -> Option<&Item> {
        self.items.iter().find(|i| &i.name == name)
    }

    #[must_use]
    pub fn state(&self) -> BarState {
        BarState {
            height: self.height,
            edge: self.edge,
            color: self.color.0,
            margin: self.margin,
            y_offset: self.y_offset,
            corner_radius: self.corner_radius,
            blur_radius: self.blur_radius,
            hidden: self.hidden,
            displays: self.panels.len(),
        }
    }

    /// Lays out and repaints, if anything changed since the last pass.
    pub fn redraw_if_dirty(&mut self) {
        if !self.dirty || self.hidden {
            return;
        }
        self.dirty = false;

        let bar_color = self.color;
        let radius = self.corner_radius;

        for panel in &self.panels {
            let size = panel.frame.size;
            // Layout depends on the panel's width, so it is computed per
            // display rather than shared across them.
            let placements = self.layout(size);
            skylight::draw(panel.window.id(), size, |ctx| {
                fill_rounded_rect(
                    ctx,
                    CGRect::new(CGPoint::new(0.0, 0.0), size),
                    radius,
                    bar_color,
                );
                for (index, frame) in placements {
                    self.items[index].draw(ctx, frame);
                }
            });
        }
    }

    /// Assigns each drawn item a frame, bucket by bucket.
    ///
    /// Right and centre-right run right-to-left so their trailing edges stay
    /// pinned as content resizes; the centre group is measured as a whole and
    /// then placed, so it stays centred rather than growing from its left edge.
    fn layout(&self, size: CGSize) -> Vec<(usize, CGRect)> {
        let width = size.width;
        let height = size.height;
        let drawn = |p: Position| {
            self.items
                .iter()
                .enumerate()
                .filter(move |(_, i)| i.drawing && i.position == p)
        };
        let group_width = |p: Position| drawn(p).map(|(_, i)| i.width()).sum::<f64>();

        let mut placements = Vec::with_capacity(self.items.len());
        let mut push = |index: usize, x: f64, w: f64| {
            placements.push((
                index,
                CGRect::new(CGPoint::new(x, 0.0), CGSize::new(w, height)),
            ));
        };

        let mut x = 0.0;
        for (index, item) in drawn(Position::Left) {
            let w = item.width();
            push(index, x, w);
            x += w;
        }

        let mut x = width;
        for (index, item) in drawn(Position::Right).collect::<Vec<_>>().into_iter().rev() {
            let w = item.width();
            x -= w;
            push(index, x, w);
        }

        let centre = group_width(Position::Center);
        let mut x = (width - centre) / 2.0;
        let centre_start = x;
        for (index, item) in drawn(Position::Center) {
            let w = item.width();
            push(index, x, w);
            x += w;
        }
        let centre_end = x;

        let mut x = centre_start;
        for (index, item) in drawn(Position::CenterLeft)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            let w = item.width();
            x -= w;
            push(index, x, w);
        }

        let mut x = centre_end;
        for (index, item) in drawn(Position::CenterRight) {
            let w = item.width();
            push(index, x, w);
            x += w;
        }

        placements
    }
}

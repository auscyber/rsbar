//! The bar itself: geometry, the item list, and the draw pass.

use crate::item::Item;
use crate::script::Job;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGContext, CGDisplayBounds, CGMainDisplayID};
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
    window: Window,
    frame: CGRect,
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
        let height = 32.0;
        let frame = Self::frame_for(height, Edge::Top, 0.0, 0.0);
        let window = Window::new(frame)?;
        window.set_scale(Self::scale())?;
        window.set_opaque(false)?;
        window.set_alpha(1.0)?;
        window.set_level(level::STATUS)?;
        window.set_tags(WindowTags::BAR | WindowTags::IGNORE_FOR_EVENTS)?;
        window.order_above(None)?;

        Ok(Self {
            height,
            edge: Edge::Top,
            color: Color(0xe014_1820),
            margin: 0.0,
            y_offset: 0.0,
            corner_radius: 0.0,
            blur_radius: 0,
            hidden: false,
            items: Vec::new(),
            window,
            frame,
            dirty: true,
        })
    }

    fn scale() -> f64 {
        // TODO: per-display backing scale once the bar spans displays.
        2.0
    }

    fn frame_for(height: f64, edge: Edge, margin: f64, y_offset: f64) -> CGRect {
        let display = CGDisplayBounds(CGMainDisplayID());
        let y = match edge {
            Edge::Top => display.origin.y + y_offset,
            Edge::Bottom => display.origin.y + display.size.height - height - y_offset,
        };
        CGRect::new(
            CGPoint::new(display.origin.x + margin, y),
            CGSize::new(display.size.width - 2.0 * margin, height),
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
            self.window.set_blur_radius(r)?;
        }
        if let Some(hidden) = patch.hidden {
            self.hidden = hidden;
            if hidden {
                self.window.order_out()?;
            } else {
                self.window.order_above(None)?;
            }
        }

        if geometry_changed {
            self.frame = Self::frame_for(self.height, self.edge, self.margin, self.y_offset);
            self.window.set_frame(self.frame)?;
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
            displays: 1,
        }
    }

    /// Lays out and repaints, if anything changed since the last pass.
    pub fn redraw_if_dirty(&mut self) {
        if !self.dirty || self.hidden {
            return;
        }
        self.dirty = false;

        let size = self.frame.size;
        let bar_color = self.color;
        let radius = self.corner_radius;
        let placements = self.layout();

        skylight::draw(self.window.id(), size, |ctx| {
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

    /// Assigns each drawn item a frame, bucket by bucket.
    ///
    /// Right and centre-right run right-to-left so their trailing edges stay
    /// pinned as content resizes; the centre group is measured as a whole and
    /// then placed, so it stays centred rather than growing from its left edge.
    fn layout(&self) -> Vec<(usize, CGRect)> {
        let width = self.frame.size.width;
        let height = self.frame.size.height;
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

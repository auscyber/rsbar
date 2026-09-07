//! One thing drawn on the bar.

use crate::text::{Font, Text};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGContext;
use rsbar_protocol::style::{Color, FontSpec};
use rsbar_protocol::{Event, ItemName, ItemPatch, ItemState, Position};
use std::collections::BTreeSet;

/// An item's icon and label are the same kind of thing, differing only in
/// which defaults they start from.
pub struct Slot {
    pub text: Text,
    pub color: Color,
}

impl Slot {
    fn new(font: &FontSpec, color: Color) -> Self {
        Self {
            text: Text::new("", Font::resolve(font)),
            color,
        }
    }

    /// Contributes nothing to layout when empty, so an icon-only item has no
    /// phantom gap where its label would be.
    fn width(&self) -> f64 {
        if self.text.string().is_empty() {
            0.0
        } else {
            self.text.metrics().width
        }
    }
}

pub struct Item {
    pub name: ItemName,
    pub position: Position,
    pub icon: Slot,
    pub label: Slot,
    pub background: Color,
    pub corner_radius: f64,
    pub padding_left: f64,
    pub padding_right: f64,
    pub y_offset: f64,
    pub drawing: bool,
    /// Gap between icon and label, when both are present.
    pub spacing: f64,

    pub script: Option<String>,
    /// Seconds between routine updates; zero means event-driven only.
    pub update_freq: u32,
    /// Seconds since this item last ran on the routine tick.
    elapsed: u32,
    events: BTreeSet<Event>,
}

impl Item {
    #[must_use]
    pub fn new(name: ItemName, position: Position) -> Self {
        Self {
            name,
            position,
            icon: Slot::new(&FontSpec::parse("Menlo:Bold:15"), Color::WHITE),
            label: Slot::new(&FontSpec::parse("Menlo:Regular:13"), Color::WHITE),
            background: Color::TRANSPARENT,
            corner_radius: 0.0,
            padding_left: 8.0,
            padding_right: 8.0,
            y_offset: 0.0,
            drawing: true,
            spacing: 5.0,
            script: None,
            update_freq: 0,
            elapsed: 0,
            events: BTreeSet::new(),
        }
    }

    /// Whether this item asked to hear about `event`.
    #[must_use]
    pub fn wants(&self, event: &Event) -> bool {
        self.events.contains(event)
    }

    pub fn subscribe(&mut self, events: impl IntoIterator<Item = Event>) {
        self.events = events.into_iter().collect();
    }

    /// Advances the routine clock by a second, reporting whether this item is
    /// due. An update frequency of zero never comes due.
    pub fn tick(&mut self) -> bool {
        if self.update_freq == 0 {
            return false;
        }
        self.elapsed += 1;
        if self.elapsed >= self.update_freq {
            self.elapsed = 0;
            return true;
        }
        false
    }

    /// Total horizontal space this item occupies, padding included.
    #[must_use]
    pub fn width(&self) -> f64 {
        let (icon, label) = (self.icon.width(), self.label.width());
        let spacing = if icon > 0.0 && label > 0.0 {
            self.spacing
        } else {
            0.0
        };
        self.padding_left + icon + spacing + label + self.padding_right
    }

    pub fn apply(&mut self, patch: &ItemPatch) {
        if let Some(icon) = &patch.icon {
            self.icon.text.set_string(icon);
        }
        if let Some(label) = &patch.label {
            self.label.text.set_string(label);
        }
        if let Some(spec) = &patch.icon_font {
            self.icon
                .text
                .set_font(Font::resolve(&FontSpec::parse(spec)));
        }
        if let Some(spec) = &patch.label_font {
            self.label
                .text
                .set_font(Font::resolve(&FontSpec::parse(spec)));
        }
        if let Some(c) = patch.icon_color {
            self.icon.color = Color(c);
        }
        if let Some(c) = patch.label_color {
            self.label.color = Color(c);
        }
        if let Some(c) = patch.background_color {
            self.background = Color(c);
        }
        if let Some(r) = patch.corner_radius {
            self.corner_radius = r;
        }
        if let Some(p) = patch.padding_left {
            self.padding_left = p;
        }
        if let Some(p) = patch.padding_right {
            self.padding_right = p;
        }
        if let Some(y) = patch.y_offset {
            self.y_offset = y;
        }
        if let Some(p) = patch.position {
            self.position = p;
        }
        if let Some(d) = patch.drawing {
            self.drawing = d;
        }
        if let Some(script) = &patch.script {
            self.script = Some(script.clone()).filter(|s| !s.is_empty());
        }
        if let Some(freq) = patch.update_freq {
            self.update_freq = freq;
            // A changed frequency restarts the clock, so a config that sets it
            // twice does not fire early on the second set.
            self.elapsed = 0;
        }
    }

    /// Draws into `frame`, which the layout has already sized to [`Self::width`].
    pub fn draw(&self, ctx: &CGContext, frame: CGRect) {
        if !self.drawing {
            return;
        }

        if !self.background.is_invisible() {
            crate::bar::fill_rounded_rect(ctx, frame, self.corner_radius, self.background);
        }

        let mut x = frame.origin.x + self.padding_left;
        let y = frame.origin.y + self.y_offset;
        for slot in [&self.icon, &self.label] {
            let w = slot.width();
            if w == 0.0 {
                continue;
            }
            slot.text.draw(
                ctx,
                CGRect::new(CGPoint::new(x, y), CGSize::new(w, frame.size.height)),
                slot.color,
            );
            x += w + self.spacing;
        }
    }

    #[must_use]
    pub fn state(&self) -> ItemState {
        ItemState {
            name: self.name.clone(),
            position: self.position,
            icon: self.icon.text.string().to_owned(),
            label: self.label.text.string().to_owned(),
            drawing: self.drawing,
            script: self.script.clone(),
            update_freq: self.update_freq,
            events: self.events.iter().cloned().collect(),
        }
    }
}

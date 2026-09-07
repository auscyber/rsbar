//! Shaped text, kept out of the ECS.
//!
//! A `CTLine` is `!Send`, so it cannot be a component. It lives here instead,
//! in a `NonSend` cache keyed by entity, refreshed from `Changed<Icon>` and
//! `Changed<Label>`.
//!
//! That is not a workaround so much as the right split: shaping is expensive
//! relative to a repaint, a bar repaints far more often than its strings
//! change, and driving the cache off change detection means it cannot go stale
//! without someone noticing — unlike a flag that has to be set by hand.

use crate::components::Run;
use crate::text::{Font, Metrics, Text};
use bevy_ecs::prelude::Entity;
use objc2_core_foundation::CGRect;
use objc2_core_graphics::CGContext;
use rsbar_protocol::style::Color;
use std::collections::HashMap;

/// One item's shaped icon and label.
pub struct Shaped {
    pub icon: Text,
    pub label: Text,
}

impl Shaped {
    fn new(icon: &Run, label: &Run) -> Self {
        Self {
            icon: Text::new(icon.string.clone(), Font::resolve(&icon.font)),
            label: Text::new(label.string.clone(), Font::resolve(&label.font)),
        }
    }

    #[must_use]
    pub fn icon_metrics(&self) -> Metrics {
        self.icon.metrics()
    }

    #[must_use]
    pub fn label_metrics(&self) -> Metrics {
        self.label.metrics()
    }

    pub fn draw_icon(&self, ctx: &CGContext, box_: CGRect, color: Color) {
        self.icon.draw(ctx, box_, color);
    }

    pub fn draw_label(&self, ctx: &CGContext, box_: CGRect, color: Color) {
        self.label.draw(ctx, box_, color);
    }
}

/// The cache. `NonSend` at the app level.
#[derive(Default)]
pub struct Cache(HashMap<Entity, Shaped>);

impl Cache {
    /// Reshapes one item. Cheap when nothing moved: [`Text`] compares before it
    /// rebuilds a line.
    pub fn refresh(&mut self, entity: Entity, icon: &Run, label: &Run) {
        match self.0.get_mut(&entity) {
            Some(shaped) => {
                shaped.icon.set_font(Font::resolve(&icon.font));
                shaped.icon.set_string(&icon.string);
                shaped.label.set_font(Font::resolve(&label.font));
                shaped.label.set_string(&label.string);
            }
            None => {
                self.0.insert(entity, Shaped::new(icon, label));
            }
        }
    }

    pub fn forget(&mut self, entity: Entity) {
        self.0.remove(&entity);
    }

    #[must_use]
    pub fn get(&self, entity: Entity) -> Option<&Shaped> {
        self.0.get(&entity)
    }
}

//! Shaped text, kept out of the ECS.
//!
//! The index this module keeps — which entity has which shaped icon and
//! label — is `NonSend` regardless: it lives in a cache keyed by entity,
//! refreshed from `Changed<Icon>` and `Changed<Label>`, read every frame by
//! the same main thread that owns the `CGContext`s it draws into. But
//! *building* a shaped line no longer has to happen on that thread —
//! [`crate::text::Line`] is a narrow `Send` wrapper around a `CTLine`,
//! argued once where it is defined. [`Cache::refresh_changed`] is what spends
//! that: one shaping job per changed item, fanned out across [`crate::pool`]
//! and joined before the pass that needed them moves on.
//!
//! That is not a workaround so much as the right split: shaping is expensive
//! relative to a repaint, a bar repaints far more often than its strings
//! change, and driving the cache off change detection means it cannot go stale
//! without someone noticing — unlike a flag that has to be set by hand.

use crate::components::Run;
use crate::text::{Font, Metrics, Text, shape_now};
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
    #[must_use]
    pub fn new(icon: &Run, label: &Run) -> Self {
        Self {
            icon: Text::new(icon.string.clone(), &Font::resolve(&icon.font)),
            label: Text::new(label.string.clone(), &Font::resolve(&label.font)),
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

/// One item's dirty half or halves, gathered on the main thread before any
/// work leaves it. A half that has not moved is `None` and never becomes a
/// [`crate::pool::blocking`] job at all — see [`Cache::refresh_changed`].
struct Job {
    entity: Entity,
    icon: Option<Run>,
    label: Option<Run>,
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
                shaped.icon.set_font(&Font::resolve(&icon.font));
                shaped.icon.set_string(&icon.string);
                shaped.label.set_font(&Font::resolve(&label.font));
                shaped.label.set_string(&label.string);
            }
            None => {
                self.0.insert(entity, Shaped::new(icon, label));
            }
        }
    }

    /// Reshapes whichever of `items` actually changed, concurrently across
    /// [`crate::pool`], blocking the caller until every one of them has
    /// landed.
    ///
    /// `items` is the coarse pass — everything `Changed<Icon>` or
    /// `Changed<Label>` fired for, which is any field of a run moving, not
    /// only its string or font. The first thing this does is the same
    /// per-half check [`Self::needs_reshaping`] makes, so a `--set clock
    /// icon.y_offset=5` still reaches no worker and no `CoreText` call at
    /// all; only a string or font that actually differs turns into a job.
    ///
    /// [`ecs::reshape`](crate::ecs) is the caller this exists for: it still
    /// blocks the thread that draws, same as [`Self::refresh`] always did,
    /// but the wall-clock cost of a burst of changed items now divides by
    /// how many cores are shaping them instead of multiplying by how many
    /// there are.
    ///
    /// One job per item that needs one, not per line: an icon and a label
    /// that changed together are one round trip, and a half that has not
    /// moved is never sent to a worker at all.
    ///
    /// Dispatched on [`crate::pool::blocking`] rather than
    /// [`crate::pool::spawn`]: this is CPU-bound work with no `await` in it
    /// anywhere, and a burst of it on the async workers would occupy every
    /// one of them for the burst's duration, delaying whatever source or IPC
    /// task was next due on them. The blocking pool exists for exactly that —
    /// a thread per job, out of a pool tokio keeps warm for ten seconds of
    /// idle time by default, comfortably longer than the gap between two
    /// bursts of items changing.
    ///
    /// Not bounded by a semaphore: the pool is not bounded the way
    /// [`crate::pool::spawn`]'s fixed worker set is, but a bar reshaping a
    /// handful of items a pass is nowhere near tokio's own default cap on
    /// concurrent blocking threads, and each job is short enough --
    /// sub-millisecond -- that even a large config's full-reshape burst costs
    /// a moment of extra thread creation, not contention worth a permit for.
    ///
    /// Joined over a plain [`std::sync::mpsc`] channel rather than the
    /// runtime's own `block_on`: the caller is the thread that draws, and
    /// nothing here should have to know whether that thread is ever polling
    /// some other executor when this runs. A channel needs no answer to that
    /// question.
    pub fn refresh_changed<'a>(&mut self, items: impl Iterator<Item = (Entity, &'a Run, &'a Run)>) {
        let jobs: Vec<Job> = items
            .filter_map(|(entity, icon, label)| {
                let existing = self.0.get(&entity);
                let icon_dirty =
                    existing.is_none_or(|shaped| shaped.icon.is_stale(&icon.string, &icon.font));
                let label_dirty =
                    existing.is_none_or(|shaped| shaped.label.is_stale(&label.string, &label.font));
                (icon_dirty || label_dirty).then(|| Job {
                    entity,
                    icon: icon_dirty.then(|| icon.clone()),
                    label: label_dirty.then(|| label.clone()),
                })
            })
            .collect();
        if jobs.is_empty() {
            return;
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let expected = jobs.len();
        for job in jobs {
            let tx = tx.clone();
            crate::pool::blocking(move || {
                let icon = job.icon.map(|run| (shape_now(&run.string, &run.font), run));
                let label = job
                    .label
                    .map(|run| (shape_now(&run.string, &run.font), run));
                let _ = tx.send((job.entity, icon, label));
            });
        }
        drop(tx);

        for _ in 0..expected {
            let Ok((entity, icon, label)) = rx.recv() else {
                // A job panicked before it could send: leaving the item's
                // last good line stand is closer to correct than blanking it,
                // and the next change to touch it will ask again.
                tracing::warn!("a shaping job on the pool vanished without answering");
                break;
            };
            let shaped = self.0.entry(entity).or_insert_with(|| Shaped {
                icon: Text::empty(),
                label: Text::empty(),
            });
            if let Some((shape, run)) = icon {
                shaped.icon.install(run.string, run.font, shape);
            }
            if let Some((shape, run)) = label {
                shaped.label.install(run.string, run.font, shape);
            }
        }
    }

    /// Whether [`Self::refresh`] would rebuild anything for this item.
    ///
    /// `Changed<Icon>` is set by *any* field of the run moving, so this is
    /// what tells a y-offset from a new string. The cache's own contents are
    /// the comparison -- there is no second notion of dirtiness to keep in
    /// step with the first.
    #[must_use]
    pub fn needs_reshaping(&self, entity: Entity, icon: &Run, label: &Run) -> bool {
        let Some(shaped) = self.0.get(&entity) else {
            return true;
        };
        shaped.icon.is_stale(&icon.string, &icon.font)
            || shaped.label.is_stale(&label.string, &label.font)
    }

    pub fn forget(&mut self, entity: Entity) {
        self.0.remove(&entity);
    }

    #[must_use]
    pub fn get(&self, entity: Entity) -> Option<&Shaped> {
        self.0.get(&entity)
    }
}

#[cfg(test)]
mod tests {
    use super::Cache;
    use crate::components::Run;
    use bevy_ecs::prelude::Entity;
    use rsbar_protocol::style::Color;

    /// The gate `reshape` leans on: `Changed<Icon>` fires for any field of a
    /// run, and only two of them shape anything.
    #[test]
    fn only_the_string_and_the_font_ask_for_a_reshape() {
        let entity = Entity::PLACEHOLDER;
        let mut cache = Cache::default();
        let icon = Run::new("Menlo:Regular:12", Color(0xffff_ffff));
        let label = Run::new("Menlo:Regular:12", Color(0xffff_ffff));

        assert!(
            cache.needs_reshaping(entity, &icon, &label),
            "nothing shaped yet"
        );
        cache.refresh(entity, &icon, &label);
        assert!(!cache.needs_reshaping(entity, &icon, &label));

        // What a `--set clock icon.y_offset=5 icon.color=…` moves.
        let nudged = Run {
            y_offset: 5.0,
            color: Color(0xff00_ff00),
            padding_left: 4.0,
            ..icon.clone()
        };
        assert!(!cache.needs_reshaping(entity, &nudged, &label));

        let retitled = Run {
            string: "12:00".to_owned(),
            ..icon.clone()
        };
        assert!(cache.needs_reshaping(entity, &retitled, &label));

        let refonted = Run {
            font: rsbar_protocol::FontSpec::parse("Menlo:Bold:15"),
            ..icon.clone()
        };
        assert!(cache.needs_reshaping(entity, &refonted, &label));

        // Either half counts.
        let relabelled = Run {
            string: "12:00".to_owned(),
            ..label.clone()
        };
        assert!(cache.needs_reshaping(entity, &icon, &relabelled));
    }

    /// The concurrent path has to answer exactly what the serial one does --
    /// it is a wall-clock change, not a behavioural one.
    #[test]
    fn refresh_changed_matches_serial_refresh() {
        let entity = Entity::PLACEHOLDER;
        let icon = Run::new("Menlo:Regular:12", Color(0xffff_ffff));
        let mut label = Run::new("Menlo:Regular:12", Color(0xffff_ffff));
        label.string = "12:34".to_owned();

        let mut serial = Cache::default();
        serial.refresh(entity, &icon, &label);

        let mut concurrent = Cache::default();
        concurrent.refresh_changed(std::iter::once((entity, &icon, &label)));

        let expected = serial.get(entity).expect("the serial path shaped it");
        let actual = concurrent
            .get(entity)
            .expect("refresh_changed shaped it too");
        assert_eq!(actual.icon_metrics(), expected.icon_metrics());
        assert_eq!(actual.label_metrics(), expected.label_metrics());
    }

    /// Nothing dirty means nothing dispatched -- an empty pass must not touch
    /// the runtime at all, let alone block waiting on it.
    #[test]
    fn refresh_changed_with_nothing_dirty_dispatches_nothing() {
        let mut cache = Cache::default();
        cache.refresh_changed(std::iter::empty());
        assert!(cache.get(Entity::PLACEHOLDER).is_none());

        let entity = Entity::PLACEHOLDER;
        let icon = Run::new("Menlo:Regular:12", Color(0xffff_ffff));
        let label = Run::new("Menlo:Regular:12", Color(0xffff_ffff));
        cache.refresh(entity, &icon, &label);
        cache.refresh_changed(std::iter::once((entity, &icon, &label)));
        assert!(
            !cache.needs_reshaping(entity, &icon, &label),
            "still shaped from the first refresh, not blanked by the second"
        );
    }
}

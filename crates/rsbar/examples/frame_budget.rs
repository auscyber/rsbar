//! What a burst of changes inside one frame costs in draws.
//!
//! `burst_cost.rs` measures what one draw costs. This measures how many of
//! them a burst buys, which is the other half of the same question: fifty
//! `--set`s landing inside one 16.67 ms frame used to be fifty repaints and
//! fifty window server round trips, all but the last of them invisible.
//!
//! The systems here are the daemon's own — `layout::needs_repaint`, the
//! budget, the run conditions — with only the draw itself stubbed out, since
//! that is the one part that needs a window server. So the number below is
//! the number of times `layout::repaint` would have run.

use bevy_ecs::prelude::*;
use bevy_ecs::schedule::Schedule;
use rsbar::bar::Settings;
use rsbar::components::{Label, Order, bundle};
use rsbar::ecs::{FrameBudget, close_frame, presenting_bar, remember_bar, remember_popups};
use rsbar::layout::{DirtyItems, ForceRepaint, clear_force_repaint, needs_repaint};
use rsbar::popup::needs_repaint_popups;
use rsbar_protocol::{ItemName, Position};
use std::time::{Duration, Instant};

/// A 60 Hz panel, and a client sending updates far faster than one.
const FRAME: Duration = Duration::from_micros(16_667);
const GAP: Duration = Duration::from_micros(200);

/// The wake's instant, moved by hand rather than by the world.
///
/// `ecs::open_frame` reads the real clock; `FrameBudget::open` takes the
/// instant explicitly so a harness like this one can drive it, which is the
/// only reason this stands in for that system.
#[derive(Resource)]
struct Clock(Instant);

#[allow(clippy::needless_pass_by_value, reason = "a Bevy system parameter")]
fn open_frame(mut budget: ResMut<FrameBudget>, clock: Res<Clock>) {
    budget.open(clock.0);
}

/// Stands in for `layout::repaint`: counts the draws, and how much damage
/// each one carried, which is what says the deferred changes were not lost.
#[derive(Resource, Default)]
struct Draws {
    count: usize,
    damage: Vec<usize>,
}

fn draw(dirty: DirtyItems, mut draws: ResMut<Draws>) {
    let damaged = dirty.iter().count();
    draws.count += 1;
    draws.damage.push(damaged);
}

struct Bar {
    world: World,
    schedule: Schedule,
    items: Vec<Entity>,
}

impl Bar {
    /// A world of `items` items, and either the schedule as it was — a draw
    /// gated only on "did anything change" — or the frame-budgeted one.
    fn new(items: usize, capped: bool) -> Self {
        let mut world = World::new();
        world.insert_resource(Settings::default());
        world.insert_resource(ForceRepaint::default());
        world.insert_resource(Draws::default());
        world.insert_resource(Clock(Instant::now()));
        world.insert_resource(FrameBudget::new(FRAME));

        let items = (0..items)
            .map(|i| {
                let name = ItemName::new(format!("item{i}")).expect("a valid name");
                let order = Order(u32::try_from(i).expect("a small count"));
                world.spawn(bundle(name, Position::Left, order)).id()
            })
            .collect();

        let mut schedule = Schedule::default();
        if capped {
            schedule.add_systems(
                (
                    needs_repaint.pipe(remember_bar),
                    needs_repaint_popups.pipe(remember_popups),
                    open_frame,
                    draw.run_if(presenting_bar),
                    clear_force_repaint.run_if(rsbar::ecs::presenting),
                    close_frame,
                )
                    .chain(),
            );
        } else {
            schedule.add_systems((draw.run_if(needs_repaint), clear_force_repaint).chain());
        }

        let mut bar = Self {
            world,
            schedule,
            items,
        };
        // Spawning marked everything changed; let the first pass draw that,
        // the way the real bar's first pass does, then start counting.
        bar.wake(GAP);
        bar.world.resource_mut::<Draws>().count = 0;
        bar.world.resource_mut::<Draws>().damage.clear();
        bar
    }

    /// One run loop wake, `gap` after the last one.
    fn wake(&mut self, gap: Duration) {
        self.world.resource_mut::<Clock>().0 += gap;
        self.schedule.run(&mut self.world);
    }

    fn touch(&mut self, which: usize) {
        let item = self.items[which % self.items.len()];
        let mut item = self.world.entity_mut(item);
        let mut label = item.get_mut::<Label>().expect("a label");
        label.0.string = format!("{}", label.0.string.len() + 1);
    }

    fn draws(&self) -> usize {
        self.world.resource::<Draws>().count
    }

    fn damage(&self) -> Vec<usize> {
        self.world.resource::<Draws>().damage.clone()
    }

    fn deferred(&self) -> Option<Duration> {
        self.world.resource::<FrameBudget>().deferred()
    }

    /// Waits out an outstanding frame, which is what the one-shot run loop
    /// wake does in the daemon.
    fn settle(&mut self) {
        if let Some(left) = self.deferred() {
            self.wake(left);
        }
    }
}

/// `changes` separate wakes, each changing one item, all inside one frame.
fn burst(changes: usize, capped: bool) -> Bar {
    let mut bar = Bar::new(changes.min(16), capped);
    for i in 0..changes {
        bar.touch(i);
        bar.wake(GAP);
    }
    bar
}

fn main() {
    println!("a burst of N changes, {GAP:?} apart, inside one {FRAME:?} frame\n");
    println!(
        "{:>8}  {:>10}  {:>10}  {:>14}",
        "changes", "draws was", "draws now", "damage drawn"
    );
    for changes in [1usize, 2, 5, 10, 50] {
        let mut was = burst(changes, false);
        was.settle();
        let mut now = burst(changes, true);
        now.settle();
        println!(
            "{changes:>8}  {:>10}  {:>10}  {:>14?}",
            was.draws(),
            now.draws(),
            now.damage()
        );
    }

    // The failure mode worth its own line: a change that arrives early in a
    // frame is not drawn *now*, so it had better be drawn later. Nothing
    // changes after the burst — only time passes.
    let mut bar = burst(50, true);
    let outstanding = bar.deferred().expect("a draw is waiting on the frame");
    let before = bar.draws();
    bar.wake(outstanding);
    println!(
        "\ndeferred by {outstanding:?}, then one wake with nothing new: {} draw(s), \
         carrying {:?} damaged items",
        bar.draws() - before,
        bar.damage().last().copied().unwrap_or_default()
    );
    assert_eq!(bar.draws(), before + 1, "the deferred draw was lost");
    assert_eq!(bar.deferred(), None, "and nothing is left outstanding");

    // Idle: no wake armed at all, so the run loop simply sleeps.
    let mut idle = Bar::new(4, true);
    idle.wake(FRAME);
    println!(
        "idle pass: {} draw(s), wake armed: {:?}",
        idle.draws(),
        idle.deferred()
    );
    assert_eq!(idle.deferred(), None, "an idle bar armed a wake");
}

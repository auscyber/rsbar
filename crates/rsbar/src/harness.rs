//! Driving the item world without a screen.
//!
//! The daemon's platform objects are awkward to fake, but they do not need
//! faking: a [`Cache`] is a map, [`Panels`] with no displays draws nothing, and
//! a [`Registry`] only touches a framework when a source starts. So a test gets
//! the *real* request path — the same `apply` the daemon calls — with nothing
//! behind it.
//!
//! What this deliberately does not cover is drawing, which needs a window
//! server, and the sources, which need the frameworks they wrap. Those are
//! verified by running the thing.

#![cfg(test)]

use crate::bar::{Panels, Settings};
use crate::layout::Placements;
use crate::requests::{Context, Items, ItemsRead, Outcome};
use crate::script::Job;
use crate::shaping::Cache;
use crate::sources::Registry;
use bevy_ecs::prelude::*;
use bevy_ecs::system::SystemState;
use rsbar_protocol::{Event, ItemName, Kind, Request};

/// A world with the item machinery and nothing platform-bound running.
pub struct Harness {
    world: World,
    settings: Settings,
    panels: Panels,
    cache: Cache,
    sources: Registry,
}

impl Harness {
    #[must_use]
    pub fn new() -> Self {
        let mut world = World::new();
        world.init_resource::<crate::components::Index>();
        world.init_resource::<Placements>();

        // A waker needs a run loop, and a test thread has one; nothing is ever
        // signalled because no source starts.
        let waker = crate::runloop::Waker::install(|| {});
        Self {
            world,
            settings: Settings::default(),
            panels: Panels::default(),
            cache: Cache::default(),
            sources: Registry::new(crate::config::shared(), waker),
        }
    }

    /// Applies one request through the daemon's own code path.
    pub fn apply(&mut self, request: Request) -> Outcome {
        let mut state: SystemState<Items> = SystemState::new(&mut self.world);
        let outcome = {
            let mut items = state
                .get_mut(&mut self.world)
                .expect("item params are always valid");
            let mut ctx = Context {
                settings: &mut self.settings,
                panels: &mut self.panels,
                cache: &mut self.cache,
                sources: &mut self.sources,
            };
            crate::requests::apply(request, &mut items, &mut ctx)
        };
        // Spawns and despawns are queued as commands; nothing is visible to the
        // next query until they are applied.
        state.apply(&mut self.world);
        outcome
    }

    /// The jobs an event would produce, without running any of them.
    pub fn jobs_for(&mut self, event: &Event) -> Vec<Job> {
        let mut state: SystemState<ItemsRead> = SystemState::new(&mut self.world);
        let read = state
            .get(&self.world)
            .expect("item params are always valid");
        read.jobs_for(event)
    }

    /// The items, as `--query items` would report them.
    pub fn items(&mut self) -> Vec<rsbar_protocol::ItemState> {
        let mut state: SystemState<Items> = SystemState::new(&mut self.world);
        let items = state
            .get_mut(&mut self.world)
            .expect("item params are always valid");
        items.states()
    }

    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Whether a source is registered right now.
    #[must_use]
    pub fn running(&self, id: &'static str) -> bool {
        self.sources.running(crate::sources::SourceId(id))
    }

    pub fn start_eager(&mut self) {
        self.sources.start_eager();
    }

    /// What a source is registered for right now.
    #[must_use]
    pub fn registered_for(&self, id: &'static str) -> Vec<Kind> {
        self.sources
            .registered_for(crate::sources::SourceId(id))
            .into_iter()
            .collect()
    }

    /// Convenience: add an item and set it in one go.
    pub fn add(&mut self, name: &str, position: rsbar_protocol::Position) -> ItemName {
        let name = ItemName::new(name).expect("valid name");
        self.apply(Request::AddItem {
            name: name.clone(),
            position,
        });
        name
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::Harness;
    use rsbar_protocol::event::{Forced, FrontApp, VolumeChange};
    use rsbar_protocol::{Event, ItemName, ItemPatch, Kind, Position, Request, Response};

    /// The workspace source, which `front_app_switched` is the lazy way in to.
    const WORKSPACE: &str = "workspace";

    #[test]
    fn nothing_is_running_until_an_item_wants_it() {
        let bar = Harness::new();
        assert!(
            !bar.running(WORKSPACE),
            "a bar nobody has configured observes nothing"
        );
    }

    #[test]
    fn subscribing_starts_the_source_and_removing_the_item_stops_it() {
        let mut bar = Harness::new();
        let item = bar.add("front", Position::Left);
        bar.apply(Request::Subscribe {
            name: item.clone(),
            events: vec![Kind::FrontAppSwitched],
        });
        assert!(bar.running(WORKSPACE));

        bar.apply(Request::RemoveItem(item));
        assert!(
            !bar.running(WORKSPACE),
            "the last item wanting it went, so it should have stopped"
        );
    }

    #[test]
    fn a_source_survives_one_of_two_items_losing_interest() {
        // The reference count earning itself: releasing on any unsubscribe
        // would silently stop an event the other item is still waiting for.
        let mut bar = Harness::new();
        let first = bar.add("one", Position::Left);
        let second = bar.add("two", Position::Left);
        for item in [&first, &second] {
            bar.apply(Request::Subscribe {
                name: item.clone(),
                events: vec![Kind::FrontAppSwitched],
            });
        }

        bar.apply(Request::Subscribe {
            name: first,
            events: vec![],
        });
        assert!(
            bar.running(WORKSPACE),
            "the second item still wants front_app_switched"
        );

        bar.apply(Request::Subscribe {
            name: second,
            events: vec![],
        });
        assert!(!bar.running(WORKSPACE));
    }

    #[test]
    fn subscribing_registers_that_event_and_not_the_rest_of_the_source() {
        // The workspace source can produce four events off four notifications.
        // Asking for one used to install all four.
        let mut bar = Harness::new();
        let item = bar.add("front", Position::Left);
        bar.apply(Request::Subscribe {
            name: item,
            events: vec![Kind::FrontAppSwitched],
        });
        assert_eq!(bar.registered_for(WORKSPACE), vec![Kind::FrontAppSwitched]);
    }

    #[test]
    fn a_later_subscription_widens_a_running_source() {
        // The question this design had no answer to: the source is already
        // running, so nothing used to ask it for the event just added, and it
        // only appeared to work because it had over-registered to begin with.
        let mut bar = Harness::new();
        let item = bar.add("front", Position::Left);
        bar.apply(Request::Subscribe {
            name: item.clone(),
            events: vec![Kind::FrontAppSwitched],
        });
        bar.apply(Request::Subscribe {
            name: item,
            events: vec![Kind::FrontAppSwitched, Kind::SystemWoke],
        });

        let mut registered = bar.registered_for(WORKSPACE);
        registered.sort();
        let mut expected = vec![Kind::FrontAppSwitched, Kind::SystemWoke];
        expected.sort();
        assert_eq!(registered, expected, "the added event has an observer now");
        assert!(bar.running(WORKSPACE));
    }

    #[test]
    fn narrowing_a_subscription_drops_only_what_was_dropped() {
        let mut bar = Harness::new();
        let item = bar.add("front", Position::Left);
        bar.apply(Request::Subscribe {
            name: item.clone(),
            events: vec![Kind::FrontAppSwitched, Kind::SystemWoke],
        });
        bar.apply(Request::Subscribe {
            name: item,
            events: vec![Kind::SystemWoke],
        });
        assert_eq!(bar.registered_for(WORKSPACE), vec![Kind::SystemWoke]);
        assert!(bar.running(WORKSPACE), "something still wants it");
    }

    #[test]
    fn two_items_wanting_different_events_of_one_source_both_get_them() {
        let mut bar = Harness::new();
        let first = bar.add("one", Position::Left);
        let second = bar.add("two", Position::Left);
        bar.apply(Request::Subscribe {
            name: first.clone(),
            events: vec![Kind::FrontAppSwitched],
        });
        bar.apply(Request::Subscribe {
            name: second,
            events: vec![Kind::SystemWoke],
        });
        let mut registered = bar.registered_for(WORKSPACE);
        registered.sort();
        let mut expected = vec![Kind::FrontAppSwitched, Kind::SystemWoke];
        expected.sort();
        assert_eq!(registered, expected);

        // One losing interest must not take the other's event with it.
        bar.apply(Request::Subscribe {
            name: first,
            events: vec![],
        });
        assert_eq!(bar.registered_for(WORKSPACE), vec![Kind::SystemWoke]);
    }

    #[test]
    fn an_eager_source_runs_with_nobody_subscribed() {
        let mut bar = Harness::new();
        bar.start_eager();
        assert!(
            bar.running("displays"),
            "the bar's own geometry depends on it, so it declares itself eager"
        );
    }

    fn name(s: &str) -> ItemName {
        ItemName::new(s).expect("valid name")
    }

    fn patch() -> ItemPatch {
        ItemPatch::default()
    }

    #[test]
    fn an_item_can_be_added_set_and_removed() {
        let mut bar = Harness::new();
        let clock = bar.add("clock", Position::Right);

        bar.apply(Request::SetItem {
            name: clock.clone(),
            patch: ItemPatch {
                label: Some("09:41".into()),
                ..patch()
            },
        });
        let items = bar.items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "09:41");

        bar.apply(Request::RemoveItem(clock));
        assert!(bar.items().is_empty());
    }

    #[test]
    fn naming_an_item_that_does_not_exist_is_an_error_not_a_panic() {
        let mut bar = Harness::new();
        for request in [
            Request::SetItem {
                name: name("ghost"),
                patch: patch(),
            },
            Request::RemoveItem(name("ghost")),
            Request::Subscribe {
                name: name("ghost"),
                events: vec![Kind::SystemWoke],
            },
        ] {
            assert!(
                matches!(bar.apply(request).response, Response::Error(_)),
                "a request naming a missing item must report it"
            );
        }
    }

    #[test]
    fn adding_an_existing_item_moves_it_rather_than_duplicating_it() {
        // This is what makes a config reload cheap: entity identity survives,
        // so change detection sees only what actually changed.
        let mut bar = Harness::new();
        bar.add("clock", Position::Right);
        bar.add("clock", Position::Left);

        let items = bar.items();
        assert_eq!(items.len(), 1, "the same name is the same item");
        assert_eq!(items[0].position, Position::Left, "and it moved");
    }

    #[test]
    fn only_subscribers_with_a_script_produce_a_job() {
        let mut bar = Harness::new();

        let listener = bar.add("listener", Position::Left);
        bar.apply(Request::SetItem {
            name: listener,
            patch: ItemPatch {
                script: Some("true".into()),
                ..patch()
            },
        });
        bar.apply(Request::Subscribe {
            name: name("listener"),
            events: vec![Kind::VolumeChanged],
        });

        // Subscribed, but no script: legal, and produces nothing.
        let quiet = bar.add("quiet", Position::Left);
        bar.apply(Request::Subscribe {
            name: quiet,
            events: vec![Kind::VolumeChanged],
        });

        // A script, but not subscribed to this event.
        let unrelated = bar.add("unrelated", Position::Left);
        bar.apply(Request::SetItem {
            name: unrelated,
            patch: ItemPatch {
                script: Some("true".into()),
                ..patch()
            },
        });

        let jobs = bar.jobs_for(&Event::VolumeChanged(VolumeChange { volume: 42 }));
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].item.as_str(), "listener");
    }

    #[test]
    fn a_click_script_is_not_mistaken_for_an_update_script() {
        // Regression: adding the click script to the query row shifted a
        // positional `(_, name, .., script)` destructuring, so every
        // subscription-driven script silently stopped firing and nothing
        // errored. The two must stay distinguishable.
        let mut bar = Harness::new();
        let item = bar.add("item", Position::Left);
        bar.apply(Request::SetItem {
            name: item,
            patch: ItemPatch {
                click_script: Some("clicked".into()),
                ..patch()
            },
        });
        bar.apply(Request::Subscribe {
            name: name("item"),
            events: vec![Kind::VolumeChanged],
        });

        assert!(
            bar.jobs_for(&Event::VolumeChanged(VolumeChange { volume: 1 }))
                .is_empty(),
            "a click script must not run on an unrelated event"
        );

        bar.apply(Request::SetItem {
            name: name("item"),
            patch: ItemPatch {
                script: Some("updated".into()),
                ..patch()
            },
        });
        let jobs = bar.jobs_for(&Event::VolumeChanged(VolumeChange { volume: 1 }));
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            jobs[0].script, "updated",
            "the update script, not the click one"
        );

        let state = &bar.items()[0];
        assert_eq!(state.script.as_deref(), Some("updated"));
        assert_eq!(state.click_script.as_deref(), Some("clicked"));
    }

    #[test]
    fn a_job_carries_the_event_that_caused_it() {
        let mut bar = Harness::new();
        let front = bar.add("front", Position::Left);
        bar.apply(Request::SetItem {
            name: front,
            patch: ItemPatch {
                script: Some("true".into()),
                ..patch()
            },
        });
        bar.apply(Request::Subscribe {
            name: name("front"),
            events: vec![Kind::FrontAppSwitched],
        });

        let jobs = bar.jobs_for(&Event::FrontAppSwitched(FrontApp {
            app: "Finder".into(),
        }));
        let env = jobs[0].event.env();
        assert_eq!(env["RSBAR_SENDER"], "front_app_switched");
        assert_eq!(env["RSBAR_APP"], "Finder");
    }

    #[test]
    fn subscribing_replaces_rather_than_accumulates() {
        let mut bar = Harness::new();
        let item = bar.add("item", Position::Left);
        bar.apply(Request::SetItem {
            name: item,
            patch: ItemPatch {
                script: Some("true".into()),
                ..patch()
            },
        });

        bar.apply(Request::Subscribe {
            name: name("item"),
            events: vec![Kind::VolumeChanged, Kind::SystemWoke],
        });
        bar.apply(Request::Subscribe {
            name: name("item"),
            events: vec![Kind::SystemWoke],
        });

        assert!(
            bar.jobs_for(&Event::VolumeChanged(VolumeChange { volume: 1 }))
                .is_empty(),
            "the earlier subscription is gone, not merged"
        );
        assert_eq!(bar.items()[0].events, vec![Kind::SystemWoke]);
    }

    #[test]
    fn update_all_runs_every_script_regardless_of_subscription() {
        let mut bar = Harness::new();
        for name in ["a", "b"] {
            let item = bar.add(name, Position::Left);
            bar.apply(Request::SetItem {
                name: item,
                patch: ItemPatch {
                    script: Some("true".into()),
                    ..patch()
                },
            });
        }
        bar.add("no-script", Position::Left);

        let outcome = bar.apply(Request::UpdateAll);
        assert_eq!(outcome.jobs.len(), 2, "only the items that have a script");
        assert!(
            outcome
                .jobs
                .iter()
                .all(|job| matches!(job.event, Event::Forced(Forced {})))
        );
    }

    #[test]
    fn a_trigger_reaches_the_items_subscribed_to_it_by_name() {
        let mut bar = Harness::new();
        let mine = bar.add("mine", Position::Left);
        bar.apply(Request::SetItem {
            name: mine,
            patch: ItemPatch {
                script: Some("true".into()),
                ..patch()
            },
        });
        bar.apply(Request::Subscribe {
            name: name("mine"),
            events: vec![Kind::Custom("my.event".into())],
        });

        let fired = bar.apply(Request::Trigger(
            Kind::Custom("my.event".into()).into_event(),
        ));
        assert_eq!(fired.jobs.len(), 1);

        let other = bar.apply(Request::Trigger(
            Kind::Custom("other.event".into()).into_event(),
        ));
        assert!(other.jobs.is_empty(), "a custom event matches by name");
    }

    #[test]
    fn the_bar_reports_what_it_was_set_to() {
        let mut bar = Harness::new();
        bar.apply(Request::SetBar(rsbar_protocol::BarPatch {
            height: Some(40.0),
            color: Some(0xff00_0000),
            ..rsbar_protocol::BarPatch::default()
        }));
        assert!((bar.settings().height - 40.0).abs() < f64::EPSILON);

        let Response::Bar(state) = bar
            .apply(Request::Query(rsbar_protocol::Query::Bar))
            .response
        else {
            panic!("querying the bar returns the bar");
        };
        assert!((state.height - 40.0).abs() < f64::EPSILON);
    }

    #[test]
    fn shutdown_asks_to_exit_without_touching_the_items() {
        let mut bar = Harness::new();
        bar.add("clock", Position::Left);
        let outcome = bar.apply(Request::Shutdown);
        assert!(outcome.exit);
        assert_eq!(bar.items().len(), 1);
    }
}

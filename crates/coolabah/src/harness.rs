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
use crate::ecs::ActiveSpaces;
use crate::layout::Placements;
use crate::protocol::event::SpaceChange;
use crate::protocol::{Event, ItemName, Kind, Request};
use crate::requests::{Context, Items, ItemsRead, Outcome};
use crate::script::Job;
use crate::shaping::Cache;
use crate::sources::Registry;
use bevy_ecs::prelude::*;
use bevy_ecs::system::RunSystemOnce as _;
use bevy_ecs::system::SystemState;
use std::num::NonZeroU64;

/// What [`crate::ecs::recompute_space_selection`] needs from the world.
type SpaceSelectionQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static crate::components::AssociatedSpace,
        &'static mut crate::components::Selected,
    ),
>;

/// A world with the item machinery and nothing platform-bound running.
pub struct Harness {
    world: World,
    settings: Settings,
    panels: Panels,
    cache: Cache,
    captures: crate::alias::Captures,
    sources: Registry,
    subscribers: crate::subscribers::Subscribers,
}

impl Harness {
    #[must_use]
    pub fn new() -> Self {
        let mut world = World::new();
        world.init_resource::<crate::components::Index>();
        world.init_resource::<Placements>();
        world.init_resource::<ActiveSpaces>();

        // `Panels::default()` never makes a real window, so nothing else here
        // would establish the connection before `apply` first acquires it.
        let _ = skylight::establish(crate::runloop::main_thread());

        // A waker needs a run loop, and a test thread has one; nothing is ever
        // signalled because no source starts.
        let waker = crate::runloop::Waker::install(crate::runloop::main_thread(), || {});
        Self {
            world,
            settings: Settings::default(),
            panels: Panels::default(),
            cache: Cache::default(),
            captures: crate::alias::Captures::default(),
            sources: Registry::new(
                crate::runloop::main_thread(),
                crate::config::shared(),
                waker,
            ),
            subscribers: crate::subscribers::Subscribers::default(),
        }
    }

    /// Applies one request through the daemon's own code path.
    pub fn apply(&mut self, request: Request) -> Outcome {
        // Lifted out and put back rather than held beside the world: the
        // daemon keeps this as a resource, and `apply_defaults` below reads it
        // from there, so a copy on the side would drift from what runs.
        let mut defaults = self
            .world
            .remove_resource::<crate::requests::Defaults>()
            .unwrap_or_default();
        let mut state: SystemState<Items> = SystemState::new(&mut self.world);
        // Blocking rather than awaited: a test thread pumps no run loop for a
        // wait to stall, and nothing else in a test contends this connection.
        let connected = crate::pool::handle().block_on(skylight::acquire());
        let outcome = {
            let mut items = state
                .get_mut(&mut self.world)
                .expect("item params are always valid");
            let mut ctx = Context {
                main: crate::runloop::main_thread(),
                connected: &connected,
                settings: &mut self.settings,
                panels: &mut self.panels,
                cache: &mut self.cache,
                captures: &mut self.captures,
                sources: &mut self.sources,
                subscribers: &mut self.subscribers,
                subscriber: None,
                defaults: &mut defaults,
            };
            crate::requests::apply(request, &mut items, &mut ctx)
        };
        // Spawns and despawns are queued as commands; nothing is visible to the
        // next query until they are applied.
        state.apply(&mut self.world);
        self.world.insert_resource(defaults);
        // What the daemon runs straight after `apply_requests`, so a
        // `--default` set here reaches an item added here.
        self.world
            .run_system_once(crate::requests::apply_defaults)
            .expect("applying defaults is always valid");
        // What the daemon does in `Last`: claims taken and dropped by the
        // request are only counts until something registers against them.
        self.sources.settle();
        outcome
    }

    /// The jobs an event would produce, without running any of them.
    pub fn jobs_for(&mut self, event: &Event) -> Vec<Job> {
        let mut state: SystemState<ItemsRead> = SystemState::new(&mut self.world);
        let read = state
            .get(&self.world)
            .expect("item params are always valid");
        let dependents = self.sources.dependents(event, &crate::sources::Target::All);
        read.jobs_for(&std::sync::Arc::new(event.clone()), &dependents)
    }

    /// The items, as `--query items` would report them.
    pub fn items(&mut self) -> Vec<crate::protocol::ItemState> {
        let mut state: SystemState<Items> = SystemState::new(&mut self.world);
        let items = state
            .get_mut(&mut self.world)
            .expect("item params are always valid");
        items.states()
    }

    /// Every item's name, left to right.
    pub fn order(&mut self) -> Vec<String> {
        self.items()
            .into_iter()
            .map(|item| item.name.to_string())
            .collect()
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

    /// Runs the end-of-frame pass again, without a request to hang it off.
    ///
    /// A source whose last claim went is marked rather than torn down, and
    /// comes down on the pass after — see `sources::Liveness` for why. So a
    /// test that wants "and now it has really stopped" asks for the next
    /// frame, and one that wants "it is still there to be claimed again"
    /// does not.
    pub fn next_frame(&mut self) -> Vec<Job> {
        self.sources.settle();
        self.deliver()
    }

    /// The delivery half of a frame: everything the registry has queued,
    /// pushed to whatever claimed it.
    ///
    /// The same three steps `ecs::dispatch_events` runs, and the reason a
    /// `--trigger` is not visible in the [`Outcome`] any more: the request
    /// only emits, and the drain is what delivers.
    fn deliver(&mut self) -> Vec<Job> {
        let mut queued: Vec<std::sync::Arc<Event>> = Vec::new();
        self.sources
            .drain(|_, event| queued.push(std::sync::Arc::new(event)));

        let mut jobs = Vec::new();
        for event in queued {
            let mut dependents = self
                .sources
                .dependents(&event, &crate::sources::Target::All);
            // A client holding a port takes the event itself, and only the
            // rest fall back to a script.
            dependents.retain(|item| !self.subscribers.push(*item, &event));
            let mut state: SystemState<ItemsRead> = SystemState::new(&mut self.world);
            let read = state
                .get(&self.world)
                .expect("item params are always valid");
            jobs.extend(read.jobs_for(&event, &dependents));
        }
        jobs
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
    pub fn add(&mut self, name: &str, position: crate::protocol::Position) -> ItemName {
        let name = ItemName::new(name).expect("valid name");
        self.apply(Request::Add(crate::protocol::ComponentKind::Item {
            name: name.clone(),
            position,
        }));
        name
    }

    /// Pushes one sample onto a graph, through `requests::push` — standing in
    /// for `--push` until the protocol carries a request for it.
    pub fn push(&mut self, name: &ItemName, value: f32) -> Outcome {
        let mut state: SystemState<Items> = SystemState::new(&mut self.world);
        let outcome = {
            let mut items = state
                .get_mut(&mut self.world)
                .expect("item params are always valid");
            crate::requests::push(name, value, &mut items)
        };
        state.apply(&mut self.world);
        outcome
    }

    /// Sets a space item's associated space directly, standing in for
    /// `associated_space` as an ordinary `--set` property until the protocol
    /// carries a field for it.
    pub fn set_associated_space(&mut self, name: &ItemName, space: NonZeroU64) {
        let entity = self
            .world
            .resource::<crate::components::Index>()
            .get(name)
            .expect("item exists");
        self.world
            .get_mut::<crate::components::AssociatedSpace>(entity)
            .expect("a space item")
            .0 = Some(space);
    }

    /// Feeds a `space_changed` event through the same recomputation
    /// `ecs::update_space_selection` runs on the real event bus.
    pub fn space_changed(&mut self, display: u32, space: NonZeroU64) {
        let event = Event::SpaceChanged(SpaceChange {
            display,
            space: space.get(),
        });
        let mut state: SystemState<(ResMut<ActiveSpaces>, SpaceSelectionQuery)> =
            SystemState::new(&mut self.world);
        let (mut active, mut spaces) = state
            .get_mut(&mut self.world)
            .expect("item params are always valid");
        crate::ecs::recompute_space_selection(&event, &mut active, &mut spaces);
        state.apply(&mut self.world);
    }

    /// Drops every component's changed-since-last-check mark, so a test can
    /// tell whether the *next* operation touched one.
    pub fn clear_trackers(&mut self) {
        self.world.clear_trackers();
    }

    /// Whether `T` was written on the named item since the last
    /// [`Self::clear_trackers`] — the cheapest proof that a no-op write did
    /// not mark a component changed, and so would not have repainted it.
    pub fn changed<T: Component>(&mut self, name: &ItemName) -> bool {
        let entity = self
            .world
            .resource::<crate::components::Index>()
            .get(name)
            .expect("item exists");
        self.world
            .query::<Ref<T>>()
            .get(&self.world, entity)
            .is_ok_and(|value| value.is_changed())
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
    use crate::protocol::event::{Forced, FrontApp, VolumeChange};
    use crate::protocol::{
        ComponentKind, Event, ItemName, ItemPatch, Kind, Position, Relative, Request, Response,
        Selector,
    };
    use std::num::NonZeroU64;

    /// The notification source, which `front_app_switched` is the lazy way in
    /// to.
    const NOTIFICATIONS: &str = "notifications";

    #[test]
    fn nothing_is_running_until_an_item_wants_it() {
        let bar = Harness::new();
        assert!(
            !bar.running(NOTIFICATIONS),
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
        assert!(bar.running(NOTIFICATIONS));

        bar.apply(Request::Remove(Selector::Name(item)));
        assert!(
            bar.running(NOTIFICATIONS),
            "the pass its last claim went in only marks it"
        );

        bar.next_frame();
        assert!(
            !bar.running(NOTIFICATIONS),
            "the last item wanting it went, so it should have stopped"
        );
    }

    #[test]
    fn a_claim_retaken_before_the_sweep_keeps_the_registration() {
        // What the stale pass is for: an item that moves gives up the claim
        // keyed to where it was and takes one keyed to where it is now, and
        // the two need not land in the same pass. Tearing the source down in
        // between and building it again is expensive and, for a Carbon
        // handler or a `CoreAudio` listener, racy.
        let mut bar = Harness::new();
        let item = bar.add("front", Position::Left);
        bar.apply(Request::Subscribe {
            name: item.clone(),
            events: vec![Kind::FrontAppSwitched],
        });

        // Everything lets go of it, and a pass runs.
        bar.apply(Request::Subscribe {
            name: item.clone(),
            events: vec![],
        });
        assert!(bar.running(NOTIFICATIONS), "marked, not stopped");

        // Something wants it again before the sweep.
        bar.apply(Request::Subscribe {
            name: item,
            events: vec![Kind::FrontAppSwitched],
        });
        bar.next_frame();
        assert!(
            bar.running(NOTIFICATIONS),
            "the claim came back in time, so the registration was never taken down"
        );
        assert_eq!(
            bar.registered_for(NOTIFICATIONS),
            vec![Kind::FrontAppSwitched]
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
            bar.running(NOTIFICATIONS),
            "the second item still wants front_app_switched"
        );

        bar.apply(Request::Subscribe {
            name: second,
            events: vec![],
        });
        bar.next_frame();
        assert!(!bar.running(NOTIFICATIONS));
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
        assert_eq!(
            bar.registered_for(NOTIFICATIONS),
            vec![Kind::FrontAppSwitched]
        );
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

        let mut registered = bar.registered_for(NOTIFICATIONS);
        registered.sort();
        let mut expected = vec![Kind::FrontAppSwitched, Kind::SystemWoke];
        expected.sort();
        assert_eq!(registered, expected, "the added event has an observer now");
        assert!(bar.running(NOTIFICATIONS));
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
        assert_eq!(bar.registered_for(NOTIFICATIONS), vec![Kind::SystemWoke]);
        assert!(bar.running(NOTIFICATIONS), "something still wants it");
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
        let mut registered = bar.registered_for(NOTIFICATIONS);
        registered.sort();
        let mut expected = vec![Kind::FrontAppSwitched, Kind::SystemWoke];
        expected.sort();
        assert_eq!(registered, expected);

        // One losing interest must not take the other's event with it.
        bar.apply(Request::Subscribe {
            name: first,
            events: vec![],
        });
        assert_eq!(bar.registered_for(NOTIFICATIONS), vec![Kind::SystemWoke]);
    }

    #[test]
    fn two_items_wanting_the_same_event_share_one_observer() {
        // Both claims refer to the same underlying thing. The first release
        // must not take it away from the item still holding one — and the
        // second must, or it observes forever.
        let mut bar = Harness::new();
        let first = bar.add("one", Position::Left);
        let second = bar.add("two", Position::Left);
        for item in [&first, &second] {
            bar.apply(Request::Subscribe {
                name: item.clone(),
                events: vec![Kind::FrontAppSwitched],
            });
        }
        assert_eq!(
            bar.registered_for(NOTIFICATIONS),
            vec![Kind::FrontAppSwitched]
        );

        bar.apply(Request::Remove(Selector::Name(first)));
        assert_eq!(
            bar.registered_for(NOTIFICATIONS),
            vec![Kind::FrontAppSwitched],
            "the other item still holds a claim on it"
        );

        bar.apply(Request::Remove(Selector::Name(second)));
        bar.next_frame();
        assert!(bar.registered_for(NOTIFICATIONS).is_empty());
        assert!(!bar.running(NOTIFICATIONS));
    }

    #[test]
    fn a_sources_events_come_and_go_with_the_items_that_want_them() {
        // One source, two items, two different events off it, each arriving
        // and leaving independently.
        let mut bar = Harness::new();

        // An item asks for the front application. The source is created and
        // serves exactly that.
        let front = bar.add("front", Position::Left);
        bar.apply(Request::Subscribe {
            name: front.clone(),
            events: vec![Kind::FrontAppSwitched],
        });
        assert_eq!(
            bar.registered_for(NOTIFICATIONS),
            vec![Kind::FrontAppSwitched]
        );

        // Another item asks about waking. The source already exists, so it is
        // widened rather than rebuilt.
        let sleep = bar.add("sleep", Position::Left);
        bar.apply(Request::Subscribe {
            name: sleep.clone(),
            events: vec![Kind::SystemWoke],
        });
        let mut both = bar.registered_for(NOTIFICATIONS);
        both.sort();
        let mut expected = vec![Kind::FrontAppSwitched, Kind::SystemWoke];
        expected.sort();
        assert_eq!(both, expected);

        // The first item goes. Nothing wants the front application any more,
        // so the source stops observing it — and keeps observing wakes.
        bar.apply(Request::Remove(Selector::Name(front)));
        assert_eq!(bar.registered_for(NOTIFICATIONS), vec![Kind::SystemWoke]);
        assert!(bar.running(NOTIFICATIONS));

        // The second goes too. Nothing wants anything off it, so it stops —
        // on the pass after the one that found it unwanted.
        bar.apply(Request::Remove(Selector::Name(sleep)));
        bar.next_frame();
        assert!(bar.registered_for(NOTIFICATIONS).is_empty());
        assert!(!bar.running(NOTIFICATIONS));
    }

    #[test]
    fn ending_a_config_drops_only_what_it_left_out() {
        let mut bar = Harness::new();
        let kept = bar.add("kept", Position::Left);
        let dropped = bar.add("dropped", Position::Left);

        bar.apply(Request::BeginConfig);
        // The new config mentions one of them and not the other.
        bar.apply(Request::Set(
            Selector::Name(kept.clone()),
            Box::new(ItemPatch {
                label: Some("still here".into()),
                ..patch()
            }),
        ));
        bar.apply(Request::EndConfig);

        let names: Vec<String> = bar
            .items()
            .into_iter()
            .map(|item| item.name.to_string())
            .collect();
        assert_eq!(names, vec![kept.to_string()]);
        assert!(!names.contains(&dropped.to_string()));
    }

    #[test]
    fn adding_items_without_a_config_block_drops_nothing() {
        // Incremental use — a client adding one item at a time — must not
        // sweep. Only what is between begin and end is a config.
        let mut bar = Harness::new();
        bar.add("first", Position::Left);
        bar.add("second", Position::Left);
        assert_eq!(bar.items().len(), 2);
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

        bar.apply(Request::Set(
            Selector::Name(clock.clone()),
            Box::new(ItemPatch {
                label: Some("09:41".into()),
                ..patch()
            }),
        ));
        let items = bar.items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label.text, "09:41");

        bar.apply(Request::Remove(Selector::Name(clock)));
        assert!(bar.items().is_empty());
    }

    #[test]
    fn naming_an_item_that_does_not_exist_is_an_error_not_a_panic() {
        let mut bar = Harness::new();
        for request in [
            Request::Set(Selector::Name(name("ghost")), Box::new(patch())),
            Request::Remove(Selector::Name(name("ghost"))),
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
        assert_eq!(items[0].geometry.position, Position::Left, "and it moved");
    }

    #[test]
    fn a_bracket_takes_the_items_its_pattern_names() {
        // ~/dendritic/sketchybar/items/menus.lua:
        // sbar.add("bracket", { "/menu\\..*/" }, { ... })
        let mut bar = Harness::new();
        for name in ["menu.1", "menu.2", "clock"] {
            bar.add(name, Position::Left);
        }
        let group = bar.add("group", Position::Left);

        bar.apply(Request::Set(
            Selector::Name(group.clone()),
            Box::new(ItemPatch {
                members: Some(vec![Selector::Pattern(r"menu\..*".into())]),
                ..Default::default()
            }),
        ));

        let members: Vec<String> = bar
            .items()
            .into_iter()
            .find(|item| item.name == group)
            .map(|item| item.members.iter().map(ToString::to_string).collect())
            .unwrap();
        let mut members = members;
        members.sort();
        assert_eq!(members, ["menu.1", "menu.2"]);
    }

    #[test]
    fn a_pattern_selects_by_name_and_leaves_everything_else_alone() {
        // What the real config does constantly: sbar.set("/menu\\..*/", ...).
        // Resolved against the daemon's own live list, because a client
        // resolving it would race a config still adding the items.
        let mut bar = Harness::new();
        for name in ["menu.1", "menu.2", "clock"] {
            bar.add(name, Position::Left);
        }

        bar.apply(Request::Set(
            Selector::Pattern(r"menu\..*".into()),
            Box::new(ItemPatch {
                geometry: Some(crate::protocol::GeometryPatch {
                    drawing: Some(crate::protocol::BoolChange::False),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        ));

        for item in bar.items() {
            let hidden = !item.geometry.drawing.get();
            assert_eq!(
                hidden,
                item.name.as_str().starts_with("menu."),
                "{}",
                item.name
            );
        }
    }

    #[test]
    fn removing_by_pattern_removes_only_what_it_names() {
        let mut bar = Harness::new();
        for name in ["space.1", "space.2", "clock"] {
            bar.add(name, Position::Left);
        }

        bar.apply(Request::Remove(Selector::Pattern(r"space\..*".into())));

        let left: Vec<_> = bar.items().into_iter().map(|item| item.name).collect();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].as_str(), "clock");
    }

    #[test]
    fn a_default_reaches_an_item_added_after_it_and_not_one_before() {
        // SketchyBar's own --default: properties copied onto every item added
        // from then on, never retroactively.
        let mut bar = Harness::new();
        bar.add("before", Position::Left);

        bar.apply(Request::SetDefault(Box::new(ItemPatch {
            label: Some("filled in".into()),
            ..Default::default()
        })));
        bar.add("after", Position::Left);

        let items = bar.items();
        let labelled = |name: &str| {
            items
                .iter()
                .find(|item| item.name.as_str() == name)
                .map(|item| item.label.text.clone())
                .unwrap()
        };
        assert_eq!(labelled("after"), "filled in");
        assert_eq!(labelled("before"), "", "defaults are not retroactive");
    }

    #[test]
    fn moving_an_item_puts_it_where_it_was_asked_for() {
        // What items/left.lua does on every app switch:
        // `sketchybar --move chevron after <last space>`.
        let mut bar = Harness::new();
        for name in ["space.1", "space.2", "chevron", "front_app"] {
            bar.add(name, Position::Left);
        }

        bar.apply(Request::Move {
            name: ItemName::new("chevron").unwrap(),
            relative: Relative::After,
            reference: ItemName::new("space.1").unwrap(),
        });
        assert_eq!(bar.order(), ["space.1", "chevron", "space.2", "front_app"]);

        bar.apply(Request::Move {
            name: ItemName::new("front_app").unwrap(),
            relative: Relative::Before,
            reference: ItemName::new("space.1").unwrap(),
        });
        assert_eq!(bar.order(), ["front_app", "space.1", "chevron", "space.2"]);
    }

    #[test]
    fn reordering_keeps_whatever_it_did_not_name() {
        let mut bar = Harness::new();
        for name in ["a", "b", "c"] {
            bar.add(name, Position::Left);
        }

        bar.apply(Request::Reorder(vec![
            ItemName::new("c").unwrap(),
            ItemName::new("a").unwrap(),
        ]));

        // `b` was not named, so it keeps its place after the ones that were
        // rather than landing somewhere arbitrary.
        assert_eq!(bar.order(), ["c", "a", "b"]);
    }

    #[test]
    fn only_subscribers_with_a_script_produce_a_job() {
        let mut bar = Harness::new();

        let listener = bar.add("listener", Position::Left);
        bar.apply(Request::Set(
            Selector::Name(listener),
            Box::new(ItemPatch {
                scripting: Some(crate::protocol::ScriptingPatch {
                    script: Some("true".into()),
                    ..Default::default()
                }),
                ..patch()
            }),
        ));
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
        bar.apply(Request::Set(
            Selector::Name(unrelated),
            Box::new(ItemPatch {
                scripting: Some(crate::protocol::ScriptingPatch {
                    script: Some("true".into()),
                    ..Default::default()
                }),
                ..patch()
            }),
        ));

        let jobs = bar.jobs_for(&Event::VolumeChanged(VolumeChange { volume: 42 }));
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].item.name().as_str(), "listener");
    }

    #[test]
    fn a_click_script_is_not_mistaken_for_an_update_script() {
        // Regression: adding the click script to the query row shifted a
        // positional `(_, name, .., script)` destructuring, so every
        // subscription-driven script silently stopped firing and nothing
        // errored. The two must stay distinguishable.
        let mut bar = Harness::new();
        let item = bar.add("item", Position::Left);
        bar.apply(Request::Set(
            Selector::Name(item),
            Box::new(ItemPatch {
                scripting: Some(crate::protocol::ScriptingPatch {
                    click_script: Some("clicked".into()),
                    ..Default::default()
                }),
                ..patch()
            }),
        ));
        bar.apply(Request::Subscribe {
            name: name("item"),
            events: vec![Kind::VolumeChanged],
        });

        assert!(
            bar.jobs_for(&Event::VolumeChanged(VolumeChange { volume: 1 }))
                .is_empty(),
            "a click script must not run on an unrelated event"
        );

        bar.apply(Request::Set(
            Selector::Name(name("item")),
            Box::new(ItemPatch {
                scripting: Some(crate::protocol::ScriptingPatch {
                    script: Some("updated".into()),
                    ..Default::default()
                }),
                ..patch()
            }),
        ));
        let jobs = bar.jobs_for(&Event::VolumeChanged(VolumeChange { volume: 1 }));
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            jobs[0].script.as_ref(),
            "updated",
            "the update script, not the click one"
        );

        let state = &bar.items()[0];
        assert_eq!(state.scripting.script.as_deref(), Some("updated"));
        assert_eq!(state.scripting.click_script.as_deref(), Some("clicked"));
    }

    #[test]
    fn a_job_carries_the_event_that_caused_it() {
        let mut bar = Harness::new();
        let front = bar.add("front", Position::Left);
        bar.apply(Request::Set(
            Selector::Name(front),
            Box::new(ItemPatch {
                scripting: Some(crate::protocol::ScriptingPatch {
                    script: Some("true".into()),
                    ..Default::default()
                }),
                ..patch()
            }),
        ));
        bar.apply(Request::Subscribe {
            name: name("front"),
            events: vec![Kind::FrontAppSwitched],
        });

        let jobs = bar.jobs_for(&Event::FrontAppSwitched(FrontApp {
            app: "Finder".into(),
        }));
        let env = jobs[0].event.env();
        assert_eq!(env["SENDER"], "front_app_switched");
        assert_eq!(env["APP"], "Finder");
    }

    #[test]
    fn subscribing_replaces_rather_than_accumulates() {
        let mut bar = Harness::new();
        let item = bar.add("item", Position::Left);
        bar.apply(Request::Set(
            Selector::Name(item),
            Box::new(ItemPatch {
                scripting: Some(crate::protocol::ScriptingPatch {
                    script: Some("true".into()),
                    ..Default::default()
                }),
                ..patch()
            }),
        ));

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
            bar.apply(Request::Set(
                Selector::Name(item),
                Box::new(ItemPatch {
                    scripting: Some(crate::protocol::ScriptingPatch {
                        script: Some("true".into()),
                        ..Default::default()
                    }),
                    ..patch()
                }),
            ));
        }
        bar.add("no-script", Position::Left);

        let outcome = bar.apply(Request::UpdateAll);
        assert_eq!(outcome.jobs.len(), 2, "only the items that have a script");
        assert!(
            outcome
                .jobs
                .iter()
                .all(|job| matches!(*job.event, Event::Forced(Forced {})))
        );
    }

    #[test]
    fn a_trigger_reaches_the_items_subscribed_to_it_by_name() {
        let mut bar = Harness::new();
        let mine = bar.add("mine", Position::Left);
        bar.apply(Request::Set(
            Selector::Name(mine),
            Box::new(ItemPatch {
                scripting: Some(crate::protocol::ScriptingPatch {
                    script: Some("true".into()),
                    ..Default::default()
                }),
                ..patch()
            }),
        ));
        bar.apply(Request::Subscribe {
            name: name("mine"),
            events: vec![Kind::Custom("my.event".into())],
        });

        // The request only emits; the drain is what delivers. So the jobs
        // arrive on the next frame rather than in the outcome.
        let fired = bar.apply(Request::Trigger(
            Kind::Custom("my.event".into()).into_event(),
        ));
        assert!(
            fired.jobs.is_empty(),
            "triggering queues, it does not deliver"
        );
        assert_eq!(bar.next_frame().len(), 1);

        bar.apply(Request::Trigger(
            Kind::Custom("other.event".into()).into_event(),
        ));
        assert!(
            bar.next_frame().is_empty(),
            "a custom event matches by name"
        );
    }

    #[test]
    fn the_bar_reports_what_it_was_set_to() {
        let mut bar = Harness::new();
        bar.apply(Request::SetBar(crate::protocol::BarPatch {
            height: Some(40.0),
            color: Some(crate::protocol::Color(0xff00_0000)),
            ..crate::protocol::BarPatch::default()
        }));
        assert!((bar.settings().height - 40.0).abs() < f64::EPSILON);

        let Response::Bar(state) = bar
            .apply(Request::Query(crate::protocol::Query::Bar))
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

    fn space(bar: &mut Harness, item: &str) -> ItemName {
        let name = name(item);
        bar.apply(Request::Add(ComponentKind::Space {
            name: name.clone(),
            position: Position::Left,
        }));
        name
    }

    #[test]
    fn re_pushing_a_graphs_current_value_changes_nothing() {
        let mut bar = Harness::new();
        bar.apply(Request::Add(ComponentKind::Graph {
            name: name("cpu"),
            position: Position::Right,
        }));
        let cpu = name("cpu");

        assert_eq!(bar.push(&cpu, 1.0).response, Response::Ok);
        bar.clear_trackers();

        assert_eq!(
            bar.push(&cpu, 1.0).response,
            Response::Ok,
            "re-pushing the same reading is not an error"
        );
        assert!(
            !bar.changed::<crate::components::Graph>(&cpu),
            "an unchanged reading must not mark the graph changed"
        );

        bar.push(&cpu, 2.0);
        assert!(
            bar.changed::<crate::components::Graph>(&cpu),
            "a moved reading must"
        );
    }

    #[test]
    fn pushing_onto_something_that_is_not_a_graph_is_an_error() {
        let mut bar = Harness::new();
        let clock = bar.add("clock", Position::Left);
        assert!(matches!(bar.push(&clock, 1.0).response, Response::Error(_)));
    }

    #[test]
    fn a_space_change_selects_the_item_associated_with_it_and_deselects_the_rest() {
        // Models `bar_manager_update_space_components`: selection follows
        // whichever space a display is now showing.
        let mut bar = Harness::new();
        let one = space(&mut bar, "space.1");
        let two = space(&mut bar, "space.2");
        bar.set_associated_space(&one, NonZeroU64::new(1).unwrap());
        bar.set_associated_space(&two, NonZeroU64::new(2).unwrap());

        // `ItemState` does not report `Selected` yet — a protocol gap, not
        // this pass's — so the recomputation is checked directly through
        // `Harness::changed`, which is exactly the damage-tracking question
        // this pass is about.
        bar.space_changed(1, NonZeroU64::new(1).unwrap());
        bar.clear_trackers();
        bar.space_changed(1, NonZeroU64::new(1).unwrap());
        assert!(
            !bar.changed::<crate::components::Selected>(&one),
            "the same space changing again must not re-mark an already-selected item"
        );
        assert!(
            !bar.changed::<crate::components::Selected>(&two),
            "nor an already-deselected one"
        );

        bar.space_changed(1, NonZeroU64::new(2).unwrap());
        assert!(
            bar.changed::<crate::components::Selected>(&one),
            "space.1 lost the display it had"
        );
        assert!(
            bar.changed::<crate::components::Selected>(&two),
            "space.2 gained it"
        );
    }

    #[test]
    fn a_space_stays_selected_while_another_display_still_shows_it() {
        // Two displays can each be on their own space; a space item is
        // selected if *any* display currently shows it, the same as
        // `bar_manager_update_space_components` checking every bar in turn.
        let mut bar = Harness::new();
        let one = space(&mut bar, "space.1");
        bar.set_associated_space(&one, NonZeroU64::new(1).unwrap());

        bar.space_changed(1, NonZeroU64::new(1).unwrap());
        bar.space_changed(2, NonZeroU64::new(9).unwrap());
        bar.clear_trackers();

        // Display 1 moves off space 1, but display 2 never showed it, so
        // nothing here actually changes... except it does: space 1 is no
        // longer shown anywhere, so it must be deselected.
        bar.space_changed(1, NonZeroU64::new(3).unwrap());
        assert!(bar.changed::<crate::components::Selected>(&one));

        // Reselect it on display 1, then move display 2 elsewhere: display 1
        // still shows it, so nothing should change.
        bar.space_changed(1, NonZeroU64::new(1).unwrap());
        bar.clear_trackers();
        bar.space_changed(2, NonZeroU64::new(4).unwrap());
        assert!(
            !bar.changed::<crate::components::Selected>(&one),
            "still shown on display 1"
        );
    }
}

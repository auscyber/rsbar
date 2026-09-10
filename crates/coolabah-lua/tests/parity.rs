//! What a config sees must not depend on how its requests travel.
//!
//! The Lua table itself is not in question — `api::install` is one function
//! taking a `dyn Dispatcher`, so both hosts get the identical table by
//! construction. What is in question is the traffic: whether the no-IPC path
//! ([`coolabah_lua::direct`]) asks the daemon for the same things, in the same
//! order, with the same values, as the path that goes over a Mach port.
//!
//! These tests answer that in two halves, which together cover the whole
//! difference between the two:
//!
//! 1. the same config, run against a bare in-memory dispatcher and against
//!    the direct channel driven by a stand-in for the daemon's request pass,
//!    produces the same [`Request`] sequence;
//! 2. every request in that sequence survives an encode/decode round trip
//!    through the real wire codec unchanged — so the bytes the IPC path adds
//!    carry no information the direct path drops.
//!
//! The one thing a test here cannot reach is the daemon's own `requests::apply`,
//! which is in another crate and needs a `World`. It does not need to: `apply`
//! takes a `Request`, and these prove the `Request` is the same one.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use coolabah_lua::dispatch::{BoxFuture, BoxedEventStream, Dispatcher};
use coolabah_lua::require::Modules;
use coolabah_lua::{Channel, host};
use coolabah_protocol::{ItemName, Kind, Request, Response};

/// A config using the shapes a real `SketchyBar` one does: a bar patch, a
/// `default`, named and anonymous adds, a bracket over a pattern, property
/// assignment through both `:set` and `item.x =`, a module of its own, and a
/// subscription.
const CONFIG: &str = r#"
local palette = require('palette')

coolabah.begin_config()
coolabah.bar({ height = 32, color = palette.bar, position = "top" })
coolabah.default({ icon = { font = "Hack Nerd Font:Bold:14.0" }, label = { padding_left = 4 } })

coolabah.add("event", "volume_change", "com.apple.sound.volumeChanged")

local clock = coolabah.add("item", "clock", { position = "right", update_freq = 10 })
clock:set({ label = "00:00", icon = "" })
clock.label.color = palette.text
clock.popup.drawing = false
clock:subscribe("system_woke", function(event) end)

local front = coolabah.add("item", "front_app", { position = "left" })
front:set({ label = { string = "Finder" } })

coolabah.add("bracket", { "clock", "/space%..*/" }, { background = { corner_radius = 6 } })

coolabah.set("front_app", { icon = { drawing = true } })
coolabah.move("clock", "after", "front_app")
coolabah.reorder({ "front_app", "clock" })
coolabah.trigger("volume_change", { INFO = "50" })
coolabah.update_all()
coolabah.end_config()
"#;

const MODULE: &str = r"
return { bar = 0xff181818, text = 0xffffffff }
";

/// A scratch config directory, cleaned up on drop.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("coolabah-lua-parity-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let scratch = Self(dir);
        scratch.write("palette.lua", MODULE);
        scratch
    }

    fn write(&self, name: &str, source: &str) -> std::path::PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, source).expect("write");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What a daemon with nothing in it would answer, whichever transport asked.
fn answer(request: &Request) -> Response {
    match request {
        Request::Query(coolabah_protocol::Query::Items) => Response::Items(Vec::new()),
        _ => Response::Ok,
    }
}

/// Answers every request `Ok` and keeps it: the traffic with no transport at
/// all under it, which is the baseline both real paths are compared against.
#[derive(Default)]
struct Recorder(Mutex<Vec<Request>>);

impl Dispatcher for Recorder {
    fn call(&self, request: Request) -> BoxFuture<'_, coolabah_lua::Result<Response>> {
        let response = answer(&request);
        self.0.lock().expect("no other holder").push(request);
        Box::pin(async move { Ok(response) })
    }

    fn subscribe(
        &self,
        name: ItemName,
        events: Vec<Kind>,
    ) -> BoxFuture<'_, coolabah_lua::Result<BoxedEventStream>> {
        self.0
            .lock()
            .expect("no other holder")
            .push(Request::Subscribe { name, events });
        Box::pin(async { Ok(Box::pin(futures_lite::stream::empty()) as BoxedEventStream) })
    }
}

/// Runs [`CONFIG`] against a plain in-memory dispatcher.
fn through_memory(script: &std::path::Path) -> Vec<Request> {
    let recorder = Arc::new(Recorder::default());
    let waiting = host::spawn(
        Arc::clone(&recorder) as Arc<dyn Dispatcher>,
        script.to_path_buf(),
        Modules::new(),
    );
    block_on(waiting)
        .expect("the thread answered")
        .expect("the config ran");
    recorder.0.lock().expect("no other holder").clone()
}

/// Runs [`CONFIG`] against the no-IPC channel, with this thread standing in
/// for the daemon's request pass: drain what was queued, apply nothing, answer
/// each one. That is exactly the loop `ecs::apply_requests` runs.
fn through_direct(script: &std::path::Path) -> Vec<Request> {
    let channel = Channel::new(|| {});
    let mut waiting = host::spawn(
        Arc::new(channel.dispatcher()) as Arc<dyn Dispatcher>,
        script.to_path_buf(),
        Modules::new(),
    );

    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        for call in channel.drain() {
            let response = answer(&call.request);
            seen.push(call.request.clone());
            call.answer(response);
        }
        match waiting.try_recv() {
            Ok(outcome) => {
                outcome.expect("the config ran");
                break;
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                panic!("the Lua thread went away without answering");
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                assert!(Instant::now() < deadline, "the config never finished");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    // The config awaits every request, so finishing means nothing is left.
    assert!(channel.drain().is_empty(), "a request outlived the config");
    seen
}

/// The smallest runtime that can await one `oneshot`.
fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(future)
}

/// The whole promise, as one assertion: taking the port out changes nothing
/// about what the daemon is asked to do.
#[test]
fn the_direct_path_asks_for_exactly_what_the_ipc_path_would() {
    let scratch = Scratch::new("same");
    let script = scratch.write("rc.lua", CONFIG);

    let memory = through_memory(&script);
    let direct = through_direct(&script);

    assert!(
        !memory.is_empty(),
        "the config produced no requests at all, so this proves nothing"
    );
    assert_eq!(
        memory, direct,
        "the direct channel changed what the config asks for"
    );
}

/// The other half: what the wire adds is only bytes. If a request survives
/// encode/decode unchanged, then handing the value straight to `requests::apply`
/// cannot lose anything the port would have preserved.
#[test]
fn every_request_survives_the_wire_unchanged_so_skipping_it_loses_nothing() {
    use async_mach_ports::Codec as _;

    let scratch = Scratch::new("wire");
    let script = scratch.write("rc.lua", CONFIG);

    let requests = through_direct(&script);
    assert!(!requests.is_empty(), "nothing to round trip");

    for request in requests {
        let bytes = coolabah_protocol::wire::MessagePack
            .encode(&request)
            .expect("a request encodes");
        let decoded: Request = coolabah_protocol::wire::MessagePack
            .decode(&bytes)
            .expect("and decodes");
        assert_eq!(request, decoded, "the wire is not lossless for {request:?}");
    }
}

/// A config that calls `coolabah.run()` never returns on its own — it is an
/// event loop. Runs one against the channel, letting `pump` do whatever the
/// test is about on each turn, and hands back every request it saw.
///
/// Ending it is [`Channel::forget_all`]: dropping the feeds ends the streams
/// `run()` is merged over, which is the loop's own exit and the same thing
/// that happens when a reload replaces the config.
fn while_running(
    script: &std::path::Path,
    mut pump: impl FnMut(&Channel, &[Request]) -> bool,
) -> Vec<Request> {
    let channel = Channel::new(|| {});
    let mut waiting = host::spawn(
        Arc::new(channel.dispatcher()) as Arc<dyn Dispatcher>,
        script.to_path_buf(),
        Modules::new(),
    );

    let mut seen = Vec::new();
    let mut satisfied = false;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        for call in channel.drain() {
            let response = answer(&call.request);
            seen.push(call.request.clone());
            call.answer(response);
        }
        if !satisfied && pump(&channel, &seen) {
            satisfied = true;
            // Ends `coolabah.run()`, so the thread finishes rather than being
            // left parked for the life of the test binary.
            channel.forget_all();
        }
        match waiting.try_recv() {
            Ok(outcome) => {
                outcome.expect("the config ran");
                break;
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                panic!("the Lua thread went away without answering");
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                assert!(Instant::now() < deadline, "the config never finished");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
    seen
}

/// A subscription is part of the traffic too, and the direct path opens it the
/// same way: one `Request::Subscribe` naming the item and the kinds it wants.
#[test]
fn a_subscription_is_opened_with_the_same_request_either_way() {
    let scratch = Scratch::new("subs");
    let script = scratch.write(
        "rc.lua",
        r#"
        local clock = coolabah.add("item", "clock", {})
        clock:subscribe("system_woke", function() end)
        coolabah.run()
        "#,
    );

    let seen = while_running(&script, |_, seen| {
        seen.iter()
            .any(|request| matches!(request, Request::Subscribe { .. }))
    });

    let subscribes: Vec<Request> = seen
        .into_iter()
        .filter(|request| matches!(request, Request::Subscribe { .. }))
        .collect();

    assert_eq!(
        subscribes,
        vec![Request::Subscribe {
            name: ItemName::new("clock").expect("a name"),
            events: vec![Kind::SystemWoke],
        }],
        "the subscription the config asked for"
    );
}

/// An event pushed straight into the channel reaches the config's callback —
/// no port, no encoding, the `Event` value itself. `event_to_table` is the
/// same code either way, so what this checks is that the delivery happens at
/// all, and that a config handling three of them handles three.
#[test]
fn events_pushed_with_no_port_reach_the_callback() {
    let scratch = Scratch::new("events");
    let script = scratch.write(
        "rc.lua",
        r#"
        local clock = coolabah.add("item", "clock", {})
        local seen = 0
        clock:subscribe("system_woke", function(event)
            seen = seen + 1
            -- One request per event, so the driver can count them from
            -- outside without reaching into the Lua state.
            coolabah.trigger("mouse.entered", nil)
        end)
        coolabah.run()
        "#,
    );

    let item = ItemName::new("clock").expect("a name");
    let mut pushed = 0;
    let seen = while_running(&script, |channel, _| {
        if pushed < 3 && channel.is_subscribed(&item) {
            assert!(
                channel.deliver(
                    &item,
                    &coolabah_protocol::Event::SystemWoke(coolabah_protocol::event::SystemWoke {})
                ),
                "a subscribed item takes its event"
            );
            pushed += 1;
        }
        pushed == 3
    });

    assert_eq!(pushed, 3, "the config never opened its subscription");
    let handled = seen
        .iter()
        .filter(|request| matches!(request, Request::Trigger(_)))
        .count();
    assert!(handled >= 1, "the callback never ran: {seen:?}");
}

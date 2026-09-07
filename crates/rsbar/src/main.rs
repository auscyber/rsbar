//! The rsbar daemon.
//!
//! Three kinds of thread, and the split is forced by the platform.
//!
//! The **main thread** draws. Everything touching the window server belongs to
//! it, because a window only composites while its process pumps a `CFRunLoop`.
//! It is also the only thread that can observe a click, since it owns the
//! windows — so it holds an [`Emitter`] and feeds the same event stream the
//! sources do.
//!
//! The **runtime thread** runs tokio, awaiting the two things that arrive from
//! outside: requests over the Mach port, and events from the sources. Both are
//! handed to the main thread through a channel plus a run loop wakeup.
//!
//! A **source thread** each for the frameworks that insist on a run loop of
//! their own. See [`rsbar::sources`].

use async_mach_ports::{Receiver, RecvPort, Reply};
use objc2_core_foundation::CFRunLoop;
use rsbar::bar::Bar;
use rsbar::display::ReconfigurationWatch;
use rsbar::runloop::{Timer, Waker};
use rsbar::script::{Job, Runner};
use rsbar::sources::{Emission, Events, Registry};
use rsbar_protocol::{Query, Request, Response, service_name};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;

/// Something for the main thread to act on.
///
/// The request variant is boxed because it dwarfs the event one, and this
/// crosses a channel on every event the bar ever sees.
enum Incoming {
    Request {
        request: Box<Request>,
        reply: Option<Reply>,
    },
    Event(Emission),
}

/// How many scripts may run at once. Enough that a slow one does not stall the
/// rest, few enough that a misconfigured config cannot fork without bound.
const SCRIPT_WORKERS: usize = 4;

/// The routine tick. Item update frequencies are whole seconds, so a finer
/// timer would only wake the machine more often to do nothing.
const TICK_SECONDS: f64 = 1.0;

fn main() {
    init_tracing();

    let service = service_name();
    let receiver = match Receiver::<Request>::bind(&service) {
        Ok(receiver) => receiver,
        Err(err) => {
            tracing::error!(%service, %err, "could not claim the service name");
            std::process::exit(1);
        }
    };

    let bar = match Bar::new() {
        Ok(bar) => Rc::new(RefCell::new(bar)),
        Err(err) => {
            tracing::error!(%err, "could not create the bar window");
            std::process::exit(1);
        }
    };
    bar.borrow_mut().redraw_if_dirty();

    let runner = Rc::new(Runner::start(SCRIPT_WORKERS));
    let (mut registry, events) = Registry::new();
    registry.start_eager();
    let registry = Rc::new(RefCell::new(registry));

    let (tx, rx) = mpsc::channel::<Incoming>();
    let waker = install_main_handler(&bar, &runner, &registry, rx);
    let _observers = install_observers(&bar, &runner);
    spawn_runtime(receiver, events, tx, waker);

    tracing::info!(%service, "rsbar is up");
    CFRunLoop::run();
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("RSBAR_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(true)
        .init();
}

/// Installs the run loop source that drains work onto the main thread.
///
/// Run loop signals coalesce, so this drains everything queued rather than
/// assuming one item per wakeup, and repaints once at the end instead of once
/// per request — a config run is hundreds of sets.
fn install_main_handler(
    bar: &Rc<RefCell<Bar>>,
    runner: &Rc<Runner>,
    registry: &Rc<RefCell<Registry>>,
    rx: mpsc::Receiver<Incoming>,
) -> Waker {
    let (bar, runner, registry) = (Rc::clone(bar), Rc::clone(runner), Rc::clone(registry));
    Waker::install(move || {
        let mut bar = bar.borrow_mut();
        let mut shutdown = false;

        while let Ok(incoming) = rx.try_recv() {
            let jobs = match incoming {
                Incoming::Request { request, reply } => {
                    let (response, jobs) = handle(&mut bar, &registry, *request, &mut shutdown);
                    if let Some(reply) = reply
                        && let Err(err) = reply.send(&response)
                    {
                        tracing::debug!(%err, "client stopped waiting for its answer");
                    }
                    jobs
                }
                Incoming::Event(Emission { event, info }) => {
                    tracing::debug!(%event, ?info, "event");
                    bar.jobs_for(&event, info.as_deref())
                }
            };
            for job in jobs {
                runner.run(job);
            }
        }

        bar.redraw_if_dirty();
        if shutdown && let Some(run_loop) = CFRunLoop::main() {
            run_loop.stop();
        }
    })
}

/// Awaits everything that arrives from outside this process.
fn spawn_runtime(
    receiver: Receiver<Request>,
    mut events: Events,
    tx: mpsc::Sender<Incoming>,
    waker: Waker,
) {
    std::thread::Builder::new()
        .name("rsbar-runtime".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("could not build the runtime");

            runtime.block_on(async move {
                // Both arms hand off the same way, so a request and an event
                // reach the main thread through one ordered queue.
                let hand_off = |incoming: Incoming| {
                    let sent = tx.send(incoming).is_ok();
                    if sent {
                        waker.wake();
                    }
                    sent
                };

                loop {
                    tokio::select! {
                        request = receiver.recv() => match request {
                            Ok(delivery) => {
                                let incoming = Incoming::Request {
                                    request: Box::new(delivery.value),
                                    reply: delivery.reply,
                                };
                                if !hand_off(incoming) {
                                    break;
                                }
                            }
                            // One malformed client is not the end of the service.
                            Err(err) => {
                                tracing::warn!(%err, "dropping an undecodable request");
                            }
                        },
                        Some(emission) = events.recv() => {
                            if !hand_off(Incoming::Event(emission)) {
                                break;
                            }
                        }
                    }
                }
            });
        })
        .expect("failed to spawn the runtime thread");
}

/// Everything that produces work on the main thread without being asked.
fn install_observers(bar: &Rc<RefCell<Bar>>, runner: &Rc<Runner>) -> (ReconfigurationWatch, Timer) {
    let rebuild = |bar: &Rc<RefCell<Bar>>| {
        let mut bar = bar.borrow_mut();
        if let Err(err) = bar.rebuild_panels() {
            tracing::error!(%err, "could not rebuild the bar after a display change");
        }
        bar.redraw_if_dirty();
    };

    // Displays appearing or disappearing is the one thing NSWorkspace does not
    // report, and it invalidates every panel's geometry.
    let displays = {
        let bar = Rc::clone(bar);
        ReconfigurationWatch::install(move || rebuild(&bar))
    };

    let tick = {
        let (bar, runner) = (Rc::clone(bar), Rc::clone(runner));
        Timer::every(TICK_SECONDS, move || {
            for job in bar.borrow_mut().tick() {
                runner.run(job);
            }
        })
    };

    (displays, tick)
}

fn handle(
    bar: &mut Bar,
    registry: &Rc<RefCell<Registry>>,
    request: Request,
    shutdown: &mut bool,
) -> (Response, Vec<Job>) {
    let found = |ok: bool, name: &rsbar_protocol::ItemName| {
        if ok {
            Response::Ok
        } else {
            Response::Error(format!("no item named `{name}`"))
        }
    };

    match request {
        Request::SetBar(patch) => {
            tracing::debug!(?patch, "set bar");
            (
                match bar.apply(&patch) {
                    Ok(()) => Response::Ok,
                    Err(err) => Response::Error(err.to_string()),
                },
                Vec::new(),
            )
        }
        Request::AddItem { name, position } => {
            tracing::debug!(%name, ?position, "add item");
            bar.add_item(name, position);
            (Response::Ok, Vec::new())
        }
        Request::SetItem { name, patch } => {
            tracing::trace!(%name, ?patch, "set item");
            (found(bar.set_item(&name, &patch), &name), Vec::new())
        }
        Request::RemoveItem(name) => {
            tracing::debug!(%name, "remove item");
            (found(bar.remove_item(&name), &name), Vec::new())
        }
        Request::Subscribe { name, events } => {
            tracing::debug!(%name, ?events, "subscribe");
            // Starting a source is the point at which a config's subscriptions
            // decide what this process actually observes.
            registry.borrow_mut().ensure_all(&events);
            (found(bar.subscribe(&name, events), &name), Vec::new())
        }
        Request::Trigger { event, info } => {
            tracing::debug!(%event, ?info, "trigger");
            (Response::Ok, bar.jobs_for(&event, info.as_deref()))
        }
        Request::UpdateAll => (Response::Ok, bar.all_jobs()),
        Request::Query(Query::Bar) => (Response::Bar(Box::new(bar.state())), Vec::new()),
        Request::Query(Query::Items) => (
            Response::Items(bar.items().iter().map(rsbar::item::Item::state).collect()),
            Vec::new(),
        ),
        Request::Query(Query::Item(name)) => (
            match bar.item(&name) {
                Some(item) => Response::Item(Box::new(item.state())),
                None => Response::Error(format!("no item named `{name}`")),
            },
            Vec::new(),
        ),
        Request::Shutdown => {
            tracing::info!("shutting down");
            *shutdown = true;
            (Response::Ok, Vec::new())
        }
    }
}

/// Unused today, but the shape clicks will arrive through: the main thread owns
/// the windows, so it is the only place a click can be observed, and it feeds
/// the same stream every source writes to.
const _: fn(&Registry) -> rsbar::sources::Emitter = Registry::emitter;

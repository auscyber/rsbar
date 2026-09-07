//! The rsbar daemon.
//!
//! Two threads, and the split is forced by the platform. Everything that
//! touches the window server belongs to the thread running the `CFRunLoop`,
//! because a window only composites while its process pumps one. IPC wants to
//! block on a port. So the main thread draws and the IPC thread receives, and
//! requests cross between them through a channel plus a run loop wakeup.

use async_mach_ports::{Receiver, RecvPort, Reply};
use objc2_core_foundation::CFRunLoop;
use rsbar::bar::Bar;
use rsbar::display::ReconfigurationWatch;
use rsbar::runloop::{Timer, Waker};
use rsbar::script::{Job, Runner};
use rsbar::sources::Sources;
use rsbar_protocol::{Event, Query, Request, Response, service_name};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;

/// One request, with wherever its answer should go.
struct Incoming {
    request: Request,
    reply: Option<Reply>,
}

/// How many scripts may run at once. Enough that a slow one does not stall
/// the rest, few enough that a misconfigured config cannot fork without bound.
const SCRIPT_WORKERS: usize = 4;

/// The routine tick. Item update frequencies are whole seconds, so a finer
/// timer would only wake the machine more often to do nothing.
const TICK_SECONDS: f64 = 1.0;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("RSBAR_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

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
    let (tx, rx) = mpsc::channel::<Incoming>();

    // Signals coalesce, so the handler drains everything queued rather than
    // assuming one job per wakeup, and repaints once at the end instead of
    // once per request.
    let waker = {
        let bar = Rc::clone(&bar);
        let runner = Rc::clone(&runner);
        Waker::install(move || {
            let mut bar = bar.borrow_mut();
            let mut shutdown = false;
            while let Ok(incoming) = rx.try_recv() {
                let (response, jobs) = handle(&mut bar, incoming.request, &mut shutdown);
                for job in jobs {
                    runner.run(job);
                }
                if let Some(reply) = incoming.reply
                    && let Err(err) = reply.send(&response)
                {
                    tracing::debug!(%err, "client stopped waiting for its answer");
                }
            }
            bar.redraw_if_dirty();
            if shutdown && let Some(run_loop) = CFRunLoop::main() {
                run_loop.stop();
            }
        })
    };

    // Held for the life of the process: dropping any of these stops the thing
    // it drives.
    let _observers = install_observers(&bar, &runner);

    spawn_ipc(receiver, tx, waker);

    tracing::info!(%service, "rsbar is up");
    CFRunLoop::run();
}

/// Receives requests and hands them to the main thread.
///
/// Lives on its own thread because it blocks on a port, which the drawing
/// thread must never do.
fn spawn_ipc(receiver: Receiver<Request>, tx: mpsc::Sender<Incoming>, waker: Waker) {
    std::thread::Builder::new()
        .name("rsbar-ipc".into())
        .spawn(move || {
            futures_lite::future::block_on(async {
                loop {
                    match receiver.recv().await {
                        Ok(delivery) => {
                            let incoming = Incoming {
                                request: delivery.value,
                                reply: delivery.reply,
                            };
                            if tx.send(incoming).is_err() {
                                break;
                            }
                            waker.wake();
                        }
                        // One malformed client is not the end of the service.
                        Err(err) => tracing::warn!(%err, "dropping an undecodable request"),
                    }
                }
            });
        })
        .expect("failed to spawn the IPC thread");
}

/// Everything that produces work without being asked: system notifications,
/// display changes, and the routine tick.
fn install_observers(
    bar: &Rc<RefCell<Bar>>,
    runner: &Rc<Runner>,
) -> (Sources, ReconfigurationWatch, Timer) {
    let rebuild = |bar: &Rc<RefCell<Bar>>| {
        let mut bar = bar.borrow_mut();
        if let Err(err) = bar.rebuild_panels() {
            tracing::error!(%err, "could not rebuild the bar after a display change");
        }
        bar.redraw_if_dirty();
    };

    // Workspace notifications arrive on this thread already, so they act on
    // the bar directly rather than going back through the run loop source.
    let sources = {
        let (bar, runner) = (Rc::clone(bar), Rc::clone(runner));
        Sources::install(move |event, info| {
            for job in bar.borrow().jobs_for(&event, info.as_deref()) {
                runner.run(job);
            }
            // The active display changing can also mean its geometry did.
            if event == Event::DisplayChanged {
                rebuild(&bar);
            }
        })
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

    (sources, displays, tick)
}

fn handle(bar: &mut Bar, request: Request, shutdown: &mut bool) -> (Response, Vec<Job>) {
    let found = |ok: bool, name: &rsbar_protocol::ItemName| {
        if ok {
            Response::Ok
        } else {
            Response::Error(format!("no item named `{name}`"))
        }
    };

    match request {
        Request::SetBar(patch) => (
            match bar.apply(&patch) {
                Ok(()) => Response::Ok,
                Err(err) => Response::Error(err.to_string()),
            },
            Vec::new(),
        ),
        Request::AddItem { name, position } => {
            bar.add_item(name, position);
            (Response::Ok, Vec::new())
        }
        Request::SetItem { name, patch } => {
            let ok = bar.set_item(&name, &patch);
            (found(ok, &name), Vec::new())
        }
        Request::RemoveItem(name) => {
            let ok = bar.remove_item(&name);
            (found(ok, &name), Vec::new())
        }
        Request::Subscribe { name, events } => {
            let ok = bar.subscribe(&name, events);
            (found(ok, &name), Vec::new())
        }
        Request::Trigger { event, info } => {
            let jobs = bar.jobs_for(&event, info.as_deref());
            (Response::Ok, jobs)
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
            *shutdown = true;
            (Response::Ok, Vec::new())
        }
    }
}

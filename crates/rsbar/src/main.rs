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
use rsbar::runloop::Waker;
use rsbar_protocol::{Query, Request, Response, service_name};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;

/// One request, with wherever its answer should go.
struct Job {
    request: Request,
    reply: Option<Reply>,
}

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

    let (tx, rx) = mpsc::channel::<Job>();

    // Signals coalesce, so the handler drains everything queued rather than
    // assuming one job per wakeup, and repaints once at the end instead of
    // once per request.
    let waker = {
        let bar = Rc::clone(&bar);
        Waker::install(move || {
            let mut bar = bar.borrow_mut();
            let mut shutdown = false;
            while let Ok(job) = rx.try_recv() {
                let response = handle(&mut bar, job.request, &mut shutdown);
                if let Some(reply) = job.reply
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

    std::thread::Builder::new()
        .name("rsbar-ipc".into())
        .spawn(move || {
            futures_lite::future::block_on(async {
                loop {
                    match receiver.recv().await {
                        Ok(delivery) => {
                            let job = Job {
                                request: delivery.value,
                                reply: delivery.reply,
                            };
                            if tx.send(job).is_err() {
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

    tracing::info!(%service, "rsbar is up");
    CFRunLoop::run();
}

fn handle(bar: &mut Bar, request: Request, shutdown: &mut bool) -> Response {
    match request {
        Request::SetBar(patch) => match bar.apply(&patch) {
            Ok(()) => Response::Ok,
            Err(err) => Response::Error(err.to_string()),
        },
        Request::AddItem { name, position } => {
            bar.add_item(name, position);
            Response::Ok
        }
        Request::SetItem { name, patch } => {
            if bar.set_item(&name, &patch) {
                Response::Ok
            } else {
                Response::Error(format!("no item named `{name}`"))
            }
        }
        Request::RemoveItem(name) => {
            if bar.remove_item(&name) {
                Response::Ok
            } else {
                Response::Error(format!("no item named `{name}`"))
            }
        }
        Request::Query(Query::Bar) => Response::Bar(Box::new(bar.state())),
        Request::Query(Query::Items) => {
            Response::Items(bar.items().iter().map(rsbar::item::Item::state).collect())
        }
        Request::Query(Query::Item(name)) => match bar.item(&name) {
            Some(item) => Response::Item(Box::new(item.state())),
            None => Response::Error(format!("no item named `{name}`")),
        },
        Request::Shutdown => {
            *shutdown = true;
            Response::Ok
        }
    }
}

//! The rsbar daemon.
//!
//! Three kinds of thread, and the split is forced by the platform.
//!
//! The **main thread** is the Bevy app. Everything touching the window server
//! belongs to it, because a window only composites while its process pumps a
//! `CFRunLoop` — which is exactly what the app's runner blocks on. It is also
//! the only thread that can observe a click, since it owns the windows.
//!
//! The **IPC thread** blocks on the Mach port and hands requests over a channel.
//!
//! A **source thread** each for the frameworks that insist on a run loop of
//! their own. See [`rsbar::sources`].

use async_mach_ports::{Receiver, RecvPort};
use rsbar::bar::{Panels, Settings};
use rsbar::ecs::{self, Inbox, IpcRequest};
use rsbar::runloop::Waker;
use rsbar::script::Runner;
use rsbar::sources::Registry;
use rsbar_protocol::{Request, service_name};
use std::sync::mpsc;

/// How many scripts may run at once. Enough that a slow one does not stall the
/// rest, few enough that a misconfigured config cannot fork without bound.
const SCRIPT_WORKERS: usize = 4;

/// Depth of the request queue. Deep enough for a whole config run to land
/// between two ticks without the IPC thread blocking on it.
const REQUEST_QUEUE: usize = 1024;

fn main() -> std::process::ExitCode {
    init_tracing();

    let service = service_name();
    let receiver = match Receiver::<Request>::bind(&service) {
        Ok(receiver) => receiver,
        Err(err) => {
            tracing::error!(%service, %err, "could not claim the service name");
            return std::process::ExitCode::FAILURE;
        }
    };

    let settings = Settings::default();
    let mut panels = Panels::default();
    if let Err(err) = panels.rebuild(&settings) {
        tracing::error!(%err, "could not create the bar window");
        return std::process::ExitCode::FAILURE;
    }

    // Installed before anything that needs to wake the app. Signalling it is
    // what interrupts the runner's sleep in the run loop, so an event is looked
    // at when it happens rather than at the next routine tick.
    let waker = Waker::install(|| {});

    let config = rsbar::config::shared();
    let mut registry = Registry::new(rsbar::config::Shared::clone(&config), waker.clone());
    registry.start_eager();

    let (tx, requests) = mpsc::sync_channel::<IpcRequest>(REQUEST_QUEUE);
    spawn_ipc(receiver, tx, waker);

    tracing::info!(service = %service, "rsbar is up");
    let app = ecs::build(
        Inbox { requests },
        settings,
        panels,
        registry,
        Runner::start(SCRIPT_WORKERS),
        config,
        service,
    );
    match ecs::run(app) {
        bevy_app::AppExit::Success => std::process::ExitCode::SUCCESS,
        bevy_app::AppExit::Error(code) => std::process::ExitCode::from(code.get()),
    }
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

/// Receives requests and hands them to the app.
///
/// Its own thread because it blocks on a port, which the drawing thread must
/// never do.
fn spawn_ipc(receiver: Receiver<Request>, tx: mpsc::SyncSender<IpcRequest>, waker: Waker) {
    std::thread::Builder::new()
        .name("rsbar-ipc".into())
        .spawn(move || {
            futures_lite::future::block_on(async {
                loop {
                    match receiver.recv().await {
                        Ok(delivery) => {
                            let request = IpcRequest {
                                request: Box::new(delivery.value),
                                reply: delivery.reply,
                            };
                            if tx.send(request).is_err() {
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

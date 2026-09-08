//! The rsbar daemon.
//!
//! The **main thread** is the Bevy app, and it is also the reactor.
//! Everything touching the window server belongs to it, because a window only
//! composites while its process pumps a `CFRunLoop` — which is exactly what
//! the app's runner is parked in. It is also the only thread that can observe
//! a click, since it owns the windows, and now the only one that touches the
//! service port: the run loop receives on it natively, so there is no IPC
//! thread and nothing blocked on `mach_msg` anywhere. See [`rsbar::ipc`].
//!
//! What is left is a **script worker pool**, which is genuinely blocking work
//! — forked shells — and the odd **source thread** belonging to a framework
//! that insists on a run loop of its own. See [`rsbar::sources`].

use rsbar::bar::{Panels, Settings};
use rsbar::ecs::{self, Inbox};
use rsbar::script::Runner;
use rsbar::sources::Registry;
use rsbar_protocol::wire::{MessagePack, Receiver};
use rsbar_protocol::{Request, service_name};
use std::rc::Rc;

/// How many scripts may run at once. Enough that a slow one does not stall the
/// rest, few enough that a misconfigured config cannot fork without bound.
const SCRIPT_WORKERS: usize = 4;

#[skylight::main(pass, also(exit_waker, rebuild, waker, serve, build, Registry::new))]
fn main() -> std::process::ExitCode {
    init_tracing();

    // `mtm` is already a `skylight::MainThread` -- the attribute reads this
    // thread's run loop once and keeps it beside the proof, so no registration
    // below ever asks `CFRunLoop::current()` which loop it is on.

    // One binary, two modes, the way `sketchybar` works: bare, it is the
    // daemon; with a command, it talks to a running one. They are the same
    // program on purpose — a separate client is a second copy of the wire
    // format, and the two drifted the first time a field was added to a
    // request, with every message failing to decode until both were rebuilt.
    let mut args = std::env::args();
    let _binary = args.next();
    let args: Vec<String> = args.collect();
    if !args.is_empty() {
        return rsbar::cli::run(args.into_iter());
    }

    // Published before anything spawns, so a worker that wants the thread
    // that draws can reach it without being handed a reference.
    rsbar::runloop::publish_main(mtm);

    // First, so a signal arriving during start-up is still answered. The
    // ordering is no longer load-bearing the way it was when this masked
    // signals and inherited that mask into every later thread -- `sigaction`
    // is process-wide from the moment it is installed. See `signals`.
    let _stop = match rsbar::signals::Stop::listen(rsbar::ecs::exit_waker()) {
        Ok(stop) => stop,
        Err(err) => {
            tracing::error!(%err, "could not arrange to stop on a signal");
            return std::process::ExitCode::FAILURE;
        }
    };

    let service = service_name();

    // Before anything is created. A second daemon would otherwise get as far
    // as building its windows before the service name refused it.
    let lock = match rsbar::lock::only(mtm, &service) {
        Ok(lock) => lock,
        Err(err) => {
            tracing::error!(%err, "refusing to start a second bar");
            return std::process::ExitCode::FAILURE;
        }
    };
    tracing::debug!(lock = %lock.path().display(), "took the lock");

    ask_for_screen_recording();

    let receiver = match Receiver::<Request>::bind(&service, MessagePack) {
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

    // Taken before anything that needs to wake the app. Signalling it is what
    // brings a pass about, so an event is looked at when it happens rather
    // than at some later wake. Every source signals it, so does the delivery
    // of a request, and so does a routine's deadline coming due.
    //
    // `ecs::waker()` rather than a source of this module's own: they are the
    // same source deliberately. Signals coalesce inside CoreFoundation, so a
    // request and a deadline arriving in one turn of the loop are one pass
    // between them -- across two sources they would have been two.
    let waker = rsbar::ecs::waker();

    let config = rsbar::config::shared();
    let mut registry = Registry::new(rsbar::config::Shared::clone(&config), waker.clone());
    // Only the sources that declared themselves eager. Everything else waits
    // for an item to want it, and stops again when the last one does not.
    registry.start_eager();

    // The port goes on the run loop the app is about to be parked in, so a
    // request is delivered on the thread that applies it. Held until the loop
    // returns: dropping it takes the service off the run loop.
    let requests = Rc::new(rsbar::ipc::Requests::default());
    // Signalled rather than passed directly: several messages delivered in one
    // turn of the run loop coalesce into a single signal, so a config run is
    // one pass and one repaint rather than seventy of each.
    let signal = waker.clone();
    let Some(_service) = rsbar::ipc::serve(receiver, Rc::clone(&requests), move || signal.wake())
    else {
        tracing::error!(%service, "could not put the service port on the run loop");
        return std::process::ExitCode::FAILURE;
    };

    tracing::info!(service = %service, "rsbar is up");
    let app = ecs::build(
        Inbox(requests),
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

/// Asks for Screen Recording, and waits for the answer.
///
/// An alias is a *picture* of another application's menu bar item, so without
/// this grant every capture fails and every alias shows whatever it managed
/// once, for ever -- silently, because a refusal looks exactly like a menu bar
/// item that has not changed. The daemon had both the check and the prompt and
/// called neither, which is the worst of the three options.
///
/// Blocking here is deliberate and this is the one place it is right: the run
/// loop has not started, nothing is drawn yet, and coming up with a bar whose
/// aliases can never work is worse than taking a moment to ask. The prompt is
/// the system's own and appears once; after that this returns immediately.
///
/// A grant made while this waits is picked up without a restart --
/// `CGPreflightScreenCaptureAccess` re-reads the answer on every call, the
/// same way `AXIsProcessTrusted` does.
fn ask_for_screen_recording() {
    if rsbar::alias::screen_capture_trusted() {
        return;
    }

    tracing::warn!(
        "no Screen Recording permission: menu bar aliases cannot be captured. \
         Asking for it now -- grant it in System Settings > Privacy & Security > \
         Screen Recording."
    );
    // The answer here is always false -- the prompt is asynchronous and the
    // user has not seen it yet. The loop below is what waits for it.
    let _ = rsbar::alias::request_screen_capture();

    // The prompt returns before the user has answered, so poll for the answer
    // -- the one poll in this daemon, bounded, before the run loop exists and
    // with nothing else to do. There is no notification for this grant the way
    // there is for Accessibility.
    let deadline = std::time::Instant::now() + WAIT_FOR_SCREEN_RECORDING;
    while std::time::Instant::now() < deadline {
        if rsbar::alias::screen_capture_trusted() {
            tracing::info!("Screen Recording granted; aliases will capture");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }

    tracing::error!(
        "still no Screen Recording permission; starting anyway. Aliases will \
         draw nothing until it is granted and rsbar is restarted."
    );
}

/// How long to wait for an answer to the Screen Recording prompt before coming
/// up without it. Long enough to find the checkbox, short enough that a
/// headless start is not wedged.
const WAIT_FOR_SCREEN_RECORDING: std::time::Duration = std::time::Duration::from_secs(30);

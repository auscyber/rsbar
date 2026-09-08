//! Whether a burst of requests reaches a running daemon intact.
//!
//! The `rsbard` client spends most of its time being a process, so a burst
//! driven from a shell measures `fork` rather than IPC. This sends every
//! request over one connection instead, as a `call` — which means the daemon
//! answered each one, and answering is the only proof that a request was not
//! lost between the kernel's queue and the pass that applies it.
//!
//! ```text
//! RSBAR_SERVICE=... cargo run --release --example ipc_burst -- 500
//! ```

use async_mach_ports::SendPort as _;
use rsbar_protocol::wire::{MessagePack, Sender};
use rsbar_protocol::{Query, Request, Response, service_name};
use std::time::Instant;

/// How many requests to send when the command line does not say.
const DEFAULT: usize = 500;

fn main() -> std::process::ExitCode {
    let count: usize = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(DEFAULT);

    let service = service_name();
    let Ok(sender) = Sender::<Request>::connect(&service, MessagePack) else {
        eprintln!("rsbar is not running on {service}");
        return std::process::ExitCode::FAILURE;
    };

    // A query rather than a write: it exercises the whole round trip — queue,
    // pass, reply — without needing an item the config happens to have.
    let started = Instant::now();
    let mut answered = 0usize;
    let mut failed = 0usize;
    for _ in 0..count {
        match sender.call_blocking::<Response>(&Request::Query(Query::Items)) {
            Ok(Response::Items(_)) => answered += 1,
            Ok(other) => {
                eprintln!("unexpected answer: {other:?}");
                failed += 1;
            }
            Err(err) => {
                eprintln!("{err}");
                failed += 1;
            }
        }
    }
    let elapsed = started.elapsed();

    println!(
        "{answered}/{count} answered, {failed} failed, {elapsed:.2?} total, {:.2?} each",
        elapsed / u32::try_from(count).unwrap_or(1)
    );

    if failed == 0 && answered == count {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

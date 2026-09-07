//! `SketchyBar`'s own command grammar, aimed at this daemon.
//!
//! `sketchybar`'s CLI lets one invocation carry several domains —
//! `--bar ... --add item ... --set ... --subscribe ...` — so a whole config is
//! one process and one connection rather than one spawn per property. That is
//! the property worth keeping, so [`run`] parses the *entire* argument list
//! into a plan up front ([`grammar::parse`]) and only then opens a connection,
//! sending every request over it in order.
//!
//! `crates/rsbar/src/main.rs` dispatches into here when invoked with
//! arguments; with none, it starts the daemon instead. That split belongs to
//! `main.rs`, not to this module.

mod args;
mod client;
mod grammar;
mod json;

pub use grammar::ParseError;

use std::process::ExitCode;

/// Parses `args` as a `SketchyBar`-shaped command line and sends the
/// resulting requests to the running daemon, in argv order, over one
/// connection.
///
/// `args` is the CLI's own arguments, not including the program name — call
/// this with `std::env::args().skip(1)`.
#[must_use]
pub fn run(args: impl Iterator<Item = String>) -> ExitCode {
    let args: Vec<String> = args.collect();
    let requests = match grammar::parse(&args) {
        Ok(requests) => requests,
        Err(err) => {
            eprintln!("rsbar: {err}");
            return ExitCode::FAILURE;
        }
    };
    if requests.is_empty() {
        eprintln!("rsbar: no command given");
        return ExitCode::FAILURE;
    }
    client::send_all(&requests)
}

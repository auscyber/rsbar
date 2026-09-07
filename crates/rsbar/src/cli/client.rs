//! Sends a plan of requests to the daemon over one connection.

use async_mach_ports::{SendPort, Sender};
use rsbar_protocol::{Request, Response, service_name};
use std::process::ExitCode;

/// Sends every request in order over one connection, printing each response
/// as it arrives.
///
/// A later request is sent even if an earlier one failed — one typo in a
/// ten-domain invocation should not silently swallow the other nine — but the
/// process still exits with failure if anything did.
pub(super) fn send_all(requests: &[Request]) -> ExitCode {
    let service = service_name();
    let sender = match Sender::<Request>::connect(&service) {
        Ok(sender) => sender,
        Err(err) => {
            eprintln!("rsbar is not running ({err})");
            return ExitCode::FAILURE;
        }
    };

    let mut failed = false;
    for request in requests {
        match sender.call_blocking::<Response>(request) {
            Ok(response) => failed |= !print_response(response),
            Err(err) => {
                eprintln!("{err}");
                failed = true;
            }
        }
    }

    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Prints one response. Returns whether it counts as success.
fn print_response(response: Response) -> bool {
    match response {
        Response::Ok => true,
        Response::Error(message) => {
            eprintln!("{message}");
            false
        }
        Response::Bar(state) => {
            println!("{state:#?}");
            true
        }
        Response::Items(items) => {
            for item in items {
                println!("{item:?}");
            }
            true
        }
        Response::Item(item) => {
            println!("{item:#?}");
            true
        }
        Response::MenuItems(found) => {
            for item in found {
                println!("{item}");
            }
            true
        }
    }
}

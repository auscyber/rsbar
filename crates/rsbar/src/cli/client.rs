//! Sends a plan of requests to the daemon over one connection.

use async_mach_ports::{SendPort, Sender};
use rsbar_protocol::{Request, Response, service_name};
use std::process::ExitCode;

use super::json::to_sketchybar_json;

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
///
/// A query prints real JSON, in `SketchyBar`'s own spellings (`"on"`/`"off"`
/// rather than `true`/`false`, `q`/`e` for the centre positions) via
/// [`to_sketchybar_json`], since that is the shape a config's own `--query`
/// output is written against — see `~/dendritic/sketchybar/items/menus.lua`'s
/// `menu_items[1]:query().geometry.drawing == "on"`.
fn print_response(response: Response) -> bool {
    match response {
        Response::Ok => true,
        Response::Error(message) => {
            eprintln!("{message}");
            false
        }
        Response::Bar(state) => {
            println!("{}", to_sketchybar_json(&state));
            true
        }
        Response::Items(items) => {
            println!("{}", to_sketchybar_json(&items));
            true
        }
        Response::Item(item) => {
            println!("{}", to_sketchybar_json(&item));
            true
        }
        Response::Defaults(patch) => {
            println!("{}", to_sketchybar_json(&patch));
            true
        }
        Response::MenuItems(found) | Response::AppMenus(found) => {
            for item in found {
                println!("{item}");
            }
            true
        }
    }
}

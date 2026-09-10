//! Spike: prove `crate::menus` (task 13's native `-l`/`-s` replacement) and
//! `alias::press_item` actually do something on this machine, rather than
//! just registering cleanly and never firing.
//!
//! Evidence a press really opened a menu: diff the raw window list before and
//! after. A real dropdown is a brand new on-screen window (a popup menu at a
//! very high level); if the count and layers are identical before and after,
//! the press did not do anything, whatever it returned.
use coolabah::{alias, menus};
use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
use objc2_core_graphics::{CGWindowListCopyWindowInfo, CGWindowListOption};
use skylight::cf::{Array, WindowKey};
use std::future::Future;
use std::time::Duration;

/// Runs `future` to completion on a thread of its own, pumping *this*
/// thread's `CFRunLoop` in short bursts while it waits.
///
/// `menus::list` and `menus::press` are `async` now: the `AppKit` half runs
/// through `runloop::on_main`, which answers by queueing an errand on the
/// main thread's `CFRunLoopSource` and waking it -- and that source only
/// fires while something is actually pumping the loop. `coolabah::pool::handle()
/// .block_on(future)` alone would park *this* thread without ever pumping it,
/// which is exactly the deadlock `clippy.toml` bans `block_on` in the daemon
/// over. A one-shot probe has no reactor of its own, so it drives the
/// blocking half on a second thread and spends this one pumping instead --
/// acceptable here in a way it would not be in the daemon proper, which is
/// never blocked on anything.
fn block_on_main<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> T {
    let (done, wait) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(coolabah::pool::handle().block_on(future));
    });
    loop {
        if let Ok(value) = wait.try_recv() {
            return value;
        }
        // SAFETY: `kCFRunLoopDefaultMode` is a `'static` constant, and this
        // runs only on the main thread, which owns the loop being pumped.
        unsafe { CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, 0.05, false) };
    }
}

#[derive(Debug, Clone)]
struct Row {
    id: i64,
    layer: i64,
    owner: String,
    name: String,
}

fn snapshot() -> Vec<Row> {
    let list = CGWindowListCopyWindowInfo(CGWindowListOption::OptionAll, 0).expect("window list");
    let list = Array::new(&list);
    (0..list.len())
        .filter_map(|i| list.dict(i))
        .map(|dict| Row {
            id: dict.get::<i64>(WindowKey::Number).unwrap_or(-1),
            layer: dict.get::<i64>(WindowKey::Layer).unwrap_or(i64::MIN),
            owner: dict.get::<String>(WindowKey::OwnerName).unwrap_or_default(),
            name: dict.get::<String>(WindowKey::Name).unwrap_or_default(),
        })
        .collect()
}

fn diff(before: &[Row], after: &[Row]) -> Vec<Row> {
    after
        .iter()
        .filter(|a| !before.iter().any(|b| b.id == a.id))
        .cloned()
        .collect()
}

fn screenshot(name: &str) {
    let path = format!(
        "/private/tmp/claude-501/-Users-ivypierlot-code-rust-sketchybar/e1bcf168-6e1e-42ce-8f66-2653a528a79b/scratchpad/{name}"
    );
    match std::process::Command::new("screencapture")
        .args(["-x", &path])
        .status()
    {
        Ok(status) if status.success() => println!("  screenshot: {path}"),
        Ok(status) => println!("  screencapture exited with {status}"),
        Err(err) => println!("  could not run screencapture: {err}"),
    }
}

#[skylight::main(also(now), pass)]
fn main() {
    // `on_main` -- which `menus::list`/`menus::press` now go through --
    // answers nothing until the main loop is published; the daemon does this
    // in `main.rs`, and a probe has to as well.
    coolabah::runloop::publish_main(mtm);

    println!(
        "Accessibility permission: {}",
        alias::accessibility_trusted().is_ok()
    );
    println!(
        "Screen Recording permission: {}",
        alias::screen_capture_trusted()
    );
    println!();

    println!("=== menus::list() (front app's own AXMenuBar) ===");
    match block_on_main(menus::list()) {
        Ok(items) => {
            for item in &items {
                println!("  [{}] {:?}", item.index, item.title);
            }
            if let Some(target) = items.iter().find(|i| i.index > 0) {
                println!(
                    "\nPressing index {} ({:?}) via menus::press ...",
                    target.index, target.title
                );
                let before = snapshot();
                let result = block_on_main(menus::press(target.index));
                std::thread::sleep(Duration::from_millis(300));
                let after = snapshot();
                println!("  menus::press result: {result:?}");
                let new_windows = diff(&before, &after);
                if new_windows.is_empty() {
                    println!(
                        "  NO new window appeared after the press — this did not visibly open a menu."
                    );
                } else {
                    println!("  {} new window(s) appeared:", new_windows.len());
                    for w in &new_windows {
                        println!(
                            "    id={} layer={} owner={:?} name={:?}",
                            w.id, w.layer, w.owner, w.name
                        );
                    }
                    screenshot("menu_open_front_app.png");
                }
                // Best-effort only: neither `AXCancelAction` nor a second
                // `press` reliably closes a menu opened this way — measured
                // live, `AXCancelAction` returns `Success` on Ghostty's own
                // menu bar items without actually closing anything, and is
                // outright unsupported (`kAXErrorActionUnsupported`) on
                // OneDrive's extras item. `menus.c` has the same gap: it
                // never checks whether its own `AXCancelAction` call did
                // anything either. A leftover open menu here is a harmless
                // side effect of this probe, not a functional problem with
                // pressing.
                let _ = block_on_main(menus::press(target.index));
            }
        }
        Err(err) => println!("menus::list() failed: {err}"),
    }

    println!("\n=== alias::press_item on a real extras item ===");
    let menu_bar = match alias::Snapshot::now() {
        Ok(menu_bar) => menu_bar,
        Err(err) => {
            println!("could not list the menu bar layer: {err}");
            return;
        }
    };
    match alias::list_menu_bar_items(&menu_bar) {
        Ok(items) if !items.is_empty() => {
            let target = &items[0];
            println!(
                "Pressing {},{} via alias::press_item ...",
                target.owner, target.name
            );
            let before = snapshot();
            let result = alias::press_item(&menu_bar, &target.owner, &target.name);
            std::thread::sleep(Duration::from_millis(300));
            let after = snapshot();
            println!("  alias::press_item result: {result:?}");
            let new_windows = diff(&before, &after);
            if new_windows.is_empty() {
                println!(
                    "  NO new window appeared after the press — this did not visibly open anything."
                );
            } else {
                println!("  {} new window(s) appeared:", new_windows.len());
                for w in &new_windows {
                    println!(
                        "    id={} layer={} owner={:?} name={:?}",
                        w.id, w.layer, w.owner, w.name
                    );
                }
                screenshot("menu_open_alias_item.png");
            }
            // Best-effort only — see the comment on the equivalent line
            // above; this often leaves the menu open.
            let _ = alias::press_item(&menu_bar, &target.owner, &target.name);
        }
        Ok(_) => println!("no aliasable items found to press"),
        Err(err) => println!("alias::list_menu_bar_items failed: {err}"),
    }
}

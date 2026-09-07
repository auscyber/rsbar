//! Spike: prove `crate::menus` (task 13's native `-l`/`-s` replacement) and
//! `alias::press_item` actually do something on this machine, rather than
//! just registering cleanly and never firing.
//!
//! Evidence a press really opened a menu: diff the raw window list before and
//! after. A real dropdown is a brand new on-screen window (a popup menu at a
//! very high level); if the count and layers are identical before and after,
//! the press did not do anything, whatever it returned.
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFString, CFType};
use objc2_core_graphics::{
    CGWindowListCopyWindowInfo, CGWindowListOption, kCGWindowLayer, kCGWindowName, kCGWindowNumber,
    kCGWindowOwnerName,
};
use rsbar::{alias, menus};
use std::ptr::NonNull;
use std::time::Duration;

#[derive(Debug, Clone)]
struct Row {
    id: i64,
    layer: i64,
    owner: String,
    name: String,
}

fn dict_at(array: &CFArray, i: isize) -> Option<&CFDictionary> {
    let ptr = unsafe { array.value_at_index(i) };
    let ptr = NonNull::new(ptr.cast_mut())?.cast::<CFType>();
    unsafe { ptr.as_ref() }.downcast_ref::<CFDictionary>()
}

fn dict_string(dict: &CFDictionary, key: &CFString) -> Option<String> {
    let ptr = unsafe { dict.value((std::ptr::from_ref(key)).cast()) };
    let ptr = NonNull::new(ptr.cast_mut())?.cast::<CFType>();
    Some(
        unsafe { ptr.as_ref() }
            .downcast_ref::<CFString>()?
            .to_string(),
    )
}

fn dict_i64(dict: &CFDictionary, key: &CFString) -> Option<i64> {
    let ptr = unsafe { dict.value((std::ptr::from_ref(key)).cast()) };
    let ptr = NonNull::new(ptr.cast_mut())?.cast::<CFType>();
    unsafe { ptr.as_ref() }.downcast_ref::<CFNumber>()?.as_i64()
}

fn snapshot() -> Vec<Row> {
    let list = CGWindowListCopyWindowInfo(CGWindowListOption::OptionAll, 0).expect("window list");
    let mut rows = Vec::new();
    for i in 0..list.count() {
        let Some(dict) = dict_at(&list, i) else {
            continue;
        };
        let id = dict_i64(dict, unsafe { kCGWindowNumber }).unwrap_or(-1);
        let layer = dict_i64(dict, unsafe { kCGWindowLayer }).unwrap_or(i64::MIN);
        let owner = dict_string(dict, unsafe { kCGWindowOwnerName }).unwrap_or_default();
        let name = dict_string(dict, unsafe { kCGWindowName }).unwrap_or_default();
        rows.push(Row {
            id,
            layer,
            owner,
            name,
        });
    }
    rows
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

fn main() {
    println!(
        "Accessibility permission: {}",
        alias::accessibility_trusted()
    );
    println!(
        "Screen Recording permission: {}",
        alias::screen_capture_trusted()
    );
    println!();

    println!("=== menus::list() (front app's own AXMenuBar) ===");
    match menus::list() {
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
                let result = menus::press(target.index);
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
                let _ = menus::press(target.index);
            }
        }
        Err(err) => println!("menus::list() failed: {err}"),
    }

    println!("\n=== alias::press_item on a real extras item ===");
    match alias::list_menu_bar_items() {
        Ok(items) if !items.is_empty() => {
            let target = &items[0];
            println!(
                "Pressing {},{} via alias::press_item ...",
                target.owner, target.name
            );
            let before = snapshot();
            let result = alias::press_item(&target.owner, &target.name);
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
            let _ = alias::press_item(&target.owner, &target.name);
        }
        Ok(_) => println!("no aliasable items found to press"),
        Err(err) => println!("alias::list_menu_bar_items failed: {err}"),
    }
}

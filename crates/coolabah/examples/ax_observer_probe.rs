//! Spike: does `AXObserverAddNotification(kAXValueChangedNotification)` (and
//! `kAXTitleChangedNotification`) actually fire when a menu bar extra's
//! *contents* change — the Control Centre clock's displayed text, in
//! particular — as opposed to merely registering without error?
//!
//! Usage: `cargo run -p coolabah --example ax_observer_probe -- <owner> <name>`,
//! e.g. `ax_observer_probe "Control Centre" Clock`. Defaults to that.
//! `NARROW=1` registers only `AXValueChanged`/`AXTitleChanged` instead of the
//! full sweep of every `AXNotificationConstants.h` name below, to check
//! whether the broad list was doing any work; it was not — the same two
//! notifications fire either way, which is why `alias_watch` only asks for
//! those two.
//!
//! # Result (macOS 26.5.1, this machine)
//!
//! **Yes, once scoped to the right pid and filtered by identity.** Four
//! things were checked, in order:
//!
//! 1. Adding the notification directly to the extras item's own
//!    `AXUIElement` (role `AXMenuBarItem`) is rejected outright with
//!    `kAXErrorNotificationUnsupported` (-25207) — for every notification
//!    tried, for both a Control Centre-native item (Clock) and a genuinely
//!    separate-process one (Fantastical's own status item). This is the OS
//!    stating the role does not participate in the AX notification system at
//!    all, not a registration that silently never fires.
//! 2. Adding the *same* notifications to the owning application's top-level
//!    element, or to its `AXExtrasMenuBar` container, is accepted
//!    (`AXError::Success`) — and it fires. Watched live across a minute
//!    rollover, this exact run caught:
//!    ```text
//!    AX notification fired: AXTitleChanged owner_pid=762 (AXError(0)) role=Some("AXButton") title=None desc=Some("Clock") value=Some("Mon 7 Sep  4:20 pm")
//!    AX notification fired: AXValueChanged owner_pid=762 (AXError(0)) role=Some("AXButton") title=None desc=Some("Clock") value=Some("Mon 7 Sep  4:20 pm")
//!    ```
//!    at the minute boundary itself — the Control Centre clock's own change,
//!    identifiable by `AXDescription` reading `"Clock"`.
//! 3. What *also* fires at that same broad scope is unrelated traffic: a
//!    different app's still-mounted (but not currently visible) popover text,
//!    `AXValueChanged` roughly once a second, `AXDescription` empty,
//!    delivered to an observer that was created for Control Centre's pid.
//!    `AXUIElementGetPid` on the *notified element* gives that element's own,
//!    real pid, and it does not match the pid the observer was created for —
//!    which is the check that throws this traffic out without throwing out
//!    the clock's. A consumer has to make that check itself; nothing filters
//!    it upstream.
//! 4. The system-wide element and the item's own children were also tried,
//!    for completeness: the former accepts no notification, the latter has
//!    no children to add one to.
//!
//! Bonus finding along the way: Control Centre's own extras-bar children have
//! no `AXTitle` at all (`None` for all 17 checked) — the identifying string
//! lives in `AXDescription` instead (e.g. `"Clock"`), and `AXValue` already
//! holds the live rendered text (`"Mon 7 Sep  3:59 pm"`). `alias.rs`'s
//! `ax::enumerate_extras_menu_items` used to filter on `AXTitle` only, silently
//! dropping every genuinely Control Centre-native module from its candidate
//! list before position-matching ever ran; it now accepts `AXDescription`
//! too. `crate::alias_watch` is where the pid- and identity-filtered observer
//! this result implies actually lives.

#![allow(clippy::too_many_lines)]

use objc2_app_kit::NSWorkspace;
use objc2_application_services::AXUIElement;
use objc2_core_foundation::{CFArray, CFRetained, CFRunLoop, Type, kCFRunLoopDefaultMode};
use skylight::ax;
use std::time::{Duration, Instant};

/// Finds `(pid, element)` for the extras-menu-bar child titled `name` under
/// the running application named `owner`.
fn find(owner: &str, name: &str) -> Option<(i32, CFRetained<AXUIElement>)> {
    let workspace = NSWorkspace::sharedWorkspace();
    for app in &workspace.runningApplications().to_vec() {
        let pid = app.processIdentifier();
        if pid <= 0 {
            continue;
        }
        let app_name = app.localizedName().map(|s| s.to_string());
        if app_name.as_deref() != Some(owner) {
            continue;
        }
        // SAFETY: pid is a live process id.
        let element = ax::application(ax::Trusted::now()?, pid);
        let Some(extras) = ax::attribute::<CFRetained<AXUIElement>>(&element, "AXExtrasMenuBar")
        else {
            println!("  no AXExtrasMenuBar for pid={pid}");
            continue;
        };
        let Some(children) = ax::attribute::<CFRetained<CFArray>>(&extras, "AXVisibleChildren")
            .or_else(|| ax::attribute::<CFRetained<CFArray>>(&extras, "AXChildren"))
        else {
            println!("  no children for pid={pid}");
            continue;
        };
        println!("  {} extras children", children.count());
        for i in 0..children.count() {
            let Some(child) = ax::array_element(&children, i) else {
                continue;
            };
            let title = ax::attribute::<String>(child, "AXTitle");
            let desc = ax::attribute::<String>(child, "AXDescription");
            let frame = ax::frame(child);
            let value = ax::attribute::<String>(child, "AXValue");
            let role = ax::attribute::<String>(child, "AXRole");
            println!(
                "    child[{i}] role={role:?} title={title:?} desc={desc:?} value={value:?} frame={frame:?}"
            );
            let identity = title.as_deref().or(desc.as_deref());
            if identity == Some(name) {
                println!(
                    "found element: identity={identity:?} role={:?} frame={frame:?}",
                    ax::attribute::<String>(child, "AXRole")
                );
                return Some((pid, child.retain()));
            }
        }
    }
    None
}

/// Every notification name in Apple's public `AXNotificationConstants.h`,
/// not just the two or three that seemed relevant — tried against every
/// element this probe can reach, per the request to be exhaustive rather
/// than assume the rest would also be rejected.
const NOTIFICATIONS: &[&str] = &[
    "AXMainWindowChanged",
    "AXFocusedWindowChanged",
    "AXFocusedUIElementChanged",
    "AXFocusedTabChanged",
    "AXApplicationActivated",
    "AXApplicationDeactivated",
    "AXApplicationHidden",
    "AXApplicationShown",
    "AXWindowCreated",
    "AXWindowMoved",
    "AXWindowResized",
    "AXWindowMiniaturized",
    "AXWindowDeminiaturized",
    "AXDrawerCreated",
    "AXSheetCreated",
    "AXUIElementDestroyed",
    "AXValueChanged",
    "AXTitleChanged",
    "AXResized",
    "AXMoved",
    "AXCreated",
    "AXMenuOpened",
    "AXMenuClosed",
    "AXMenuItemSelected",
    "AXRowCountChanged",
    "AXRowExpanded",
    "AXRowCollapsed",
    "AXSelectedCellsChanged",
    "AXUnitsChanged",
    "AXSelectedChildrenMoved",
    "AXSelectedChildrenChanged",
    "AXSelectedRowsChanged",
    "AXSelectedColumnsChanged",
    "AXSelectedTextChanged",
    "AXLayoutChanged",
    "AXAnnouncementRequested",
    "AXHelpTagCreated",
    "AXElementBusyChanged",
    "AXPriorityChanged",
    "AXLoadComplete",
];

/// Every notification the probe registered, printed as it arrives.
///
/// The context is what this probe is really demonstrating alongside the
/// daemon's own use: `ax::Observer` hands the callback a `&Started` rather
/// than a `*mut c_void` to cast back, so nothing here writes a cast at all.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the shape `ax::Observer` calls back with"
)]
fn on_notification(notified: ax::Notified<'_, Started>) {
    let element = notified.element;
    println!(
        "[{:?}] AX notification fired: {} owner_pid={:?} ({:?} since start) role={:?} title={:?} \
         desc={:?} value={:?}",
        Instant::now(),
        notified.notification,
        ax::pid_of(element),
        notified.context.0.elapsed(),
        ax::attribute::<String>(element, "AXRole"),
        ax::attribute::<String>(element, "AXTitle"),
        ax::attribute::<String>(element, "AXDescription"),
        ax::attribute::<String>(element, "AXValue"),
    );
}

/// When the probe started, so each notification prints how long it took.
struct Started(Instant);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (owner, name) = if args.len() >= 2 {
        (args[0].clone(), args[1].clone())
    } else {
        ("Control Centre".to_owned(), "Clock".to_owned())
    };
    // NARROW=1: register only AXValueChanged/AXTitleChanged, to isolate
    // whether the full 39-notification list was what made delivery work, or
    // whether it would have worked with just the two relevant names anyway.
    let notifications: &[&str] = if std::env::var("NARROW").as_deref() == Ok("1") {
        &["AXValueChanged", "AXTitleChanged"]
    } else {
        NOTIFICATIONS
    };

    println!(
        "accessibility_trusted = {}",
        coolabah::alias::accessibility_trusted().is_ok()
    );

    let Some((pid, element)) = find(&owner, &name) else {
        eprintln!("no extras-menu-bar element matches owner={owner:?} title={name:?}");
        std::process::exit(1);
    };
    println!("pid={pid}");

    // Does the item itself have children (a sub-element that might accept a
    // notification even though the AXMenuBarItem parent flatly refuses one)?
    let children = ax::attribute::<CFRetained<CFArray>>(&element, "AXChildren");
    println!(
        "item AXChildren: {:?}",
        children.as_ref().map(|c| c.count())
    );
    let mut child_elements: Vec<CFRetained<AXUIElement>> = Vec::new();
    if let Some(children) = &children {
        for i in 0..children.count() {
            let Some(child) = ax::array_element(children, i) else {
                continue;
            };
            println!(
                "  child[{i}] role={:?} title={:?} desc={:?} value={:?}",
                ax::attribute::<String>(child, "AXRole"),
                ax::attribute::<String>(child, "AXTitle"),
                ax::attribute::<String>(child, "AXDescription"),
                ax::attribute::<String>(child, "AXValue"),
            );
            child_elements.push(child.retain());
        }
    }

    let Some(access) = ax::Trusted::now() else {
        println!("no Accessibility grant; nothing to observe");
        return;
    };
    let mut observer =
        match ax::Observer::create(access, pid, Started(Instant::now()), on_notification) {
            Ok(observer) => observer,
            Err(err) => {
                eprintln!("could not create an observer: {err}");
                std::process::exit(1);
            }
        };

    // Everything plausible, so the probe can tell which registration is the
    // one that actually delivers: the item itself, its children, the owning
    // application, its extras bar, and the system-wide element.
    let app_element = ax::application(access, pid);
    let extras = ax::attribute::<CFRetained<AXUIElement>>(&app_element, "AXExtrasMenuBar");
    // SAFETY: takes no arguments.
    let system_wide = unsafe { AXUIElement::new_system_wide() };

    let mut targets: Vec<(&str, &AXUIElement)> = vec![
        ("item", &element),
        ("app", &app_element),
        ("system-wide", &system_wide),
    ];
    if let Some(extras) = extras.as_deref() {
        targets.push(("extras bar", extras));
    }
    targets.extend(child_elements.iter().map(|c| ("child", &**c)));

    for (where_, target) in targets {
        for notif_name in notifications {
            match observer.watch(target, notif_name) {
                Ok(()) => println!("[on {where_}] watch({notif_name}) -> ok"),
                Err(err) => println!("[on {where_}] watch({notif_name}) -> {err}"),
            }
        }
    }
    println!("{} notifications registered", observer.watching());

    if let Some(run_loop) = CFRunLoop::current() {
        // SAFETY: the source is valid and `kCFRunLoopDefaultMode` matches how
        // this probe pumps below.
        unsafe {
            run_loop.add_source(Some(&observer.run_loop_source()), kCFRunLoopDefaultMode);
        }
    }

    println!(
        "pumping for 150s (pid {}); guarantees a real minute rollover no matter when this \
         started — wait for the clock to tick over a minute",
        std::process::id()
    );
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(150) {
        // SAFETY: plain CoreFoundation call on this thread's run loop.
        unsafe {
            CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, 1.0, true);
        }
    }
    println!("done");
}

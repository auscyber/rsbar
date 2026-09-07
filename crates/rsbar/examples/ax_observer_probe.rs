//! Spike: does `AXObserverAddNotification(kAXValueChangedNotification)` (and
//! `kAXTitleChangedNotification`) actually fire when a menu bar extra's
//! *contents* change — the Control Centre clock's displayed text, in
//! particular — as opposed to merely registering without error?
//!
//! Usage: `cargo run -p rsbar --example ax_observer_probe -- <owner> <name>`,
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
use objc2_application_services::{AXError, AXObserver, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{
    CFArray, CFRetained, CFRunLoop, CFString, CFType, CGPoint, CGRect, CGSize, Type,
    kCFRunLoopDefaultMode,
};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

fn attribute(element: &AXUIElement, name: &str) -> Option<CFRetained<CFType>> {
    let attr = CFString::from_str(name);
    let mut value: *const CFType = std::ptr::null();
    let err = unsafe { element.copy_attribute_value(&attr, NonNull::from(&mut value)) };
    if err != AXError::Success {
        return None;
    }
    let ptr = NonNull::new(value.cast_mut())?;
    Some(unsafe { CFRetained::from_raw(ptr) })
}
fn copy_element(element: &AXUIElement, name: &str) -> Option<CFRetained<AXUIElement>> {
    attribute(element, name)?.downcast::<AXUIElement>().ok()
}
fn copy_array(element: &AXUIElement, name: &str) -> Option<CFRetained<CFArray>> {
    attribute(element, name)?.downcast::<CFArray>().ok()
}
unsafe fn array_element(array: &CFArray, i: isize) -> Option<&AXUIElement> {
    let ptr = unsafe { array.value_at_index(i) };
    let ptr = NonNull::new(ptr.cast_mut())?.cast::<CFType>();
    let ty = unsafe { ptr.as_ref() };
    ty.downcast_ref::<AXUIElement>()
}
fn attribute_string(element: &AXUIElement, name: &str) -> Option<String> {
    Some(
        attribute(element, name)?
            .downcast::<CFString>()
            .ok()?
            .to_string(),
    )
}
fn attribute_value<const N: usize>(
    element: &AXUIElement,
    name: &str,
    the_type: AXValueType,
) -> Option<[u8; N]> {
    let value = attribute(element, name)?.downcast::<AXValue>().ok()?;
    let mut buf = [0u8; N];
    let ok = unsafe { value.value(the_type, NonNull::from(&mut buf).cast()) };
    ok.then_some(buf)
}
fn attribute_point(element: &AXUIElement, name: &str) -> Option<CGPoint> {
    let bytes = attribute_value::<{ size_of::<CGPoint>() }>(element, name, AXValueType::CGPoint)?;
    Some(unsafe { std::mem::transmute::<[u8; size_of::<CGPoint>()], CGPoint>(bytes) })
}
fn attribute_size(element: &AXUIElement, name: &str) -> Option<CGSize> {
    let bytes = attribute_value::<{ size_of::<CGSize>() }>(element, name, AXValueType::CGSize)?;
    Some(unsafe { std::mem::transmute::<[u8; size_of::<CGSize>()], CGSize>(bytes) })
}
fn attribute_frame(element: &AXUIElement) -> Option<CGRect> {
    if let Some(bytes) =
        attribute_value::<{ size_of::<CGRect>() }>(element, "AXFrame", AXValueType::CGRect)
    {
        return Some(unsafe { std::mem::transmute::<[u8; size_of::<CGRect>()], CGRect>(bytes) });
    }
    let position = attribute_point(element, "AXPosition")?;
    let size = attribute_size(element, "AXSize")?;
    Some(CGRect::new(position, size))
}

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
        let element = unsafe { AXUIElement::new_application(pid) };
        let Some(extras) = copy_element(&element, "AXExtrasMenuBar") else {
            println!("  no AXExtrasMenuBar for pid={pid}");
            continue;
        };
        let Some(children) =
            copy_array(&extras, "AXVisibleChildren").or_else(|| copy_array(&extras, "AXChildren"))
        else {
            println!("  no children for pid={pid}");
            continue;
        };
        println!("  {} extras children", children.count());
        for i in 0..children.count() {
            let Some(child) = (unsafe { array_element(&children, i) }) else {
                continue;
            };
            let title = attribute_string(child, "AXTitle");
            let desc = attribute_string(child, "AXDescription");
            let frame = attribute_frame(child);
            let value = attribute_string(child, "AXValue");
            let role = attribute_string(child, "AXRole");
            println!(
                "    child[{i}] role={role:?} title={title:?} desc={desc:?} value={value:?} frame={frame:?}"
            );
            let identity = title.as_deref().or(desc.as_deref());
            if identity == Some(name) {
                println!(
                    "found element: identity={identity:?} role={:?} frame={frame:?}",
                    attribute_string(child, "AXRole")
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

extern "C-unwind" fn on_notification(
    _observer: NonNull<AXObserver>,
    element: NonNull<AXUIElement>,
    notification: NonNull<CFString>,
    _refcon: *mut c_void,
) {
    // SAFETY: the callback contract guarantees a live CFString for the
    // duration of this call, and `element` is a live AXUIElement for the
    // duration of this call too.
    let name = unsafe { notification.as_ref() }.to_string();
    let element = unsafe { element.as_ref() };
    let desc = attribute_string(element, "AXDescription");
    let title = attribute_string(element, "AXTitle");
    let role = attribute_string(element, "AXRole");
    let value = attribute_string(element, "AXValue");
    let mut owner_pid: i32 = 0;
    let pid_err = unsafe { element.pid(NonNull::from(&mut owner_pid)) };
    println!(
        "[{:?}] AX notification fired: {name} owner_pid={owner_pid} ({pid_err:?}) role={role:?} title={title:?} desc={desc:?} value={value:?}",
        Instant::now()
    );
}

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
        rsbar::alias::accessibility_trusted()
    );

    let Some((pid, element)) = find(&owner, &name) else {
        eprintln!("no extras-menu-bar element matches owner={owner:?} title={name:?}");
        std::process::exit(1);
    };
    println!("pid={pid}");

    // Does the item itself have children (a sub-element that might accept a
    // notification even though the AXMenuBarItem parent flatly refuses one)?
    let children = copy_array(&element, "AXChildren");
    println!(
        "item AXChildren: {:?}",
        children.as_ref().map(|c| c.count())
    );
    let mut child_elements: Vec<CFRetained<AXUIElement>> = Vec::new();
    if let Some(children) = &children {
        for i in 0..children.count() {
            let Some(child) = (unsafe { array_element(children, i) }) else {
                continue;
            };
            println!(
                "  child[{i}] role={:?} title={:?} desc={:?} value={:?}",
                attribute_string(child, "AXRole"),
                attribute_string(child, "AXTitle"),
                attribute_string(child, "AXDescription"),
                attribute_string(child, "AXValue"),
            );
            child_elements.push(child.retain());
        }
    }

    let mut observer_ptr: *mut AXObserver = std::ptr::null_mut();
    // SAFETY: `pid` is live; `observer_ptr` is a valid out-pointer.
    let status =
        unsafe { AXObserver::create(pid, Some(on_notification), NonNull::from(&mut observer_ptr)) };
    println!("AXObserver::create -> {status:?}");
    let Some(observer_ptr) = NonNull::new(observer_ptr) else {
        eprintln!("observer is null");
        std::process::exit(1);
    };
    // SAFETY: a non-null result from AXObserverCreate carries a +1 reference.
    let observer = unsafe { CFRetained::from_raw(observer_ptr) };

    // SAFETY: `observer` stays alive for the process's remaining lifetime.
    let source = unsafe { observer.run_loop_source() };
    if let Some(run_loop) = CFRunLoop::current() {
        // SAFETY: `source` is a valid run loop source; `kCFRunLoopDefaultMode`
        // matches how this probe pumps below.
        unsafe {
            run_loop.add_source(Some(source.as_ref()), kCFRunLoopDefaultMode);
        }
    }

    for notif_name in notifications {
        let notif = CFString::from_str(notif_name);
        // SAFETY: `element` and `notif` are both live for the call; refcon is
        // unused by this probe.
        let status = unsafe { observer.add_notification(&element, &notif, std::ptr::null_mut()) };
        println!("[on item] add_notification({notif_name}) -> {status:?}");
    }

    for (i, child) in child_elements.iter().enumerate() {
        for notif_name in notifications {
            let notif = CFString::from_str(notif_name);
            // SAFETY: `child` and `notif` are both live for the call.
            let status = unsafe { observer.add_notification(child, &notif, std::ptr::null_mut()) };
            println!("[on child[{i}]] add_notification({notif_name}) -> {status:?}");
        }
    }

    // SAFETY: pid is live.
    let app_element = unsafe { AXUIElement::new_application(pid) };
    for notif_name in notifications {
        let notif = CFString::from_str(notif_name);
        let status =
            unsafe { observer.add_notification(&app_element, &notif, std::ptr::null_mut()) };
        println!("[on app] add_notification({notif_name}) -> {status:?}");
    }
    if let Some(extras) = copy_element(&app_element, "AXExtrasMenuBar") {
        for notif_name in notifications {
            let notif = CFString::from_str(notif_name);
            let status =
                unsafe { observer.add_notification(&extras, &notif, std::ptr::null_mut()) };
            println!("[on extras bar] add_notification({notif_name}) -> {status:?}");
        }
    }

    // SAFETY: takes no arguments.
    let system_wide = unsafe { AXUIElement::new_system_wide() };
    for notif_name in notifications {
        let notif = CFString::from_str(notif_name);
        let status =
            unsafe { observer.add_notification(&system_wide, &notif, std::ptr::null_mut()) };
        println!("[on system-wide] add_notification({notif_name}) -> {status:?}");
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

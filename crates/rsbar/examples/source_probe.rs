//! Spike: register one real event source through the actual [`Registry`] and
//! print what it emits.
//!
//! This is the daemon's own machinery, not a stand-in for it: the same
//! `Registry`, the same `Waker`, the same `CFRunLoopRunInMode` pump
//! `ecs::run` uses. If a source fires here, it fires in `rsbard`.
//!
//! ```text
//! cargo run -p rsbar --example source_probe -- brightness_changed
//! cargo run -p rsbar --example source_probe -- wifi_changed
//! cargo run -p rsbar --example source_probe -- space_changed
//! cargo run -p rsbar --example source_probe -- space_windows_changed
//! cargo run -p rsbar --example source_probe -- media_changed
//! ```
//!
//! `brightness_changed` and `space_changed` additionally trigger the real
//! event themselves a couple of seconds in, rather than waiting on a human:
//! brightness nudges the main display through the same `DisplayServices`
//! setter its sibling getter/observer come from, and spaces flips the active
//! space on the main display through `SLSManagedDisplaySetCurrentSpace` (the
//! same private call yabai uses) and flips it back a moment later. Both are
//! real, visible changes on the machine running this.
//!
//! `wifi_changed` expects something else to be power-cycling the radio
//! (`networksetup -setairportpower en0 off && sleep 2 && networksetup
//! -setairportpower en0 on`) concurrently. `media_changed` needs no trigger:
//! the point is that it refuses to register at all.

use bevy_ecs::entity::Entity;
use objc2_core_foundation::{
    CFRetained, CFRunLoop, CFRunLoopRunResult, CFString, kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{CGDirectDisplayID, CGError, CGMainDisplayID};
use rsbar::runloop::Waker;
use rsbar::sources::Registry;
use rsbar_protocol::Kind;
use std::ptr::NonNull;
use std::time::Duration;

#[link(name = "DisplayServices", kind = "framework")]
unsafe extern "C" {
    fn DisplayServicesGetBrightness(display: CGDirectDisplayID, brightness: *mut f32) -> CGError;
    fn DisplayServicesSetBrightness(display: CGDirectDisplayID, brightness: f32) -> CGError;
}

#[link(name = "SkyLight", kind = "framework")]
unsafe extern "C" {
    fn SLSManagedDisplaySetCurrentSpace(
        cid: skylight::ConnectionId,
        display_uuid: *const CFString,
        sid: skylight::SpaceId,
    ) -> CGError;
}

fn parse_kind(name: &str) -> Option<Kind> {
    Some(match name {
        "brightness_changed" => Kind::BrightnessChanged,
        "wifi_changed" => Kind::WifiChanged,
        "space_changed" => Kind::SpaceChanged,
        "space_windows_changed" => Kind::SpaceWindowsChanged,
        "media_changed" => Kind::MediaChanged,
        other => {
            eprintln!("unknown kind: {other}");
            return None;
        }
    })
}

/// Nudges the main display's brightness up, then back down, two seconds in —
/// enough of a real change for `DisplayServicesRegisterForBrightnessChangeNotifications`
/// to fire, and small enough not to leave the screen somewhere odd.
fn trigger_brightness() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(2));
        let display = CGMainDisplayID();
        let mut current: f32 = 0.5;
        // SAFETY: `current` is a valid f32 out-pointer for the call.
        unsafe { DisplayServicesGetBrightness(display, &raw mut current) };
        let nudged = (current + 0.05).min(1.0);
        println!("[trigger] brightness {current:.3} -> {nudged:.3}");
        // SAFETY: `display` is the live main display id.
        unsafe { DisplayServicesSetBrightness(display, nudged) };
        std::thread::sleep(Duration::from_millis(800));
        println!("[trigger] brightness {nudged:.3} -> {current:.3}");
        // SAFETY: as above.
        unsafe { DisplayServicesSetBrightness(display, current) };
    });
}

/// Flips the active space on the main display, then flips it back — a real,
/// visible space switch rather than a synthetic notification.
fn trigger_space_switch() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(2));
        // SAFETY: takes no arguments and always succeeds.
        let cid = unsafe { skylight::ffi::SLSMainConnectionID() };
        // SAFETY: `cid` is this process's connection id; the call hands back
        // a +1 `CFString` or null.
        let Some(uuid) =
            NonNull::new(unsafe { skylight::ffi::SLSCopyActiveMenuBarDisplayIdentifier(cid) })
        else {
            println!("[trigger] no active display identifier; cannot switch spaces");
            return;
        };
        // SAFETY: the Copy convention hands back a reference this now owns.
        let uuid = unsafe { CFRetained::<CFString>::from_raw(uuid) };
        // SAFETY: `cid` and `uuid` are both live for the call.
        let current = unsafe {
            skylight::ffi::SLSManagedDisplayGetCurrentSpace(cid, CFRetained::as_ptr(&uuid).as_ptr())
        };
        if current == 0 {
            println!("[trigger] could not read the current space");
            return;
        }

        // SAFETY: `cid` and `uuid` are live; the call reads the display's
        // space list.
        let Some(spaces) = NonNull::new(unsafe { skylight::ffi::SLSCopyManagedDisplaySpaces(cid) })
        else {
            println!("[trigger] could not enumerate spaces");
            return;
        };
        // SAFETY: the Copy convention hands back a reference this now owns.
        let _spaces = unsafe { CFRetained::<objc2_core_foundation::CFArray>::from_raw(spaces) };

        println!(
            "[trigger] current space is {current}; see `defaults read com.apple.spaces` for the sibling id to target"
        );
        let target = std::env::var("PROBE_TARGET_SPACE")
            .ok()
            .and_then(|s| s.parse::<u64>().ok());
        let Some(target) = target else {
            println!(
                "[trigger] set PROBE_TARGET_SPACE=<id64> to actually switch; skipping the real switch"
            );
            return;
        };

        println!("[trigger] space {current} -> {target}");
        // SAFETY: `cid` and `uuid` are live for the call.
        let status = unsafe {
            SLSManagedDisplaySetCurrentSpace(cid, CFRetained::as_ptr(&uuid).as_ptr(), target)
        };
        println!("[trigger] SLSManagedDisplaySetCurrentSpace -> {status:?}");
        std::thread::sleep(Duration::from_millis(500));
        // SAFETY: `cid` and `uuid` are live for the call.
        let now = unsafe {
            skylight::ffi::SLSManagedDisplayGetCurrentSpace(cid, CFRetained::as_ptr(&uuid).as_ptr())
        };
        println!("[trigger] SLSManagedDisplayGetCurrentSpace now reports {now} (wanted {target})");
        std::thread::sleep(Duration::from_secs(1));
        println!("[trigger] space {target} -> {current}");
        // SAFETY: as above.
        let status = unsafe {
            SLSManagedDisplaySetCurrentSpace(cid, CFRetained::as_ptr(&uuid).as_ptr(), current)
        };
        println!("[trigger] SLSManagedDisplaySetCurrentSpace -> {status:?}");
    });
}

fn main() {
    tracing_subscriber::fmt::init();
    let arg = std::env::args().nth(1).unwrap_or_default();
    let Some(kind) = parse_kind(&arg) else {
        eprintln!(
            "usage: source_probe <brightness_changed|wifi_changed|space_changed|space_windows_changed|media_changed>"
        );
        std::process::exit(1);
    };

    let waker = Waker::install(|| {});
    let config = rsbar::config::shared();
    let mut registry = Registry::new(config, waker.clone());

    let who = Entity::from_raw_u32(1).expect("valid entity index");
    let mut watch = registry.watch(who, &kind);
    registry.settle();
    println!(
        "registered: {} (running = {})",
        kind,
        registry.running(rsbar::sources::SourceId(source_id_for(&kind)))
    );

    // PROBE_RESTART=1 exercises the lazy stop/start cycle the registry puts
    // every source through: drop the only claim (stopping it), then take a
    // fresh one (starting it again) before triggering. A source that only
    // works the first time — e.g. a static `OnceCell` sink never refreshed on
    // re-registration — fails silently here.
    if std::env::var("PROBE_RESTART").as_deref() == Ok("1") {
        std::thread::sleep(Duration::from_millis(500));
        drop(watch);
        registry.settle();
        println!(
            "dropped the claim; running = {}",
            registry.running(rsbar::sources::SourceId(source_id_for(&kind)))
        );
        std::thread::sleep(Duration::from_millis(500));
        watch = registry.watch(who, &kind);
        registry.settle();
        println!(
            "re-claimed; running = {}",
            registry.running(rsbar::sources::SourceId(source_id_for(&kind)))
        );
    }

    match kind {
        Kind::BrightnessChanged => trigger_brightness(),
        Kind::SpaceChanged => trigger_space_switch(),
        _ => {}
    }

    println!("watching {kind} for 20s (Ctrl-C to stop early)...");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        // SAFETY: matches the daemon's own runner in `ecs::run`.
        let woke = unsafe {
            CFRunLoop::run_in_mode(
                kCFRunLoopDefaultMode,
                Duration::from_millis(200).as_secs_f64(),
                true,
            )
        };
        if matches!(
            woke,
            CFRunLoopRunResult::Stopped | CFRunLoopRunResult::Finished
        ) {
            break;
        }
        registry.drain(|id, event| {
            println!("[{id}] {event:?}");
        });
    }
    drop(watch);
    println!("done");
}

fn source_id_for(kind: &Kind) -> &'static str {
    match kind {
        Kind::BrightnessChanged => "brightness",
        Kind::WifiChanged => "wifi",
        Kind::SpaceChanged | Kind::SpaceWindowsChanged => "spaces",
        Kind::MediaChanged => "media",
        _ => "?",
    }
}

//! Spike: prove alias capture actually works on this machine.
#![allow(clippy::too_many_lines)]

use bevy_ecs::entity::Entity;
use bevy_ecs::world::World;
use objc2_core_foundation::{CFString, CFType, CFURLPathStyle};
use objc2_core_graphics::CGImage;
use rsbar::alias;
use rsbar::alias::{Alias, Captures, MenuBarItem, RawMenuBarWindow};
use std::ffi::c_void;
use std::process::Command;

#[skylight::main(also(time_the_scan, now, time_a_pass, capture_via_alias))]
fn main() {
    print_sw_vers();
    print_permissions();

    time_the_scan();

    let snapshot = match alias::Snapshot::now() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            println!("could not enumerate menu bar windows: {err}");
            return;
        }
    };
    print_raw_windows(&alias::diagnostics(&snapshot));

    let items = match alias::list_menu_bar_items(&snapshot) {
        Ok(items) => items,
        Err(err) => {
            println!("could not list aliasable items: {err}");
            return;
        }
    };
    print_items(&items);

    time_a_pass(&items);

    // Prefer a duplicate-disambiguated item (name ending "(n)") if one
    // exists, to prove the index scheme actually picks out one specific
    // window rather than just re-finding whichever matches first. A name
    // given on the command line wins, so a specific item can be checked.
    let wanted = std::env::args().nth(1);
    let target = wanted
        .as_ref()
        .and_then(|name| items.iter().find(|item| &item.name == name))
        .or_else(|| {
            items
                .iter()
                .find(|item| item.name.ends_with(')') && item.name.contains('('))
        })
        .or_else(|| items.first());

    let Some(target) = target else {
        println!(
            "No aliasable menu bar items found. On a machine where every extra is drawn by \
             Control Center and the Accessibility match above failed (or permission is \
             missing), this is the expected, reportable outcome rather than a bug."
        );
        return;
    };

    capture_via_alias(target);
}

/// One frame at 60 Hz. Nothing on the main thread may cost more than this.
const FRAME: std::time::Duration = std::time::Duration::from_micros(16_667);

/// Waits for `Captures`' shared window table (see `alias::Captures::table`)
/// to hold something. A real pass gets woken when a repair lands; this probe
/// has no run loop pumping it, so it polls `begin_pass`, which is what
/// collects one, instead.
fn poll_table(captures: &mut Captures, table: &std::sync::RwLock<alias::Snapshot>) {
    while alias::diagnostics(&table.read().expect("not poisoned")).is_empty() {
        captures.begin_pass();
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// The Accessibility walk, timed per application, with and without a
/// messaging timeout.
///
/// The two numbers this prints are the whole argument for both halves of the
/// fix: the timeout says how much of the cost was one or two applications
/// that had stopped answering, and the per-application breakdown says whether
/// what is left can ever fit in a frame (it cannot, which is why the walk
/// moved off this thread).
#[skylight::main_thread]
fn time_the_scan() {
    println!("=== the Accessibility walk, timed ===");
    for (label, timeout) in [
        ("API default (~6s per element)", None),
        ("bounded", Some(alias::AX_TIMEOUT)),
    ] {
        let mut per_app = alias::time_extras_scan(proof, timeout);
        let total: std::time::Duration = per_app.iter().map(|(_, _, d, _)| *d).sum();
        let found: usize = per_app.iter().map(|(_, _, _, n)| n).sum();
        per_app.sort_by_key(|(_, _, d, _)| std::cmp::Reverse(*d));
        println!(
            "{label}: {} app(s), {found} item(s), {:.1?} total",
            per_app.len(),
            total
        );
        for (name, pid, elapsed, items) in per_app.iter().take(5) {
            println!("    {elapsed:>10.1?}  pid {pid:<8} {items} item(s)  {name}");
        }
    }
    println!();
}

/// What one turn of `ecs::refresh_aliases` really costs on the main thread,
/// through the daemon's own [`Captures`] rather than a stand-in.
///
/// Two passes, because they cost wildly different things and only one of them
/// happens more than once. The first resolves and captures every alias — a
/// config run — and pays a window server screen capture plus a full pixel
/// hash per item. Every pass after it captures *nothing* unless an
/// Accessibility notification said an item changed, so its whole cost is a
/// table read -- see `alias::Captures::table`.
#[skylight::main_thread]
fn time_a_pass(items: &[MenuBarItem]) {
    println!("=== refresh passes on the main thread ===");
    let mut world = World::new();
    let entities: Vec<(Entity, String)> = items
        .iter()
        .map(|item| {
            (
                world.spawn_empty().id(),
                format!("{},{}", item.owner, item.name),
            )
        })
        .collect();

    // Nothing pumps this run loop, which is fine: the probe never waits for a
    // scan, it only measures what the main thread pays.
    let mut captures = Captures::new(rsbar::runloop::Waker::install(proof, || {}));

    // The window list is a table `Captures` keeps and repairs now, not a
    // fresh fetch per pass -- see `alias::Captures::table`. Force the first
    // repair explicitly, the way a launch or a grant would, then wait for it
    // to land the way a real pass gets woken to collect one.
    let table = captures.table();
    captures.rescan();
    poll_table(&mut captures, &table);

    let mut worst = std::time::Duration::ZERO;
    for pass in 1..=4 {
        let started = std::time::Instant::now();
        let snapshot = table.read().expect("not poisoned");
        let listed = started.elapsed();
        let mut drawn = 0;
        for (entity, spec) in &entities {
            if captures
                .refresh(proof, *entity, spec, &snapshot, rsbar::alias::Look::Resolve)
                .is_some()
            {
                drawn += 1;
            }
        }
        drop(snapshot);
        let elapsed = started.elapsed();
        println!(
            "  pass {pass}: {elapsed:>8.1?}  (table read {listed:.1?}, {drawn} of {} \
             re-captured)",
            entities.len()
        );
        if pass > 1 {
            worst = worst.max(elapsed);
        }
    }

    // What pass 1 was actually paying for, on its own: the window server
    // screen capture and the pixel hash behind it. Nothing in this stage
    // touched that path, and it is what a config run costs. A fresh,
    // synchronous list here rather than the shared table: this half tests
    // raw `Alias`/`capture_now`, not `Captures`.
    let snapshot = alias::Snapshot::now(proof).expect("the list worked a moment ago");
    let mut warm: Vec<Alias> = items
        .iter()
        .map(|item| Alias::new(item.owner.clone(), item.name.clone()))
        .collect();
    // One acquisition for the whole probe: this is the only thing running, so
    // holding the connection throughout is what the daemon's frame does in
    // miniature.
    let connected = rsbar::pool::handle().block_on(skylight::acquire());
    let mut ok_count = 0;
    for alias in &mut warm {
        if alias.capture_now(&connected, &snapshot).is_some() {
            ok_count += 1;
        }
    }
    let started = std::time::Instant::now();
    for alias in &mut warm {
        let _ = alias.capture_now(&connected, &snapshot);
    }
    let each = started.elapsed() / ok_count.max(1);
    println!("  one warm capture, averaged over {ok_count}: {each:.1?}");

    // Where that goes, since it is now the largest main-thread cost left.
    let ids: Vec<_> = rsbar::alias::diagnostics(&snapshot)
        .into_iter()
        .filter(|w| !w.name.is_empty())
        .map(|w| w.id)
        .collect();
    let mut pixels = std::time::Duration::ZERO;
    let mut rect = std::time::Duration::ZERO;
    // Measuring what these cost on this thread is the whole point of the
    // probe -- it is the number that says why they belong on a worker.
    #[expect(
        clippy::disallowed_methods,
        reason = "timing the main-thread cost is what this measures"
    )]
    for id in &ids {
        let started = std::time::Instant::now();
        let _ = skylight::capture(&connected, *id);
        pixels += started.elapsed();
        let started = std::time::Instant::now();
        let _ = skylight::true_rect(&connected, *id);
        rect += started.elapsed();
    }
    let n = u32::try_from(ids.len().max(1)).unwrap_or(1);
    println!(
        "    of which Window::capture {:.1?}, Window::true_rect {:.1?}",
        pixels / n,
        rect / n
    );

    println!("  one frame is {FRAME:.1?}; worst steady-state pass {worst:.1?}");
    assert!(
        worst < FRAME,
        "a steady-state refresh pass must fit in one frame"
    );
    println!();
}

fn print_sw_vers() {
    match Command::new("sw_vers").output() {
        Ok(out) => print!("{}", String::from_utf8_lossy(&out.stdout)),
        Err(err) => println!("could not run sw_vers: {err}"),
    }
}

fn print_permissions() {
    let screen_capture = alias::screen_capture_trusted();
    let accessibility = alias::accessibility_trusted().is_ok();
    println!("Screen Recording permission: {screen_capture}");
    println!("Accessibility permission:    {accessibility}");
    if !screen_capture {
        println!(
            "-> no Screen Recording permission: capture will fail with Error::Window(NoCapture). \
             Requesting it now (grant it in System Settings, then re-run)."
        );
        let _ = alias::request_screen_capture();
    }
    if !accessibility {
        println!(
            "-> no Accessibility permission: Control Center items cannot be resolved to a real \
             owner/name. Requesting it now (grant it in System Settings, then re-run)."
        );
        let _ = alias::request_accessibility();
    }
    println!();
}

fn print_raw_windows(raw: &[RawMenuBarWindow]) {
    println!("{} window(s) at the menu bar layer:", raw.len());
    for w in raw {
        println!(
            "  id={:<6} owner={:<24?} name={:<24?} pid={:<8} bounds={:?}",
            w.id, w.owner, w.name, w.pid, w.bounds
        );
    }
    println!();
}

fn print_items(items: &[MenuBarItem]) {
    println!("{} aliasable item(s) (named, owner resolved):", items.len());
    for item in items {
        println!("  {},{},{}", item.owner, item.name, item.pid);
    }
    println!();
}

#[skylight::main_thread]
fn capture_via_alias(target: &MenuBarItem) {
    println!("Capturing {},{} ...", target.owner, target.name);
    let mut item = Alias::new(target.owner.clone(), target.name.clone());
    let snapshot = match alias::Snapshot::now(proof) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            println!("could not list the menu bar layer: {err}");
            return;
        }
    };
    let connected = rsbar::pool::handle().block_on(skylight::acquire());
    match item.capture_now(&connected, &snapshot) {
        Some(capture) => {
            println!(
                "Captured {}x{} points, image {}x{}px.",
                capture.size.width,
                capture.size.height,
                CGImage::width(Some(&capture.image)),
                CGImage::height(Some(&capture.image))
            );
            let out = "/private/tmp/claude-501/-Users-ivypierlot-code-rust-sketchybar/e1bcf168-6e1e-42ce-8f66-2653a528a79b/scratchpad/alias_capture.png";
            match write_png(&capture.image, out) {
                Ok(()) => println!("Wrote {out}"),
                Err(err) => println!("failed to write PNG: {err}"),
            }
        }
        None => println!("capture failed"),
    }
}

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    fn CGImageDestinationCreateWithURL(
        url: *const c_void,
        kind: *const c_void,
        count: usize,
        options: *const c_void,
    ) -> *mut c_void;
    fn CGImageDestinationAddImage(
        dest: *mut c_void,
        image: *const c_void,
        properties: *const c_void,
    );
    fn CGImageDestinationFinalize(dest: *mut c_void) -> bool;
}

fn write_png(image: &CGImage, path: &str) -> Result<(), String> {
    let cf_path = CFString::from_str(path);
    let url = objc2_core_foundation::CFURL::with_file_system_path(
        None,
        Some(&cf_path),
        CFURLPathStyle::CFURLPOSIXPathStyle,
        false,
    )
    .ok_or("could not build a file URL")?;
    let uti = CFString::from_static_str("public.png");

    // SAFETY: `url` and `uti` are both live `CFType`s for the duration of the
    // call; `options` and `properties` are optional per ImageIO's contract.
    let dest = unsafe {
        CGImageDestinationCreateWithURL(
            (&raw const *url).cast::<c_void>(),
            (&raw const *uti).cast::<c_void>(),
            1,
            std::ptr::null(),
        )
    };
    if dest.is_null() {
        return Err("CGImageDestinationCreateWithURL returned null".into());
    }

    // SAFETY: `dest` was just created, non-null, and `image` outlives the call.
    unsafe {
        CGImageDestinationAddImage(
            dest,
            (&raw const **image).cast::<c_void>(),
            std::ptr::null(),
        );
    }
    // SAFETY: `dest` is a valid, non-finalized destination.
    let ok = unsafe { CGImageDestinationFinalize(dest) };
    // SAFETY: `dest` carries a +1 reference this function owns.
    unsafe { skylight::ffi::CFRelease(dest.cast::<CFType>()) };

    if ok {
        Ok(())
    } else {
        Err("CGImageDestinationFinalize failed".into())
    }
}

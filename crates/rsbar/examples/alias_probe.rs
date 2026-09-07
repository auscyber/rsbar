//! Spike: prove alias capture actually works on this machine.
//!
//! Not wired into the crate (`alias.rs` isn't declared as a module in
//! `lib.rs` yet), so this pulls the same source file in directly to exercise
//! the real code rather than a copy of it.
#![allow(clippy::too_many_lines)]
#[path = "../src/alias.rs"]
mod alias;

use alias::{Alias, MenuBarItem, RawMenuBarWindow};
use objc2_core_foundation::{CFRetained, CFString, CFType, CFURLPathStyle};
use objc2_core_graphics::CGImage;
use std::ffi::c_void;
use std::process::Command;

fn main() {
    print_sw_vers();
    print_permissions();

    let raw = match alias::diagnostics() {
        Ok(raw) => raw,
        Err(err) => {
            println!("could not enumerate menu bar windows: {err}");
            return;
        }
    };
    print_raw_windows(&raw);

    let items = match alias::list_menu_bar_items() {
        Ok(items) => items,
        Err(err) => {
            println!("could not list aliasable items: {err}");
            return;
        }
    };
    print_items(&items);

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

fn print_sw_vers() {
    match Command::new("sw_vers").output() {
        Ok(out) => print!("{}", String::from_utf8_lossy(&out.stdout)),
        Err(err) => println!("could not run sw_vers: {err}"),
    }
}

fn print_permissions() {
    let screen_capture = alias::screen_capture_trusted();
    let accessibility = alias::accessibility_trusted();
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

fn capture_via_alias(target: &MenuBarItem) {
    println!("Capturing {},{} ...", target.owner, target.name);
    let mut item = Alias::new(target.owner.clone(), target.name.clone());
    match item.capture() {
        Ok(capture) => {
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
        Err(err) => println!("capture failed: {err}"),
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

fn write_png(image: &CFRetained<CGImage>, path: &str) -> Result<(), String> {
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

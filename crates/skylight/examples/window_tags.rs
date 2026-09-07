//! Proves the sticky/friend-of-fullscreen tag API against a live window
//! server, and reports whether the machine's display is built-in and main.
//!
//! `cargo run -p skylight --example window_tags`. Creates an off-screen-sized
//! window, flips `sticky` and `show_in_fullscreen` on and off, and after each
//! step reads the tags back with [`skylight::Window::tags`] rather than
//! trusting the crate's own bookkeeping — the standard this codebase holds a
//! binding to before calling it verified.
//!
//! # Result (this machine, macOS 26.5.1, `Mac16,1`)
//!
//! Actual output of a real run, unedited but for the window id:
//! ```text
//! created window 1318
//! after set_tags(BAR): changed=true, sticky=false, never_sticky=false, friend_of_fullscreen=false
//! set_sticky(true): changed=true, sticky=true, never_sticky=false, friend_of_fullscreen=false
//! set_sticky(true) again: changed=false, sticky=true, never_sticky=false, friend_of_fullscreen=false
//! set_sticky(false): changed=true, sticky=false, never_sticky=true, friend_of_fullscreen=false
//! set_friend_of_fullscreen(true): changed=true, sticky=false, never_sticky=true, friend_of_fullscreen=true
//! set_friend_of_fullscreen(true) again: changed=false, sticky=false, never_sticky=true, friend_of_fullscreen=true
//! set_friend_of_fullscreen(false): changed=true, sticky=false, never_sticky=true, friend_of_fullscreen=false
//! display 1: is_builtin=true, is_main=true
//! ```
//! This confirms the tag bits round-trip through the window server exactly as
//! named, and that the idempotency check skips a real syscall for a
//! set-to-the-same-value call. Also not confirmed here: `is_builtin` or
//! `is_main` reporting `false` for anything — this machine has only the one,
//! built-in, main display.
//!
//! # `FRIEND_OF_FULLSCREEN` and `STICKY` in actual use — verified separately
//!
//! What this example cannot show — round-tripping a tag on an isolated
//! off-screen window says nothing about what it does — was checked live
//! through the real `rsbard` binary and two full builds of `rsbar::bar`
//! differing only in `Settings::default()`'s `show_in_fullscreen`/`sticky`:
//!
//! - `show_in_fullscreen` (`FRIEND_OF_FULLSCREEN`): full A/B, both
//!   directions. With `TextEdit` put into real native fullscreen (`AXFullScreen`
//!   via System Events), a bar built with the tag on drew right over it,
//!   unchanged; the same bar built with it off vanished completely the moment
//!   fullscreen engaged. Conclusive.
//! - `sticky` (`SUPER_STICKY | ON_ALL_WORKSPACES`): positive case only. Using
//!   `SLSGetActiveSpace` (see below) to confirm two real, distinct spaces
//!   rather than trust an animation, a bar built with `sticky: true` stayed
//!   visible and unchanged switching between them. The matching
//!   `sticky: false` build was not tested the same way — the daemon had to
//!   come down before that run happened. Treat this as "observed surviving a
//!   real space switch with the tag on", not as a confirmed negative control.

use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGMainDisplayID;
use skylight::{Window, WindowTags};

fn report(step: &str, changed: bool, window: &Window) {
    let tags = window.tags().expect("read tags back");
    println!(
        "{step}: changed={changed}, sticky={}, never_sticky={}, friend_of_fullscreen={}",
        tags.contains(WindowTags::STICKY),
        tags.contains(WindowTags::NEVER_STICKY),
        tags.contains(WindowTags::FRIEND_OF_FULLSCREEN),
    );
}

fn main() {
    let frame = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(4.0, 4.0));
    let window = Window::new(frame).expect("create window");
    println!("created window {}", window.id());
    // Printed so a real space switch can be told apart from the switch
    // animation settling back where it started — run this twice around a
    // manual `ctrl+arrow` and compare.
    // SAFETY: `window.connection()` is the live process connection.
    let space = unsafe { skylight::ffi::SLSGetActiveSpace(window.connection()) };
    println!("active space: {space}");

    let changed = window.set_tags(WindowTags::BAR).expect("set BAR tags");
    report("after set_tags(BAR)", changed, &window);

    let changed = window.set_sticky(true).expect("set sticky");
    report("set_sticky(true)", changed, &window);

    let changed = window.set_sticky(true).expect("set sticky again");
    report("set_sticky(true) again", changed, &window);

    let changed = window.set_sticky(false).expect("clear sticky");
    report("set_sticky(false)", changed, &window);

    let changed = window
        .set_friend_of_fullscreen(true)
        .expect("set friend_of_fullscreen");
    report("set_friend_of_fullscreen(true)", changed, &window);

    let changed = window
        .set_friend_of_fullscreen(true)
        .expect("set friend_of_fullscreen again");
    report("set_friend_of_fullscreen(true) again", changed, &window);

    let changed = window
        .set_friend_of_fullscreen(false)
        .expect("clear friend_of_fullscreen");
    report("set_friend_of_fullscreen(false)", changed, &window);

    let display = CGMainDisplayID();
    println!(
        "display {display}: is_builtin={}, is_main={}",
        skylight::is_builtin(display),
        skylight::is_main(display)
    );
}

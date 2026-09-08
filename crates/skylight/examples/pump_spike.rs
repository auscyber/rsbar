//! Gate for the Bevy move: does a window server window still composite when
//! the run loop is pumped by repeated `run_in_mode` instead of one `run()`?
//!
//! M1 established that a window only composites while its process pumps a run
//! loop — drawing then blocking in `sleep` succeeds at every call and never
//! appears. A Bevy runner cannot call `CFRunLoop::run()`, because it has to get
//! control back to tick the schedules. It would instead block in
//! `run_in_mode(.., return_after_source_handled = true)` and tick on each wake.
//!
//! This draws once, then does exactly that, doing no work per wake.
//!
//! Result: **it passes.** The bar renders and is still composited a minute
//! later, and the process is genuinely asleep — `ps` reports 0.0% CPU, and 20
//! seconds of Time Profiler sampling caught it on-CPU zero times ("No stack
//! counts found", which is the answer rather than a failure).

use objc2_core_foundation::{
    CFRunLoop, CFRunLoopRunResult, CGPoint, CGRect, CGSize, kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{CGContext, CGDisplayBounds, CGMainDisplayID};
use skylight::{Window, WindowTags, level};
use std::time::Duration;

const HEIGHT: f64 = 40.0;
/// Long enough that a quiet minute costs a handful of wakes, short enough to
/// stand in for a timer the real runner would compute from the next tick.
const TIMEOUT: f64 = 5.0;
/// Long enough to show the window is not merely surviving a moment.
const RUN_FOR: Duration = Duration::from_mins(1);

fn main() {
    let bounds = CGDisplayBounds(CGMainDisplayID());
    let frame = CGRect::new(bounds.origin, CGSize::new(bounds.size.width, HEIGHT));

    let mtm = objc2::MainThreadMarker::new().expect("an example runs on the main thread");
    let window = Window::new(frame, mtm).expect("create");
    window.set_scale(2.0).expect("scale");
    window.set_opaque(false).expect("opaque");
    window.set_alpha(1.0).expect("alpha");
    window.set_level(level::STATUS).expect("level");
    window
        .set_tags((WindowTags::BAR - WindowTags::AVOIDS_CAPTURE) | WindowTags::IGNORE_FOR_EVENTS)
        .expect("tags");
    window.order_above(None).expect("order");

    skylight::draw(&window, frame.size, |ctx| {
        CGContext::set_rgb_fill_color(Some(ctx), 0.05, 0.05, 0.08, 0.9);
        CGContext::fill_rect(Some(ctx), CGRect::new(CGPoint::new(0.0, 0.0), frame.size));
        CGContext::set_rgb_fill_color(Some(ctx), 0.4, 1.0, 0.5, 1.0);
        CGContext::fill_rect(
            Some(ctx),
            CGRect::new(CGPoint::new(24.0, 10.0), CGSize::new(200.0, 20.0)),
        );
    });

    let start = std::time::Instant::now();
    let mut wakes = 0u32;
    println!(
        "pumping with run_in_mode for 60s (pid {})",
        std::process::id()
    );

    while start.elapsed() < RUN_FOR {
        // SAFETY: plain CoreFoundation call on this thread's run loop.
        let result = unsafe { CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, TIMEOUT, true) };
        if result == CFRunLoopRunResult::HandledSource {
            wakes += 1;
        }
        // A real runner would call app.update() here. Deliberately nothing, so
        // any CPU measured is the pump itself and not the work.
    }

    println!("done after {wakes} source wakes");
}

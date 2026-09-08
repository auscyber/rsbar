//! Smoke test for the whole private-API path: put a bar on screen.
//!
//! Run with `cargo run -p skylight --example bar_window`. A translucent strip
//! should appear across the top of the main display, above the menu bar, and
//! stay there for ten seconds — including if you switch spaces or fullscreen an
//! app. If it does, every hard part of the window server work is correct.

use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColor, CGDisplayBounds, CGMainDisplayID};
use objc2_quartz_core::CALayer;
use skylight::{Window, WindowTags, level};

const BAR_HEIGHT: f64 = 40.0;

fn main() {
    let display = CGMainDisplayID();
    let bounds = CGDisplayBounds(display);
    let frame = CGRect::new(
        CGPoint::new(bounds.origin.x, bounds.origin.y),
        CGSize::new(bounds.size.width, BAR_HEIGHT),
    );

    let mtm = objc2::MainThreadMarker::new().expect("an example runs on the main thread");
    let window = Window::new(frame, mtm).expect("create window");
    window.set_scale(2.0).expect("scale");
    window.set_opaque(false).expect("opacity");
    window.set_alpha(1.0).expect("alpha");
    window.set_level(level::STATUS).expect("level");
    // `WindowTags::BAR` includes AVOIDS_CAPTURE, which would also hide the bar
    // from screencapture — so this smoke test can be verified visually.
    window
        .set_tags((WindowTags::BAR - WindowTags::AVOIDS_CAPTURE) | WindowTags::IGNORE_FOR_EVENTS)
        .expect("tags");

    let root = CALayer::new();
    skylight::without_implicit_animations(mtm, || {
        root.setFrame(CGRect::new(CGPoint::new(0.0, 0.0), frame.size));
        root.setContentsScale(2.0);
        root.setBackgroundColor(Some(&CGColor::new_srgb(0.05, 0.05, 0.08, 0.85)));

        // A second layer, so we can see that sublayers composite and that the
        // y-flip in `present` puts things where they belong.
        let pill = CALayer::new();
        pill.setFrame(CGRect::new(
            CGPoint::new(20.0, 8.0),
            CGSize::new(120.0, 24.0),
        ));
        pill.setContentsScale(2.0);
        pill.setCornerRadius(12.0);
        pill.setBackgroundColor(Some(&CGColor::new_srgb(0.35, 0.65, 1.0, 1.0)));
        root.addSublayer(&pill);
    });

    // Order in *before* drawing: the window server hands out a drawing context
    // for a window that is on screen.
    window.order_above(None).expect("order in");
    skylight::present(&window, frame.size, &root);

    println!(
        "window {} up on display {display} at {frame:?}",
        window.id()
    );
    println!("holding 10s");
    // A window server window only composites while its process pumps a run
    // loop. Without this the drawing above stays queued and never appears.
    unsafe {
        objc2_core_foundation::CFRunLoop::run_in_mode(
            objc2_core_foundation::kCFRunLoopDefaultMode,
            10.0,
            false,
        );
    }
    println!("done");
}

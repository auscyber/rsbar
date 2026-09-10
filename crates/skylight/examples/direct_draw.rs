//! Minimal test: can we put pixels into a window server window at all?
//! No `CALayer`, no layer tree — just fill a rect in the raw context.

use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGContext, CGDisplayBounds, CGMainDisplayID};
use skylight::{Window, WindowTags, ffi, level};
use std::ptr;

fn main() {
    let bounds = CGDisplayBounds(CGMainDisplayID());
    let frame = CGRect::new(
        CGPoint::new(bounds.origin.x + 200.0, bounds.origin.y + 300.0),
        CGSize::new(600.0, 120.0),
    );

    let mtm = objc2::MainThreadMarker::new().expect("an example runs on the main thread");
    let window = Window::new(frame, mtm).expect("create");

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("build a runtime");
    let cid = rt.block_on(async {
        let connected = skylight::acquire().await;
        window.set_scale(&connected, 1.0).expect("scale");
        window.set_opaque(&connected, false).expect("opaque");
        window.set_alpha(&connected, 1.0).expect("alpha");
        window
            .set_level(&connected, level::FLOATING)
            .expect("level");
        window
            .set_tags(
                &connected,
                WindowTags::FLOATING | WindowTags::IGNORE_FOR_EVENTS,
            )
            .expect("tags");
        window.order_above(&connected, None).expect("order");
        connected.id()
    });

    unsafe {
        let ctx = ffi::SLWindowContextCreate(cid, window.id(), ptr::null_mut());
        assert!(!ctx.is_null(), "no context");
        let ctx = &*ctx;

        CGContext::set_rgb_fill_color(Some(ctx), 1.0, 0.0, 0.0, 1.0);
        CGContext::fill_rect(Some(ctx), CGRect::new(CGPoint::new(0.0, 0.0), frame.size));
        CGContext::flush(Some(ctx));
        let err = ffi::SLSFlushWindowContentRegion(cid, window.id(), ptr::null_mut());
        println!("window {} flush -> {err:?}", window.id());
    }

    // A window server window only composites while its process pumps a run
    // loop; a bare sleep leaves the drawing queued and invisible.
    unsafe {
        objc2_core_foundation::CFRunLoop::run_in_mode(
            objc2_core_foundation::kCFRunLoopDefaultMode,
            8.0,
            false,
        );
    }
}

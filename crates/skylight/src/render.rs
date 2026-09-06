use crate::ffi::{self, ConnectionId, WindowId};
use objc2_core_foundation::{CFType, CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGContext;
use objc2_quartz_core::{CALayer, CATransaction};
use std::ptr;

/// Draws into a window server window through a raw context.
///
/// The context is already flipped to a top-left origin, so a caller works in
/// the same coordinate space as layer geometry and as the frames it asked the
/// window server for.
///
/// Everything drawn is published when `f` returns; nothing is visible before.
pub fn draw<R>(window: WindowId, size: CGSize, f: impl FnOnce(&CGContext) -> R) -> Option<R> {
    // SAFETY: `window` is a live window on this connection, and the context is
    // released on every path that created one.
    unsafe {
        let cid = crate::window::connection_id();
        let ctx = ffi::SLWindowContextCreate(cid, window, ptr::null_mut());
        if ctx.is_null() {
            tracing::warn!(id = window, "window server returned no drawing context");
            return None;
        }
        let ctx = &*ctx;

        CGContext::clear_rect(Some(ctx), CGRect::new(CGPoint::new(0.0, 0.0), size));
        CGContext::save_g_state(Some(ctx));
        CGContext::translate_ctm(Some(ctx), 0.0, size.height);
        CGContext::scale_ctm(Some(ctx), 1.0, -1.0);

        let result = f(ctx);

        CGContext::restore_g_state(Some(ctx));
        CGContext::flush(Some(ctx));
        ffi::SLSFlushWindowContentRegion(cid, window, ptr::null_mut());
        ffi::CFRelease(ptr::from_ref(ctx).cast_mut().cast::<CFType>());
        Some(result)
    }
}

/// Rasterises a layer tree into a window server window.
///
/// The context is created and released per present rather than held: it is
/// cheap, and a cached one goes stale the moment the window is reshaped.
///
/// The y-flip is not optional. A window server context has its origin at the
/// bottom left; `CALayer` geometry runs top-down. Without the flip the bar
/// renders upside down.
pub fn present(window: WindowId, size: CGSize, layer: &CALayer) {
    // SAFETY: `window` is a live window on this connection. The context is
    // released before returning on every path that created one.
    unsafe {
        let cid: ConnectionId = crate::window::connection_id();
        let ctx = ffi::SLWindowContextCreate(cid, window, ptr::null_mut());
        if ctx.is_null() {
            tracing::warn!(id = window, "window server returned no drawing context");
            return;
        }
        let ctx = &*ctx;

        CGContext::clear_rect(Some(ctx), CGRect::new(CGPoint::new(0.0, 0.0), size));
        CGContext::save_g_state(Some(ctx));
        CGContext::translate_ctm(Some(ctx), 0.0, size.height);
        CGContext::scale_ctm(Some(ctx), 1.0, -1.0);
        layer.renderInContext(ctx);
        CGContext::restore_g_state(Some(ctx));
        CGContext::flush(Some(ctx));

        // Drawing into the context is not enough; this is what publishes it.
        ffi::SLSFlushWindowContentRegion(cid, window, ptr::null_mut());
        ffi::CFRelease(ptr::from_ref(ctx).cast_mut().cast::<CFType>());
    }
}

/// Applies layer mutations without Core Animation's implicit animations.
///
/// Every `CALayer` property assignment otherwise animates over a quarter of a
/// second, which for a bar that repaints on a timer reads as smear.
pub fn without_implicit_animations<R>(f: impl FnOnce() -> R) -> R {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            CATransaction::commit();
        }
    }

    CATransaction::begin();
    CATransaction::setDisableActions(true);
    let _guard = Guard;
    f()
}

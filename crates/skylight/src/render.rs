use crate::ffi::{self, ConnectionId, WindowId};
use objc2_core_foundation::{CFType, CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGContext;
use objc2_quartz_core::{CALayer, CATransaction};
use std::ptr;
use std::ptr::NonNull;

/// Draws into a window server window through a raw context.
///
/// The context is already flipped to a top-left origin, so a caller works in
/// the same coordinate space as layer geometry and as the frames it asked the
/// window server for.
///
/// Everything drawn is published when `f` returns; nothing is visible before.
pub fn draw<R>(window: WindowId, size: CGSize, f: impl FnOnce(&CGContext) -> R) -> Option<R> {
    draw_damaged(window, size, None, f)
}

/// The same, over part of the window only.
///
/// `damage` is the rects that may change, in the same top-left space as the
/// frames a caller lays out in. They are cleared and clipped to, so anything
/// `f` draws outside them is discarded and the pixels there survive untouched.
///
/// `None` redraws the whole window: the entire surface is cleared first, so a
/// caller that does not know what changed must draw everything back.
///
/// This is what stops a bar flickering. Clearing the whole window and painting
/// every item back means every item is re-rasterised whenever any one of them
/// changes, and a clock ticking once a second is enough to make the static
/// items beside it shimmer.
pub fn draw_damaged<R>(
    window: WindowId,
    size: CGSize,
    damage: Option<&[CGRect]>,
    f: impl FnOnce(&CGContext) -> R,
) -> Option<R> {
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

        CGContext::save_g_state(Some(ctx));
        // The clear happens inside the flip, so the rects a caller damaged are
        // in the space it laid them out in rather than the window server's.
        CGContext::translate_ctm(Some(ctx), 0.0, size.height);
        CGContext::scale_ctm(Some(ctx), 1.0, -1.0);

        match damage {
            None => CGContext::clear_rect(Some(ctx), CGRect::new(CGPoint::new(0.0, 0.0), size)),
            Some(rects) => {
                let Some(first) = NonNull::new(rects.as_ptr().cast_mut()) else {
                    // Nothing to redraw. A caller should not have asked, but
                    // clipping to an empty list would clip to everything.
                    CGContext::restore_g_state(Some(ctx));
                    ffi::CFRelease(ptr::from_ref(ctx).cast_mut().cast::<CFType>());
                    return None;
                };
                CGContext::clip_to_rects(Some(ctx), first, rects.len());
                for rect in rects {
                    CGContext::clear_rect(Some(ctx), *rect);
                }
            }
        }

        let result = f(ctx);

        CGContext::restore_g_state(Some(ctx));
        CGContext::flush(Some(ctx));
        // The whole window is still published. Only the damaged pixels differ
        // from what is already there, so republishing the rest costs nothing
        // and saves building a region the window server has to agree with.
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

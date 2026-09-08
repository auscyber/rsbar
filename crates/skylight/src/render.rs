use crate::ffi::{self, ConnectionId, WindowId};
use crate::window::Window;
use objc2_core_foundation::{CFType, CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGContext;
use objc2_quartz_core::{CALayer, CATransaction};
use std::ptr;
use std::ptr::NonNull;

/// A window server drawing context, released when it goes out of scope.
///
/// `SLWindowContextCreate` hands back a +1 reference that has to be
/// `CFRelease`d on every path -- which previously meant three release calls
/// per function and one more on an early return, each of them a chance to
/// leak a context per repaint. Holding it is what makes that impossible.
struct Context {
    ctx: NonNull<CGContext>,
}

impl Context {
    /// The window's context, or nothing if the window server declined.
    fn create(cid: ConnectionId, window: WindowId) -> Option<Self> {
        // SAFETY: `window` is a live window on this connection; the returned
        // reference is owned here and released in `Drop`.
        let ctx = unsafe { ffi::SLWindowContextCreate(cid, window, ptr::null_mut()) };
        let Some(ctx) = NonNull::new(ctx) else {
            tracing::warn!(id = window, "window server returned no drawing context");
            return None;
        };
        Some(Self { ctx })
    }

    fn get(&self) -> &CGContext {
        // SAFETY: created above and live until this is dropped.
        unsafe { self.ctx.as_ref() }
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: the +1 reference `create` took, released exactly once.
        unsafe { ffi::CFRelease(self.ctx.as_ptr().cast::<CFType>()) };
    }
}

/// A saved graphics state, restored when it goes out of scope.
///
/// The flip a caller draws under is a CTM change, and leaving it applied would
/// leak into whatever drew next through the same context. So would a fill
/// colour, or a text position — which is why this is public rather than
/// private to this module: anything drawing into a context borrowed from
/// somewhere else has the same obligation, and an early return is what makes
/// remembering to restore by hand a thing that eventually gets forgotten.
pub struct SavedState<'a>(&'a CGContext);

impl<'a> SavedState<'a> {
    /// Saves the context's state, restoring it when the guard drops.
    #[must_use]
    pub fn save(ctx: &'a CGContext) -> Self {
        CGContext::save_g_state(Some(ctx));
        Self(ctx)
    }
}

impl Drop for SavedState<'_> {
    fn drop(&mut self) {
        CGContext::restore_g_state(Some(self.0));
    }
}

/// Draws into a window server window through a raw context.
///
/// The context is already flipped to a top-left origin, so a caller works in
/// the same coordinate space as layer geometry and as the frames it asked the
/// window server for.
///
/// Everything drawn is published when `f` returns; nothing is visible before.
/// Taking the window itself, not its id: a [`Window`] can only have been made
/// on the main thread and cannot leave it, so the argument is the proof and
/// there is nothing for a caller to pass or declare. See [`Window`]'s own doc
/// comment for the two halves of that.
pub fn draw<R>(window: &Window, size: CGSize, f: impl FnOnce(&CGContext) -> R) -> Option<R> {
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
/// As [`draw`], the window itself is the main-thread proof.
pub fn draw_damaged<R>(
    window: &Window,
    size: CGSize,
    damage: Option<&[CGRect]>,
    f: impl FnOnce(&CGContext) -> R,
) -> Option<R> {
    let cid = crate::window::connection_id();
    let context = Context::create(cid, window.id())?;
    let ctx = context.get();

    let result = {
        let _state = SavedState::save(ctx);
        // The clear happens inside the flip, so the rects a caller damaged are
        // in the space it laid them out in rather than the window server's.
        CGContext::translate_ctm(Some(ctx), 0.0, size.height);
        CGContext::scale_ctm(Some(ctx), 1.0, -1.0);

        match damage {
            None => CGContext::clear_rect(Some(ctx), CGRect::new(CGPoint::new(0.0, 0.0), size)),
            Some(rects) => {
                let Some(first) = NonNull::new(rects.as_ptr().cast_mut()) else {
                    // Nothing to redraw. A caller should not have asked, but
                    // clipping to an empty list would clip to everything. The
                    // context and the saved state go on the way out.
                    return None;
                };
                // SAFETY: `first` points at `rects`, which outlives the call.
                unsafe { CGContext::clip_to_rects(Some(ctx), first, rects.len()) };
                for rect in rects {
                    CGContext::clear_rect(Some(ctx), *rect);
                }
            }
        }

        f(ctx)
    };

    CGContext::flush(Some(ctx));
    // The whole window is still published. Only the damaged pixels differ
    // from what is already there, so republishing the rest costs nothing
    // and saves building a region the window server has to agree with.
    // SAFETY: `window` is live on `cid`.
    unsafe { ffi::SLSFlushWindowContentRegion(cid, window.id(), ptr::null_mut()) };
    Some(result)
}

/// Rasterises a layer tree into a window server window.
///
/// The context is created and released per present rather than held: it is
/// cheap, and a cached one goes stale the moment the window is reshaped.
///
/// The y-flip is not optional. A window server context has its origin at the
/// bottom left; `CALayer` geometry runs top-down. Without the flip the bar
/// renders upside down.
/// As [`draw`], the window itself is the main-thread proof.
pub fn present(window: &Window, size: CGSize, layer: &CALayer) {
    let cid = crate::window::connection_id();
    let Some(context) = Context::create(cid, window.id()) else {
        return;
    };
    let ctx = context.get();

    CGContext::clear_rect(Some(ctx), CGRect::new(CGPoint::new(0.0, 0.0), size));
    {
        let _state = SavedState::save(ctx);
        CGContext::translate_ctm(Some(ctx), 0.0, size.height);
        CGContext::scale_ctm(Some(ctx), 1.0, -1.0);
        layer.renderInContext(ctx);
    }
    CGContext::flush(Some(ctx));

    // Drawing into the context is not enough; this is what publishes it.
    // SAFETY: `window` is live on `cid`.
    unsafe { ffi::SLSFlushWindowContentRegion(cid, window.id(), ptr::null_mut()) };
}

/// Applies layer mutations without Core Animation's implicit animations.
///
/// Every `CALayer` property assignment otherwise animates over a quarter of a
/// second, which for a bar that repaints on a timer reads as smear.
pub fn without_implicit_animations<R>(
    _proof: impl crate::MainThreadProof,
    f: impl FnOnce() -> R,
) -> R {
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

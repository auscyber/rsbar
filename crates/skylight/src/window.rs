use crate::error::{Error, Result, ok};
use crate::ffi::{self, ConnectionId, WindowId};
use crate::region::Region;
use crate::tags::WindowTags;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGImage;
use std::ptr;
use std::ptr::NonNull;

/// Standard CoreGraphics window levels, resolved from `CGWindowLevelForKey`.
/// Hardcoded because the key-to-level mapping has been stable for two decades
/// and pulling in the lookup buys nothing.
pub mod level {
    use std::ffi::c_int;

    /// Below the menu bar. Where `SketchyBar` puts a non-topmost bar.
    pub const BACKSTOP_MENU: c_int = -20;
    pub const NORMAL: c_int = 0;
    pub const FLOATING: c_int = 3;
    /// Level of the system menu bar itself.
    pub const STATUS: c_int = 25;
    pub const POPUP_MENU: c_int = 101;
}

/// The process-wide window server connection.
pub(crate) fn connection_id() -> ConnectionId {
    connection()
}

fn connection() -> ConnectionId {
    use std::sync::LazyLock;
    static CONNECTION: LazyLock<ConnectionId> =
        LazyLock::new(|| unsafe { ffi::SLSMainConnectionID() });
    *CONNECTION
}

/// `kCGBackingStoreBuffered`.
const BACKING_BUFFERED: std::ffi::c_int = 2;

/// `13` is the long-standing incantation for a plain composited window with a
/// backing store.
///
/// `SketchyBar` passes `13 | (1 << 18)`, but it never draws through
/// `SLWindowContextCreate` — it binds its own `CAContext` surface. That extra
/// bit suppresses the backing store, so a window created with it accepts a
/// drawing context and then shows nothing.
const WINDOW_OPTIONS: std::ffi::c_int = 13;

/// A window owned by the window server directly, with no `NSWindow` and no view
/// hierarchy behind it.
///
/// This is what lets a bar sit above the menu bar, on every space, and over
/// fullscreen apps — none of which `AppKit` will grant a borderless `NSWindow`.
#[derive(Debug)]
pub struct Window {
    id: WindowId,
    connection: ConnectionId,
    /// A window adopted with [`Window::from_existing`] belongs to someone else
    /// and must not be released on drop.
    owned: bool,
}

impl Window {
    /// Creates a window covering `frame`, alpha-blended and initially unmapped.
    ///
    /// The caller still has to give it a level, tags and a resolution, then
    /// order it in; a freshly created window is not yet on screen.
    // Window server coordinates are `float`. Screen geometry is far inside
    // f32's exact-integer range, so the narrowing is lossless in practice.
    #[allow(clippy::cast_possible_truncation)]
    pub fn new(frame: CGRect) -> Result<Self> {
        let connection = connection();
        let shape = Region::from_rect(&CGRect::new(CGPoint::new(0.0, 0.0), frame.size))?;
        // An empty opaque shape is what makes every pixel alpha-blended.
        let opaque = Region::empty()?;

        let mut tags: u64 = 0;
        let mut id: WindowId = 0;
        // SAFETY: both regions outlive the call, `tags` and `id` are valid
        // out-pointers, and the tag width matches `TAG_BITS`.
        ok(unsafe {
            ffi::SLSNewWindowWithOpaqueShapeAndContext(
                connection,
                BACKING_BUFFERED,
                shape.as_ptr(),
                opaque.as_ptr(),
                WINDOW_OPTIONS,
                &raw mut tags,
                frame.origin.x as f32,
                frame.origin.y as f32,
                ffi::TAG_BITS,
                &raw mut id,
                ptr::null_mut(),
            )
        })
        .map_err(Error::CreateWindow)?;

        Ok(Self {
            id,
            connection,
            owned: true,
        })
    }

    /// Adopts a window this process did not create. Never released on drop.
    #[must_use]
    pub fn from_existing(id: WindowId) -> Self {
        Self {
            id,
            connection: connection(),
            owned: false,
        }
    }

    #[must_use]
    pub fn id(&self) -> WindowId {
        self.id
    }

    #[must_use]
    pub fn connection(&self) -> ConnectionId {
        self.connection
    }

    /// Moves and resizes in one call. The origin travels as an offset and the
    /// region is anchored at zero — passing the frame's own origin in the
    /// region instead double-applies it.
    #[allow(clippy::cast_possible_truncation)]
    pub fn set_frame(&self, frame: CGRect) -> Result<()> {
        let region = Region::from_rect(&CGRect::new(CGPoint::new(0.0, 0.0), frame.size))?;
        // SAFETY: `region` outlives the call.
        ok(unsafe {
            ffi::SLSSetWindowShape(
                self.connection,
                self.id,
                frame.origin.x as f32,
                frame.origin.y as f32,
                region.as_ptr(),
            )
        })
        .map_err(Error::Shape)
    }

    /// Backing scale. Must match the `contentsScale` of the layer tree drawn
    /// into this window or retina output is soft.
    pub fn set_scale(&self, scale: f64) -> Result<()> {
        // SAFETY: plain scalar arguments.
        ok(unsafe { ffi::SLSSetWindowResolution(self.connection, self.id, scale) })
            .map_err(Error::Resolution)
    }

    pub fn set_alpha(&self, alpha: f32) -> Result<()> {
        // SAFETY: plain scalar arguments.
        ok(unsafe { ffi::SLSSetWindowAlpha(self.connection, self.id, alpha) })
            .map_err(Error::Opacity)
    }

    /// `false` keeps the window alpha-blended; `true` promises every pixel is
    /// opaque and lets the compositor skip what is behind it.
    pub fn set_opaque(&self, opaque: bool) -> Result<()> {
        // SAFETY: plain scalar arguments.
        ok(unsafe { ffi::SLSSetWindowOpacity(self.connection, self.id, opaque) })
            .map_err(Error::Opacity)
    }

    /// Sets the level, and pins the sub-level so ordering within that level is
    /// deterministic rather than whatever the last mutation left behind.
    pub fn set_level(&self, level: std::ffi::c_int) -> Result<()> {
        // SAFETY: plain scalar arguments.
        ok(unsafe { ffi::SLSSetWindowLevel(self.connection, self.id, level) })
            .map_err(Error::Level)?;
        // SAFETY: plain scalar arguments.
        ok(unsafe { ffi::SLSSetWindowSubLevel(self.connection, self.id, 0) }).map_err(Error::Level)
    }

    pub fn set_blur_radius(&self, radius: i32) -> Result<()> {
        // SAFETY: plain scalar arguments.
        ok(unsafe { ffi::SLSSetWindowBackgroundBlurRadius(self.connection, self.id, radius) })
            .map_err(Error::Blur)
    }

    pub fn set_tags(&self, tags: WindowTags) -> Result<()> {
        let mut bits = tags.bits();
        // SAFETY: `bits` is a valid in/out pointer of `TAG_BITS` bits.
        ok(
            unsafe {
                ffi::SLSSetWindowTags(self.connection, self.id, &raw mut bits, ffi::TAG_BITS)
            },
        )
        .map_err(Error::Tags)
    }

    pub fn clear_tags(&self, tags: WindowTags) -> Result<()> {
        let mut bits = tags.bits();
        // SAFETY: `bits` is a valid in/out pointer of `TAG_BITS` bits.
        ok(unsafe {
            ffi::SLSClearWindowTags(self.connection, self.id, &raw mut bits, ffi::TAG_BITS)
        })
        .map_err(Error::Tags)
    }

    /// Maps the window, above `relative_to` or above everything at its level.
    pub fn order_above(&self, relative_to: Option<WindowId>) -> Result<()> {
        self.order(ffi::ORDER_ABOVE, relative_to.unwrap_or(0))
    }

    pub fn order_below(&self, relative_to: Option<WindowId>) -> Result<()> {
        self.order(ffi::ORDER_BELOW, relative_to.unwrap_or(0))
    }

    /// Unmaps the window without destroying it.
    pub fn order_out(&self) -> Result<()> {
        self.order(ffi::ORDER_OUT, 0)
    }

    fn order(&self, mode: std::ffi::c_int, relative_to: WindowId) -> Result<()> {
        // SAFETY: plain scalar arguments.
        ok(unsafe { ffi::SLSOrderWindow(self.connection, self.id, mode, relative_to) })
            .map_err(Error::Order)
    }

    /// Renders this window's current on-screen contents to a still image.
    ///
    /// This is how `alias` items mirror another process's menu bar item. It
    /// needs Screen Recording permission; without it the window server leaves
    /// the image null rather than reporting an error, which surfaces here as
    /// [`Error::NoCapture`].
    pub fn capture(&self) -> Result<CFRetained<CGImage>> {
        // The full-resolution capture bit, cross-checked against SketchyBar's
        // `window.c`.
        const FULL_RESOLUTION: u32 = 1 << 8;

        // `CGRectNull` reconstructed by hand: `objc2-core-graphics` does not
        // bind the extern symbol, but the null rect's definition — infinite
        // origin, zero size — is a stable, documented constant, and this is
        // what tells the window server to capture the whole window.
        let whole_window = CGRect::new(
            CGPoint::new(f64::INFINITY, f64::INFINITY),
            CGSize::new(0.0, 0.0),
        );

        let wide_id = u64::from(self.id);
        let mut image: *mut CGImage = ptr::null_mut();
        // SAFETY: `wide_id` outlives the call, and `image` is a valid
        // out-pointer.
        unsafe {
            ffi::SLSCaptureWindowsContentsToRectWithOptions(
                self.connection,
                &raw const wide_id,
                true,
                whole_window,
                FULL_RESOLUTION,
                &raw mut image,
            );
        }

        let image = NonNull::new(image).ok_or(Error::NoCapture)?;
        // SAFETY: a non-null result carries a +1 reference, ownership of
        // which transfers to `CFRetained`.
        Ok(unsafe { CFRetained::from_raw(image) })
    }

    /// This window's true size in screen points.
    ///
    /// The window list's own bounds can disagree with this; this is the
    /// value `SketchyBar` actually draws a capture at.
    pub fn true_rect(&self) -> Result<CGRect> {
        let mut rect = CGRect::ZERO;
        // SAFETY: `rect` is a valid out-pointer.
        ok(unsafe { ffi::SLSGetScreenRectForWindow(self.connection, self.id, &raw mut rect) })
            .map_err(Error::ScreenRect)?;
        Ok(rect)
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        if !self.owned {
            return;
        }
        // SAFETY: we created this window and have not released it.
        if let Err(err) = ok(unsafe { ffi::SLSReleaseWindow(self.connection, self.id) }) {
            tracing::warn!(?err, id = self.id, "failed to release window server window");
        }
    }
}

/// Suspends compositing for the duration of `f`, so a batch of window mutations
/// lands as one visual change instead of a sequence of them.
///
/// Re-enables on unwind too: a leaked suspension freezes the entire display.
pub fn batched<R>(f: impl FnOnce() -> R) -> R {
    struct Guard(ConnectionId);
    impl Drop for Guard {
        fn drop(&mut self) {
            // SAFETY: matches the disable in `batched`.
            unsafe { ffi::SLSReenableUpdate(self.0) };
        }
    }

    let cid = connection();
    // SAFETY: the guard re-enables on every path out, including a panic.
    unsafe { ffi::SLSDisableUpdate(cid) };
    let _guard = Guard(cid);
    f()
}

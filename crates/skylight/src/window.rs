use crate::Connected;
use crate::error::{Error, Result, ok};
use crate::ffi::{self, ConnectionId, WindowId};
use crate::region::Region;
use crate::tags::WindowTags;
use objc2_core_foundation::{CFArray, CFNumber, CFRetained, CFType, CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGImage;
use std::cell::Cell;
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

/// The connection, asked for once.
///
/// `SLSMainConnectionID` wants the main thread — it is what establishes the
/// process's connection — so the first ask carries proof. What comes back is a
/// plain id any thread may carry; see [`crate::connection`] for why using it
/// needs a lock rather than a thread.
fn connection(proof: impl crate::MainThreadProof) -> ConnectionId {
    crate::connection::establish(proof)
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
///
/// # `Send`, not main-thread-only
///
/// A `Window` used to carry a [`crate::OnlyOnMain`] field and be proof of the
/// thread it was made on, on the theory that the window server only answers
/// the thread pumping the run loop that composites it. That was never true —
/// it answers any thread. What is genuinely single-threaded is a *sequence*:
/// [`batched`] brackets a run of mutations with `SLSDisableUpdate` and
/// `SLSReenableUpdate`, and two of those overlapping breaks the second one's
/// compositing suspension early. That is what [`crate::Connected`] is
/// exclusive use of, which is why every method here takes one rather than
/// asking for a thread.
///
/// So a `Window` is an ordinary `Send` value: [`Window::new`] still takes
/// proof, because minting the process's one connection wants the main thread
/// the first time, but nothing about holding a `Window` afterwards does.
///
/// A window this process did not create is [`Foreign`] instead — same
/// exclusivity, but read-only, and by id rather than by owning value.
///
/// ```
/// fn onto_a_worker<T: Send>(_: T) {}
/// fn check(window: skylight::Window) {
///     onto_a_worker(window);
/// }
/// ```
#[derive(Debug)]
pub struct Window {
    id: WindowId,
    /// What this process has asked the window server for, so
    /// [`Self::set_tags`]/[`Self::clear_tags`] can skip a syscall — and the
    /// repaint/reframe it can trigger downstream — when asked to apply a
    /// value that is already in effect.
    known_tags: Cell<WindowTags>,
}

impl Window {
    /// Creates a window covering `frame`, alpha-blended and initially unmapped.
    ///
    /// The caller still has to give it a level, tags and a resolution, then
    /// order it in; a freshly created window is not yet on screen.
    ///
    /// The proof is what [`crate::establish`] wants the first time anything
    /// asks for the process's connection, not anything this window keeps:
    /// every method after this one takes the [`crate::Connected`] its call
    /// needs instead.
    // Window server coordinates are `float`. Screen geometry is far inside
    // f32's exact-integer range, so the narrowing is lossless in practice.
    #[allow(clippy::cast_possible_truncation)]
    pub fn new(frame: CGRect, proof: impl crate::MainThreadProof) -> Result<Self> {
        let connection = connection(&proof);
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
            known_tags: Cell::new(WindowTags::empty()),
        })
    }

    #[must_use]
    pub fn id(&self) -> WindowId {
        self.id
    }

    /// Moves and resizes in one call. The origin travels as an offset and the
    /// region is anchored at zero — passing the frame's own origin in the
    /// region instead double-applies it.
    #[allow(clippy::cast_possible_truncation)]
    pub fn set_frame(&self, connected: &Connected, frame: CGRect) -> Result<()> {
        let region = Region::from_rect(&CGRect::new(CGPoint::new(0.0, 0.0), frame.size))?;
        // SAFETY: `region` outlives the call.
        ok(unsafe {
            ffi::SLSSetWindowShape(
                connected.id(),
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
    pub fn set_scale(&self, connected: &Connected, scale: f64) -> Result<()> {
        // SAFETY: plain scalar arguments.
        ok(unsafe { ffi::SLSSetWindowResolution(connected.id(), self.id, scale) })
            .map_err(Error::Resolution)
    }

    pub fn set_alpha(&self, connected: &Connected, alpha: f32) -> Result<()> {
        // SAFETY: plain scalar arguments.
        ok(unsafe { ffi::SLSSetWindowAlpha(connected.id(), self.id, alpha) })
            .map_err(Error::Opacity)
    }

    /// `false` keeps the window alpha-blended; `true` promises every pixel is
    /// opaque and lets the compositor skip what is behind it.
    pub fn set_opaque(&self, connected: &Connected, opaque: bool) -> Result<()> {
        // SAFETY: plain scalar arguments.
        ok(unsafe { ffi::SLSSetWindowOpacity(connected.id(), self.id, opaque) })
            .map_err(Error::Opacity)
    }

    /// Sets the level, and pins the sub-level so ordering within that level is
    /// deterministic rather than whatever the last mutation left behind.
    pub fn set_level(&self, connected: &Connected, level: std::ffi::c_int) -> Result<()> {
        // SAFETY: plain scalar arguments.
        ok(unsafe { ffi::SLSSetWindowLevel(connected.id(), self.id, level) })
            .map_err(Error::Level)?;
        // SAFETY: plain scalar arguments.
        ok(unsafe { ffi::SLSSetWindowSubLevel(connected.id(), self.id, 0) }).map_err(Error::Level)
    }

    pub fn set_blur_radius(&self, connected: &Connected, radius: i32) -> Result<()> {
        // SAFETY: plain scalar arguments.
        ok(unsafe { ffi::SLSSetWindowBackgroundBlurRadius(connected.id(), self.id, radius) })
            .map_err(Error::Blur)
    }

    /// Adds `tags`, reporting whether any of them were not already set — so a
    /// caller with its own damage tracking can skip whatever reframe or
    /// repaint a no-op tag write would otherwise trigger downstream.
    pub fn set_tags(&self, connected: &Connected, tags: WindowTags) -> Result<bool> {
        let before = self.known_tags.get();
        if before.contains(tags) {
            return Ok(false);
        }
        let mut bits = tags.bits();
        // SAFETY: `bits` is a valid in/out pointer of `TAG_BITS` bits.
        ok(unsafe { ffi::SLSSetWindowTags(connected.id(), self.id, &raw mut bits, ffi::TAG_BITS) })
            .map_err(Error::Tags)?;
        self.known_tags.set(before.union(tags));
        Ok(true)
    }

    /// Removes `tags`, reporting whether any of them were set to begin with.
    /// See [`Self::set_tags`].
    pub fn clear_tags(&self, connected: &Connected, tags: WindowTags) -> Result<bool> {
        let before = self.known_tags.get();
        if before.intersection(tags).is_empty() {
            return Ok(false);
        }
        let mut bits = tags.bits();
        // SAFETY: `bits` is a valid in/out pointer of `TAG_BITS` bits.
        ok(unsafe {
            ffi::SLSClearWindowTags(connected.id(), self.id, &raw mut bits, ffi::TAG_BITS)
        })
        .map_err(Error::Tags)?;
        self.known_tags.set(before.difference(tags));
        Ok(true)
    }

    /// Sets or clears [`WindowTags::STICKY`] as one unit, explicitly setting
    /// the opposite [`WindowTags::NEVER_STICKY`] tag when turning it off —
    /// the same set-the-opposite shape `Panels::set_clickable` in `coolabah`
    /// uses for its own two-state tag toggle. Returns whether anything
    /// changed.
    pub fn set_sticky(&self, connected: &Connected, sticky: bool) -> Result<bool> {
        let (set, clear) = if sticky {
            (WindowTags::STICKY, WindowTags::NEVER_STICKY)
        } else {
            (WindowTags::NEVER_STICKY, WindowTags::STICKY)
        };
        let cleared = self.clear_tags(connected, clear)?;
        let set = self.set_tags(connected, set)?;
        Ok(cleared || set)
    }

    /// Sets or clears [`WindowTags::FRIEND_OF_FULLSCREEN`]. Returns whether
    /// anything changed.
    pub fn set_friend_of_fullscreen(&self, connected: &Connected, friend: bool) -> Result<bool> {
        if friend {
            self.set_tags(connected, WindowTags::FRIEND_OF_FULLSCREEN)
        } else {
            self.clear_tags(connected, WindowTags::FRIEND_OF_FULLSCREEN)
        }
    }

    /// Registers a rectangle within this window that the window server will
    /// report the cursor entering and leaving.
    ///
    /// Per rectangle, not per window: several may be live at once and each
    /// fires at its own boundary, which is what lets one shared bar window
    /// report hover for each item on it. The events arrive as Carbon
    /// `kEventMouseEntered`/`kEventMouseExited` on the run loop, not through
    /// any callback registered here.
    ///
    /// `kEventMouseMoved` is deliberately not the mechanism: it is never
    /// delivered to this process at all, however the window is tagged --
    /// established by moving a real cursor across the bar and seeing nothing.
    ///
    /// # Errors
    ///
    /// Returns the window server's error if the rectangle is rejected.
    pub fn add_tracking_rect(&self, connected: &Connected, rect: CGRect) -> Result<()> {
        // SAFETY: `self.id` is this process's own window and `rect` is passed
        // by value.
        ok(unsafe { ffi::SLSAddTrackingRect(connected.id(), self.id, rect) }).map_err(Error::Tags)
    }

    /// Drops every rectangle [`Self::add_tracking_rect`] registered.
    ///
    /// There is no call to remove one individually, so a layout change means
    /// clearing them all and adding back the ones that still apply.
    ///
    /// # Errors
    ///
    /// Returns the window server's error if the call is rejected.
    pub fn clear_tracking_rects(&self, connected: &Connected) -> Result<()> {
        // SAFETY: as above.
        ok(unsafe { ffi::SLSRemoveAllTrackingAreas(connected.id(), self.id) }).map_err(Error::Tags)
    }

    /// Reads this window's tags back from the window server, rather than
    /// trusting what this process last asked for — the ground truth this
    /// crate's own bookkeeping is checked against.
    ///
    /// No single-window tag getter is known to yabai, `rift` or `paneru`;
    /// all three go through the same query-and-iterate path this mirrors
    /// (yabai's `window_tags()` in `window.c`). Returns empty tags, rather
    /// than an error, if the window server has nothing to say about this
    /// window id — matching that C reference's own `uint64_t tags = 0;`
    /// fallback.
    #[must_use]
    pub fn tags(&self, connected: &Connected) -> WindowTags {
        window_tags(connected, self.id)
    }

    /// Maps the window, above `relative_to` or above everything at its level.
    pub fn order_above(&self, connected: &Connected, relative_to: Option<WindowId>) -> Result<()> {
        self.order(connected, ffi::ORDER_ABOVE, relative_to.unwrap_or(0))
    }

    pub fn order_below(&self, connected: &Connected, relative_to: Option<WindowId>) -> Result<()> {
        self.order(connected, ffi::ORDER_BELOW, relative_to.unwrap_or(0))
    }

    /// Unmaps the window without destroying it.
    pub fn order_out(&self, connected: &Connected) -> Result<()> {
        self.order(connected, ffi::ORDER_OUT, 0)
    }

    fn order(
        &self,
        connected: &Connected,
        mode: std::ffi::c_int,
        relative_to: WindowId,
    ) -> Result<()> {
        // SAFETY: plain scalar arguments.
        ok(unsafe { ffi::SLSOrderWindow(connected.id(), self.id, mode, relative_to) })
            .map_err(Error::Order)
    }
}

/// The body of [`Window::tags`] and [`Foreign::tags`], by id — the two differ
/// only in whether the id is a window this process owns.
///
/// No single-window tag getter is known to yabai, `rift` or `paneru`; all
/// three go through the same query-and-iterate path this mirrors (yabai's
/// `window_tags()` in `window.c`). Returns empty tags, rather than an error,
/// if the window server has nothing to say about this window id — matching
/// that C reference's own `uint64_t tags = 0;` fallback.
#[allow(clippy::cast_possible_wrap)]
fn window_tags(connected: &Connected, id: WindowId) -> WindowTags {
    let number = CFNumber::new_i32(id as i32);
    let list = CFArray::from_objects(&[&*number]);

    // SAFETY: `list` is a live array of one element for the call, and the
    // result is either null or a +1 query `list` does not need to outlive.
    let Some(query) = NonNull::new(unsafe {
        ffi::SLSWindowQueryWindows(connected.id(), CFRetained::as_ptr(&list).as_ptr(), 1)
    }) else {
        return WindowTags::empty();
    };
    // SAFETY: `query` carries a +1 reference this now owns.
    let query = unsafe { CFRetained::<CFType>::from_raw(query) };
    let query = CFRetained::as_ptr(&query).as_ptr();

    // SAFETY: `query` is live for the call; the result is either null or
    // a +1 iterator this now owns.
    let Some(iterator) = NonNull::new(unsafe { ffi::SLSWindowQueryResultCopyWindows(query) })
    else {
        return WindowTags::empty();
    };
    // SAFETY: as above.
    let iterator = unsafe { CFRetained::<CFType>::from_raw(iterator) };
    let iterator = CFRetained::as_ptr(&iterator).as_ptr();

    // SAFETY: `iterator` is live for both calls below.
    if unsafe { ffi::SLSWindowIteratorGetCount(iterator) } != 1
        || !unsafe { ffi::SLSWindowIteratorAdvance(iterator) }
    {
        return WindowTags::empty();
    }
    // SAFETY: `iterator` is still live and has just been advanced to a
    // real entry.
    WindowTags::from_bits_retain(unsafe { ffi::SLSWindowIteratorGetTags(iterator) })
}

/// A window this process did not create.
///
/// Everything a [`Window`] does past ordering, moving and drawing is really a
/// question about a window's *state*, and the window server answers those
/// questions about any window, not only ones this process made — that is what
/// [`crate::sys::window`]'s reads are. `Foreign` is the ergonomic front for
/// them: a plain `Send` id, `Copy`, no drop behaviour, safe to hold on a
/// worker the same way [`WindowId`] already is for the capture pool.
///
/// This is the shape a window manager wants far more often than [`Window`]
/// is: `paneru` mostly asks what other processes' windows are doing, and only
/// rarely makes one of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Foreign(WindowId);

impl Foreign {
    #[must_use]
    pub const fn new(id: WindowId) -> Self {
        Self(id)
    }

    #[must_use]
    pub const fn id(self) -> WindowId {
        self.0
    }

    /// The window's frame in screen coordinates.
    pub fn bounds(self, connected: &Connected) -> Result<CGRect> {
        let mut frame = CGRect::ZERO;
        // SAFETY: `frame` is a valid out-pointer.
        ok(unsafe {
            crate::sys::window::SLSGetWindowBounds(connected.id(), self.0, &raw mut frame)
        })
        .map_err(Error::ForeignWindow)?;
        Ok(frame)
    }

    /// The window's level — see [`level`] for the ones worth comparing against.
    pub fn level(self, connected: &Connected) -> Result<std::ffi::c_int> {
        let mut level = 0;
        // SAFETY: `level` is a valid out-pointer.
        ok(unsafe {
            crate::sys::window::SLSGetWindowLevel(connected.id(), self.0, &raw mut level)
        })
        .map_err(Error::ForeignWindow)?;
        Ok(level)
    }

    pub fn alpha(self, connected: &Connected) -> Result<f32> {
        let mut alpha = 0.0f32;
        // SAFETY: `alpha` is a valid out-pointer.
        ok(unsafe {
            crate::sys::window::SLSGetWindowAlpha(connected.id(), self.0, &raw mut alpha)
        })
        .map_err(Error::ForeignWindow)?;
        Ok(alpha)
    }

    /// Whether the window is mapped — on screen, as opposed to merely existing.
    pub fn is_ordered_in(self, connected: &Connected) -> Result<bool> {
        let mut ordered: u8 = 0;
        // SAFETY: `ordered` is a valid one-byte out-pointer.
        ok(unsafe {
            crate::sys::window::SLSWindowIsOrderedIn(connected.id(), self.0, &raw mut ordered)
        })
        .map_err(Error::ForeignWindow)?;
        Ok(ordered != 0)
    }

    /// The connection that owns this window — half of "which process is
    /// this", the other half being `SLSConnectionGetPID` on the id this
    /// returns (declared in [`crate::sys::window`], not wrapped here: it asks
    /// a question about a connection, not a window).
    pub fn owner(self, connected: &Connected) -> Result<ConnectionId> {
        let mut owner: std::ffi::c_int = 0;
        // SAFETY: `owner` is a valid out-pointer.
        ok(unsafe {
            crate::sys::window::SLSGetWindowOwner(connected.id(), self.0, &raw mut owner)
        })
        .map_err(Error::ForeignWindow)?;
        Ok(ConnectionId::from_raw(owner))
    }

    /// Which spaces the window is on — more than one if it is sticky.
    #[allow(clippy::cast_possible_wrap, clippy::missing_panics_doc)]
    pub fn spaces(self, connected: &Connected) -> Vec<crate::SpaceId> {
        let number = CFNumber::new_i32(self.0 as i32);
        let list = CFArray::from_objects(&[&*number]);
        // SAFETY: `list` is live for the call, and the result is either null
        // or a +1 array this now owns.
        let Some(spaces) = NonNull::new(unsafe {
            crate::sys::window::SLSCopySpacesForWindows(
                connected.id(),
                crate::sys::window::ALL_SPACES_SELECTOR,
                CFRetained::as_ptr(&list).as_ptr(),
            )
        }) else {
            return Vec::new();
        };
        // SAFETY: a non-null result carries a +1 reference this now owns.
        let spaces = unsafe { CFRetained::from_raw(spaces) };
        crate::cf::Array::new(spaces.as_ref())
            .iter::<CFNumber>()
            .filter_map(CFNumber::as_i64)
            .map(i64::cast_unsigned)
            .collect()
    }

    /// The windows attached to this one — a sheet, a drawer, a popover's
    /// shadow.
    // Window ids are `WindowId` (`u32`) round-tripped through `CFNumber` as
    // `i64` -- the widest common type the window server's own copy calls
    // use -- so the narrowing back is lossless in practice, the same as
    // `window_tags`'s widening the other way.
    #[allow(
        clippy::missing_panics_doc,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation
    )]
    pub fn associated_windows(self, connected: &Connected) -> Vec<Self> {
        // SAFETY: the result is either null or a +1 array this now owns.
        let Some(windows) = NonNull::new(unsafe {
            crate::sys::window::SLSCopyAssociatedWindows(connected.id(), self.0)
        }) else {
            return Vec::new();
        };
        // SAFETY: a non-null result carries a +1 reference this now owns.
        let windows = unsafe { CFRetained::from_raw(windows) };
        crate::cf::Array::new(windows.as_ref())
            .iter::<CFNumber>()
            .filter_map(CFNumber::as_i64)
            .map(|id| Self(id as u32))
            .collect()
    }

    /// Which display the window is mostly on, by the window server's UUID
    /// naming — see [`crate::display::active_menu_bar`] for the other side of
    /// that naming.
    #[must_use]
    pub fn display_uuid(
        self,
        connected: &Connected,
    ) -> Option<CFRetained<objc2_core_foundation::CFString>> {
        // SAFETY: the result is either null or a +1 string this now owns.
        let uuid = NonNull::new(unsafe {
            crate::sys::window::SLSCopyManagedDisplayForWindow(connected.id(), self.0)
        })?;
        // SAFETY: a non-null result carries a +1 reference this now owns.
        Some(unsafe { CFRetained::from_raw(uuid) })
    }

    /// One of the window's properties by name — the read half of
    /// [`Window::set_tags`]'s sibling, `SLSSetWindowProperty`, though nothing
    /// in this crate sets one on a window it does not own.
    pub fn property(
        self,
        connected: &Connected,
        key: &objc2_core_foundation::CFString,
    ) -> Result<Option<CFRetained<CFType>>> {
        let mut value: *mut CFType = ptr::null_mut();
        // SAFETY: `key` is live for the call and `value` a valid out-pointer.
        ok(unsafe {
            crate::sys::window::SLSCopyWindowProperty(
                connected.id(),
                self.0,
                std::ptr::from_ref(key).cast_mut(),
                &raw mut value,
            )
        })
        .map_err(Error::ForeignWindow)?;
        // SAFETY: a non-null result carries a +1 reference this now owns.
        Ok(NonNull::new(value).map(|value| unsafe { CFRetained::from_raw(value) }))
    }

    #[must_use]
    pub fn tags(self, connected: &Connected) -> WindowTags {
        window_tags(connected, self.0)
    }
}

impl From<&Window> for Foreign {
    /// A window this process made, seen the way any other process's would be —
    /// for a caller that wants to treat its own windows uniformly with
    /// everyone else's, such as when checking what is on top of it.
    fn from(window: &Window) -> Self {
        Self(window.id)
    }
}

/// The window ids on a set of spaces, filtered by tags — see
/// [`crate::sys::window::SLSCopyWindowsWithOptionsAndTags`] for what
/// `owner`/`options`/`set_tags`/`clear_tags` mean.
// See `associated_windows` for why the `i64` round trip through `CFNumber`
// back to a `WindowId` is lossless in practice.
#[allow(
    clippy::missing_panics_doc,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation
)]
pub fn windows_on_spaces(
    connected: &Connected,
    spaces: &[crate::SpaceId],
    owner: u32,
    options: u32,
    set_tags: WindowTags,
    clear_tags: WindowTags,
) -> Vec<Foreign> {
    #[allow(clippy::cast_possible_wrap)]
    let numbers: Vec<_> = spaces
        .iter()
        .map(|&id| CFNumber::new_i64(id as i64))
        .collect();
    let refs: Vec<&CFNumber> = numbers.iter().map(|n| &**n).collect();
    let list = CFArray::from_objects(&refs);
    let mut set_tags = set_tags.bits();
    let mut clear_tags = clear_tags.bits();
    // SAFETY: `list` is live for the call, `set_tags`/`clear_tags` are valid
    // in/out pointers, and the result is either null or a +1 array this now
    // owns.
    let Some(windows) = NonNull::new(unsafe {
        crate::sys::window::SLSCopyWindowsWithOptionsAndTags(
            connected.id(),
            owner,
            CFRetained::as_ptr(&list).as_ptr(),
            options,
            &raw mut set_tags,
            &raw mut clear_tags,
        )
    }) else {
        return Vec::new();
    };
    // SAFETY: a non-null result carries a +1 reference this now owns.
    let windows = unsafe { CFRetained::from_raw(windows) };
    crate::cf::Array::new(windows.as_ref())
        .iter::<CFNumber>()
        .filter_map(CFNumber::as_i64)
        .map(|id| Foreign(id as u32))
        .collect()
}

/// Renders a window's current on-screen contents to a still image.
///
/// Takes an id rather than a [`Window`], and needs no proof: this is how
/// `alias` items mirror another process's menu bar item, and it runs on the
/// capture pool. Doing it on the thread that composites cost a measured 115ms
/// of every second.
///
/// Needs Screen Recording permission; without it the window server leaves the
/// image null rather than reporting an error, which surfaces here as
/// [`Error::NoCapture`].
pub fn capture(connected: &crate::Connected, window: WindowId) -> Result<CFRetained<CGImage>> {
    // The full-resolution capture bit, cross-checked against SketchyBar's
    // `window.c`.
    const FULL_RESOLUTION: u32 = 1 << 8;

    // `CGRectNull` reconstructed by hand: `objc2-core-graphics` does not bind
    // the extern symbol, but the null rect's definition -- infinite origin,
    // zero size -- is a stable, documented constant, and this is what tells
    // the window server to capture the whole window.
    let whole_window = CGRect::new(
        CGPoint::new(f64::INFINITY, f64::INFINITY),
        CGSize::new(0.0, 0.0),
    );

    let wide_id = u64::from(window);
    let mut image: *mut CGImage = ptr::null_mut();
    // SAFETY: `wide_id` outlives the call, and `image` is a valid
    // out-pointer.
    unsafe {
        ffi::SLSCaptureWindowsContentsToRectWithOptions(
            connected.id(),
            &raw const wide_id,
            true,
            whole_window,
            FULL_RESOLUTION,
            &raw mut image,
        );
    }

    let image = NonNull::new(image).ok_or(Error::NoCapture)?;
    // SAFETY: a non-null result carries a +1 reference, ownership of which
    // transfers to `CFRetained`.
    Ok(unsafe { CFRetained::from_raw(image) })
}

/// A window's true size in screen points, by id.
///
/// The window list's own bounds can disagree with this; this is the value
/// `SketchyBar` actually draws a capture at. On the capture pool for the same
/// reason [`capture`] is.
pub fn true_rect(connected: &crate::Connected, window: WindowId) -> Result<CGRect> {
    let mut rect = CGRect::ZERO;
    // SAFETY: `rect` is a valid out-pointer.
    ok(unsafe { ffi::SLSGetScreenRectForWindow(connected.id(), window, &raw mut rect) })
        .map_err(Error::ScreenRect)?;
    Ok(rect)
}

impl Drop for Window {
    /// Releases the window server window.
    ///
    /// Cannot await, and must not lock: the dropper may already be holding
    /// [`crate::Connected`] — a `Window` going out of scope inside a
    /// [`batched`] closure is ordinary, not a bug — so this reaches for the
    /// established connection id directly rather than acquiring one.
    /// `SLSReleaseWindow` is a single call, not a sequence, so it needs no
    /// more than that.
    fn drop(&mut self) {
        let cid = crate::connection::established();
        // SAFETY: we created this window and have not released it.
        if let Err(err) = ok(unsafe { ffi::SLSReleaseWindow(cid, self.id) }) {
            tracing::warn!(?err, id = self.id, "failed to release window server window");
        }
    }
}

struct CompositingGuard(ConnectionId);
impl Drop for CompositingGuard {
    fn drop(&mut self) {
        // SAFETY: matches the disable in `batched`/`batched_with`; the guard
        // cannot outlive the connection that made it.
        unsafe { ffi::SLSReenableUpdate(self.0) };
    }
}

/// Suspends compositing for the duration of `f`, so a batch of window mutations
/// lands as one visual change instead of a sequence of them.
///
/// Acquires [`crate::Connected`] itself rather than taking one, because the
/// suspension has to bracket the whole batch: a caller handed the guard could
/// drop it partway through `f` and let another sequence interleave. Async
/// because the acquire can be — see [`crate::connection`] for why a blocking
/// wait here would be wrong on the thread that composites.
///
/// A caller already holding [`crate::Connected`] — a frame's own pass, which
/// holds one for its whole duration — wants [`batched_with`] instead: taking
/// this lock a second time, non-reentrantly, would deadlock against itself.
///
/// Re-enables on unwind too: a leaked suspension freezes the entire display.
pub async fn batched<R>(f: impl FnOnce(&Connected) -> R) -> R {
    let connected = crate::connection::acquire().await;
    batched_with(&connected, f)
}

/// The same suspension as [`batched`], over a connection the caller already
/// has exclusive use of.
///
/// Re-enables on unwind too: a leaked suspension freezes the entire display.
pub fn batched_with<R>(connected: &Connected, f: impl FnOnce(&Connected) -> R) -> R {
    let cid = connected.id();
    // SAFETY: the guard re-enables on every path out, including a panic.
    unsafe { ffi::SLSDisableUpdate(cid) };
    let _guard = CompositingGuard(cid);
    f(connected)
}

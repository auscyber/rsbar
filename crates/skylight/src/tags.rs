//! Window server window tags.
//!
//! One 64-bit bitset controls most of a window's behaviour that has no public
//! equivalent: which spaces it follows, whether it takes clicks, whether Expose
//! and screen capture can see it. Names and bit positions are the community's
//! reverse-engineering of `CGSWindowTag`; unnamed bits are not "free", they are
//! merely undocumented.
//!
//! # What corroborates these, and what does not
//!
//! **`rift` is not an independent check.** Its `SLSWindowTags`
//! (`sys/skylight.rs`) does list all 64 bits and does agree on every one — but
//! it agrees *exactly*, name for name and doc comment for doc comment, which is
//! what a copy looks like rather than what two reverse-engineerings look like.
//! Diffing the two lists yields nothing. So the set below has one origin and
//! two spellings of it, and `rift` cannot be cited as evidence for any bit in
//! it. (This is also the answer to "does `rift` know tags we do not": it knows
//! precisely the same ones.)
//!
//! The genuinely independent sources are much smaller and only partly agree:
//!
//! * **yabai** names two bits and only two — `1 << 11` for sticky and `1 << 3`
//!   for the shadow (`window.c`'s `do_window_sticky`/`do_window_shadow`). Both
//!   match [`WindowTags::ON_ALL_WORKSPACES`] and
//!   [`WindowTags::DISABLE_SHADOW`].
//! * **`SketchyBar`** sets `kCGSExposeFadeTagBit` and
//!   `kCGSPreventsActivationTagBit` by name, matching bits 38 and 16, and tests
//!   `tags & 0x400000000000000` — bit 58, which this set calls
//!   [`WindowTags::IGNORE_FOR_SCREEN_SHARING`] — as part of deciding whether a
//!   window is a real application window (`app_windows.c`'s
//!   `iterator_window_suitable`). That use does not obviously follow from that
//!   name, so treat bit 58's name as the weakest here.
//! * **`NUIKit/CGSInternal`**'s `CGSWindowTagBit` (`CGSWindow.h`) is the only
//!   reference that enumerates the tag space, and it is where the *low* half of
//!   this set is confirmed: **bits 0 through 31 agree name for name and
//!   position for position**, `kCGSDocumentWindowTagBit` through
//!   `kCGSModalWindowTagBit`. That is a real independent confirmation of half
//!   the set.
//!
//! ## And the high half disagrees
//!
//! `CGSInternal` splits the bitset in two — its "hi" values are meant for
//! `tags[1]`, so its `1 << n` there is absolute bit `32 + n`. Read that way it
//! agrees with this set at bits 46, 48 and 49 and **disagrees from bit 34
//! upwards**, by one position at first and then by more as each list gains
//! names the other lacks. `CGSInternal` puts `SuperSticky` at 45 where this set
//! has [`WindowTags::FRIEND_OF_FULLSCREEN`], and names three tags absent here
//! entirely (`WindowIsMagicMirror`, `FollowsUser`, `MergesWithMenuBar`), while
//! this set names five absent there ([`WindowTags::FRIEND_OF_FULLSCREEN`],
//! [`WindowTags::DESKTOP_AFFINITY`],
//! [`WindowTags::IGNORES_WORKSPACE_HEURISTICS`],
//! [`WindowTags::USER_INPUT_ACCESSORY`],
//! [`WindowTags::NON_COMPOSITING_BACKING_STORE`]).
//!
//! Which is right is not decidable from the references: `CGSInternal` predates
//! a decade of releases, and tags added since could as easily have been
//! inserted as appended. What settles the two bits this bar actually depends on
//! is the live check below — a bar built with bit 45 draws over a fullscreen
//! app and one without it does not, which is not what bit 45 would do if it
//! were `SuperSticky`. **Nothing else above bit 33 has been checked either
//! way**, so a caller reaching for one of those names should expect to verify
//! it first.
//!
//! Upstream `SketchyBar` does not actually use either tag for its `sticky` or
//! `show_in_fullscreen` options: `sticky` is a dedicated, always-shown
//! `SLSSpaceCreate`d space windows get added to (`window_open`);
//! `show_in_fullscreen` is the bar manager comparing `SLSSpaceGetType(cid,
//! dsid) != 4` on every space change and ordering the window in or out by
//! hand (`bar_manager_handle_space_change`). Neither mechanism was ported
//! here, so this crate's reliance on the tag bits directly is *not*
//! cross-checked against a reference implementation actually using them for
//! this purpose.
//!
//! Checked live instead, through `coolabah`'s own bar rather than an isolated
//! window (`skylight/examples/window_tags.rs` has the detail) — see
//! [`WindowTags::STICKY`] and [`WindowTags::FRIEND_OF_FULLSCREEN`] for what
//! that check found.

use bitflags::bitflags;

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[repr(transparent)]
    pub struct WindowTags: u64 {
/// Shows with the standard document-window appearance.
        const DOCUMENT = 1u64 << 0;
        /// Floats above ordinary application windows.
        const FLOATING = 1u64 << 1;
        /// Suppresses Dock badging while the window is minimized.
        const DO_NOT_SHOW_BADGE_IN_DOCK = 1u64 << 2;
        /// Forces the window to render without a shadow.
        const DISABLE_SHADOW = 1u64 << 3;
        /// Requests higher-quality resampling from WindowServer.
        const HIGH_QUALITY_RESAMPLING = 1u64 << 4;
        /// Allows the window to set the cursor while inactive.
        const SETS_CURSOR_IN_BACKGROUND = 1u64 << 5;
        /// Keeps the window responsive during modal run loops.
        const WORKS_WHEN_MODAL = 1u64 << 6;
        /// Anchors the window to another window.
        const ATTACHED = 1u64 << 7;
        /// Ignores the window alpha while dragging.
        const IGNORE_ALPHA_FOR_DRAGGING = 1u64 << 8;
        /// Lets pointer events pass through the window.
        const IGNORE_FOR_EVENTS = 1u64 << 9;
        /// Makes the window intercept pointer events.
        const OPAQUE_FOR_EVENTS = 1u64 << 10;
        /// Shows the window on every workspace, but alone drops out mid-transition
        /// during a space switch -- see [`WindowTags::STICKY`].
        const ON_ALL_WORKSPACES = 1u64 << 11;
        /// Bypasses normal CPS pointer-event dispatch.
        const POINTER_EVENTS_AVOID_CPS = 1u64 << 12;
        /// Mirrors AppKit's visible-state tracking.
        const KIT_VISIBLE = 1u64 << 13;
        /// Removes the window from lists when the app deactivates.
        const HIDE_ON_DEACTIVATE = 1u64 << 14;
        /// Prevents ordering the app front when the window appears.
        const AVOIDS_ACTIVATION = 1u64 << 15;
        /// Prevents ordering the app front when the window is selected.
        const PREVENTS_ACTIVATION = 1u64 << 16;
        /// Opts the window out of Option-modifier activation behavior.
        const IGNORES_OPTION = 1u64 << 17;
        /// Excludes the window from standard window cycling.
        const IGNORES_CYCLE = 1u64 << 18;
        /// Defers normal ordering operations for the window.
        const DEFERS_ORDERING = 1u64 << 19;
        /// Defers activation requests for the window.
        const DEFERS_ACTIVATION = 1u64 << 20;
        /// Prevents WindowServer from front-ordering the window.
        const IGNORE_AS_FRONT_WINDOW = 1u64 << 21;
        /// Lets WindowServer handle dragging when the app stalls.
        const ENABLE_SERVER_SIDE_DRAG = 1u64 << 22;
        /// Grabs mouse-down events before normal dispatch.
        const MOUSE_DOWN_EVENTS_GRABBED = 1u64 << 23;
        /// Ignores requests to hide the window.
        const DONT_HIDE = 1u64 << 24;
        /// Prevents the host display from dimming.
        const DONT_DIM_WINDOW_DISPLAY = 1u64 << 25;
        /// Converts all pointers to the window's preferred type.
        const INSTANT_MOUSER_WINDOW = 1u64 << 26;
        /// Follows the user across active-space changes.
        const OWNER_FOLLOWS_FOREGROUND = 1u64 << 27;
        /// Uses distinct active and inactive window levels.
        const ACTIVATION_WINDOW_LEVEL = 1u64 << 28;
        /// Brings the owning app forward when selected.
        const BRING_OWNER_FORWARD = 1u64 << 29;
        /// Allows the window to appear before login completes.
        const PERMITTED_BEFORE_LOGIN = 1u64 << 30;
        /// Marks the window as modal.
        const MODAL = 1u64 << 31;
        /// Marks windows that cooperate with the built-in window manager.
        const WINDOW_MANAGER_AWARE = 1u64 << 32;
        /// Follows the user across the focused document space.
        const FOLLOWS_DOCUMENT_SPACE = 1u64 << 33;
        /// Excludes the window from mirrored-display reflections.
        const NO_MIRROR_REFLECTION = 1u64 << 34;
        /// Enables an internal compositor meshing mode.
        const MESHED = 1u64 << 35;
        /// Marks a window as a current CoreDrag target.
        const CORE_DRAG_IS_DRAGGING = 1u64 << 36;
        /// Excludes the window from screen-capture streams.
        const AVOIDS_CAPTURE = 1u64 << 37;
        /// Excludes the window from Expose processing.
        const IGNORE_FOR_EXPOSE = 1u64 << 38;
        /// Marks the window as hidden.
        const HIDDEN = 1u64 << 39;
        /// Explicitly includes the window in window cycling.
        const INCLUDE_IN_CYCLE = 1u64 << 40;
        /// Captures gestures while the app is inactive.
        const WANTS_GESTURES_IN_BACKGROUND = 1u64 << 41;
        /// Marks the window as fullscreen.
        const FULL_SCREEN = 1u64 << 42;
        /// Marks the window as the accessibility zoom source.
        const MAGIC_ZOOM = 1u64 << 43;
        /// Keeps the window on all spaces through transitions.
        const SUPER_STICKY = 1u64 << 44;
        /// Allows the window to appear over fullscreen apps.
        const FRIEND_OF_FULLSCREEN = 1u64 << 45;
        /// Attaches the window to the menu bar.
        const MENU_BAR = 1u64 << 46;
        /// Gives the window affinity for the desktop level.
        const DESKTOP_AFFINITY = 1u64 << 47;
        /// Forces the window to remain space-bound.
        const NEVER_STICKY = 1u64 << 48;
        /// Places the window at desktop-picture level.
        const DESKTOP_PICTURE = 1u64 << 49;
        /// Disables workspace-placement heuristics for the window.
        const IGNORES_WORKSPACE_HEURISTICS = 1u64 << 50;
        /// Orders the window forward when it flushes.
        const ORDERS_FORWARD_ON_FLUSH = 1u64 << 51;
        /// Marks the window as a user-input accessory.
        const USER_INPUT_ACCESSORY = 1u64 << 52;
        /// Uses a non-standard compositing backing store.
        const NON_COMPOSITING_BACKING_STORE = 1u64 << 53;
        /// Drags the movement-group parent with the window.
        const DRAGS_MOVEMENT_GROUP_PARENT = 1u64 << 54;
        /// Keeps layered surfaces separate during swipe gestures.
        const NEVER_FLATTEN_SURFACES_DURING_SWIPES = 1u64 << 55;
        /// Allows the window to enter native fullscreen mode.
        const FULL_SCREEN_CAPABLE = 1u64 << 56;
        /// Allows the window to join Split View tile spaces.
        const FULL_SCREEN_TILE_CAPABLE = 1u64 << 57;
        /// Excludes the window from screen sharing.
        const IGNORE_FOR_SCREEN_SHARING = 1u64 << 58;
        /// Shares this child window alongside its parent.
        const SHARE_ALONG_WITH_PARENT = 1u64 << 59;
        /// Marks the window as currently miniaturized.
        const MINIATURIZED = 1u64 << 60;
        /// Enables the shared-window indicator state.
        const WINDOW_SHARING_INDICATOR = 1u64 << 61;
        /// Ignores transient ordering changes during filtering.
        const IGNORE_TRANSIENT_ORDERING_FOR_FILTERING = 1u64 << 62;
        /// Marks the window as having a trivial layer tree.
        const TRIVIAL_LAYER_TREE = 1u64 << 63;

    }
}

impl WindowTags {
    /// Present on every space and, thanks to `SUPER_STICKY`, still present
    /// through the switch animation. The pair a `sticky` option toggles as
    /// one unit; the tag-space's opposite is [`Self::NEVER_STICKY`].
    pub const STICKY: Self = Self::SUPER_STICKY.union(Self::ON_ALL_WORKSPACES);

    /// What a status bar wants: invisible to Expose and capture, and never a
    /// reason to activate us.
    ///
    /// Deliberately excludes the event tags, [`Self::STICKY`] and
    /// [`Self::FRIEND_OF_FULLSCREEN`] — all three are options a running bar
    /// can flip at any time (clickability, `sticky`, `show_in_fullscreen`),
    /// not fixed facts about being a bar, so a caller adds whichever
    /// combination its current settings call for both at window creation and
    /// later, on a live window, via [`crate::Window::set_tags`] and
    /// [`crate::Window::clear_tags`].
    pub const BAR: Self = Self::DISABLE_SHADOW
        .union(Self::IGNORE_FOR_EXPOSE)
        .union(Self::AVOIDS_CAPTURE)
        .union(Self::PREVENTS_ACTIVATION);
}

#[cfg(test)]
mod tests {
    use super::WindowTags;

    /// The five bits something other than this file's own list vouches for,
    /// pinned at the positions those references name.
    ///
    /// Not ceremony: the module docs above say the high half of this set is
    /// contradicted by `CGSInternal` from bit 34 up, so the bits that *are*
    /// corroborated are exactly the ones a future renumbering must not move
    /// silently. Bits 3 and 11 are yabai's; 16 and 38 are `SketchyBar`'s; 58 is
    /// the one `SketchyBar` tests numerically.
    #[test]
    fn the_bits_an_independent_reference_names_sit_where_it_names_them() {
        assert_eq!(WindowTags::DISABLE_SHADOW.bits(), 1 << 3);
        assert_eq!(WindowTags::ON_ALL_WORKSPACES.bits(), 1 << 11);
        assert_eq!(WindowTags::PREVENTS_ACTIVATION.bits(), 1 << 16);
        assert_eq!(WindowTags::IGNORE_FOR_EXPOSE.bits(), 1 << 38);
        assert_eq!(
            WindowTags::IGNORE_FOR_SCREEN_SHARING.bits(),
            0x0400_0000_0000_0000,
            "the bit SketchyBar's iterator_window_suitable tests"
        );
    }

    /// The two the bar's behaviour was checked against live, and the one place
    /// this crate contradicts `CGSInternal` on the strength of that check.
    #[test]
    fn the_two_bits_verified_on_a_running_bar_are_where_the_check_found_them() {
        assert_eq!(WindowTags::SUPER_STICKY.bits(), 1 << 44);
        assert_eq!(WindowTags::FRIEND_OF_FULLSCREEN.bits(), 1 << 45);
        assert_eq!(WindowTags::STICKY.bits(), (1 << 44) | (1 << 11));
    }

    /// `SLSSetWindowTags` takes a width in bits, and `ffi::TAG_BITS` derives it
    /// from this type's size. Sixty-four names is what makes those agree.
    #[test]
    fn every_bit_of_the_bitset_is_named() {
        assert_eq!(WindowTags::all().bits(), u64::MAX);
    }
}

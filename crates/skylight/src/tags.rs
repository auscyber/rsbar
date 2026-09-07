//! Window server window tags.
//!
//! One 64-bit bitset controls most of a window's behaviour that has no public
//! equivalent: which spaces it follows, whether it takes clicks, whether Expose
//! and screen capture can see it. Names and bit positions are the community's
//! reverse-engineering of `CGSWindowTag`; unnamed bits are not "free", they are
//! merely undocumented. `SUPER_STICKY`'s and `FRIEND_OF_FULLSCREEN`'s bit
//! positions and names are cross-checked against `rift`'s independent
//! `SLSWindowTags` bitflags (`sys/skylight.rs`), which lists all 64 bits and
//! agrees exactly on both.
//!
//! Upstream `SketchyBar` does not actually use either tag for its `sticky` or
//! `show_in_fullscreen` options — `window.c`/`bar_manager.c` only ever set
//! `kCGSExposeFadeTagBit`/`kCGSPreventsActivationTagBit` on a window. `sticky`
//! is a dedicated, always-shown `SLSSpaceCreate`d space windows get added to
//! (`window_open`); `show_in_fullscreen` is the bar manager comparing
//! `SLSSpaceGetType(cid, dsid) != 4` on every space change and ordering the
//! window in or out by hand (`bar_manager_handle_space_change`). Neither
//! mechanism was ported here — this crate instead relies on the tag bits
//! directly, which is the simpler design if they hold up, but is consequently
//! *not* cross-checked against a reference implementation actually using them
//! for this purpose.
//!
//! Checked live instead, through `rsbar`'s own bar rather than an isolated
//! window (`skylight/examples/window_tags.rs` has the detail):
//! `FRIEND_OF_FULLSCREEN` was confirmed both ways — a bar built with it draws
//! over a real native-fullscreen app, one built without it vanishes the
//! instant fullscreen engages. `STICKY` was only confirmed one way — a bar
//! built with it survives a real, verified (`SLSGetActiveSpace`-checked)
//! space switch; the `sticky: false` side of that comparison was not run.
//! See [`WindowTags::STICKY`] and
//! [`WindowTags::FRIEND_OF_FULLSCREEN`].

use bitflags::bitflags;

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct WindowTags: u64 {
        /// Floats above normal windows within its level.
        const FLOATING              = 1 << 1;
        /// Suppresses the system drop shadow. A bar wants this.
        const DISABLE_SHADOW        = 1 << 3;
        /// Clicks fall through to whatever is underneath.
        const IGNORE_FOR_EVENTS     = 1 << 9;
        /// The inverse: the window is hit-testable. Needed before it can be clicked.
        const OPAQUE_FOR_EVENTS     = 1 << 10;
        /// Present on every space.
        const ON_ALL_WORKSPACES     = 1 << 11;
        /// Clicking it does not activate the owning process.
        const AVOIDS_ACTIVATION     = 1 << 15;
        /// The owning process can never be activated by it.
        const PREVENTS_ACTIVATION   = 1 << 16;
        /// Skipped by cmd-tab style cycling.
        const IGNORES_CYCLE         = 1 << 18;
        /// Never counts as the front window.
        const IGNORE_AS_FRONT       = 1 << 21;
        /// Excluded from screen capture and screenshots.
        const AVOIDS_CAPTURE        = 1 << 37;
        /// Excluded from Expose / Mission Control.
        const IGNORE_FOR_EXPOSE     = 1 << 38;
        /// Stays put across space transitions, not just on every space.
        /// `ON_ALL_WORKSPACES` alone drops out during the animation.
        const SUPER_STICKY          = 1 << 44;
        /// Allowed to draw over native fullscreen apps. Without this a bar
        /// disappears the moment anything goes fullscreen.
        const FRIEND_OF_FULLSCREEN  = 1 << 45;
        /// Attaches to the menu bar.
        const MENU_BAR              = 1 << 46;
        const DESKTOP_AFFINITY      = 1 << 47;
        const NEVER_STICKY          = 1 << 48;
        const DESKTOP_PICTURE       = 1 << 49;
        const IGNORES_WORKSPACE_HEURISTICS = 1 << 50;
    }
}

impl WindowTags {
    /// Present on every space and, thanks to `SUPER_STICKY`, still present
    /// through the switch animation — `ON_ALL_WORKSPACES` alone drops out
    /// mid-transition (see its own doc). The pair a `sticky` option toggles
    /// as one unit; the tag-space's opposite is [`Self::NEVER_STICKY`].
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

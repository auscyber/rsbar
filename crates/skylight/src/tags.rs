//! Window server window tags.
//!
//! One 64-bit bitset controls most of a window's behaviour that has no public
//! equivalent: which spaces it follows, whether it takes clicks, whether Expose
//! and screen capture can see it. Names and bit positions are the community's
//! reverse-engineering of `CGSWindowTag`; unnamed bits are not "free", they are
//! merely undocumented.

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
    /// What a status bar wants: visible everywhere including over fullscreen,
    /// invisible to Expose and capture, and never a reason to activate us.
    ///
    /// Deliberately excludes both event tags — add [`Self::OPAQUE_FOR_EVENTS`]
    /// to take clicks or [`Self::IGNORE_FOR_EVENTS`] to pass them through.
    pub const BAR: Self = Self::ON_ALL_WORKSPACES
        .union(Self::SUPER_STICKY)
        .union(Self::FRIEND_OF_FULLSCREEN)
        .union(Self::DISABLE_SHADOW)
        .union(Self::IGNORE_FOR_EXPOSE)
        .union(Self::AVOIDS_CAPTURE)
        .union(Self::PREVENTS_ACTIVATION);
}

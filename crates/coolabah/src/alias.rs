//! Alias items: mirroring another application's menu bar item into our bar.
//!
//! The window server keeps every menu bar extra as an ordinary window at
//! [`MENU_BAR_LAYER`], owned (per `kCGWindowOwnerPID`) by the process that
//! drew it — but on macOS 26 many extras (Sound, Clock, Focus, Bluetooth's
//! "`BentoBox`" module, ...) are drawn directly by **Control Center**, so
//! `kCGWindowOwnerPID`/`kCGWindowOwnerName` both say "Control Center" no
//! matter which module it is. `kCGWindowName` is often a real title ("Sound",
//! "Clock", "`FocusModes`"), but several unrelated modules also share the
//! placeholder name `Item-0`. The window is still capturable either way —
//! window server geometry does not care who owns a window — but a bare
//! `Item-0` cannot be told apart from the four other windows also called
//! `Item-0`.
//!
//! Two fixes apply, both cross-checked against `SketchyBar`:
//! - **Recovering a real owner**, from `source_pid.m`: every running
//!   application exposes its status items through `AXExtrasMenuBar`, titled
//!   and positioned in screen coordinates, whether or not that application
//!   ends up compositing its own window. When a window's owner looks like
//!   Control Center, this module walks every running application's extras
//!   menu bar and matches the window's bounds against an AX element's frame.
//!   A match within a few points recovers the item's real owner and name; it
//!   does not fire for the genuinely Control Center-native modules (Sound,
//!   Clock, `FocusModes`, `BentoBox`), because there is no other process to
//!   recover. An unresolved item is reported under the owner "Control
//!   Center", not silently dropped.
//! - **Disambiguating what's left**, from alias.c's "rework alias logic to
//!   handle duplicate entries": [`list_menu_bar_items`] appends a `(n)`
//!   suffix to every name that repeats, so what would otherwise be five
//!   indistinguishable `Item-0`s become `Item-0(1)` through `Item-0(5)`.
//!
//! Both permissions this relies on fail silently:
//! - **Screen Recording** — without it [`skylight::Window::capture`] leaves
//!   the image null; this module surfaces that as [`Error::Window`].
//! - **Accessibility** — without it every `AXUIElementCopyAttributeValue`
//!   call returns [`AXError::APIDisabled`], so the Control Center workaround
//!   silently finds nothing. [`accessibility_trusted`] reports this ahead of
//!   time so a caller can tell "no items" from "no permission".
//!
//! # Nothing here runs on a clock
//!
//! `ecs::refresh_aliases` only runs a pass at all when something asked for
//! one. Two push routes were investigated live (`skylight/examples/notify_probe.rs`,
//! `coolabah/examples/ax_observer_probe.rs`, macOS 26.5.1) and both ruled out:
//!
//! - **`SkyLight`/CGS window notifications** (what `sources/spaces.rs` uses
//!   for space events): registering every plausibly-relevant `kCGSEvent*` —
//!   window move/resize/reorder/visibility, dirty-screen, connection
//!   visibility, title-changed, menu bar creation/style, space-window-transaction
//!   — always reports success, but nothing ever arrives, not even for a
//!   window this process owns and moves itself. Upstream `SketchyBar` agrees:
//!   its one content-adjacent registration, `kCGSWindowTitleChanged`, only
//!   *pauses* its capture poll during a transition, never triggers one.
//! - **`AXObserver`**, at two scopes:
//!   - On the item's own `AXUIElement`: rejected immediately with
//!     `kAXErrorNotificationUnsupported` for both value- and title-changed —
//!     `AXMenuBarItem` does not participate in AX's notification system at
//!     all. Unused.
//!   - On the *owning application's* top-level `AXUIElement`: accepted, and
//!     it fires — but scoped to the whole application, not the item: the
//!     same run also caught an unrelated popover firing `AXValueChanged` once
//!     a second under the same pid. [`crate::alias_watch`] turns that into a
//!     usable signal: one observer per pid an aliased item resolves to, every
//!     notification checked against both the notified element's pid
//!     (`AXUIElementGetPid`, which is what rules the unrelated app's traffic
//!     out) and its `AXDescription`/`AXTitle` against the alias's name — or,
//!     for an item whose `(n)`-suffixed name can never match, its on-screen
//!     frame against where the window server put its window. See
//!     [`crate::alias_watch`]'s module doc for the full mechanism.
//!
//! What makes leaning on that safe is [`Snapshot::still_is`]: the window
//! server will happily keep capturing a `WindowId` whose real item was
//! destroyed and rebuilt out from under it, producing plausible pixels
//! forever with no error at any point, and neither the capture nor a push
//! notification can see that. Confirming the cached id still carries this
//! owner and name costs no Accessibility call and no round trip -- it reads
//! [`Captures::table`], a shared list kept across passes rather than
//! refetched by each one (see the next section) -- so it still happens on
//! every capture, replacing a counter that re-resolved every 240th one and
//! hoped. The trade for not refetching every pass is that this check is only
//! as fresh as the table's last repair: a window torn down and rebuilt
//! between two repairs reads as unchanged until the next one, which
//! [`Captures::refresh`] asks for the moment it notices (see
//! [`Captures::table`]'s doc for when that is).
//!
//! # A window list kept, not fetched
//!
//! [`Captures::table`] holds the menu bar layer as of its last repair, not a
//! fresh read for the pass in progress: every alias resolves and every
//! `still_is` check reads the same `Arc<RwLock<Snapshot>>`, so a pass that
//! resolves nothing takes a read lock and nothing else — no
//! `CGWindowListCopyWindowInfo`, no plist parse, measured at about a fifth of
//! this daemon's CPU under a many-alias config when it ran on every pass
//! instead.
//!
//! Repairing it — the walk itself — runs on a worker ([`Lister`]) and writes
//! back under a write lock on the pass that collects it, so the compositing
//! thread never blocks on the window server. Four edges ask for a repair, and
//! only these: an alias resolving for the very first time and missing, an
//! alias whose cached window vanished from the table (`still_is` turned
//! false), an explicit rescan (an application launching, the Accessibility
//! grant arriving), and a display change ([`Captures::note_reconfigured`]),
//! which moves every window in the layer at once and so is the one edge no
//! individual alias could notice for itself. An alias that looked and simply
//! is not running never asks — that would be the polling this replaced.
//!
//! Because the table is not refetched every pass, this is the daemon's
//! honest limit on how quickly it notices a window torn down and rebuilt
//! under the same id with no notification of its own: not "by the next
//! capture" but "by the next repair", which the four edges above start as
//! soon as anything notices a reason to.
//!
//! # The Accessibility walk is not on this thread
//!
//! Recovering a Control Centre item's real owner means asking every running
//! application over synchronous cross-process Accessibility calls. Serially,
//! on the thread that composites the bar, that measured **2.6 seconds** here
//! (a single stalled application was 1.5 s of it). Two fixes, both needed:
//! [`skylight::ax::set_messaging_timeout`] bounds each application to
//! [`AX_TIMEOUT`], bringing the serial walk to 277 ms; and
//! [`extras::Scanner`] runs what is left off this thread, one task per pid,
//! so the cost is the slowest single application (255 ms) rather than the
//! sum, and the main thread waits for none of it.
//!
//! A scan is asked for by an *event*: an application launching, the grant
//! arriving, or an alias failing the very first resolve it ever attempts.
//! Never by an alias that looked again and still did not find itself — that
//! is a poll with extra steps, and an alias naming an application that is not
//! running would ask forever.
//!
//! # Nothing is kept for an item nobody can see
//!
//! An alias inside a closed popup, or one whose `drawing` is off, holds no
//! [`Captured`] image, no [`Mirror`], no dirty subscription and no
//! `AXObserver` — `ecs::refresh_aliases` drops all four the moment it goes
//! out of sight, and resolves it again when it comes back. `--query` still
//! answers for such an item — its spec and its properties are components,
//! and neither needs a picture.
//!
//! # The left-hand items are not windows
//!
//! `list_menu_bar_items` only finds items at [`MENU_BAR_LAYER`], which is
//! every right-hand extra and nothing on the left. The Apple menu and an
//! application's own titles are painted into one shared window-server
//! surface and have no window of their own. See [`crate::menus`], which
//! lists and presses them instead.

use crate::extras;
use objc2_core_foundation::{CFArray, CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGDataProvider, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo,
    CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess, CGWindowListCopyWindowInfo,
    CGWindowListOption,
};
use skylight::WindowId;
use skylight::ax;
use skylight::cf::{Dict, WindowKey};
use std::cell::RefCell;
use std::sync::{Arc, Mutex, RwLock};

/// `CGWindowLevelForKey(kCGStatusWindowLevelKey)`'s window list layer.
/// Stable for two decades; `SketchyBar` hardcodes the same value.
const MENU_BAR_LAYER: i64 = 0x19;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("the window server would not list its windows")]
    NoWindowList,
    #[error("no menu bar item matches this alias")]
    NotFound,
    #[error(
        "pressing this menu bar item did not succeed (Accessibility permission is likely missing)"
    )]
    PressFailed,
    #[error(transparent)]
    Window(#[from] skylight::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Whether this process can capture other windows' pixels.
///
/// Absence is silent everywhere else in this module, so check this once and
/// report it — a config with no aliasable items and a config that can't
/// capture anything look identical otherwise.
#[must_use]
pub fn screen_capture_trusted() -> bool {
    CGPreflightScreenCaptureAccess()
}

/// Prompts the user for Screen Recording access if it is not already granted.
/// A no-op, returning `true` immediately, if it already is.
#[must_use]
pub fn request_screen_capture() -> bool {
    CGRequestScreenCaptureAccess()
}

/// Whether this process can query other applications' accessibility trees.
///
/// Needed only for the macOS 26 Control Centre workaround — a menu bar item
/// still owned by its own process aliases fine without this.
///
/// # Errors
///
/// [`skylight::Error::NotTrusted`] if the grant is missing.
pub fn accessibility_trusted() -> skylight::Result<()> {
    ax::trusted()
}

/// Prompts the user for Accessibility access if it is not already granted.
#[must_use]
pub fn request_accessibility() -> bool {
    ax::request()
}

/// One menu bar item a config can name in an alias, as `--query
/// default_menu_items` would report it.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuBarItem {
    pub owner: String,
    pub name: String,
    pub pid: i32,
}

/// Lists every aliasable menu bar item currently on screen, left to right.
///
/// An item whose owner could not be resolved off Control Center is still
/// listed under the owner "Control Center" (see the module docs). A name
/// repeated within this listing gets a `(n)` suffix, 1-based in
/// left-to-right order among that duplicate group — use the suffixed form
/// verbatim when constructing an [`Alias`] for such an item.
///
/// # Errors
///
/// Returns [`Error::NoWindowList`] if the window server refuses the list,
/// which is what a missing Screen Recording grant looks like from here.
pub fn list_menu_bar_items(snapshot: &Snapshot) -> Result<Vec<MenuBarItem>> {
    let mut windows: Vec<&RawWindow> = snapshot
        .windows
        .iter()
        .filter(|w| !w.name.is_empty())
        .collect();
    windows.sort_by(|a, b| a.bounds.origin.x.total_cmp(&b.bounds.origin.x));
    Ok(windows
        .into_iter()
        .map(|w| MenuBarItem {
            owner: w.owner.clone(),
            name: w.name.clone(),
            pid: w.pid,
        })
        .collect())
}

/// Presses the menu behind an already-listed alias item — `AXCancelAction`
/// then `AXPressAction` on its `AXExtrasMenuBar` element, the same sequence
/// `SketchyBar`'s own menu helper uses — so a click on a mirrored item opens
/// the real menu instead of only showing a picture of it. Does not touch this
/// item's [`Captures`]: the resulting menu is the system's own window, not a
/// capture.
///
/// A name carrying the `(n)` duplicate suffix from
/// [`disambiguate_duplicates`] cannot be matched by identity (the real
/// `AXDescription`/`AXTitle` never carries that suffix) and falls back to the
/// nearest `AXExtrasMenuBar` child by on-screen frame — the same tolerance
/// [`extras::resolve`] uses, scoped to one already-known pid.
///
/// # Errors
///
/// [`Error::NotFound`] if no menu bar item currently matches `owner`/`name`,
/// or nothing in that owner's `AXExtrasMenuBar` is close enough to be it —
/// which is what missing Accessibility permission looks like from here, same
/// as everywhere else in this module. [`Error::PressFailed`] if a matching
/// element was found but the AX action itself was refused.
pub fn press_item(snapshot: &Snapshot, owner: &str, name: &str) -> Result<()> {
    let access = skylight::ax::Trusted::get()?;
    let window = snapshot.find(owner, name).ok_or(Error::NotFound)?;
    let element =
        extras::find_extras_child(window.pid, name, window.bounds)?.ok_or(Error::NotFound)?;
    Ok(ax::press(access, &element)?)
}

/// A captured menu bar item, ready to draw.
#[derive(Clone)]
pub struct Capture {
    pub image: CFRetained<CGImage>,
    /// The window's true size in screen points — what `SketchyBar` draws the
    /// capture at, which can disagree with the window list's own bounds.
    pub size: CGSize,
}

/// Mirrors one other application's menu bar item.
///
/// Holds the resolved window across captures so a refresh is one window
/// server round trip rather than a full re-scan and re-resolve. The window is
/// re-found automatically if it disappears (the app quit, the item was
/// removed) or a capture fails.
pub struct Alias {
    owner: String,
    name: String,
    /// The resolved window, as the id a [`Snapshot`] is keyed by, so the
    /// cached one can be re-checked against a fresh list without a window
    /// server round trip of its own. This *is* what "holds the resolved
    /// window across captures" above means -- there used to be a second,
    /// separate `window: Option<skylight::Window>` field beside this one
    /// that the same sentence described, and it never held anything: nothing
    /// in [`Alias::resolve`] ever wrote it.
    window_id: Option<WindowId>,
    /// Where the window server last put it. Kept for [`crate::alias_watch`],
    /// which needs a place to match on for an item whose `(n)`-suffixed name
    /// can never match.
    bounds: Option<CGRect>,
    /// The pid the item last resolved under, kept across an [`invalidate`]
    /// so [`crate::alias_watch`] can keep watching the last-known owner while a
    /// fresh resolve is pending, rather than tearing the observer down and
    /// straight back up.
    ///
    /// [`invalidate`]: Alias::invalidate
    pid: Option<i32>,
}

impl Alias {
    #[must_use]
    pub fn new(owner: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
            name: name.into(),
            window_id: None,
            bounds: None,
            pid: None,
        }
    }

    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether this alias is still pointing at a window that is still the
    /// one it names — so there is nothing to re-resolve and nothing new to
    /// draw.
    #[must_use]
    pub fn settled(&self, snapshot: &Snapshot) -> bool {
        self.window_id
            .is_some_and(|id| snapshot.still_is(id, &self.owner, &self.name))
    }

    /// The pid this item last resolved to, if it ever has.
    #[must_use]
    pub fn pid(&self) -> Option<i32> {
        self.pid
    }

    /// Where the window server last put this item, if it ever resolved one.
    #[must_use]
    pub fn bounds(&self) -> Option<CGRect> {
        self.bounds
    }

    /// Forces the next [`Alias::resolve`] to re-find the window rather
    /// than reuse a cached one.
    pub fn invalidate(&mut self) {
        self.window_id = None;
    }

    /// Finds the window this alias currently names, without touching a pixel.
    ///
    /// The cheap half, and the half that must run where the snapshot is: a
    /// couple of hash lookups in a list the pass already built. The expensive
    /// half is [`capture_window`], which needs nothing but the id this
    /// returns and so runs on a worker.
    ///
    /// `None` when no menu bar item goes by this owner and name any more --
    /// the application is not running, or the item was removed.
    pub fn resolve(&mut self, snapshot: &Snapshot) -> Option<WindowId> {
        // The window server will keep capturing a `WindowId` whose real item
        // was destroyed and rebuilt out from under it -- an app tearing down
        // and recreating its `NSStatusItem` -- with no error at any point.
        // Confirming the id still carries this owner and name costs one hash
        // lookup in a list already built for this pass, so it happens every
        // time rather than on a counter.
        if let Some(id) = self.window_id
            && !snapshot.still_is(id, &self.owner, &self.name)
        {
            self.invalidate();
        }
        if self.window_id.is_none() {
            let found = snapshot.find(&self.owner, &self.name)?;
            self.pid = Some(found.pid);
            self.window_id = Some(found.id);
            self.bounds = Some(found.bounds);
        }
        self.window_id
    }

    /// Resolve and capture in one go, on whichever thread asks.
    ///
    /// Not what the daemon does: it resolves on the main thread and captures
    /// on a worker, because the capture is the part that costs a frame. This
    /// is for a diagnostic that wants the answer now and has no run loop to
    /// hand it back to.
    pub fn capture_now(
        &mut self,
        connected: &skylight::Connected,
        snapshot: &Snapshot,
    ) -> Option<Captured> {
        capture_window(connected, self.resolve(snapshot)?)
    }

    /// The window this alias last resolved to, if it has.
    ///
    /// What a capture coming back from a worker is checked against: an alias
    /// that re-resolved while its picture was being taken is not the item that
    /// picture is of.
    #[must_use]
    pub fn window_id(&self) -> Option<WindowId> {
        self.window_id
    }
}

/// A menu bar layer window, with its Control Center ownership already
/// resolved where possible.
struct RawWindow {
    id: WindowId,
    owner: String,
    name: String,
    pid: i32,
    bounds: CGRect,
}

/// Every menu bar layer window before [`list_menu_bar_items`]'s filtering —
/// including ones with an empty name, and Control Center items whose real
/// owner could not be resolved. For diagnosing what the window server and the
/// Accessibility API actually expose on a given machine.
#[derive(Debug, Clone)]
pub struct RawMenuBarWindow {
    pub id: WindowId,
    pub owner: String,
    pub name: String,
    pub pid: i32,
    pub bounds: CGRect,
}

/// Every window at the menu bar layer, unfiltered — empty names and
/// unresolved Control Center items included.
#[must_use]
pub fn diagnostics(snapshot: &Snapshot) -> Vec<RawMenuBarWindow> {
    snapshot
        .windows
        .iter()
        .map(|w| RawMenuBarWindow {
            id: w.id,
            owner: w.owner.clone(),
            name: w.name.clone(),
            pid: w.pid,
            bounds: w.bounds,
        })
        .collect()
}

/// One look at the menu bar layer: every window on it, attributed and
/// disambiguated, as of a single window server call.
///
/// Made once and shared by every alias that reads it. That is the whole
/// point: twenty-nine aliases used to mean twenty-nine
/// `CGWindowListCopyWindowInfo` calls and twenty-nine chances to trigger the
/// Accessibility walk, for one list that does not change in between.
pub struct Snapshot {
    windows: Vec<RawWindow>,
}

impl Snapshot {
    /// Lists the menu bar layer now, attributing Control Centre-owned windows
    /// against `extras` — the last completed Accessibility scan, or `None`
    /// when there has not been one yet.
    ///
    /// `None` is a first-class answer, not a degraded one: a window whose
    /// real owner cannot be recovered keeps the owner the window server
    /// reported, which is exactly what a pre-macOS-26 machine reports anyway
    /// and what a machine without the Accessibility grant will always report.
    ///
    /// # Errors
    ///
    /// [`Error::NoWindowList`] if the window server refuses the list.
    pub(crate) fn list(extras: Option<&[extras::ExtrasMenuItem]>) -> Result<Self> {
        let mut windows = list_raw_windows()?;
        attribute_owners(&mut windows, extras);
        disambiguate_duplicates(&mut windows);
        Ok(Self { windows })
    }

    /// Finishes a raw list [`Lister`] fetched off the main thread:
    /// attributes Control Centre ownership against whatever scan is current
    /// *now* (often fresher than the one running when the fetch started),
    /// then disambiguates duplicates. Both steps are cheap and pure -- see
    /// [`list_raw_windows`] for the half that is not.
    fn finish(mut windows: Vec<RawWindow>) -> Self {
        extras::with_last_scan(|extras| attribute_owners(&mut windows, extras));
        disambiguate_duplicates(&mut windows);
        Self { windows }
    }

    /// The same, against whatever the last background scan produced, without
    /// waiting for one.
    ///
    /// What every caller inside the daemon wants — a click opening a mirrored
    /// menu, a `--query default_menu_items`. None of them may block the
    /// compositing thread on an Accessibility walk, and none of them needs
    /// to: the answer is already there, or it is not and the window server's
    /// own owner is used.
    ///
    /// # Errors
    ///
    /// As [`Snapshot::list`].
    pub fn cached() -> Result<Self> {
        extras::with_last_scan(Self::list)
    }

    /// The cached scan if there has ever been one, and otherwise the walk
    /// done here and now.
    ///
    /// For a one-shot demand that must have the *right* answer and has
    /// nowhere to wait for it — `--query default_menu_items`, whose whole
    /// output is owner names, and which a config reads to decide which
    /// aliases to create. Answering that from an empty cache hands back
    /// `Control Centre,Fantastical` for an item really owned by
    /// `Fantastical Helper`, and a config that writes the wrong spec down
    /// keeps it forever.
    ///
    /// At most once: the walk it does is remembered, so the next reader —
    /// and every later pass — gets it for free.
    ///
    /// # Errors
    ///
    /// As [`Snapshot::list`].
    #[skylight::main_thread]
    pub fn cached_or_scan() -> Result<Self> {
        if extras::scanned() {
            return Self::cached();
        }
        Self::now(proof)
    }

    /// The same, doing the Accessibility walk inline and blocking on it,
    /// whether or not one has already been done.
    ///
    /// Only for a one-shot process with nowhere to wait — the CLI, a probe.
    ///
    /// # Errors
    ///
    /// As [`Snapshot::list`]. A missing Accessibility grant is not an error
    /// here; it just leaves Control Centre's own items unattributed.
    #[skylight::main_thread]
    pub fn now() -> Result<Self> {
        scan_extras_blocking(proof);
        Self::cached()
    }

    /// Every `owner,name` pair this snapshot could resolve an alias to.
    ///
    /// For saying what *was* on offer when one did not match: a spec naming a
    /// real item under a slightly different spelling and a spec whose
    /// application is not running are otherwise the same `NotFound`, and they
    /// want opposite responses from whoever reads the log.
    fn items(&self) -> impl Iterator<Item = (&str, &str)> {
        self.windows
            .iter()
            .map(|w| (w.owner.as_str(), w.name.as_str()))
    }

    fn find(&self, owner: &str, name: &str) -> Option<&RawWindow> {
        self.windows
            .iter()
            .find(|w| w.owner == owner && w.name == name)
    }

    /// Whether `id` is still the window this owner and name resolve to.
    ///
    /// Bounds are deliberately not part of it: a status item legitimately
    /// slides sideways whenever a neighbour appears or goes, and treating
    /// that as a destroyed item would force a re-resolve for nothing.
    fn still_is(&self, id: WindowId, owner: &str, name: &str) -> bool {
        self.windows
            .iter()
            .any(|w| w.id == id && w.owner == owner && w.name == name)
    }

    /// Whether anything here is still attributed to Control Centre — the one
    /// condition an Accessibility scan could improve on.
    #[must_use]
    pub fn has_unattributed(&self) -> bool {
        self.windows
            .iter()
            .any(|w| is_control_center_owner(&w.owner))
    }
}

thread_local! {
    /// The Accessibility grant, watched across scans.
    ///
    /// The daemon runs its whole world on one run loop thread, so one cell is
    /// the whole process. Held rather than re-read into a local because both
    /// halves of it are about *change*: the prompt must happen at most once,
    /// and the grant arriving is worth saying out loud exactly once.
    static TRUST: RefCell<ax::Trust> = RefCell::new(ax::Trust::new());
}

/// The Accessibility walk, done here and now on the calling thread, and
/// remembered where every later reader will find it.
///
/// Only for a caller with nowhere to wait — see [`Snapshot::now`]. Does
/// nothing without the Accessibility grant, which the watcher reports once
/// rather than every call site reporting it again.
#[skylight::main_thread(pass)]
fn scan_extras_blocking() {
    report_grant_changes();
    match extras::enumerate_extras_menu_items(mtm) {
        // Remembered, so this is paid at most once: every later reader, and
        // every pass, sees the same answer without walking again.
        Ok(items) => extras::remember_scan(items),
        Err(err) => {
            if err == skylight::Error::NotTrusted {
                TRUST.with_borrow_mut(ax::Trust::prompt);
            }
            tracing::debug!(%err, "cannot recover menu bar item owners");
        }
    }
}

/// How long the walk gives one application to answer.
pub use crate::extras::AX_TIMEOUT;

/// One frame at 60 Hz — the budget for anything on the compositing thread.
const FRAME: std::time::Duration = std::time::Duration::from_micros(16_667);

/// Why one alias is being looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Look {
    /// A notification named this item: what it draws changed, so capture.
    Changed,
    /// Something changed what aliases resolve to -- a fresh owner scan, an
    /// application launching. That is a reason to check *which window* this
    /// is, not a reason to re-read the pixels: an alias still pointing at the
    /// same window has nothing new to draw, and a capture is a window server
    /// screen grab measured here at around twenty milliseconds.
    Resolve,
}

/// Which aliases a pass has a reason to look at. See [`Captures::due`].
pub enum Due {
    /// Every one of them: something changed what an alias would resolve to.
    Everything,
    /// Only these, because a notification named them and nothing else.
    These(Vec<bevy_ecs::entity::Entity>),
}

/// The Accessibility walk, timed one application at a time, for
/// `examples/alias_probe.rs`.
///
/// Here rather than in the probe because the walk is `pub(crate)`, and it
/// stays that way: what a caller outside this crate has any business with is
/// a [`Snapshot`], not the machinery that attributes one. `timeout` of `None`
/// is the Accessibility API's own default, which is what this cost before
/// [`AX_TIMEOUT`] existed.
///
/// Returns `(application, pid, elapsed, items found)`.
#[doc(hidden)]
#[must_use]
#[skylight::main_thread(pass)]
pub fn time_extras_scan(
    timeout: Option<std::time::Duration>,
) -> Vec<(String, i32, std::time::Duration, usize)> {
    let Some(access) = skylight::ax::Trusted::now() else {
        return Vec::new();
    };
    extras::running_apps(mtm)
        .into_iter()
        .map(|(pid, owner)| {
            let started = std::time::Instant::now();
            let found = extras::extras_for_pid(access, pid, &owner, timeout).len();
            (owner, pid, started.elapsed(), found)
        })
        .collect()
}

/// Says once, out loud, when the Accessibility grant arrives or is taken
/// away. Not a restart: `AXIsProcessTrusted` re-reads the TCC database every
/// time, so a box ticked in System Settings takes effect on the next look.
fn report_grant_changes() {
    if let Some(granted) = TRUST.with_borrow_mut(ax::Trust::poll) {
        tracing::info!(granted, "the Accessibility grant changed");
    }
}

/// The window server round trip and the binary-plist parse behind it, with
/// no Accessibility-derived ownership fixup -- the half of [`Snapshot::list`]
/// that needs no thread-local and no main thread, so [`Lister`] can run it on
/// a worker. Measured at a fifth of this daemon's CPU under a many-alias
/// config.
fn list_raw_windows() -> Result<Vec<RawWindow>> {
    let list =
        CGWindowListCopyWindowInfo(CGWindowListOption::OptionAll, 0).ok_or(Error::NoWindowList)?;
    Ok(parse_window_list(&list))
}

/// Every menu bar layer window the window server currently reports, before
/// anything is known about who really owns it.
///
/// Split from [`attribute_owners`] at the Accessibility boundary. This half
/// is a window server call — fast, main-thread-legal, no `AXUIElement`
/// anywhere in it. The other half is pure.
fn parse_window_list(list: &CFArray) -> Vec<RawWindow> {
    let mut windows = Vec::new();
    for i in 0..list.count() {
        let Some(dict) = Dict::at(list, i) else {
            continue;
        };

        let Some(layer) = dict.get::<i64>(WindowKey::Layer) else {
            continue;
        };
        if layer != MENU_BAR_LAYER {
            continue;
        }

        let Some(owner) = dict.get::<String>(WindowKey::OwnerName) else {
            continue;
        };
        if owner == "Window Server" {
            continue;
        }
        let Some(pid) = dict.get::<i64>(WindowKey::OwnerPid) else {
            continue;
        };
        let Some(id) = dict.get::<i64>(WindowKey::Number) else {
            continue;
        };
        let Some(bounds) = dict.get::<CGRect>(WindowKey::Bounds) else {
            continue;
        };
        let name = dict.get::<String>(WindowKey::Name).unwrap_or_default();

        windows.push(RawWindow {
            id: WindowId::try_from(id).unwrap_or(WindowId::MAX),
            owner,
            name,
            pid: i32::try_from(pid).unwrap_or(i32::MAX),
            bounds,
        });
    }
    windows
}

/// Recovers the real owner of every window the window server blamed on
/// Control Centre, as far as `extras` allows.
///
/// Pure, and the reason the split exists: `extras` is the expensive,
/// off-thread half, and it is an *input* here rather than something this
/// reaches out for. `None` means no scan has landed yet, which every caller
/// already handles — an unrecovered window keeps the owner the window server
/// gave it, exactly as it does on a machine with no Accessibility grant.
fn attribute_owners(windows: &mut [RawWindow], extras: Option<&[extras::ExtrasMenuItem]>) {
    let Some(extras) = extras else {
        return;
    };
    for window in windows {
        if !is_control_center_owner(&window.owner) {
            continue;
        }
        if let Some(item) = extras::resolve(&window.name, window.bounds, extras) {
            window.owner.clone_from(&item.owner);
            window.pid = item.pid;
        }
    }
}

/// Appends a `(n)` suffix, 1-based in left-to-right order, to the name of
/// every window whose `(owner, name)` is not already unique — matching
/// upstream `SketchyBar`'s fix for the same problem. Windows with a unique
/// name are untouched.
fn same_item(a: &RawWindow, b: &RawWindow) -> bool {
    a.owner == b.owner && a.name == b.name
}

fn disambiguate_duplicates(windows: &mut [RawWindow]) {
    // One sort, no hashing, no cloned keys: ordering by owner, then name,
    // then x puts every set of identically-named windows in one contiguous
    // run, already left to right, so the suffix is just the position within
    // the run.
    let mut order: Vec<usize> = (0..windows.len()).collect();
    order.sort_unstable_by(|&a, &b| {
        let (l, r) = (&windows[a], &windows[b]);
        l.owner
            .cmp(&r.owner)
            .then_with(|| l.name.cmp(&r.name))
            .then_with(|| l.bounds.origin.x.total_cmp(&r.bounds.origin.x))
    });

    let mut start = 0;
    while start < order.len() {
        let mut end = start + 1;
        while end < order.len() && same_item(&windows[order[start]], &windows[order[end]]) {
            end += 1;
        }
        if end - start > 1 {
            for (nth, &i) in order[start..end].iter().enumerate() {
                windows[i].name = format!("{}({})", windows[i].name, nth + 1);
            }
        }
        start = end;
    }
}

fn is_control_center_owner(owner: &str) -> bool {
    matches!(owner, "Control Center" | "ControlCenter" | "Control Centre")
}

pub(crate) fn rect_center(rect: CGRect) -> CGPoint {
    CGPoint::new(
        rect.origin.x + rect.size.width / 2.0,
        rect.origin.y + rect.size.height / 2.0,
    )
}

pub(crate) fn point_distance(a: CGPoint, b: CGPoint) -> f64 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    dx.hypot(dy)
}

/// A captured image that may be moved between threads.
///
/// The one place this module asserts anything about threads, and deliberately
/// as narrow as the assertion really is: it covers a single immutable image
/// and nothing else. Putting `unsafe impl Send` on the struct that *carries*
/// one would keep applying to whatever field someone adds later — which is
/// exactly how such an assertion quietly stops being true.
///
/// Dereferences to the image, so a caller drawing one never sees the wrapper.
///
/// # Safety
///
/// A `CGImage` is immutable from the moment the window server returns it —
/// nothing here ever draws into one — and CoreFoundation's retain and release
/// are atomic. Moving one is therefore a transfer of ownership with no shared
/// mutable state behind it, which is the whole of the argument.
pub struct SendImage(CFRetained<CGImage>);

// SAFETY: see the type's own doc comment.
unsafe impl Send for SendImage {}

impl SendImage {
    /// Takes ownership of an image for the trip to another thread.
    #[must_use]
    pub const fn new(image: CFRetained<CGImage>) -> Self {
        Self(image)
    }
}

impl std::ops::Deref for SendImage {
    type Target = CGImage;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::fmt::Debug for SendImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendImage")
            .field("width", &CGImage::width(Some(&self.0)))
            .field("height", &CGImage::height(Some(&self.0)))
            .finish()
    }
}

pub struct Captured {
    pub image: SendImage,
    /// The full captured window's size, in points. Most of it is margin —
    /// measured on the Control Centre clock, 296x60 device px capture, only
    /// 239x25 is actually inked (the rest is the system's own inter-item
    /// spacing, baked into the window). Kept in full alongside [`Self::trim`]
    /// because `image` is drawn at this size; only what a caller lays out
    /// space for should shrink.
    pub size: CGSize,
    /// The inked (non-transparent) sub-rectangle within `image`, in the same
    /// point space as [`Self::size`] — origin measured from the image's own
    /// top-left corner, y increasing downward. A layout wanting this item's
    /// real on-screen footprint should use `trim.size`, not `size`, and a
    /// caller drawing the full image at some destination rect should offset
    /// that rect's origin by `-trim.origin` so the ink lands where the
    /// (trimmed-width) layout put it, then clip to the trimmed rect so the
    /// untrimmed margin does not spill onto a neighbour.
    ///
    /// Equal to `CGRect::new(CGPoint::ZERO, size)` — the untrimmed rect —
    /// when the capture came back fully transparent (a legitimately blank
    /// item, not zero-width) or in the one pixel format this module does not
    /// know how to read alpha out of.
    pub trim: CGRect,
    /// A digest of the pixels, so an icon that did not change can be told
    /// from one that did. `SketchyBar` re-draws an alias whether or not it moved; a clock
    /// that only changes once a minute should not cost a repaint every second.
    pub digest: u64,
}

/// A finished capture on its way back from the worker that took it.
///
/// Carries no `unsafe` assertion of its own: every field is already `Send`,
/// the image because [`SendImage`] says so and the rest because they are plain
/// values. Adding a field that is not `Send` makes this struct stop being
/// `Send`, and the compiler says so — which is the point of putting the
/// assertion on the image rather than here.
struct InFlight {
    entity: bevy_ecs::entity::Entity,
    /// The spec it was taken for. A mirror re-specced while this was out is a
    /// different item now, and its pixels are not this one's.
    spec: String,
    /// The window it came from, so a result for a window the mirror has since
    /// re-resolved away from is discarded rather than drawn.
    window: WindowId,
    captured: Captured,
}

/// How many captures may be in the air at once.
///
/// A capture is a window server round trip, so the useful number is "several",
/// not "one per alias": a config with seventy-five of them should not open
/// seventy-five conversations with the window server in one pass. A bound on
/// the conversation, not a thread count -- the threads are
/// [`crate::pool`]'s, and exist only while a round trip is actually out.
const CAPTURE_WORKERS: usize = 4;

/// Takes menu bar item pictures off the thread that draws them.
///
/// The same shape as [`extras::Scanner`], for the same reason: the expensive
/// half of a capture — the window server round trip, hashing every pixel to
/// tell whether anything moved, and finding the inked sub-rectangle — needs
/// nothing from the main thread but a window id, while the main thread is the
/// only place the result can be drawn. Measured on a config with seventy-five
/// aliases, doing it inline cost **115 ms of every second** on the compositing
/// thread, which is three whole frames dropped per second, every second.
///
/// Resolution stays on the main thread. It is a hash lookup in a snapshot that
/// pass already built, and it is the half that must see a consistent view of
/// the menu bar layer.
pub(crate) struct Captor {
    /// How many captures may be in the air at once; see [`CAPTURE_WORKERS`].
    permits: Arc<tokio::sync::Semaphore>,
    wake: crate::runloop::Waker,
    /// Captures that have landed and not yet been collected.
    finished: Arc<Mutex<Vec<InFlight>>>,
    /// Whether [`Captor::finished`] holds anything, readable without the
    /// lock -- see [`extras::Scanner`]'s own flag for why a run condition may
    /// not take a lock a worker holds.
    landed: Arc<std::sync::atomic::AtomicBool>,
    /// Which entities have a capture out. One item asked for twice before the
    /// first lands would otherwise pay twice for the same picture.
    out: Arc<Mutex<std::collections::HashSet<bevy_ecs::entity::Entity>>>,
}

impl Captor {
    pub(crate) fn new(wake: crate::runloop::Waker) -> Self {
        Self {
            permits: Arc::new(tokio::sync::Semaphore::new(CAPTURE_WORKERS)),
            wake,
            finished: Arc::new(Mutex::new(Vec::new())),
            landed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            out: Arc::new(Mutex::new(std::collections::HashSet::new())),
        }
    }

    /// Starts capturing `window` for `entity`, unless one is already out for
    /// it.
    fn submit(&self, entity: bevy_ecs::entity::Entity, spec: &str, window: WindowId) {
        if !self
            .out
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(entity)
        {
            return;
        }
        let spec = spec.to_owned();
        let finished = Arc::clone(&self.finished);
        let landed = Arc::clone(&self.landed);
        let out = Arc::clone(&self.out);
        let wake = self.wake.clone();
        let permits = Arc::clone(&self.permits);
        crate::pool::spawn(async move {
            // Waiting for a turn is asynchronous; the round trip to the
            // window server is not, and has no form that is.
            let _turn = permits.acquire_owned().await;
            // The window server connection, for the round trip only. Awaited
            // rather than blocked on: whatever holds it -- a frame's window
            // work, another capture -- is holding it for a round trip too,
            // and a worker parked in the kernel would be one fewer doing the
            // analysis that follows.
            let connected = skylight::acquire().await;
            let taken = crate::pool::blocking(move || capture_window(&connected, window))
                .await
                .ok()
                .flatten();
            // Cleared before the wake, so the pass this wakes may ask for
            // this entity again and be heard.
            out.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&entity);
            if let Some(captured) = taken {
                finished
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(InFlight {
                        entity,
                        spec,
                        window,
                        captured,
                    });
                landed.store(true, std::sync::atomic::Ordering::Release);
            }
            wake.wake();
        });
    }

    /// Everything that has landed since the last ask.
    fn take(&self) -> Vec<InFlight> {
        let mut held = self
            .finished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let taken = std::mem::take(&mut *held);
        // Cleared while the lock is held, not before or after it. A worker
        // publishes by pushing under this lock and *then* raising the flag,
        // so clearing outside it can swallow a capture that landed in
        // between -- and a swallowed flag is a wake that never happens, which
        // on an idle bar means pixels that never arrive.
        self.landed
            .store(false, std::sync::atomic::Ordering::Release);
        drop(held);
        taken
    }

    /// Whether anything is waiting to be collected — the run condition's half.
    fn ready(&self) -> bool {
        self.landed.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// The whole expensive half of a capture, on whatever thread runs it.
///
/// Everything a drawn alias needs is produced here — the pixels, their true
/// size, the digest that says whether they moved, and the inked rectangle —
/// so the main thread's share is a hash comparison and a move.
// This *is* the worker: `Captor` spawns this so the two window server calls
// below are not on the thread that draws. Every other caller should be
// submitting to `Captor` instead, which is what the lint is there to say.
#[expect(
    clippy::disallowed_methods,
    reason = "the capture pool is where these belong"
)]
fn capture_window(connected: &skylight::Connected, window: WindowId) -> Option<Captured> {
    let image = skylight::capture(connected, window).ok()?;
    let size = skylight::true_rect(connected, window).ok()?.size;
    let Analysis { digest, trim_px } = analyze(&image);
    let trim = trim_rect(&image, size, trim_px);
    Some(Captured {
        image: SendImage::new(image),
        size,
        trim,
        digest,
    })
}

/// Fetches the menu bar window list off the compositing thread.
///
/// The same shape as [`Captor`] and [`extras::Scanner`], for the same
/// reason: [`list_raw_windows`] -- the window server round trip and the
/// binary-plist parse behind it -- needs nothing off the main thread, while
/// finishing the result ([`Snapshot::finish`]) reads `extras`' thread-local
/// scan and so must run where that pass runs.
struct Lister {
    wake: crate::runloop::Waker,
    /// Set while a fetch is out, so a burst of aliases all wanting the list
    /// costs one round trip rather than one each.
    running: Arc<std::sync::atomic::AtomicBool>,
    finished: Arc<Mutex<Option<Result<Vec<RawWindow>>>>>,
    /// Whether `finished` holds anything, readable without the lock -- see
    /// [`Captor::landed`] for why.
    landed: Arc<std::sync::atomic::AtomicBool>,
}

impl Lister {
    fn new(wake: crate::runloop::Waker) -> Self {
        Self {
            wake,
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            finished: Arc::new(Mutex::new(None)),
            landed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Starts a fetch, unless one is already out.
    fn request(&self) {
        if self.running.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let finished = Arc::clone(&self.finished);
        let landed = Arc::clone(&self.landed);
        let running = Arc::clone(&self.running);
        let wake = self.wake.clone();
        crate::pool::spawn(async move {
            // A panicked worker still answers, rather than leaving `running`
            // stuck true and every later pass believing a fetch is out.
            let result = crate::pool::blocking(list_raw_windows)
                .await
                .unwrap_or(Err(Error::NoWindowList));
            *finished
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
            landed.store(true, std::sync::atomic::Ordering::Release);
            running.store(false, std::sync::atomic::Ordering::Release);
            wake.wake();
        });
    }

    /// Collects a finished fetch, if one landed.
    ///
    /// Called every pass regardless of whether this particular pass wants a
    /// list: a fetch that lands after nothing needs it any more (the entity
    /// that asked went out of sight) must still clear `landed`, or the run
    /// condition reads true forever and the pass runs on every tick doing
    /// nothing. See [`Captor::take`], which makes the same trade.
    ///
    /// Cleared unconditionally, under the same lock as the take -- not only
    /// when something was actually taken. The worker publishes as two
    /// separate steps, `*finished.lock() = Some(_)` and then
    /// `landed.store(true)` (see [`Lister::request`]); a `take` landing in
    /// the gap between them would drain the value and leave `landed` still
    /// true from the store still to come, and clearing only inside the
    /// `Some` arm would never see it again -- the slot stays `None` forever
    /// after, so the one arm that could reset the flag never runs.
    /// Clearing every call self-heals from that race the same way
    /// [`Captor::take`] already does.
    fn take(&self) -> Option<Result<Vec<RawWindow>>> {
        let mut held = self
            .finished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let taken = held.take();
        self.landed
            .store(false, std::sync::atomic::Ordering::Release);
        drop(held);
        taken
    }

    /// Whether a finished fetch is waiting to be collected — the run
    /// condition's half.
    fn ready(&self) -> bool {
        self.landed.load(std::sync::atomic::Ordering::Acquire)
    }
}

pub struct Captures {
    mirrors: std::collections::HashMap<bevy_ecs::entity::Entity, Mirror>,
    /// Every entity's live AX notification registration, if it has one. See
    /// [`crate::alias_watch`].
    watch: crate::alias_watch::AxWatch,
    /// The Accessibility walk, owned rather than called: it runs on worker
    /// threads and lands here between passes. See [`extras::Scanner`].
    scan: extras::Scanner,
    /// The pictures themselves, taken the same way and for the same reason.
    /// See [`Captor`].
    captor: Captor,
    /// The window list, fetched the same way but kept rather than handed out
    /// per pass — see [`Lister`] and [`Captures::table`].
    lister: Lister,
    /// The menu bar layer as of its last repair, shared with whatever holds
    /// a clone of [`Captures::table`]. Read every pass; written only when a
    /// [`Lister`] fetch lands, in [`Captures::begin_pass`].
    table: Arc<RwLock<Snapshot>>,
    /// Set alongside `rescan` and cleared by [`Captures::note_rescan`],
    /// rather than acted on in the same pass that raised it -- the pass that
    /// raises it may have no repaired table yet to check `has_unattributed`
    /// against.
    note_rescan_owed: bool,
    /// Set by something that could plausibly have changed who owns a status
    /// item -- a new or edited alias, an application launching, the
    /// Accessibility grant arriving. Cleared by the scan it causes.
    ///
    /// This is what keeps the scan edge-triggered. "Still unresolved" is not
    /// a cause: an alias naming an application that is not running would ask
    /// forever, which is the polling this replaced.
    rescan: bool,
    /// The scan generation this last asked at, so a burst of new aliases
    /// costs one walk rather than one each. See [`Captures::ask_for_scan`].
    asked_at: Option<u64>,
    /// The longest a single refresh has taken. Held so a regression says so
    /// in the log rather than turning up as a stutter someone has to notice.
    worst: std::time::Duration,
    /// What the pass in progress has spent *resolving*, and how many aliases
    /// that was.
    ///
    /// Reported alongside the pass total because the two answers look
    /// identical in a bare duration and are not the same bug: a pass that is
    /// slow because it captured a dozen items is doing asked-for work, while
    /// a pass that is slow with *one* capture is blocked inside the window
    /// server -- which is exactly what a missing Screen Recording grant looks
    /// like from here, and it blocks for thirty seconds before failing.
    resolving: (std::time::Duration, u32),
    /// Whether the last [`Captures::begin_pass`] found a reason every alias
    /// should be looked at, rather than only the ones a notification named.
    full: bool,
}

/// What each alias item is mirroring, and the last thing it captured.
struct Mirror {
    alias: Alias,
    /// The spec it was built from, so a changed spec rebuilds it.
    spec: String,
    captured: Option<Captured>,
}

/// Splits an `Owner,Name` spec. The name may itself contain commas, so only
/// the first one separates.
fn split_spec(spec: &str) -> Option<(&str, &str)> {
    let (owner, name) = spec.split_once(',')?;
    let (owner, name) = (owner.trim(), name.trim());
    (!owner.is_empty() && !name.is_empty()).then_some((owner, name))
}

/// The byte offset of alpha within one pixel, for the pixel formats this
/// function actually recognizes — 32 bits per pixel, 8 bits per component,
/// premultiplied or plain alpha either first or last. `None` for anything
/// else (including no alpha channel at all, or an alpha-only image), so a
/// caller falls back to the untrimmed rect rather than guess at a layout it
/// cannot confirm.
///
/// Measured live on this machine: the window server's own screen capture
/// (`Window::capture`) comes back `PremultipliedFirst` + `Order32Little`,
/// which puts alpha in the *last* byte of each 4-byte pixel — conceptually
/// "alpha first" in the 32-bit word, reversed by little-endian storage.
fn alpha_byte_offset(image: &CGImage) -> Option<usize> {
    if CGImage::bits_per_pixel(Some(image)) != 32 || CGImage::bits_per_component(Some(image)) != 8 {
        return None;
    }
    let little = CGImage::byte_order_info(Some(image)) == CGImageByteOrderInfo::Order32Little;
    match CGImage::alpha_info(Some(image)) {
        CGImageAlphaInfo::PremultipliedFirst | CGImageAlphaInfo::First => {
            Some(if little { 3 } else { 0 })
        }
        CGImageAlphaInfo::PremultipliedLast | CGImageAlphaInfo::Last => {
            Some(if little { 0 } else { 3 })
        }
        _ => None,
    }
}

/// What one pass over a capture's raw pixels measures.
struct Analysis {
    digest: u64,
    /// The alpha bounding box, in device pixels, top-left-relative to the
    /// image. `None` when every pixel was fully transparent, or the pixel
    /// format wasn't one [`alpha_byte_offset`] recognizes.
    trim_px: Option<CGRect>,
}

/// Hashes a capture's pixels and finds its alpha bounding box in the same
/// pass — the hash already has to walk every byte, so measuring the ink
/// alongside it costs one extra comparison per pixel rather than a second
/// full traversal of the buffer.
fn analyze(image: &CGImage) -> Analysis {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();

    let width = CGImage::width(Some(image));
    let height = CGImage::height(Some(image));
    let bytes_per_row = CGImage::bytes_per_row(Some(image));
    let bytes_per_pixel = CGImage::bits_per_pixel(Some(image)) / 8;
    let alpha_offset = alpha_byte_offset(image);

    let (mut min_x, mut max_x, mut min_y, mut max_y) = (usize::MAX, 0, usize::MAX, 0);

    // The pixels, not the object: two captures of an item that has not
    // changed are different `CGImage`s with identical contents.
    if let Some(data) = CGDataProvider::data(CGImage::data_provider(Some(image)).as_deref()) {
        // SAFETY: a freshly copied `CFData` nothing else holds, so nothing can
        // mutate it while the slice is alive.
        let bytes = unsafe { data.as_bytes_unchecked() };
        for row in 0..height {
            let start = row * bytes_per_row;
            let Some(row_bytes) = bytes.get(start..(start + bytes_per_row).min(bytes.len())) else {
                break;
            };
            row_bytes.hash(&mut hasher);

            if let Some(offset) = alpha_offset
                && bytes_per_pixel > 0
            {
                for col in 0..width {
                    let at = col * bytes_per_pixel + offset;
                    if row_bytes.get(at).copied().unwrap_or(0) != 0 {
                        min_x = min_x.min(col);
                        max_x = max_x.max(col);
                        min_y = min_y.min(row);
                        max_y = max_y.max(row);
                    }
                }
            }
        }
    }
    width.hash(&mut hasher);
    height.hash(&mut hasher);

    // A menu bar item is at most a few hundred pixels across — nowhere near
    // f64's exact-integer range — so these are lossless in practice.
    #[allow(clippy::cast_precision_loss)]
    let trim_px = (min_x <= max_x && min_y <= max_y).then(|| {
        CGRect::new(
            CGPoint::new(min_x as f64, min_y as f64),
            CGSize::new((max_x + 1 - min_x) as f64, (max_y + 1 - min_y) as f64),
        )
    });

    Analysis {
        digest: hasher.finish(),
        trim_px,
    }
}

/// Converts an alpha bounding box measured in device pixels (see
/// [`analyze`]) into the point space `size` is already in, falling back to
/// the full, untrimmed rect when there is nothing to trim to — a fully
/// transparent capture, or an unrecognized pixel format. Either is a
/// legitimate state, not a zero-width item that should vanish from the bar.
fn trim_rect(image: &CGImage, size: CGSize, trim_px: Option<CGRect>) -> CGRect {
    let full = CGRect::new(CGPoint::ZERO, size);
    let Some(trim_px) = trim_px else {
        return full;
    };
    let width_px = CGImage::width(Some(image));
    let height_px = CGImage::height(Some(image));
    if width_px == 0 || height_px == 0 {
        return full;
    }
    // Same lossless-in-practice cast as `analyze`'s pixel bounds above.
    #[allow(clippy::cast_precision_loss)]
    let (scale_x, scale_y) = (size.width / width_px as f64, size.height / height_px as f64);
    CGRect::new(
        CGPoint::new(trim_px.origin.x * scale_x, trim_px.origin.y * scale_y),
        CGSize::new(trim_px.size.width * scale_x, trim_px.size.height * scale_y),
    )
}

/// A `Captures` whose scan wakes nothing, for the in-crate tests.
///
/// Only there: production goes through [`Captures::new`], because a finished
/// scan that never wakes the run loop is a scan whose answer is never
/// applied.
#[cfg(test)]
impl Default for Captures {
    fn default() -> Self {
        // A test thread has a run loop even though nothing pumps it.
        Self::new(crate::runloop::Waker::install(
            crate::runloop::main_thread(),
            || {},
        ))
    }
}

impl Captures {
    /// The waker is how a finished Accessibility scan gets back onto the main
    /// thread: the workers store their answer and signal, and the pass that
    /// wakes applies it.
    #[must_use]
    pub fn new(wake: crate::runloop::Waker) -> Self {
        Self {
            mirrors: std::collections::HashMap::new(),
            watch: crate::alias_watch::AxWatch::new(wake.clone()),
            scan: extras::Scanner::new(wake.clone()),
            captor: Captor::new(wake.clone()),
            lister: Lister::new(wake),
            // Empty until the first repair lands -- every alias misses until
            // then, which is exactly what `full: true` below already asks
            // for.
            table: Arc::new(RwLock::new(Snapshot {
                windows: Vec::new(),
            })),
            note_rescan_owed: false,
            // Nothing to scan for yet -- the first alias to arrive asks.
            rescan: false,
            asked_at: None,
            worst: std::time::Duration::ZERO,
            resolving: (std::time::Duration::ZERO, 0),
            // The first pass has to look at everything; nothing has been
            // captured yet.
            full: true,
        }
    }

    /// Something happened that could have changed who owns a status item.
    ///
    /// The causes, and there are only these: an application launching, the
    /// Accessibility grant turning up, and a newly-added alias failing its
    /// very first resolve. Each is an edge. The last is *not* "we looked and
    /// did not find it" — that would ask forever for an application that is
    /// simply not running; it is "this alias has never looked before", which
    /// happens once per alias.
    pub fn rescan(&mut self) {
        self.rescan = true;
    }

    /// The window server has moved the menu bar layer under us.
    ///
    /// A display arriving, leaving or changing resolution repositions every
    /// window in that layer, so every bound the table is holding is stale --
    /// and the table is kept between passes now, so no individual alias would
    /// notice for itself. This is the one repair edge nobody else can raise.
    ///
    /// It asks [`Lister`] directly rather than going through
    /// [`Captures::rescan`]: what a display move changes is where windows are,
    /// never who owns them, and `rescan` additionally means "an alias that has
    /// never resolved might now", which is a different claim. Note that the
    /// owner scan is not thereby avoided -- [`Captures::begin_pass`] owes one
    /// after *any* landed repair, since a repaired table can attribute a
    /// Control Centre item that was unattributed before -- it is only that
    /// this is not the thing asking for it. `ask_for_scan`'s
    /// `has_unattributed` guard is what keeps that free on a bar whose aliases
    /// all resolve.
    pub fn note_reconfigured(&self) {
        self.lister.request();
    }

    /// Whether a pass would find anything to do -- the run condition's
    /// question.
    ///
    /// Five cheap reads: a cause recorded, a finished owner scan, a finished
    /// capture, a repaired table, a notification matched. An alias that is up
    /// to date and unchanged makes this `false`, which is the point -- and a
    /// repair landing has to be one of them, or a table repaired on nobody's
    /// behalf would sit uncollected until some alias came due for its own
    /// reasons.
    #[must_use]
    pub fn pending(&self) -> bool {
        self.rescan
            || self.scan.ready()
            || self.captor.ready()
            || self.lister.ready()
            || self.watch.any_dirty()
    }

    /// The one look at the menu bar layer this pass gets, collecting a
    /// finished Accessibility scan first if one landed.
    ///
    /// Asking for a scan happens here and only here, and needs two things at
    /// once: a cause recorded by [`Captures::rescan`], and something in the
    /// list a scan could actually improve on. A bar whose aliases all resolve
    /// cleanly never asks.
    ///
    /// Starts a pass: collects a finished owner scan, collects a finished
    /// window list repair into [`Captures::table`], and decides whether this
    /// is a pass that looks at every alias.
    pub fn begin_pass(&mut self) {
        // Both of these change what *every* alias would resolve to: a fresh
        // owner scan can move any Control Centre item to its real owner, and
        // the events behind `rescan` -- an application launching, the grant
        // arriving -- are the ones that can make an alias that never resolved
        // resolve.
        self.resolving = (std::time::Duration::ZERO, 0);
        self.full = self.scan.take() | self.rescan;
        // Drained every pass, not only a pass that ends up using it -- see
        // [`Lister::take`].
        if let Some(fetched) = self.lister.take() {
            match fetched {
                Ok(raw) => {
                    *self
                        .table
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Snapshot::finish(raw);
                    // A repaired table can resolve any alias that was
                    // missing from the old one, same reasoning as a finished
                    // owner scan above.
                    self.full = true;
                    self.note_rescan_owed = true;
                }
                Err(err) => {
                    // A permission problem, most likely -- the table is left
                    // as it was rather than emptied, since stale-but-present
                    // beats losing every alias already resolved.
                    tracing::debug!(%err, "could not repair the menu bar layer");
                }
            }
        }
        let rescan = std::mem::take(&mut self.rescan);
        self.note_rescan_owed |= rescan;
        if rescan {
            self.lister.request();
        }
    }

    /// A handle on the menu bar layer as of its last repair.
    ///
    /// Shared, not copied: every alias resolves against the same
    /// `Arc<RwLock<Snapshot>>`, so a read here costs a lock rather than a
    /// window server round trip. The clone is cheap (an `Arc` bump); hold the
    /// read guard only as long as the pass needs it.
    ///
    /// Four edges ask [`Lister`] to repair it -- see [`Captures::refresh`],
    /// [`Captures::begin_pass`] and [`Captures::note_reconfigured`] for where
    /// each fires: an alias resolving for the first time and missing, an alias
    /// whose cached window vanished (`still_is` turned false), an explicit
    /// rescan, and a display change. The repair lands on a later pass, through
    /// `begin_pass`, which is what wakes the run loop to collect it.
    #[must_use]
    pub fn table(&self) -> Arc<RwLock<Snapshot>> {
        Arc::clone(&self.table)
    }

    /// Asks for an owner scan on nobody's behalf, after a launch or a grant
    /// -- once a pass actually has a snapshot to check `has_unattributed`
    /// against, which may be later than the pass that raised the cause.
    #[skylight::main_thread]
    pub fn note_rescan(&mut self, snapshot: &Snapshot) {
        if std::mem::take(&mut self.note_rescan_owed) {
            report_grant_changes();
            self.ask_for_scan(proof, "", snapshot);
        }
    }

    /// Starts an owner scan, if there is anything one could improve on.
    ///
    /// The guard is the point: a list with no Control Centre-owned window
    /// left in it has nothing an Accessibility walk could recover, so a bar
    /// whose aliases all resolve cleanly never walks at all.
    #[skylight::main_thread(pass)]
    fn ask_for_scan(&mut self, owner: &str, snapshot: &Snapshot) {
        if !snapshot.has_unattributed() {
            return;
        }
        // A walk only ever moves a window *away* from Control Centre and onto
        // the application that really owns it. So an alias that already names
        // Control Centre and still did not resolve cannot be helped by one --
        // a scan can only make its owner rarer, never commoner -- and asking
        // is a walk of every running application for an answer that is already
        // known not to be there. Two thirds of a config's aliases name Control
        // Centre directly; measured on a 75-alias config, this is most of the
        // asks.
        if is_control_center_owner(owner) {
            return;
        }
        // One walk per burst, not one per alias. `Scanner::request` only
        // suppresses an ask while a scan is *running*, so seventy-five aliases
        // arriving over the 250 ms a config takes to load used to start a
        // fresh walk of all 123 running applications every time one landed --
        // measured at 38 of them, some 4,700 cross-process Accessibility
        // calls, for an answer that does not change between two of them.
        //
        // The generation is what tells "nothing has answered me yet" from "one
        // has, and it did not resolve this alias". Asking again is worth it
        // only in the second case: a walk that has *completed* since the last
        // ask may have found an owner this alias did not exist to benefit
        // from.
        let generation = self.scan.generation();
        if self.asked_at == Some(generation) {
            return;
        }
        // Stamped only once a scan actually starts for this generation --
        // not merely once this function is *called* for it. Recording the
        // attempt regardless used to let a missing grant, or losing the race
        // with `Scanner::request`'s own `running` flag, freeze `asked_at` on
        // a generation no scan would ever complete: since `generation` only
        // moves when a scan finishes, nothing could ever ask again for the
        // rest of the process's life -- including the ask this same function
        // exists to answer when the Accessibility grant arrives after an
        // earlier, unsuccessful attempt.
        match skylight::ax::Trusted::now() {
            // `request` no longer takes the marker: `Scanner::request` is
            // annotated, so the attribute supplies it. Its `bool` says
            // whether this call actually started a scan (`false` means one
            // was already out), and only that is worth remembering.
            Some(access) => {
                if self.scan.request(mtm, access) {
                    self.asked_at = Some(generation);
                }
            }
            // The one place a missing grant is decided about: prompt at most
            // once, and otherwise keep the owner the window server reported,
            // which is what a pre-macOS-26 machine reports anyway. Not
            // stamped: no scan started, so this generation must stay askable.
            _ => TRUST.with_borrow_mut(ax::Trust::prompt),
        }
    }

    /// Records how long one whole refresh took, reporting each new worst.
    ///
    /// The budget is a frame: this runs on the thread that composites the
    /// bar, so anything longer is a dropped frame by definition. Reported at
    /// `warn` only when it is both a new worst *and* over budget, so a
    /// healthy bar says nothing and a regression says it once.
    pub fn note_pass(&mut self, elapsed: std::time::Duration) {
        let worst = elapsed > self.worst;
        self.worst = self.worst.max(elapsed);
        let (resolving, resolved) = self.resolving;
        if elapsed > FRAME {
            // Every overrun, not only a new worst: a bar that drops frames
            // steadily and a bar that dropped one at start-up look identical
            // otherwise, and they are not the same bug. The capture split is
            // there so a reader does not have to guess which of those, or a
            // third, it was. `resolving` is the *resolution* half only --
            // the pictures are taken on workers now, so a pass that overran
            // with a tiny `resolving` overran on something else entirely.
            tracing::warn!(
                ?elapsed,
                ?resolving,
                resolved,
                worst = ?self.worst,
                "an alias refresh overran a frame"
            );
        } else if worst {
            tracing::debug!(
                ?elapsed,
                ?resolving,
                resolved,
                "the longest alias refresh so far"
            );
        }
    }

    /// Which aliases this pass has any reason to look at.
    ///
    /// [`Due::Everything`] only when something changed what an alias would
    /// resolve to at all. Otherwise the answer is the entities a notification
    /// actually named — one status item's clock ticking over must cost one
    /// capture, not one per alias in the config, which is what walking the
    /// whole query and asking each "was it you?" amounted to.
    ///
    /// Consumes both, so a cause is acted on once. Call it after
    /// [`Captures::collect`], which is where a finished capture is installed,
    /// and after [`Captures::begin_pass`], which is where a finished owner
    /// scan or window list repair is collected and is therefore where one of
    /// `Due::Everything`'s two causes is discovered.
    pub fn due(&mut self) -> Due {
        if std::mem::take(&mut self.full) {
            Due::Everything
        } else {
            Due::These(self.watch.take_dirty_set())
        }
    }

    /// Re-captures one item, reporting its digest if what it draws changed.
    ///
    /// `None` means nothing to redraw -- the capture failed, it came back
    /// identical, or nothing has said this item changed since it was last
    /// read. `look` says why, and the two reasons cost very different things
    /// — see [`Look`]. An alias that has never captured is read whichever it
    /// is: there is nothing being kept for it.
    #[skylight::main_thread]
    pub fn refresh(
        &mut self,
        entity: bevy_ecs::entity::Entity,
        spec: &str,
        snapshot: &Snapshot,
        look: Look,
    ) -> Option<u64> {
        let Some((owner, name)) = split_spec(spec) else {
            tracing::warn!(spec, "an alias is written `Owner,Name`");
            return None;
        };

        let mut created = false;
        let mirror = match self.mirrors.get_mut(&entity) {
            Some(mirror) if mirror.spec == spec => mirror,
            _ => {
                self.watch.forget(entity);
                created = true;
                self.mirrors
                    .entry(entity)
                    .insert_entry(Mirror {
                        alias: Alias::new(owner, name),
                        spec: spec.to_owned(),
                        captured: None,
                    })
                    .into_mut()
            }
        };

        // Watched, not polled. A mirrored item is re-read when its owner
        // says something changed, and the first time it is seen -- never on a
        // timer. Capturing a window and hashing every pixel of it twice a
        // second, forever, to learn that a static icon is still static, was
        // the largest single cost in this process.
        let first = mirror.captured.is_none();
        if !first && look == Look::Resolve && mirror.alias.settled(snapshot) {
            self.watch.sync_one(
                entity,
                mirror.alias.name(),
                mirror.alias.pid(),
                mirror.alias.bounds(),
            );
            return None;
        }
        if !first {
            tracing::debug!(spec, ?look, "this alias has a reason to be re-read");
        }

        // Whether the table had ever found this one, checked before
        // `resolve` may invalidate it -- an alias that had a window and lost
        // it is as much a reason to repair the table as one that never found
        // one, and `resolve` does not report which happened.
        let had_window = mirror.alias.window_id().is_some();
        let began = std::time::Instant::now();
        // Resolution only. The window server round trip, the pixel hash and
        // the trim all happen on a worker -- see `Captor` -- so what this
        // costs the compositing thread is a hash lookup in [`Captures::table`]
        // as it was at its last repair.
        let resolved = mirror.alias.resolve(snapshot);
        self.resolving.0 += began.elapsed();
        self.resolving.1 += 1;
        let Some(window) = resolved else {
            // An alias whose application is not running is an ordinary state,
            // and it is waited for rather than retried: the launch
            // notification is what brings it back.
            //
            // What was actually on offer, not just that nothing matched: a
            // spec naming a real item under a slightly different spelling and
            // one whose application is absent are otherwise the same silence,
            // and they want opposite responses from whoever reads this.
            tracing::debug!(
                spec,
                available = ?snapshot
                    .items()
                    .map(|(owner, name)| format!("{owner},{name}"))
                    .collect::<Vec<_>>(),
                "could not resolve the menu bar item"
            );
            // The one place an unresolved alias asks for a scan, and it is
            // still an edge, not a poll: only the pass that *created* this
            // mirror may ask. An alias that resolves without one never
            // triggers a walk, and one that does not resolve asks exactly
            // once -- after that it waits for an event.
            if created {
                self.ask_for_scan(proof, owner, snapshot);
            }
            // Two edges ask the table to repair, and only these: a first
            // resolve missing (`created`), and a cached window that vanished
            // (`had_window`). An alias that looked again and still is not
            // there does not -- that would be the polling this replaced, and
            // `Lister::request` is idempotent besides, so a burst of either
            // costs one walk.
            if created || had_window {
                self.lister.request();
            }
            return None;
        };

        self.watch.sync_one(
            entity,
            mirror.alias.name(),
            mirror.alias.pid(),
            mirror.alias.bounds(),
        );
        self.captor.submit(entity, spec, window);
        // Nothing to report yet. The pixels arrive on a later pass, through
        // `collect`, which is what wakes the run loop to run it.
        None
    }

    /// Installs every capture that has landed, answering which entities now
    /// hold different pixels.
    ///
    /// The digest comparison stays here rather than on the worker: it is the
    /// question "did what is *drawn* change", and only this side knows what is
    /// drawn. A capture that hashes the same as the one already held is
    /// dropped without a repaint, which is what stops a status item redrawing
    /// itself for every notification its application happens to send.
    pub fn collect(&mut self) -> std::collections::HashMap<bevy_ecs::entity::Entity, u64> {
        let mut changed = std::collections::HashMap::new();
        for landed in self.captor.take() {
            let Some(mirror) = self.mirrors.get_mut(&landed.entity) else {
                // Despawned, or no longer mirrored, while this was out.
                continue;
            };
            if mirror.spec != landed.spec || mirror.alias.window_id() != Some(landed.window) {
                // Re-specced or re-resolved meanwhile: these are some other
                // item's pixels now.
                continue;
            }
            if mirror
                .captured
                .as_ref()
                .is_some_and(|held| held.digest == landed.captured.digest)
            {
                continue;
            }
            changed.insert(landed.entity, landed.captured.digest);
            mirror.captured = Some(landed.captured);
        }
        changed
    }

    #[must_use]
    pub fn get(&self, entity: bevy_ecs::entity::Entity) -> Option<&Captured> {
        self.mirrors.get(&entity)?.captured.as_ref()
    }

    /// Whether this entity is currently being mirrored at all.
    #[must_use]
    pub fn holds(&self, entity: bevy_ecs::entity::Entity) -> bool {
        self.mirrors.contains_key(&entity)
    }

    /// Stops mirroring one item, answering whether there was anything to
    /// stop.
    ///
    /// Everything goes: the held pixels, the dirty subscription, and — when
    /// this was the last entity watching its owning application — that
    /// application's `AXObserver`, since [`crate::alias_watch::AxWatch`]
    /// drops the observer with the last watcher rather than keeping it for a
    /// caller that may never come back.
    pub fn forget(&mut self, entity: bevy_ecs::entity::Entity) -> bool {
        self.watch.forget(entity);
        self.mirrors.remove(&entity).is_some()
    }
}

#[cfg(test)]
mod send_tests {
    //! What may cross a thread, checked by the compiler rather than claimed.

    use super::{Captured, InFlight, SendImage};

    const fn assert_send<T: Send>() {}

    #[test]
    fn a_capture_may_be_moved_to_the_thread_that_draws_it() {
        // `InFlight` is what a worker hands back, so it has to be `Send` --
        // but only because every field is, and the image only because
        // `SendImage` carries the one `unsafe impl` in the module. If someone
        // adds a field that is not `Send`, this stops compiling, which is the
        // whole reason the assertion lives on the image and not on the struct
        // around it.
        assert_send::<SendImage>();
        assert_send::<Captured>();
        assert_send::<InFlight>();
    }
}

#[cfg(test)]
mod table_tests {
    //! What a landed table repair must trigger, simulated by filling in what
    //! `Lister::request`'s worker would have. The two repair-edge tests below
    //! are the exception to that: asking an idle `Lister` for a repair really
    //! does start one, so `a_display_change_asks_for_a_repair` puts a real
    //! window list walk on the pool -- read-only, and nothing waits on it.

    use super::{Captures, Due, RawWindow};
    use std::sync::atomic::Ordering;

    #[test]
    fn a_landed_repair_makes_every_alias_due_and_owes_a_rescan_check() {
        let mut captures = Captures::default();
        // `Captures::new` starts with `full: true` -- there is nothing
        // captured yet either way -- so drain that one before asserting
        // anything about a repair landing.
        captures.due();
        captures.begin_pass();
        assert!(
            !matches!(captures.due(), Due::Everything),
            "nothing landed yet"
        );

        // What `begin_pass` collects, without a real worker behind it.
        *captures.lister.finished.lock().expect("not poisoned") = Some(Ok(Vec::<RawWindow>::new()));
        captures.lister.landed.store(true, Ordering::Release);

        captures.begin_pass();
        assert!(
            captures.note_rescan_owed,
            "a repaired table can resolve a Control Centre item that was \
             unattributed before, same as a finished owner scan"
        );
        match captures.due() {
            Due::Everything => {}
            Due::These(_) => panic!("a landed repair should make every alias due"),
        }
    }

    #[test]
    fn a_display_change_asks_for_a_repair() {
        let captures = Captures::default();
        assert!(
            !captures.lister.running.load(Ordering::Acquire),
            "nothing asked yet"
        );
        captures.note_reconfigured();
        assert!(
            captures.lister.running.load(Ordering::Acquire),
            "a display moving every window in the layer is what the kept table \
             cannot notice on its own"
        );
    }

    #[test]
    fn a_burst_of_display_changes_costs_one_round_trip() {
        let captures = Captures::default();
        // A repair already out. Three displays reconfiguring at once is one
        // event each, and none of them should start a second walk.
        captures.lister.running.store(true, Ordering::Release);

        captures.note_reconfigured();
        captures.note_reconfigured();
        captures.note_reconfigured();

        assert!(
            captures
                .lister
                .finished
                .lock()
                .expect("not poisoned")
                .is_none(),
            "a repair already in flight should not have started another"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{RawWindow, Snapshot, attribute_owners, disambiguate_duplicates};
    use crate::extras::ExtrasMenuItem;
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    fn rect(x: f64, width: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, -30.0), CGSize::new(width, 30.0))
    }

    /// What the window server reports on this machine: several unrelated
    /// modules all blamed on Control Centre, one of which really belongs to
    /// another process.
    fn windows() -> Vec<RawWindow> {
        vec![
            RawWindow {
                id: 63,
                owner: "Control Centre".to_owned(),
                name: "Clock".to_owned(),
                pid: 762,
                bounds: rect(1211.0, 143.0),
            },
            RawWindow {
                id: 107,
                owner: "Control Centre".to_owned(),
                name: "Fantastical".to_owned(),
                pid: 762,
                bounds: rect(1019.0, 38.0),
            },
            RawWindow {
                id: 219,
                owner: "Cotabby".to_owned(),
                name: "Item-0".to_owned(),
                pid: 1866,
                bounds: rect(657.0, 51.0),
            },
        ]
    }

    /// One real, separately-owned item published through its own
    /// `AXExtrasMenuBar`, positioned where the window server put its window.
    fn extras() -> Vec<ExtrasMenuItem> {
        vec![ExtrasMenuItem {
            owner: "Fantastical Helper".to_owned(),
            title: "Fantastical".to_owned(),
            pid: 1632,
            frame: rect(1019.0, 38.0),
        }]
    }

    #[test]
    fn without_a_scan_control_centre_keeps_the_window_servers_owner() {
        let mut windows = windows();
        attribute_owners(&mut windows, None);
        assert_eq!(windows[1].owner, "Control Centre");
        assert_eq!(windows[1].pid, 762);
        // And nothing else is disturbed by the absence.
        assert_eq!(windows[0].owner, "Control Centre");
        assert_eq!(windows[2].owner, "Cotabby");
    }

    #[test]
    fn a_scan_recovers_the_real_owner() {
        let mut windows = windows();
        attribute_owners(&mut windows, Some(&extras()));
        assert_eq!(windows[1].owner, "Fantastical Helper");
        assert_eq!(windows[1].pid, 1632);
        // Control Centre really does own the clock; nothing invents an owner
        // for it, and a window it never owned is left alone.
        assert_eq!(windows[0].owner, "Control Centre");
        assert_eq!(windows[2].owner, "Cotabby");
        assert_eq!(windows[2].pid, 1866);
    }

    #[test]
    fn duplicates_are_numbered_left_to_right() {
        let mut windows = vec![
            RawWindow {
                id: 2,
                owner: "Control Centre".to_owned(),
                name: "Item-0".to_owned(),
                pid: 762,
                bounds: rect(900.0, 30.0),
            },
            RawWindow {
                id: 1,
                owner: "Control Centre".to_owned(),
                name: "Item-0".to_owned(),
                pid: 762,
                bounds: rect(700.0, 30.0),
            },
            RawWindow {
                id: 3,
                owner: "Cotabby".to_owned(),
                name: "Item-0".to_owned(),
                pid: 1866,
                bounds: rect(650.0, 30.0),
            },
        ];
        disambiguate_duplicates(&mut windows);
        assert_eq!(windows[1].name, "Item-0(1)");
        assert_eq!(windows[0].name, "Item-0(2)");
        // A different owner is a different group, so its single item keeps
        // its plain name.
        assert_eq!(windows[2].name, "Item-0");
    }

    #[test]
    fn a_rebuilt_item_under_the_same_id_is_not_still_the_same_item() {
        let snapshot = Snapshot { windows: windows() };
        assert!(snapshot.still_is(63, "Control Centre", "Clock"));
        // The window server reuses ids; the item behind one can be torn down
        // and rebuilt with no error anywhere.
        assert!(!snapshot.still_is(63, "Control Centre", "Sound"));
        assert!(!snapshot.still_is(999, "Control Centre", "Clock"));
    }

    #[test]
    fn only_an_unattributed_list_is_worth_scanning_for() {
        let mut resolved = windows();
        attribute_owners(&mut resolved, Some(&extras()));
        assert!(Snapshot { windows: resolved }.has_unattributed());

        let none = vec![RawWindow {
            id: 219,
            owner: "Cotabby".to_owned(),
            name: "Item-0".to_owned(),
            pid: 1866,
            bounds: rect(657.0, 51.0),
        }];
        assert!(!Snapshot { windows: none }.has_unattributed());
    }
}

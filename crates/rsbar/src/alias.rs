//! Alias items: mirroring another application's menu bar item into our bar.
//!
//! The window server keeps every menu bar extra as an ordinary window at
//! [`MENU_BAR_LAYER`], owned (per `kCGWindowOwnerPID`) by the process that
//! drew it. Historically that made aliasing straightforward: list the
//! layer-25 windows, match one by owner and title, and capture its window ID.
//!
//! On macOS 26 that ownership link partly breaks. Many menu bar extras that
//! used to belong to their own process (Sound, Clock, Focus, Bluetooth's
//! "`BentoBox`" module, ...) are now drawn directly by **Control Center**, so
//! `kCGWindowOwnerPID`/`kCGWindowOwnerName` both say "Control Center" no
//! matter which module it is. `kCGWindowName` is *not* generally empty —
//! measured on this machine (macOS 26.5.1) it is often a real title
//! ("Sound", "Clock", "`FocusModes`"), but several unrelated modules also share
//! the literal placeholder name `Item-0`, which by itself names nothing in
//! particular. The window is still capturable either way — window server
//! geometry does not care who owns a window — but a bare `Item-0` cannot be
//! told apart from the four other windows also called `Item-0`.
//!
//! Two fixes apply, both cross-checked against `SketchyBar`:
//! - **Recovering a real owner**, from `source_pid.m`: every running
//!   application exposes its status items through `AXExtrasMenuBar`, titled
//!   and positioned in screen coordinates, whether or not that application
//!   ends up compositing its own window. So when a window's owner looks like
//!   Control Center, this module walks every running application's extras
//!   menu bar and matches the window's bounds against an AX element's frame.
//!   A match within a few points recovers the item's real owner and name; on
//!   this machine it did not fire for any of the genuinely Control
//!   Center-native modules (Sound, Clock, `FocusModes`, `BentoBox`), because
//!   there is no other process to recover — Control Center really is the
//!   owner now. Nothing papers over that: an unresolved item is reported
//!   under the owner "Control Center", not silently dropped.
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
//! # Push updates: one route delivers, scoped and filtered
//!
//! [`Captures::refresh`] is still called on a timer by `ecs::refresh_aliases`
//! — nothing here replaces the poll — but it is no longer the only way a
//! capture gets triggered. Two routes were checked against real, live
//! changes rather than trusted on registration succeeding
//! (`skylight/examples/notify_probe.rs` and
//! `rsbar/examples/ax_observer_probe.rs` are the evidence, cross-checked live
//! on macOS 26.5.1):
//!
//! - **`SkyLight`/CGS window notifications**, the mechanism `sources/spaces.rs`
//!   uses for space events: registering every plausibly-relevant `kCGSEvent*`
//!   this investigation could find — window move/resize/reorder/visibility,
//!   dirty-screen, connection visibility, title-changed, menu bar
//!   creation/style, space-window-transaction — via both
//!   `SLSRegisterConnectionNotifyProc` and the connection-less
//!   `SLSRegisterNotifyProc`, plus `SLSRequestNotificationsForWindows` for the
//!   target window, always reports success, but nothing ever arrives — not
//!   for the menu bar clock's minute rollover, not for a real foreign window
//!   (Finder) resized six times live, not even for a window this process
//!   owns and moves/orders itself. Cross-checking upstream `SketchyBar`'s own
//!   `sketchybar.c`/`window.c` settles it further: the only content-adjacent
//!   event it registers, `kCGSWindowTitleChanged` (1322), is used purely to
//!   *pause* its capture poll for ~1s during a transition
//!   (`g_disable_capture`) — never to trigger one. This route stays
//!   unused: it looks non-functional on this machine, matching
//!   `sources/spaces.rs`'s own note that its space-change event was never
//!   observed firing live either.
//! - **`AXObserver`**, at two different scopes:
//!   - On the item's own `AXUIElement`: rejected immediately with
//!     `kAXErrorNotificationUnsupported`, for `kAXValueChangedNotification`
//!     and `kAXTitleChangedNotification` alike, on both a Control Center-native
//!     item (Clock) and a genuinely separate-process one (Fantastical). The
//!     `AXMenuBarItem` role does not participate in AX's notification system
//!     at all — confirmed as a real rejection, not silence. This scope stays
//!     unused.
//!   - On the *owning application's* top-level `AXUIElement`: accepted, and
//!     it fires. `examples/ax_observer_probe.rs`, watched live across a
//!     minute rollover, caught the Control Centre clock's own change —
//!     `AXTitleChanged`/`AXValueChanged`, `owner_pid` matching Control
//!     Centre's, `AXDescription` reading `"Clock"`, `AXValue` reading the new
//!     time. Nothing about the item itself is scoped this broad, though:
//!     watching an application's element also reports notifications from
//!     anything else mounted under that pid — the same run confirmed an
//!     unrelated app's still-visible popover text firing `AXValueChanged`
//!     once a second. [`crate::alias_watch`] is what turns that into a usable
//!     signal rather than a firehose: one observer per pid that an aliased
//!     item currently resolves to, and every notification checked against
//!     both the notified element's own pid (`AXUIElementGetPid`, which is
//!     what actually rules the unrelated app's traffic out — it does not
//!     share the aliased item's pid) and its `AXDescription`/`AXTitle`
//!     against the alias's name, exactly. A miss is silently left to the
//!     poll; see [`crate::alias_watch`]'s module doc for the full mechanism and its
//!     honest limits (a disambiguated `(n)` name can never match, for one).
//!
//! [`STALE_AFTER`] is what makes leaning on either of these safe: a poll that
//! also double-checks the window is still the right one whenever it looks
//! suspiciously unchanging, closing the gap a plain capture-and-hash loop —
//! or a plugged-in push notification whose scope has already turned out to
//! carry noise — cannot see on its own (see its doc comment).
//!
//! # The left-hand items are not windows
//!
//! `list_menu_bar_items` only finds items at [`MENU_BAR_LAYER`], which is
//! every right-hand extra and nothing on the left. Widening the filter finds
//! nothing more: the Apple menu and an application's own titles are painted
//! into one shared window-server surface and have no window of their own. See
//! [`crate::menus`], which lists and presses them instead.

use objc2_application_services::{AXError, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{
    CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType, CGPoint, CGRect, CGSize, Type,
};
use objc2_core_graphics::{
    CGDataProvider, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo,
    CGPreflightScreenCaptureAccess, CGRectMakeWithDictionaryRepresentation,
    CGRequestScreenCaptureAccess, CGWindowListCopyWindowInfo, CGWindowListOption, kCGWindowBounds,
    kCGWindowLayer, kCGWindowName, kCGWindowNumber, kCGWindowOwnerName, kCGWindowOwnerPID,
};
use skylight::{Window, WindowId};
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;

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
/// Needed only for the macOS 26 Control Center workaround — a menu bar item
/// still owned by its own process aliases fine without this.
#[must_use]
pub fn accessibility_trusted() -> bool {
    // SAFETY: no arguments.
    unsafe { objc2_application_services::AXIsProcessTrusted() }
}

/// Prompts the user for Accessibility access if it is not already granted.
#[must_use]
pub fn request_accessibility() -> bool {
    // SAFETY: reads a `'static` extern constant.
    let key = unsafe { objc2_application_services::kAXTrustedCheckOptionPrompt };
    let true_value = objc2_core_foundation::CFBoolean::new(true);
    let dict = CFDictionary::from_slices(&[key], &[true_value]);
    // SAFETY: `dict` maps the documented prompt key to a `CFBoolean`, exactly
    // as `AXIsProcessTrustedWithOptions` expects.
    unsafe { objc2_application_services::AXIsProcessTrustedWithOptions(Some(dict.as_opaque())) }
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
/// listed under the owner "Control Center" — the window is real and
/// capturable even when nothing identifies who really draws it (see the
/// module docs). What such items commonly lack is a *unique* name: several
/// unrelated Control Center modules currently share the placeholder name
/// `Item-0`. Rather than hide that, every name repeated within this listing
/// gets a `(n)` suffix appended, 1-based in left-to-right order among just
/// that duplicate group, cross-checked against upstream `SketchyBar`'s own
/// fix for this (`rework alias logic to handle duplicate entries`). Use the
/// suffixed form verbatim when constructing an [`Alias`] for such an item.
///
/// # Errors
///
/// Returns [`Error::NoWindowList`] if the window server refuses the list,
/// which is what a missing Screen Recording grant looks like from here.
pub fn list_menu_bar_items() -> Result<Vec<MenuBarItem>> {
    let mut windows = raw_menu_bar_windows()?;
    windows.retain(|w| !w.name.is_empty());
    windows.sort_by(|a, b| a.bounds.origin.x.total_cmp(&b.bounds.origin.x));
    Ok(windows
        .into_iter()
        .map(|w| MenuBarItem {
            owner: w.owner,
            name: w.name,
            pid: w.pid,
        })
        .collect())
}

/// Presses the menu behind an already-listed alias item — `AXCancelAction`
/// then `AXPressAction` on its `AXExtrasMenuBar` element, the same sequence
/// `SketchyBar`'s own menu helper uses — so a click on a mirrored item opens
/// the real menu instead of only ever showing a picture of it. Does not touch
/// this item's [`Captures`]: the resulting menu is the system's own window,
/// not this one's capture, so nothing here should ever look like a change to
/// what is drawn.
///
/// A name carrying the `(n)` duplicate suffix from
/// [`disambiguate_duplicates`] cannot be matched by identity (the real
/// `AXDescription`/`AXTitle` never carries that suffix) and falls back to the
/// nearest `AXExtrasMenuBar` child by on-screen frame — the same tolerance
/// [`ax::resolve`] uses, just scoped to one already-known pid instead of
/// walking every running application.
///
/// # Errors
///
/// [`Error::NotFound`] if no menu bar item currently matches `owner`/`name`,
/// or nothing in that owner's `AXExtrasMenuBar` is close enough to be it —
/// which is what missing Accessibility permission looks like from here, same
/// as everywhere else in this module. [`Error::PressFailed`] if a matching
/// element was found but the AX action itself was refused.
pub fn press_item(owner: &str, name: &str) -> Result<()> {
    let window = raw_menu_bar_windows()?
        .into_iter()
        .find(|w| w.owner == owner && w.name == name)
        .ok_or(Error::NotFound)?;
    let element = ax::find_extras_child(window.pid, name, window.bounds).ok_or(Error::NotFound)?;
    if ax::press(&element) {
        Ok(())
    } else {
        Err(Error::PressFailed)
    }
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
    window: Option<Window>,
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
            window: None,
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

    /// The pid this item last resolved to, if it ever has.
    #[must_use]
    pub fn pid(&self) -> Option<i32> {
        self.pid
    }

    /// Forces the next [`Alias::capture`] to re-resolve the window rather
    /// than reuse a cached one.
    pub fn invalidate(&mut self) {
        self.window = None;
    }

    /// Captures the item's current contents, re-finding its window first if
    /// none is cached or the cached one no longer captures.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] if no menu bar item goes by this owner and
    /// name any more, or the window server's error if the capture itself
    /// fails — including [`skylight::Error::NoCapture`], which is what a
    /// missing Screen Recording grant looks like.
    pub fn capture(&mut self) -> Result<Capture> {
        if self.window.is_none() {
            self.window = Some(self.find_window()?);
        }

        // Populated immediately above, or already present.
        let Some(window) = self.window.as_ref() else {
            return Err(Error::NotFound);
        };
        match window.capture() {
            Ok(image) => {
                let size = window.true_rect()?.size;
                Ok(Capture { image, size })
            }
            Err(err) => {
                // The window most likely closed since it was found; drop it
                // so the next capture re-resolves instead of repeating the
                // same failure forever.
                self.window = None;
                Err(err.into())
            }
        }
    }

    fn find_window(&mut self) -> Result<Window> {
        let found = raw_menu_bar_windows()?
            .into_iter()
            .find(|w| w.owner == self.owner && w.name == self.name)
            .ok_or(Error::NotFound)?;
        self.pid = Some(found.pid);
        Ok(Window::from_existing(found.id))
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
///
/// # Errors
///
/// Returns [`Error::NoWindowList`] if the window server refuses the list.
pub fn diagnostics() -> Result<Vec<RawMenuBarWindow>> {
    Ok(raw_menu_bar_windows()?
        .into_iter()
        .map(|w| RawMenuBarWindow {
            id: w.id,
            owner: w.owner,
            name: w.name,
            pid: w.pid,
            bounds: w.bounds,
        })
        .collect())
}

fn raw_menu_bar_windows() -> Result<Vec<RawWindow>> {
    let list =
        CGWindowListCopyWindowInfo(CGWindowListOption::OptionAll, 0).ok_or(Error::NoWindowList)?;

    // The Accessibility scan is expensive (walks every running app) and only
    // useful once Control Center shows up as an owner, so it runs at most
    // once per call, lazily.
    let mut extras: Option<Vec<ax::ExtrasMenuItem>> = None;

    let mut windows = Vec::new();
    for i in 0..list.count() {
        // SAFETY: `i` is in bounds; every element of this array is a
        // `CFDictionary`, per `CGWindowListCopyWindowInfo`'s documented
        // contract.
        let Some(dict) = (unsafe { dict_at(&list, i) }) else {
            continue;
        };

        let Some(layer) = dict_i64(dict, unsafe { kCGWindowLayer }) else {
            continue;
        };
        if layer != MENU_BAR_LAYER {
            continue;
        }

        let Some(owner) = dict_string(dict, unsafe { kCGWindowOwnerName }) else {
            continue;
        };
        if owner == "Window Server" {
            continue;
        }
        let Some(pid) = dict_i64(dict, unsafe { kCGWindowOwnerPID }) else {
            continue;
        };
        let Some(id) = dict_i64(dict, unsafe { kCGWindowNumber }) else {
            continue;
        };
        let Some(bounds) = dict_rect(dict, unsafe { kCGWindowBounds }) else {
            continue;
        };
        let name = dict_string(dict, unsafe { kCGWindowName }).unwrap_or_default();

        let pid = i32::try_from(pid).unwrap_or(i32::MAX);
        let (owner, pid) = if is_control_center_owner(&owner) {
            let extras = extras.get_or_insert_with(ax::enumerate_extras_menu_items);
            ax::resolve(&name, bounds, extras)
                .map(|item| (item.owner.clone(), item.pid))
                .unwrap_or((owner, pid))
        } else {
            (owner, pid)
        };

        windows.push(RawWindow {
            id: WindowId::try_from(id).unwrap_or(WindowId::MAX),
            owner,
            name,
            pid,
            bounds,
        });
    }

    disambiguate_duplicates(&mut windows);
    Ok(windows)
}

/// Appends a `(n)` suffix, 1-based in left-to-right order, to the name of
/// every window whose `(owner, name)` is not already unique — matching
/// upstream `SketchyBar`'s fix for the same problem. Windows with a unique
/// name are untouched.
fn disambiguate_duplicates(windows: &mut [RawWindow]) {
    let mut order: Vec<usize> = (0..windows.len()).collect();
    order.sort_by(|&a, &b| {
        windows[a]
            .bounds
            .origin
            .x
            .total_cmp(&windows[b].bounds.origin.x)
    });

    let mut total: HashMap<(String, String), u32> = HashMap::new();
    for &i in &order {
        *total
            .entry((windows[i].owner.clone(), windows[i].name.clone()))
            .or_insert(0) += 1;
    }

    let mut seen: HashMap<(String, String), u32> = HashMap::new();
    for i in order {
        let key = (windows[i].owner.clone(), windows[i].name.clone());
        if total[&key] <= 1 {
            continue;
        }
        let count = seen.entry(key).or_insert(0);
        *count += 1;
        windows[i].name = format!("{}({})", windows[i].name, count);
    }
}

fn is_control_center_owner(owner: &str) -> bool {
    matches!(owner, "Control Center" | "ControlCenter" | "Control Centre")
}

// ---- CFDictionary field extraction -----------------------------------------

/// # Safety
/// `i` must be a valid index into `array`, and every element must be a
/// `CFDictionary`.
unsafe fn dict_at(array: &CFArray, i: isize) -> Option<&CFDictionary> {
    let ptr = unsafe { array.value_at_index(i) };
    let ptr = NonNull::new(ptr.cast_mut())?.cast::<CFType>();
    let ty = unsafe { ptr.as_ref() };
    ty.downcast_ref::<CFDictionary>()
}

fn cf_key(key: &CFString) -> *const c_void {
    (std::ptr::from_ref(key)).cast()
}

fn dict_value<'a>(dict: &'a CFDictionary, key: &CFString) -> Option<&'a CFType> {
    // SAFETY: `key` is a valid, live `CFString` for the duration of the call.
    let ptr = unsafe { dict.value(cf_key(key)) };
    let ptr = NonNull::new(ptr.cast_mut())?.cast::<CFType>();
    // SAFETY: the pointer came from the dictionary we borrowed `dict` from,
    // so it lives at least as long as `dict` does.
    Some(unsafe { ptr.as_ref() })
}

fn dict_string(dict: &CFDictionary, key: &CFString) -> Option<String> {
    Some(
        dict_value(dict, key)?
            .downcast_ref::<CFString>()?
            .to_string(),
    )
}

fn dict_i64(dict: &CFDictionary, key: &CFString) -> Option<i64> {
    dict_value(dict, key)?.downcast_ref::<CFNumber>()?.as_i64()
}

fn dict_rect(dict: &CFDictionary, key: &CFString) -> Option<CGRect> {
    let bounds = dict_value(dict, key)?.downcast_ref::<CFDictionary>()?;
    let mut rect = CGRect::default();
    // SAFETY: `bounds` is a valid dictionary and `rect` a valid out-pointer.
    unsafe { CGRectMakeWithDictionaryRepresentation(Some(bounds), &raw mut rect) }.then_some(rect)
}

fn rect_center(rect: CGRect) -> CGPoint {
    CGPoint::new(
        rect.origin.x + rect.size.width / 2.0,
        rect.origin.y + rect.size.height / 2.0,
    )
}

fn point_distance(a: CGPoint, b: CGPoint) -> f64 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    dx.hypot(dy)
}

/// The macOS 26 Control Center workaround: recovering a menu bar item's real
/// owner and title through the Accessibility API, cross-checked against
/// `SketchyBar`'s `source_pid.m`.
///
/// Also where [`super::press_item`]'s AX lookup lives, and — via
/// [`frontmost_application`] and [`menu_bar_children`] — the two primitives
/// [`crate::menus`] builds on for the frontmost application's own
/// `AXMenuBar`, a different attribute from this module's `AXExtrasMenuBar`
/// but the same unsafe AX plumbing underneath, so it is shared here rather
/// than duplicated.
pub(crate) mod ax {
    use super::{
        AXError, AXUIElement, AXValue, AXValueType, CFArray, CFRetained, CFString, CFType, CGPoint,
        CGRect, CGSize, Type, point_distance, rect_center,
    };
    use objc2_app_kit::NSWorkspace;
    use std::ptr::NonNull;
    use std::time::Duration;

    /// One entry from a running application's `AXExtrasMenuBar`: a status
    /// item this process can plausibly be the real owner of.
    pub(super) struct ExtrasMenuItem {
        pub owner: String,
        pub title: String,
        pub pid: i32,
        pub frame: CGRect,
    }

    /// Walks every running application's extras menu bar. Returns nothing,
    /// silently, if Accessibility permission is not granted — the same
    /// silent-failure shape as Screen Recording, which is why
    /// [`super::accessibility_trusted`] exists.
    pub(super) fn enumerate_extras_menu_items() -> Vec<ExtrasMenuItem> {
        let mut items = Vec::new();

        let workspace = NSWorkspace::sharedWorkspace();
        let running = workspace.runningApplications();

        for app in &running.to_vec() {
            let pid = app.processIdentifier();
            if pid <= 0 {
                continue;
            }
            let owner = app
                .localizedName()
                .map_or_else(|| pid.to_string(), |name| name.to_string());

            // SAFETY: `pid` is a valid, live process id.
            let element = unsafe { AXUIElement::new_application(pid) };
            let Some(extras) = copy_element(&element, "AXExtrasMenuBar") else {
                continue;
            };
            let Some(children) = copy_array(&extras, "AXVisibleChildren")
                .or_else(|| copy_array(&extras, "AXChildren"))
            else {
                continue;
            };

            for i in 0..children.count() {
                // SAFETY: `i` is in bounds; every element of an
                // `AXUIElement`'s children attribute is itself an
                // `AXUIElement`.
                let Some(child) = (unsafe { array_element(&children, i) }) else {
                    continue;
                };

                if !attribute_bool(child, "AXEnabled").unwrap_or(true) {
                    continue;
                }
                // Not an `AXRole == "AXButton"` filter here, deliberately,
                // even though `alias_watch` uses exactly that to clear
                // notification noise: measured live, several real,
                // correctly-attributed items on this machine — OneDrive's,
                // Spotlight's, Fantastical's own status items among them —
                // do not report that role from their own `AXExtrasMenuBar`
                // child, so requiring it here silently dropped them from the
                // candidate list and collapsed their recovered owner back to
                // plain "Control Center" for everything. `alias_watch`'s use
                // is narrower and safe: it only ever discards notifications
                // already known to share a *watched* pid, not a whole
                // app's candidacy for owner recovery.
                //
                // Control Centre's own children have no `AXTitle` at all —
                // their identifying string lives in `AXDescription` instead
                // (confirmed live by `examples/ax_observer_probe.rs`, e.g.
                // `"Clock"`). `AXDescription` is checked first to match
                // `alias_watch`'s own precedence.
                let description =
                    attribute_string(child, "AXDescription").filter(|d| !d.is_empty());
                let title = attribute_string(child, "AXTitle").filter(|t| !t.is_empty());
                let Some(identity) = description.or(title) else {
                    continue;
                };
                let Some(frame) = attribute_frame(child) else {
                    continue;
                };

                items.push(ExtrasMenuItem {
                    owner: owner.clone(),
                    title: identity,
                    pid,
                    frame,
                });
            }
        }

        items
    }

    /// Matches a captured window's name and bounds against the extras
    /// gathered by [`enumerate_extras_menu_items`], the same two-pass
    /// tolerance `SketchyBar`'s `source_pid_for_window_with_name_hint` uses:
    /// first an exact title match within a tight radius, then bounds alone
    /// within a looser one for items whose title didn't round-trip.
    pub(super) fn resolve<'a>(
        window_name: &str,
        bounds: CGRect,
        extras: &'a [ExtrasMenuItem],
    ) -> Option<&'a ExtrasMenuItem> {
        const TIGHT_RADIUS: f64 = 6.0;
        const TIGHT_SIZE_TOLERANCE: f64 = 8.0;
        const LOOSE_RADIUS: f64 = 14.0;

        let center = rect_center(bounds);
        let close_enough = |item: &&ExtrasMenuItem| {
            let d = point_distance(center, rect_center(item.frame));
            let w_diff = (item.frame.size.width - bounds.size.width).abs();
            let h_diff = (item.frame.size.height - bounds.size.height).abs();
            d <= TIGHT_RADIUS && w_diff <= TIGHT_SIZE_TOLERANCE && h_diff <= TIGHT_SIZE_TOLERANCE
        };

        if !window_name.is_empty() {
            let named = extras
                .iter()
                .filter(|item| item.title == window_name)
                .filter(close_enough)
                .min_by(|a, b| {
                    point_distance(center, rect_center(a.frame))
                        .total_cmp(&point_distance(center, rect_center(b.frame)))
                });
            if named.is_some() {
                return named;
            }
        }

        extras
            .iter()
            .filter(|item| point_distance(center, rect_center(item.frame)) <= LOOSE_RADIUS)
            .min_by(|a, b| {
                point_distance(center, rect_center(a.frame))
                    .total_cmp(&point_distance(center, rect_center(b.frame)))
            })
    }

    fn attribute(element: &AXUIElement, name: &str) -> Option<CFRetained<CFType>> {
        let attr = CFString::from_str(name);
        let mut value: *const CFType = std::ptr::null();
        // SAFETY: `attr` is a live `CFString` for the call's duration, and
        // `value` is a valid out-pointer.
        let err = unsafe { element.copy_attribute_value(&attr, NonNull::from(&mut value)) };
        if err != AXError::Success {
            return None;
        }
        let ptr = NonNull::new(value.cast_mut())?;
        // SAFETY: a non-null result from `AXUIElementCopyAttributeValue`
        // carries a +1 reference, which transfers to `CFRetained`.
        Some(unsafe { CFRetained::from_raw(ptr) })
    }

    pub(crate) fn copy_element(
        element: &AXUIElement,
        name: &str,
    ) -> Option<CFRetained<AXUIElement>> {
        attribute(element, name)?.downcast::<AXUIElement>().ok()
    }

    pub(crate) fn copy_array(element: &AXUIElement, name: &str) -> Option<CFRetained<CFArray>> {
        attribute(element, name)?.downcast::<CFArray>().ok()
    }

    /// # Safety
    /// `i` must be a valid index into `array`, and every element must be an
    /// `AXUIElement`.
    pub(crate) unsafe fn array_element(array: &CFArray, i: isize) -> Option<&AXUIElement> {
        let ptr = unsafe { array.value_at_index(i) };
        let ptr = NonNull::new(ptr.cast_mut())?.cast::<CFType>();
        let ty = unsafe { ptr.as_ref() };
        ty.downcast_ref::<AXUIElement>()
    }

    pub(crate) fn attribute_string(element: &AXUIElement, name: &str) -> Option<String> {
        Some(
            attribute(element, name)?
                .downcast::<CFString>()
                .ok()?
                .to_string(),
        )
    }

    fn attribute_bool(element: &AXUIElement, name: &str) -> Option<bool> {
        let value = attribute(element, name)?;
        let boolean = value.downcast::<objc2_core_foundation::CFBoolean>().ok()?;
        Some(boolean.as_bool())
    }

    fn attribute_value<const N: usize>(
        element: &AXUIElement,
        name: &str,
        the_type: AXValueType,
    ) -> Option<[u8; N]> {
        let value = attribute(element, name)?.downcast::<AXValue>().ok()?;
        let mut buf = [0u8; N];
        // SAFETY: `buf` has exactly the size `the_type`'s C structure needs,
        // matched by the caller.
        let ok = unsafe { value.value(the_type, NonNull::from(&mut buf).cast()) };
        ok.then_some(buf)
    }

    fn attribute_point(element: &AXUIElement, name: &str) -> Option<CGPoint> {
        let bytes =
            attribute_value::<{ size_of::<CGPoint>() }>(element, name, AXValueType::CGPoint)?;
        Some(unsafe { std::mem::transmute::<[u8; size_of::<CGPoint>()], CGPoint>(bytes) })
    }

    fn attribute_size(element: &AXUIElement, name: &str) -> Option<CGSize> {
        let bytes = attribute_value::<{ size_of::<CGSize>() }>(element, name, AXValueType::CGSize)?;
        Some(unsafe { std::mem::transmute::<[u8; size_of::<CGSize>()], CGSize>(bytes) })
    }

    pub(crate) fn attribute_frame(element: &AXUIElement) -> Option<CGRect> {
        if let Some(bytes) =
            attribute_value::<{ size_of::<CGRect>() }>(element, "AXFrame", AXValueType::CGRect)
        {
            return Some(unsafe {
                std::mem::transmute::<[u8; size_of::<CGRect>()], CGRect>(bytes)
            });
        }
        let position = attribute_point(element, "AXPosition")?;
        let size = attribute_size(element, "AXSize")?;
        Some(CGRect::new(position, size))
    }

    /// How far a candidate `AXExtrasMenuBar` child's on-screen frame may sit
    /// from the resolved window's own bounds and still be trusted as the
    /// element behind it, for [`find_extras_child`]'s fallback. Looser than
    /// [`resolve`]'s own `LOOSE_RADIUS` (14pt): that constant tolerates a
    /// window-vs-AX-frame mismatch while still searching *every* running
    /// application for the right owner; here the owner is already known and
    /// only its own items are candidates, so a same-pid neighbour a little
    /// further off is still far more likely to be a mismeasurement than a
    /// different item.
    const PRESS_MATCH_RADIUS: f64 = 20.0;

    /// Finds the `AXExtrasMenuBar` child behind an already-resolved window,
    /// for [`super::press_item`]. Unlike [`resolve`], the pid is already
    /// known, so only that one application's extras are walked rather than
    /// every running one.
    ///
    /// Prefers an exact `AXDescription`/`AXTitle` match (same precedence as
    /// [`enumerate_extras_menu_items`]), then falls back to the nearest
    /// candidate by frame within [`PRESS_MATCH_RADIUS`] — what a disambiguated
    /// `(n)` name always needs, since the real identity string never carries
    /// that suffix.
    pub(crate) fn find_extras_child(
        pid: i32,
        name_hint: &str,
        frame_hint: CGRect,
    ) -> Option<CFRetained<AXUIElement>> {
        // SAFETY: `pid` is a process id the caller just read off the live
        // window list.
        let app = unsafe { AXUIElement::new_application(pid) };
        let extras = copy_element(&app, "AXExtrasMenuBar")?;
        let children = copy_array(&extras, "AXVisibleChildren")
            .or_else(|| copy_array(&extras, "AXChildren"))?;

        let mut nearest: Option<(CFRetained<AXUIElement>, f64)> = None;
        for i in 0..children.count() {
            // SAFETY: `i` is in bounds; every element is an `AXUIElement`,
            // per `array_element`'s own contract.
            let Some(child) = (unsafe { array_element(&children, i) }) else {
                continue;
            };
            let identity = attribute_string(child, "AXDescription")
                .filter(|d| !d.is_empty())
                .or_else(|| attribute_string(child, "AXTitle").filter(|t| !t.is_empty()));
            if identity.as_deref() == Some(name_hint) {
                return Some(child.retain());
            }
            let Some(frame) = attribute_frame(child) else {
                continue;
            };
            let distance = point_distance(rect_center(frame), rect_center(frame_hint));
            if nearest.as_ref().is_none_or(|(_, best)| distance < *best) {
                nearest = Some((child.retain(), distance));
            }
        }

        let (element, distance) = nearest?;
        (distance <= PRESS_MATCH_RADIUS).then_some(element)
    }

    /// `AXCancelAction` then `AXPressAction`, exactly the sequence
    /// `SketchyBar`'s own menu helper performs — the cancel first dismisses
    /// any menu already open, so the press reliably opens this one rather
    /// than sometimes toggling it shut.
    pub(crate) fn press(element: &AXUIElement) -> bool {
        let cancel = CFString::from_str("AXCancel");
        // SAFETY: `cancel` is a live `CFString` for the call's duration.
        unsafe { element.perform_action(&cancel) };
        std::thread::sleep(Duration::from_millis(1));
        let press = CFString::from_str("AXPress");
        // SAFETY: `press` is a live `CFString` for the call's duration.
        unsafe { element.perform_action(&press) == AXError::Success }
    }

    /// The frontmost application's pid and name, for [`crate::menus`].
    pub(crate) fn frontmost_application() -> Option<(i32, String)> {
        let app = NSWorkspace::sharedWorkspace().frontmostApplication()?;
        let pid = app.processIdentifier();
        if pid <= 0 {
            return None;
        }
        let name = app
            .localizedName()
            .map_or_else(|| pid.to_string(), |name| name.to_string());
        Some((pid, name))
    }

    /// One application's own top-level menu bar (`AXMenuBar`) — the Apple
    /// menu and its File/Edit/View/... titles — as opposed to
    /// [`enumerate_extras_menu_items`]'s `AXExtrasMenuBar`, a different
    /// attribute entirely. For [`crate::menus`].
    pub(crate) fn menu_bar_children(pid: i32) -> Option<CFRetained<CFArray>> {
        // SAFETY: `pid` is a live process id.
        let app = unsafe { AXUIElement::new_application(pid) };
        let menu_bar = copy_element(&app, "AXMenuBar")?;
        copy_array(&menu_bar, "AXVisibleChildren").or_else(|| copy_array(&menu_bar, "AXChildren"))
    }
}

/// A captured menu bar item, ready to draw.
pub struct Captured {
    pub image: CFRetained<CGImage>,
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
    /// A digest of the pixels, so an unchanged icon can be told from a changed
    /// one. `SketchyBar` re-draws an alias whether or not it moved; a clock
    /// that only changes once a minute should not cost a repaint every second.
    pub digest: u64,
}

/// What each alias item is mirroring, and the last thing it captured.
///
/// Out of the ECS for the same reason the shaped text is: a `CGImage` is not
/// `Send`, so it cannot be a component. Keyed by entity alongside it.
#[derive(Default)]
pub struct Captures {
    mirrors: std::collections::HashMap<bevy_ecs::entity::Entity, Mirror>,
    /// Every entity's live AX notification registration, if it has one. See
    /// [`crate::alias_watch`].
    watch: crate::alias_watch::AxWatch,
}

struct Mirror {
    alias: Alias,
    /// The spec it was built from, so a changed spec rebuilds it.
    spec: String,
    captured: Option<Captured>,
    /// Consecutive [`Captures::refresh`] calls in a row that captured the
    /// same digest as `captured`. See [`STALE_AFTER`].
    unchanged: u32,
    /// Set once an AX notification has actually matched this item — never
    /// on registration alone. Only once this is true does `refresh` trust
    /// the push enough to poll it any slower than every tick.
    confirmed: bool,
    /// Real captures skipped since the last one, while `confirmed` and
    /// nothing is dirty. See [`SLOW_FACTOR`].
    skipped: u32,
}

/// After this many consecutive unchanged captures, [`Captures::refresh`]
/// forces the alias to re-resolve its window before capturing again, rather
/// than trusting the cached one.
///
/// This exists for a failure mode `capture()`'s own error handling cannot
/// see: the window server can keep successfully capturing a `WindowId` whose
/// real item was destroyed and rebuilt out from under it — most commonly an
/// app tearing down and recreating its `NSStatusItem` — producing the same
/// bytes forever with no error at any point. [`Alias::invalidate`] existed to
/// handle exactly this, but nothing called it: this is that caller.
///
/// A poll interval is the caller's business, not this module's (see
/// `ecs::ALIAS_POLL`), so this counts calls rather than time. At the daemon's
/// current 500ms poll, this is two minutes — long enough that a legitimately
/// static icon eats only one extra window-list scan every couple of minutes,
/// short enough that a genuinely stale mirror does not stay stale for long.
///
/// This counts real captures, not ticks, so a `confirmed` alias — one
/// [`SLOW_FACTOR`] is already stretching to a real capture only once every
/// twenty ticks — reaches this ceiling roughly twenty times slower in wall
/// time than an unconfirmed one, with no extra state needed to make that so.
/// That is a deliberate trade rather than an oversight: the bug this guards
/// against is a window-server quirk unrelated to whether AX observation is
/// working, so a confirmed item is not meaningfully less likely to hit it,
/// only slower to notice it if it does. Accepting that is worth not having
/// two different staleness policies to reason about. A pid actually dying —
/// the owning app restarting under a new one — is also caught well before
/// this ceiling regardless: the next scheduled real capture (at most
/// [`SLOW_FACTOR`] ticks away) re-resolves the window, notices the pid
/// changed, and re-points [`crate::alias_watch::AxWatch`] at the new one
/// through [`crate::alias_watch::AxWatch::sync_one`].
const STALE_AFTER: u32 = 240;

/// How many [`Captures::refresh`] ticks a `confirmed` alias's real capture is
/// stretched to once its AX push has actually matched it at least once —
/// `SLOW_FACTOR * ecs::ALIAS_POLL`, so at the daemon's current 500ms poll,
/// roughly ten seconds between real captures instead of one every tick.
/// [`crate::alias_watch::AxWatch::take_dirty`] returning `true` resets this
/// immediately: a real change is captured on the very next tick, not at the
/// end of this window.
const SLOW_FACTOR: u32 = 20;

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

    // The pixels, not the object: two captures of an unchanged item are
    // different `CGImage`s with identical contents.
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

impl Captures {
    /// Re-captures one item, reporting its digest if what it draws changed.
    ///
    /// `None` means nothing to redraw — either the capture failed, it came
    /// back identical (most re-captures do), or this tick was skipped
    /// because a confirmed AX push says nothing has changed (see
    /// [`SLOW_FACTOR`]).
    pub fn refresh(&mut self, entity: bevy_ecs::entity::Entity, spec: &str) -> Option<u64> {
        let Some((owner, name)) = split_spec(spec) else {
            tracing::warn!(spec, "an alias is written `Owner,Name`");
            return None;
        };

        let mirror = match self.mirrors.get_mut(&entity) {
            Some(mirror) if mirror.spec == spec => mirror,
            _ => {
                self.watch.forget(entity);
                self.mirrors
                    .entry(entity)
                    .insert_entry(Mirror {
                        alias: Alias::new(owner, name),
                        spec: spec.to_owned(),
                        captured: None,
                        unchanged: 0,
                        confirmed: false,
                        skipped: 0,
                    })
                    .into_mut()
            }
        };

        if self.watch.take_dirty(entity) {
            tracing::debug!(
                spec,
                "an AX notification marked this alias dirty; re-capturing now"
            );
            mirror.confirmed = true;
            mirror.skipped = 0;
        } else if mirror.confirmed {
            if mirror.skipped < SLOW_FACTOR {
                mirror.skipped += 1;
                self.watch
                    .sync_one(entity, mirror.alias.name(), mirror.alias.pid());
                return None;
            }
            mirror.skipped = 0;
        }

        if mirror.unchanged >= STALE_AFTER {
            mirror.alias.invalidate();
            mirror.unchanged = 0;
        }

        let capture = match mirror.alias.capture() {
            Ok(capture) => capture,
            Err(err) => {
                tracing::debug!(%err, spec, "could not capture the menu bar item");
                return None;
            }
        };

        self.watch
            .sync_one(entity, mirror.alias.name(), mirror.alias.pid());

        let Analysis { digest, trim_px } = analyze(&capture.image);
        if mirror
            .captured
            .as_ref()
            .is_some_and(|held| held.digest == digest)
        {
            mirror.unchanged = mirror.unchanged.saturating_add(1);
            return None;
        }
        mirror.unchanged = 0;
        let trim = trim_rect(&capture.image, capture.size, trim_px);
        mirror.captured = Some(Captured {
            image: capture.image,
            size: capture.size,
            trim,
            digest,
        });
        Some(digest)
    }

    #[must_use]
    pub fn get(&self, entity: bevy_ecs::entity::Entity) -> Option<&Captured> {
        self.mirrors.get(&entity)?.captured.as_ref()
    }

    pub fn forget(&mut self, entity: bevy_ecs::entity::Entity) {
        self.mirrors.remove(&entity);
        self.watch.forget(entity);
    }
}

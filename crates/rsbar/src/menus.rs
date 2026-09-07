//! The frontmost application's own menu bar — the Apple menu and its
//! File/Edit/View/... titles — natively, replacing the user's C helper
//! (`helpers/menus/menus.c`, built with `-l`/`-A`/`-s`) with the equivalent
//! behaviour in-process: no vendored C, no build step, no shelling out.
//!
//! # Why this is a separate module from [`crate::alias`]
//!
//! `menus.c`'s `-A` — print the aliasable extras — is already
//! [`crate::alias::list_menu_bar_items`], and better: it disambiguates
//! duplicate names and resolves a Control Center-hosted item's real owner
//! the same way regardless of macOS version, rather than `menus.c`'s
//! `source_pid_needs_workaround`, which only ever ran the resolution at all
//! on macOS 26+ (`Gestalt(gestaltSystemVersionMajor, ...) >= 26`) and used a
//! tight exact-position match (`point_distance_squared <= 1.0`) with no
//! fallback. Both approaches solve the same problem — recovering a status
//! item's real owner once Control Center starts hosting it — so this daemon
//! keeps the one already proven against this machine
//! ([`crate::alias`]'s module docs) instead of carrying two.
//!
//! `-l` and `-s` have no equivalent in `crate::alias` at all, because they
//! read a *different* Accessibility attribute — `AXMenuBar`, not
//! `AXExtrasMenuBar` — on a *different* element, the frontmost application
//! rather than a status item's owner. This module is what reads it.
//!
//! # Why these titles were never findable as menu bar aliases (task 12)
//!
//! `--query menu-items` only ever lists windows at
//! the menu bar layer (0x19) — the status items on the right.
//! The Apple menu and an app's own File/Edit/View/... titles on the left are
//! not there, and widening that filter finds nothing, because there is
//! nothing to find: `crates/rsbar/examples/layer_probe.rs` (a scratch probe
//! written for this investigation) dumps every on-screen window at every
//! layer, unfiltered, and on this machine (macOS 26.5.1) the left-hand menu
//! bar chrome is not decomposed into one window per title at all. It shows
//! up as exactly two windows owned by `Window Server`, both named
//! `"Menubar"`, one per display, at layer 0x18 — one level *below* the
//! status items' own 0x19 — each covering the *entire* menu bar strip on its
//! display. Nothing else in the full, unfiltered list — at 0x18, at 0x19, or
//! anywhere else — corresponds to "File" or "Edit" or the Apple logo
//! individually. This is a real platform limit, not a filtering bug: the
//! window server paints the frontmost application's menu titles directly
//! into that one shared surface from data it pulls out of the app itself
//! (`AXMenuBarAttribute`/`AXVisibleChildren`, per [`list`] below), the same
//! way it has since long before status items existed, and there is nothing
//! resembling a per-title window anywhere in the list to capture.
//!
//! Confirming this is not just theoretical: the very config this daemon
//! replaces does not try to alias these items either.
//! `items/menus.lua`/`items/left.lua` build the Apple glyph as a static
//! icon string and the File/Edit/View/... row as plain text labels refreshed
//! from `menus -l` on every `front_app_switched` event, each bound to
//! `menus -s <index>` as its click script — exactly the `list`/`press` shape
//! this module provides, never a captured picture. "Aliasable" for these
//! items means listed-and-pressable, not pixel-mirrored, and that is a real
//! difference from [`crate::alias`]'s items: there is no window to capture,
//! so there is no [`crate::alias::Capture`] and no [`crate::alias::Captures`]
//! entry for one of these — nothing here is a drawable component, so nothing
//! here needs a place in `layout.rs`'s damage-tracking gate the way
//! `AliasContent` does. A caller wanting to show these titles draws them as
//! ordinary text, the way the Lua config above already does.
//!
//! # Pressing does not touch a mirrored item's own capture
//!
//! [`crate::alias::press_item`] and [`press`] both only ever perform an AX
//! action on the *system's* menu bar element. The menu that opens is the
//! system's own window, not anything this daemon draws or captures, so
//! neither function reads or writes an [`crate::alias::Alias`]'s cached
//! window, invalidates it, or otherwise makes
//! [`crate::alias::Captures::refresh`] think anything changed. A press is
//! genuinely a no-op as far as this daemon's own draw state is concerned —
//! deliberately, since a click that repainted a mirrored item for no reason
//! would be the same kind of bug as a change that fails to repaint one.

use crate::alias::ax;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("no application is currently frontmost")]
    NoFrontApp,
    #[error("the frontmost application exposes no menu bar")]
    NoMenuBar,
    #[error("no menu at this index")]
    NoSuchMenu,
    #[error("pressing this menu did not succeed (Accessibility permission is likely missing)")]
    PressFailed,
}

pub type Result<T> = std::result::Result<T, Error>;

/// One of the frontmost application's own top-level menus, in on-screen
/// left-to-right order.
///
/// Index 0 is always the Apple menu (`AXTitle` is the literal string
/// `"Apple"`, not the logo glyph itself — measured live on this machine,
/// Ghostty frontmost: `[0] title="Apple"`), index 1 is the application's own
/// name, and the rest are whatever that application put in its menu bar.
/// `menus.c`'s `-l` skips index 0 when printing (`for i = 1; i < count`);
/// this returns every index instead and lets a caller decide, since
/// `items/menus.lua` draws index 0 as a separate, permanently-visible glyph
/// rather than part of the refreshed row `-l` feeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuTitle {
    pub index: usize,
    pub title: String,
}

/// Lists the frontmost application's own menu titles.
///
/// # Errors
///
/// [`Error::NoFrontApp`] if nothing is frontmost. [`Error::NoMenuBar`] if
/// Accessibility permission is missing (the same silent-failure shape
/// documented on [`crate::alias::accessibility_trusted`] applies here: a
/// missing grant and a frontmost app that genuinely has no menu bar look
/// identical) or the frontmost application really does not expose one.
pub fn list() -> Result<Vec<MenuTitle>> {
    let (pid, _name) = ax::frontmost_application().ok_or(Error::NoFrontApp)?;
    let children = ax::menu_bar_children(pid).ok_or(Error::NoMenuBar)?;

    let mut items = Vec::new();
    for i in 0..children.count() {
        // SAFETY: `i` is in bounds; every element of `AXVisibleChildren` is
        // itself an `AXUIElement`.
        let Some(item) = (unsafe { ax::array_element(&children, i) }) else {
            continue;
        };
        let title = ax::attribute_string(item, "AXTitle").unwrap_or_default();
        items.push(MenuTitle {
            // A menu bar has nowhere near `usize::MAX` entries; the fallback
            // only matters for a pathological `CFArray` this loop would
            // never actually see.
            index: usize::try_from(i).unwrap_or(usize::MAX),
            title,
        });
    }
    Ok(items)
}

/// Presses one of the frontmost application's own top-level menus by the
/// index [`list`] reported.
///
/// # Errors
///
/// As [`list`], plus [`Error::NoSuchMenu`] if `index` is out of range for the
/// frontmost application's current menu bar, and [`Error::PressFailed`] if a
/// matching element was found but the AX action itself was refused.
pub fn press(index: usize) -> Result<()> {
    let (pid, _name) = ax::frontmost_application().ok_or(Error::NoFrontApp)?;
    let children = ax::menu_bar_children(pid).ok_or(Error::NoMenuBar)?;
    let count = usize::try_from(children.count()).unwrap_or(0);
    if index >= count {
        return Err(Error::NoSuchMenu);
    }
    // SAFETY: `index < count`, just checked above, and every element of
    // `AXVisibleChildren` is itself an `AXUIElement`.
    let item = unsafe { ax::array_element(&children, isize::try_from(index).unwrap_or(0)) }
        .ok_or(Error::NoSuchMenu)?;
    if ax::press(item) {
        Ok(())
    } else {
        Err(Error::PressFailed)
    }
}

//! The frontmost application's own menu bar -- the Apple menu and its
//! File/Edit/View titles -- replacing the config's `menus.c` helper with no C
//! and no shell-out.
//!
//! Separate from [`crate::alias`] because it reads a different attribute on a
//! different element: `AXMenuBar` on the frontmost application, rather than
//! `AXExtrasMenuBar` on a status item's owner. `menus.c`'s `-A` is already
//! [`crate::alias::list_menu_bar_items`], which disambiguates duplicate names
//! and resolves a Control Centre-hosted item's owner on every macOS version
//! rather than only on 26 and later.
//!
//! # These titles are not windows, so they cannot be mirrored
//!
//! `crates/rsbar/examples/layer_probe.rs` dumps every on-screen window at
//! every layer. On macOS 26.5.1 the whole left-hand menu bar is two windows
//! owned by `Window Server`, both named `"Menubar"`, one per display, at layer
//! 0x18 -- one below the status items -- each spanning its display. Nothing
//! anywhere in that list is "File" or "Edit" or the Apple logo on its own: the
//! window server paints an application's titles straight into that shared
//! surface out of the app's own `AXMenuBar`. So these are listable and
//! pressable but never capturable, which is what the config this replaces
//! already assumed -- `items/menus.lua` draws them as plain text refreshed
//! from `menus -l` and clicks them with `menus -s <index>`.
//!
//! Nothing here is a drawable component: a press acts on the system's own
//! menu bar element and never touches an [`crate::alias::Alias`]'s cached
//! window, so it cannot make a mirrored item look dirty.

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

/// One of the frontmost application's own top-level menus, left to right.
///
/// Index 0 is the Apple menu, whose `AXTitle` is the literal string `"Apple"`
/// rather than the glyph; index 1 is the application's own name. `menus.c`'s
/// `-l` skips index 0, but this returns it, because the config draws it as its
/// own permanently-visible item rather than part of the refreshed row.
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

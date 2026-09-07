//! Enumerating displays and finding their backing scale.

use objc2::MainThreadMarker;
use objc2_app_kit::NSScreen;
use objc2_core_foundation::CGRect;
use objc2_core_graphics::{CGDirectDisplayID, CGDisplayBounds, CGGetActiveDisplayList};
use objc2_foundation::NSString;

/// A display the bar can sit on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Display {
    pub id: CGDirectDisplayID,
    /// In global coordinates, origin top-left — the space window frames use.
    pub bounds: CGRect,
    /// Backing scale. Mixed-DPI setups are ordinary, so this is per display
    /// rather than a single global value.
    pub scale: f64,
}

/// Displays currently active, in the window server's order.
///
/// Mirrored displays report the same bounds; they are left in, because each
/// still needs its own window for the bar to appear on both.
#[must_use]
pub fn active() -> Vec<Display> {
    const MAX_DISPLAYS: u32 = 16;
    let mut ids = [0 as CGDirectDisplayID; MAX_DISPLAYS as usize];
    let mut count: u32 = 0;

    // SAFETY: `ids` has room for MAX_DISPLAYS, and `count` is a valid out-pointer.
    let result = unsafe { CGGetActiveDisplayList(MAX_DISPLAYS, ids.as_mut_ptr(), &raw mut count) };
    if result != objc2_core_graphics::CGError::Success {
        tracing::warn!(
            ?result,
            "could not enumerate displays; assuming the main one"
        );
        let id = objc2_core_graphics::CGMainDisplayID();
        return vec![Display {
            id,
            bounds: CGDisplayBounds(id),
            scale: scale_for(id),
        }];
    }

    ids[..count as usize]
        .iter()
        .map(|&id| Display {
            id,
            bounds: CGDisplayBounds(id),
            scale: scale_for(id),
        })
        .collect()
}

/// The backing scale for a display, by matching `NSScreen` on its display id.
///
/// There is no CoreGraphics call for this — `CGDisplayPixelsWide` reports the
/// current mode's point size, not its backing factor — so it has to come from
/// `AppKit`. Falls back to 2.0, since guessing retina on a retina machine is
/// the less visible mistake: a too-high scale looks correct, a too-low one is
/// visibly soft.
fn scale_for(id: CGDirectDisplayID) -> f64 {
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::warn!("backing scale queried off the main thread; assuming 2.0");
        return 2.0;
    };

    let key = NSString::from_str("NSScreenNumber");
    let screens = NSScreen::screens(mtm);
    for index in 0..screens.count() {
        let screen = screens.objectAtIndex(index);
        let description = screen.deviceDescription();
        let Some(number) = description.objectForKey(&key) else {
            continue;
        };
        let Ok(number) = number.downcast::<objc2_foundation::NSNumber>() else {
            continue;
        };
        if number.as_u32() == id {
            return screen.backingScaleFactor();
        }
    }
    2.0
}

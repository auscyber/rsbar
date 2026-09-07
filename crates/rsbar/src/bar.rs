//! The bar's own settings and its windows.
//!
//! Split deliberately: [`Settings`] is plain data and a `Resource`, so change
//! detection covers it; [`Panels`] owns window server handles and is `NonSend`.

use crate::display::{self, Display};
use bevy_ecs::prelude::Resource;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use rsbar_protocol::style::Color;
use rsbar_protocol::{BarPatch, BarState, Edge};
use skylight::{Window, WindowTags, level};

#[derive(Resource, Debug, Clone, PartialEq)]
pub struct Settings {
    pub height: f64,
    pub edge: Edge,
    pub color: Color,
    pub margin: f64,
    pub y_offset: f64,
    pub corner_radius: f64,
    pub blur_radius: i32,
    pub hidden: bool,
    /// Whether the bar draws above the system menu bar or below it.
    ///
    /// Below by default. The bar occupies the menu bar's own strip either
    /// way — that is where a status bar goes — but underneath, the menu
    /// extras and the notification centre still draw and still take clicks.
    /// Above, the bar covers them with no way to get at them, which is
    /// `SketchyBar`'s look and wants the system menu bar set to hide itself.
    pub topmost: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            height: 32.0,
            edge: Edge::Top,
            color: Color(0xe014_1820),
            margin: 0.0,
            y_offset: 0.0,
            corner_radius: 0.0,
            blur_radius: 0,
            hidden: false,
            topmost: false,
        }
    }
}

impl Settings {
    /// Applies a patch, reporting whether anything about the geometry moved —
    /// the caller has to reshape the windows if so.
    pub fn apply(&mut self, patch: &BarPatch) -> Changes {
        let mut changes = Changes::empty();
        if let Some(h) = patch.height {
            self.height = h;
            changes.insert(Changes::GEOMETRY);
        }
        if let Some(e) = patch.edge {
            self.edge = e;
            changes.insert(Changes::GEOMETRY);
        }
        if let Some(m) = patch.margin {
            self.margin = m;
            changes.insert(Changes::GEOMETRY);
        }
        if let Some(y) = patch.y_offset {
            self.y_offset = y;
            changes.insert(Changes::GEOMETRY);
        }
        if let Some(c) = patch.color {
            self.color = Color(c);
        }
        if let Some(r) = patch.corner_radius {
            self.corner_radius = r;
        }
        if let Some(r) = patch.blur_radius {
            self.blur_radius = r;
            changes.insert(Changes::BLUR);
        }
        if let Some(hidden) = patch.hidden {
            self.hidden = hidden;
            changes.insert(Changes::VISIBILITY);
        }
        if let Some(topmost) = patch.topmost {
            self.topmost = topmost;
            changes.insert(Changes::LEVEL);
        }
        changes
    }

    /// Where the bar sits on one display.
    #[must_use]
    pub fn frame_for(&self, display: &Display) -> CGRect {
        let bounds = display.bounds;
        let y = match self.edge {
            Edge::Top => bounds.origin.y + self.y_offset,
            Edge::Bottom => bounds.origin.y + bounds.size.height - self.height - self.y_offset,
        };
        CGRect::new(
            CGPoint::new(bounds.origin.x + self.margin, y),
            CGSize::new(bounds.size.width - 2.0 * self.margin, self.height),
        )
    }

    #[must_use]
    pub fn state(&self, displays: usize) -> BarState {
        BarState {
            height: self.height,
            edge: self.edge,
            color: self.color.0,
            margin: self.margin,
            y_offset: self.y_offset,
            corner_radius: self.corner_radius,
            blur_radius: self.blur_radius,
            topmost: self.topmost,
            hidden: self.hidden,
            displays,
        }
    }
}

bitflags::bitflags! {
    /// What a patch touched that needs more than a repaint.
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Changes: u8 {
        /// The windows have to be reframed.
        const GEOMETRY = 1 << 0;
        const BLUR = 1 << 1;
        const VISIBILITY = 1 << 2;
        /// Above or below the system menu bar.
        const LEVEL = 1 << 3;
    }
}

/// The bar on one display.
pub struct Panel {
    pub display: Display,
    pub window: Window,
    pub frame: CGRect,
}

/// One panel per display.
///
/// Not one window stretched across the desktop: with "Displays have separate
/// Spaces" a single window renders on only one of them.
#[derive(Default)]
pub struct Panels {
    panels: Vec<Panel>,
    /// Whether the bar takes clicks. Remembered so a panel built for a newly
    /// attached display comes up matching the others.
    clickable: bool,
}

impl Panels {
    pub fn iter(&self) -> impl Iterator<Item = &Panel> {
        self.panels.iter()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.panels.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.panels.is_empty()
    }

    /// Rebuilds against the displays that exist now, reusing the window of a
    /// display that is still present so a resolution change does not flicker
    /// the bar away. Windows of departed displays are dropped, which releases
    /// them.
    ///
    /// # Errors
    ///
    /// Returns the window server's error if a window cannot be created or
    /// configured.
    pub fn rebuild(&mut self, settings: &Settings) -> skylight::Result<()> {
        let mut panels = Vec::new();
        for display in display::active() {
            let frame = settings.frame_for(&display);
            let panel = match self.panels.iter().position(|p| p.display.id == display.id) {
                Some(index) => {
                    let mut panel = self.panels.remove(index);
                    panel.window.set_frame(frame)?;
                    if (panel.display.scale - display.scale).abs() > f64::EPSILON {
                        panel.window.set_scale(display.scale)?;
                    }
                    panel.display = display;
                    panel.frame = frame;
                    panel
                }
                None => Panel {
                    window: new_window(frame, &display, settings, self.clickable)?,
                    display,
                    frame,
                },
            };
            tracing::debug!(
                display = panel.display.id,
                scale = panel.display.scale,
                frame = ?panel.frame,
                "panel"
            );
            panels.push(panel);
        }
        self.panels = panels;
        Ok(())
    }

    /// Moves every panel to where the settings now say it goes.
    ///
    /// One batch, so a height change lands on every display at once rather than
    /// rippling across monitors.
    ///
    /// # Errors
    ///
    /// Returns the window server's error if a window cannot be reshaped.
    pub fn reframe(&mut self, settings: &Settings) -> skylight::Result<()> {
        let frames: Vec<_> = self
            .panels
            .iter()
            .map(|p| settings.frame_for(&p.display))
            .collect();
        skylight::batched(|| -> skylight::Result<()> {
            for (panel, frame) in self.panels.iter_mut().zip(frames) {
                panel.window.set_frame(frame)?;
                panel.frame = frame;
            }
            Ok(())
        })
    }

    /// # Errors
    ///
    /// Returns the window server's error if a window rejects the change.
    pub fn set_blur(&self, radius: i32) -> skylight::Result<()> {
        for panel in &self.panels {
            panel.window.set_blur_radius(radius)?;
        }
        Ok(())
    }

    /// Moves the bar above or below the system menu bar.
    ///
    /// # Errors
    ///
    /// Returns the window server's error if a window rejects the change.
    pub fn set_level(&self, settings: &Settings) -> skylight::Result<()> {
        for panel in &self.panels {
            panel.window.set_level(bar_level(settings))?;
        }
        Ok(())
    }

    /// Makes the bar take clicks, or let them through.
    ///
    /// Off by default, and turned on only once something wants a click. A bar
    /// that swallows every click in its strip when nothing is listening is a
    /// worse default than one that is invisible to the pointer — `SketchyBar`
    /// is always clickable, but it does not have to be.
    ///
    /// # Errors
    ///
    /// Returns the window server's error if a window rejects the change.
    pub fn set_clickable(&self, clickable: bool) -> skylight::Result<()> {
        let (set, clear) = if clickable {
            (WindowTags::OPAQUE_FOR_EVENTS, WindowTags::IGNORE_FOR_EVENTS)
        } else {
            (WindowTags::IGNORE_FOR_EVENTS, WindowTags::OPAQUE_FOR_EVENTS)
        };
        for panel in &self.panels {
            panel.window.clear_tags(clear)?;
            panel.window.set_tags(set)?;
        }
        Ok(())
    }

    /// # Errors
    ///
    /// Returns the window server's error if a window rejects the change.
    pub fn set_hidden(&self, hidden: bool) -> skylight::Result<()> {
        for panel in &self.panels {
            if hidden {
                panel.window.order_out()?;
            } else {
                panel.window.order_above(None)?;
            }
        }
        Ok(())
    }
}

/// Where the bar sits relative to the system menu bar.
fn bar_level(settings: &Settings) -> std::ffi::c_int {
    if settings.topmost {
        level::STATUS
    } else {
        level::BACKSTOP_MENU
    }
}

fn new_window(
    frame: CGRect,
    display: &Display,
    settings: &Settings,
    clickable: bool,
) -> skylight::Result<Window> {
    let window = Window::new(frame)?;
    window.set_scale(display.scale)?;
    window.set_opaque(false)?;
    window.set_alpha(1.0)?;
    window.set_level(bar_level(settings))?;
    let pointer = if clickable {
        WindowTags::OPAQUE_FOR_EVENTS
    } else {
        WindowTags::IGNORE_FOR_EVENTS
    };
    window.set_tags(WindowTags::BAR | pointer)?;
    if settings.blur_radius != 0 {
        window.set_blur_radius(settings.blur_radius)?;
    }
    if !settings.hidden {
        window.order_above(None)?;
    }
    Ok(window)
}

/// Fills a rectangle whose corners are rounded, falling back to a plain fill
/// when the radius is zero — building a path for a square corner is wasted work
/// on the repaint path.
pub fn fill_rounded_rect(
    ctx: &objc2_core_graphics::CGContext,
    rect: CGRect,
    radius: f64,
    color: Color,
) {
    use objc2_core_graphics::CGContext;
    CGContext::set_rgb_fill_color(
        Some(ctx),
        color.red(),
        color.green(),
        color.blue(),
        color.alpha(),
    );

    let radius = radius
        .min(rect.size.width / 2.0)
        .min(rect.size.height / 2.0);
    if radius <= 0.0 {
        CGContext::fill_rect(Some(ctx), rect);
        return;
    }

    let (x, y) = (rect.origin.x, rect.origin.y);
    let (w, h) = (rect.size.width, rect.size.height);
    CGContext::begin_path(Some(ctx));
    CGContext::move_to_point(Some(ctx), x + radius, y);
    CGContext::add_arc_to_point(Some(ctx), x + w, y, x + w, y + h, radius);
    CGContext::add_arc_to_point(Some(ctx), x + w, y + h, x, y + h, radius);
    CGContext::add_arc_to_point(Some(ctx), x, y + h, x, y, radius);
    CGContext::add_arc_to_point(Some(ctx), x, y, x + w, y, radius);
    CGContext::close_path(Some(ctx));
    CGContext::fill_path(Some(ctx));
}

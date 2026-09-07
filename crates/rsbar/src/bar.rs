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
        }
    }
}

impl Settings {
    /// Applies a patch, reporting whether anything about the geometry moved —
    /// the caller has to reshape the windows if so.
    pub fn apply(&mut self, patch: &BarPatch) -> Changes {
        let mut changes = Changes::default();
        if let Some(h) = patch.height {
            self.height = h;
            changes.geometry = true;
        }
        if let Some(e) = patch.edge {
            self.edge = e;
            changes.geometry = true;
        }
        if let Some(m) = patch.margin {
            self.margin = m;
            changes.geometry = true;
        }
        if let Some(y) = patch.y_offset {
            self.y_offset = y;
            changes.geometry = true;
        }
        if let Some(c) = patch.color {
            self.color = Color(c);
        }
        if let Some(r) = patch.corner_radius {
            self.corner_radius = r;
        }
        if let Some(r) = patch.blur_radius {
            self.blur_radius = r;
            changes.blur = true;
        }
        if let Some(hidden) = patch.hidden {
            self.hidden = hidden;
            changes.visibility = true;
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
            hidden: self.hidden,
            displays,
        }
    }
}

/// What a patch touched that needs more than a repaint.
#[derive(Default, Debug, Clone, Copy)]
pub struct Changes {
    pub geometry: bool,
    pub blur: bool,
    pub visibility: bool,
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
pub struct Panels(Vec<Panel>);

impl Panels {
    pub fn iter(&self) -> impl Iterator<Item = &Panel> {
        self.0.iter()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
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
            let panel = match self.0.iter().position(|p| p.display.id == display.id) {
                Some(index) => {
                    let mut panel = self.0.remove(index);
                    panel.window.set_frame(frame)?;
                    if (panel.display.scale - display.scale).abs() > f64::EPSILON {
                        panel.window.set_scale(display.scale)?;
                    }
                    panel.display = display;
                    panel.frame = frame;
                    panel
                }
                None => Panel {
                    window: new_window(frame, &display, settings)?,
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
        self.0 = panels;
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
            .0
            .iter()
            .map(|p| settings.frame_for(&p.display))
            .collect();
        skylight::batched(|| -> skylight::Result<()> {
            for (panel, frame) in self.0.iter_mut().zip(frames) {
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
        for panel in &self.0 {
            panel.window.set_blur_radius(radius)?;
        }
        Ok(())
    }

    /// # Errors
    ///
    /// Returns the window server's error if a window rejects the change.
    pub fn set_hidden(&self, hidden: bool) -> skylight::Result<()> {
        for panel in &self.0 {
            if hidden {
                panel.window.order_out()?;
            } else {
                panel.window.order_above(None)?;
            }
        }
        Ok(())
    }
}

fn new_window(frame: CGRect, display: &Display, settings: &Settings) -> skylight::Result<Window> {
    let window = Window::new(frame)?;
    window.set_scale(display.scale)?;
    window.set_opaque(false)?;
    window.set_alpha(1.0)?;
    window.set_level(level::STATUS)?;
    window.set_tags(WindowTags::BAR | WindowTags::IGNORE_FOR_EVENTS)?;
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

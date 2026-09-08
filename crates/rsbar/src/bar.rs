//! The bar's own settings and its windows.
//!
//! Split deliberately: [`Settings`] is plain data and a `Resource`, so change
//! detection covers it; [`Panels`] owns window server handles and is `NonSend`.

use crate::components::DisplayTarget;
use crate::display::{self, Display};
use bevy_ecs::prelude::Resource;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGContext, CGImage};
use rsbar_protocol::style::Color;
use rsbar_protocol::{BarPatch, BarState, Edge};
use skylight::{Window, WindowTags, is_builtin, level};

#[derive(Resource, Debug, Clone, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each is an independent SketchyBar property, not a state machine in disguise"
)]
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
    /// Space before the first item and after the last, which is not the
    /// same as [`Self::margin`]: margin insets the bar from the screen
    /// edge, this insets the items from the bar.
    pub padding_left: f64,
    pub padding_right: f64,
    /// Which displays the bar appears on at all.
    pub display: DisplayTarget,
    /// Whether the bar stays put across a space switch.
    ///
    /// Defaults to `true`: a bar window carries no tags at all until
    /// something applies them, so this default is what a freshly created
    /// window is given at construction (see [`new_window`]) — without it the
    /// bar would vanish the moment a space switch happened.
    pub sticky: bool,
    /// Whether the bar draws over a fullscreen app instead of being covered
    /// by it. Defaults to `true` for the same reason as [`Self::sticky`].
    pub show_in_fullscreen: bool,
    /// Width of the gap the centre-left and centre-right buckets leave
    /// around the notch. Built-in display only — see [`Self::frame_for`].
    pub notch_width: f64,
    /// Extra y-offset applied to the bar's frame. Built-in display only.
    pub notch_offset: f64,
    /// Overrides the bar's own height when greater than zero. Built-in
    /// display only.
    pub notch_display_height: f64,
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
            // `SketchyBar`'s own defaults (`bar_manager_init`), not zero:
            // a config that sets neither expects its items inset from the
            // screen edges, and the reference bar visibly is.
            padding_left: 20.0,
            padding_right: 20.0,
            display: DisplayTarget::All,
            sticky: true,
            show_in_fullscreen: true,
            notch_width: 0.0,
            notch_offset: 0.0,
            notch_display_height: 0.0,
        }
    }
}

impl Settings {
    /// Applies a patch, reporting which pieces of window server work it made
    /// necessary.
    ///
    /// Every arm asks [`Changes`](rsbar_protocol::Changes) what the patch
    /// would *change*, never what it wrote: a `--bar blur_radius=12` on a bar
    /// already blurred to 12 used to insert [`Changes::BLUR`] and provoke a
    /// window server call for nothing, and the same went for every other
    /// non-boolean field here. The flags stay coarser than the fields on
    /// purpose — each one stands for a distinct call the caller then makes,
    /// and several fields can imply the same call.
    pub fn apply(&mut self, patch: &BarPatch) -> Changes {
        use rsbar_protocol::{Boolish, Changes as _};

        let mut changes = Changes::empty();
        if let Some(h) = patch.height.and_then(|h| h.changes(&self.height)) {
            self.height = h;
            changes.insert(Changes::GEOMETRY);
        }
        if let Some(e) = patch.edge.and_then(|e| e.changes(&self.edge)) {
            self.edge = e;
            changes.insert(Changes::GEOMETRY);
        }
        if let Some(m) = patch.margin.and_then(|m| m.changes(&self.margin)) {
            self.margin = m;
            changes.insert(Changes::GEOMETRY);
        }
        if let Some(y) = patch.y_offset.and_then(|y| y.changes(&self.y_offset)) {
            self.y_offset = y;
            changes.insert(Changes::GEOMETRY);
        }
        if let Some(c) = patch.color.and_then(|c| c.changes(&self.color)) {
            self.color = c;
        }
        if let Some(r) = patch
            .corner_radius
            .and_then(|r| r.changes(&self.corner_radius))
        {
            self.corner_radius = r;
        }
        if let Some(r) = patch.blur_radius.and_then(|r| r.changes(&self.blur_radius)) {
            self.blur_radius = r;
            changes.insert(Changes::BLUR);
        }
        if let Some(hidden) = patch.hidden.and_then(|c| c.changes(&Boolish(self.hidden))) {
            self.hidden = hidden.get();
            changes.insert(Changes::VISIBILITY);
        }
        if let Some(topmost) = patch
            .topmost
            .and_then(|c| c.changes(&Boolish(self.topmost)))
        {
            self.topmost = topmost.get();
            changes.insert(Changes::LEVEL);
        }
        if let Some(sticky) = patch.sticky.and_then(|c| c.changes(&Boolish(self.sticky))) {
            self.sticky = sticky.get();
            changes.insert(Changes::STICKY);
        }
        if let Some(show) = patch
            .show_in_fullscreen
            .and_then(|c| c.changes(&Boolish(self.show_in_fullscreen)))
        {
            self.show_in_fullscreen = show.get();
            changes.insert(Changes::FULLSCREEN);
        }
        if let Some(w) = patch.notch_width.and_then(|w| w.changes(&self.notch_width)) {
            // Only the centre-left/centre-right item layout reads this — the
            // panel window's own frame does not change, so this deliberately
            // does not set `Changes::GEOMETRY`. `Settings` being a `ResMut`
            // resource is enough on its own to mark it changed and trigger
            // `layout`'s next full repaint.
            self.notch_width = w;
        }
        if let Some(o) = patch
            .notch_offset
            .and_then(|o| o.changes(&self.notch_offset))
        {
            self.notch_offset = o;
            changes.insert(Changes::GEOMETRY);
        }
        if let Some(h) = patch
            .notch_display_height
            .and_then(|h| h.changes(&self.notch_display_height))
        {
            self.notch_display_height = h;
            changes.insert(Changes::GEOMETRY);
        }
        if let Some(p) = patch
            .padding_left
            .and_then(|p| p.changes(&self.padding_left))
        {
            self.padding_left = p;
        }
        if let Some(p) = patch
            .padding_right
            .and_then(|p| p.changes(&self.padding_right))
        {
            self.padding_right = p;
        }
        if let Some(target) = patch.display.and_then(|d| d.changes(&self.display)) {
            // Rebuilding every panel — tearing down and recreating window
            // server windows — is the most expensive thing on this list, and
            // it is why every field here is now guarded rather than only this
            // one.
            self.display = target;
            changes.insert(Changes::DISPLAYS);
        }
        changes
    }

    /// Where the bar sits on one display.
    ///
    /// `notch_offset` and `notch_display_height` apply only on the built-in
    /// display, matching `SketchyBar`'s own `bar_get_frame` (`bar.c`
    /// lines 437-489) — an external monitor has no notch to make room for.
    #[must_use]
    pub fn frame_for(&self, display: &Display) -> CGRect {
        let bounds = display.bounds;
        let builtin = is_builtin(display.id);
        let notch_offset = if builtin { self.notch_offset } else { 0.0 };
        let height = if builtin && self.notch_display_height > 0.0 {
            self.notch_display_height
        } else {
            self.height
        };
        let y = match self.edge {
            Edge::Top => bounds.origin.y + self.y_offset + notch_offset,
            Edge::Bottom => {
                bounds.origin.y + bounds.size.height - height - self.y_offset - notch_offset
            }
        };
        CGRect::new(
            CGPoint::new(bounds.origin.x + self.margin, y),
            CGSize::new(bounds.size.width - 2.0 * self.margin, height),
        )
    }

    #[must_use]
    pub fn state(&self, displays: usize) -> BarState {
        BarState {
            height: self.height,
            edge: self.edge,
            color: self.color,
            margin: self.margin,
            y_offset: self.y_offset,
            corner_radius: self.corner_radius,
            blur_radius: self.blur_radius,
            topmost: self.topmost.into(),
            hidden: self.hidden.into(),
            displays,
            // Settable and, until now, unreportable: a config could set the
            // paddings and the display target and never read them back.
            padding_left: self.padding_left,
            padding_right: self.padding_right,
            display: self.display,
            sticky: self.sticky.into(),
            show_in_fullscreen: self.show_in_fullscreen.into(),
            notch_width: self.notch_width,
            notch_offset: self.notch_offset,
            notch_display_height: self.notch_display_height,
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
        /// Which displays have a panel at all, as opposed to [`Self::GEOMETRY`]
        /// which only moves panels that already exist.
        const DISPLAYS = 1 << 4;
        /// `sticky` changed — [`Panels::set_sticky`] needs a call.
        const STICKY = 1 << 5;
        /// `show_in_fullscreen` changed — [`Panels::set_show_in_fullscreen`]
        /// needs a call.
        const FULLSCREEN = 1 << 6;
    }
}

/// The bar on one display.
pub struct Panel {
    pub display: Display,
    pub window: Window,
    pub frame: CGRect,
    /// This panel's 1-based position in [`display::active`]'s order — what
    /// an item's or the bar's own `display` property is written against.
    pub ordinal: u32,
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
    /// them — which is also what happens to one `settings.display` no longer
    /// selects: it is simply left out of the rebuilt set.
    ///
    /// # Errors
    ///
    /// Returns the window server's error if a window cannot be created or
    /// configured.
    #[skylight::main_thread]
    pub fn rebuild(&mut self, settings: &Settings) -> skylight::Result<()> {
        let mut panels = Vec::new();
        for (index, display) in display::active(proof.marker()).into_iter().enumerate() {
            let ordinal = u32::try_from(index + 1).unwrap_or(u32::MAX);
            if !settings.display.matches(ordinal) {
                continue;
            }
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
                    panel.ordinal = ordinal;
                    panel
                }
                None => Panel {
                    window: new_window(proof, frame, &display, settings, self.clickable)?,
                    display,
                    frame,
                    ordinal,
                },
            };
            tracing::debug!(
                display = panel.display.id,
                scale = panel.display.scale,
                frame = ?panel.frame,
                ordinal = panel.ordinal,
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
        // The proof comes from a panel this already holds -- a `Window` could
        // not exist here otherwise -- rather than from a check of its own.
        // Read off before the loop borrows the panels back.
        let Some(proof) = self
            .panels
            .first()
            .map(|panel| skylight::MainThreadProof::marker(&panel.window))
        else {
            return Ok(());
        };
        skylight::batched(proof, || -> skylight::Result<()> {
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

    /// Makes the bar stay put across a space switch, or not.
    ///
    /// # Errors
    ///
    /// Returns the window server's error if a window rejects the change.
    pub fn set_sticky(&self, sticky: bool) -> skylight::Result<()> {
        for panel in &self.panels {
            panel.window.set_sticky(sticky)?;
        }
        Ok(())
    }

    /// Makes the bar draw over a fullscreen app, or not.
    ///
    /// # Errors
    ///
    /// Returns the window server's error if a window rejects the change.
    pub fn set_show_in_fullscreen(&self, show: bool) -> skylight::Result<()> {
        for panel in &self.panels {
            panel.window.set_friend_of_fullscreen(show)?;
        }
        Ok(())
    }

    /// How wide one display's bar is, for anything that has to stay on it.
    #[must_use]
    pub fn width_for(&self, display: u32) -> Option<f64> {
        self.panels
            .iter()
            .find(|p| p.display.id == display)
            .map(|p| p.frame.size.width)
    }

    #[must_use]
    pub fn scale_for(&self, display: u32) -> f64 {
        self.panels
            .iter()
            .find(|p| p.display.id == display)
            .map_or(1.0, |p| p.display.scale)
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

#[skylight::main_thread]
fn new_window(
    frame: CGRect,
    display: &Display,
    settings: &Settings,
    clickable: bool,
) -> skylight::Result<Window> {
    // No marker written: the attribute supplies it, checking once on entry.
    // A `Window` is `!Send`, so proving the thread here proves it for the rest
    // of this window's life -- see `skylight::Window`.
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
    // `WindowTags::BAR` deliberately excludes these two — they are options a
    // running bar can flip, not fixed facts about being a bar — so a freshly
    // created window needs them applied explicitly or it vanishes on the
    // first space switch or fullscreen app.
    window.set_sticky(settings.sticky)?;
    window.set_friend_of_fullscreen(settings.show_in_fullscreen)?;
    if settings.blur_radius != 0 {
        window.set_blur_radius(settings.blur_radius)?;
    }
    if !settings.hidden {
        window.order_above(None)?;
    }
    Ok(window)
}

/// Traces a rectangle whose corners are rounded, falling back to a plain
/// rectangle when the radius is zero — building arcs for a square corner is
/// wasted work on the repaint path. Leaves the path open for the caller to
/// fill, stroke, or both.
fn rounded_rect_path(ctx: &objc2_core_graphics::CGContext, rect: CGRect, radius: f64) {
    use objc2_core_graphics::CGContext;
    let radius = radius
        .min(rect.size.width / 2.0)
        .min(rect.size.height / 2.0);
    CGContext::begin_path(Some(ctx));
    if radius <= 0.0 {
        CGContext::add_rect(Some(ctx), rect);
        return;
    }

    let (x, y) = (rect.origin.x, rect.origin.y);
    let (w, h) = (rect.size.width, rect.size.height);
    CGContext::move_to_point(Some(ctx), x + radius, y);
    CGContext::add_arc_to_point(Some(ctx), x + w, y, x + w, y + h, radius);
    CGContext::add_arc_to_point(Some(ctx), x + w, y + h, x, y + h, radius);
    CGContext::add_arc_to_point(Some(ctx), x, y + h, x, y, radius);
    CGContext::add_arc_to_point(Some(ctx), x, y, x + w, y, radius);
    CGContext::close_path(Some(ctx));
}

/// Fills a rectangle whose corners are rounded, falling back to a plain fill
/// when the radius is zero.
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
    rounded_rect_path(ctx, rect, radius);
    CGContext::fill_path(Some(ctx));
}

/// Strokes a rectangle whose corners are rounded, the same path
/// [`fill_rounded_rect`] would fill.
pub fn stroke_rounded_rect(
    ctx: &objc2_core_graphics::CGContext,
    rect: CGRect,
    radius: f64,
    color: Color,
    width: f64,
) {
    use objc2_core_graphics::CGContext;
    CGContext::set_rgb_stroke_color(
        Some(ctx),
        color.red(),
        color.green(),
        color.blue(),
        color.alpha(),
    );
    CGContext::set_line_width(Some(ctx), width);
    rounded_rect_path(ctx, rect, radius);
    CGContext::stroke_path(Some(ctx));
}

/// Draws a captured image, confined to `visible`.
///
/// A captured menu bar item is mostly transparent margin, and `rect` is the
/// whole capture — so without the clip the margin of one alias reaches across
/// its neighbour.
pub fn draw_image_clipped(ctx: &CGContext, rect: CGRect, visible: CGRect, image: &CGImage) {
    CGContext::save_g_state(Some(ctx));
    CGContext::clip_to_rect(Some(ctx), visible);
    draw_image(ctx, rect, image);
    CGContext::restore_g_state(Some(ctx));
}

/// Draws a captured image into a top-left oriented context.
///
/// The context is flipped so that a caller works in the same space as the
/// frames it laid out, and `CGContext::draw_image` takes its rect in the
/// context's own space — so the flip has to be undone around the image, or a
/// mirrored menu bar item comes out upside down.
pub fn draw_image(ctx: &CGContext, rect: CGRect, image: &CGImage) {
    CGContext::save_g_state(Some(ctx));
    CGContext::translate_ctm(Some(ctx), 0.0, rect.origin.y + rect.size.height);
    CGContext::scale_ctm(Some(ctx), 1.0, -1.0);
    let upright = CGRect::new(CGPoint::new(rect.origin.x, 0.0), rect.size);
    CGContext::draw_image(Some(ctx), upright, Some(image));
    CGContext::restore_g_state(Some(ctx));
}

#[cfg(test)]
mod tests {
    use super::{Changes, Settings};
    use rsbar_protocol::{BarPatch, BoolChange, Boolish};

    /// The regression this whole per-field pass exists to prevent: a `--bar`
    /// re-stating what the bar already is must not provoke a single window
    /// server call.
    #[test]
    fn setting_a_property_to_what_it_already_is_implies_no_work() {
        let mut settings = Settings::default();
        let before = settings.clone();
        let patch = BarPatch {
            height: Some(before.height),
            edge: Some(before.edge),
            color: Some(before.color),
            margin: Some(before.margin),
            blur_radius: Some(before.blur_radius),
            corner_radius: Some(before.corner_radius),
            display: Some(before.display),
            hidden: Some(Boolish(before.hidden).into()),
            topmost: Some(Boolish(before.topmost).into()),
            sticky: Some(Boolish(before.sticky).into()),
            show_in_fullscreen: Some(Boolish(before.show_in_fullscreen).into()),
            notch_offset: Some(before.notch_offset),
            ..BarPatch::default()
        };
        assert_eq!(settings.apply(&patch), Changes::empty());
        assert_eq!(settings, before);
    }

    #[test]
    fn a_real_change_implies_only_its_own_work() {
        let mut settings = Settings::default();
        let patch = BarPatch {
            blur_radius: Some(settings.blur_radius + 12),
            ..BarPatch::default()
        };
        assert_eq!(settings.apply(&patch), Changes::BLUR);
    }

    #[test]
    fn a_flip_is_always_work_because_it_cannot_be_a_no_op() {
        let mut settings = Settings::default();
        let patch = BarPatch {
            hidden: Some(BoolChange::Toggle),
            ..BarPatch::default()
        };
        let was = settings.hidden;
        assert_eq!(settings.apply(&patch), Changes::VISIBILITY);
        assert_eq!(settings.hidden, !was);
    }
}

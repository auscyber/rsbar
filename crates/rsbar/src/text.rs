//! Text shaping and drawing, via `CoreText`.
//!
//! A shaped line is expensive relative to a repaint: a bar redraws on a timer
//! but its strings change rarely, so [`Text`] keeps the `CTLine` and rebuilds
//! it only when the string or the font actually changes.

use objc2_core_foundation::{
    CFAttributedString, CFDictionary, CFRetained, CFString, CFType, CGPoint, CGRect, CGSize,
    kCFBooleanTrue,
};
use objc2_core_graphics::CGContext;
use objc2_core_text::{
    CTFont, CTFontDescriptor, CTLine, kCTFontAttributeName, kCTFontFamilyNameAttribute,
    kCTFontStyleNameAttribute, kCTForegroundColorFromContextAttributeName,
};
use rsbar_protocol::style::{Color, FontSpec};
use std::ptr;

/// What a shaped line measures, in points.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Metrics {
    pub width: f64,
    pub ascent: f64,
    pub descent: f64,
}

impl Metrics {
    /// Distance from the top of the line box to the baseline.
    #[must_use]
    pub fn height(self) -> f64 {
        self.ascent + self.descent
    }
}

/// A resolved font. Cheap to clone; the expensive part is the `CTFont`.
#[derive(Clone)]
pub struct Font {
    spec: FontSpec,
    font: CFRetained<CTFont>,
}

// Fonts already looked up, by family, style and size.
//
// Thread local rather than shared: a `CTFont` is `!Send`, and shaping happens
// on whichever thread is doing it — one cache each is correct and needs no
// lock. A bar uses a handful of specs, so this stops growing almost at once.
thread_local! {
    static RESOLVED: std::cell::RefCell<
        std::collections::HashMap<(String, String, u64), CFRetained<CTFont>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

impl Font {
    /// Resolves a spec. Falls back to the system font at the requested size if
    /// the family is not installed — a missing font should cost the user a
    /// wrong typeface, not an empty bar.
    #[must_use]
    pub fn resolve(spec: &FontSpec) -> Self {
        // Resolving is a font lookup — a descriptor, a font, and a family name
        // read back to check `CoreText` did not substitute silently. That was
        // being paid per item per reshape, twice, for the same three specs a
        // bar uses, and measured at about a third of the cost of shaping.
        let key = (spec.family.clone(), spec.style.clone(), spec.size.to_bits());
        let font = RESOLVED.with_borrow_mut(|resolved| {
            if let Some(font) = resolved.get(&key) {
                return font.clone();
            }
            let font = Self::create(&spec.family, &spec.style, spec.size)
                .unwrap_or_else(|| Self::system(spec.size));
            resolved.insert(key, font.clone());
            font
        });
        Self {
            spec: spec.clone(),
            font,
        }
    }

    /// Resolves by family (and, if given, style) *attributes* rather than a
    /// guessed PostScript name.
    ///
    /// `CTFontDescriptorCreateWithNameAndSize` matches a single string against
    /// a font's PostScript/full name, which a config never supplies —
    /// `sketchybar`'s font spec is always family + style, e.g. `"Hack Nerd
    /// Font"` + `"Regular"`. Concatenating those as `"Hack Nerd Font-Regular"`
    /// is not that font's PostScript name (`HackNF-Regular`, measured via
    /// `fc-list`/`system_profiler`), so the old lookup missed and silently
    /// substituted a font with none of the family's glyphs — every nerd-font
    /// icon in the config drew as a tofu box even though the family really was
    /// installed. Matching on `kCTFontFamilyNameAttribute` (+
    /// `kCTFontStyleNameAttribute` when a style is given) is what `NSFont`'s
    /// own family/style API does under the hood, and finds the font by the
    /// same two fields a config actually sets, however its internal
    /// PostScript name is spelled.
    ///
    /// `CoreText` substitutes silently when nothing matches, so the family
    /// that came back is compared against the one asked for.
    fn create(family: &str, style: &str, size: f64) -> Option<CFRetained<CTFont>> {
        let family_key = unsafe { kCTFontFamilyNameAttribute };
        let family_value = CFString::from_str(family);
        let descriptor = if style.is_empty() {
            let keys: [&CFString; 1] = [family_key];
            let values: [&CFType; 1] = [family_value.as_ref()];
            let attributes = CFDictionary::from_slices(&keys, &values);
            unsafe { CTFontDescriptor::with_attributes(attributes.as_opaque()) }
        } else {
            let style_key = unsafe { kCTFontStyleNameAttribute };
            let style_value = CFString::from_str(style);
            let keys: [&CFString; 2] = [family_key, style_key];
            let values: [&CFType; 2] = [family_value.as_ref(), style_value.as_ref()];
            let attributes = CFDictionary::from_slices(&keys, &values);
            unsafe { CTFontDescriptor::with_attributes(attributes.as_opaque()) }
        };
        let font = unsafe { CTFont::with_font_descriptor(&descriptor, size, ptr::null()) };
        unsafe { font.family_name() }
            .to_string()
            .eq_ignore_ascii_case(family)
            .then_some(font)
    }

    fn system(size: f64) -> CFRetained<CTFont> {
        unsafe {
            CTFont::new_ui_font_for_language(objc2_core_text::CTFontUIFontType::System, size, None)
        }
        .unwrap_or_else(|| unsafe {
            CTFont::with_name(&CFString::from_str("Helvetica"), size, ptr::null())
        })
    }

    #[must_use]
    pub fn spec(&self) -> &FontSpec {
        &self.spec
    }
}

/// A string together with its shaped form and measurements.
pub struct Text {
    string: String,
    font: Font,
    line: Option<CFRetained<CTLine>>,
    metrics: Metrics,
}

impl Text {
    #[must_use]
    pub fn new(string: impl Into<String>, font: Font) -> Self {
        let mut text = Self {
            string: string.into(),
            font,
            line: None,
            metrics: Metrics::default(),
        };
        text.shape();
        text
    }

    /// Replaces the string, reshaping only if it actually differs.
    pub fn set_string(&mut self, string: &str) {
        if self.string != string {
            self.string.clear();
            self.string.push_str(string);
            self.shape();
        }
    }

    pub fn set_font(&mut self, font: Font) {
        if self.font.spec() != font.spec() {
            self.font = font;
            self.shape();
        }
    }

    #[must_use]
    pub fn string(&self) -> &str {
        &self.string
    }

    #[must_use]
    pub fn metrics(&self) -> Metrics {
        self.metrics
    }

    fn shape(&mut self) {
        self.line = None;
        self.metrics = Metrics::default();
        if self.string.is_empty() {
            return;
        }

        let keys: [&CFString; 2] = [unsafe { kCTFontAttributeName }, unsafe {
            kCTForegroundColorFromContextAttributeName
        }];
        // Taking the colour from the context, rather than baking it into the
        // attributes, is what lets one shaped line be drawn in any colour.
        let true_value = unsafe { kCFBooleanTrue }.expect("kCFBooleanTrue");
        let values: [&CFType; 2] = [self.font.font.as_ref(), true_value.as_ref()];
        let attributes = CFDictionary::from_slices(&keys, &values);

        let cf_string = CFString::from_str(&self.string);
        let Some(styled) = (unsafe {
            CFAttributedString::new(None, Some(&cf_string), Some(attributes.as_opaque()))
        }) else {
            return;
        };
        let line = unsafe { CTLine::with_attributed_string(&styled) };

        let (mut ascent, mut descent, mut leading) = (0.0, 0.0, 0.0);
        let width =
            unsafe { line.typographic_bounds(&raw mut ascent, &raw mut descent, &raw mut leading) };
        self.metrics = Metrics {
            width,
            ascent,
            descent,
        };
        self.line = Some(line);
    }

    /// Draws the line so that `box_` contains it, vertically centred on the
    /// cap height rather than the line box — text centred on the line box sits
    /// visibly low, because the descent is mostly empty.
    pub fn draw(&self, ctx: &CGContext, box_: CGRect, color: Color) {
        let Some(line) = self.line.as_deref() else {
            return;
        };
        if color.is_invisible() {
            return;
        }

        let baseline = box_.origin.y
            + (box_.size.height - (self.metrics.ascent - self.metrics.descent)) / 2.0
            + self.metrics.ascent;

        CGContext::save_g_state(Some(ctx));
        CGContext::set_rgb_fill_color(
            Some(ctx),
            color.red(),
            color.green(),
            color.blue(),
            color.alpha(),
        );
        // The context is flipped for top-left geometry; text has to be drawn
        // the right way up again or every glyph is mirrored.
        CGContext::translate_ctm(Some(ctx), box_.origin.x, baseline);
        CGContext::scale_ctm(Some(ctx), 1.0, -1.0);
        CGContext::set_text_position(Some(ctx), 0.0, 0.0);
        unsafe { line.draw(ctx) };
        CGContext::restore_g_state(Some(ctx));
    }

    /// The box this text occupies at `origin`, given a line height.
    #[must_use]
    pub fn bounds(&self, origin: CGPoint, height: f64) -> CGRect {
        CGRect::new(origin, CGSize::new(self.metrics.width, height))
    }
}

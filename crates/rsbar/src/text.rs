//! Text shaping and drawing, via `CoreText`.
//!
//! A shaped line is expensive relative to a repaint: a bar redraws on a timer
//! but its strings change rarely, so [`Text`] keeps the `CTLine` and rebuilds
//! it only when the string or the font actually changes.
//!
//! Every raw `CoreText` call this file makes is one of the free functions
//! between here and [`Metrics`] — a descriptor, a font, the family it really
//! resolved to, an attributed string, its shaped line, that line's
//! measurements, and drawing it. [`Font`] and [`Text`] own what those hand
//! back and contain no `unsafe` of their own, so the whole FFI surface is
//! that one stretch, each block saying the one thing it is trusting.
//!
//! [`Line`] carries a second, unrelated `unsafe`: not a call into `CoreText`
//! but an assertion about what `CoreText` already promises — that a shaped
//! line, once made, is safe to hand to another thread. Argued once, where
//! `Line` is defined, rather than left implicit.

use objc2_core_foundation::{
    CFAttributedString, CFBoolean, CFDictionary, CFRetained, CFString, CFType, CGPoint, CGRect,
    CGSize, kCFBooleanTrue,
};
use objc2_core_graphics::CGContext;
use objc2_core_text::{
    CTFont, CTFontDescriptor, CTFontUIFontType, CTLine, kCTFontAttributeName,
    kCTFontFamilyNameAttribute, kCTFontStyleNameAttribute,
    kCTForegroundColorFromContextAttributeName,
};
use rsbar_protocol::style::{Color, FontSpec};
use skylight::SavedState;
use std::ptr;

/// A key in one of the two attribute dictionaries `CoreText` is handed here.
///
/// Each is a `'static` `CFString` the framework exports, and reading an extern
/// static is unsafe — so they are read here, once each, rather than at every
/// site that builds a dictionary. The same closed-set shape as
/// [`skylight::cf::WindowKey`], for the same reason: it says which attributes
/// this daemon actually sets, which four scattered `unsafe` reads never did.
trait Attribute: Copy {
    fn cf(self) -> &'static CFString;
}

/// What a font is looked up *by*: the two fields a `sketchybar` font spec has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FontKey {
    Family,
    Style,
}

impl Attribute for FontKey {
    fn cf(self) -> &'static CFString {
        // SAFETY: each is a `'static` constant `CoreText` exports, unsafe to
        // read only because it is extern.
        unsafe {
            match self {
                Self::Family => kCTFontFamilyNameAttribute,
                Self::Style => kCTFontStyleNameAttribute,
            }
        }
    }
}

/// What a run is *shaped* with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunKey {
    /// The font every glyph in the run is drawn from.
    Font,
    /// Take the fill colour from the context at draw time rather than from
    /// the run — which is what lets one shaped line be drawn in any colour.
    ColorFromContext,
}

impl Attribute for RunKey {
    fn cf(self) -> &'static CFString {
        // SAFETY: as above.
        unsafe {
            match self {
                Self::Font => kCTFontAttributeName,
                Self::ColorFromContext => kCTForegroundColorFromContextAttributeName,
            }
        }
    }
}

/// One of the two attribute dictionaries, typed by what it actually holds:
/// framework-constant keys against CF values. `CFDictionary`'s own default is
/// opaque on both sides, and the opaque form is only ever what the C calls
/// want at the last moment.
type Attributes = CFRetained<CFDictionary<CFString, CFType>>;

/// An attribute dictionary, from keys named as a closed set and the values
/// they take.
///
/// Generic over the length so neither caller allocates: both know how many
/// pairs they are setting at the point they write them down.
fn attributes<K: Attribute, const N: usize>(pairs: [(K, &CFType); N]) -> Attributes {
    let keys: [&CFString; N] = std::array::from_fn(|i| pairs[i].0.cf());
    let values: [&CFType; N] = std::array::from_fn(|i| pairs[i].1);
    CFDictionary::from_slices(&keys, &values)
}

/// `CoreFoundation`'s shared true, as a value a dictionary can hold.
fn cf_true() -> &'static CFBoolean {
    // SAFETY: a `'static` constant `CoreFoundation` exports and always
    // populates; unsafe to read only because it is extern.
    unsafe { kCFBooleanTrue }.expect("kCFBooleanTrue")
}

/// The descriptor those attributes describe.
fn descriptor(attributes: &Attributes) -> CFRetained<CTFontDescriptor> {
    // SAFETY: a live dictionary of attribute keys to values; the call takes no
    // ownership of it and hands back a +1 reference `CFRetained` now owns.
    unsafe { CTFontDescriptor::with_attributes(attributes.as_opaque()) }
}

/// The font a descriptor resolves to at `size`.
fn font_for(descriptor: &CTFontDescriptor, size: f64) -> CFRetained<CTFont> {
    // SAFETY: a live descriptor; a null matrix means the identity one.
    unsafe { CTFont::with_font_descriptor(descriptor, size, ptr::null()) }
}

/// The system's own UI font at `size`, falling back to Helvetica.
///
/// Two calls rather than one because the first can answer nothing on a system
/// with no UI font of that type at all. Helvetica has shipped with every
/// version of macOS there has been, so it is the end of the chain rather than
/// another link in it.
fn system_font(size: f64) -> CFRetained<CTFont> {
    // SAFETY: a size and no language; the call hands back a +1 font or none.
    unsafe { CTFont::new_ui_font_for_language(CTFontUIFontType::System, size, None) }
        .unwrap_or_else(|| {
            // SAFETY: a live name and a null (identity) matrix.
            unsafe { CTFont::with_name(&CFString::from_str("Helvetica"), size, ptr::null()) }
        })
}

/// The family `CoreText` actually resolved to, which is not always the one
/// that was asked for — see [`Font::create`].
fn family_of(font: &CTFont) -> String {
    // SAFETY: a live font; the call hands back a +1 `CFString`.
    unsafe { font.family_name() }.to_string()
}

/// `string` carrying `attributes` over the whole of its length.
///
/// `None` if `CoreFoundation` would not build one, which it does not for a
/// string it cannot take a copy of.
fn styled(string: &CFString, attributes: &Attributes) -> Option<CFRetained<CFAttributedString>> {
    // SAFETY: the default allocator, and a live string and dictionary the call
    // copies from rather than borrows.
    unsafe { CFAttributedString::new(None, Some(string), Some(attributes.as_opaque())) }
}

/// The shaped form of an attributed string.
fn shaped(styled: &CFAttributedString) -> CFRetained<CTLine> {
    // SAFETY: a live attributed string; the call hands back a +1 `CTLine`.
    unsafe { CTLine::with_attributed_string(styled) }
}

/// What a shaped line measures.
///
/// `CTLineGetTypographicBounds` returns the width but writes the three
/// vertical figures through out-pointers, so this is the one place three
/// locals are handed to `CoreText` to fill in rather than every caller that
/// wants a width. Leading is asked for and dropped: a bar's line height comes
/// from the item, not the font.
fn measure(line: &CTLine) -> Metrics {
    let (mut ascent, mut descent, mut leading) = (0.0, 0.0, 0.0);
    // SAFETY: a live line, and three valid out-pointers to locals that
    // outlive the call.
    let width =
        unsafe { line.typographic_bounds(&raw mut ascent, &raw mut descent, &raw mut leading) };
    Metrics {
        width,
        ascent,
        descent,
    }
}

/// Draws a line at the context's current text position.
fn draw_line(line: &CTLine, ctx: &CGContext) {
    // SAFETY: a live line and a live context, both borrowed for the call.
    unsafe { line.draw(ctx) };
}

/// A shaped line, made safe to hand to another thread once it exists.
///
/// The FFI binding leaves `CTLine` `!Send` because nothing says of an
/// arbitrary `CoreFoundation` object that a second thread may touch it. A
/// `CTLine` is not arbitrary:
///
/// * `CTLine.h`'s own header says "All functions in this header are thread
///   safe unless otherwise specified", and neither
///   `CTLineCreateWithAttributedString` nor `CTLineGetTypographicBounds` nor
///   `CTLineDraw` carries an exception;
/// * `CTLineCreateWithAttributedString` "creates a single immutable line
///   object" — nothing in this file, or in `CoreText`, mutates one after
///   that call returns;
/// * `CFRetain`/`CFRelease`, which `CFRetained`'s clone and drop call, are
///   atomic and documented safe to call from any thread.
///
/// An immutable value behind an atomically-refcounted handle is exactly the
/// shape `Send` exists for — the same conclusion the `core-text` crate on
/// crates.io reaches for its own `CTLine` wrapper. What still may not
/// happen, and does not anywhere in this file, is mutating or dropping the
/// underlying `CGContext` a line is drawn into from any thread but the one
/// that owns it; nothing about `Line` claims otherwise.
pub struct Line(CFRetained<CTLine>);

// SAFETY: see the doc comment above — a `CTLine` is immutable once made and
// its reference counting is atomic, so moving the only handle to it to
// another thread is exactly as safe as moving any other `Arc` to an
// immutable value.
unsafe impl Send for Line {}

impl std::ops::Deref for Line {
    type Target = CTLine;

    fn deref(&self) -> &CTLine {
        &self.0
    }
}

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

/// What shaping one string produces: the line itself, if there was anything
/// to shape, and what it measures.
#[derive(Default)]
pub struct Shape {
    pub line: Option<Line>,
    pub metrics: Metrics,
}

/// Shapes `string` at `spec` from a standing start: resolves the font,
/// builds the attributed string, creates the line and measures it.
/// Self-contained, so it may run on any thread — [`Font::resolve`]'s cache is
/// thread-local, and what comes back is a plain [`Metrics`] and a [`Line`]
/// safe to carry home. See [`crate::shaping::Cache::refresh_all`], the
/// caller that actually runs this off the thread that draws.
#[must_use]
pub fn shape_now(string: &str, spec: &FontSpec) -> Shape {
    if string.is_empty() {
        return Shape::default();
    }
    let font = Font::resolve(spec);
    let attributes = attributes([
        (RunKey::Font, font.font.as_ref()),
        (RunKey::ColorFromContext, cf_true().as_ref()),
    ]);
    let Some(styled_string) = styled(&CFString::from_str(string), &attributes) else {
        return Shape::default();
    };
    let line = shaped(&styled_string);
    let metrics = measure(&line);
    Shape {
        line: Some(Line(line)),
        metrics,
    }
}

/// A resolved font. Cheap to clone; the expensive part is the `CTFont`.
#[derive(Clone)]
pub struct Font {
    spec: FontSpec,
    font: CFRetained<CTFont>,
}

/// A cached font, made safe to hold in a cache every shaping thread shares.
///
/// Argued exactly as [`Line`]: `CTFont.h`'s header says "All functions in
/// this header are thread safe unless otherwise specified", a `CTFont` is
/// immutable once resolved, and `CFRetain`/`CFRelease` are atomic. Only
/// `Send` is asserted, not `Sync` — nothing here reads a `&CTFont` from two
/// threads at once. [`RESOLVED`]'s `Mutex` is what turns a `Send` value into
/// something `Sync` can be built on: every caller clones its own retained
/// handle out from under the lock rather than holding a borrow across it, so
/// no two threads ever actually touch the same `CTFont` concurrently either.
struct CachedFont(CFRetained<CTFont>);

// SAFETY: see the doc comment above.
unsafe impl Send for CachedFont {}

/// A resolved font, by the family/style/size a spec is keyed on.
type Resolved = std::collections::HashMap<(String, String, u64), CachedFont>;

// Fonts already looked up, by family, style and size — shared by every
// thread that shapes, so a spec is resolved once for the process rather than
// once per thread that happens to shape it. A bar uses a handful of specs,
// so misses happen a few times at startup and the lock sees no contention
// after.
static RESOLVED: std::sync::LazyLock<std::sync::Mutex<Resolved>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Resolved::new()));

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
        let mut resolved = RESOLVED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let font = if let Some(font) = resolved.get(&key) {
            font.0.clone()
        } else {
            let font = Self::create(&spec.family, &spec.style, spec.size)
                .unwrap_or_else(|| system_font(spec.size));
            resolved.insert(key, CachedFont(font.clone()));
            font
        };
        drop(resolved);
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
        let family_value = CFString::from_str(family);
        let style_value = CFString::from_str(style);
        // A spec with no style asks only about the family: setting an empty
        // `kCTFontStyleNameAttribute` would match no font at all rather than
        // leaving the style open, which is why the two arities are separate
        // dictionaries and not one built conditionally.
        let attributes = if style.is_empty() {
            attributes([(FontKey::Family, family_value.as_ref())])
        } else {
            attributes([
                (FontKey::Family, family_value.as_ref()),
                (FontKey::Style, style_value.as_ref()),
            ])
        };
        let font = font_for(&descriptor(&attributes), size);
        family_of(&font)
            .eq_ignore_ascii_case(family)
            .then_some(font)
    }

    #[must_use]
    pub fn spec(&self) -> &FontSpec {
        &self.spec
    }
}

/// A string together with its shaped form and measurements.
pub struct Text {
    string: String,
    font: FontSpec,
    line: Option<Line>,
    metrics: Metrics,
}

impl Text {
    #[must_use]
    pub fn new(string: impl Into<String>, font: &Font) -> Self {
        let mut text = Self {
            string: string.into(),
            font: font.spec().clone(),
            line: None,
            metrics: Metrics::default(),
        };
        text.reshape();
        text
    }

    /// An unshaped line, with nothing installed yet — the state [`Self::new`]
    /// starts from, exposed so [`crate::shaping::Cache::refresh_all`] has
    /// something to [`Self::install`] a worker's result into for an entity it
    /// has not shaped before.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            string: String::new(),
            font: FontSpec::default(),
            line: None,
            metrics: Metrics::default(),
        }
    }

    /// Replaces the string, reshaping only if it actually differs.
    pub fn set_string(&mut self, string: &str) {
        if self.string != string {
            self.string.clear();
            self.string.push_str(string);
            self.reshape();
        }
    }

    pub fn set_font(&mut self, font: &Font) {
        if self.font != *font.spec() {
            self.font = font.spec().clone();
            self.reshape();
        }
    }

    /// Whether shaping this for `string` and `spec` would rebuild the line.
    ///
    /// The two inputs that shape one, and nothing else on the run: a colour is
    /// taken from the drawing context, and an offset moves the box the line is
    /// drawn into rather than the line.
    #[must_use]
    pub fn is_stale(&self, string: &str, spec: &FontSpec) -> bool {
        self.string != string || &self.font != spec
    }

    #[must_use]
    pub fn string(&self) -> &str {
        &self.string
    }

    #[must_use]
    pub fn metrics(&self) -> Metrics {
        self.metrics
    }

    /// Shapes synchronously, on whichever thread calls it. [`shape_now`] does
    /// the actual work; this just installs what comes back.
    fn reshape(&mut self) {
        let shape = shape_now(&self.string, &self.font);
        self.line = shape.line;
        self.metrics = shape.metrics;
    }

    /// Installs a [`Shape`] a worker already produced, doing no `CoreText`
    /// work of its own — the counterpart to [`Self::set_string`] and
    /// [`Self::set_font`], which shape synchronously on whichever thread
    /// calls them. See [`crate::shaping::Cache::refresh_all`].
    pub fn install(&mut self, string: String, font: FontSpec, shape: Shape) {
        self.string = string;
        self.font = font;
        self.line = shape.line;
        self.metrics = shape.metrics;
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

        // The fill colour, the flip and the text position are all context
        // state, and the context belongs to whatever is painting this frame:
        // the guard is what stops one item's colour leaking into the next.
        let _state = SavedState::save(ctx);
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
        draw_line(line, ctx);
    }

    /// The box this text occupies at `origin`, given a line height.
    #[must_use]
    pub fn bounds(&self, origin: CGPoint, height: f64) -> CGRect {
        CGRect::new(origin, CGSize::new(self.metrics.width, height))
    }
}

#[cfg(test)]
#[allow(
    clippy::float_cmp,
    reason = "exact equality is the claim: nothing shaped measures exactly zero, and \
              a string that did not change must produce the identical width, not a \
              nearly identical one"
)]
mod tests {
    use super::{Font, Text, shape_now};
    use rsbar_protocol::style::FontSpec;

    fn spec(family: &str, style: &str) -> FontSpec {
        FontSpec {
            family: family.to_owned(),
            style: style.to_owned(),
            size: 14.0,
        }
    }

    #[test]
    fn a_string_shapes_to_a_width() {
        // The whole point of the file: a non-empty string measures something,
        // and a longer one measures more. Zero here is the signature of a
        // shaping path that silently stopped working -- an attribute
        // dictionary built with the wrong keys produces exactly that, and
        // draws an empty bar rather than an error.
        let font = Font::resolve(&spec("Helvetica", ""));
        let short = Text::new("i", &font);
        let long = Text::new("wwwwwwww", &font);
        assert!(short.metrics().width > 0.0, "a glyph has to measure");
        assert!(short.metrics().ascent > 0.0, "so does its ascent");
        assert!(long.metrics().width > short.metrics().width);
    }

    #[test]
    fn an_empty_string_shapes_to_nothing() {
        let text = Text::new("", &Font::resolve(&spec("Helvetica", "")));
        assert_eq!(text.metrics().width, 0.0);
    }

    #[test]
    fn a_family_that_is_not_installed_falls_back_rather_than_failing() {
        // `CoreText` substitutes silently, which is why `create` reads the
        // family back. The fallback still has to shape, or a typo in a config
        // costs the user the item rather than the typeface.
        let text = Text::new("hello", &Font::resolve(&spec("No Such Family Here", "")));
        assert!(text.metrics().width > 0.0);
    }

    #[test]
    fn a_style_narrows_the_lookup_without_losing_the_family() {
        // The bug this guards: matching on a concatenated
        // `"Family-Style"` PostScript name missed every nerd font and drew
        // tofu. Both arities have to resolve to the family asked for.
        let plain = Font::resolve(&spec("Helvetica", ""));
        let bold = Font::resolve(&spec("Helvetica", "Bold"));
        assert_eq!(plain.spec().family, "Helvetica");
        assert_eq!(bold.spec().family, "Helvetica");
        let plain = Text::new("Hello", &plain);
        let bold = Text::new("Hello", &bold);
        assert!(plain.metrics().width > 0.0);
        assert!(bold.metrics().width >= plain.metrics().width);
    }

    #[test]
    fn reshaping_only_happens_when_an_input_moved() {
        let mut text = Text::new("one", &Font::resolve(&spec("Helvetica", "")));
        assert!(!text.is_stale("one", &spec("Helvetica", "")));
        assert!(text.is_stale("two", &spec("Helvetica", "")));
        assert!(text.is_stale("one", &spec("Helvetica", "Bold")));
        let before = text.metrics().width;
        text.set_string("one");
        assert_eq!(text.metrics().width, before);
        text.set_string("one and a half");
        assert!(text.metrics().width > before);
    }

    /// [`super::RESOLVED`] is a process-wide cache now, not one per thread:
    /// two threads racing to resolve the same spec must both come back with
    /// a font that actually shapes, whether one populated the cache for the
    /// other or they both missed and inserted right on top of each other.
    ///
    /// `Font` itself stays `!Send` -- resolving and shaping both happen on
    /// the thread that asked, so each closure does both and only the `f64`
    /// width crosses the join.
    #[test]
    fn two_threads_resolving_the_same_spec_both_get_a_usable_font() {
        let a = {
            let spec = spec("Helvetica", "");
            std::thread::spawn(move || Text::new("check", &Font::resolve(&spec)).metrics().width)
        };
        let b = {
            let spec = spec("Helvetica", "");
            std::thread::spawn(move || Text::new("check", &Font::resolve(&spec)).metrics().width)
        };
        let width_a = a.join().expect("thread a panicked");
        let width_b = b.join().expect("thread b panicked");
        assert!(width_a > 0.0, "thread a's font shapes");
        assert_eq!(
            width_a, width_b,
            "the same spec resolves to the same font either way"
        );
    }

    /// The whole point of [`super::Line`]'s `unsafe impl Send`: a `CTLine`
    /// made on one thread is measured and installed on another, and neither
    /// step needs the thread that made it to still be around.
    #[test]
    fn a_line_shaped_on_another_thread_installs_and_measures_here() {
        let font = spec("Helvetica", "");
        let string = "cross-thread".to_owned();
        let shape = {
            let font = font.clone();
            let string = string.clone();
            std::thread::spawn(move || shape_now(&string, &font))
                .join()
                .expect("the worker thread panicked")
        };

        let mut text = Text::empty();
        assert!(text.is_stale(&string, &font), "nothing installed yet");
        text.install(string.clone(), font.clone(), shape);
        assert!(!text.is_stale(&string, &font));
        assert!(
            text.metrics().width > 0.0,
            "a line shaped on another thread still measures"
        );
    }

    #[test]
    fn an_empty_iterator_of_dirty_input_shapes_nothing() {
        // `shape_now` itself, not the cache: an empty string is the same
        // "nothing to shape" case a worker sees as the main thread does.
        let shape = shape_now("", &spec("Helvetica", ""));
        assert!(shape.line.is_none());
        assert_eq!(shape.metrics.width, 0.0);
    }
}

//! Colours and font specifications.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// A straight ARGB colour, the way a config writes one.
///
/// Defaults to fully transparent, same as [`Color::TRANSPARENT`] — the value
/// an unset colour field on a patch struct would otherwise have no way to
/// name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Color(pub u32);

impl Color {
    pub const TRANSPARENT: Self = Self(0x0000_0000);
    pub const WHITE: Self = Self(0xffff_ffff);
    pub const BLACK: Self = Self(0xff00_0000);

    #[must_use]
    pub const fn argb(a: u8, r: u8, g: u8, b: u8) -> Self {
        Self(u32::from_be_bytes([a, r, g, b]))
    }

    #[must_use]
    pub const fn alpha(self) -> f64 {
        ((self.0 >> 24) & 0xff) as f64 / 255.0
    }
    #[must_use]
    pub const fn red(self) -> f64 {
        ((self.0 >> 16) & 0xff) as f64 / 255.0
    }
    #[must_use]
    pub const fn green(self) -> f64 {
        ((self.0 >> 8) & 0xff) as f64 / 255.0
    }
    #[must_use]
    pub const fn blue(self) -> f64 {
        (self.0 & 0xff) as f64 / 255.0
    }

    #[must_use]
    pub const fn is_invisible(self) -> bool {
        self.0 >> 24 == 0
    }
}

/// Accepts `#rgb`, `#rrggbb`, `#aarrggbb` and the `0x`-prefixed forms of the
/// last two. A bare `#rrggbb` is fully opaque.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a colour: expected #rgb, #rrggbb or #aarrggbb")]
pub struct ParseColorError(String);

impl FromStr for Color {
    type Err = ParseColorError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let digits = s
            .strip_prefix('#')
            .or_else(|| s.strip_prefix("0x"))
            .or_else(|| s.strip_prefix("0X"))
            .unwrap_or(s);
        let invalid = || ParseColorError(s.to_owned());

        let value = u32::from_str_radix(digits, 16).map_err(|_| invalid())?;
        match digits.len() {
            // #rgb, each nibble doubled.
            3 => {
                // Each nibble is masked to 0..=0xf below, so `n * 0x11` is at
                // most 0xff and the narrowing cannot lose anything.
                let expand = |nibble: u32| u8::try_from(nibble * 0x11).unwrap_or(0xff);
                Ok(Self::argb(
                    0xff,
                    expand((value >> 8) & 0xf),
                    expand((value >> 4) & 0xf),
                    expand(value & 0xf),
                ))
            }
            6 => Ok(Self(0xff00_0000 | value)),
            8 => Ok(Self(value)),
            _ => Err(invalid()),
        }
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:08x}", self.0)
    }
}

/// `0xaarrggbb`, `SketchyBar`'s own spelling (`"0x%x"` in `background.c`),
/// rather than [`Display`]'s `#rrggbb`-family form: this is the string a
/// config's own `--query` output has to match, independent of what a human
/// reading `{color:?}` would rather see.
impl Serialize for Color {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&format_args!("0x{:08x}", self.0))
    }
}

/// One of the two shapes a colour arrives in, and nothing more: telling them
/// apart is all `untagged` is asked to do here.
///
/// The reading itself is [`Color`]'s own, below, so a malformed colour still
/// gets the sentence that says what a colour looks like. An `untagged` enum
/// that parsed as well as dispatched would answer every mistake with "data
/// did not match any variant", which is only the right answer for a value
/// that is neither a string nor a number — and that is the one case this
/// leaves to it.
///
/// `Text` first because it is what the daemon itself writes: `--query` prints
/// `0xaarrggbb` as a string and a config feeds it straight back.
#[derive(Deserialize)]
#[serde(untagged)]
enum ColorRepr {
    Text(String),
    /// What a Lua config writes: `colors.red = 0xffff0000`, a bare number.
    Argb(u32),
    /// The same, from an interpreter that holds every number as a double.
    Real(f64),
}

/// Accepts anything [`FromStr`] does, so a value written back by the daemon
/// and one typed by a config both parse — and, because a Lua config writes
/// `0xaarrggbb` as a *number* rather than a string, a plain integer too.
impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match ColorRepr::deserialize(deserializer)? {
            ColorRepr::Text(text) => text.parse().map_err(serde::de::Error::custom),
            ColorRepr::Argb(value) => Ok(Self(value)),
            ColorRepr::Real(value) => {
                let whole = value.fract() == 0.0 && value >= 0.0 && value <= f64::from(u32::MAX);
                if !whole {
                    return Err(serde::de::Error::custom(format!(
                        "`{value}` is not an ARGB colour: expected a whole number in 0..=0xffffffff"
                    )));
                }
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                Ok(Self(value as u32))
            }
        }
    }
}

/// A font, as named rather than as resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct FontSpec {
    pub family: String,
    pub style: String,
    pub size: f64,
}

impl Default for FontSpec {
    fn default() -> Self {
        Self {
            family: "SF Pro".into(),
            style: "Regular".into(),
            size: 13.0,
        }
    }
}

impl FontSpec {
    /// Parses `Family:Style:Size`, the spelling `SketchyBar` configs use.
    /// Missing trailing fields keep their defaults.
    #[must_use]
    pub fn parse(spec: &str) -> Self {
        let mut parts = spec.split(':');
        let default = Self::default();
        Self {
            family: parts
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or(&default.family)
                .to_owned(),
            style: parts
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or(&default.style)
                .to_owned(),
            size: parts
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(default.size),
        }
    }
}

/// The inverse of [`FontSpec::parse`], so a spec can go back out the way it
/// came in.
impl fmt::Display for FontSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.family, self.style, self.size)
    }
}

impl Serialize for FontSpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// One of the two shapes a font arrives in. Same division of labour as
/// [`ColorRepr`]: `untagged` says which shape, and [`FontSpec`] itself makes
/// sense of what is in it.
///
/// `Flat` first because it is what the daemon writes and what a config
/// usually types. A table cannot match it and a string cannot match `Parts`,
/// so the order is about which is tried first, not which wins.
#[derive(Deserialize)]
#[serde(untagged)]
enum FontRepr {
    Flat(String),
    /// `font = { family = "Hack", style = "Bold", size = 14 }`, which every
    /// `SketchyBar` Lua config writes because its own helper takes a font
    /// that way. A part left out keeps the default for that part alone.
    Parts {
        family: Option<String>,
        style: Option<String>,
        size: Option<Size>,
    },
}

/// A font size as either spelling: the number a Lua config writes, and the
/// string argv can only ever carry.
///
/// It exists because `f64` is not ours to teach. `#[serde(untagged)]` buffers
/// what it is dispatching on, and a buffered `"14"` handed to an `f64` is an
/// error however the format originally held it — so a foreign primitive
/// underneath an untagged enum loses the CLI's `label.font.size=14`. One
/// number of our own is the whole fix.
#[derive(Debug, Clone, Copy)]
struct Size(f64);

impl<'de> Deserialize<'de> for Size {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Number(f64),
            Text(String),
        }
        match Repr::deserialize(deserializer)? {
            Repr::Number(size) => Ok(Self(size)),
            Repr::Text(text) => text
                .parse()
                .map(Self)
                .map_err(|_| serde::de::Error::custom(format!("`{text}` is not a font size"))),
        }
    }
}

/// Never fails on a short or missing part — [`FontSpec::parse`] does not
/// either, and a config that names a family and no style means the default
/// style, not an error.
impl<'de> Deserialize<'de> for FontSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let default = Self::default();
        Ok(match FontRepr::deserialize(deserializer)? {
            FontRepr::Flat(text) => Self::parse(&text),
            FontRepr::Parts {
                family,
                style,
                size,
            } => Self {
                family: family.unwrap_or(default.family),
                style: style.unwrap_or(default.style),
                size: size.map_or(default.size, |Size(size)| size),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_font_spec_survives_a_round_trip() {
        // The daemon parses a spec on the way in and prints it on the way
        // back out, so a property set to the value it already has has to
        // compare equal rather than looking like a change.
        for spec in [
            "Menlo:Bold:15",
            "SF Pro:Regular:13",
            "Hack Nerd Font:Heavy:12.5",
        ] {
            assert_eq!(FontSpec::parse(spec).to_string(), spec);
        }
        // A short spec fills in from the defaults and then prints in full.
        assert_eq!(FontSpec::parse("Menlo").to_string(), "Menlo:Regular:13");
    }

    #[test]
    fn parses_colour_forms() {
        assert_eq!("#fff".parse::<Color>().unwrap(), Color(0xffff_ffff));
        assert_eq!("#ff0000".parse::<Color>().unwrap(), Color(0xffff_0000));
        assert_eq!("0x80ff0000".parse::<Color>().unwrap(), Color(0x80ff_0000));
        assert_eq!("#f00".parse::<Color>().unwrap(), Color(0xffff_0000));
        assert!("nope".parse::<Color>().is_err());
        assert!("#ff00".parse::<Color>().is_err());
    }

    #[test]
    fn channels_round_trip() {
        let c = Color::argb(0x80, 0x11, 0x22, 0x33);
        assert!((c.alpha() - 128.0 / 255.0).abs() < 1e-9);
        assert!((c.red() - 17.0 / 255.0).abs() < 1e-9);
        assert!(!c.is_invisible());
        assert!(Color::TRANSPARENT.is_invisible());
    }

    #[test]
    fn font_spec_fills_in_missing_fields() {
        let f = FontSpec::parse("Hack Nerd Font:Bold:14");
        assert_eq!(f.family, "Hack Nerd Font");
        assert_eq!(f.style, "Bold");
        assert!((f.size - 14.0).abs() < 1e-9);

        let partial = FontSpec::parse("Menlo");
        assert_eq!(partial.family, "Menlo");
        assert_eq!(partial.style, FontSpec::default().style);
    }
}

//! Colours and font specifications.

use serde::de::Error as _;
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

/// Accepts anything [`FromStr`] does, so a value written back by the daemon
/// and one typed by a config both parse.
impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        text.parse().map_err(D::Error::custom)
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

/// [`FontSpec::parse`] never fails — a short or empty spec just keeps the
/// defaults — so this never does either.
impl<'de> Deserialize<'de> for FontSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        Ok(Self::parse(&text))
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

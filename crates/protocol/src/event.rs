//! What happened, and what it carries.
//!
//! Both come out of one macro. Declaring an event in two places — a kind here
//! and a payload there — is how they drift, and how a payload ends up as
//! `Option<String>` that every script re-parses.
//!
//! The macro produces three things per declaration: a payload struct, a variant
//! of [`Event`] carrying it, and a variant of [`Kind`] without it. [`Kind`] is
//! what an item subscribes to — a subscription names an event, it does not
//! carry one — and [`Event`] is what a source emits.

use crate::Json;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

/// Where the machine is drawing power from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerSource {
    Ac,
    Battery,
    /// What a machine reports before anything has asked, and what a failed
    /// query returns.
    #[default]
    Unknown,
}

impl fmt::Display for PowerSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The spelling a script matches on, so it is part of the interface.
        f.write_str(match self {
            Self::Ac => "AC",
            Self::Battery => "BATTERY",
            Self::Unknown => "UNKNOWN",
        })
    }
}

/// Which mouse button was used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Other,
}

impl fmt::Display for MouseButton {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Other => "other",
        })
    }
}

bitflags::bitflags! {
    /// The modifier keys held during a click or scroll.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Modifiers: u8 {
        const SHIFT = 1 << 0;
        const CTRL  = 1 << 1;
        const ALT   = 1 << 2;
        const CMD   = 1 << 3;
        const FN    = 1 << 4;
    }
}

/// Serialised as its bits rather than bitflags' own string form: postcard is
/// the wire format, and a byte beats a list of names.
impl Serialize for Modifiers {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.bits().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Modifiers {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Truncating rather than failing: an unknown bit from a newer client is
        // a modifier we do not model, not a corrupt message.
        Ok(Self::from_bits_truncate(u8::deserialize(deserializer)?))
    }
}

impl fmt::Display for Modifiers {
    /// A comma-separated list, or `none`. A script splits on commas rather than
    /// decoding a bitmask.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("none");
        }
        let names = [
            (Self::SHIFT, "shift"),
            (Self::CTRL, "ctrl"),
            (Self::ALT, "alt"),
            (Self::CMD, "cmd"),
            (Self::FN, "fn"),
        ];
        let held: Vec<&str> = names
            .iter()
            .filter(|(flag, _)| self.contains(*flag))
            .map(|(_, name)| *name)
            .collect();
        f.write_str(&held.join(","))
    }
}

/// A field a payload may not have a value for, printed as the value or as
/// nothing at all.
///
/// A script tests it with `[ -z "$RSBAR_CHARGE" ]` rather than against a
/// sentinel, which is the shape a shell already has for "unset". A newtype
/// because [`events!`] calls `to_string` on every field, and `Display` cannot
/// be implemented for `Option<T>` from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Maybe<T>(pub Option<T>);

impl<T> From<Option<T>> for Maybe<T> {
    fn from(value: Option<T>) -> Self {
        Self(value)
    }
}

impl<T: fmt::Display> fmt::Display for Maybe<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Some(value) => write!(f, "{value}"),
            None => Ok(()),
        }
    }
}

/// Declares the built-in events.
///
/// `Variant = "name" => Payload { field: Type }` gives a payload struct, an
/// `Event::Variant(Payload)`, and a `Kind::Variant` named `"name"`.
///
/// Every field also becomes an environment variable for scripts, named
/// `RSBAR_` plus the field in upper case. That is why the fields are named
/// what a config would call them.
macro_rules! events {
    (
        $(
            $variant:ident = $name:literal => $data:ident {
                $( $(#[$field_meta:meta])* $field:ident : $ty:ty ),* $(,)?
            }
        ),* $(,)?
    ) => {
        $(
            #[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
            pub struct $data {
                $( $(#[$field_meta])* pub $field: $ty, )*
            }

            impl $data {
                /// This payload's fields, as a script sees them.
                // Built by inserting because the field list is a macro
                // repetition; a map literal cannot be written for one.
                #[allow(unused_mut)]
                #[must_use]
                pub fn fields(&self) -> BTreeMap<String, String> {
                    let mut fields = BTreeMap::new();
                    $( fields.insert(stringify!($field).to_owned(), self.$field.to_string()); )*
                    fields
                }
            }
        )*

        /// Something that happened, with what it carries.
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        pub enum Event {
            $( $variant($data), )*
            /// An event a config invented and triggers itself.
            Custom(Custom),
        }

        /// What an item subscribes to: an event without its payload.
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub enum Kind {
            $( $variant, )*
            Custom(String),
        }

        impl Event {
            /// What this is, for matching a subscription against.
            #[must_use]
            pub fn kind(&self) -> Kind {
                match self {
                    $( Self::$variant(_) => Kind::$variant, )*
                    Self::Custom(custom) => Kind::Custom(custom.name.clone()),
                }
            }

            /// The payload's fields, as a script sees them.
            #[must_use]
            pub fn fields(&self) -> BTreeMap<String, String> {
                match self {
                    $( Self::$variant(data) => data.fields(), )*
                    Self::Custom(custom) => custom.fields(),
                }
            }
        }

        impl Kind {
            /// The name a config writes, and a script reads in `RSBAR_SENDER`.
            #[must_use]
            pub fn name(&self) -> &str {
                match self {
                    $( Self::$variant => $name, )*
                    Self::Custom(name) => name,
                }
            }

            /// Every built-in, for validating a subscription and for `--help`.
            #[must_use]
            pub fn built_in() -> Vec<Kind> {
                vec![ $( Kind::$variant, )* ]
            }

            /// Whether `event` is one of these.
            ///
            /// Generated alongside the variants so a new event cannot be added
            /// without the match arm that recognises it. Compares directly
            /// rather than going through [`Event::kind`], which would allocate
            /// a name for every custom event on every dispatch.
            #[must_use]
            pub fn matches(&self, event: &Event) -> bool {
                match (self, event) {
                    $( (Self::$variant, Event::$variant(_)) => true, )*
                    (Self::Custom(name), Event::Custom(custom)) => *name == custom.name,
                    _ => false,
                }
            }

            /// An event of this kind carrying nothing.
            ///
            /// What `--trigger` produces: a client naming an event knows the
            /// name, not the payload the source would have filled in.
            #[must_use]
            pub fn into_event(self) -> Event {
                match self {
                    $( Self::$variant => Event::$variant($data::default()), )*
                    Self::Custom(name) => {
                        Event::Custom(Custom { name, data: Json::Null })
                    }
                }
            }
        }
    };
}

events! {
    Routine = "routine" => Routine {},
    Forced = "forced" => Forced {},
    FrontAppSwitched = "front_app_switched" => FrontApp { app: String },
    SpaceChanged = "space_changed" => SpaceChange { display: u32, space: u64 },
    DisplayChanged = "display_changed" => DisplayChange {},
    SystemWoke = "system_woke" => SystemWoke {},
    SystemWillSleep = "system_will_sleep" => SystemWillSleep {},
    VolumeChanged = "volume_changed" => VolumeChange { volume: u8 },
    BrightnessChanged = "brightness_changed" => BrightnessChange { brightness: u8 },
    PowerSourceChanged = "power_source_changed" => PowerChange {
        power_source: PowerSource,
        /// What the adapter reports it can supply. Empty on battery, and on
        /// an adapter that does not say.
        watts: Maybe<u32>,
        /// 0-100. Empty on hardware with no battery at all.
        charge: Maybe<u8>,
        charging: Maybe<bool>,
        /// Empty unless discharging with an estimate settled.
        time_to_empty_minutes: Maybe<u32>,
        /// Empty unless charging with an estimate settled.
        time_to_full_minutes: Maybe<u32>,
    },
    WifiChanged = "wifi_changed" => WifiChange { ssid: String },
    MediaChanged = "media_changed" => MediaChange { media: Json },
    SpaceWindowsChanged = "space_windows_changed" => SpaceWindowsChange { space: u64 },
    ConfigReloaded = "config_reloaded" => ConfigReload {},

    // The pointer. `.global` fires for the bar as a whole rather than for one
    // item, which is how a config reacts to the empty space between items.
    MouseEntered = "mouse.entered" => MouseEnter {},
    MouseExited = "mouse.exited" => MouseExit {},
    MouseEnteredGlobal = "mouse.entered.global" => MouseEnterGlobal {},
    MouseExitedGlobal = "mouse.exited.global" => MouseExitGlobal {},
    // `x` and `y` are where it happened, in global screen coordinates. The
    // daemon routes on them, and a script gets them for free.
    MouseClicked = "mouse.clicked" => MouseClick {
        button: MouseButton,
        modifiers: Modifiers,
        x: f64,
        y: f64,
    },
    MouseScrolled = "mouse.scrolled" => Scroll {
        scroll_delta: f64,
        modifiers: Modifiers,
        x: f64,
        y: f64,
    },
    MouseScrolledGlobal = "mouse.scrolled.global" => ScrollGlobal {
        scroll_delta: f64,
        modifiers: Modifiers,
        x: f64,
        y: f64,
    },
}

/// An event a config invented. Its payload is whatever the trigger passed.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Custom {
    pub name: String,
    pub data: Json,
}

impl Custom {
    fn fields(&self) -> BTreeMap<String, String> {
        match &self.data {
            Json::Null => BTreeMap::new(),
            data => BTreeMap::from([("data".to_owned(), data.to_string())]),
        }
    }
}

impl Event {
    /// The environment a script is handed, beyond `RSBAR_NAME`.
    ///
    /// `RSBAR_SENDER` is the event's name. `RSBAR_INFO` is the payload as one
    /// value — the field itself when there is exactly one, a JSON object when
    /// there are several, empty when there are none — which is what a config
    /// already reaches for. Every field also arrives under its own name, so a
    /// script that wants the volume can read `RSBAR_VOLUME` instead of parsing.
    #[must_use]
    pub fn env(&self) -> BTreeMap<String, String> {
        let fields = self.fields();
        let info = {
            let mut values = fields.values();
            match (values.next(), values.next()) {
                (None, _) => String::new(),
                (Some(only), None) => only.clone(),
                _ => Json::Object(
                    fields
                        .iter()
                        .map(|(k, v)| (k.clone(), Json::String(v.clone())))
                        .collect(),
                )
                .to_string(),
            }
        };

        // Under both spellings: a config's plugin scripts are written against
        // `SketchyBar`, which passes `SENDER`/`INFO` unprefixed, and the whole
        // point of matching its CLI is that those scripts run unchanged. The
        // `RSBAR_` names stay because a bare `NAME` in the environment is easy
        // for something else to have set, and a script that wants to be sure
        // can ask for the one nothing else uses.
        let mut env = BTreeMap::from([
            ("SENDER".to_owned(), self.kind().name().to_owned()),
            ("RSBAR_SENDER".to_owned(), self.kind().name().to_owned()),
            ("INFO".to_owned(), info.clone()),
            ("RSBAR_INFO".to_owned(), info),
        ]);
        env.extend(fields.into_iter().flat_map(|(name, value)| {
            let upper = name.to_uppercase();
            [
                (upper.clone(), value.clone()),
                (format!("RSBAR_{upper}"), value),
            ]
        }));
        env
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not an event name")]
pub struct InvalidEvent(String);

impl FromStr for Kind {
    type Err = InvalidEvent;

    /// Accepts `-` as well as `_`, since a CLI reads better with dashes.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalised = s.replace('-', "_").to_ascii_lowercase();
        if let Some(kind) = Self::built_in()
            .into_iter()
            .find(|k| k.name() == normalised)
        {
            return Ok(kind);
        }
        // A custom event has to be nameable by the same rules, or a typo in a
        // built-in name would silently become a custom event nobody triggers.
        if normalised.is_empty()
            || !normalised
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.'))
        {
            return Err(InvalidEvent(s.to_owned()));
        }
        Ok(Self::Custom(normalised))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_names_round_trip() {
        for kind in Kind::built_in() {
            assert_eq!(kind.name().parse::<Kind>().as_ref(), Ok(&kind));
        }
    }

    #[test]
    fn dashes_and_case_are_accepted() {
        assert_eq!("front-app-switched".parse(), Ok(Kind::FrontAppSwitched));
        assert_eq!("SYSTEM_WOKE".parse(), Ok(Kind::SystemWoke));
    }

    #[test]
    fn custom_events_are_still_validated() {
        assert_eq!("my.event".parse(), Ok(Kind::Custom("my.event".into())));
        assert!("has space".parse::<Kind>().is_err());
        assert!("".parse::<Kind>().is_err());
    }

    #[test]
    fn an_event_knows_its_own_kind() {
        let event = Event::VolumeChanged(VolumeChange { volume: 42 });
        assert_eq!(event.kind(), Kind::VolumeChanged);
    }

    #[test]
    fn a_single_field_becomes_info_directly() {
        let env = Event::VolumeChanged(VolumeChange { volume: 42 }).env();
        assert_eq!(env["RSBAR_SENDER"], "volume_changed");
        assert_eq!(env["RSBAR_INFO"], "42");
        assert_eq!(env["RSBAR_VOLUME"], "42");
    }

    #[test]
    fn several_fields_become_a_json_object_and_named_variables() {
        let env = Event::SpaceChanged(SpaceChange {
            display: 2,
            space: 7,
        })
        .env();
        assert_eq!(env["RSBAR_DISPLAY"], "2");
        assert_eq!(env["RSBAR_SPACE"], "7");
        let info = &env["RSBAR_INFO"];
        assert!(
            info.contains("\"display\""),
            "several fields render as an object: {info}"
        );
    }

    #[test]
    fn an_empty_payload_still_sets_info_so_a_script_never_sees_it_unset() {
        let env = Event::SystemWoke(SystemWoke {}).env();
        assert_eq!(env["RSBAR_INFO"], "");
    }

    #[test]
    fn a_custom_event_carries_its_own_name_and_data() {
        let event = Event::Custom(Custom {
            name: "my.event".into(),
            data: Json::parse_or_string("hi"),
        });
        assert_eq!(event.kind(), Kind::Custom("my.event".into()));
        let env = event.env();
        assert_eq!(env["RSBAR_SENDER"], "my.event");
        assert_eq!(env["RSBAR_INFO"], "hi");
    }

    #[test]
    fn a_tag_matches_only_its_own_event() {
        let volume = Event::VolumeChanged(VolumeChange { volume: 42 });
        assert!(Kind::VolumeChanged.matches(&volume));
        assert!(!Kind::SystemWoke.matches(&volume));
    }

    #[test]
    fn custom_tags_match_by_name() {
        let mine = Event::Custom(Custom {
            name: "mine".into(),
            data: Json::Null,
        });
        assert!(Kind::Custom("mine".into()).matches(&mine));
        assert!(!Kind::Custom("yours".into()).matches(&mine));
        assert!(!Kind::VolumeChanged.matches(&mine));
    }

    #[test]
    fn matching_agrees_with_the_tag_an_event_reports() {
        // The two must not be able to disagree.
        for kind in Kind::built_in() {
            let event = kind.clone().into_event();
            assert_eq!(event.kind(), kind);
            assert!(kind.matches(&event));
        }
    }

    #[test]
    fn mouse_events_keep_the_dotted_names_a_config_writes() {
        assert_eq!("mouse.clicked".parse(), Ok(Kind::MouseClicked));
        assert_eq!(
            "mouse.scrolled.global".parse(),
            Ok(Kind::MouseScrolledGlobal)
        );
        assert_eq!(Kind::MouseEnteredGlobal.name(), "mouse.entered.global");
    }

    #[test]
    fn a_click_reports_its_button_and_modifiers_by_name() {
        let click = Event::MouseClicked(MouseClick {
            button: MouseButton::Right,
            modifiers: Modifiers::CMD | Modifiers::SHIFT,
            x: 100.0,
            y: 8.0,
        });
        let env = click.env();
        assert_eq!(env["RSBAR_BUTTON"], "right");
        assert_eq!(env["RSBAR_MODIFIERS"], "shift,cmd");
    }

    #[test]
    fn no_modifiers_reads_as_none_rather_than_empty() {
        // A script comparing against "" would be a trap.
        assert_eq!(Modifiers::empty().to_string(), "none");
    }

    #[test]
    fn events_round_trip_through_the_wire_format() {
        let event = Event::SpaceChanged(SpaceChange {
            display: 1,
            space: 9,
        });
        let bytes = postcard::to_allocvec(&event).unwrap();
        assert_eq!(postcard::from_bytes::<Event>(&bytes).unwrap(), event);
    }
}

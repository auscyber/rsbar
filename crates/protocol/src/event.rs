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
use coolabah_protocol_macros::{EnvFields, Spelling, events};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

/// Where the machine is drawing power from, and what only that side of it
/// can say.
///
/// Determinate rather than a tag beside a pile of optionals. Minutes-to-full
/// only means anything on mains and minutes-to-empty only means anything on
/// battery; the old shape let one payload carry both, or neither, with
/// nothing saying which was a real reading and which was just absent. Here a
/// value that cannot exist cannot be spelled.
///
/// Scripts still see every one of these as its own variable — that is what
/// `#[derive(EnvFields)]` projects, and what keeps a determinate Rust type and
/// a flat shell environment from being a choice between two.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, EnvFields, Spelling,
)]
#[serde(rename_all = "snake_case")]
pub enum PowerSource {
    /// On mains.
    ///
    /// The spelling is what a script matches on, so it is part of the
    /// interface rather than a debug rendering.
    #[spell("AC")]
    Ac {
        /// What the adapter reports it can supply — a nameplate rating, not
        /// a measurement, and not every adapter reports one.
        adapter_watts: Option<u32>,
        /// Whether the battery is actually taking charge. A full battery on
        /// mains is not.
        charging: bool,
        /// Minutes to full, once charging with an estimate settled.
        time_to_full_minutes: Option<u32>,
    },
    /// On battery, and so discharging by definition -- which is why
    /// `charging` is spelled out here rather than left empty: a script should
    /// not have to read the power source back to interpret a hole.
    #[env(charging = false)]
    #[spell("BATTERY")]
    Battery {
        /// Minutes to empty, once the estimate has settled.
        time_to_empty_minutes: Option<u32>,
    },
    /// What a machine reports before anything has asked, and what a failed
    /// query returns.
    #[default]
    #[spell("UNKNOWN")]
    Unknown,
}

/// Which mouse button was used.
///
/// Printed only: nothing parses one back, so the spelling table drives
/// [`Display`](fmt::Display) alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Spelling)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    #[default]
    #[spell("left")]
    Left,
    #[spell("right")]
    Right,
    #[spell("other")]
    Other,
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

/// Serialised as its bits rather than bitflags' own string form: a modifier set
/// is read by machines on both ends, and an integer beats a list of names.
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

/// How one leaf value reaches a script, under a name of its own.
///
/// `fields()` used to call `to_string` on everything, which forced an absent
/// value to be a newtype with a `Display` of its own — `Display` cannot be
/// implemented for `Option<T>` from here, and a wire type is not the place to
/// invent a Haskell. A trait of this crate's own can be, so the fields are
/// plain `Option`s and "absent" is the empty string a shell already tests
/// with `[ -z "$CHARGE" ]`.
///
/// No blanket impl over `Display`: it would collide with the one for
/// `Option<T>`, and the set of types a payload field can be is small, known
/// and worth naming.
/// Only the leaf: what a nested value projects up beyond its own name is
/// [`EnvFields`]'s business, and derived rather than written by hand.
pub trait Field {
    /// What a script reads under this field's own name.
    fn to_field(&self) -> String;
}

macro_rules! field_via_display {
    ($($ty:ty),* $(,)?) => {
        $(
            impl Field for $ty {
                fn to_field(&self) -> String {
                    self.to_string()
                }
            }
        )*
    };
}

field_via_display!(
    String,
    bool,
    u8,
    u32,
    u64,
    f64,
    MouseButton,
    Modifiers,
    PowerSource,
    Json
);

/// An absent value is the empty string, which is the shape a shell already
/// has for "unset".
impl<T: fmt::Display> Field for Option<T> {
    fn to_field(&self) -> String {
        match self {
            Some(value) => value.to_string(),
            None => String::new(),
        }
    }
}

/// A payload's script environment, worked out from its fields.
///
/// Derived rather than written: the upper-casing happens once per field, in
/// the proc macro, and a struct outside the [`events!`] list gets the same
/// treatment by asking for it.
pub trait EnvFields {
    /// This payload's fields, as a script sees them.
    #[must_use]
    fn fields(&self) -> BTreeMap<String, String>;

    /// The same, under the names a script's environment uses.
    ///
    /// Only a field that expands into further names -- [`PowerSource`] -- has
    /// names the derive cannot know, and only those are owned.
    #[must_use]
    fn env_fields(&self) -> BTreeMap<Cow<'static, str>, String>;
}

/// As the name a config writes, not as a variant map -- that name is the
/// whole vocabulary a client and this daemon share.
///
/// Only the unscoped form has a wire representation. A `Kind<Entity>` names an
/// entity in one `World`, which means nothing in another process, so there is
/// deliberately no way to send one.
impl Serialize for Kind<()> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self.name())
    }
}

impl<'de> Deserialize<'de> for Kind<()> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <Cow<'de, str>>::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

events! {
    Routine = "routine" => Routine {},
    Forced = "forced" => Forced {},
    FrontAppSwitched = "front_app_switched" => FrontApp { app: String },
    /// An application finishing launch.
    ///
    /// The daemon's own reason for it is aliases: an alias naming an
    /// application that is not running cannot resolve, and this is the
    /// moment it can. A config can subscribe to it like any other.
    AppLaunched = "app_launched" => AppLaunch { app: String },
    SpaceChanged = "space_changed" => SpaceChange { display: u32, space: u64 },
    DisplayChanged = "display_changed" => DisplayChange {},
    SystemWoke = "system_woke" => SystemWoke {},
    SystemWillSleep = "system_will_sleep" => SystemWillSleep {},
    VolumeChanged = "volume_changed" => VolumeChange { volume: u8 },
    BrightnessChanged = "brightness_changed" => BrightnessChange { brightness: u8 },
    PowerSourceChanged = "power_source_changed" => PowerChange {
        /// Mains or battery — and, inside it, the numbers only that side has.
        /// It projects `ADAPTER_WATTS`, `CHARGING` and the two estimates up
        /// alongside `POWER_SOURCE`.
        #[env(flatten)]
        power_source: PowerSource,
        /// Power actually moving through the battery right now, in whole
        /// watts, whichever way it is going — `charging` says which, so this
        /// is a magnitude and never carries a sign. Empty on hardware with no
        /// battery. Legitimately `0` on a full battery on mains.
        watts: Option<u32>,
        /// 0-100. Empty on hardware with no battery at all.
        charge: Option<u8>,
    },
    WifiChanged = "wifi_changed" => WifiChange { ssid: String },
    MediaChanged = "media_changed" => MediaChange { media: Json },
    SpaceWindowsChanged = "space_windows_changed" => SpaceWindowsChange { space: u64 },
    ConfigReloaded = "config_reloaded" => ConfigReload {},
    /// The Accessibility grant arriving, or being taken away again.
    ///
    /// The system's own prompt says to restart the application, and this
    /// event is what makes that unnecessary: whatever gave up while untrusted
    /// -- an alias whose real owner could not be recovered, a menu that could
    /// not be listed -- gets told the moment the box is ticked.
    AccessibilityChanged = "accessibility_changed" => AccessibilityChange { trusted: bool },

    // The pointer. `.global` fires for the bar as a whole rather than for one
    // item, which is how a config reacts to the empty space between items —
    // and is exactly why the `.global` twin of each is never `@scoped`.
    MouseEntered = "mouse.entered" @scoped => MouseEnter {},
    MouseExited = "mouse.exited" @scoped => MouseExit {},
    MouseEnteredGlobal = "mouse.entered.global" => MouseEnterGlobal {},
    MouseExitedGlobal = "mouse.exited.global" => MouseExitGlobal {},
    // `x` and `y` are where it happened, in global screen coordinates. The
    // daemon routes on them, and a script gets them for free.
    MouseClicked = "mouse.clicked" @scoped => MouseClick {
        button: MouseButton,
        modifiers: Modifiers,
        x: f64,
        y: f64,
    },
    MouseScrolled = "mouse.scrolled" @scoped => Scroll {
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
/// The name of an event a config invented.
///
/// Not a bare `String`. Three commands spell an event name -- `--add event`,
/// `--trigger` and `--subscribe` -- and the failure that matters is a typo in
/// a built-in name quietly becoming a custom event nobody ever fires. Parsing
/// through [`Kind`] rules that out: a name that is a built-in comes back as
/// the built-in, and only what is left can be one of these.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EventName(String);

impl EventName {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The subscription this name means.
    #[must_use]
    pub fn kind(&self) -> Kind {
        Kind::Custom(self.0.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidEventName {
    #[error(transparent)]
    NotAName(#[from] InvalidEvent),
    #[error("`{0}` is a built-in event; only a name coolabah does not already define can be added")]
    BuiltIn(String),
}

impl FromStr for EventName {
    type Err = InvalidEventName;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.parse::<Kind>()? {
            Kind::Custom(name) => Ok(Self(name)),
            built_in => Err(InvalidEventName::BuiltIn(built_in.name().to_owned())),
        }
    }
}

impl TryFrom<String> for EventName {
    type Error = InvalidEventName;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<EventName> for String {
    fn from(name: EventName) -> Self {
        name.0
    }
}

impl fmt::Display for EventName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A `NSDistributedNotificationCenter` notification to bridge an event from.
///
/// Distributed notification names are another process's business entirely --
/// `com.apple.dock.prefchanged`, a bundle identifier, anything a poster chose
/// -- so this validates only what would make the name unusable rather than
/// pretending to know the space: it must be non-empty and free of control
/// characters, which is exactly what [`crate::ItemName`] rules out for the
/// same reason.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NotificationName(String);

impl NotificationName {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a notification name")]
pub struct InvalidNotificationName(String);

impl FromStr for NotificationName {
    type Err = InvalidNotificationName;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() || s.chars().any(char::is_control) {
            return Err(InvalidNotificationName(s.to_owned()));
        }
        Ok(Self(s.to_owned()))
    }
}

impl TryFrom<String> for NotificationName {
    type Error = InvalidNotificationName;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<NotificationName> for String {
    fn from(name: NotificationName) -> Self {
        name.0
    }
}

impl fmt::Display for NotificationName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An event a config invented, and whatever it was triggered with.
///
/// A name and a map, rather than a name and an opaque blob: `--trigger demo
/// VAR=Test` means `$VAR` is set for the script, so the variables are the
/// event's fields and there is nothing to unpack on the way out. Values are
/// strings because that is what an environment variable is.
///
/// Ordered rather than hashed, so `$INFO`'s JSON has the same key order every
/// time -- a script diffing it, or a person reading a log, should not see it
/// shuffle between two identical triggers.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Custom {
    pub name: String,
    /// A field of its own rather than `#[serde(flatten)]`. Flattening makes
    /// serde write a map of unknown length, which the old postcard transport
    /// could not encode at all -- every `--trigger demo VAR=1` failed on the
    /// wire before it left the CLI. `MessagePack` buffers such a map and encodes
    /// it, so flattening is now merely a choice rather than impossible; this
    /// stays a named field because `$INFO`'s JSON is written against it.
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
}

impl Custom {
    /// Named after the event alone, carrying nothing.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            vars: BTreeMap::new(),
        }
    }

    fn fields(&self) -> BTreeMap<String, String> {
        self.vars.clone()
    }
}

impl Event {
    /// The environment a script is handed, beyond `NAME`.
    ///
    /// `SENDER` is the event's name. `INFO` is the payload as one value — the
    /// field itself when there is exactly one, a JSON object when there are
    /// several, empty when there are none — which is what a config already
    /// reaches for. Every field also arrives under its own name, so a script
    /// that wants the volume can read `VOLUME` instead of parsing.
    ///
    /// Exactly the names `SketchyBar` sets, and no others: a config's plugin
    /// scripts are written against it, and the whole point of matching its
    /// CLI is that those scripts run unchanged. There used to be an
    /// `COOLABAH_`-prefixed twin of each, on the theory that a bare `NAME` is
    /// easy for something else in the environment to have set — but a script
    /// that reads the prefixed one is a script that no longer runs under
    /// `SketchyBar`, which is the one thing this is not allowed to cost.
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

        let mut env = BTreeMap::from([
            ("SENDER".to_owned(), self.kind().name().to_owned()),
            ("INFO".to_owned(), info),
        ]);
        env.extend(
            self.env_fields()
                .into_iter()
                .map(|(name, value)| (name.into_owned(), value)),
        );
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
    use async_mach_ports::Codec as _;

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
        assert_eq!(env["SENDER"], "volume_changed");
        assert_eq!(env["INFO"], "42");
        assert_eq!(env["VOLUME"], "42");
    }

    #[test]
    fn several_fields_become_a_json_object_and_named_variables() {
        let env = Event::SpaceChanged(SpaceChange {
            display: 2,
            space: 7,
        })
        .env();
        assert_eq!(env["DISPLAY"], "2");
        assert_eq!(env["SPACE"], "7");
        let info = &env["INFO"];
        assert!(
            info.contains("\"display\""),
            "several fields render as an object: {info}"
        );
    }

    #[test]
    fn an_empty_payload_still_sets_info_so_a_script_never_sees_it_unset() {
        let env = Event::SystemWoke(SystemWoke {}).env();
        assert_eq!(env["INFO"], "");
    }

    #[test]
    fn a_custom_events_variables_reach_the_script_by_name() {
        // `--trigger demo VAR=Test` means the script reads `$VAR`, not one
        // `$DATA` holding `{"VAR":"Test"}` for it to parse back out.
        let event = Event::Custom(Custom {
            name: "demo".into(),
            vars: BTreeMap::from([
                ("VAR".to_owned(), "Test".to_owned()),
                ("OTHER".to_owned(), "2".to_owned()),
            ]),
        });
        assert_eq!(event.kind(), Kind::Custom("demo".into()));

        let env = event.env();
        assert_eq!(env["SENDER"], "demo");
        assert_eq!(env["VAR"], "Test");
        assert_eq!(env["OTHER"], "2");
        // And under the prefixed spelling too, for a script that wants the
        // name nothing else could have set.
        assert_eq!(env["VAR"], "Test");
    }

    #[test]
    fn a_custom_event_with_one_variable_puts_it_in_info() {
        let event = Event::Custom(Custom {
            name: "my.event".into(),
            vars: BTreeMap::from([("value".to_owned(), "hi".to_owned())]),
        });
        let env = event.env();
        assert_eq!(env["SENDER"], "my.event");
        assert_eq!(env["INFO"], "hi");
    }

    #[test]
    fn a_tag_matches_only_its_own_event() {
        let volume = Event::VolumeChanged(VolumeChange { volume: 42 });
        assert!(Kind::VolumeChanged.matches(&volume));
        assert!(!Kind::SystemWoke.matches(&volume));
    }

    #[test]
    fn custom_tags_match_by_name() {
        let mine = Event::Custom(Custom::new("mine"));
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
        assert_eq!("mouse.clicked".parse(), Ok(Kind::MouseClicked(())));
        assert_eq!(
            "mouse.scrolled.global".parse(),
            Ok(Kind::MouseScrolledGlobal)
        );
        assert_eq!(Kind::MouseEnteredGlobal.name(), "mouse.entered.global");
    }

    #[test]
    fn a_scoped_kind_recasts_its_item_and_leaves_the_rest_alone() {
        assert_eq!(
            Kind::MouseClicked(()).map(|()| 7),
            Kind::<i32>::MouseClicked(7)
        );
        assert_eq!(Kind::SystemWoke.map(|()| 7), Kind::<i32>::SystemWoke);
    }

    #[test]
    fn only_a_scoped_kind_carries_an_item_to_read_back() {
        assert_eq!(Kind::MouseClicked(()).map(|()| 7).item(), Some(&7));
        assert_eq!(Kind::SystemWoke.map(|()| 7).item(), None);
        assert_eq!(Kind::<i32>::Custom("mine".into()).item(), None);
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
        assert_eq!(env["BUTTON"], "right");
        assert_eq!(env["MODIFIERS"], "shift,cmd");
    }

    #[test]
    fn no_modifiers_reads_as_none_rather_than_empty() {
        // A script comparing against "" would be a trap.
        assert_eq!(Modifiers::empty().to_string(), "none");
    }

    #[test]
    fn a_determinate_field_still_flattens_into_the_environment() {
        let env = Event::PowerSourceChanged(PowerChange {
            power_source: PowerSource::Ac {
                adapter_watts: Some(96),
                charging: true,
                time_to_full_minutes: Some(41),
            },
            watts: Some(30),
            charge: Some(80),
        })
        .env();
        assert_eq!(env["POWER_SOURCE"], "AC");
        assert_eq!(env["ADAPTER_WATTS"], "96");
        assert_eq!(env["CHARGING"], "true");
        assert_eq!(env["TIME_TO_FULL_MINUTES"], "41");
        // The other side's number is present and empty, never missing.
        assert_eq!(env["TIME_TO_EMPTY_MINUTES"], "");
        assert_eq!(env["WATTS"], "30");
        assert_eq!(env["CHARGE"], "80");
        // `INFO` keys stay lower-case while the variables are upper-cased,
        // and an expanded field appears in both.
        let info = &env["INFO"];
        assert!(info.contains("\"adapter_watts\":\"96\""), "{info}");
        assert!(info.contains("\"power_source\":\"AC\""), "{info}");
    }

    #[test]
    fn a_variant_that_cannot_be_charging_says_so_rather_than_leaving_a_hole() {
        let env = Event::PowerSourceChanged(PowerChange {
            power_source: PowerSource::Battery {
                time_to_empty_minutes: Some(212),
            },
            watts: Some(12),
            charge: Some(64),
        })
        .env();
        assert_eq!(env["POWER_SOURCE"], "BATTERY");
        assert_eq!(env["TIME_TO_EMPTY_MINUTES"], "212");
        // On battery is discharging by definition, and saying `false` is
        // worth more to a script than an empty it would have to interpret.
        assert_eq!(env["CHARGING"], "false");
        assert_eq!(env["ADAPTER_WATTS"], "");
        assert_eq!(env["TIME_TO_FULL_MINUTES"], "");
    }

    #[test]
    fn an_unknown_power_source_still_sets_every_name() {
        let env = Event::PowerSourceChanged(PowerChange::default()).env();
        assert_eq!(env["POWER_SOURCE"], "UNKNOWN");
        for name in [
            "ADAPTER_WATTS",
            "CHARGING",
            "TIME_TO_FULL_MINUTES",
            "TIME_TO_EMPTY_MINUTES",
            "WATTS",
            "CHARGE",
        ] {
            assert_eq!(env[name], "", "{name} is present and empty, never missing");
        }
    }

    #[test]
    fn no_variable_is_prefixed() {
        // The `COOLABAH_` twins are gone: a script that reads one is a script
        // that no longer runs under SketchyBar.
        let env = Event::VolumeChanged(VolumeChange { volume: 42 }).env();
        assert!(
            env.keys().all(|name| !name.starts_with("COOLABAH_")),
            "{env:?}"
        );
    }

    /// A nested enum outside the `events!` list: the derive is the whole
    /// implementation, and `Quality` is deliberately not a payload.
    #[derive(Debug, Clone, Copy, Default, EnvFields)]
    enum Quality {
        Good {
            confidence: u8,
        },
        /// Stale by definition, so it says so rather than leaving a hole.
        #[env(stale = true)]
        Stale {
            age_seconds: u32,
        },
        #[default]
        Unknown,
    }

    #[derive(Debug, Clone, Copy, Default, EnvFields)]
    struct Reading {
        #[env(flatten)]
        quality: Quality,
        samples: u32,
    }

    #[derive(Debug, Clone, Copy, Default, EnvFields)]
    struct Sample {
        #[env(flatten)]
        reading: Reading,
        id: u32,
    }

    impl fmt::Display for Quality {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(match self {
                Self::Good { .. } => "good",
                Self::Stale { .. } => "stale",
                Self::Unknown => "unknown",
            })
        }
    }

    impl fmt::Display for Reading {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{} ({})", self.quality, self.samples)
        }
    }

    field_via_display!(Quality, Reading);

    #[test]
    fn nesting_projects_every_level_up_to_one_flat_environment() {
        let env = Sample {
            reading: Reading {
                quality: Quality::Stale { age_seconds: 90 },
                samples: 3,
            },
            id: 7,
        }
        .env_fields();
        assert_eq!(env["ID"], "7");
        assert_eq!(env["SAMPLES"], "3");
        // Two levels down, under its own name rather than a prefixed one.
        assert_eq!(env["QUALITY"], "stale");
        assert_eq!(env["AGE_SECONDS"], "90");
        // The constant the variant implies rather than stores.
        assert_eq!(env["STALE"], "true");
        // The other variant's field, present and empty.
        assert_eq!(env["CONFIDENCE"], "");
        // And the nested value's own name is there too.
        assert_eq!(env["READING"], "stale (3)");
    }

    #[test]
    fn an_inactive_variants_names_are_empty_rather_than_absent() {
        let fields = Sample::default().fields();
        // Lower-case here, which is what `INFO`'s JSON is built from.
        for name in ["confidence", "age_seconds", "stale"] {
            assert_eq!(fields[name], "", "{name} is present and empty");
        }
        assert_eq!(fields["quality"], "unknown");

        let good = Reading {
            quality: Quality::Good { confidence: 90 },
            samples: 1,
        }
        .fields();
        assert_eq!(good["confidence"], "90");
        assert_eq!(good["age_seconds"], "");
        assert_eq!(good["stale"], "");
    }

    #[test]
    fn events_round_trip_through_the_wire_format() {
        let event = Event::SpaceChanged(SpaceChange {
            display: 1,
            space: 9,
        });
        let bytes = crate::wire::MessagePack.encode(&event).unwrap();
        assert_eq!(
            crate::wire::MessagePack.decode::<Event>(&bytes).unwrap(),
            event
        );
    }
}

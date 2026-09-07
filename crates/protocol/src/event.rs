//! What an item can be told about.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Why an item is being updated.
///
/// A `Custom` event is any name a config invented and triggers itself, so the
/// set is open — unlike `SketchyBar`, which packs subscriptions into a `u64`
/// mask and is therefore capped at 64 events in total.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    /// The periodic tick, gated per item by its update frequency.
    Routine,
    /// An explicit refresh, ignoring update frequency.
    Forced,
    FrontAppSwitched,
    SpaceChanged,
    DisplayChanged,
    SystemWoke,
    SystemWillSleep,
    VolumeChanged,
    BrightnessChanged,
    PowerSourceChanged,
    WifiChanged,
    MediaChanged,
    SpaceWindowsChanged,
    /// The config file changed on disk and has been re-run.
    ConfigReloaded,
    Custom(String),
}

impl Event {
    /// The name a script sees in `RSBAR_SENDER`.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Routine => "routine",
            Self::Forced => "forced",
            Self::FrontAppSwitched => "front_app_switched",
            Self::SpaceChanged => "space_changed",
            Self::DisplayChanged => "display_changed",
            Self::SystemWoke => "system_woke",
            Self::SystemWillSleep => "system_will_sleep",
            Self::VolumeChanged => "volume_changed",
            Self::BrightnessChanged => "brightness_changed",
            Self::PowerSourceChanged => "power_source_changed",
            Self::WifiChanged => "wifi_changed",
            Self::MediaChanged => "media_changed",
            Self::SpaceWindowsChanged => "space_windows_changed",
            Self::ConfigReloaded => "config_reloaded",
            Self::Custom(name) => name,
        }
    }

    /// The events a config can name, excluding `Custom`.
    pub const BUILT_IN: [Self; 14] = [
        Self::Routine,
        Self::Forced,
        Self::FrontAppSwitched,
        Self::SpaceChanged,
        Self::DisplayChanged,
        Self::SystemWoke,
        Self::SystemWillSleep,
        Self::VolumeChanged,
        Self::BrightnessChanged,
        Self::PowerSourceChanged,
        Self::WifiChanged,
        Self::MediaChanged,
        Self::SpaceWindowsChanged,
        Self::ConfigReloaded,
    ];
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not an event name")]
pub struct InvalidEvent(String);

impl FromStr for Event {
    type Err = InvalidEvent;

    /// Accepts `-` as well as `_`, since a CLI reads better with dashes.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalised = s.replace('-', "_").to_ascii_lowercase();
        if let Some(event) = Self::BUILT_IN.iter().find(|e| e.name() == normalised) {
            return Ok(event.clone());
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
        for event in &Event::BUILT_IN {
            assert_eq!(event.name().parse::<Event>().as_ref(), Ok(event));
        }
    }

    #[test]
    fn dashes_and_case_are_accepted() {
        assert_eq!("front-app-switched".parse(), Ok(Event::FrontAppSwitched));
        assert_eq!("SYSTEM_WOKE".parse(), Ok(Event::SystemWoke));
    }

    #[test]
    fn custom_events_are_still_validated() {
        assert_eq!("my.event".parse(), Ok(Event::Custom("my.event".into())));
        assert!("has space".parse::<Event>().is_err());
        assert!("".parse::<Event>().is_err());
    }
}

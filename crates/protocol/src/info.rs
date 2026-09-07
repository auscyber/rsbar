//! What an event carries beyond the fact that it happened.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Where the machine is drawing power from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerSource {
    Ac,
    Battery,
    Unknown,
}

impl fmt::Display for PowerSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The spelling a script sees, so it is part of the interface.
        f.write_str(match self {
            Self::Ac => "AC",
            Self::Battery => "BATTERY",
            Self::Unknown => "UNKNOWN",
        })
    }
}

/// The payload of an event.
///
/// Typed rather than a bare string. `SketchyBar` passes everything through one
/// `INFO` variable and leaves each script to re-parse it; a script here still
/// gets `RSBAR_INFO`, but the daemon knows what it is holding, so it can also
/// hand over the parts separately and a Lua handler can be given real values
/// instead of a string to pick apart.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Info {
    /// The event carries nothing — it is the fact that it happened.
    #[default]
    None,
    /// The application that came to the front.
    App { name: String },
    /// Output volume, as a whole percentage.
    Volume { percent: u8 },
    /// Where power is coming from.
    Power { source: PowerSource },
    /// The space now current on each display, by arrangement id.
    Space { display: u32, space: u64 },
    /// Anything a config invented and triggered itself, or an event whose
    /// shape is the framework's rather than ours. A JSON value, so a Lua
    /// handler gets a structure rather than a string to pick apart.
    Custom(crate::Json),
}

impl Info {
    /// What a script sees in `RSBAR_INFO`.
    ///
    /// One value, because that is the variable every config already reaches
    /// for. The structured parts arrive alongside it via [`Self::env`].
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::None => String::new(),
            Self::App { name } => name.clone(),
            Self::Volume { percent } => percent.to_string(),
            Self::Power { source } => source.to_string(),
            Self::Space { space, .. } => space.to_string(),
            Self::Custom(value) => value.to_string(),
        }
    }

    /// The environment a script is handed, beyond `RSBAR_NAME` and
    /// `RSBAR_SENDER`.
    ///
    /// Named per field rather than making every script parse `RSBAR_INFO`.
    #[must_use]
    pub fn env(&self) -> Vec<(&'static str, String)> {
        let mut vars = vec![("RSBAR_INFO", self.summary())];
        match self {
            Self::App { name } => vars.push(("RSBAR_APP", name.clone())),
            Self::Volume { percent } => vars.push(("RSBAR_VOLUME", percent.to_string())),
            Self::Power { source } => vars.push(("RSBAR_POWER_SOURCE", source.to_string())),
            Self::Space { display, space } => {
                vars.push(("RSBAR_DISPLAY", display.to_string()));
                vars.push(("RSBAR_SPACE", space.to_string()));
            }
            Self::None | Self::Custom(_) => {}
        }
        vars
    }

    #[must_use]
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

impl fmt::Display for Info {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_payload_summarises_to_something_a_script_can_read() {
        assert_eq!(Info::None.summary(), "");
        assert_eq!(
            Info::App {
                name: "Finder".into()
            }
            .summary(),
            "Finder"
        );
        assert_eq!(Info::Volume { percent: 42 }.summary(), "42");
        assert_eq!(
            Info::Power {
                source: PowerSource::Battery
            }
            .summary(),
            "BATTERY"
        );
        assert_eq!(
            Info::Custom(crate::Json::String("hello".into())).summary(),
            "hello"
        );
    }

    #[test]
    fn structured_payloads_also_arrive_as_named_variables() {
        let env = Info::Volume { percent: 42 }.env();
        assert!(env.contains(&("RSBAR_INFO", "42".to_owned())));
        assert!(env.contains(&("RSBAR_VOLUME", "42".to_owned())));

        let env = Info::Space {
            display: 2,
            space: 7,
        }
        .env();
        assert!(env.contains(&("RSBAR_DISPLAY", "2".to_owned())));
        assert!(env.contains(&("RSBAR_SPACE", "7".to_owned())));
    }

    #[test]
    fn an_empty_payload_still_sets_info_so_a_script_never_sees_it_unset() {
        assert_eq!(Info::None.env(), vec![("RSBAR_INFO", String::new())]);
    }

    #[test]
    fn power_source_spelling_is_part_of_the_interface() {
        // Configs match on these, so they are not free to change.
        assert_eq!(PowerSource::Ac.to_string(), "AC");
        assert_eq!(PowerSource::Battery.to_string(), "BATTERY");
    }
}

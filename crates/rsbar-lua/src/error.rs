//! Typed errors, all of which end up as a Lua error with a real message —
//! never a silent `nil` or a swallowed failure.

use rsbar_protocol::event::{InvalidEvent, InvalidEventName, InvalidNotificationName};
use rsbar_protocol::{InvalidName, InvalidPosition, Response};

/// Anything that can go wrong issuing a request or waiting on events, from
/// either dispatcher.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("`{0}` is not a valid item name: {1}")]
    InvalidName(String, InvalidName),
    #[error("`{0}` is not a position: {1}")]
    InvalidPosition(String, InvalidPosition),
    #[error("`{0}` is not an event name: {1}")]
    InvalidEvent(String, InvalidEvent),
    #[error("`{0}` cannot be added as an event: {1}")]
    InvalidEventName(String, InvalidEventName),
    #[error("`{0}` is not a notification to bridge an event from: {1}")]
    InvalidNotificationName(String, InvalidNotificationName),
    #[error("`{0}` is not a colour: expected 0xaarrggbb, 0xrrggbb or \"#rrggbb\"")]
    InvalidColor(String),
    #[error("`{0}` is not a valid item-name pattern: {1}")]
    InvalidPattern(String, String),
    #[error("rsbar is not running")]
    NotRunning,
    #[error("lost contact with rsbar: {0}")]
    Transport(#[source] async_mach_ports::Error),
    #[error("rsbar rejected the request: {0}")]
    Rejected(String),
    #[error("rsbar answered with {0:?}, which does not belong to this request")]
    UnexpectedResponse(Response),
}

impl From<async_mach_ports::Error> for ApiError {
    fn from(err: async_mach_ports::Error) -> Self {
        match err {
            async_mach_ports::Error::NotRunning => Self::NotRunning,
            other => Self::Transport(other),
        }
    }
}

impl From<ApiError> for mlua::Error {
    fn from(err: ApiError) -> Self {
        Self::RuntimeError(format!("rsbar: {err}"))
    }
}

pub type Result<T> = std::result::Result<T, ApiError>;

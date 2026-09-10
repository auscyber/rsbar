use objc2_application_services::AXError;
use objc2_core_graphics::CGError;

pub type Result<T> = std::result::Result<T, Error>;

/// Every window server call reports the same opaque `CGError`, and every
/// Accessibility call the same opaque `AXError`, so the variant records which
/// call produced it. Without that a failure is untraceable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("failed to build a window region: {0:?}")]
    Region(CGError),
    #[error("failed to create a window server window: {0:?}")]
    CreateWindow(CGError),
    #[error("failed to release window server window: {0:?}")]
    ReleaseWindow(CGError),
    #[error("failed to set window shape: {0:?}")]
    Shape(CGError),
    #[error("failed to set window resolution: {0:?}")]
    Resolution(CGError),
    #[error("failed to set window opacity or alpha: {0:?}")]
    Opacity(CGError),
    #[error("failed to set window level: {0:?}")]
    Level(CGError),
    #[error("failed to order window: {0:?}")]
    Order(CGError),
    #[error("failed to set window tags: {0:?}")]
    Tags(CGError),
    #[error("failed to set window blur radius: {0:?}")]
    Blur(CGError),
    #[error("the window server returned no drawing context for this window")]
    NoContext,
    #[error(
        "the window server returned no captured image; the process probably lacks Screen Recording permission"
    )]
    NoCapture,
    #[error("failed to read a window's true screen rect: {0:?}")]
    ScreenRect(CGError),
    #[error("the window server declined a notification registration: {0:?}")]
    Notify(CGError),
    #[error(
        "this process is not trusted for Accessibility; grant it under System Settings > Privacy & Security > Accessibility"
    )]
    NotTrusted,
    #[error(
        "the Accessibility API is disabled for this process; it was granted after the process \
         started and the process has to be replaced before the API will answer"
    )]
    ApiDisabled,
    #[error("an element refused the action: {0:?}")]
    Action(AXError),
    #[error("failed to create an Accessibility observer for this application: {0:?}")]
    CreateObserver(AXError),
    #[error("failed to register an Accessibility notification: {0:?}")]
    Notification(AXError),
    #[error("no application is currently frontmost")]
    NoFrontApp,
    #[error("this application exposes no menu bar")]
    NoMenuBar,
    #[error("failed to read or set a display's brightness: {0:?}")]
    Brightness(CGError),
    #[error("failed to enumerate the active displays: {0:?}")]
    DisplayList(CGError),
    #[error("failed to read a property of a window this process does not own: {0:?}")]
    ForeignWindow(CGError),
}

/// Turns the window server's success-or-code convention into a `Result`.
pub(crate) fn ok(err: CGError) -> std::result::Result<(), CGError> {
    if err == CGError::Success {
        Ok(())
    } else {
        Err(err)
    }
}

use objc2_core_graphics::CGError;

pub type Result<T> = std::result::Result<T, Error>;

/// Every window server call reports the same opaque `CGError`, so the variant
/// records which call produced it. Without that a failure is untraceable.
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
}

/// Turns the window server's success-or-code convention into a `Result`.
pub(crate) fn ok(err: CGError) -> std::result::Result<(), CGError> {
    if err == CGError::Success {
        Ok(())
    } else {
        Err(err)
    }
}

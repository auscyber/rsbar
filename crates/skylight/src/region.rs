use crate::error::{Error, Result, ok};
use crate::ffi;
use objc2_core_foundation::{CFRetained, CFType, CGRect};
use std::ptr::{self, NonNull};

/// A `CGSRegionRef`, which describes a window's shape.
///
/// Wrapped only so the retain is released on drop — the window server hands
/// these out with a +1 count and leaks them otherwise.
#[repr(transparent)]
pub(crate) struct Region(CFRetained<CFType>);

impl Region {
    pub(crate) fn from_rect(rect: &CGRect) -> Result<Self> {
        let mut raw: *mut CFType = ptr::null_mut();
        // SAFETY: `raw` is a valid out-pointer; the callee writes a +1 region.
        ok(unsafe { ffi::CGSNewRegionWithRect(rect, &raw mut raw) }).map_err(Error::Region)?;
        let raw = NonNull::new(raw).ok_or(Error::Region(objc2_core_graphics::CGError::Failure))?;
        // SAFETY: ownership of the +1 reference transfers to `CFRetained`.
        Ok(Self(unsafe { CFRetained::from_raw(raw) }))
    }

    /// An empty region. As a window's *opaque* shape this means "no pixel is
    /// opaque", which is what makes the window alpha-blended.
    pub(crate) fn empty() -> Result<Self> {
        // SAFETY: the callee returns a +1 region or null.
        let raw = NonNull::new(unsafe { ffi::CGRegionCreateEmptyRegion() })
            .ok_or(Error::Region(objc2_core_graphics::CGError::Failure))?;
        // SAFETY: ownership of the +1 reference transfers to `CFRetained`.
        Ok(Self(unsafe { CFRetained::from_raw(raw) }))
    }

    pub(crate) fn as_ptr(&self) -> *mut CFType {
        CFRetained::<CFType>::as_ptr(&self.0).as_ptr()
    }
}

//! Getting Rust values back out of Core Foundation objects.
//!
//! Two edges of the macOS APIs wrapped here hand back an object with no static type at all: a
//! dictionary from the window server's window list, and an attribute from an
//! application's Accessibility tree. The key's name and the attribute's name
//! are documentation, not a guarantee, so every read here is a checked
//! conversion that answers `None` rather than trusting either.

use objc2_application_services::{AXValue, AXValueType};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, CGPoint, CGRect,
    CGSize, ConcreteType,
};
use objc2_core_graphics::{
    CGRectMakeWithDictionaryRepresentation, kCGWindowBounds, kCGWindowLayer, kCGWindowName,
    kCGWindowNumber, kCGWindowOwnerName, kCGWindowOwnerPID,
};
use std::ffi::c_void;
use std::ptr::NonNull;

/// A value recoverable from a Core Foundation object.
///
/// The one conversion both untyped edges go through: a dictionary value
/// ([`Dict::get`]) and an Accessibility attribute ([`crate::ax::attribute`])
/// are each a `CFType` that may be any object at all, and each caller already
/// knows what it expects to find. Naming that expectation as `T` turns
/// "downcast and hope" into one checked conversion written once, and makes a
/// wrong guess a `None` instead of a bad cast.
pub trait FromCF: Sized {
    fn from_cf(value: &CFType) -> Option<Self>;
}

/// Any CF object, kept as itself. The type id is checked, so a `None` means
/// the value really was something else.
impl<T: ConcreteType> FromCF for CFRetained<T> {
    fn from_cf(value: &CFType) -> Option<Self> {
        Some(value.downcast_ref::<T>()?.retain())
    }
}

impl FromCF for String {
    fn from_cf(value: &CFType) -> Option<Self> {
        Some(value.downcast_ref::<CFString>()?.to_string())
    }
}

impl FromCF for i64 {
    fn from_cf(value: &CFType) -> Option<Self> {
        value.downcast_ref::<CFNumber>()?.as_i64()
    }
}

impl FromCF for bool {
    fn from_cf(value: &CFType) -> Option<Self> {
        Some(value.downcast_ref::<CFBoolean>()?.as_bool())
    }
}

/// A geometry struct an `AXValue` can be carrying.
///
/// Accessibility does not hand back a point or a rect as a CF object of its
/// own; it hands back an opaque `AXValue` and a tag saying which C structure
/// is inside, which is then copied out into storage the caller provides. This
/// pairs each Rust type with its tag once, so the copy is written once too.
///
/// # Safety
///
/// `TYPE` must be the `AXValueType` whose C structure is exactly `Self`, since
/// [`ax_geometry`] hands `AXValue` a `Self`-sized buffer to write into.
unsafe trait AxGeometry: Default {
    const TYPE: AXValueType;
}

// SAFETY: each of these is the C structure its tag names -- `CGPoint`,
// `CGSize` and `CGRect` are the same layout Core Graphics declares them as.
unsafe impl AxGeometry for CGPoint {
    const TYPE: AXValueType = AXValueType::CGPoint;
}
unsafe impl AxGeometry for CGSize {
    const TYPE: AXValueType = AXValueType::CGSize;
}
unsafe impl AxGeometry for CGRect {
    const TYPE: AXValueType = AXValueType::CGRect;
}

/// The structure inside an `AXValue`, if that is what this is and it holds a
/// `T`.
fn ax_geometry<T: AxGeometry>(value: &CFType) -> Option<T> {
    let value = value.downcast_ref::<AXValue>()?;
    let mut out = T::default();
    // SAFETY: `out` is exactly the C structure `T::TYPE` names, per the
    // trait's contract, and is a valid out-pointer.
    unsafe { value.value(T::TYPE, NonNull::from(&mut out).cast()) }.then_some(out)
}

impl FromCF for CGPoint {
    fn from_cf(value: &CFType) -> Option<Self> {
        ax_geometry(value)
    }
}

impl FromCF for CGSize {
    fn from_cf(value: &CFType) -> Option<Self> {
        ax_geometry(value)
    }
}

/// A rect, from either encoding one reaches this crate in: an `AXValue`, or
/// the nested `X`/`Y`/`Width`/`Height` dictionary Core Graphics writes into
/// `kCGWindowBounds`. The two are disjoint -- an `AXValue` is never a
/// `CFDictionary` -- so trying both is unambiguous, and a caller gets to ask
/// for a rect without knowing which side it came from.
impl FromCF for CGRect {
    fn from_cf(value: &CFType) -> Option<Self> {
        if let Some(rect) = ax_geometry(value) {
            return Some(rect);
        }
        let bounds = value.downcast_ref::<CFDictionary>()?;
        let mut rect = Self::default();
        // SAFETY: `bounds` is a valid dictionary and `rect` a valid
        // out-pointer.
        unsafe { CGRectMakeWithDictionaryRepresentation(Some(bounds), &raw mut rect) }
            .then_some(rect)
    }
}

/// A borrowed `CFArray`, read by index.
///
/// The unchecked part of reading one — a bounds check the C call does not do,
/// a null element, and the assertion that an element is a CF object at all —
/// happens here, once, so a caller names an index and a type and gets an
/// `Option` back.
#[derive(Clone, Copy)]
pub struct Array<'a>(&'a CFArray);

impl<'a> Array<'a> {
    #[must_use]
    pub fn new(array: &'a CFArray) -> Self {
        Self(array)
    }

    #[must_use]
    pub fn len(self) -> isize {
        self.0.count()
    }

    #[must_use]
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// The element at `i`, whatever kind of object it is, if it is there.
    ///
    /// Safe, because the two things that could go wrong are both checked: an
    /// index past the end, which `value_at_index` would read anyway, and a
    /// null element. What is left is forming a `&CFType` from the element,
    /// which cannot be checked -- `downcast_ref` needs a `&CFType` before it
    /// can compare type ids, so there is no way to ask "is this a CF object"
    /// without first asserting that it is. That assertion holds for every
    /// array reachable here: they all come back from a framework copy call
    /// whose contract is an array of CF objects.
    ///
    /// Untyped on purpose, for the arrays whose elements have no concrete
    /// type to name -- `IOPSCopyPowerSourcesList`'s entries are opaque tokens
    /// that only ever go straight back into another `IOPS` call.
    #[must_use]
    pub fn value(self, i: isize) -> Option<&'a CFType> {
        if i < 0 || i >= self.len() {
            return None;
        }
        // SAFETY: the index is in range, checked above.
        let ptr = unsafe { self.0.value_at_index(i) };
        let ptr = NonNull::new(ptr.cast_mut())?.cast::<CFType>();
        // SAFETY: a CF array's elements are CF objects; see the note above.
        // The pointer borrows from the array, so it lives as long as this.
        Some(unsafe { ptr.as_ref() })
    }

    /// The element at `i`, if it is there and is a `T`.
    ///
    /// Being a `T` specifically is genuinely checked, so a `None` means the
    /// element really was something else.
    #[must_use]
    pub fn get<T: ConcreteType>(self, i: isize) -> Option<&'a T> {
        self.value(i)?.downcast_ref::<T>()
    }

    /// The dictionary at `i`, ready to read by key.
    #[must_use]
    pub fn dict(self, i: isize) -> Option<Dict<'a>> {
        self.get::<CFDictionary>(i).map(Dict)
    }

    /// Every element that is a `T`, in order.
    pub fn iter<T: ConcreteType + 'a>(self) -> impl Iterator<Item = &'a T> {
        (0..self.len()).filter_map(move |i| self.get::<T>(i))
    }
}

/// Something a dictionary can be read by.
///
/// Two shapes reach a dictionary here: a framework's exported `'static`
/// `CFString` constants, which are worth naming as a closed set (see
/// [`WindowKey`]), and a plain `&str` for a key a caller spells itself.
/// Building the `CFString` is scoped to the read rather than returned,
/// because for the `&str` case there is nothing to return a reference to.
pub trait DictKey {
    fn with_cf<R>(&self, f: impl FnOnce(&CFString) -> R) -> R;
}

impl DictKey for str {
    fn with_cf<R>(&self, f: impl FnOnce(&CFString) -> R) -> R {
        f(&CFString::from_str(self))
    }
}

impl<T: DictKey + ?Sized> DictKey for &T {
    fn with_cf<R>(&self, f: impl FnOnce(&CFString) -> R) -> R {
        (**self).with_cf(f)
    }
}

impl DictKey for CFString {
    fn with_cf<R>(&self, f: impl FnOnce(&CFString) -> R) -> R {
        f(self)
    }
}

/// A key in one of the dictionaries `CGWindowListCopyWindowInfo` returns.
///
/// Every one of these is a `'static` `CFString` the framework exports, and
/// reading an extern static is unsafe -- so each is read here, once, instead
/// of at all six call sites. Naming them as a closed set also says which keys
/// this crate actually asks for, which the raw constants never did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WindowKey {
    /// The window's layer. The menu bar's is `0x19`.
    Layer,
    /// The window server's own id for it, a [`crate::WindowId`].
    Number,
    /// Its screen rect, as a nested `X`/`Y`/`Width`/`Height` dictionary.
    Bounds,
    /// The window's own title, which is often empty.
    Name,
    /// The localized name of the application that owns it.
    OwnerName,
    /// That application's process id.
    OwnerPid,
}

impl DictKey for WindowKey {
    fn with_cf<R>(&self, f: impl FnOnce(&CFString) -> R) -> R {
        f(self.cf())
    }
}

impl WindowKey {
    fn cf(self) -> &'static CFString {
        // SAFETY: each of these is a `'static` constant the framework
        // exports, unsafe to read only because it is extern.
        unsafe {
            match self {
                Self::Layer => kCGWindowLayer,
                Self::Number => kCGWindowNumber,
                Self::Bounds => kCGWindowBounds,
                Self::Name => kCGWindowName,
                Self::OwnerName => kCGWindowOwnerName,
                Self::OwnerPid => kCGWindowOwnerPID,
            }
        }
    }
}

/// A borrowed `CFDictionary`, read by key.
///
/// Every read answers `None` for an absent key as well as for a value of
/// another type, which is the whole reason these are methods on the
/// dictionary rather than free functions taking one: the unchecked pointer
/// work happens once, here, and a caller only ever names a key and a type.
#[derive(Clone, Copy)]
pub struct Dict<'a>(&'a CFDictionary);

impl<'a> Dict<'a> {
    /// Reads a dictionary already in hand.
    #[must_use]
    pub fn new(dict: &'a CFDictionary) -> Self {
        Self(dict)
    }

    /// One element of a `CFArray`, if it is there and is a dictionary.
    #[must_use]
    pub fn at(array: &'a CFArray, i: isize) -> Option<Self> {
        Array::new(array).dict(i)
    }

    /// The value under `key`, whatever kind of object it turns out to be.
    fn value(self, key: impl DictKey) -> Option<&'a CFType> {
        let ptr = key.with_cf(|key| {
            let key: *const c_void = std::ptr::from_ref(key).cast();
            // SAFETY: `key` is a live `CFString` for the call's duration.
            unsafe { self.0.value(key) }
        });
        let ptr = NonNull::new(ptr.cast_mut())?.cast::<CFType>();
        // SAFETY: a CF dictionary's values are CF objects, so this really is
        // one; and the pointer came from the dictionary this borrows, so it
        // lives at least as long as that does. What *kind* of object is not
        // asserted here -- [`Dict::get`] checks it.
        Some(unsafe { ptr.as_ref() })
    }

    /// The value under `key`, if it is there and is a `T`.
    #[must_use]
    pub fn get<T: FromCF>(self, key: impl DictKey) -> Option<T> {
        T::from_cf(self.value(key)?)
    }
}

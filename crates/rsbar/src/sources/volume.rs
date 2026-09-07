//! Output volume changes, from `CoreAudio`.
//!
//! Two listeners, not one. The volume of the current output device answers
//! "how loud is it", but the *default output device* can itself change —
//! plugging in headphones — and the old device's volume then means nothing.
//! So the device-change listener re-registers the volume listener on whatever
//! became default.

use crate::sources::{Emission, Emitter, Source, StartError};
use rsbar_protocol::Event;
use std::cell::RefCell;
use std::ffi::c_void;
type AudioObjectId = u32;
type OsStatus = i32;

/// `CoreAudio` names its properties with four-character codes.
const fn fourcc(code: [u8; 4]) -> u32 {
    u32::from_be_bytes(code)
}

const SYSTEM_OBJECT: AudioObjectId = 1;
const DEFAULT_OUTPUT_DEVICE: u32 = fourcc(*b"dOut");
const VOLUME_SCALAR: u32 = fourcc(*b"volm");
const MUTE: u32 = fourcc(*b"mute");
const SCOPE_GLOBAL: u32 = fourcc(*b"glob");
const SCOPE_OUTPUT: u32 = fourcc(*b"outp");
/// `kAudioObjectPropertyElementMain`.
const ELEMENT_MAIN: u32 = 0;
/// The first channel. Some devices report a usable volume only here.
const ELEMENT_LEFT: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct PropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

impl PropertyAddress {
    const fn new(selector: u32, scope: u32) -> Self {
        Self {
            selector,
            scope,
            element: ELEMENT_MAIN,
        }
    }
}

type ListenerProc = extern "C-unwind" fn(
    object: AudioObjectId,
    count: u32,
    addresses: *const PropertyAddress,
    context: *mut c_void,
) -> OsStatus;

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectAddPropertyListener(
        object: AudioObjectId,
        address: *const PropertyAddress,
        listener: ListenerProc,
        context: *mut c_void,
    ) -> OsStatus;

    fn AudioObjectRemovePropertyListener(
        object: AudioObjectId,
        address: *const PropertyAddress,
        listener: ListenerProc,
        context: *mut c_void,
    ) -> OsStatus;

    fn AudioObjectGetPropertyData(
        object: AudioObjectId,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        data_size: *mut u32,
        data: *mut c_void,
    ) -> OsStatus;
}

/// The three `CoreAudio` calls this module makes, each wrapped once.
///
/// Every `unsafe` in this file lives here. Above this line the module is
/// ordinary Rust; below it, nothing touches a raw pointer.
mod ffi_safe {
    use super::{
        AudioObjectAddPropertyListener, AudioObjectGetPropertyData, AudioObjectId,
        AudioObjectRemovePropertyListener, ListenerProc, OsStatus, PropertyAddress,
    };
    use std::ffi::c_void;

    /// Reads a fixed-size property, or `None` if the object does not have it —
    /// which is ordinary, not exceptional: plenty of devices lack a software
    /// volume control.
    pub(super) fn get<T: Copy + Default>(
        object: AudioObjectId,
        address: PropertyAddress,
    ) -> Option<T> {
        let mut value = T::default();
        let mut size = u32::try_from(size_of::<T>()).ok()?;
        // SAFETY: `value` is exactly `size` bytes, `address` outlives the call,
        // and a null qualifier with size 0 is what "no qualifier" means here.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                &raw const address,
                0,
                std::ptr::null(),
                &raw mut size,
                (&raw mut value).cast::<c_void>(),
            )
        };
        (status == 0).then_some(value)
    }

    /// Registers `listener`. The listener must outlive the registration, which
    /// is why every caller here passes a `fn` item rather than a closure.
    pub(super) fn add_listener(
        object: AudioObjectId,
        address: PropertyAddress,
        listener: ListenerProc,
    ) -> OsStatus {
        // SAFETY: `address` outlives the call; `listener` is a `fn` item with
        // static lifetime and the null context is never dereferenced.
        unsafe {
            AudioObjectAddPropertyListener(
                object,
                &raw const address,
                listener,
                std::ptr::null_mut(),
            )
        }
    }

    /// Removes a registration made by [`add_listener`] with the same triple.
    pub(super) fn remove_listener(
        object: AudioObjectId,
        address: PropertyAddress,
        listener: ListenerProc,
    ) {
        // SAFETY: same arguments the matching `add_listener` was given.
        unsafe {
            AudioObjectRemovePropertyListener(
                object,
                &raw const address,
                listener,
                std::ptr::null_mut(),
            );
        }
    }
}

use ffi_safe::{add_listener, get, remove_listener};

fn default_output_device() -> Option<AudioObjectId> {
    get(
        SYSTEM_OBJECT,
        PropertyAddress::new(DEFAULT_OUTPUT_DEVICE, SCOPE_GLOBAL),
    )
}

/// Output volume as a fraction, following `SketchyBar`'s reading of it.
///
/// Both the main element and element 1 are consulted, for mute and for volume
/// alike. Plenty of devices report nothing useful on the main element and put
/// the real value on the first channel, so a non-zero left volume wins; either
/// mute silences it.
#[must_use]
fn current_scalar() -> Option<f32> {
    let device = default_output_device()?;
    let read_u32 = |selector, element| {
        get::<u32>(
            device,
            PropertyAddress {
                selector,
                scope: SCOPE_OUTPUT,
                element,
            },
        )
        .unwrap_or(0)
    };
    let scalar_of = |selector, element| {
        get::<f32>(
            device,
            PropertyAddress {
                selector,
                scope: SCOPE_OUTPUT,
                element,
            },
        )
        .unwrap_or(0.0)
    };

    let muted_main = read_u32(MUTE, ELEMENT_MAIN) != 0;
    let muted_left = read_u32(MUTE, ELEMENT_LEFT) != 0;
    let volume_main = scalar_of(VOLUME_SCALAR, ELEMENT_MAIN);
    let volume_left = scalar_of(VOLUME_SCALAR, ELEMENT_LEFT);

    Some(if volume_left > 0.0 {
        if muted_left || muted_main {
            0.0
        } else {
            volume_left
        }
    } else if muted_main {
        0.0
    } else {
        volume_main
    })
}

/// A 0.0-1.0 scalar as a whole percentage.
///
/// The clamp is what makes the cast total: the value is rounded and pinned to
/// 0..=100 before narrowing, so nothing can truncate or lose a sign.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn percent(scalar: f32) -> u32 {
    (scalar * 100.0).round().clamp(0.0, 100.0) as u32
}

/// Output volume as a whole percentage, or `None` if there is no output device.
#[must_use]
pub fn current_percentage() -> Option<u32> {
    current_scalar().map(percent)
}

thread_local! {
    /// `CoreAudio` delivers on the thread that registered, so the handler and
    /// the device currently being listened to live here rather than behind a
    /// context pointer that would have to be leaked.
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

struct State {
    emit: Emitter,
    listening_to: Option<AudioObjectId>,
    /// `CoreAudio` fires several times for one user-visible change — per
    /// channel, and for mute alongside volume. Only a real move is reported.
    last: f32,
}

/// How much the volume must move to count as a change. `SketchyBar`'s value.
const EPSILON: f32 = 1e-2;

extern "C-unwind" fn changed(
    _object: AudioObjectId,
    _count: u32,
    _addresses: *const PropertyAddress,
    _context: *mut c_void,
) -> OsStatus {
    STATE.with_borrow_mut(|state| {
        let Some(state) = state else { return };
        let Some(scalar) = current_scalar() else {
            return;
        };
        if (scalar - state.last).abs() < EPSILON {
            return;
        }
        state.last = scalar;
        let emission = Emission::new(Event::VolumeChanged, Some(percent(scalar).to_string()));
        // Dropping beats blocking: this is a `CoreAudio` callback.
        let _ = state.emit.try_send(emission);
    });
    0
}

/// Re-points the volume listener at whatever is now the default output.
extern "C-unwind" fn device_changed(
    _object: AudioObjectId,
    _count: u32,
    _addresses: *const PropertyAddress,
    _context: *mut c_void,
) -> OsStatus {
    STATE.with_borrow_mut(|state| {
        if let Some(state) = state {
            state.relisten();
        }
    });
    // The device changing is itself a volume change from a script's point of
    // view: the number it should display just became a different number.
    changed(0, 0, std::ptr::null(), std::ptr::null_mut())
}

/// Both elements of both properties, which is what has to be watched to catch
/// every device's idea of a volume change.
const WATCHED: [(u32, u32); 4] = [
    (VOLUME_SCALAR, ELEMENT_MAIN),
    (VOLUME_SCALAR, ELEMENT_LEFT),
    (MUTE, ELEMENT_MAIN),
    (MUTE, ELEMENT_LEFT),
];

impl State {
    fn unlisten(&mut self) {
        let Some(previous) = self.listening_to.take() else {
            return;
        };
        for (selector, element) in WATCHED {
            remove_listener(
                previous,
                PropertyAddress {
                    selector,
                    scope: SCOPE_OUTPUT,
                    element,
                },
                changed,
            );
        }
    }

    fn relisten(&mut self) {
        self.unlisten();
        let Some(device) = default_output_device() else {
            return;
        };
        for (selector, element) in WATCHED {
            add_listener(
                device,
                PropertyAddress {
                    selector,
                    scope: SCOPE_OUTPUT,
                    element,
                },
                changed,
            );
        }
        self.listening_to = Some(device);
    }
}

/// Deregisters the listeners when the source stops.
struct Listeners;

impl Drop for Listeners {
    fn drop(&mut self) {
        STATE.with_borrow_mut(|slot| {
            if let Some(state) = slot.as_mut() {
                state.unlisten();
            }
            *slot = None;
        });
        remove_listener(
            SYSTEM_OBJECT,
            PropertyAddress::new(DEFAULT_OUTPUT_DEVICE, SCOPE_GLOBAL),
            device_changed,
        );
    }
}

pub struct Volume;

impl Source for Volume {
    fn name(&self) -> &'static str {
        "volume"
    }

    fn provides(&self) -> Vec<Event> {
        vec![Event::VolumeChanged]
    }

    fn install(&mut self, emit: Emitter) -> Result<Box<dyn std::any::Any>, StartError> {
        if default_output_device().is_none() {
            return Err(StartError {
                name: "volume",
                reason: "there is no default output device".to_owned(),
            });
        }

        STATE.with_borrow_mut(|slot| {
            let mut state = State {
                emit,
                listening_to: None,
                last: -1.0,
            };
            state.relisten();
            *slot = Some(state);
        });

        let status = add_listener(
            SYSTEM_OBJECT,
            PropertyAddress::new(DEFAULT_OUTPUT_DEVICE, SCOPE_GLOBAL),
            device_changed,
        );
        if status != 0 {
            return Err(StartError {
                name: "volume",
                reason: format!("could not watch the default output device ({status})"),
            });
        }

        Ok(Box::new(Listeners))
    }
}

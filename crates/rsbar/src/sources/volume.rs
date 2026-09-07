//! Output volume changes, from `CoreAudio`.
//!
//! Two listeners, not one. The volume of the current output device answers
//! "how loud is it", but the *default output device* can itself change —
//! plugging in headphones — and the old device's volume then means nothing.
//! So the device-change listener re-registers the volume listener on whatever
//! became default.

use crate::sources::{
    CallbackState, Cause, Emitter, Registering, Registration, Source, SourceId, StartError,
};
use rsbar_protocol::event::VolumeChange;
use rsbar_protocol::{Event, Kind};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
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

    /// Registers `listener`, which will be handed `context` on every call.
    ///
    /// # Safety
    ///
    /// `context` must stay valid and shared-referenceable for as long as the
    /// registration lives — `CoreAudio` may call back from its own thread at
    /// any time until the matching [`remove_listener`].
    pub(super) unsafe fn add_listener(
        object: AudioObjectId,
        address: PropertyAddress,
        listener: ListenerProc,
        context: *mut c_void,
    ) -> OsStatus {
        // SAFETY: `address` outlives the call; the caller guarantees `context`.
        unsafe { AudioObjectAddPropertyListener(object, &raw const address, listener, context) }
    }

    /// Removes a registration made by [`add_listener`].
    ///
    /// `CoreAudio` matches on the listener *and* the context, so this has to be
    /// given the same pointer the registration was made with or the listener
    /// stays installed.
    ///
    /// # Safety
    ///
    /// Same requirement as [`add_listener`]: `context` must still be the
    /// pointer that registration used.
    pub(super) unsafe fn remove_listener(
        object: AudioObjectId,
        address: PropertyAddress,
        listener: ListenerProc,
        context: *mut c_void,
    ) {
        // SAFETY: the caller passes the pointer the registration used.
        unsafe {
            AudioObjectRemovePropertyListener(object, &raw const address, listener, context);
        }
    }
}

use ffi_safe::{add_listener, get, remove_listener};
use std::collections::BTreeSet;

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
fn percent(scalar: f32) -> u8 {
    (scalar * 100.0).round().clamp(0.0, 100.0) as u8
}

/// Output volume as a whole percentage, or `None` if there is no output device.
#[must_use]
pub fn current_percentage() -> Option<u8> {
    current_scalar().map(percent)
}

/// What the listeners need, reachable from whatever thread `CoreAudio` calls
/// back on.
///
/// The previous version kept this in a `thread_local`, on the belief that
/// `CoreAudio` delivers on the thread that registered. It does not — by default
/// it uses an internal notification thread — so the callback found an empty
/// slot and every volume change was silently dropped. The state travels as the
/// listener's context pointer instead, which is what that argument is for.
///
/// No lock: the only mutable state is two words, and a callback that blocks is
/// a callback that stalls `CoreAudio`'s notification thread. Atomics also mean
/// there is no guard to poison and nothing that can panic in a callback.
struct Shared {
    emit: Emitter,
    /// The device currently listened to, or [`NO_DEVICE`].
    listening_to: AtomicU32,
    /// The last scalar reported, as bits. `CoreAudio` fires several times for
    /// one user-visible change — per channel, and for mute alongside volume —
    /// so only a real move is passed on.
    last: AtomicU32,
}

impl crate::sources::sealed::Sealed for Shared {}
impl crate::sources::Payload for Shared {}

/// `kAudioObjectUnknown`, which is never a real device.
const NO_DEVICE: AudioObjectId = 0;

/// How much the volume must move to count as a change. `SketchyBar`'s value.
const EPSILON: f32 = 1e-2;

extern "C-unwind" fn changed(
    _object: AudioObjectId,
    _count: u32,
    _addresses: *const PropertyAddress,
    context: *mut c_void,
) -> OsStatus {
    // SAFETY: every registration passes the leaked `Shared`.
    let Some(shared) = (unsafe { CallbackState::<Shared>::recover(context) }) else {
        return 0;
    };
    let Some(scalar) = current_scalar() else {
        return 0;
    };

    let last = f32::from_bits(shared.last.load(Ordering::Relaxed));
    if (scalar - last).abs() < EPSILON {
        return 0;
    }
    shared.last.store(scalar.to_bits(), Ordering::Relaxed);

    // Dropping beats blocking: this is a `CoreAudio` callback.
    shared.emit.send(Event::VolumeChanged(VolumeChange {
        volume: percent(scalar),
    }));
    0
}

/// Re-points the volume listener at whatever is now the default output.
extern "C-unwind" fn device_changed(
    _object: AudioObjectId,
    _count: u32,
    _addresses: *const PropertyAddress,
    context: *mut c_void,
) -> OsStatus {
    // SAFETY: every registration passes the source's `CallbackState<Shared>`.
    if let Some(shared) = unsafe { CallbackState::<Shared>::recover(context) } {
        shared.relisten();
    }
    // The device changing is itself a volume change from a script's point of
    // view: the number it should display just became a different number.
    changed(0, 0, std::ptr::null(), context)
}

/// Both elements of both properties, which is what has to be watched to catch
/// every device's idea of a volume change.
const WATCHED: [(u32, u32); 4] = [
    (VOLUME_SCALAR, ELEMENT_MAIN),
    (VOLUME_SCALAR, ELEMENT_LEFT),
    (MUTE, ELEMENT_MAIN),
    (MUTE, ELEMENT_LEFT),
];

impl Shared {
    /// The pointer every registration was made with is this state's own
    /// address, so it is recovered rather than threaded through.
    fn unlisten(&self) {
        let previous = self.listening_to.swap(NO_DEVICE, Ordering::Relaxed);
        if previous == NO_DEVICE {
            return;
        }
        for (selector, element) in WATCHED {
            CallbackState::with_ptr_of(self, |context| {
                // SAFETY: the same callback and context the registration used.
                unsafe {
                    remove_listener(
                        previous,
                        PropertyAddress {
                            selector,
                            scope: SCOPE_OUTPUT,
                            element,
                        },
                        changed,
                        context,
                    );
                }
            });
        }
    }

    fn relisten(&self) {
        self.unlisten();
        let Some(device) = default_output_device() else {
            return;
        };
        for (selector, element) in WATCHED {
            CallbackState::with_ptr_of(self, |context| {
                // SAFETY: as above.
                unsafe {
                    add_listener(
                        device,
                        PropertyAddress {
                            selector,
                            scope: SCOPE_OUTPUT,
                            element,
                        },
                        changed,
                        context,
                    );
                }
            });
        }
        self.listening_to.store(device, Ordering::Relaxed);
    }
}

/// Deregisters the listeners when the source stops.
struct Listeners(CallbackState<Shared>);

impl Drop for Listeners {
    fn drop(&mut self) {
        self.0.get().unlisten();
        self.0.with_ptr(|context| {
            // SAFETY: the same callback and context the registration used.
            unsafe {
                remove_listener(
                    SYSTEM_OBJECT,
                    PropertyAddress::new(DEFAULT_OUTPUT_DEVICE, SCOPE_GLOBAL),
                    device_changed,
                    context,
                );
            }
        });
    }
}

pub struct Volume;

impl Source for Volume {
    fn id(&self) -> SourceId {
        SourceId("volume")
    }

    fn provides(&self) -> Vec<Kind> {
        vec![Kind::VolumeChanged]
    }

    fn register(
        &mut self,
        _wanted: &BTreeSet<Kind>,
        cx: &mut Registering<'_>,
    ) -> Result<Registration, StartError> {
        let emit = cx.emitter();
        if default_output_device().is_none() {
            return Err(StartError::new(self.id(), Cause::NoOutputDevice));
        }

        let state = CallbackState::new(Shared {
            emit,
            listening_to: AtomicU32::new(NO_DEVICE),
            last: AtomicU32::new((-1.0f32).to_bits()),
        });
        state.get().relisten();

        let status = state.with_ptr(|context| {
            // SAFETY: the state outlives the listener; it is never freed.
            unsafe {
                add_listener(
                    SYSTEM_OBJECT,
                    PropertyAddress::new(DEFAULT_OUTPUT_DEVICE, SCOPE_GLOBAL),
                    device_changed,
                    context,
                )
            }
        });
        if status != 0 {
            return Err(StartError::new(self.id(), Cause::CoreAudio(status)));
        }

        // The state is deliberately never freed. `CoreAudio` delivers on a
        // thread of its own and offers no call that waits for an in-flight
        // callback to finish, so releasing it at deregistration races a
        // callback already running — a use-after-free that would surface as a
        // rare crash on quit. The other sources reclaim theirs; this one holds
        // a second reference so dropping `Listeners` cannot free it.
        CallbackState::leak(CallbackState::clone_of(&state));
        Ok(Box::new(Listeners(state)))
    }
}

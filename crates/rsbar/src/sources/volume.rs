//! Output volume changes, from `CoreAudio`.
//!
//! Two listeners, not one. The volume of the current output device answers
//! "how loud is it", but the *default output device* can itself change —
//! plugging in headphones — and the old device's volume then means nothing.
//! So the device-change listener re-registers the volume listener on whatever
//! became default.

use crate::sources::{Cause, Registering, Source, SourceId, StartError};
use rsbar_protocol::event::VolumeChange;
use rsbar_protocol::{Event, Kind};
use skylight::callback::{Callback, Relay};
use std::collections::BTreeSet;
use std::ffi::c_void;
use std::sync::Arc;
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
#[derive(Clone, Copy, Debug)]
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

/// Reads a fixed-size property, or `None` if the object does not have it —
/// which is ordinary, not exceptional: plenty of devices lack a software
/// volume control.
fn get<T: Copy + Default>(object: AudioObjectId, address: PropertyAddress) -> Option<T> {
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

/// `noErr`: what a property listener returns, whether or not it did anything.
///
/// Also what a listener whose registration has gone returns — unlike Carbon's
/// handlers, nothing chains here, so there is no event to swallow and "fine"
/// is the honest answer.
const NO_ERROR: OsStatus = 0;

/// One `CoreAudio` property listener: what it watches, and what it calls.
///
/// `AudioObjectRemovePropertyListener` matches on the object, the address,
/// the callback *and* the context, so a removal differing from its
/// registration in any of the four silently leaves the listener installed —
/// which is what used to be spelled out at four call sites, each casting a
/// context pointer out by hand. Here the four travel together in one value and
/// the fourth is the pointer the registration was made with, passed in rather
/// than recomputed, so add and remove cannot disagree.
#[derive(Clone, Copy, Debug)]
struct PropertyListener {
    object: AudioObjectId,
    address: PropertyAddress,
    proc: ListenerProc,
}

impl PropertyListener {
    /// A listener on `address` of `object`, calling `handler`.
    fn on(object: AudioObjectId, address: PropertyAddress, proc: ListenerProc) -> Self {
        Self {
            object,
            address,
            proc,
        }
    }

    /// Installs it against `context`, which `CoreAudio` keeps and hands back.
    fn add(self, context: *mut c_void) -> OsStatus {
        // SAFETY: `address` outlives the call, and `context` is a weak
        // reference the handle keeps for as long as the registration lives.
        unsafe {
            AudioObjectAddPropertyListener(self.object, &raw const self.address, self.proc, context)
        }
    }

    /// Removes it. The same four values the registration used, because it is
    /// the same value and the same pointer.
    fn remove(self, context: *mut c_void) {
        // SAFETY: as above.
        unsafe {
            AudioObjectRemovePropertyListener(
                self.object,
                &raw const self.address,
                self.proc,
                context,
            );
        }
    }
}

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

/// `kAudioObjectUnknown`, which is never a real device.
const NO_DEVICE: AudioObjectId = 0;

/// How much the volume must move to count as a change. `SketchyBar`'s value.
const EPSILON: f32 = 1e-2;

/// The state both `CoreAudio` callbacks are handed, reachable from whatever
/// thread `CoreAudio` calls back on — by default an internal notification
/// thread of its own, not the one that registered.
///
/// `device` is an atomic rather than something behind a lock because
/// `device_changed` and the install closure both touch it synchronously, and a
/// callback that blocks is a callback that stalls `CoreAudio`'s notification
/// thread.
struct Listening {
    relay: Arc<Relay<()>>,
    device: Arc<AtomicU32>,
}

/// What `CoreAudio` calls when a volume or mute property moves.
///
/// Ignores everything it was handed: which channel or property fired does not
/// change what gets read back, so there is nothing here to do but say "look
/// again" and let [`decide`] re-read through [`current_scalar`].
fn volume_moved(
    _object: AudioObjectId,
    _count: u32,
    _addresses: *const PropertyAddress,
    state: &Listening,
) -> OsStatus {
    state.relay.post(());
    NO_ERROR
}

/// What `CoreAudio` calls when the default output device changes.
///
/// Takes the context because it re-points the volume listeners at the new
/// device, and `AudioObjectRemovePropertyListener` matches removal on that
/// pointer — so it has nowhere else to get one. That repointing is a
/// registration change, not reporting work, so it happens here rather than in
/// [`decide`]; the device changing is itself a volume change from a script's
/// point of view, so this posts too.
fn device_changed(
    _object: AudioObjectId,
    _count: u32,
    _addresses: *const PropertyAddress,
    state: &Listening,
    context: *mut c_void,
) -> OsStatus {
    relisten(&state.device, context);
    state.relay.post(());
    NO_ERROR
}

/// Both elements of both properties, which is what has to be watched to catch
/// every device's idea of a volume change.
const WATCHED: [(u32, u32); 4] = [
    (VOLUME_SCALAR, ELEMENT_MAIN),
    (VOLUME_SCALAR, ELEMENT_LEFT),
    (MUTE, ELEMENT_MAIN),
    (MUTE, ELEMENT_LEFT),
];

/// Both take the context pointer rather than deriving one, because that is
/// what `CoreAudio` matches a removal on — see [`PropertyListener`]. Free
/// functions rather than methods, so the install closure can call them on its
/// own `device` clone before `device_changed` ever fires.
fn unlisten(device: &AtomicU32, context: *mut c_void) {
    let previous = device.swap(NO_DEVICE, Ordering::Relaxed);
    if previous == NO_DEVICE {
        return;
    }
    for (selector, element) in WATCHED {
        volume_listener(previous, selector, element).remove(context);
    }
}

fn relisten(device: &AtomicU32, context: *mut c_void) {
    unlisten(device, context);
    let Some(new_device) = default_output_device() else {
        return;
    };
    for (selector, element) in WATCHED {
        volume_listener(new_device, selector, element).add(context);
    }
    device.store(new_device, Ordering::Relaxed);
}

/// One of the four volume-or-mute properties, on one device.
fn volume_listener(device: AudioObjectId, selector: u32, element: u32) -> PropertyListener {
    PropertyListener::on(
        device,
        PropertyAddress {
            selector,
            scope: SCOPE_OUTPUT,
            element,
        },
        VOLUME_MOVED,
    )
}

skylight::trampoline!(DEVICE_CHANGED = device_changed(
    object: AudioObjectId,
    count: u32,
    addresses: *const PropertyAddress,
    @ state: &Listening,
) + context -> OsStatus; dropped NO_ERROR);

skylight::trampoline!(VOLUME_MOVED = volume_moved(
    object: AudioObjectId,
    count: u32,
    addresses: *const PropertyAddress,
    @ state: &Listening,
) -> OsStatus; dropped NO_ERROR);

/// Reports a scalar, if it moved by more than [`EPSILON`] since the last one
/// reported. `CoreAudio` fires several times for one user-visible change — per
/// channel, and for mute alongside volume — so only a real move goes out.
///
/// The loop ends on its own: dropping the [`Callback`] drops the last
/// [`Relay`], `next` answers `None`, and this returns.
async fn decide(mut ticks: skylight::callback::Events<()>, emit: crate::sources::Emitter) {
    let mut last = -1.0f32;
    while ticks.next().await.is_some() {
        let Some(scalar) = current_scalar() else {
            continue;
        };
        if (scalar - last).abs() < EPSILON {
            continue;
        }
        last = scalar;
        emit.send(Event::VolumeChanged(VolumeChange {
            volume: percent(scalar),
        }));
    }
}

#[derive(Default)]
pub struct Volume;

impl Source for Volume {
    fn id(&self) -> SourceId {
        SourceId("volume")
    }

    fn provides(&self) -> Vec<Kind> {
        vec![Kind::VolumeChanged]
    }

    fn run(
        &mut self,
        _wanted: &BTreeSet<Kind>,
        cx: Registering,
    ) -> Result<crate::pool::Task, StartError> {
        if default_output_device().is_none() {
            return Err(StartError::new(self.id(), Cause::NoOutputDevice));
        }

        let emit = cx.emitter();
        let id = self.id();

        // `!Send` means "build it where it will live", not "put it on the
        // main thread": `CoreAudio` delivers from a notification thread of
        // its own regardless of which thread holds the registration, and
        // needs no run loop, so the listeners are built on
        // `pool::sources()`'s thread rather than the one that draws the bar.
        //
        // The cost: a registration failure past this point can no longer
        // come back through this `run`'s `Result`, since building it happens
        // after this function has already returned `Ok`. Logged instead —
        // the same choice `wifi` and `power` make.
        Ok(crate::pool::sources().spawn(move |_here| async move {
            let (relay, ticks) = skylight::callback::relay::<()>();

            let device = Arc::new(AtomicU32::new(NO_DEVICE));
            let state = Arc::new(Listening {
                relay,
                device: Arc::clone(&device),
            });
            let listeners = match Callback::new(state, move |context| {
                // The device listener, plus the four volume-and-mute ones
                // `relisten` installs. `AudioObjectRemovePropertyListener`
                // matches on object, address, callback *and* context, so the
                // teardown repeats exactly what went in.
                let listener = PropertyListener::on(
                    SYSTEM_OBJECT,
                    PropertyAddress::new(DEFAULT_OUTPUT_DEVICE, SCOPE_GLOBAL),
                    DEVICE_CHANGED,
                );
                relisten(&device, context);

                let status = listener.add(context);
                if status != NO_ERROR {
                    // Nothing may still hold the pointer on an error path.
                    unlisten(&device, context);
                    return Err(Cause::CoreAudio(status));
                }
                Ok(move || {
                    unlisten(&device, context);
                    listener.remove(context);
                })
            }) {
                Ok(listeners) => listeners,
                Err(cause) => {
                    let err = StartError::new(id, cause);
                    tracing::error!(%err, "volume source could not register on its own thread");
                    return;
                }
            };
            let _listeners = listeners;
            decide(ticks, emit).await;
        }))
    }
}

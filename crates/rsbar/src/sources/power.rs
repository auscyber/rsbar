//! Power source changes, from `IOKit`.
//!
//! # Charge, time remaining and wattage — task #16
//!
//! `IOPSCopyPowerSourcesInfo`'s snapshot is not only "AC or battery": the
//! same blob lists every power source (the internal battery, on anything
//! that has one) with its charge, whether it is taking charge, and minutes
//! to empty or to full, and a sibling call,
//! `IOPSCopyExternalPowerAdapterDetails`, answers the attached adapter's
//! wattage. All of it moves on the one notification this file already
//! installs, so there is no second facility to watch and nothing to poll —
//! see [`read`]. That is also why this stays one module rather than a
//! `battery` source next to it: a script watching `power_source_changed`
//! wants the whole picture in the one payload it already reacts to, not a
//! second event to subscribe to and a second script run for the same
//! moment.
//!
//! `PowerChange` does not carry charge, time or wattage yet — see this
//! crate's task-#16 report for the exact fields asked for. Until it does,
//! [`changed`] tracks the full [`Snapshot`] and only forwards the field the
//! wire format has today, logging the rest so the read and the dedup are
//! provably correct ahead of the payload landing.

use crate::sources::{
    CallbackState, Cause, Emitter, Registering, Registration, Source, SourceId, StartError,
};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFRunLoop, CFRunLoopSource, CFString,
    CFType,
};
use rsbar_protocol::event::PowerChange;
use rsbar_protocol::{Event, Kind, PowerSource};
use std::collections::BTreeSet;
use std::ffi::c_void;
use std::ptr::NonNull;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    /// A run loop source that fires whenever the power source situation
    /// changes. Nothing else in `IOKit` reports this without polling.
    fn IOPSNotificationCreateRunLoopSource(
        callback: extern "C-unwind" fn(*mut c_void),
        context: *mut c_void,
    ) -> *mut CFRunLoopSource;

    fn IOPSCopyPowerSourcesInfo() -> *mut CFType;
    fn IOPSGetProvidingPowerSourceType(snapshot: *mut CFType) -> *const CFString;
    /// The power sources `snapshot` describes, as opaque tokens only
    /// [`IOPSGetPowerSourceDescription`] knows how to read. A +1 `CFArray`.
    fn IOPSCopyPowerSourcesList(snapshot: *mut CFType) -> *mut CFArray;
    /// The dictionary describing one entry from that list. Borrowed from
    /// `snapshot` — this must not be released.
    fn IOPSGetPowerSourceDescription(
        snapshot: *mut CFType,
        source: *const CFType,
    ) -> *mut CFDictionary;
    /// The wattage, current and voltage of whatever is plugged in, or null
    /// on battery. A +1 `CFDictionary`.
    fn IOPSCopyExternalPowerAdapterDetails() -> *mut CFDictionary;
}

/// String keys `IOPSGetPowerSourceDescription`'s and
/// `IOPSCopyExternalPowerAdapterDetails`' dictionaries answer to.
/// `IOPSKeys.h` only ships these as strings, not as bindable symbols, the
/// same way `providing_source` already matches `"AC Power"`/`"Battery
/// Power"` as literals rather than linked constants.
mod keys {
    pub const TYPE: &str = "Type";
    pub const INTERNAL_BATTERY: &str = "InternalBattery";
    pub const CURRENT_CAPACITY: &str = "Current Capacity";
    pub const MAX_CAPACITY: &str = "Max Capacity";
    pub const IS_CHARGING: &str = "Is Charging";
    pub const TIME_TO_EMPTY: &str = "Time to Empty";
    pub const TIME_TO_FULL_CHARGE: &str = "Time to Full Charge";
    pub const ADAPTER_WATTS: &str = "Watts";
}

fn dict_value<'a>(dict: &'a CFDictionary, key: &str) -> Option<&'a CFType> {
    let key = CFString::from_str(key);
    // SAFETY: `key` is a live `CFString` for the call, and the pointer
    // returned borrows from `dict`, which outlasts the `'a` handed back.
    let ptr = unsafe { dict.value(CFRetained::as_ptr(&key).as_ptr().cast()) };
    let ptr = NonNull::new(ptr.cast_mut())?.cast::<CFType>();
    // SAFETY: the pointer came from `dict`, so it lives as long as `dict`.
    Some(unsafe { ptr.as_ref() })
}

fn dict_i64(dict: &CFDictionary, key: &str) -> Option<i64> {
    dict_value(dict, key)?.downcast_ref::<CFNumber>()?.as_i64()
}

fn dict_bool(dict: &CFDictionary, key: &str) -> Option<bool> {
    Some(
        dict_value(dict, key)?
            .downcast_ref::<CFBoolean>()?
            .as_bool(),
    )
}

fn dict_string(dict: &CFDictionary, key: &str) -> Option<String> {
    Some(
        dict_value(dict, key)?
            .downcast_ref::<CFString>()?
            .to_string(),
    )
}

/// A negative reading is `IOKit`'s "not applicable or still calculating",
/// for both `Time to Empty` and `Time to Full Charge`.
fn non_negative_minutes(raw: Option<i64>) -> Option<u32> {
    raw.and_then(|m| u32::try_from(m).ok())
}

fn power_source_of(snapshot: *mut CFType) -> PowerSource {
    // SAFETY: `snapshot` is a live blob for the duration of this call.
    let kind = unsafe { IOPSGetProvidingPowerSourceType(snapshot) };
    if kind.is_null() {
        return PowerSource::Unknown;
    }
    // SAFETY: `kind` is borrowed from `snapshot`, which is live here.
    match unsafe { &*kind }.to_string().as_str() {
        "AC Power" => PowerSource::Ac,
        "Battery Power" => PowerSource::Battery,
        _ => PowerSource::Unknown,
    }
}

/// Whether the machine is on mains or on battery right now.
#[must_use]
pub fn providing_source() -> PowerSource {
    // SAFETY: the snapshot is a +1 `CoreFoundation` object released below.
    unsafe {
        let snapshot = IOPSCopyPowerSourcesInfo();
        if snapshot.is_null() {
            return PowerSource::Unknown;
        }
        let source = power_source_of(snapshot);
        skylight::ffi::CFRelease(snapshot);
        source
    }
}

/// What one power source entry in the snapshot says about the internal
/// battery.
#[derive(Debug, Clone, Copy, PartialEq)]
struct BatteryReading {
    charge: u8,
    charging: bool,
    time_to_empty_minutes: Option<u32>,
    time_to_full_minutes: Option<u32>,
}

#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn charge_percent(current: i64, max: i64) -> u8 {
    (current * 100 / max).clamp(0, 100) as u8
}

/// The internal battery's reading from `snapshot`, or `None` on hardware
/// with no battery at all — a Mac mini or Studio has no `InternalBattery`
/// entry in the list.
fn internal_battery(snapshot: *mut CFType) -> Option<BatteryReading> {
    // SAFETY: `snapshot` is live for the duration of this call.
    let list = NonNull::new(unsafe { IOPSCopyPowerSourcesList(snapshot) })?;
    // SAFETY: the Copy convention hands back a reference this now owns.
    let list = unsafe { CFRetained::<CFArray>::from_raw(list) };

    for i in 0..list.count() {
        // SAFETY: `i` is in `0..list.count()`, so in bounds.
        let source = unsafe { list.value_at_index(i) };
        // SAFETY: `snapshot` and `source` are both live; the dictionary
        // this returns is borrowed from `snapshot`, so it is not released.
        let dict = unsafe { IOPSGetPowerSourceDescription(snapshot, source.cast()) };
        let Some(dict) = NonNull::new(dict) else {
            continue;
        };
        // SAFETY: borrowed from `snapshot`, which is live here.
        let dict = unsafe { dict.as_ref() };
        if dict_string(dict, keys::TYPE).as_deref() != Some(keys::INTERNAL_BATTERY) {
            continue;
        }
        let max = dict_i64(dict, keys::MAX_CAPACITY).filter(|&m| m > 0)?;
        let current = dict_i64(dict, keys::CURRENT_CAPACITY)?;
        let charging = dict_bool(dict, keys::IS_CHARGING).unwrap_or(false);
        // Confirmed live: `Time to Full Charge` reads `0`, not the documented
        // `-1`, while this machine is discharging -- `0` is ambiguous between
        // "full charge is 0 minutes away" and "not applicable", so which
        // field is even asked for is gated on `charging` rather than trusting
        // the sign alone.
        return Some(BatteryReading {
            charge: charge_percent(current, max),
            charging,
            time_to_empty_minutes: (!charging)
                .then(|| non_negative_minutes(dict_i64(dict, keys::TIME_TO_EMPTY)))
                .flatten(),
            time_to_full_minutes: charging
                .then(|| non_negative_minutes(dict_i64(dict, keys::TIME_TO_FULL_CHARGE)))
                .flatten(),
        });
    }
    None
}

/// The attached adapter's wattage, or `None` on battery, or when the
/// adapter does not report one — a courtesy some report, not a guarantee.
fn adapter_watts() -> Option<u32> {
    // SAFETY: takes no arguments; a +1 `CFDictionary`, or null on battery.
    let details = NonNull::new(unsafe { IOPSCopyExternalPowerAdapterDetails() })?;
    // SAFETY: the Copy convention hands back a reference this now owns.
    let details = unsafe { CFRetained::<CFDictionary>::from_raw(details) };
    u32::try_from(dict_i64(&details, keys::ADAPTER_WATTS)?).ok()
}

/// Everything one read of the power situation answers: which source the
/// machine is drawing from, the internal battery's state, and the
/// attached adapter's wattage.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Snapshot {
    power_source: PowerSource,
    watts: Option<u32>,
    charge: Option<u8>,
    charging: Option<bool>,
    time_to_empty_minutes: Option<u32>,
    time_to_full_minutes: Option<u32>,
}

impl Snapshot {
    /// Whether this differs from `other` in a way an item would draw.
    ///
    /// The time estimates are deliberately not part of this. `IOKit` notifies
    /// about once a minute whether or not anything happened, and the estimate
    /// wobbles by a minute or two every time it does, so treating those as a
    /// change means a script run per minute forever for a number nobody
    /// watches tick. They still travel in the payload, so an item woken for
    /// any other reason draws a current one.
    fn differs_from(&self, other: &Self) -> bool {
        self.power_source != other.power_source
            || self.charge != other.charge
            || self.charging != other.charging
            || self.watts != other.watts
    }

    fn into_event(self) -> PowerChange {
        PowerChange {
            power_source: self.power_source,
            watts: self.watts.into(),
            charge: self.charge.into(),
            charging: self.charging.into(),
            time_to_empty_minutes: self.time_to_empty_minutes.into(),
            time_to_full_minutes: self.time_to_full_minutes.into(),
        }
    }
}

/// One combined read: the `IOPSCopyPowerSourcesInfo` snapshot for the power
/// source and the battery, plus the sibling `IOPSCopyExternalPowerAdapterDetails`
/// call for wattage.
fn read() -> Snapshot {
    // SAFETY: a +1 `CoreFoundation` object, released below.
    let snapshot = unsafe { IOPSCopyPowerSourcesInfo() };
    if snapshot.is_null() {
        return Snapshot {
            power_source: PowerSource::Unknown,
            watts: None,
            charge: None,
            charging: None,
            time_to_empty_minutes: None,
            time_to_full_minutes: None,
        };
    }
    let power_source = power_source_of(snapshot);
    let battery = internal_battery(snapshot);
    // SAFETY: `snapshot` is the +1 reference `IOPSCopyPowerSourcesInfo`
    // returned, and nothing above retains a pointer into it afterwards.
    unsafe { skylight::ffi::CFRelease(snapshot) };

    Snapshot {
        power_source,
        watts: adapter_watts(),
        charge: battery.map(|b| b.charge),
        charging: battery.map(|b| b.charging),
        time_to_empty_minutes: battery.and_then(|b| b.time_to_empty_minutes),
        time_to_full_minutes: battery.and_then(|b| b.time_to_full_minutes),
    }
}

/// State the `IOKit` callback is handed a pointer to: the sink, and the
/// last snapshot sent — so a notification that changed nothing real
/// (`IOKit` fires this for far more than an AC/battery flip: a battery
/// percent tick, an adapter settling on a reported wattage, anything in the
/// snapshot) does not turn into an event, a script run and a repaint for
/// values nothing actually moved.
pub struct State {
    emit: Emitter,
    last: tokio::sync::Mutex<Option<Snapshot>>,
}

/// Keeps the notification source installed; dropping it stops the events.
struct Watch {
    source: CFRetained<CFRunLoopSource>,
    run_loop: CFRetained<CFRunLoop>,
    /// The state `IOKit` was handed a pointer to. Held, never read: the
    /// callback recovers it from that pointer, so it has to outlive the
    /// registration. Dropping it here — after `Drop` has removed the source —
    /// is what makes that safe.
    _state: CallbackState<State>,
}

impl Watch {
    /// Starts reporting power source changes on the current run loop.
    fn install(state: CallbackState<State>) -> Result<Self, StartError> {
        extern "C-unwind" fn changed(context: *mut c_void) {
            // SAFETY: the registration passed a `CallbackState<State>` pointer.
            let Some(state) = (unsafe { CallbackState::<State>::recover(context) }) else {
                return;
            };
            let snapshot = read();

            // `blocking_lock`, not an await: this runs inside `IOKit`'s
            // callback on the run loop, never inside a polled task. See
            // `mod.rs`'s note on `tokio::sync` over `std::sync` — the lock
            // type is kept consistent with every other source's rather than
            // reached for only because this one callback allows it.
            let mut last = state.last.blocking_lock();
            let worth_reporting = last
                .as_ref()
                .is_none_or(|prev| prev.differs_from(&snapshot));
            *last = Some(snapshot);
            drop(last);

            if worth_reporting {
                // Dropping beats blocking: this is an `IOKit` callback.
                state
                    .emit
                    .send(Event::PowerSourceChanged(snapshot.into_event()));
            }
        }

        let source = state.with_ptr(|context| {
            // SAFETY: the state outlives the registration; see `CallbackState`.
            unsafe { IOPSNotificationCreateRunLoopSource(changed, context) }
        });
        let Some(source) = std::ptr::NonNull::new(source) else {
            return Err(StartError::new(SourceId("power"), Cause::IoKit));
        };
        // SAFETY: the call returns a +1 reference we now own.
        let source = unsafe { CFRetained::from_raw(source) };

        let run_loop = CFRunLoop::current().expect("no run loop on this thread");
        run_loop.add_source(Some(&source), unsafe {
            objc2_core_foundation::kCFRunLoopCommonModes
        });
        Ok(Self {
            source,
            run_loop,
            _state: state,
        })
    }
}

pub struct Power;

impl Source for Power {
    fn id(&self) -> SourceId {
        SourceId("power")
    }

    fn provides(&self) -> Vec<Kind> {
        vec![Kind::PowerSourceChanged]
    }

    fn register(
        &mut self,
        _wanted: &BTreeSet<Kind>,
        cx: &mut Registering<'_>,
    ) -> Result<Registration, StartError> {
        let emit = cx.emitter();
        let state = CallbackState::new(State {
            emit,
            last: tokio::sync::Mutex::new(None),
        });
        Ok(Box::new(Watch::install(state)?))
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.run_loop.remove_source(Some(&self.source), unsafe {
            objc2_core_foundation::kCFRunLoopCommonModes
        });
    }
}

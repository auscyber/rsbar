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
//! `PowerChange` carries all of it. The AC-or-battery split is the
//! [`PowerSource`] enum itself, which holds the numbers only its own side
//! has — the adapter's rating and minutes-to-full on mains, minutes-to-empty
//! on battery — so a reading cannot claim both. What is left flat is what
//! both sides share: `charge` and the measured `watts`, each an `Option`
//! because hardware with no battery (a Mac mini, a Studio) has neither.
//!
//! The two wattages are different numbers and both are worth having.
//! `adapter_watts` is what is plugged in says it can supply, a nameplate
//! rating that never moves; `watts` is what is actually flowing through the
//! battery this moment, from a third facility again — see [`measured_watts`]. [`Snapshot::differs_from`]
//! still excludes the two time estimates from the dedup: see its own doc for
//! why.

use crate::protocol::event::PowerChange;
use crate::protocol::{Event, Kind, PowerSource};
use crate::sources::{Cause, Registering, Source, SourceId, StartError};
use objc2_core_foundation::{CFArray, CFDictionary, CFRetained, CFRunLoopSource, CFString, CFType};
use skylight::callback::{Callback, Relay};
use skylight::cf::{Array, Dict, DictKey};
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

    /// A matching dictionary naming one `IOKit` class. A +1 `CFDictionary`,
    /// which [`IOServiceGetMatchingService`] then consumes.
    fn IOServiceMatching(class: *const std::ffi::c_char) -> *mut CFDictionary;
    /// The first service matching `matching`, or `0`. Consumes `matching`
    /// whether or not it finds anything, so that must not be released.
    fn IOServiceGetMatchingService(main_port: u32, matching: *mut CFDictionary) -> u32;
    /// Every property a registry entry publishes. A +1 `CFDictionary` through
    /// the out-pointer, and `0` on success.
    fn IORegistryEntryCreateCFProperties(
        entry: u32,
        properties: *mut *mut CFDictionary,
        allocator: *const c_void,
        options: u32,
    ) -> i32;
    fn IOObjectRelease(object: u32) -> i32;
}

/// A key in the dictionaries `IOPSGetPowerSourceDescription` and
/// `IOPSCopyExternalPowerAdapterDetails` answer with.
///
/// A closed set, for the same reason [`skylight::cf::WindowKey`] is one: it
/// says which keys this daemon actually asks for, which scattered string
/// literals never did. Unlike that one there is no `unsafe` to fold away —
/// `IOPSKeys.h` ships these as C string macros rather than as exported
/// `CFString` symbols, so there is nothing to link against and the spelling
/// *is* the binding, the same way `providing_source` matches `"AC Power"`
/// as a literal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum PowerKey {
    /// Which kind of source an entry describes; [`INTERNAL_BATTERY`] is the
    /// only one this looks for.
    Type,
    /// Charge now and charge when full, in whatever unit the source counts
    /// in — a percentage only after dividing one by the other.
    CurrentCapacity,
    MaxCapacity,
    /// Whether the source is taking charge, not whether an adapter is
    /// attached: a full battery on mains is not charging.
    IsCharging,
    TimeToEmpty,
    TimeToFullCharge,
    /// On the adapter's dictionary rather than a power source's: what is
    /// plugged in, if it says.
    AdapterWatts,
}

/// The `Type` an internal battery reports.
const INTERNAL_BATTERY: &str = "InternalBattery";

impl PowerKey {
    const fn name(self) -> &'static str {
        match self {
            Self::Type => "Type",
            Self::CurrentCapacity => "Current Capacity",
            Self::MaxCapacity => "Max Capacity",
            Self::IsCharging => "Is Charging",
            Self::TimeToEmpty => "Time to Empty",
            Self::TimeToFullCharge => "Time to Full Charge",
            Self::AdapterWatts => "Watts",
        }
    }
}

impl DictKey for PowerKey {
    fn with_cf<R>(&self, f: impl FnOnce(&CFString) -> R) -> R {
        self.name().with_cf(f)
    }
}

/// A key in the property dictionary the `AppleSmartBattery` registry entry
/// publishes — the live measurements, which the `IOPS` snapshot has none of.
///
/// A closed set for the same reason [`PowerKey`] is, and confirmed present on
/// this machine (a `Mac16,1`) with `ioreg -c AppleSmartBattery` rather than
/// taken from the Intel-era key lists, which do not all survive on Apple
/// silicon. `AppleRawCurrentCapacity`, `InstantAmperage` and `BatteryPower`
/// are all there beside these; `Amperage` is preferred over `InstantAmperage`
/// because it is the averaged reading, and a bar showing a number that jumps
/// every second is worse than one that lags it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum BatteryKey {
    /// Pack current in mA, signed: negative discharging, positive charging.
    /// Stored as a 64-bit two's-complement value that `ioreg` prints
    /// unsigned; `CFNumber` hands it back as the `i64` it actually is.
    Amperage,
    /// Pack voltage in mV.
    Voltage,
}

impl BatteryKey {
    const fn name(self) -> &'static str {
        match self {
            Self::Amperage => "Amperage",
            Self::Voltage => "Voltage",
        }
    }
}

impl DictKey for BatteryKey {
    fn with_cf<R>(&self, f: impl FnOnce(&CFString) -> R) -> R {
        self.name().with_cf(f)
    }
}

/// The `IOKit` class publishing the internal battery's live measurements.
const SMART_BATTERY: &std::ffi::CStr = c"AppleSmartBattery";

/// The `AppleSmartBattery` registry entry, released when it goes.
///
/// An `io_service_t` is a mach port rather than a `CoreFoundation` object, so
/// `CFRetained` cannot hold it and `IOObjectRelease` is the release — written
/// once, here, so a reader is an ordinary `?` chain.
struct SmartBattery(u32);

impl SmartBattery {
    /// The battery's entry, or `None` on hardware with no battery at all.
    fn open() -> Option<Self> {
        // SAFETY: a NUL-terminated class name; a +1 matching dictionary.
        let matching = unsafe { IOServiceMatching(SMART_BATTERY.as_ptr()) };
        if matching.is_null() {
            return None;
        }
        // SAFETY: the call consumes `matching`, so it is deliberately not
        // released here. `0` is `kIOMainPortDefault`.
        let service = unsafe { IOServiceGetMatchingService(0, matching) };
        (service != 0).then_some(Self(service))
    }

    /// Every property the entry publishes.
    fn properties(&self) -> Option<CFRetained<CFDictionary>> {
        let mut out: *mut CFDictionary = std::ptr::null_mut();
        // SAFETY: the entry is live, and `out` is a valid out-pointer.
        let status =
            unsafe { IORegistryEntryCreateCFProperties(self.0, &raw mut out, std::ptr::null(), 0) };
        let out = NonNull::new(out).filter(|_| status == 0)?;
        // SAFETY: the Create convention hands back a reference this now owns.
        Some(unsafe { CFRetained::from_raw(out) })
    }
}

impl Drop for SmartBattery {
    fn drop(&mut self) {
        // SAFETY: the port `open` produced, released exactly once.
        unsafe { IOObjectRelease(self.0) };
    }
}

/// Power actually moving through the battery right now, in whole watts.
///
/// Not [`adapter_watts`], which is the adapter's nameplate rating: this is
/// `Amperage` × `Voltage`, the draw a person would recognise, and the number
/// `SketchyBar` has no way to show at all. A magnitude — `charging` already
/// says which direction it is going, so a sign in a `$WATTS` a script
/// interpolates would only print a leading minus half the time.
///
/// Cross-checked live against the same entry's
/// `PowerTelemetryData.SystemLoad`, which is the same figure in mW and agreed
/// exactly.
fn measured_watts() -> Option<u32> {
    let properties = SmartBattery::open()?.properties()?;
    let battery = Dict::new(&properties);
    let milliamps = battery.get::<i64>(BatteryKey::Amperage)?.unsigned_abs();
    let millivolts = u64::try_from(battery.get::<i64>(BatteryKey::Voltage)?).ok()?;
    // mA times mV is microwatts; rounded rather than truncated, so a 11.7 W
    // draw reads 12 rather than 11.
    let microwatts = milliamps.checked_mul(millivolts)?;
    u32::try_from((microwatts + 500_000) / 1_000_000).ok()
}

/// One `IOPSCopyPowerSourcesInfo` blob, released when it goes.
///
/// The blob has no concrete CF type — every call takes it as a bare
/// `CFTypeRef` — so the release used to be a hand-written `CFRelease` at each
/// of the two places that read one. `CFRetained<CFType>` is the same
/// ownership written once, and everything the blob answers hangs off it as a
/// method.
struct Info(CFRetained<CFType>);

impl Info {
    /// The current power situation, or `None` if `IOKit` would not say.
    fn read() -> Option<Self> {
        // SAFETY: takes no arguments; a +1 `CoreFoundation` object, or null.
        let info = NonNull::new(unsafe { IOPSCopyPowerSourcesInfo() })?;
        // SAFETY: the Copy convention hands back a reference this now owns.
        Some(Self(unsafe { CFRetained::from_raw(info) }))
    }

    fn as_ptr(&self) -> *mut CFType {
        CFRetained::as_ptr(&self.0).as_ptr()
    }

    /// Which side the machine is drawing from, before that side's own
    /// numbers are filled in.
    fn providing(&self) -> Side {
        // SAFETY: the blob is live for as long as `self` is.
        let kind = unsafe { IOPSGetProvidingPowerSourceType(self.as_ptr()) };
        if kind.is_null() {
            return Side::Unknown;
        }
        // SAFETY: `kind` is borrowed from the blob, which is live here.
        match unsafe { &*kind }.to_string().as_str() {
            "AC Power" => Side::Ac,
            "Battery Power" => Side::Battery,
            _ => Side::Unknown,
        }
    }

    /// The internal battery's reading, or `None` on hardware with no battery
    /// at all — a Mac mini or Studio has no `InternalBattery` entry.
    fn internal_battery(&self) -> Option<BatteryReading> {
        // SAFETY: the blob is live; a +1 `CFArray`, or null.
        let list = NonNull::new(unsafe { IOPSCopyPowerSourcesList(self.as_ptr()) })?;
        // SAFETY: the Copy convention hands back a reference this now owns.
        let list = unsafe { CFRetained::<CFArray>::from_raw(list) };
        let list = Array::new(&list);

        for index in 0..list.len() {
            let Some(source) = list.value(index) else {
                continue;
            };
            // SAFETY: the blob and `source` are both live; the dictionary
            // this returns is borrowed from the blob, so it is not released.
            let dict =
                unsafe { IOPSGetPowerSourceDescription(self.as_ptr(), std::ptr::from_ref(source)) };
            let Some(dict) = NonNull::new(dict) else {
                continue;
            };
            // SAFETY: borrowed from the blob, which is live here.
            let dict = Dict::new(unsafe { dict.as_ref() });

            if dict.get::<String>(PowerKey::Type).as_deref() != Some(INTERNAL_BATTERY) {
                continue;
            }
            let max = dict.get::<i64>(PowerKey::MaxCapacity).filter(|&m| m > 0)?;
            let current = dict.get::<i64>(PowerKey::CurrentCapacity)?;
            let charging = dict.get::<bool>(PowerKey::IsCharging).unwrap_or(false);
            // Confirmed live: `Time to Full Charge` reads `0`, not the
            // documented `-1`, while this machine is discharging -- `0` is
            // ambiguous between "full charge is 0 minutes away" and "not
            // applicable", so which field is even asked for is gated on
            // `charging` rather than trusting the sign alone.
            return Some(BatteryReading {
                charge: charge_percent(current, max),
                charging,
                time_to_empty_minutes: (!charging)
                    .then(|| {
                        dict.get::<i64>(PowerKey::TimeToEmpty)
                            .and_then(|m| u32::try_from(m).ok())
                    })
                    .flatten(),
                time_to_full_minutes: charging
                    .then(|| {
                        dict.get::<i64>(PowerKey::TimeToFullCharge)
                            .and_then(|m| u32::try_from(m).ok())
                    })
                    .flatten(),
            });
        }
        None
    }
}

/// Which side the machine is drawing from, with nothing attached.
///
/// `IOPSGetProvidingPowerSourceType` answers only this; the numbers that
/// distinguish the two [`PowerSource`] variants come from the battery entry
/// and the adapter dictionary, so they are put together in [`read`] rather
/// than guessed here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Ac,
    Battery,
    Unknown,
}

/// Whether the machine is on mains or on battery right now, with the numbers
/// that side carries.
#[must_use]
pub fn providing_source() -> PowerSource {
    read().power_source
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

/// The attached adapter's wattage, or `None` on battery, or when the
/// adapter does not report one — a courtesy some report, not a guarantee.
fn adapter_watts() -> Option<u32> {
    // SAFETY: takes no arguments; a +1 `CFDictionary`, or null on battery.
    let details = NonNull::new(unsafe { IOPSCopyExternalPowerAdapterDetails() })?;
    // SAFETY: the Copy convention hands back a reference this now owns.
    let details = unsafe { CFRetained::<CFDictionary>::from_raw(details) };
    let watts = Dict::new(&details).get::<i64>(PowerKey::AdapterWatts)?;
    u32::try_from(watts).ok()
}

/// Everything one read of the power situation answers: which source the
/// machine is drawing from, the internal battery's state, the power measured
/// flowing through it, and the attached adapter's rating.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Snapshot {
    power_source: PowerSource,
    watts: Option<u32>,
    charge: Option<u8>,
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
        self.charge != other.charge
            || self.watts != other.watts
            || !same_side(self.power_source, other.power_source)
    }

    fn into_event(self) -> PowerChange {
        PowerChange {
            power_source: self.power_source,
            watts: self.watts,
            charge: self.charge,
        }
    }
}

/// Whether two readings say the same thing about where power is coming from,
/// ignoring the time estimates — see [`Snapshot::differs_from`] for why.
fn same_side(a: PowerSource, b: PowerSource) -> bool {
    match (a, b) {
        (
            PowerSource::Ac {
                adapter_watts: left,
                charging: left_charging,
                ..
            },
            PowerSource::Ac {
                adapter_watts: right,
                charging: right_charging,
                ..
            },
        ) => left == right && left_charging == right_charging,
        (PowerSource::Battery { .. }, PowerSource::Battery { .. })
        | (PowerSource::Unknown, PowerSource::Unknown) => true,
        _ => false,
    }
}

/// One combined read: the `IOPSCopyPowerSourcesInfo` snapshot for the power
/// source and the battery, the sibling `IOPSCopyExternalPowerAdapterDetails`
/// call for the adapter's rating, and the `AppleSmartBattery` registry entry
/// for the measured draw.
fn read() -> Snapshot {
    let Some(info) = Info::read() else {
        return Snapshot {
            power_source: PowerSource::Unknown,
            watts: None,
            charge: None,
        };
    };
    let battery = info.internal_battery();
    let charging = battery.is_some_and(|b| b.charging);

    Snapshot {
        power_source: match info.providing() {
            Side::Ac => PowerSource::Ac {
                adapter_watts: adapter_watts(),
                charging,
                time_to_full_minutes: battery.and_then(|b| b.time_to_full_minutes),
            },
            Side::Battery => PowerSource::Battery {
                time_to_empty_minutes: battery.and_then(|b| b.time_to_empty_minutes),
            },
            Side::Unknown => PowerSource::Unknown,
        },
        watts: measured_watts(),
        charge: battery.map(|b| b.charge),
    }
}

/// What `IOKit` calls when the power situation might have changed.
///
/// Takes no argument worth reading: `report` always re-reads the whole
/// situation through [`read`], so the callback has nothing to do but say
/// "something happened" and return. The dedup against the last snapshot is
/// [`decide`]'s.
fn report(relay: &Relay<()>) {
    relay.post(());
}

skylight::trampoline!(REPORT = report(@ relay: &Relay<()>));

/// Reports a reading, if anything an item would draw actually moved since the
/// last one — `IOKit` fires for far more than an AC/battery flip: a battery
/// percent tick, an adapter settling on a reported wattage, anything in the
/// snapshot.
///
/// The loop ends on its own: dropping the [`Callback`] drops the last
/// [`Relay`], `next` answers `None`, and this returns.
async fn decide(mut ticks: skylight::callback::Events<()>, emit: crate::sources::Emitter) {
    let mut last: Option<Snapshot> = None;
    while ticks.next().await.is_some() {
        let snapshot = read();
        let worth_reporting = last.is_none_or(|prev| prev.differs_from(&snapshot));
        last = Some(snapshot);
        if worth_reporting {
            emit.send(Event::PowerSourceChanged(snapshot.into_event()));
        }
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

    fn run(
        &mut self,
        _wanted: &BTreeSet<Kind>,
        cx: Registering,
    ) -> Result<crate::pool::Task, StartError> {
        let emit = cx.emitter();
        let id = self.id();

        // `!Send` does not mean "put it on the main thread" -- it means "build
        // it where it will live". `IOPSNotificationCreateRunLoopSource` wants
        // no thread in particular, but the `CFRunLoopSource` it hands back has
        // to be scheduled on `pool::run_loop()` to ever fire, so this is built
        // there from the start rather than built here and moved -- `Callback`
        // cannot move anyway.
        //
        // The cost: a registration refusal can no longer come back through
        // this `run`'s `Result`, since building it happens after this
        // function has already returned `Ok`. Logged instead. The registry
        // still treats the source as running -- there is nothing to retry,
        // this failure mode (`IOPSNotificationCreateRunLoopSource` returning
        // null) is exceedingly rare, and a source with nothing to report is
        // the same as one from an item's point of view.
        Ok(crate::pool::sources().spawn(move |_here| async move {
            let (relay, ticks) = skylight::callback::relay::<()>();
            let run_loop = crate::pool::run_loop();

            // `IOKit` publishes no "stop notifying" call, and does not need
            // one: being *on* a run loop is what makes the source fire, so
            // taking it off is a real deregistration.
            let watch = match Callback::new(relay, |context| {
                // SAFETY: `context` is the weak reference the callback keeps
                // for as long as the registration lives.
                let source = unsafe { IOPSNotificationCreateRunLoopSource(REPORT, context) };
                // Null means `IOKit` took nothing, including the context.
                let source = NonNull::new(source).ok_or(Cause::IoKit)?;
                // SAFETY: the Create convention hands back a +1 reference.
                let source: CFRetained<CFRunLoopSource> = unsafe { CFRetained::from_raw(source) };
                let unschedule = crate::runloop::scheduled(run_loop, source);
                Ok(move || unschedule.now())
            }) {
                Ok(watch) => watch,
                Err(cause) => {
                    let err = StartError::new(id, cause);
                    tracing::error!(%err, "power source could not register on its own thread");
                    return;
                }
            };
            let _watch = watch;
            decide(ticks, emit).await;
        }))
    }
}

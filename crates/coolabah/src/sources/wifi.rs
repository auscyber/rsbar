//! `WiFi` network changes, from `SCDynamicStore`.
//!
//! `State:/Network/Interface/<bsd-name>/AirPort` is where the window server's
//! network agent publishes the joined network for every 802.11 interface. A
//! wildcard pattern watches all of them at once — this machine's `en0` today,
//! whatever a Mac with more than one radio calls its others — and the
//! notification callback is handed exactly the key that changed, so reading
//! back through the same key is what tells this which interface moved rather
//! than assuming one.
//!
//! Verified against the real interface: registering this exact sequence and
//! then power-cycling the `en0` radio (`networksetup -setairportpower en0
//! off`/`on`) fired the callback both times. The `SSID_STR` field itself came
//! back empty on this machine — `CoreWLAN` and `SCDynamicStore` alike redact
//! it for a process without Location authorization, which a background daemon
//! is not going to hold — but the *event* still fires and still names the
//! interface, which is what `wifi_changed` promises. A process with Location
//! access (Terminal, granted through System Settings) sees the real name
//! through this identical path.
//!
//! `SCDynamicStore` notifies on more than a join or leave — signal-quality and
//! other `AirPort` sub-keys live under the same wildcard — so this keeps the
//! last SSID seen per interface key and only emits when it actually moved.
//!
//! # The callback reads; the task decides
//!
//! The per-interface last-seen map lives in [`decide`]'s task, as an ordinary
//! local held across the `.await` — not behind the callback's context. A
//! `SCDynamicStore` callback cannot hold anything between calls and has
//! nowhere to `.await`, so putting the map there would mean a lock taken
//! blockingly from a C callback. The callback instead does only what is true
//! *inside* the call — the store handle and the `CFArray` of changed keys are
//! borrowed for its duration and dangling after it, so the SSID is read there
//! — and posts a plain [`Joined`] through a [`relay`](skylight::callback::relay).

use crate::protocol::event::WifiChange;
use crate::protocol::{Event, Kind};
use crate::sources::{Cause, Registering, Source, SourceId, StartError};
use objc2_core_foundation::{CFArray, CFDictionary, CFRetained, CFRunLoopSource, CFString, CFType};
use skylight::callback::{Callback, Relay};
use std::collections::{BTreeSet, HashMap};
use std::ffi::c_void;
use std::ptr::NonNull;

/// The pattern `SCDynamicStore` matches keys against: every `AirPort`
/// interface, on every session.
const AIRPORT_KEY_PATTERN: &str = "State:/Network/Interface/.*/AirPort";

/// The one field this cares about in the dictionary the key resolves to.
const SSID_KEY: &str = "SSID_STR";

/// `SCDynamicStoreContext`. Only `version` and `info` are used — the retain,
/// release and copy-description callbacks are for a context `SCDynamicStore`
/// itself owns, and this one is owned by the [`Callback`] instead, kept alive
/// inside the future [`Source::run`](crate::sources::Source::run) returns.
#[repr(C)]
struct StoreContext {
    version: isize,
    info: *mut c_void,
    retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<extern "C" fn(*const c_void)>,
    copy_description: Option<extern "C" fn(*const c_void) -> *mut CFString>,
}

type StoreCallback =
    extern "C-unwind" fn(store: *mut CFType, changed_keys: *mut CFArray, info: *mut c_void);

#[link(name = "SystemConfiguration", kind = "framework")]
unsafe extern "C" {
    fn SCDynamicStoreCreate(
        allocator: *const CFType,
        name: *const CFString,
        callout: StoreCallback,
        context: *mut StoreContext,
    ) -> *mut CFType;

    fn SCDynamicStoreCopyValue(store: *mut CFType, key: *const CFString) -> *mut CFType;

    fn SCDynamicStoreSetNotificationKeys(
        store: *mut CFType,
        keys: *const CFArray,
        patterns: *const CFArray,
    ) -> u8;

    fn SCDynamicStoreCreateRunLoopSource(
        allocator: *const CFType,
        store: *mut CFType,
        order: isize,
    ) -> *mut CFRunLoopSource;
}

/// Reads `SSID_STR` out of the dictionary `key` currently resolves to.
///
/// `None` covers both "the key no longer resolves" (the interface just went
/// down) and "no such field" — neither is a fault, just nothing to report.
fn read_ssid(store: *mut CFType, key: &CFString) -> Option<String> {
    // SAFETY: `store` is the live store the callback was handed; `key` is a
    // valid `CFString` for the duration of the call.
    let dict = unsafe { SCDynamicStoreCopyValue(store, std::ptr::from_ref(key)) };
    let dict = NonNull::new(dict)?;
    // SAFETY: `SCDynamicStoreCopyValue` follows the Copy convention: a +1
    // reference this now owns.
    let dict = unsafe { CFRetained::<CFType>::from_raw(dict) };
    // SAFETY: the key resolves to a dictionary, per `SCDynamicStore`'s
    // documented shape for an `AirPort` interface entry.
    let dict: &CFDictionary =
        unsafe { &*CFRetained::as_ptr(&dict).as_ptr().cast::<CFDictionary>() };

    let ssid_key = CFString::from_str(SSID_KEY);
    // SAFETY: `dict` and `ssid_key` are both live for the call, which
    // borrows rather than taking ownership of what it returns. The pointer
    // must be to the `CFString` itself, not to the `CFRetained` wrapper — the
    // wrapper's own address is just a Rust stack slot holding that pointer.
    let value = unsafe { dict.value(CFRetained::as_ptr(&ssid_key).as_ptr().cast()) };
    let value = NonNull::new(value.cast_mut())?.cast::<CFString>();
    // SAFETY: the dictionary's `SSID_STR` entry, when present, is a CFString.
    let ssid = unsafe { value.as_ref() };
    Some(ssid.to_string())
}

/// One interface's `AirPort` key, and what it resolved to when it changed.
///
/// Owned, because everything the callback was handed to produce it is gone by
/// the time the task reads it.
struct Joined {
    key: String,
    ssid: Option<String>,
}

/// What `SCDynamicStore` calls when a watched key changes.
///
/// Reads, posts, returns. The dedup is [`decide`]'s.
fn changed(store: *mut CFType, changed_keys: *mut CFArray, relay: &Relay<Joined>) {
    let Some(keys) = NonNull::new(changed_keys) else {
        return;
    };
    // SAFETY: a live `CFArray` for the duration of this call.
    let keys = unsafe { keys.cast::<CFArray>().as_ref() };

    for index in 0..keys.count() {
        // SAFETY: `index` is within `[0, keys.count())`, and every element of
        // this array is a `CFString` key name.
        let key = unsafe { keys.value_at_index(index) };
        let Some(key) = NonNull::new(key.cast_mut()) else {
            continue;
        };
        // SAFETY: as above.
        let key = unsafe { key.cast::<CFString>().as_ref() };
        relay.post(Joined {
            ssid: read_ssid(store, key),
            key: key.to_string(),
        });
    }
}

skylight::trampoline!(CHANGED = changed(
    store: *mut CFType,
    changed_keys: *mut CFArray,
    @ relay: &Relay<Joined>,
));

/// Reports the joins that actually moved, until the registration goes away.
///
/// The loop ends on its own: dropping the [`Callback`] drops the last
/// [`Relay`], `next` answers `None`, and this returns. There is no separate
/// stop signal, and nothing to remember to send one through.
async fn decide(mut joins: skylight::callback::Events<Joined>, emit: crate::sources::Emitter) {
    let mut last: HashMap<String, Option<String>> = HashMap::new();
    while let Some(Joined { key, ssid }) = joins.next().await {
        if last.get(&key) == Some(&ssid) {
            continue;
        }
        last.insert(key, ssid.clone());
        emit.send(Event::WifiChanged(WifiChange {
            ssid: ssid.unwrap_or_default(),
        }));
    }
}

pub struct Wifi;

impl Source for Wifi {
    fn id(&self) -> SourceId {
        SourceId("wifi")
    }

    fn provides(&self) -> Vec<Kind> {
        vec![Kind::WifiChanged]
    }

    fn run(
        &mut self,
        _wanted: &BTreeSet<Kind>,
        cx: Registering,
    ) -> Result<crate::pool::Task, StartError> {
        let emit = cx.emitter();
        let id = self.id();

        // `!Send` means "build it where it will live", not "put it on the
        // main thread": `wifi` needs nothing from AppKit or the window
        // server, only `pool::run_loop()`'s thread, which is where the
        // `CFRunLoopSource` below is scheduled -- so the whole registration
        // is built there from the start.
        //
        // The cost: a refusal can no longer come back through this `run`'s
        // `Result`, since this closure runs after `run` has already returned
        // `Ok`. Logged instead -- `SCDynamicStore` refusing is exceedingly
        // rare, and the registry has nothing sane to retry.
        Ok(crate::pool::sources().spawn(move |_here| async move {
            let (relay, joins) = skylight::callback::relay::<Joined>();
            let run_loop = crate::pool::run_loop();

            // `SCDynamicStore` has no unregister call because the store *is*
            // the registration: taking the source off the loop and releasing
            // the store is the whole teardown.
            let watch = match Callback::new(relay, |context| {
                let name = CFString::from_str("coolabah-wifi");
                let mut store_context = StoreContext {
                    version: 0,
                    info: context,
                    retain: None,
                    release: None,
                    copy_description: None,
                };

                // SAFETY: `name` and `store_context` outlive the call, and
                // `context` stays valid to reconstruct for as long as the
                // registration lives.
                let store = unsafe {
                    SCDynamicStoreCreate(
                        std::ptr::null(),
                        CFRetained::as_ptr(&name).as_ptr(),
                        CHANGED,
                        &raw mut store_context,
                    )
                };
                let store = NonNull::new(store).ok_or(Cause::DynamicStore)?;
                // SAFETY: the Create convention hands back a +1 reference.
                let store = unsafe { CFRetained::<CFType>::from_raw(store) };

                let pattern = CFString::from_str(AIRPORT_KEY_PATTERN);
                let patterns = CFArray::from_objects(&[&*pattern]);
                // SAFETY: `store` and `patterns` are both live for the call.
                let ok = unsafe {
                    SCDynamicStoreSetNotificationKeys(
                        CFRetained::as_ptr(&store).as_ptr(),
                        std::ptr::null(),
                        CFRetained::as_ptr(&patterns).as_ptr().cast(),
                    )
                };
                if ok == 0 {
                    return Err(Cause::DynamicStore);
                }

                // SAFETY: `store` is live; the call hands back a +1 run loop
                // source.
                let source = unsafe {
                    SCDynamicStoreCreateRunLoopSource(
                        std::ptr::null(),
                        CFRetained::as_ptr(&store).as_ptr(),
                        0,
                    )
                };
                let source = NonNull::new(source).ok_or(Cause::DynamicStore)?;
                // SAFETY: as above.
                let source = unsafe { CFRetained::from_raw(source) };
                let unschedule = crate::runloop::scheduled(run_loop, source);

                Ok(move || {
                    unschedule.now();
                    drop(store);
                })
            }) {
                Ok(watch) => watch,
                Err(cause) => {
                    let err = StartError::new(id, cause);
                    tracing::error!(%err, "wifi source could not register on its own thread");
                    return;
                }
            };
            let _watch = watch;
            decide(joins, emit).await;
        }))
    }
}

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

use crate::sources::{
    CallbackState, Cause, Emitter, Registering, Registration, Source, SourceId, StartError,
};
use objc2_core_foundation::{
    CFArray, CFDictionary, CFRetained, CFRunLoop, CFRunLoopSource, CFString, CFType,
};
use rsbar_protocol::event::WifiChange;
use rsbar_protocol::{Event, Kind};
use std::collections::BTreeSet;
use std::ffi::c_void;
use std::ptr::NonNull;

/// The pattern `SCDynamicStore` matches keys against: every `AirPort`
/// interface, on every session.
const AIRPORT_KEY_PATTERN: &str = "State:/Network/Interface/.*/AirPort";

/// The one field this cares about in the dictionary the key resolves to.
const SSID_KEY: &str = "SSID_STR";

/// `SCDynamicStoreContext`. Only `version` and `info` are used — the retain,
/// release and copy-description callbacks are for a context `SCDynamicStore`
/// itself owns, and this one is owned by the [`CallbackState`] instead, kept
/// alive by [`Watch`].
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

extern "C-unwind" fn changed(store: *mut CFType, changed_keys: *mut CFArray, context: *mut c_void) {
    // SAFETY: the registration passed a `CallbackState<Emitter>` pointer.
    let Some(emit) = (unsafe { CallbackState::<Emitter>::recover(context) }) else {
        return;
    };
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
        if let Some(ssid) = read_ssid(store, key) {
            emit.send(Event::WifiChanged(WifiChange { ssid }));
        }
    }
}

/// Deregisters on drop: the run loop source is removed, and the store —
/// released when `store` drops — takes the notification registration with it.
struct Watch {
    /// Never read: held only so the store outlives the registration and its
    /// release deregisters. See the type-level doc comment.
    _store: CFRetained<CFType>,
    source: CFRetained<CFRunLoopSource>,
    run_loop: CFRetained<CFRunLoop>,
    /// Outlives the store: `SCDynamicStore` may still be draining a callback
    /// when this drops, and the state must still be there to recover.
    _state: CallbackState<Emitter>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.run_loop.remove_source(Some(&self.source), unsafe {
            objc2_core_foundation::kCFRunLoopCommonModes
        });
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

    fn register(
        &mut self,
        _wanted: &BTreeSet<Kind>,
        cx: &mut Registering<'_>,
    ) -> Result<Registration, StartError> {
        let emit = cx.emitter();
        let state = CallbackState::new(emit);

        let name = CFString::from_str("rsbar-wifi");
        let mut context = StoreContext {
            version: 0,
            info: state.with_ptr(|ptr| ptr),
            retain: None,
            release: None,
            copy_description: None,
        };

        // SAFETY: `name` outlives the call; `context` outlives the store
        // (dropped only when `Watch` is, which deregisters first).
        let store = unsafe {
            SCDynamicStoreCreate(
                std::ptr::null(),
                CFRetained::as_ptr(&name).as_ptr(),
                changed,
                &raw mut context,
            )
        };
        let store =
            NonNull::new(store).ok_or_else(|| StartError::new(self.id(), Cause::DynamicStore))?;
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
            return Err(StartError::new(self.id(), Cause::DynamicStore));
        }

        // SAFETY: `store` is live; the call hands back a +1 run loop source.
        let source = unsafe {
            SCDynamicStoreCreateRunLoopSource(
                std::ptr::null(),
                CFRetained::as_ptr(&store).as_ptr(),
                0,
            )
        };
        let source =
            NonNull::new(source).ok_or_else(|| StartError::new(self.id(), Cause::DynamicStore))?;
        // SAFETY: as above.
        let source = unsafe { CFRetained::from_raw(source) };

        let run_loop = CFRunLoop::current().expect("no run loop on this thread");
        run_loop.add_source(Some(&source), unsafe {
            objc2_core_foundation::kCFRunLoopCommonModes
        });

        Ok(Box::new(Watch {
            _store: store,
            source,
            run_loop,
            _state: state,
        }))
    }
}

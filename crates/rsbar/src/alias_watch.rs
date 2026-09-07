//! Turning an Accessibility notification on an aliased item's owning
//! application into "re-capture this one now".
//!
//! One [`AXObserver`] per owning pid, registered on that application's
//! top-level element (`AXUIElement::new_application`) rather than on the
//! menu bar item itself — `AXMenuBarItem` rejects every notification
//! outright with `kAXErrorNotificationUnsupported`, confirmed live by
//! `examples/ax_observer_probe.rs`. Registering on the app element instead
//! is accepted, and it does fire: the same probe, watched across a live
//! minute rollover, caught the Control Centre clock's own change —
//! `AXTitleChanged`/`AXValueChanged`, `owner_pid` matching Control Centre's,
//! `AXDescription` reading `"Clock"`.
//!
//! What makes an app-wide registration usable rather than a firehose is
//! three checks on every notification, all required before anything is
//! marked dirty:
//! - the notified element's own pid (`AXUIElementGetPid`) must equal the pid
//!   this observer was created for. The same probe run also received
//!   `AXValueChanged` from a wholly unrelated pid — another app's
//!   still-mounted popover text — despite the observer being registered for
//!   Control Centre, so this is not a redundant check;
//! - the notified element's `AXRole` must be `AXButton` or `AXMenuBarItem` —
//!   the two roles measured live for a real item, an `AXExtrasMenuBar`
//!   child itself (`AXMenuBarItem`, both for Control Centre's own hosted
//!   modules and for a genuinely separate process's own status item, e.g.
//!   Fantastical's) or the interior element a Control Centre-hosted item's
//!   own value change actually lands on (`AXButton`). This is what clears
//!   out noise that shares the *same* pid as a watched item — a slider or
//!   label inside Control Centre's own still-mounted popovers, say — which
//!   the pid check above cannot see, since it is honestly from the right
//!   process. A first cut at this checked for `AXButton` alone; that turned
//!   out to also describe the enumeration bug below, so it moved here
//!   instead of into [`super::ax::enumerate_extras_menu_items`], where
//!   requiring it wrongly dropped every real, separate-process item this
//!   machine has (`OneDrive`'s, Spotlight's, Fantastical's own status items —
//!   all `AXMenuBarItem`, not `AXButton`, at that level of the tree);
//! - the notified element's `AXDescription`, falling back to `AXTitle`, must
//!   equal the watched alias's name, exactly.
//!
//! A miss just leaves the item to the poll — no worse off than having no
//! observer at all — so both checks err toward discarding rather than
//! guessing. Marking an entity dirty only ever makes *that entity's own*
//! [`Alias::capture`](super::Alias::capture) run sooner; it never hands one
//! item's notification to another item's repaint, so a wrong or ambiguous
//! match costs an extra harmless capture, never a wrong picture.
//!
//! One consequence worth stating plainly: an alias whose name carries the
//! `(n)` duplicate suffix from [`disambiguate_duplicates`
//! ](super::disambiguate_duplicates) can never match here, because the real
//! `AXDescription` never carries that suffix either. Those items — and any
//! whose window title simply is not what the owning app calls the element
//! internally — fall back to the poll exactly as if this module did not
//! exist.

use bevy_ecs::entity::Entity;
use objc2_application_services::{AXError, AXObserver, AXUIElement};
use objc2_core_foundation::{CFRetained, CFRunLoop, CFString, CFType, kCFRunLoopCommonModes};
use std::collections::{HashMap, HashSet, hash_map::Entry};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

/// What this module asks an owning application to report. Both fired for the
/// Control Centre clock's minute rollover in `examples/ax_observer_probe.rs`,
/// on an element whose identifying text lives in `AXDescription` and whose
/// live value is in `AXValue`; `AXTitleChanged` also covers items that
/// identify themselves through `AXTitle` instead.
const NOTIFICATIONS: &[&str] = &["AXValueChanged", "AXTitleChanged"];

fn attribute_string(element: &AXUIElement, name: &str) -> Option<String> {
    let attr = CFString::from_str(name);
    let mut value: *const CFType = std::ptr::null();
    // SAFETY: `attr` is live for the duration of the call, and `value` is a
    // valid out-pointer.
    let err = unsafe { element.copy_attribute_value(&attr, NonNull::from(&mut value)) };
    if err != AXError::Success {
        return None;
    }
    let ptr = NonNull::new(value.cast_mut())?;
    // SAFETY: a non-null result from `AXUIElementCopyAttributeValue` carries
    // a +1 reference, which transfers to `CFRetained`.
    let value = unsafe { CFRetained::<CFType>::from_raw(ptr) };
    value.downcast::<CFString>().ok().map(|s| s.to_string())
}

/// What one owning application's observer needs at hand when a notification
/// arrives: who it was created for, and which entities under it are watching
/// for which name.
struct Context {
    pid: i32,
    watched: Mutex<HashMap<Entity, String>>,
    dirty: Mutex<HashSet<Entity>>,
}

/// One pid's live registration.
///
/// Dropping this drops the [`AXObserver`], which — per its own documented
/// contract — removes its run loop source automatically; nothing else here
/// needs to.
struct PidObserver {
    // Never read again after creation. Kept alive only because dropping it
    // is what tears the registration down.
    _observer: CFRetained<AXObserver>,
    context: Arc<Context>,
}

impl PidObserver {
    /// Registers an observer for `pid` on the current thread's run loop,
    /// which must be the daemon's main one — the same thread that will later
    /// drop this value, since the callback below borrows `context` rather
    /// than taking a reference count, on the assumption that a notification
    /// can only ever arrive synchronously on the run loop its source was
    /// added to.
    ///
    /// `None` means the framework refused every notification for this pid,
    /// which happens without Accessibility permission — same failure shape
    /// as the rest of this module's Accessibility use, silent rather than an
    /// error a caller can act on.
    fn create(pid: i32) -> Option<Self> {
        let mut observer_ptr: *mut AXObserver = std::ptr::null_mut();
        // SAFETY: `pid` is a live process id, `observer_ptr` is a valid
        // out-pointer, and `on_notification` matches `AXObserverCallback`'s
        // signature (a safe `extern "C-unwind" fn` coerces to the `unsafe`
        // function-pointer type the callback slot expects).
        let status = unsafe {
            AXObserver::create(pid, Some(on_notification), NonNull::from(&mut observer_ptr))
        };
        if status != AXError::Success {
            tracing::debug!(
                pid,
                ?status,
                "AXObserverCreate refused; falling back to the poll"
            );
            return None;
        }
        let observer_ptr = NonNull::new(observer_ptr)?;
        // SAFETY: a non-null result from `AXObserverCreate` carries a +1
        // reference.
        let observer = unsafe { CFRetained::from_raw(observer_ptr) };

        let context = Arc::new(Context {
            pid,
            watched: Mutex::new(HashMap::new()),
            dirty: Mutex::new(HashSet::new()),
        });
        // SAFETY: `pid` is a live process id.
        let app = unsafe { AXUIElement::new_application(pid) };
        let refcon = Arc::as_ptr(&context).cast_mut().cast::<c_void>();

        let mut any_ok = false;
        for name in NOTIFICATIONS {
            let notif = CFString::from_str(name);
            // SAFETY: `app` and `notif` are both live for the call, and
            // `refcon` stays valid for as long as `context` does — which is
            // as long as this `PidObserver` is, since it owns the `Arc`.
            let status = unsafe { observer.add_notification(&app, &notif, refcon) };
            any_ok |= status == AXError::Success;
        }
        if !any_ok {
            tracing::debug!(
                pid,
                "AXObserverAddNotification refused every notification; falling back to the poll"
            );
            return None;
        }

        // SAFETY: `observer` outlives the source it hands back; adding it to
        // the current thread's run loop is exactly what
        // `AXObserverGetRunLoopSource`'s own documentation prescribes.
        let source = unsafe { observer.run_loop_source() };
        if let Some(run_loop) = CFRunLoop::current() {
            // SAFETY: reads a `'static` extern constant.
            run_loop.add_source(Some(source.as_ref()), unsafe { kCFRunLoopCommonModes });
        }

        tracing::debug!(
            pid,
            "registered an AX observer for aliased items owned by this pid"
        );
        Some(Self {
            _observer: observer,
            context,
        })
    }
}

extern "C-unwind" fn on_notification(
    _observer: NonNull<AXObserver>,
    element: NonNull<AXUIElement>,
    notification: NonNull<CFString>,
    refcon: *mut c_void,
) {
    if refcon.is_null() {
        return;
    }
    // SAFETY: `refcon` was handed out by `PidObserver::create` as a pointer
    // into a `Context` that the same `PidObserver` keeps alive for exactly
    // as long as this observer stays registered, and an `AXObserver`
    // callback only ever runs synchronously on the run loop its source was
    // added to — the same thread that would drop the `PidObserver` — so this
    // borrow cannot outlive it.
    let context = unsafe { &*refcon.cast::<Context>() };
    // SAFETY: the callback contract guarantees a live `CFString` for the
    // duration of this call.
    let notification = unsafe { notification.as_ref() }.to_string();

    // SAFETY: the callback contract guarantees a live `AXUIElement` for the
    // duration of this call.
    let element = unsafe { element.as_ref() };
    let mut owner_pid: i32 = 0;
    // SAFETY: `owner_pid` is a valid out-pointer.
    if unsafe { element.pid(NonNull::from(&mut owner_pid)) } != AXError::Success {
        return;
    }
    // `ax_observer_probe` caught this same kind of observer receiving
    // notifications sourced from a wholly unrelated pid. Discard anything
    // that is not actually from the application this observer watches.
    if owner_pid != context.pid {
        tracing::trace!(
            expected_pid = context.pid,
            owner_pid,
            notification,
            "discarding an AX notification from an unrelated pid"
        );
        return;
    }

    // A real menu bar item's role is one of two things, measured live on
    // this machine: every item enumerated straight off an app's own
    // `AXExtrasMenuBar` — Control Centre's own hosted modules *and* a
    // genuinely separate process's own status item (Fantastical's, e.g.) —
    // is `AXMenuBarItem`; the specific descendant that actually reports a
    // value change for a Control Centre-hosted item is `AXButton` instead
    // (that is the role on the element the clock's own `AXValueChanged`
    // arrived on). The probe's firehose of unrelated traffic — another app's
    // still-mounted popover text, or a slider or label inside Control
    // Centre's own popovers, either of which shares a watched pid and so
    // survives the check above — is neither, which is what actually clears
    // that noise rather than the pid check alone.
    let role = attribute_string(element, "AXRole");
    if !matches!(role.as_deref(), Some("AXButton" | "AXMenuBarItem")) {
        tracing::trace!(
            pid = context.pid,
            notification,
            ?role,
            "discarding an AX notification from a non-item role"
        );
        return;
    }

    let description = attribute_string(element, "AXDescription");
    let title = attribute_string(element, "AXTitle");
    let identity = [description.as_deref(), title.as_deref()];

    let watched = context.watched.lock().unwrap();
    let matched: Vec<Entity> = watched
        .iter()
        .filter(|(_, name)| identity.contains(&Some(name.as_str())))
        .map(|(&entity, _)| entity)
        .collect();
    drop(watched);
    if matched.is_empty() {
        tracing::trace!(
            pid = context.pid,
            notification,
            ?description,
            ?title,
            "an AX notification matched none of this pid's watched aliases"
        );
        return;
    }
    tracing::info!(
        pid = context.pid,
        notification,
        ?description,
        ?title,
        matched = matched.len(),
        "AX notification matched an aliased item; marking it dirty"
    );
    context.dirty.lock().unwrap().extend(matched);
}

/// Every aliased entity's live AX registration, by the pid it currently
/// believes owns it.
#[derive(Default)]
pub struct AxWatch {
    by_pid: HashMap<i32, PidObserver>,
    entity_pid: HashMap<Entity, i32>,
    /// Pids that already refused a registration, so a config with an item
    /// this module cannot observe does not retry `AXObserverCreate` on every
    /// poll tick.
    failed: HashSet<i32>,
}

impl AxWatch {
    /// Keeps one entity's registration current: which pid it should be
    /// watched under, and what name identifies it there.
    ///
    /// `pid` is `None` until the alias has resolved a window at least once —
    /// there is nothing to observe before that, so this is a no-op.
    pub fn sync_one(&mut self, entity: Entity, name: &str, pid: Option<i32>) {
        let previous = self.entity_pid.get(&entity).copied();
        if previous != pid {
            if let Some(old) = previous {
                self.detach(entity, old);
            }
            if let Some(pid) = pid {
                self.entity_pid.insert(entity, pid);
            } else {
                self.entity_pid.remove(&entity);
            }
        }

        let Some(pid) = pid else { return };
        if self.failed.contains(&pid) {
            return;
        }
        if let Entry::Vacant(slot) = self.by_pid.entry(pid) {
            if let Some(observer) = PidObserver::create(pid) {
                slot.insert(observer);
            } else {
                self.failed.insert(pid);
                self.entity_pid.remove(&entity);
                return;
            }
        }
        if let Some(observer) = self.by_pid.get(&pid) {
            observer
                .context
                .watched
                .lock()
                .unwrap()
                .insert(entity, name.to_owned());
        }
    }

    fn detach(&mut self, entity: Entity, pid: i32) {
        let Some(observer) = self.by_pid.get(&pid) else {
            return;
        };
        observer.context.watched.lock().unwrap().remove(&entity);
        observer.context.dirty.lock().unwrap().remove(&entity);
        if observer.context.watched.lock().unwrap().is_empty() {
            self.by_pid.remove(&pid);
        }
    }

    /// Forgets everything about one entity, tearing down its owning
    /// observer too if nothing else still watches it.
    pub fn forget(&mut self, entity: Entity) {
        if let Some(pid) = self.entity_pid.remove(&entity) {
            self.detach(entity, pid);
        }
    }

    /// Whether a notification matched this entity since the last check.
    /// One-shot: asking clears it, the same shape as an edge rather than a
    /// level.
    pub fn take_dirty(&mut self, entity: Entity) -> bool {
        let Some(&pid) = self.entity_pid.get(&entity) else {
            return false;
        };
        let Some(observer) = self.by_pid.get(&pid) else {
            return false;
        };
        observer.context.dirty.lock().unwrap().remove(&entity)
    }
}

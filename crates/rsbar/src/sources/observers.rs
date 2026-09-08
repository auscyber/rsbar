//! Notification observers, and taking them down again.
//!
//! Split out because more than one *centre* is observed — `NSWorkspace`'s and
//! the distributed one — and the registration is one call per notification
//! with a token to keep. [`super::notifications`] is the only source that
//! holds these, one per centre it has had a reason to open.

use crate::sources::Emitter;
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSNotificationName};
use rsbar_protocol::{Event, Kind};
use std::collections::HashMap;

/// Builds the event a notification means, reading whatever it carries.
pub type ToEvent = fn(&NSNotification) -> Event;

/// The same, for an observer that needs more than the notification itself.
///
/// A bridged custom event has to know which name it was *declared* under, and
/// a distributed notification never says — the poster does not know rsbar
/// exists. That is a capture, so it cannot be a plain function pointer.
pub type ToEventWith = Box<dyn Fn(&NSNotification) -> Event>;

/// What taking one observer back down needs: the centre it was registered
/// with, and the token that centre handed back.
///
/// The centre travels with the token rather than being reached for at
/// teardown, because removing an observer is only meaningful against the
/// centre it came from — and a `Retained` of one is a retain, not a copy of
/// anything.
pub struct Observation {
    center: Retained<NSNotificationCenter>,
    token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

impl Drop for Observation {
    fn drop(&mut self) {
        // SAFETY: the token came from this centre and is still live — it has
        // been held here, untouched, since `observe` put it there.
        unsafe { self.center.removeObserver(self.token.as_ref()) };
    }
}

/// Keeps observers registered, one per event, keyed by [`Kind`] so a source can
/// add or drop the observer for one event without disturbing the others — what
/// a subscription changing under a running source needs, since re-registering
/// the lot would deregister observers that were already right.
///
/// Dropping an entry — or dropping the whole map — is what deregisters, on the
/// thread that registered it, so there is no removal call written by hand
/// below.
pub struct Observers {
    center: Retained<NSNotificationCenter>,
    tokens: HashMap<Kind, Observation>,
}

impl Observers {
    #[must_use]
    pub fn new(center: Retained<NSNotificationCenter>) -> Self {
        Self {
            center,
            tokens: HashMap::new(),
        }
    }

    /// Whether the event already has an observer.
    #[must_use]
    pub fn has(&self, kind: &Kind) -> bool {
        self.tokens.contains_key(kind)
    }

    /// Whether this is holding nothing — which is what tells the source that
    /// owns it that the centre behind it can be let go.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Deregisters the observer for one event, if there is one.
    ///
    /// Taking it out of the map is the whole of it: the guard that comes out
    /// deregisters as it goes.
    pub fn forget(&mut self, kind: &Kind) {
        drop(self.tokens.remove(kind));
    }

    /// Drops the observers for everything outside `wanted`.
    pub fn retain(&mut self, wanted: &std::collections::BTreeSet<Kind>) {
        let extra: Vec<Kind> = self
            .tokens
            .keys()
            .filter(|kind| !wanted.contains(kind))
            .cloned()
            .collect();
        for kind in extra {
            self.forget(&kind);
        }
    }

    /// Registers one notification as the event `to_event` builds from it.
    ///
    /// Replaces any observer already held for `kind`, so calling twice does not
    /// double up.
    pub fn observe(
        &mut self,
        kind: Kind,
        name: &NSNotificationName,
        emit: &Emitter,
        to_event: ToEvent,
    ) {
        self.observe_with(kind, name, emit, Box::new(to_event));
    }

    /// The same, for an observer whose event needs something the notification
    /// does not carry.
    pub fn observe_with(
        &mut self,
        kind: Kind,
        name: &NSNotificationName,
        emit: &Emitter,
        to_event: ToEventWith,
    ) {
        self.forget(&kind);
        let emit = emit.clone();
        let block = RcBlock::new(move |note: std::ptr::NonNull<NSNotification>| {
            // SAFETY: the notification is live for the duration of the call.
            let event = to_event(unsafe { note.as_ref() });
            // A full channel means the daemon is not keeping up. Dropping the
            // event beats blocking a system notification callback.
            emit.send(event);
        });

        // SAFETY: `name` and `block` are live for the call, and the centre
        // retains the block for as long as the observer it hands back lives.
        let token = unsafe {
            self.center
                .addObserverForName_object_queue_usingBlock(Some(name), None, None, &block)
        };
        self.tokens.insert(
            kind,
            Observation {
                center: self.center.clone(),
                token,
            },
        );
    }
}

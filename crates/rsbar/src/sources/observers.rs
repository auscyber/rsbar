//! Notification observers, and taking them down again.
//!
//! Shared because more than one source registers for `NSNotification`s, and the
//! registration is one call per notification with a token to keep.

use crate::sources::Emitter;
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSNotificationName};
use rsbar_protocol::{Event, Kind};
use std::collections::HashMap;

/// Builds the event a notification means, reading whatever it carries.
pub type ToEvent = fn(&NSNotification) -> Event;

/// Keeps observers registered, one per event. Dropping it deregisters them all,
/// on the thread that registered them.
///
/// Keyed by [`Kind`] so a source can add or drop the observer for one event
/// without disturbing the others — what a subscription changing under a running
/// source needs, since re-registering the lot would deregister observers that
/// were already right.
pub struct Observers {
    center: Retained<NSNotificationCenter>,
    tokens: HashMap<Kind, Retained<ProtocolObject<dyn NSObjectProtocol>>>,
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

    /// Deregisters the observer for one event, if there is one.
    pub fn forget(&mut self, kind: &Kind) {
        if let Some(token) = self.tokens.remove(kind) {
            // SAFETY: the token came from this centre and is still live.
            unsafe { self.center.removeObserver(token.as_ref()) };
        }
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
        self.forget(&kind);
        let emit = emit.clone();
        let block = RcBlock::new(move |note: std::ptr::NonNull<NSNotification>| {
            // SAFETY: the notification is live for the duration of the call.
            let event = to_event(unsafe { note.as_ref() });
            // A full channel means the daemon is not keeping up. Dropping the
            // event beats blocking a system notification callback.
            emit.send(event);
        });

        let token = unsafe {
            self.center
                .addObserverForName_object_queue_usingBlock(Some(name), None, None, &block)
        };
        self.tokens.insert(kind, token);
    }
}

impl Drop for Observers {
    fn drop(&mut self) {
        for (_, token) in self.tokens.drain() {
            // SAFETY: the token came from this centre and is still live.
            unsafe { self.center.removeObserver(token.as_ref()) };
        }
    }
}

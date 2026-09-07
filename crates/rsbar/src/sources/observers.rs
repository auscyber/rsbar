//! Notification observers, and taking them down again.
//!
//! Shared because more than one source registers for `NSNotification`s, and the
//! registration is one call per notification with a token to keep.

use crate::sources::Emitter;
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSNotificationName};
use rsbar_protocol::Event;

/// Builds the event a notification means, reading whatever it carries.
pub type ToEvent = fn(&NSNotification) -> Event;

/// Keeps observers registered. Dropping it deregisters them, on the thread that
/// registered them.
pub struct Observers {
    center: Retained<NSNotificationCenter>,
    tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl Observers {
    #[must_use]
    pub fn new(center: Retained<NSNotificationCenter>) -> Self {
        Self {
            center,
            tokens: Vec::new(),
        }
    }

    /// Registers one notification as the event `to_event` builds from it.
    pub fn observe(&mut self, name: &NSNotificationName, emit: &Emitter, to_event: ToEvent) {
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
        self.tokens.push(token);
    }
}

impl Drop for Observers {
    fn drop(&mut self) {
        for token in self.tokens.drain(..) {
            // SAFETY: the token came from this centre and is still live.
            unsafe { self.center.removeObserver(token.as_ref()) };
        }
    }
}

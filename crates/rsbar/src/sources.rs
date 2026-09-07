//! Where events come from.
//!
//! `NSWorkspace` posts its notifications on the main thread, which is also the
//! thread that draws — so a handler can act on the bar directly instead of
//! going back through the run loop source the IPC thread needs.

use block2::RcBlock;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSString};
use rsbar_protocol::Event;
use std::rc::Rc;

/// Keeps the observers alive. Dropping it deregisters them.
pub struct Sources {
    center: objc2::rc::Retained<NSNotificationCenter>,
    tokens: Vec<objc2::rc::Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl Sources {
    /// Subscribes to the workspace notifications that map onto built-in
    /// events, calling `handler` with the event and whatever context it
    /// carries.
    ///
    /// These five share one notification centre and cost almost nothing, so
    /// they are registered eagerly. The expensive sources — audio, wifi, media
    /// — are worth deferring until an item actually subscribes.
    pub fn install<F>(handler: F) -> Self
    where
        F: Fn(Event, Option<String>) + 'static,
    {
        let workspace = NSWorkspace::sharedWorkspace();
        let center = workspace.notificationCenter();
        let handler = Rc::new(handler);

        // `NSWorkspaceActiveDisplayDidChangeNotification` is undocumented and
        // absent from the generated bindings, so it is named by string — the
        // same way SketchyBar reaches it.
        let display_changed = NSString::from_str("NSWorkspaceActiveDisplayDidChangeNotification");

        let mut tokens = Vec::new();
        for (name, event, carries_app) in [
            (
                unsafe { objc2_app_kit::NSWorkspaceDidActivateApplicationNotification },
                Event::FrontAppSwitched,
                true,
            ),
            (
                unsafe { objc2_app_kit::NSWorkspaceActiveSpaceDidChangeNotification },
                Event::SpaceChanged,
                false,
            ),
            (&display_changed, Event::DisplayChanged, false),
            (
                unsafe { objc2_app_kit::NSWorkspaceWillSleepNotification },
                Event::SystemWillSleep,
                false,
            ),
            (
                unsafe { objc2_app_kit::NSWorkspaceDidWakeNotification },
                Event::SystemWoke,
                false,
            ),
        ] {
            let handler = Rc::clone(&handler);
            let block = RcBlock::new(move |note: std::ptr::NonNull<NSNotification>| {
                // SAFETY: the notification is live for the duration of the call.
                let info = carries_app
                    .then(|| app_name(unsafe { note.as_ref() }))
                    .flatten();
                handler(event.clone(), info);
            });

            let token = unsafe {
                center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &block)
            };
            tokens.push(token);
        }

        Self { center, tokens }
    }
}

/// The localized name of the application a workspace notification is about.
fn app_name(note: &NSNotification) -> Option<String> {
    let info = note.userInfo()?;
    let key = NSString::from_str("NSWorkspaceApplicationKey");
    // A checked downcast rather than a transmute: the key is documented to
    // hold an NSRunningApplication, but this is `userInfo` from another
    // process's notification, so it is worth actually verifying.
    let app = info
        .objectForKey(&key)?
        .downcast::<NSRunningApplication>()
        .ok()?;
    app.localizedName().map(|name| name.to_string())
}

impl Drop for Sources {
    fn drop(&mut self) {
        for token in self.tokens.drain(..) {
            // SAFETY: the token came from this centre and is still live.
            unsafe { self.center.removeObserver(token.as_ref()) };
        }
    }
}

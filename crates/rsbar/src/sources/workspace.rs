//! Workspace events: the front application, spaces, displays, sleep and wake.

use crate::sources::{Emission, Emitter, Source, StartError};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSString};
use rsbar_protocol::Event;

/// Keeps the observers registered. Dropping it deregisters them, on the thread
/// that registered them.
struct Observers {
    center: Retained<NSNotificationCenter>,
    tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl Drop for Observers {
    fn drop(&mut self) {
        for token in self.tokens.drain(..) {
            // SAFETY: the token came from this centre and is still live.
            unsafe { self.center.removeObserver(token.as_ref()) };
        }
    }
}

pub struct Workspace;

impl Source for Workspace {
    fn name(&self) -> &'static str {
        "workspace"
    }

    fn provides(&self) -> Vec<Event> {
        vec![
            Event::FrontAppSwitched,
            Event::SpaceChanged,
            Event::DisplayChanged,
            Event::SystemWoke,
            Event::SystemWillSleep,
        ]
    }

    fn install(&mut self, emit: Emitter) -> Result<Box<dyn std::any::Any>, StartError> {
        let workspace = NSWorkspace::sharedWorkspace();
        let center = workspace.notificationCenter();

        // `NSWorkspaceActiveDisplayDidChangeNotification` is undocumented and
        // absent from the generated bindings, so it is named by string — the
        // same way `SketchyBar` reaches it.
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
            let emit = emit.clone();
            let block = RcBlock::new(move |note: std::ptr::NonNull<NSNotification>| {
                // SAFETY: the notification is live for the duration of the call.
                let info = carries_app
                    .then(|| app_name(unsafe { note.as_ref() }))
                    .flatten();
                // A full channel means the daemon is not keeping up; dropping
                // the event is better than blocking a system notification.
                let _ = emit.try_send(Emission::new(event.clone(), info));
            });

            let token = unsafe {
                center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &block)
            };
            tokens.push(token);
        }

        Ok(Box::new(Observers { center, tokens }))
    }
}

/// The localized name of the application a workspace notification is about.
fn app_name(note: &NSNotification) -> Option<String> {
    let info = note.userInfo()?;
    let key = NSString::from_str("NSWorkspaceApplicationKey");
    // A checked downcast rather than a transmute: the key is documented to
    // hold an NSRunningApplication, but this dictionary comes from another
    // process's notification, so it is worth actually verifying.
    let app = info
        .objectForKey(&key)?
        .downcast::<NSRunningApplication>()
        .ok()?;
    app.localizedName().map(|name| name.to_string())
}

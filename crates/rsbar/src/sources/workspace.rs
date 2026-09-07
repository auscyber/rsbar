//! Workspace events: the front application, spaces, displays, sleep and wake.

use crate::sources::{Emitter, Registration, Source, SourceId, StartError};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSNotificationName, NSString};
use rsbar_protocol::event::{DisplayChange, FrontApp, SpaceChange, SystemWillSleep, SystemWoke};
use rsbar_protocol::{Event, Kind};

/// What a notification contributes to `RSBAR_INFO`, beyond the bare fact that
/// it happened.
/// Builds the event a notification means, reading whatever it carries.
type ToEvent = fn(&NSNotification) -> Event;

/// Keeps observers registered. Dropping it deregisters them, on the thread that
/// registered them.
///
/// Registration is one call per notification and there are several of them, so
/// this owns the repetition rather than leaving it at every call site.
struct Observers {
    center: Retained<NSNotificationCenter>,
    tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl Observers {
    fn new(center: Retained<NSNotificationCenter>) -> Self {
        Self {
            center,
            tokens: Vec::new(),
        }
    }

    /// Registers one notification as `event`, carrying whatever `info` pulls
    /// out of it.
    fn observe(&mut self, name: &NSNotificationName, emit: &Emitter, to_event: ToEvent) {
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

/// The localized name of the application a notification is about, as the event
/// it means.
fn front_app(note: &NSNotification) -> Event {
    let Some(info) = note.userInfo() else {
        return Event::FrontAppSwitched(FrontApp::default());
    };
    let key = NSString::from_str("NSWorkspaceApplicationKey");
    // A checked downcast rather than a transmute: the key is documented to hold
    // an NSRunningApplication, but this dictionary comes from another process's
    // notification, so it is worth actually verifying.
    let app = info
        .objectForKey(&key)
        .and_then(|value| value.downcast::<NSRunningApplication>().ok())
        .and_then(|app| app.localizedName())
        .map(|name| name.to_string())
        .unwrap_or_default();
    Event::FrontAppSwitched(FrontApp { app })
}

fn space_changed(_: &NSNotification) -> Event {
    // The notification says only that it happened; which space is a separate
    // question the window server answers.
    Event::SpaceChanged(SpaceChange::default())
}

fn display_changed(_: &NSNotification) -> Event {
    Event::DisplayChanged(DisplayChange {})
}

fn will_sleep(_: &NSNotification) -> Event {
    Event::SystemWillSleep(SystemWillSleep {})
}

fn woke(_: &NSNotification) -> Event {
    Event::SystemWoke(SystemWoke {})
}

pub struct Workspace;

impl Source for Workspace {
    fn id(&self) -> SourceId {
        SourceId("workspace")
    }

    fn provides(&self) -> Vec<Kind> {
        vec![
            Kind::FrontAppSwitched,
            Kind::SpaceChanged,
            Kind::DisplayChanged,
            Kind::SystemWoke,
            Kind::SystemWillSleep,
        ]
    }

    fn register(&mut self, emit: Emitter) -> Result<Registration, StartError> {
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        let mut observers = Observers::new(center);

        // `NSWorkspaceActiveDisplayDidChangeNotification` is undocumented and
        // absent from the generated bindings, so it is named by string — the
        // same way SketchyBar reaches it.
        let display_notification =
            NSString::from_str("NSWorkspaceActiveDisplayDidChangeNotification");

        for (name, to_event) in [
            (
                unsafe { objc2_app_kit::NSWorkspaceDidActivateApplicationNotification },
                front_app as ToEvent,
            ),
            (
                unsafe { objc2_app_kit::NSWorkspaceActiveSpaceDidChangeNotification },
                space_changed as ToEvent,
            ),
            (&display_notification, display_changed as ToEvent),
            (
                unsafe { objc2_app_kit::NSWorkspaceWillSleepNotification },
                will_sleep as ToEvent,
            ),
            (
                unsafe { objc2_app_kit::NSWorkspaceDidWakeNotification },
                woke as ToEvent,
            ),
        ] {
            observers.observe(name, &emit, to_event);
        }

        Ok(Box::new(observers))
    }
}

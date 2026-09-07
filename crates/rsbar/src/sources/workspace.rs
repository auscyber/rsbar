//! Workspace events: the front application, spaces, displays, sleep and wake.

use crate::sources::{Emission, Emitter, Registration, Source, SourceId, StartError};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSNotificationName, NSString};
use rsbar_protocol::{Event, Info};

/// What a notification contributes to `RSBAR_INFO`, beyond the bare fact that
/// it happened.
type ExtractInfo = fn(&NSNotification) -> Info;

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
    fn observe(
        &mut self,
        name: &NSNotificationName,
        emit: &Emitter,
        event: Event,
        info: ExtractInfo,
    ) {
        let emit = emit.clone();
        let block = RcBlock::new(move |note: std::ptr::NonNull<NSNotification>| {
            // SAFETY: the notification is live for the duration of the call.
            let info = info(unsafe { note.as_ref() });
            // A full channel means the daemon is not keeping up. Dropping the
            // event beats blocking a system notification callback.
            emit.send(Emission::new(event.clone(), info));
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

/// Notifications that carry nothing worth passing on.
fn no_info(_: &NSNotification) -> Info {
    Info::None
}

/// The localized name of the application a workspace notification is about.
fn app_name(note: &NSNotification) -> Info {
    let Some(info) = note.userInfo() else {
        return Info::None;
    };
    let key = NSString::from_str("NSWorkspaceApplicationKey");
    // A checked downcast rather than a transmute: the key is documented to hold
    // an NSRunningApplication, but this dictionary comes from another process's
    // notification, so it is worth actually verifying.
    let Some(app) = info
        .objectForKey(&key)
        .and_then(|value| value.downcast::<NSRunningApplication>().ok())
    else {
        return Info::None;
    };
    app.localizedName().map_or(Info::None, |name| Info::App {
        name: name.to_string(),
    })
}

pub struct Workspace;

impl Source for Workspace {
    fn id(&self) -> SourceId {
        SourceId("workspace")
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

    fn register(&mut self, emit: Emitter) -> Result<Registration, StartError> {
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        let mut observers = Observers::new(center);

        // `NSWorkspaceActiveDisplayDidChangeNotification` is undocumented and
        // absent from the generated bindings, so it is named by string — the
        // same way SketchyBar reaches it.
        let display_changed = NSString::from_str("NSWorkspaceActiveDisplayDidChangeNotification");

        for (name, event, info) in [
            (
                unsafe { objc2_app_kit::NSWorkspaceDidActivateApplicationNotification },
                Event::FrontAppSwitched,
                app_name as ExtractInfo,
            ),
            (
                unsafe { objc2_app_kit::NSWorkspaceActiveSpaceDidChangeNotification },
                Event::SpaceChanged,
                no_info as ExtractInfo,
            ),
            (
                &display_changed,
                Event::DisplayChanged,
                no_info as ExtractInfo,
            ),
            (
                unsafe { objc2_app_kit::NSWorkspaceWillSleepNotification },
                Event::SystemWillSleep,
                no_info as ExtractInfo,
            ),
            (
                unsafe { objc2_app_kit::NSWorkspaceDidWakeNotification },
                Event::SystemWoke,
                no_info as ExtractInfo,
            ),
        ] {
            observers.observe(name, &emit, event, info);
        }

        Ok(Box::new(observers))
    }
}

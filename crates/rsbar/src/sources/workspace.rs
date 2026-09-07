//! Workspace events: the front application, spaces, sleep and wake.
//!
//! Displays are not here. `NSWorkspace`'s display notification and
//! `CoreGraphics`' reconfiguration callback both mean `display_changed`, and
//! having two sources claim it meant subscribing to it started both — five
//! notification observers for a bar that only wanted to know a monitor was
//! plugged in. They live together in [`super::displays`] instead.

use crate::sources::observers::{Observers, ToEvent};
use crate::sources::{Cause, Emitter, Registering, Registration, Source, SourceId, StartError};
use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_foundation::{NSNotification, NSString};
use rsbar_protocol::event::{FrontApp, SystemWillSleep, SystemWoke};
use rsbar_protocol::{Event, Kind};
use std::collections::BTreeSet;

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
            Kind::SystemWoke,
            Kind::SystemWillSleep,
        ]
    }

    fn register(
        &mut self,
        wanted: &BTreeSet<Kind>,
        cx: &mut Registering<'_>,
    ) -> Result<Registration, StartError> {
        let emit = cx.emitter();
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        let mut observers = Observers::new(center);
        install(&mut observers, wanted, &emit);
        Ok(Box::new(observers))
    }

    fn update(
        &mut self,
        wanted: &BTreeSet<Kind>,
        current: &mut Registration,
        cx: &mut Registering<'_>,
    ) -> Result<(), StartError> {
        let Some(observers) = current.downcast_mut::<Observers>() else {
            // Not the registration this source handed out. Nothing sane to
            // adjust, so say so rather than quietly observing nothing.
            return Err(StartError::new(self.id(), Cause::MismatchedRegistration));
        };
        observers.retain(wanted);
        install(observers, wanted, &cx.emitter());
        Ok(())
    }
}

/// Adds an observer for each wanted event that does not have one.
///
/// Every event this source provides is one notification, which is what makes
/// the set a subscription asks for something it can honour exactly.
fn install(observers: &mut Observers, wanted: &BTreeSet<Kind>, emit: &Emitter) {
    for (kind, name, to_event) in [
        (
            Kind::FrontAppSwitched,
            unsafe { objc2_app_kit::NSWorkspaceDidActivateApplicationNotification },
            front_app as ToEvent,
        ),
        (
            Kind::SystemWillSleep,
            unsafe { objc2_app_kit::NSWorkspaceWillSleepNotification },
            will_sleep as ToEvent,
        ),
        (
            Kind::SystemWoke,
            unsafe { objc2_app_kit::NSWorkspaceDidWakeNotification },
            woke as ToEvent,
        ),
    ] {
        if wanted.contains(&kind) && !observers.has(&kind) {
            observers.observe(kind, name, emit, to_event);
        }
    }
}

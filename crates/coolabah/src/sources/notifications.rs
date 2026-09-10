//! Every notification this daemon observes, on whichever centre carries it.
//!
//! One source per centre was the first cut, and it left the same table split
//! across two sources for no reason but which singleton they reached for. So
//! this owns *all* of them: a row says which [`Centre`] carries the
//! notification, what it is called there, and the event it means, and nothing
//! else in the tree holds a centre or registers an observer.
//!
//! The rows come from two places and are otherwise identical. Four are fixed —
//! the front application, sleep, wake, and focus moving to another display, all
//! on `NSWorkspace`'s centre. The rest are written by a config at runtime:
//! `--add event <name> <NSDistributedNotificationName>` declares one the
//! *system* fires, and [`Declared`] is where the registry leaves it. That is
//! the whole difference — a bridged custom event is a row whose centre is the
//! distributed one and whose existence is decided by a request rather than by
//! this file.
//!
//! Distributed notifications are their own centre because an
//! `NSNotificationCenter` only ever hears what this process posts, which for a
//! bar reacting to other applications is nothing at all. It is a subclass, so
//! the observer bookkeeping is the superclass's either way.
//!
//! A centre is fetched on the first row that needs it and let go with the last
//! one — `NSDistributedNotificationCenter::defaultCenter()` connects this
//! process to `distnoted`, and a bar that never declared a bridged event
//! should not be paying for that.
//!
//! Displays are half here: `NSWorkspaceActiveDisplayDidChangeNotification` is
//! a row like any other, but `CoreGraphics`' reconfiguration callback is not a
//! notification and stays in [`super::displays`], which is eager for it.

use crate::protocol::event::{
    AppLaunch, Custom, DisplayChange, FrontApp, SystemWillSleep, SystemWoke,
};
use crate::protocol::{Event, EventName, Kind, NotificationName};
use crate::sources::observers::{Observers, ToEvent, ToEventWith};
use crate::sources::{Emitter, Registering, Source, SourceId, StartError};
use objc2::rc::Retained;
use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_foundation::{
    NSDistributedNotificationCenter, NSNotification, NSNotificationCenter, NSNotificationName,
    NSString,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

/// Which declared event each notification name means.
///
/// Shared with the [`Registry`](super::Registry) rather than owned here,
/// because a declaration arrives as a request long after the source table is
/// built: the registry writes it and this reads it at register time. A
/// `Mutex` and not a channel because it is state, not a stream — a source
/// re-registering has to see the whole set, not the last change.
pub type Declared = Arc<Mutex<HashMap<EventName, NotificationName>>>;

/// A notification centre this source observes on.
///
/// The discriminant is the index into [`Installed`], so reaching a centre's
/// observers is an index rather than a search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Centre {
    /// `NSWorkspace`'s own centre: what this process is told about the machine
    /// it is running on.
    Workspace,
    /// `NSDistributedNotificationCenter`: what every *other* process on the
    /// machine posts. Nothing reaches for this until a config has bridged an
    /// event from it.
    Distributed,
}

impl Centre {
    const ALL: [Self; 2] = [Self::Workspace, Self::Distributed];

    /// Fetches the centre itself. Called once per centre per registration, on
    /// the first row that needs it.
    fn get(self) -> Retained<NSNotificationCenter> {
        match self {
            Self::Workspace => NSWorkspace::sharedWorkspace().notificationCenter(),
            // A distributed centre *is* a notification centre; the observer
            // calls are the superclass's.
            Self::Distributed => {
                Retained::into_super(NSDistributedNotificationCenter::defaultCenter())
            }
        }
    }
}

impl std::fmt::Display for Centre {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Workspace => "NSWorkspace",
            Self::Distributed => "NSDistributedNotificationCenter",
        })
    }
}

/// The observers this source is holding, by centre.
///
/// `None` means that centre has never been needed, which is the state a bar
/// with no bridged events leaves the distributed one in for its whole life.
/// The same lazy start and lazy stop the registry gives sources, one level
/// down: a centre appears with the first row on it and goes with the last.
#[derive(Default)]
struct Installed {
    centres: [Option<Observers>; Centre::ALL.len()],
}

impl Installed {
    /// This centre's observers, fetching the centre if this is the first row
    /// that needs it.
    fn on(&mut self, centre: Centre) -> &mut Observers {
        self.centres[centre as usize].get_or_insert_with(|| {
            tracing::debug!(%centre, "opened a notification centre");
            Observers::new(centre.get())
        })
    }

    /// Drops the observers for everything outside `wanted`, and lets go of any
    /// centre left with none.
    fn retain(&mut self, wanted: &BTreeSet<Kind>) {
        for (index, slot) in self.centres.iter_mut().enumerate() {
            let Some(observers) = slot else { continue };
            observers.retain(wanted);
            if observers.is_empty() {
                tracing::debug!(centre = %Centre::ALL[index], "closed a notification centre");
                *slot = None;
            }
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

/// The same, for an application that has just finished launching.
fn app_launched(note: &NSNotification) -> Event {
    Event::AppLaunched(AppLaunch {
        app: launched_name(note),
    })
}

/// `NSWorkspaceApplicationKey`'s localized name, empty if the notification
/// did not carry one — the same extraction [`front_app`] does.
fn launched_name(note: &NSNotification) -> String {
    let Some(info) = note.userInfo() else {
        return String::new();
    };
    let key = NSString::from_str("NSWorkspaceApplicationKey");
    info.objectForKey(&key)
        .and_then(|value| value.downcast::<NSRunningApplication>().ok())
        .and_then(|app| app.localizedName())
        .map(|name| name.to_string())
        .unwrap_or_default()
}

fn will_sleep(_: &NSNotification) -> Event {
    Event::SystemWillSleep(SystemWillSleep {})
}

fn woke(_: &NSNotification) -> Event {
    Event::SystemWoke(SystemWoke {})
}

fn display_changed(_: &NSNotification) -> Event {
    Event::DisplayChanged(DisplayChange {})
}

/// The event one bridged notification means, with whatever it carried.
///
/// A distributed notification's `userInfo` is another process's dictionary,
/// so only the entries that are already strings are taken: those are what an
/// environment variable can hold, and what a script reading `$INFO` expects.
/// Anything else is dropped rather than stringified into something a config
/// would have to guess the shape of.
///
/// The name has to be captured rather than read off the notification: a
/// distributed notification never says which event it was *declared* as, since
/// the poster does not know coolabah exists.
fn bridged(name: &EventName, note: &NSNotification) -> Event {
    let mut vars = BTreeMap::new();
    if let Some(info) = note.userInfo() {
        for key in info.allKeys().to_vec() {
            let Ok(key) = key.downcast::<NSString>() else {
                continue;
            };
            let Some(value) = info
                .objectForKey(&key)
                .and_then(|value| value.downcast::<NSString>().ok())
            else {
                continue;
            };
            vars.insert(key.to_string(), value.to_string());
        }
    }
    Event::Custom(Custom {
        name: name.as_str().to_owned(),
        vars,
    })
}

pub struct Notifications {
    declared: Declared,
    /// How a running future is told demand changed — see
    /// [`Source::update`]. `None` until the first [`Source::run`].
    adjust: Option<tokio::sync::watch::Sender<BTreeSet<Kind>>>,
}

impl Notifications {
    #[must_use]
    pub fn new(declared: Declared) -> Self {
        Self {
            declared,
            adjust: None,
        }
    }
}

/// Adds an observer for each wanted row that does not have one.
///
/// The fixed rows first, then the ones a config wrote. A row nobody asked for
/// is a notification nobody observes — and a centre carrying only such rows is
/// a centre never opened.
///
/// A free function rather than a method: it runs inside
/// [`Notifications::run`]'s returned future, which owns `installed` for the
/// rest of the source's life and no longer has a `&self` to call back into.
fn install(
    declared: &Declared,
    installed: &mut Installed,
    wanted: &BTreeSet<Kind>,
    emit: &Emitter,
) {
    // SAFETY: `NSString` constants exported by AppKit, immortal for the
    // lifetime of the process; reading them is unsafe only because they are
    // `extern` statics.
    let (activated, launched, sleeping, waking) = unsafe {
        (
            objc2_app_kit::NSWorkspaceDidActivateApplicationNotification,
            objc2_app_kit::NSWorkspaceDidLaunchApplicationNotification,
            objc2_app_kit::NSWorkspaceWillSleepNotification,
            objc2_app_kit::NSWorkspaceDidWakeNotification,
        )
    };
    // `NSWorkspaceActiveDisplayDidChangeNotification` is undocumented and
    // absent from the generated bindings, so it is named by string — the
    // same way SketchyBar reaches it.
    let focus_moved = NSString::from_str("NSWorkspaceActiveDisplayDidChangeNotification");

    for (kind, centre, name, to_event) in [
        (
            Kind::FrontAppSwitched,
            Centre::Workspace,
            activated,
            front_app as ToEvent,
        ),
        (
            Kind::AppLaunched,
            Centre::Workspace,
            launched,
            app_launched as ToEvent,
        ),
        (
            Kind::SystemWillSleep,
            Centre::Workspace,
            sleeping,
            will_sleep as ToEvent,
        ),
        (Kind::SystemWoke, Centre::Workspace, waking, woke as ToEvent),
        (
            Kind::DisplayChanged,
            Centre::Workspace,
            &*focus_moved,
            display_changed as ToEvent,
        ),
    ] {
        add(
            installed,
            wanted,
            emit,
            kind,
            centre,
            name,
            Box::new(to_event),
        );
    }

    // One row per `--add event <name> <notification>`. Nothing is registered
    // for an event declared without a notification: `--trigger` fires it and
    // there is no system notification to wait for.
    let Ok(declared) = declared.lock() else {
        return;
    };
    for (event, notification) in declared.iter() {
        let kind = event.kind();
        let name = NSString::from_str(notification.as_str());
        let event = event.clone();
        add(
            installed,
            wanted,
            emit,
            kind,
            Centre::Distributed,
            &name,
            Box::new(move |note| bridged(&event, note)),
        );
    }
}

/// One row, installed if it is wanted and not already up.
fn add(
    installed: &mut Installed,
    wanted: &BTreeSet<Kind>,
    emit: &Emitter,
    kind: Kind,
    centre: Centre,
    name: &NSNotificationName,
    to_event: ToEventWith,
) {
    if !wanted.contains(&kind) {
        return;
    }
    // Asked before `on`, so a row already up does not reopen its centre — and
    // so an unwanted row never opens one at all.
    let observers = installed.on(centre);
    if !observers.has(&kind) {
        observers.observe_with(kind, name, emit, to_event);
    }
}

impl Source for Notifications {
    fn id(&self) -> SourceId {
        SourceId("notifications")
    }

    /// The fixed rows, plus whatever a config has bridged so far.
    ///
    /// The second half is empty until a declaration lands, which is why the
    /// registry has to be told when one does rather than reading this once at
    /// start-up.
    fn provides(&self) -> Vec<Kind> {
        let mut kinds = vec![
            Kind::FrontAppSwitched,
            Kind::AppLaunched,
            Kind::SystemWoke,
            Kind::SystemWillSleep,
            Kind::DisplayChanged,
        ];
        if let Ok(declared) = self.declared.lock() {
            kinds.extend(declared.keys().map(EventName::kind));
        }
        kinds
    }

    /// Installs the wanted rows, then holds them — and reinstalls as demand
    /// moves — until dropped.
    ///
    /// Each observer token lives inside the one future this returns rather
    /// than behind a registration handed back to the registry, and
    /// [`Source::update`] adjusts the running set by sending
    /// over [`Notifications::adjust`] instead of reaching into anything.
    /// Nothing here needs the main thread specifically — `NSNotificationCenter`
    /// answers from any thread — but the observers it holds are `!Send`
    /// (`Retained` is), so the future has to live on a single thread, and the
    /// main one is already where every other source's holds its.
    fn run(
        &mut self,
        wanted: &BTreeSet<Kind>,
        cx: Registering,
    ) -> Result<crate::pool::Task, StartError> {
        let emit = cx.emitter();
        let mut installed = Installed::default();
        install(&self.declared, &mut installed, wanted, &emit);

        let (adjust, mut changes) = tokio::sync::watch::channel(wanted.clone());
        self.adjust = Some(adjust);
        let declared = std::sync::Arc::clone(&self.declared);

        Ok(crate::runloop::owned(
            cx.main_thread(),
            move |_proof| async move {
                let mut installed = installed;
                while changes.changed().await.is_ok() {
                    let wanted = changes.borrow_and_update().clone();
                    installed.retain(&wanted);
                    install(&declared, &mut installed, &wanted, &emit);
                }
            },
        ))
    }

    fn update(&mut self, wanted: &BTreeSet<Kind>, cx: &mut Registering) -> Result<(), StartError> {
        let _ = cx;
        if let Some(adjust) = &self.adjust {
            // An error means the future has already gone, which only happens
            // once its `Task` is dropped -- and the registry drops that
            // before it ever calls `update` again. Nothing to do either way.
            let _ = adjust.send(wanted.clone());
        }
        Ok(())
    }
}

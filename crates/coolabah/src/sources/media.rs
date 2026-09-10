//! Now-playing changes, from `MediaRemote` — except this refuses to start.
//!
//! `MRMediaRemoteRegisterForNowPlayingNotifications` and
//! `MRMediaRemoteGetNowPlayingInfo` are entitlement-gated as of macOS 15.3, and
//! this process holds no such entitlement. `SketchyBar`'s own `media.m` says as
//! much in a comment above its (now non-functional) provider: "The media
//! remote private framework was locked for use on macOS 15.3."
//!
//! This was checked empirically before writing anything else here, on macOS
//! 26.5.1, with a spike that `dlopen`s `MediaRemote.framework` directly:
//!
//! - `dlopen` succeeds; `MRMediaRemoteRegisterForNowPlayingNotifications`,
//!   `MRMediaRemoteGetNowPlayingInfo`, `MRMediaRemoteGetNowPlayingApplicationIsPlaying`
//!   and the `kMRMediaRemoteNowPlayingInfoDidChangeNotification` string all
//!   resolve via `dlsym` and can be called without crashing.
//! - Registering for the notification, and separately observing it through
//!   both `NSNotificationCenter` and `NSDistributedNotificationCenter`,
//!   produced nothing while `QuickTime Player` played a 20-second local audio
//!   file for its full duration (confirmed playing via `playing of document 1`
//!   for the whole window).
//! - `MRMediaRemoteGetNowPlayingInfo` returned `nil` and
//!   `MRMediaRemoteGetNowPlayingApplicationIsPlaying` reported `false`
//!   throughout that same confirmed-playing window.
//!
//! None of the three calls return an error — they succeed and simply never
//! produce anything, indistinguishable at the API boundary from "nothing is
//! playing". A source built on top of that would register successfully, log
//! nothing wrong, and never fire — exactly the failure mode this crate has
//! hit before and is trying not to repeat. So rather than ship that, this
//! source refuses to start at all: subscribing to `media_changed` fails
//! loudly, in the log, with [`Cause::MediaRemoteBlocked`], instead of quietly
//! doing nothing forever.
//!
//! If a future build of this daemon carries the entitlement Apple grants
//! `Music.app`, `Podcasts.app` and a handful of others, the fix is to delete
//! this file's `run` body and write the real one — the framework calls
//! above are the ones to use, and the notification and dictionary key names
//! are unchanged.

use crate::protocol::Kind;
use crate::sources::{Cause, Registering, Source, SourceId, StartError};
use std::collections::BTreeSet;

pub struct Media;

impl Source for Media {
    fn id(&self) -> SourceId {
        SourceId("media")
    }

    fn provides(&self) -> Vec<Kind> {
        vec![Kind::MediaChanged]
    }

    fn run(
        &mut self,
        _wanted: &BTreeSet<Kind>,
        _cx: Registering,
    ) -> Result<crate::pool::Task, StartError> {
        Err(StartError::new(self.id(), Cause::MediaRemoteBlocked))
    }
}

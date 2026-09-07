//! Clients that want events pushed to them, rather than a script run.
//!
//! A subscription normally ends in a forked script: the daemon runs a shell
//! command with the event in its environment. A long-lived client — a Lua
//! config hosting its own VM — wants the opposite, so it attaches a port to
//! its `Subscribe` and the event is handed straight back over it. `SketchyBar`
//! calls the same idea `mach_helper`.
//!
//! Out of the ECS, alongside the shaped text and the alias captures, because
//! the port is a kernel right rather than data.

use bevy_ecs::entity::Entity;
use rsbar_protocol::Event;
use std::collections::HashMap;

/// Where an item's events go, when somewhere is not a script.
#[derive(Default)]
pub struct Subscribers(HashMap<Entity, async_mach_ports::Subscriber>);

impl Subscribers {
    /// Routes an item's events to `port`, replacing wherever they went before.
    pub fn set(&mut self, item: Entity, port: async_mach_ports::Subscriber) {
        self.0.insert(item, port);
    }

    /// Stops routing an item's events, so they go back to its script.
    pub fn clear(&mut self, item: Entity) {
        self.0.remove(&item);
    }

    /// Pushes an event to whoever is listening for this item.
    ///
    /// Reports whether it was taken: an item with a live subscriber does not
    /// also run its script, or a Lua config would fork a shell for every event
    /// it handles itself.
    pub fn push(&mut self, item: Entity, event: &Event) -> bool {
        let Some(port) = self.0.get(&item) else {
            return false;
        };
        match port.try_send(event) {
            Ok(()) => true,
            // The client is gone. Its events go back to being nobody's.
            Err(async_mach_ports::Error::PeerGone) => {
                tracing::debug!("a subscriber exited; dropping its port");
                self.0.remove(&item);
                false
            }
            // Alive but behind. Dropping beats stalling the daemon on a client
            // that is not reading, which is the same trade a source makes.
            Err(err) => {
                tracing::warn!(%err, "dropping an event; a subscriber is not keeping up");
                true
            }
        }
    }
}

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
//!
//! # The queue in front of the port
//!
//! A Mach port holds **two** messages before a send has to wait — measured,
//! not the 5 of `MACH_PORT_QLIMIT_DEFAULT` — and the depth is the kernel's to
//! choose. So a subscriber that stops reading for an instant backs up almost
//! immediately, and the daemon is left with two bad answers: block the thread
//! that composites the bar, or throw the event away. This used to throw it
//! away.
//!
//! Neither is necessary, because the shallow queue is only the *kernel's*.
//! Each port gets an unbounded queue of its own in this process, and a task
//! that exists to drain it. [`Subscribers::push`] hands an event over and
//! returns — it cannot block and cannot fail for want of room — and the
//! backlog, if there is one, is ours: visible, attributable to a client, and
//! bounded only by memory.
//!
//! **One task per port, and per-port ordering follows from that.** A queue is
//! drained in sequence by the one task that owns its port, so a `mouse.exited`
//! cannot overtake the `mouse.entered` before it. Concurrency is *across*
//! subscribers: a client that has wedged holds up nothing but its own events.
//!
//! # A task, not a thread
//!
//! A wedged subscriber used to hold a whole thread parked in `mach_msg`, and a
//! Lua config subscribing seventeen items meant seventeen threads whose entire
//! job was to be asleep. Each drain is now a task on [`crate::pool`], and it
//! holds no thread at all — not even while the client has stopped reading.
//!
//! A Mach port has no writable event: `EVFILT_MACHPORT` reports arrivals only,
//! so there is nothing for a reactor to poll for room. What there is instead is
//! `MACH_NOTIFY_SEND_POSSIBLE` — ask the kernel to send a *message* when the
//! destination has room, and await that message, which a reactor can wait for.
//! That is what [`Subscriber::send`](rsbar_protocol::wire::Subscriber::send)
//! does, so the wait is an ordinary `.await` and the kernel still wakes the
//! drain at the moment the subscriber takes a message.
//!
//! Unbounded is not unwatched, the same rule [`crate::sources`] follows: a
//! backlog past [`HIGH_WATER`] is logged as it forms and again when it clears,
//! with the item that owns it, because a wedged client that nothing says
//! anything about is one that hides for months.

use bevy_ecs::entity::Entity;
use rsbar_protocol::Event;
use rsbar_protocol::wire::Subscriber;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

/// How deep one subscriber's backlog may get before it is worth saying so.
///
/// Not a cap: nothing is refused or dropped at this depth, and every event
/// past it is delivered like any other. It means a client is not keeping up,
/// which is a thing to see in the log rather than one to hide by throwing
/// events away. The same number, for the same reason, as
/// [`crate::sources`]'s.
const HIGH_WATER: usize = 256;

/// One subscriber's port, the queue in front of it, and the task draining the
/// one into the other.
///
/// Dropping this drops the queue's sending end; the drain's `recv` then
/// answers `None`, it finishes what it is holding and returns, and the port is
/// released with it. That is the whole of the teardown — [`Subscribers::clear`]
/// and the daemon exiting both go through it, and neither has to remember
/// anything.
struct Feed {
    /// Unbounded on purpose: `send` on this cannot wait and cannot fail for
    /// want of room, which is what lets [`Subscribers::push`] be infallible on
    /// the thread that draws the bar. It is also why the queue is tokio's
    /// rather than a channel with an async `send` — the *producer* here is the
    /// main thread, which has nowhere to `.await`. It fails only once the drain
    /// has gone, which is how a dead peer is noticed.
    events: UnboundedSender<Event>,
    /// How many events are queued but not yet handed to the kernel. Shared
    /// with the drain, which is the only thing that decrements it.
    depth: Arc<AtomicUsize>,
}

impl Feed {
    /// Starts draining `port`, as a task on [`crate::pool`].
    fn start(item: Entity, port: Subscriber) -> Self {
        let (events, mut backlog) = unbounded_channel::<Event>();
        let depth = Arc::new(AtomicUsize::new(0));

        let counted = Arc::clone(&depth);
        crate::pool::spawn(async move {
            let mut warned = false;
            // Ends when the last sender is dropped: `clear`, a replaced
            // subscription, or the process going away.
            while let Some(event) = backlog.recv().await {
                // Yields until the subscriber has room, rather than holding a
                // thread while it does not.
                match port.send(&event).await {
                    Ok(()) => {}
                    // The client is gone. Leaving the loop drops the receiving
                    // end, and the next `push` finds the queue closed and
                    // forgets the subscription.
                    Err(async_mach_ports::Error::PeerGone) => {
                        tracing::debug!(?item, "a subscriber exited; dropping its port");
                        return;
                    }
                    // Anything else is about this one event, not the channel,
                    // so the queue keeps moving.
                    Err(err) => tracing::warn!(%err, ?item, "an event could not be pushed"),
                }

                let left = counted.fetch_sub(1, Ordering::Relaxed) - 1;
                if warned && left < HIGH_WATER {
                    warned = false;
                    tracing::info!(?item, "a subscriber caught up");
                } else if !warned && left >= HIGH_WATER {
                    warned = true;
                }
            }
        });

        Self { events, depth }
    }

    /// Queues one event, reporting whether the drain thread is still there to
    /// take it.
    fn push(&self, item: Entity, event: &Event) -> bool {
        // Counted *before* it is queued, never after: the drain thread can see
        // the event the instant `send` returns, and a decrement that overtook
        // its increment would take the depth below zero.
        let queued = self.depth.fetch_add(1, Ordering::Relaxed) + 1;
        if self.events.send(event.clone()).is_err() {
            self.depth.fetch_sub(1, Ordering::Relaxed);
            return false;
        }
        // On the crossing only. Past it every push would say the same thing,
        // which is how a log stops being read.
        if queued == HIGH_WATER {
            tracing::warn!(
                ?item,
                queued,
                "a subscriber is not keeping up; its backlog is growing"
            );
        }
        true
    }
}

/// Where an item's events go, when somewhere is not a script.
#[derive(Default)]
pub struct Subscribers(HashMap<Entity, Feed>);

impl Subscribers {
    /// Routes an item's events to `port`, replacing wherever they went before.
    ///
    /// Replacing drops the old [`Feed`], which stops its drain once it has
    /// finished the event in its hands.
    pub fn set(&mut self, item: Entity, port: Subscriber) {
        self.0.insert(item, Feed::start(item, port));
    }

    /// Stops routing an item's events, so they go back to its script.
    pub fn clear(&mut self, item: Entity) {
        self.0.remove(&item);
    }

    /// Pushes an event to whoever is listening for this item.
    ///
    /// Reports whether it was taken: an item with a live subscriber does not
    /// also run its script, or a Lua config would fork a shell for every event
    /// it handles itself. Taken means *queued for* the subscriber — the send
    /// itself happens on that subscriber's own thread, because this one draws
    /// the bar.
    pub fn push(&mut self, item: Entity, event: &Event) -> bool {
        let Some(feed) = self.0.get(&item) else {
            return false;
        };
        if feed.push(item, event) {
            return true;
        }
        // The drain thread found the peer gone. Its events go back to being
        // nobody's.
        self.0.remove(&item);
        false
    }

    /// How many of an item's events are queued and not yet in the kernel.
    /// Only the tests ask.
    #[must_use]
    pub fn backlog(&self, item: Entity) -> usize {
        self.0
            .get(&item)
            .map_or(0, |feed| feed.depth.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::Subscribers;
    use async_mach_ports::{RecvPort, SendPort};
    use bevy_ecs::entity::Entity;
    use rsbar_protocol::wire::{MessagePack, Receiver, Sender};
    use rsbar_protocol::{Event, Request, event};
    use std::time::{Duration, Instant};

    fn service_name(test: &str) -> String {
        format!("com.auscyber.rsbar.test.{test}.{}", std::process::id())
    }

    /// A numbered event, so a test can say what order they arrived in.
    fn numbered(index: usize) -> Event {
        Event::Custom(event::Custom {
            name: format!("event.{index}"),
            vars: std::collections::BTreeMap::new(),
        })
    }

    /// Sets up a subscription the way a client does, and hands back both ends:
    /// what the daemon pushes to, and what the client would read.
    fn subscription(name: &str) -> (super::Subscriber, Receiver<Event>) {
        let service = service_name(name);
        let daemon = Receiver::<Request>::bind(&service, MessagePack).expect("bind");
        let client = Sender::<Request>::connect(&service, MessagePack).expect("connect");
        let events = client
            .subscribe_blocking::<Event>(&Request::UpdateAll)
            .expect("subscribe");
        let delivery = daemon.recv_blocking().expect("the subscribe request");
        (
            delivery.subscriber.expect("a subscribe carries a port"),
            events,
        )
    }

    /// The point of the queue: far more events than the kernel will hold, none
    /// refused, none lost, and none of them delivered out of order.
    #[test]
    fn a_subscriber_that_is_not_reading_takes_everything_anyway() {
        // Well past the port's depth, which is two.
        const SENT: usize = 64;

        let (port, events) = subscription("subscriber-backlog");
        let mut subscribers = Subscribers::default();
        let item = Entity::from_raw_u32(1).expect("an entity");
        subscribers.set(item, port);

        for index in 0..SENT {
            assert!(
                subscribers.push(item, &numbered(index)),
                "an event is queued, never refused"
            );
        }

        for index in 0..SENT {
            let event = events.recv_blocking().expect("an event").value;
            assert_eq!(event, numbered(index), "in the order they were pushed");
        }
    }

    /// Pushing does not wait on the client: a subscriber that reads nothing at
    /// all still cannot hold up the thread that draws the bar.
    #[test]
    fn pushing_does_not_wait_for_a_subscriber_that_never_reads() {
        const SENT: usize = 512;

        let (port, _events) = subscription("subscriber-stalled");
        let mut subscribers = Subscribers::default();
        let item = Entity::from_raw_u32(1).expect("an entity");
        subscribers.set(item, port);

        let started = Instant::now();
        for index in 0..SENT {
            assert!(subscribers.push(item, &numbered(index)));
        }
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(500),
            "queueing {SENT} events took {elapsed:?}; it should not wait on the client"
        );
        // Everything past what the kernel took is still ours, and counted.
        assert!(
            subscribers.backlog(item) > 0,
            "a stalled subscriber's backlog is held, not thrown away"
        );
    }

    /// A client that exits is forgotten, so its item goes back to running its
    /// script.
    #[test]
    fn a_subscriber_that_exits_is_dropped() {
        let (port, events) = subscription("subscriber-gone");
        let mut subscribers = Subscribers::default();
        let item = Entity::from_raw_u32(1).expect("an entity");
        subscribers.set(item, port);

        drop(events);

        // The first push after the peer went may still be queued -- the drain
        // thread has not tried the send yet -- so this is what the daemon sees
        // over the next few events, not on any particular one.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !subscribers.push(item, &numbered(0)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("a subscriber whose client exited was never dropped");
    }
}

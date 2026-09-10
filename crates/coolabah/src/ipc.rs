//! Requests, arriving on the thread that can act on them.
//!
//! Receiving off a Mach port costs a thread either way: park one in
//! `mach_msg`, or park one in an executor awaiting a kqueue that a second
//! thread blocks in. Either way the request lands on a thread that may not
//! touch a window, paying two wakeups to get to the main thread.
//!
//! The run loop the daemon already pumps can receive on the port itself: see
//! [`crate::runloop::MachSource`]. The port becomes a native run loop source,
//! `mach_msg` is CoreFoundation's, and the handler runs on the main thread
//! with the message in hand -- no thread parked, one wakeup total.
//!
//! Because the run loop does the receive, `Receiver::decode_message` reads
//! the message directly; `try_recv` would find nothing on the port's queue.
//!
//! [`Requests`] is what's left: a `RefCell<VecDeque>` rather than a
//! synchronised channel, since both ends are on one thread. It exists
//! because delivery and the pass that applies requests are different
//! moments -- a burst arrives as several callbacks but should be applied as
//! one update.

use crate::ecs::IpcRequest;
use crate::protocol::Request;
use crate::protocol::wire::Receiver;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

/// What the run loop has taken off the port and the app has not yet applied.
///
/// Single-threaded on purpose: the old queue was a bounded `mpsc::SyncSender`
/// synchronising an IPC thread with the main thread. With no IPC thread and no
/// cross-thread handover, neither the bound nor the synchronisation buys
/// anything -- backpressure now comes from the kernel's own port queue limit,
/// which is what makes a client wait when the daemon is mid-pass.
#[derive(Default)]
pub struct Requests(RefCell<VecDeque<IpcRequest>>);

impl Requests {
    /// Queues a request for the next pass.
    fn push(&self, request: IpcRequest) {
        self.0.borrow_mut().push_back(request);
    }

    /// Takes the oldest request, if there is one.
    ///
    /// The borrow is released before the caller does anything with what it
    /// got: applying a request can re-enter the run loop, and the delivery
    /// handler pushes into this same queue.
    #[must_use]
    pub fn pop(&self) -> Option<IpcRequest> {
        self.0.borrow_mut().pop_front()
    }

    /// How many are waiting. Only the tests ask.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.borrow().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The service port, on the run loop, for as long as this is held.
///
/// Dropping it stops the daemon answering — so it lives as long as the app
/// does. It keeps the receiver alive alongside the source rather than leaving
/// that to the handler's box, so the receive right cannot be released while
/// CoreFoundation still has the port.
///
/// **The field order is the drop order, and that is what makes it true.** The
/// source comes off the run loop first; only then is the last `Rc` to the
/// receiver released, which is what deallocates the receive right. Swap the
/// two and the run loop is left holding a `CFMachPort` over a name the kernel
/// is free to hand to something else.
pub struct Service {
    _source: crate::runloop::MachSource,
    _receiver: Rc<Receiver<Request>>,
}

/// Puts `receiver`'s port on the current thread's run loop, queueing what
/// arrives into `requests` and calling `notify` once per message.
///
/// `notify` is how the app finds out — [`crate::ecs::pass`], in the daemon.
/// It is a parameter rather than a call because the run loop's *only*
/// relationship with the app should be this one, and because a test can pass
/// something that counts.
///
/// Returns `None` if CoreFoundation declines the port, which means something
/// else already wrapped it.
#[skylight::main_thread]
pub fn serve<F: Fn() + 'static>(
    receiver: Receiver<Request>,
    requests: Rc<Requests>,
    notify: F,
) -> Option<Service> {
    let receiver = Rc::new(receiver);
    let port = receiver.as_raw_port();

    let held = Rc::clone(&receiver);
    let deliver = move |message: &[u8]| {
        // `decode_message` turns the message CoreFoundation just received into
        // a request: the layout, trailer, and out-of-line memory never cross
        // into this crate, and the reply right arrives already owned.
        //
        // SAFETY: `message` is the message the run loop just received on this
        // receiver's port, decoded once and by nothing else -- nothing else
        // receives on the port, and `Service` keeps the receiver alive
        // alongside the source that delivers to it.
        match unsafe { held.decode_message(message) } {
            Ok(delivery) => {
                requests.push(IpcRequest {
                    request: Box::new(delivery.value),
                    reply: delivery.reply,
                    subscriber: delivery.subscriber,
                });
                notify();
            }
            // Any process in the session can reach a service port, so a
            // message this protocol cannot read is an expected input rather
            // than a fault. Dropping the delivery releases whatever rights and
            // mapping came with it.
            Err(err) => tracing::warn!(%err, "dropping an undecodable request"),
        }
    };

    // SAFETY: `port` is the receive right the receiver owns, and a clone of
    // that `Rc` is kept in the returned `Service` alongside the source, so the
    // right outlives it. Nothing else receives on the port: taking this route
    // means the crate's own kqueue reactor never sees it, which is exactly
    // what `as_raw_port` documents.
    let source = unsafe { crate::runloop::MachSource::install(proof, port, deliver) }?;

    Some(Service {
        _source: source,
        _receiver: receiver,
    })
}

#[cfg(test)]
mod tests {
    use super::{Requests, serve};
    use crate::protocol::wire::{MessagePack, Receiver};
    use crate::protocol::{Request, Response};
    use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
    use std::cell::Cell;
    use std::rc::Rc;

    /// Lets the run loop take a turn, so a message on the port is delivered.
    fn pump() {
        // SAFETY: `kCFRunLoopDefaultMode` is a `'static` constant, and this is
        // the thread that owns the run loop being pumped.
        unsafe { CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, 0.2, false) };
    }

    fn service_name(test: &str) -> String {
        name!(service test)
    }

    /// The point of the whole change: a request arrives on the run loop
    /// thread, with no thread of its own between the port and the app.
    #[test]
    fn a_request_arrives_on_the_run_loop_and_can_be_answered() {
        let name = service_name("runloop-ipc");
        let receiver = Receiver::<Request>::bind(&name, MessagePack).expect("bind");
        let queue = Rc::new(Requests::default());
        let woken = Rc::new(Cell::new(0u32));

        let counted = Rc::clone(&woken);
        let _service = serve(
            crate::runloop::main_thread(),
            receiver,
            Rc::clone(&queue),
            move || {
                counted.set(counted.get() + 1);
            },
        )
        .expect("the run loop took the port");

        let client_name = name.clone();
        let client = std::thread::spawn(move || {
            use async_mach_ports::SendPort as _;
            let sender =
                crate::protocol::wire::Sender::<Request>::connect(&client_name, MessagePack)
                    .expect("connect");
            sender
                .call_blocking::<Response>(&Request::Query(crate::protocol::Query::Bar))
                .expect("call")
        });

        while queue.is_empty() {
            pump();
        }
        assert_eq!(woken.get(), 1, "one message, one wake");

        let request = queue.pop().expect("a queued request");
        assert!(queue.is_empty(), "exactly one request arrived");
        request
            .reply
            .expect("a call asks for an answer")
            .send(&Response::Ok)
            .expect("reply");

        assert!(matches!(client.join().expect("client"), Response::Ok));
    }

    /// How many messages the burst test sends in one go -- more than the
    /// kernel will hold on the port at once, so the client is made to wait
    /// part way through and the run loop has to keep up.
    const SENT: usize = 32;

    /// A burst is several deliveries and one drain: messages queue into
    /// [`Requests`] and the pass takes them together, so a hundred `--set`s
    /// cost one repaint rather than a hundred.
    #[test]
    fn a_burst_queues_up_and_drains_at_once() {
        let name = service_name("runloop-burst");
        let receiver = Receiver::<Request>::bind(&name, MessagePack).expect("bind");
        let queue = Rc::new(Requests::default());

        let _service = serve(
            crate::runloop::main_thread(),
            receiver,
            Rc::clone(&queue),
            || {},
        )
        .expect("take the port");

        let client_name = name.clone();
        let client = std::thread::spawn(move || {
            use async_mach_ports::SendPort as _;
            let sender =
                crate::protocol::wire::Sender::<Request>::connect(&client_name, MessagePack)
                    .expect("connect");
            for _ in 0..SENT {
                sender.send_blocking(&Request::UpdateAll).expect("send");
            }
        });

        while queue.len() < SENT {
            pump();
        }
        client.join().expect("client");
        assert_eq!(queue.len(), SENT, "nothing was lost on the way in");
    }
}

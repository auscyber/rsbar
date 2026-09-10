//! Spike: does `SLSRemoveNotifyProc` survive removing the *last* listener?
//!
//! The signature was read off the arm64 code (see
//! [`skylight::ffi::SLSRemoveNotifyProc`]). One branch of it is worth checking
//! before relying on it: when the erase empties an event's vector, `SkyLight`
//! makes a further call to the window server, and that call's
//! `kCGErrorInvalidOperation` (0x3f2) edge is a `bl` with the next function
//! immediately after it — a noreturn, i.e. an abort inside `SkyLight`.
//!
//! `sources/spaces.rs` is this process's only user of `SLSRegisterNotifyProc`,
//! so every stop of that source empties four vectors and takes that branch. If
//! it aborts, per-run registration is not safe and the registrations have to
//! outlive the source instead.
//!
//! Delivery is not what this tests, and could not be: `notify_probe.rs`
//! established that none of these events fire on this build. This only asks
//! whether register-then-remove, repeated, leaves the process alive.
//!
//! Usage: `cargo run -p skylight --example notify_remove`
//!
//! # Result (macOS 26.5.1, this machine)
//!
//! **Survives.** Three register/remove rounds across all four of the events
//! `sources/spaces.rs` uses: every call answered `CGError(0)` and the process
//! reached the end. The empty-vector branch is taken on every one of those
//! removals — this process registers nothing else for those events — so it is
//! reached and does not abort here.
//!
//! The connection above is load-bearing, and a run without one would have
//! proved less than it looked. On the empty-vector path the code first calls
//! something and `cbz`s on the answer, skipping the server message — and the
//! abort edge with it — when that answer is zero. A process that never
//! established a connection can take that shortcut and never reach the branch
//! under test at all. With a connection in hand the message is actually sent,
//! and the `kCGErrorInvalidOperation` edge is what did not fire.
//!
//! Removing a `(proc, context)` pair that was **never registered** also answers
//! `CGError(0)`, and so does removing the same pair twice. That is the
//! disassembly's claim confirmed from the outside: found and not-found converge
//! on the same `mov w0, #0`, so the return value carries no information and a
//! caller must not read success as "something was removed".
//!
//! What this does not show is that the entry was actually erased, which needs a
//! delivery to watch stop and therefore cannot be shown on this build at all.
//! It shows the weaker thing that was actually in doubt: repeated
//! register/remove cycles are safe, so a source may register per run.

use skylight::ffi;
use std::ffi::c_void;

const EVENTS: &[u32] = &[1401, 1325, 1326, 1339];

/// The process connection, on the thread the window server answers.
#[skylight::main_thread]
fn connection() -> ffi::ConnectionId {
    // SAFETY: takes no arguments beyond the proof.
    unsafe { ffi::SLSMainConnectionID(proof.marker()) }
}

extern "C-unwind" fn nothing(
    _event: u32,
    _data: *mut c_void,
    _length: usize,
    _context: *mut c_void,
    _connection: ffi::ConnectionId,
) {
}

#[skylight::main(also(connection))]
fn main() {
    // A window server connection first. The removal's "the vector is now
    // empty" path calls something whose zero answer skips the rest -- and
    // skips the `kCGErrorInvalidOperation` abort edge with it. A process that
    // never established a connection may take that shortcut and prove nothing,
    // so establish one, the way `coolabah` has by the time it stops a source.
    let cid = connection();
    println!("connection: {cid:?}");

    // Several rounds, because the branch under test is "the vector became
    // empty" -- one round alone would not show a second registration failing
    // to take after the first removal tore the event down too far.
    for round in 1..=3 {
        for &event in EVENTS {
            let context = std::ptr::without_provenance_mut::<c_void>(0xdead_0000 + event as usize);

            // SAFETY: `nothing` matches the notify procedure signature and
            // reads none of its arguments; `context` is never dereferenced by
            // anything, only compared as an address.
            let registered = unsafe { ffi::SLSRegisterNotifyProc(nothing, event, context) };

            // SAFETY: exactly the three arguments that registered above.
            let removed = unsafe { ffi::SLSRemoveNotifyProc(nothing, event, context) };

            println!("round {round} event {event}: register {registered:?}, remove {removed:?}");
        }
    }

    // The interesting case for a real stop/start: remove something that was
    // never registered, and remove the same thing twice.
    let stray = std::ptr::without_provenance_mut::<c_void>(0x1234);
    // SAFETY: as above -- the context is only ever compared, never read.
    let never = unsafe { ffi::SLSRemoveNotifyProc(nothing, 1401, stray) };
    // SAFETY: as above.
    let again = unsafe { ffi::SLSRemoveNotifyProc(nothing, 1401, stray) };
    println!("never registered: {never:?}, and again: {again:?}");

    println!("SURVIVED");
}

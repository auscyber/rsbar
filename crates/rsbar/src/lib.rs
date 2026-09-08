//! The rsbar daemon: bar geometry, the item world, drawing, and the app that
//! drives them.
//!
//! # What runs on the main thread, and what does not
//!
//! One rule, stated here because it is the one thing every module has to
//! agree on:
//!
//! > **The main thread draws. Nothing else on it may wait.** Anything that
//! > waits — for another process, for the window server, for a subprocess, for
//! > a signal — happens somewhere else and comes back as data.
//!
//! ## The main thread
//!
//! Carbon's `RunApplicationEventLoop` owns it (see [`runloop::AppLoop`]), which is
//! what dispatches clicks and keeps the window server compositing. Everything
//! below is there because the platform gives it no choice, or because it is
//! too cheap to be worth moving:
//!
//! - every `SkyLight` and CoreGraphics call — drawing, window creation, the
//!   menu bar layer, [`text`]'s shaping;
//! - the ECS schedule, which is arithmetic over the item world;
//! - run loop sources and timers, including the one IPC arrives on
//!   ([`ipc`]) and the one that ends the process ([`ecs::exit_waker`]);
//! - alias *resolution* — a hash lookup in a snapshot the pass already has.
//!
//! [`bevy_ecs::system::NonSend`] (36 uses) and `MainThreadMarker` (11) say so
//! to the compiler rather than in prose: a system asking for either cannot be
//! scheduled anywhere else, and a type reached only through them cannot escape.
//!
//! ## The worker pools
//!
//! Two, sized oppositely on purpose, and deliberately not shared:
//!
//! | pool | threads | what it does |
//! |---|---|---|
//! | `rsbar-ax` ([`extras::Scanner`]) | 16, one task per pid | Accessibility RPCs into every running application |
//! | `rsbar-capture` ([`alias::Captor`]) | 4 | window pictures, pixel hashing, finding the inked rect |
//!
//! Sixteen because those tasks are *blocked* in another process's reply, so
//! sizing for cores would leave the machine idle; four because a capture is a
//! window server round trip and seventy-five of them at once is a conversation
//! nobody wins. One shared pool would take the worse of both: twenty-nine
//! aliases' worth of blocked AX tasks queued ahead of the capture that has a
//! frame to make.
//!
//! ## The threads that are each one wait
//!
//! Not pool work — each of these exists to be parked in exactly one place, and
//! a pool would only add a queue in front of it:
//!
//! - `rsbar-signals` ([`signals`]) — `sigwait`;
//! - `rsbar-config` ([`sources::config`]) — runs the config, which is a shell
//!   script that talks back to this daemon over IPC;
//! - `rsbar-script-N` ([`script`]) — subprocesses;
//! - `rsbar-subscriber-N` ([`subscribers`]) — one blocking Mach send per
//!   subscriber, so a stalled subscriber stalls only itself.
//!
//! ## Crossing the line
//!
//! Every off-thread piece of work here has the same shape, and new ones should
//! keep it:
//!
//! 1. the main thread hands over **plain `Copy` data** — a pid, a window id, a
//!    rect. Never a CoreFoundation object it still holds, and never a
//!    `World`;
//! 2. the worker returns **`Send` data** into an `Arc<Mutex<..>>` it owns a
//!    handle to, so a result outlives whatever asked for it;
//! 3. an `AtomicBool` beside that mutex says whether anything landed, because
//!    a run condition asks every pass and must not take a lock a worker holds;
//! 4. [`runloop::Waker::wake`] brings the run loop back. Without it an idle
//!    bar would sit there holding a finished answer;
//! 5. the main thread applies the result inside an ordinary system.
//!
//! `Send` is the whole enforcement. The one hand-written exception is
//! [`alias::SendImage`], and it carries its own argument for why a
//! `CGImage` nobody else has a reference to may travel.

pub mod alias;
mod alias_watch;
pub mod bar;
pub mod cli;
pub mod components;
pub mod config;
pub mod display;
pub mod ecs;
mod extras;
pub mod handler;
#[cfg(test)]
mod harness;
pub mod ipc;
pub mod layout;
pub mod lock;
pub mod menus;
pub mod pool;
pub mod popup;
pub mod requests;
pub mod runloop;
pub mod script;
pub mod shaping;
pub mod signals;
pub mod sources;
pub mod subscribers;
pub mod text;
pub mod tracking;

pub use rsbar_protocol::style;

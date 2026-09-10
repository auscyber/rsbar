//! Bindings to the private window server API behind `SkyLight.framework`.
//!
//! [`Window`] is for a caller that wants what `AppKit` will not give a
//! borderless window: a level above the menu bar, presence on every space
//! including over a fullscreen app, and a drawing surface with no view
//! hierarchy attached — a status bar's three requirements, and all three come
//! from the window server directly. [`Foreign`] and [`sys`] are for a caller
//! that mostly wants to ask about *other* processes' windows instead — a
//! window manager's more usual shape — and need no window of their own to do
//! it.
//!
//! # Run loop
//!
//! A window server window only composites while its process pumps a run loop.
//! Drawing into one from a process that then blocks in `sleep` succeeds at
//! every call and never appears on screen. Whatever drives this crate must run
//! a `CFRunLoop`.
//!
//! None of this is in a public SDK header. Signatures here are cross-checked
//! against yabai, `SketchyBar` and `hs._asm.undocumented.spaces`, but they remain
//! claims about an ABI Apple is free to change in any release.

#![cfg(target_os = "macos")]
// Every fallible call here fails one way: the window server rejected it and
// returned a `CGError`, which the `Error` variant names. A per-method
// `# Errors` section would restate that fifteen times.
#![allow(clippy::missing_errors_doc)]

// So the macros in `skylight-macros`, which name `::skylight`, work inside this
// crate as well as outside it.
extern crate self as skylight;

pub mod ax;
pub mod callback;
pub mod cf;
mod connection;
pub mod display;
mod error;
pub mod ffi;
mod main_thread;
mod region;
mod render;
pub mod sys;
mod tags;
mod window;

pub use connection::{Connected, acquire, establish};
pub use display::{Display, is_builtin, is_main};
pub use error::{Error, Result};
pub use ffi::{ConnectionId, SpaceId, WindowId};
pub use main_thread::{MainOnly, MainThread, MainThreadProof, OnlyOnMain};

/// Registers a window server notify procedure.
///
/// `proc` is the entry point [`trampoline!`](crate::trampoline) built beside an
/// ordinary handler. The returned [`Callback`](callback::Callback) owns the
/// state; **dropping it makes the procedure inert** — there is no call that
/// takes one back, so that is as close as this API gets to deregistering, and
/// it is safe to do while a callback is running.
///
/// # Errors
///
/// The window server's error if it declines the registration, in which case the
/// state is released — nothing was installed to reach it.
pub fn register_notify<S: Send + Sync + 'static>(
    proc: extern "C-unwind" fn(
        u32,
        *mut std::ffi::c_void,
        usize,
        *mut std::ffi::c_void,
        ffi::ConnectionId,
    ),
    event: u32,
    state: std::sync::Arc<S>,
) -> Result<callback::Callback<S>> {
    callback::Callback::outliving(state, |context| {
        // SAFETY: `proc` is a trampoline built for the `S` behind `context`,
        // whose weak count the callback keeps for good.
        let status = unsafe { ffi::SLSRegisterNotifyProc(proc, event, context) };
        if status == objc2_core_graphics::CGError::Success {
            // `SLSRemoveNotifyProc` matches on the proc *and* the context, so
            // all three arguments have to be the ones that registered -- see
            // its declaration in `ffi.rs`. It answers `kCGErrorSuccess` either
            // way, so there is nothing to check: releasing the state is what
            // makes the procedure inert, and this only stops the window server
            // calling into it in the first place.
            Ok(move || {
                // SAFETY: `proc`, `event` and `context` are exactly what
                // registered, and the context is compared as an address rather
                // than dereferenced.
                unsafe { ffi::SLSRemoveNotifyProc(proc, event, context) };
            })
        } else {
            Err(Error::Notify(status))
        }
    })
}

pub use render::{SavedState, draw, draw_damaged, present, without_implicit_animations};
pub use skylight_macros::{MainThreadOnly, main, main_thread};
pub use tags::WindowTags;
pub use window::{
    Foreign, Window, batched, batched_with, capture, level, true_rect, windows_on_spaces,
};

//! Bindings to the private window server API behind `SkyLight.framework`.
//!
//! A status bar needs three things `AppKit` will not give a borderless window:
//! a level above the menu bar, presence on every space including over a
//! fullscreen app, and a drawing surface with no view hierarchy attached. All
//! three come from the window server directly.
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

mod error;
pub mod ffi;
mod region;
mod render;
mod tags;
mod window;

pub use error::{Error, Result};
pub use ffi::{ConnectionId, SpaceId, WindowId};
pub use render::{draw, present, without_implicit_animations};
pub use tags::WindowTags;
pub use window::{Window, batched, level};

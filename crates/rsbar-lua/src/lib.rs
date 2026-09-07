//! The rsbar Lua API: a host-agnostic `install()` built on an injected,
//! `async` [`Dispatcher`], plus an IPC implementation of it over the
//! daemon's Mach service.
//!
//! See the crate's `Cargo.toml` for how this compiles as both an `rlib` (for
//! the daemon's embedded host) and a `cdylib` (for `require("rsbar")`), and
//! [`dispatch`] for the seam between the two.

pub mod api;
pub mod convert;
pub mod dispatch;
pub mod error;
pub mod events;
pub mod ipc;

pub use dispatch::{BoxedEventStream, Dispatcher};
pub use error::{ApiError, Result};
pub use ipc::IpcDispatcher;

/// The loadable-module entry point: `require("rsbar")` calls `luaopen_rsbar`.
/// Only built with the `module` feature — see `Cargo.toml`. Connects to the
/// daemon named by `RSBAR_SERVICE`, or the built-in default.
#[cfg(feature = "module")]
#[mlua::lua_module]
fn rsbar(lua: &mlua::Lua) -> mlua::Result<mlua::Table> {
    api::install(lua, std::rc::Rc::new(IpcDispatcher::new()))
}

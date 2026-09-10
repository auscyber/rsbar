//! The coolabah Lua API: a host-agnostic `install()` built on an injected,
//! `async` [`Dispatcher`], plus two implementations of it — one over the
//! daemon's Mach service ([`ipc`]) and one with no IPC at all ([`direct`]),
//! for Lua the daemon is hosting itself.
//!
//! See the crate's `Cargo.toml` for how this compiles as both an `rlib` (for
//! the daemon's embedded host) and a `cdylib` (for `require("coolabah")`), and
//! [`dispatch`] for the seam all three share.

// `name!` crate-wide, so no module has to import it and no module has to
// spell the product's name. See `coolabah_protocol::naming`.
#[macro_use]
extern crate coolabah_protocol;

/// The wire vocabulary, under a name that is not the product's.
///
/// Module code says `crate::protocol::Kind`, so the crate's real name appears
/// exactly twice in this crate -- the `extern crate` above and this line --
/// and nowhere in any module. Renaming the product touches neither.
pub(crate) use ::coolabah_protocol as protocol;

pub mod api;
pub mod convert;
pub mod direct;
pub mod dispatch;
pub mod error;
pub mod events;
pub mod host;
pub mod ipc;
pub mod require;

pub use direct::{Call, Channel, DirectDispatcher};
pub use dispatch::{BoxFuture, BoxedEventStream, Dispatcher};
pub use error::{ApiError, Result};
pub use ipc::IpcDispatcher;

/// The loadable-module entry point: `require("coolabah")` calls `luaopen_coolabah`.
/// Only built with the `module` feature — see `Cargo.toml`. Connects to the
/// daemon named by `COOLABAH_SERVICE`, or the built-in default.
#[cfg(feature = "module")]
#[mlua::lua_module]
fn coolabah(lua: &mlua::Lua) -> mlua::Result<mlua::Table> {
    api::install(lua, std::sync::Arc::new(IpcDispatcher::new()))
}

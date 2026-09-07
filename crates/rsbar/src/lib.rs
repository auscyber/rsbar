//! The rsbar daemon: bar geometry, item model, drawing, and the app that
//! drives them.

pub mod bar;
pub mod display;
pub mod ecs;
pub mod handle;
pub mod item;
pub mod runloop;
pub mod script;
pub mod sources;
pub mod text;

pub use rsbar_protocol::style;

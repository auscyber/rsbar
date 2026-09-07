//! The rsbar daemon: bar geometry, the item world, drawing, and the app that
//! drives them.

pub mod bar;
pub mod components;
pub mod config;
pub mod display;
pub mod ecs;
pub mod layout;
pub mod requests;
pub mod runloop;
pub mod script;
pub mod shaping;
pub mod sources;
pub mod text;

pub use rsbar_protocol::style;

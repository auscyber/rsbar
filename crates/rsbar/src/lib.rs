//! The rsbar daemon: bar geometry, the item world, drawing, and the app that
//! drives them.

pub mod alias;
mod alias_watch;
pub mod bar;
pub mod cli;
pub mod components;
pub mod config;
pub mod display;
pub mod ecs;
#[cfg(test)]
mod harness;
pub mod layout;
pub mod requests;
pub mod runloop;
pub mod script;
pub mod shaping;
pub mod sources;
pub mod subscribers;
pub mod text;

pub use rsbar_protocol::style;

//! anvil's library: everything but the command-line and TUI front ends.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::disallowed_methods))]

pub mod apply;
mod bytes;
pub mod cancel;
pub mod config;
pub mod doctor;
pub mod document;
pub mod editor;
mod engine;
pub mod error;
#[cfg(test)]
mod examples;
mod exec;
pub mod facts;
pub mod fanout;
pub mod files;
pub mod filestore;
pub mod harden;
mod interrupt;
pub mod inventory;
pub mod logger;
pub mod options;
pub mod packages;
pub mod paths;
pub mod plan;
pub mod plugin;
pub mod prepared;
pub mod private;
mod record;
mod remote;
pub mod report;
pub mod rollback;
pub mod run;
pub mod secret;
mod services;
mod ssh;
pub mod status;
pub mod step;
pub mod tasks;
pub mod template;
pub mod term;
#[cfg(any(test, feature = "test-util"))]
#[cfg_attr(feature = "test-util", allow(clippy::disallowed_methods))]
pub mod test_util;
pub mod transport;
mod trust;
pub mod update;

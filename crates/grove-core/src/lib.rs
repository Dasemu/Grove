//! Sync engine core: git plumbing wrapper, config, snapshots and state classification.

pub mod busy;
pub mod config;
pub mod git;
pub mod inspect;
pub mod layout;
pub mod migrate;
pub mod receive;
pub mod registry;
pub mod resolve;
pub mod snapshot;
pub mod state;
pub mod sync;

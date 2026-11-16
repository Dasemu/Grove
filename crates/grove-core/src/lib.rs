//! Sync engine core: git plumbing wrapper, config, snapshots and state classification.

#[cfg(test)]
#[path = "../../../test-support/tmp.rs"]
mod test_tmp;

pub mod busy;
pub mod config;
pub mod git;
pub mod inspect;
pub mod layout;
pub mod migrate;
pub mod prune;
pub mod receive;
pub mod registry;
pub mod resolve;
pub mod snapshot;
pub mod state;
pub mod sync;

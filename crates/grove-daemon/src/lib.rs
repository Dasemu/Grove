//! grove user daemon: watches managed repos, debounces changes, runs sync passes on a schedule
//! and serves a small JSON protocol over a Unix socket. All durable state lives in git; the
//! daemon itself holds nothing that a crash could lose.

pub mod activity;
pub mod daemon;
pub mod discover;
pub mod ipc;

pub use activity::{Activity, Event};
pub use daemon::{Daemon, Settings};

//! Application-level tokio tasks and shared state.
//!
//! Bodies of long-running tasks spawned from `main` (imbe forwarder,
//! vocoder OS thread, grant follower, dibit readers, ...) live here
//! so `main.rs` stays focused on boot orchestration.

#[cfg(target_os = "linux")]
pub mod autoppm;
pub mod call_tracker;
#[cfg(target_os = "linux")]
pub mod dibit_readers;
#[cfg(target_os = "linux")]
pub mod follower;
pub mod grant_stats;
pub mod imbe_forwarder;
pub mod vocoder_task;

//! Application-level tokio tasks and shared state.
//!
//! Bodies of long-running tasks spawned from `main` (imbe forwarder,
//! vocoder OS thread, grant follower, dibit readers, ...) live here
//! so `main.rs` stays focused on boot orchestration.

#[cfg(any(target_os = "linux", test))]
pub mod autoppm;
pub mod audio_pacer;
pub mod call_counters;
pub mod dibit_airtime;
#[cfg(target_os = "linux")]
pub mod dibit_readers;
#[cfg(target_os = "linux")]
pub mod forensics;
pub mod grant_follower;
pub mod grant_stats;
pub mod imbe_forwarder;
pub mod seed_snapshot;
#[cfg(target_os = "linux")]
pub mod sw_demod_task;
pub mod ui_state;
pub mod vocoder_task;
#[cfg(target_os = "linux")]
pub mod wideband_iq_task;

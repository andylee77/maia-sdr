//! Application-level tokio tasks and shared state.
//!
//! Bodies of long-running tasks spawned from `main` (imbe forwarder,
//! vocoder OS thread, grant follower, dibit readers, ...) live here
//! so `main.rs` stays focused on boot orchestration.

#[cfg(target_os = "linux")]
pub mod autoppm;
pub mod audio_pacer;
#[cfg(target_os = "linux")]
pub mod dibit_readers;
pub mod grant_follower;
pub mod grant_stats;
pub mod imbe_forwarder;
#[cfg(target_os = "linux")]
pub mod sw_demod_task;
pub mod vocoder_task;
#[cfg(target_os = "linux")]
pub mod wideband_iq_task;

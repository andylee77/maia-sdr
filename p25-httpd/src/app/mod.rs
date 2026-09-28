//! Application-level tokio tasks and shared state.
//!
//! Bodies of long-running tasks spawned from `main` (imbe forwarder,
//! vocoder OS thread, grant follower, dibit readers, ...) live here
//! so `main.rs` stays focused on boot orchestration.

#[cfg(any(target_os = "linux", test))]
pub mod autoppm;
pub mod audio_pacer;
pub mod call_counters;
/// Change 071b: software C4FM on the control channel; LSM / C4FM choice.
pub mod c4fm_task;
/// Change 067: board clock from the site / NTP / by hand.
pub mod clock_task;
pub mod dibit_airtime;
/// Change 071: find local systems (band sweep, control-channel probe).
pub mod discovery;
#[cfg(target_os = "linux")]
pub mod dibit_readers;
#[cfg(target_os = "linux")]
pub mod forensics;
pub mod grant_follower;
pub mod grant_stats;
/// Change 072: fills the activity history.
pub mod history_task;
/// Change 066: per-chain objects of a traffic chain.
pub mod traffic_lane;
/// Change 066: traffic LSM heartbeat, one task per chain.
#[cfg(target_os = "linux")]
pub mod traffic_heartbeat;
pub mod traffic_pll_watchdog;
pub mod imbe_forwarder;
/// Change 071b: one reader per DDC IQ ring, fanned out.
pub mod iq_hub;
/// Change 066: which traffic chain follows a grant.
pub mod lane_policy;
/// Change 070: keeps the receive window on the site's channels.
pub mod recentre_task;
pub mod seed_snapshot;
#[cfg(target_os = "linux")]
pub mod sw_demod_task;
pub mod ui_state;
pub mod vocoder_task;
#[cfg(target_os = "linux")]
pub mod wideband_iq_task;

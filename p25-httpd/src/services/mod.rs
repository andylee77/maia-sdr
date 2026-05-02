//! Cross-cutting services shared by the protocol, audio, and HTTP
//! layers — structured event log, monitor/TG allow-list, NTP sync,
//! FFT spectrum snapshot, per-site baselines.

pub mod event_log;
pub mod monitor;
pub mod ntp;
pub mod sites;
pub mod spectrum;
pub mod sync_trace;

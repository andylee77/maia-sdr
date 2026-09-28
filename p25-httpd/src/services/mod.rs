//! Cross-cutting services shared by the protocol, audio, and HTTP
//! layers — structured event log, monitor/TG allow-list, NTP sync,
//! FFT spectrum snapshot, per-site baselines, persisted web-UI settings.

pub mod event_log;
pub mod monitor;
pub mod ntp;
/// Change 067: site time from the control channel.
pub mod site_clock;
pub mod sites;
pub mod spectrum;
pub mod sync_trace;
pub mod ui_settings;

//! Cross-cutting services shared by the protocol, audio, and HTTP
//! layers — structured event log, monitor/TG allow-list, NTP sync,
//! FFT spectrum snapshot.

pub mod event_log;
pub mod monitor;
pub mod ntp;
pub mod spectrum;

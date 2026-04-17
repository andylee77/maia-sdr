//! REST + WebSocket API handlers, organised by consumer-facing domain.
//!
//! Each submodule maps to a logical grouping of endpoints that an
//! Android-app (or diagnostic-tool) consumer would reach for together:
//!
//!   - `chain` — HDL chain internals: dibit dumps, IQ captures, LSM control bits, NID ring.
//!   - `debug` — Visual diagnostics: spectrum, constellation.
//!   - `history` — Time-series data: event log, recordings, TSBK history.
//!   - `radio` — Live radio state: stats, grants, bands, decoder chains.
//!   - `system` — System identity + health + endpoint self-describe.
//!   - `talkgroups` — Per-talkgroup metadata: aliases, monitor list, encryption, grant map.
//!   - `traffic` — Current-call view: traffic chain, IMBE, vocoded audio.
//!   - `tuning` — Runtime knobs: retune, gain, modulation, BCH/sync thresholds, decoder reset.
//!   - `ws` — WebSocket streams: /ws/events and /ws/audio.

pub mod chain;
pub mod debug;
pub mod history;
pub mod radio;
pub mod system;
pub mod talkgroups;
pub mod traffic;
pub mod tuning;
pub mod ws;

//! REST + WebSocket API handlers, organised by consumer-facing domain.
//!
//! Each submodule maps to a logical grouping of endpoints that an
//! Android-app (or diagnostic-tool) consumer would reach for together.
//! Grouping is intentionally consumer-facing, not implementation-facing
//! — a screen in the Android app would consume exactly one submodule:
//!
//!   - [`chain`] — HDL chain internals: dibit dumps, IQ captures, LSM control bits, NID ring.
//!   - [`debug`] — Visual diagnostics: spectrum, constellation.
//!   - [`history`] — Time-series data: event log, recordings, TSBK history.
//!   - [`radio`] — Live radio state: stats, grants, bands, decoder chains.
//!   - [`system`] — System identity + health + endpoint self-describe. Holds `ENDPOINT_CATALOGUE`.
//!   - [`talkgroups`] — Per-talkgroup metadata: aliases, monitor list, encryption, grant map.
//!   - [`traffic`] — Current-call view: traffic chain, IMBE, vocoded audio.
//!   - [`tuning`] — Runtime knobs: retune, gain, modulation, BCH/sync thresholds, decoder reset.
//!   - [`ws`] — WebSocket streams: /ws/events and /ws/audio.
//!
//! ## Adding a handler
//!
//! 1. Pick the submodule whose consumer-facing category best fits. If
//!    it straddles two, pick the one matching the primary user-visible
//!    screen (e.g. audio endpoints live in `traffic` because they're
//!    bound to the current call, not in `history` or `ws`).
//! 2. Use `pub async fn ...` so `router()` in the parent module can
//!    reference it as `api::<module>::<handler>`.
//! 3. Add an `ENDPOINT_CATALOGUE` entry in `api::system` in the **same
//!    commit** — the catalogue is the runtime self-describe feed and
//!    the spec Android + diagnostic tools consume.
//! 4. If the response is structured, add a typed struct to `p25-json`
//!    and return `Json<YourStruct>`. Reserve `Json<serde_json::Value>`
//!    for genuinely dynamic shapes.
//!
//! See [`doc/API_CONSUMERS.md`](../../../../doc/API_CONSUMERS.md) for
//! the broader consumer contract.
//!
//! ## Preludes
//!
//! Every submodule includes a standard prelude (AppState, p25_json,
//! axum primitives). Some imports will be unused per module; that's
//! intentional — the prelude is a template, and `#[allow(unused_imports)]`
//! keeps per-module churn low.

pub mod chain;
pub mod debug;
pub mod history;
pub mod radio;
pub mod system;
pub mod talkgroups;
pub mod traffic;
pub mod tuning;
pub mod ws;

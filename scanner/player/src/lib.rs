//! ATSC TV in the browser: an ATSC transport stream's programme decoded to pictures and sound,
//! in Rust compiled to WebAssembly for the scanner's Viewer page.
//!
//! - `ts`: the transport stream (ITU-T H.222.0): packets, the PAT and PMT, PES packets and their
//!   timestamps.
//! - `ac3`: AC-3 audio (ATSC A/52).
//! - `bits`: the bit reader both use.
//! - `player`: a programme played, its streams through their decoders.
//! - `wasm` (WebAssembly only): the functions the page calls.
//!
//! Written from the standards; FFmpeg's decode of the same streams is the tests' reference.

// The decoders index their arrays as the standards' pseudo-code does.
#![allow(clippy::needless_range_loop)]

pub mod ac3;
pub mod bits;
pub mod mpeg2;
pub mod player;
pub mod ts;
#[cfg(target_arch = "wasm32")]
mod wasm;

//! Services around the radio: configuration, the event log, the call history and recordings,
//! packet data, the scan that finds systems, the board clock, the crystal correction, the
//! board's health, the unit's mode with ATSC TV's channel finder, and the radio core's burst
//! timing check.

pub mod access;
pub mod atsc;
pub mod burst;
pub mod clock;
pub mod config;
pub mod crystal;
pub mod discovery;
pub mod events;
pub mod files;
pub mod history;
pub mod iq;
pub mod mode;
pub mod notices;
pub mod packet_data;
pub mod recordings;
pub mod system;
pub mod tls;

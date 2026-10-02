//! Services around the radio: configuration, the event log, the call history and recordings,
//! packet data, the scan that finds systems, the board clock, the crystal correction and the
//! board's health.

pub mod clock;
pub mod config;
pub mod crystal;
pub mod discovery;
pub mod events;
pub mod history;
pub mod iq;
pub mod notices;
pub mod packet_data;
pub mod recordings;
pub mod system;

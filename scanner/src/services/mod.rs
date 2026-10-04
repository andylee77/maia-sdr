//! Services around the radio: configuration, the event log, the call history and recordings,
//! packet data, the scan that finds systems, the board clock, the crystal correction, the
//! board's health, and the unit's mode with ATSC TV's channel finder.

pub mod atsc;
pub mod clock;
pub mod config;
pub mod crystal;
pub mod discovery;
pub mod events;
pub mod history;
pub mod iq;
pub mod mode;
pub mod notices;
pub mod packet_data;
pub mod recordings;
pub mod system;

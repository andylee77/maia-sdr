//! Services around the radio: configuration, the event log, the call history and recordings,
//! packet data, the scan that finds systems, the board clock, the crystal correction, the
//! board's health, the unit's mode with ATSC TV's channel finder, data mode's listening, the
//! analog receiver, the aircraft module's listening, and the radio core's burst timing check.

pub mod access;
pub mod adsb;
pub mod analog;
pub mod atsc;
pub mod burst;
pub mod clock;
pub mod config;
pub mod crystal;
pub mod data;
pub mod discovery;
pub mod events;
pub mod files;
pub mod history;
pub mod iiod;
pub mod iq;
pub mod mode;
pub mod notices;
pub mod packet_data;
pub mod recordings;
pub mod system;
pub mod tls;

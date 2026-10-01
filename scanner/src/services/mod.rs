//! Services around the radio: configuration, the event log, the call history and recordings, the
//! scan that finds systems, the board clock and the crystal correction.

pub mod clock;
pub mod config;
pub mod crystal;
pub mod discovery;
pub mod events;
pub mod history;
pub mod notices;
pub mod recordings;

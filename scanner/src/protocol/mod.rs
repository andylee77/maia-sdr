//! The protocols, each behind its decoders: P25 Phase 1 and DMR Tier III; ATSC TV, found in the
//! spectrum; the data bands' bursts (`ism`); the analog receivers (`analog`); and what aircraft
//! send of themselves (`adsb`).

pub mod adsb;
pub mod analog;
pub mod atsc;
pub mod dmr;
pub mod events;
pub mod fec;
pub mod ism;
pub mod p25;

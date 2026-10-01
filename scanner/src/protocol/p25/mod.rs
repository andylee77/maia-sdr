//! P25 Phase 1: error correction, trunking signalling blocks (TSBK), packet data units, voice
//! frames and the wire format. Ported from SDRTrunk's `module/decode/p25/phase1`.

pub mod control;
pub mod fec;
pub mod framer;
pub mod pdu;
pub mod tsbk;
pub mod types;
pub mod voice_frame;
pub mod wire;

#[cfg(test)]
pub(crate) mod test_fixtures;

//! P25 Phase 1 framer: finds frame sync in the dibit stream, reads and corrects the network
//! identifier (NID), and assembles each data unit. Trunking signalling (TSDU) and packet data
//! (PDU) are split into blocks here, because their length is only known block by block
//! (SDRTrunk `P25P1MessageFramer`).

use std::collections::HashMap;

use serde::Serialize;

use super::fec::bch;
use super::fec::{TrellisDecoder, TsduDeinterleaver};
use super::pdu::{self, PduBlock, PduHeader};
use super::tsbk::{CrcConvention, TsbkBlock, TsbkMessage};
use super::types::DataUnit;
use super::wire::{FRAME_SYNC_MASK, FRAME_SYNC_PATTERN, NID_STATUS_DIBIT_INDEX, NID_TRANSMITTED_DIBITS};

/// Hamming distance (bits of 48) at which the sync correlator fires.
pub const SYNC_THRESHOLD: u32 = 6;

/// A NID whose 32 dibits are more than this many of one value is noise: BCH would "correct" it
/// into a valid-looking codeword.
const DIBIT_DOMINANCE_LIMIT: u32 = 24;

#[derive(Debug, Clone, Copy)]
pub struct FramerConfig {
    pub sync_threshold: u32,
    /// NIDs that BCH corrected with more bit errors than this are rejected.
    pub bch_max_errors: u32,
}

impl Default for FramerConfig {
    fn default() -> Self {
        FramerConfig { sync_threshold: SYNC_THRESHOLD, bch_max_errors: bch::T_MAX_ERRORS }
    }
}

/// An accepted NID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nid {
    pub nac: u16,
    pub duid: DataUnit,
    pub bch_errors: u8,
}

/// What the framer hands on. Voice and terminator bodies are the raw dibits after the NID,
/// status dibits in place.
#[derive(Debug)]
pub enum Framed<'a> {
    /// Every accepted NID, before its data unit.
    Nid(Nid),
    /// A trunking signalling block that passed its CRC; `index` 0..=2 within the TSDU.
    Tsbk { index: u8, opcode: u8, message: TsbkMessage },
    Hdu(&'a [u8]),
    Ldu1(&'a [u8]),
    Ldu2(&'a [u8]),
    Tdu,
    TduLc(&'a [u8]),
    Pdu { header: PduHeader, blocks: Vec<PduBlock>, expected: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Hunting,
    ReadingNid { read: usize, bits: u64 },
    ReadingDataUnit { duid: DataUnit },
}

/// Counters for the diagnostics pages.
#[derive(Debug, Clone, Serialize)]
pub struct FramerStats {
    pub dibits: u64,
    pub dibit_hist: [u64; 4],
    pub sync_hits: u64,
    /// Correlator distance per dibit while hunting (bucket 24 = 24 or more).
    pub sync_distance_hist: [u64; 25],
    pub nid_bch_failures: u64,
    pub nid_entropy_rejected: u64,
    pub nid_invalid_duid: u64,
    pub nid_nac_mismatch: u64,
    pub nid_ok: u64,
    pub nac_relocks: u64,
    /// On-air DUID nibble of the NIDs BCH accepted.
    pub raw_duid_hist: [u64; 16],
    pub tsdus: u64,
    pub tsbk_attempts_by_pos: [u64; 3],
    pub tsbk_ok_by_pos: [u64; 3],
    pub tsbk_trellis_failures: u64,
    pub tsbk_crc_failures: u64,
    pub tsbk_crc_plain: u64,
    pub tsbk_crc_xored: u64,
    pub tsbk_unknown_opcode: u64,
    #[serde(serialize_with = "crate::util::ser::array")]
    pub tsbk_opcodes_ok: [u64; 64],
    #[serde(serialize_with = "crate::util::ser::array")]
    pub tsbk_opcodes_failed: [u64; 64],
    /// Standard, Motorola (0x90), Harris (0xA4), other.
    pub tsbk_mfid_ok: [u64; 4],
    pub hdus: u64,
    pub ldu1s: u64,
    pub ldu2s: u64,
    pub tdus: u64,
    pub tdu_lcs: u64,
    pub pdus: u64,
    pub pdu_header_failures: u64,
    pub pdu_blocks: u64,
}

impl Default for FramerStats {
    fn default() -> Self {
        FramerStats {
            dibits: 0,
            dibit_hist: [0; 4],
            sync_hits: 0,
            sync_distance_hist: [0; 25],
            nid_bch_failures: 0,
            nid_entropy_rejected: 0,
            nid_invalid_duid: 0,
            nid_nac_mismatch: 0,
            nid_ok: 0,
            nac_relocks: 0,
            raw_duid_hist: [0; 16],
            tsdus: 0,
            tsbk_attempts_by_pos: [0; 3],
            tsbk_ok_by_pos: [0; 3],
            tsbk_trellis_failures: 0,
            tsbk_crc_failures: 0,
            tsbk_crc_plain: 0,
            tsbk_crc_xored: 0,
            tsbk_unknown_opcode: 0,
            tsbk_opcodes_ok: [0; 64],
            tsbk_opcodes_failed: [0; 64],
            tsbk_mfid_ok: [0; 4],
            hdus: 0,
            ldu1s: 0,
            ldu2s: 0,
            tdus: 0,
            tdu_lcs: 0,
            pdus: 0,
            pdu_header_failures: 0,
            pdu_blocks: 0,
        }
    }
}

impl FramerStats {
    pub fn tsbk_attempts(&self) -> u64 {
        self.tsbk_attempts_by_pos.iter().sum()
    }

    pub fn tsbk_ok(&self) -> u64 {
        self.tsbk_crc_plain + self.tsbk_crc_xored
    }
}

/// Locks onto the system's NAC (SDRTrunk `NACTracker`): the most-observed NAC with at least
/// `MIN_OBSERVATIONS`. NIDs of another NAC are then dropped, unless that NAC arrives
/// `RELOCK_AFTER` times in a row (the channel now carries another system).
#[derive(Debug, Default)]
pub struct NacTracker {
    /// NAC → (observations, sequence of the last one).
    entries: HashMap<u16, (u32, u64)>,
    seq: u64,
    /// (NAC, run length) of consecutive NIDs that disagree with the lock.
    other_run: (u16, u32),
}

impl NacTracker {
    const MAX_TRACKED: usize = 3;
    const MIN_OBSERVATIONS: u32 = 3;
    pub const RELOCK_AFTER: u32 = 8;

    /// A valid NID whose NAC is not the locked one. True when the lock moves to `nac` (the
    /// tracker is then cleared and counts `nac` from scratch).
    pub fn other_nac(&mut self, nac: u16) -> bool {
        self.other_run = if self.other_run.0 == nac { (nac, self.other_run.1 + 1) } else { (nac, 1) };
        if self.other_run.1 >= Self::RELOCK_AFTER {
            self.reset();
            return true;
        }
        false
    }

    pub fn track(&mut self, nac: u16) {
        self.other_run = (0, 0);
        self.seq = self.seq.wrapping_add(1);
        if let Some(entry) = self.entries.get_mut(&nac) {
            entry.0 = entry.0.saturating_add(1);
            entry.1 = self.seq;
            return;
        }
        self.entries.insert(nac, (1, self.seq));
        if self.entries.len() > Self::MAX_TRACKED {
            if let Some(oldest) = self.entries.iter().min_by_key(|(_, (_, t))| *t).map(|(k, _)| *k) {
                self.entries.remove(&oldest);
            }
        }
    }

    /// The locked NAC, or 0.
    pub fn dominant(&self) -> u16 {
        self.entries
            .iter()
            .filter(|(_, (count, _))| *count >= Self::MIN_OBSERVATIONS)
            .max_by_key(|(_, (count, _))| *count)
            .map(|(nac, _)| *nac)
            .unwrap_or(0)
    }

    pub fn reset(&mut self) {
        self.entries.clear();
        self.seq = 0;
        self.other_run = (0, 0);
    }
}

pub struct Framer {
    config: FramerConfig,
    state: State,
    sync_register: u64,
    /// Dibits shifted in since hunting began; the correlator needs a full pattern.
    hunted: usize,
    buffer: Vec<u8>,
    expected_len: usize,
    tsbk_blocks: usize,
    pdu_header: Option<PduHeader>,
    nac: NacTracker,
    pub stats: FramerStats,
}

impl Default for Framer {
    fn default() -> Self {
        Framer::new(FramerConfig::default())
    }
}

impl Framer {
    pub fn new(config: FramerConfig) -> Self {
        Framer {
            config,
            state: State::Hunting,
            sync_register: 0,
            hunted: 0,
            buffer: Vec::with_capacity(1024),
            expected_len: 0,
            tsbk_blocks: 0,
            pdu_header: None,
            nac: NacTracker::default(),
            stats: FramerStats::default(),
        }
    }

    /// The channel changed: drop the frame in progress and the NAC lock.
    pub fn reset(&mut self) {
        self.hunt();
        self.sync_register = 0;
        self.nac.reset();
    }

    /// The locked NAC, or 0.
    #[cfg(test)]
    pub fn locked_nac(&self) -> u16 {
        self.nac.dominant()
    }

    /// A demodulator that finds frame sync itself (soft sync on the C4FM samples) reports it
    /// right after the last sync dibit; the NID follows. Ignored while a unit is being read.
    pub fn sync_detected(&mut self) {
        if self.state == State::Hunting {
            self.stats.sync_hits += 1;
            self.state = State::ReadingNid { read: 0, bits: 0 };
        }
    }

    /// A NID or data unit is being read (SDRTrunk `isAssembling`).
    pub fn is_assembling(&self) -> bool {
        self.state != State::Hunting
    }

    pub fn push(&mut self, dibit: u8, sink: &mut impl FnMut(Framed<'_>)) {
        let dibit = dibit & 0x03;
        self.stats.dibits += 1;
        self.stats.dibit_hist[dibit as usize] += 1;
        match self.state {
            State::Hunting => self.hunt_dibit(dibit),
            State::ReadingNid { read, bits } => {
                let bits = if read == NID_STATUS_DIBIT_INDEX { bits } else { (bits << 2) | dibit as u64 };
                let read = read + 1;
                if read < NID_TRANSMITTED_DIBITS {
                    self.state = State::ReadingNid { read, bits };
                } else {
                    self.nid_complete(bits, sink);
                }
            }
            State::ReadingDataUnit { duid } => {
                self.buffer.push(dibit);
                if self.buffer.len() >= self.expected_len && self.unit_complete(duid, sink) {
                    self.hunt();
                }
            }
        }
    }

    fn hunt(&mut self) {
        self.state = State::Hunting;
        self.hunted = 0;
        self.buffer.clear();
        self.expected_len = 0;
        self.tsbk_blocks = 0;
        self.pdu_header = None;
    }

    fn hunt_dibit(&mut self, dibit: u8) {
        self.sync_register = ((self.sync_register << 2) | dibit as u64) & FRAME_SYNC_MASK;
        self.hunted += 1;
        if self.hunted < 24 {
            return;
        }
        let distance = (self.sync_register ^ FRAME_SYNC_PATTERN).count_ones();
        self.stats.sync_distance_hist[(distance as usize).min(24)] += 1;
        if distance <= self.config.sync_threshold {
            self.stats.sync_hits += 1;
            self.state = State::ReadingNid { read: 0, bits: 0 };
        }
    }

    fn nid_complete(&mut self, bits: u64, sink: &mut impl FnMut(Framed<'_>)) {
        let on_air_duid = ((bits >> 48) & 0xF) as usize;
        let mut hist = [0u32; 4];
        for i in 0..32 {
            hist[((bits >> (i * 2)) & 0x3) as usize] += 1;
        }
        if hist.iter().copied().max().unwrap_or(0) > DIBIT_DOMINANCE_LIMIT {
            self.stats.nid_entropy_rejected += 1;
            self.hunt();
            return;
        }
        let Some(decoded) = bch::decode_nid(bits).filter(|d| u32::from(d.n_errors) <= self.config.bch_max_errors) else {
            self.stats.nid_bch_failures += 1;
            self.hunt();
            return;
        };
        self.stats.raw_duid_hist[on_air_duid] += 1;
        let duid = DataUnit::from_duid(decoded.duid);
        let nac = decoded.nac;
        let expected = self.nac.dominant();
        if expected != 0 && nac != expected {
            if duid.is_some() && self.nac.other_nac(nac) {
                self.stats.nac_relocks += 1;
            } else {
                self.stats.nid_nac_mismatch += 1;
                self.hunt();
                return;
            }
        }
        let Some(duid) = duid else {
            self.stats.nid_invalid_duid += 1;
            self.hunt();
            return;
        };
        self.nac.track(nac);
        self.stats.nid_ok += 1;
        sink(Framed::Nid(Nid { nac, duid, bch_errors: decoded.n_errors }));
        self.hunt();
        self.expected_len = duid.length_dibits();
        self.state = State::ReadingDataUnit { duid };
    }

    /// True when the data unit is done; false when it goes on (another TSBK or the PDU's data
    /// blocks).
    fn unit_complete(&mut self, duid: DataUnit, sink: &mut impl FnMut(Framed<'_>)) -> bool {
        match duid {
            DataUnit::Tsdu => self.tsdu_block(sink),
            DataUnit::Pdu => self.pdu_step(sink),
            DataUnit::Hdu => {
                self.stats.hdus += 1;
                sink(Framed::Hdu(&self.buffer));
                true
            }
            DataUnit::Ldu1 => {
                self.stats.ldu1s += 1;
                sink(Framed::Ldu1(&self.buffer));
                true
            }
            DataUnit::Ldu2 => {
                self.stats.ldu2s += 1;
                sink(Framed::Ldu2(&self.buffer));
                true
            }
            DataUnit::Tdu => {
                self.stats.tdus += 1;
                sink(Framed::Tdu);
                true
            }
            DataUnit::TduLc => {
                self.stats.tdu_lcs += 1;
                sink(Framed::TduLc(&self.buffer));
                true
            }
        }
    }

    /// The next TSBK of the TSDU. A block that fails the trellis or its CRC does not end the
    /// TSDU: reading goes on to the next block, as SDRTrunk's `dispatchTSBK` does. Only a
    /// clean block with the last-block flag, or the third block, ends it.
    fn tsdu_block(&mut self, sink: &mut impl FnMut(Framed<'_>)) -> bool {
        let index = self.tsbk_blocks;
        if index == 0 {
            self.stats.tsdus += 1;
        }
        let data = TsduDeinterleaver::deinterleave_multi(&self.buffer, index + 1);
        let start = index * TsduDeinterleaver::TRELLIS_DATA_DIBITS;
        let mut clean_last = false;
        if let Some(block_dibits) = data.get(start..start + TsduDeinterleaver::TRELLIS_DATA_DIBITS) {
            self.stats.tsbk_attempts_by_pos[index] += 1;
            match TrellisDecoder::decode(block_dibits) {
                None => self.stats.tsbk_trellis_failures += 1,
                Some(bytes) => {
                    let block = TsbkBlock::parse(&bytes);
                    let opcode = bytes[0] & 0x3F;
                    match block.crc_valid(&bytes) {
                        None => {
                            self.stats.tsbk_crc_failures += 1;
                            self.stats.tsbk_opcodes_failed[opcode as usize] += 1;
                        }
                        Some(convention) => {
                            match convention {
                                CrcConvention::Plain => self.stats.tsbk_crc_plain += 1,
                                CrcConvention::Xored => self.stats.tsbk_crc_xored += 1,
                            }
                            self.stats.tsbk_ok_by_pos[index] += 1;
                            self.stats.tsbk_opcodes_ok[opcode as usize] += 1;
                            self.stats.tsbk_mfid_ok[match block.manufacturer {
                                0x00 => 0,
                                0x90 => 1,
                                0xA4 => 2,
                                _ => 3,
                            }] += 1;
                            clean_last = block.last_block;
                            match block.decode() {
                                Some(message) => sink(Framed::Tsbk { index: index as u8, opcode, message }),
                                None => self.stats.tsbk_unknown_opcode += 1,
                            }
                        }
                    }
                }
            }
        }
        self.tsbk_blocks += 1;
        if clean_last || self.tsbk_blocks >= TsduDeinterleaver::MAX_BLOCKS {
            return true;
        }
        match TsduDeinterleaver::body_dibits_for_blocks(self.tsbk_blocks + 1) {
            Some(len) => {
                self.expected_len = len;
                false
            }
            None => true,
        }
    }

    /// A PDU in two steps: the header block, then (reading on) the data blocks it announces.
    fn pdu_step(&mut self, sink: &mut impl FnMut(Framed<'_>)) -> bool {
        let data = pdu::strip_status(&self.buffer);
        let Some(header) = self.pdu_header.take() else {
            self.stats.pdus += 1;
            let Some(header) = data.get(..pdu::BLOCK_DIBITS).and_then(PduHeader::decode).filter(|h| h.crc_ok) else {
                self.stats.pdu_header_failures += 1;
                return true;
            };
            let n = (header.blocks_to_follow as usize).min(pdu::MAX_BLOCKS);
            if n == 0 {
                sink(Framed::Pdu { header, blocks: Vec::new(), expected: 0 });
                return true;
            }
            self.expected_len = pdu::raw_len_for_data(pdu::BLOCK_DIBITS * (1 + n));
            self.pdu_header = Some(header);
            return false;
        };
        let n = (header.blocks_to_follow as usize).min(pdu::MAX_BLOCKS);
        let confirmed = header.confirmed_blocks();
        let blocks: Vec<PduBlock> = (1..=n)
            .filter_map(|k| {
                let s = k * pdu::BLOCK_DIBITS;
                data.get(s..s + pdu::BLOCK_DIBITS).and_then(|d| pdu::decode_block(d, confirmed))
            })
            .collect();
        self.stats.pdu_blocks += blocks.len() as u64;
        sink(Framed::Pdu { header, blocks, expected: n });
        true
    }
}

#[cfg(test)]
#[path = "framer_tests.rs"]
mod tests;

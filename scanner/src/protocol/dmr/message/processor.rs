//! Framer events to messages: port of SDRTrunk `DMRMessageProcessor` with
//! `SLCAssembler`, `FLCAssembler`, `MBCAssembler` and the frame F part of
//! `VoiceSuperFrameProcessor`.
//!
//! Left out: audio, talker alias assembly, packet sequence assembly and the
//! encryption IV of `VoiceSuperFrameProcessor` (clear voice only).

use std::collections::HashMap;

use super::crc_mask::DmrCrcMaskManager;
use super::csbk::{self, Csbk, CsbkKind};
use super::data::DataBurst;
use super::lc::{self, FullLc, ShortLc};
use super::voice::ShortBurst;
use super::DmrMessage;
use crate::protocol::dmr::fec::{bptc, crc};
use crate::protocol::dmr::framer::FramerEvent;
use crate::protocol::dmr::sync::DmrSyncPattern;

/// Link control start/stop values (SDRTrunk `LCSS` ordinals).
const SINGLE_FRAGMENT: u8 = 0;
const FIRST_FRAGMENT: u8 = 1;
const LAST_FRAGMENT: u8 = 2;
const CONTINUATION_FRAGMENT: u8 = 3;

/// Turns framer events into messages in SDRTrunk's dispatch order. Ports `DMRMessageProcessor`.
pub struct DmrMessageProcessor {
    slc: SlcAssembler,
    /// Embedded LC assemblers for timeslots 1 and 2.
    flc: [FlcAssembler; 2],
    mbc: MbcAssembler,
    masks: DmrCrcMaskManager,
    /// LCN -> downlink frequency (Hz), for grants and clears.
    lcn_frequencies: HashMap<u16, u64>,
    timestamp_ms: u64,
}

impl DmrMessageProcessor {
    /// `lcn_frequencies` maps logical channel numbers to downlink Hz (SDRTrunk's timeslot map).
    pub fn new(lcn_frequencies: HashMap<u16, u64>) -> Self {
        DmrMessageProcessor {
            slc: SlcAssembler::default(),
            flc: [FlcAssembler::new(1), FlcAssembler::new(2)],
            mbc: MbcAssembler::default(),
            masks: DmrCrcMaskManager::default(),
            lcn_frequencies,
            timestamp_ms: 0,
        }
    }

    /// Decodes one framer event into zero or more messages, in dispatch order.
    pub fn process(&mut self, event: FramerEvent) -> Vec<DmrMessage> {
        let mut out = Vec::new();
        match event {
            FramerEvent::Burst(burst) => {
                self.timestamp_ms = burst.dibit_index * 1000 / 4800;
                if let Some(message) =
                    DmrMessage::from_burst(&burst, self.timestamp_ms, &mut self.masks)
                {
                    self.receive(message, &mut out);
                }
            }
            FramerEvent::SyncLoss { timeslot, bits } => {
                let message = DmrMessage::SyncLoss {
                    timestamp_ms: self.timestamp_ms,
                    timeslot,
                    bits,
                };
                self.receive(message, &mut out);
            }
        }
        out
    }

    /// Ports `DMRMessageProcessor.receive()`: checks, enriches and dispatches a
    /// message, then feeds the assemblers and dispatches what they complete.
    fn receive(&mut self, mut message: DmrMessage, out: &mut Vec<DmrMessage>) {
        if let DmrMessage::SyncLoss { .. } = message {
            self.slc.reset();
            for flc in &mut self.flc {
                flc.reset();
            }
        }

        // A CSBK that failed its CRC may use an alternate mask seen before (RAS).
        if let DmrMessage::Csbk(csbk) = &mut message {
            if !csbk.burst.valid {
                let residual = u32::from(crc::calculate_residual(&csbk.burst.bits, 0));
                csbk.burst.valid = self.masks.is_valid_csbk(
                    csbk.opcode().value(),
                    residual,
                    csbk.burst.timestamp_ms,
                );
            }
        }

        // Voice superframe: burst F's single-burst signalling (base station only).
        if let DmrMessage::Voice(voice) = &mut message {
            if voice.pattern == DmrSyncPattern::BsVoiceFrameF && voice.emb.is_some() {
                voice.embedded = Some(ShortBurst::extract(voice.flc_fragment()));
            }
        }

        self.enrich(&mut message);

        // What the assemblers make of this message goes out after it.
        let mut follow_ups: Vec<DmrMessage> = Vec::new();
        match &message {
            DmrMessage::Voice(voice) if voice.emb.is_some() => {
                let emb = voice.emb.unwrap();
                let flc = &mut self.flc[if voice.timeslot == 1 { 0 } else { 1 }];
                if let Some(lc) = flc.process(
                    emb.lcss,
                    voice.flc_fragment(),
                    voice.timestamp_ms,
                    &mut self.masks,
                ) {
                    follow_ups.push(DmrMessage::FullLc(lc));
                }
                if voice.pattern.has_cach() {
                    let cach = voice.cach;
                    if let Some(slc) =
                        self.slc
                            .process(cach.lcss, &cach.payload, voice.timestamp_ms)
                    {
                        follow_ups.push(DmrMessage::ShortLc(slc));
                    }
                }
            }
            burst if burst.is_burst() => {
                let (pattern, cach) = match burst {
                    DmrMessage::Voice(voice) => (voice.pattern, Some(voice.cach)),
                    DmrMessage::UnknownBurst { pattern, .. } => (*pattern, None),
                    other => {
                        let data = other.data_burst().unwrap();
                        (data.pattern, Some(data.cach))
                    }
                };
                if let (true, Some(cach)) = (pattern.has_cach(), cach) {
                    if let Some(slc) =
                        self.slc
                            .process(cach.lcss, &cach.payload, burst.timestamp_ms())
                    {
                        follow_ups.push(DmrMessage::ShortLc(slc));
                    }
                }
                match burst {
                    DmrMessage::Csbk(csbk) if csbk.kind == CsbkKind::MbcHeader => {
                        self.mbc.process_header(csbk)
                    }
                    DmrMessage::MbcContinuation(block) => {
                        if let Some(multi) = self.mbc.process_continuation(block) {
                            follow_ups.push(DmrMessage::Csbk(multi));
                        }
                    }
                    other => self.mbc.reset(other.timeslot()),
                }
            }
            _ => {}
        }

        out.push(message);
        for follow_up in follow_ups {
            self.receive(follow_up, out);
        }
    }

    /// Fills in the downlink frequency of a grant / clear / vote-now channel
    /// from the LCN map. Ports `DMRMessageProcessor.enrich()`.
    fn enrich(&self, message: &mut DmrMessage) {
        if let DmrMessage::Csbk(csbk) = message {
            if csbk.kind.takes_frequencies() {
                if let Some(channel) = &mut csbk.channel {
                    if !channel.absolute {
                        if let Some(&hz) = self.lcn_frequencies.get(&channel.lcn) {
                            channel.downlink_hz = Some(hz);
                        }
                    }
                }
            }
        }
    }
}

/// A fragment buffer for the SLC and FLC assemblers (SDRTrunk's
/// `CorrectedBinaryMessage` with an add pointer).
#[derive(Debug, Default)]
struct Fragments {
    bits: Vec<u8>,
    pointer: usize,
    count: u32,
    open: bool,
}

impl Fragments {
    fn start(&mut self, size: usize, pointer: usize) {
        self.bits = vec![0; size];
        self.pointer = pointer;
        self.open = true;
    }

    fn reset(&mut self) {
        self.open = false;
        self.count = 0;
    }

    /// Adds a fragment; false when it would overflow (SDRTrunk's `BitSetFullException`).
    fn add(&mut self, fragment: &[u8], size: usize) -> bool {
        self.count += 1;
        if !self.open {
            self.start(size, 0);
        }
        if self.pointer + fragment.len() > self.bits.len() {
            return false;
        }
        self.bits[self.pointer..self.pointer + fragment.len()].copy_from_slice(fragment);
        self.pointer += fragment.len();
        true
    }
}

/// Short LC from four 17-bit CACH fragments. Ports `SLCAssembler`.
#[derive(Debug, Default)]
struct SlcAssembler {
    fragments: Fragments,
}

impl SlcAssembler {
    fn reset(&mut self) {
        self.fragments.reset();
    }

    fn process(&mut self, lcss: u8, fragment: &[u8; 17], timestamp_ms: u64) -> Option<ShortLc> {
        match lcss {
            FIRST_FRAGMENT => {
                let message = self.dispatch(timestamp_ms);
                self.fragments.start(68, 0);
                self.add(fragment, timestamp_ms);
                message
            }
            CONTINUATION_FRAGMENT => {
                if !self.fragments.open {
                    self.fragments.start(68, 17);
                }
                self.add(fragment, timestamp_ms);
                None
            }
            LAST_FRAGMENT => {
                if !self.fragments.open {
                    self.fragments.start(68, 51);
                }
                self.add(fragment, timestamp_ms);
                self.dispatch(timestamp_ms)
            }
            // A single fragment SLC (SDRTrunk also lands here on invalid CACHs).
            _ => Some(lc::create_short(fragment.to_vec(), timestamp_ms)),
        }
    }

    fn add(&mut self, fragment: &[u8], timestamp_ms: u64) {
        if !self.fragments.add(fragment, 68) {
            self.dispatch(timestamp_ms);
        }
    }

    /// The short LC once four fragments are in; CRC-8 checked (SDRTrunk's
    /// `decode()` overwrites the CRC result with the BPTC result).
    fn dispatch(&mut self, timestamp_ms: u64) -> Option<ShortLc> {
        let message = if self.fragments.open && self.fragments.count == 4 {
            Some(match bptc::decode_68_36(&self.fragments.bits) {
                Some((bits, _)) => lc::create_short(bits.to_vec(), timestamp_ms),
                None => {
                    let raw = bptc::extract_68_36(&bptc::deinterleave_68_36(&self.fragments.bits));
                    let mut slc = lc::create_short(raw.to_vec(), timestamp_ms);
                    slc.valid = false;
                    slc
                }
            })
        } else {
            None
        };
        self.fragments.reset();
        message
    }
}

/// Embedded full LC from four 32-bit voice burst fragments. Ports `FLCAssembler`.
#[derive(Debug)]
struct FlcAssembler {
    timeslot: u8,
    fragments: Fragments,
}

impl FlcAssembler {
    fn new(timeslot: u8) -> Self {
        FlcAssembler {
            timeslot,
            fragments: Fragments::default(),
        }
    }

    fn reset(&mut self) {
        self.fragments.reset();
    }

    fn process(
        &mut self,
        lcss: u8,
        fragment: &[u8],
        timestamp_ms: u64,
        masks: &mut DmrCrcMaskManager,
    ) -> Option<FullLc> {
        match lcss {
            FIRST_FRAGMENT => {
                let message = self.dispatch(timestamp_ms, masks);
                self.fragments.start(128, 0);
                self.add(fragment, timestamp_ms, masks);
                message
            }
            CONTINUATION_FRAGMENT => {
                if !self.fragments.open {
                    self.fragments.start(128, 32);
                }
                self.add(fragment, timestamp_ms, masks);
                None
            }
            LAST_FRAGMENT => {
                if !self.fragments.open {
                    self.fragments.start(128, 96);
                }
                self.add(fragment, timestamp_ms, masks);
                self.dispatch(timestamp_ms, masks)
            }
            // SINGLE_FRAGMENT: reverse channel signalling, not assembled.
            _ => {
                debug_assert_eq!(lcss, SINGLE_FRAGMENT);
                self.dispatch(timestamp_ms, masks);
                None
            }
        }
    }

    fn add(&mut self, fragment: &[u8], timestamp_ms: u64, masks: &mut DmrCrcMaskManager) {
        if !self.fragments.add(fragment, 128) {
            self.dispatch(timestamp_ms, masks);
        }
    }

    fn dispatch(&mut self, timestamp_ms: u64, masks: &mut DmrCrcMaskManager) -> Option<FullLc> {
        let message = if self.fragments.open && self.fragments.count == 4 {
            let (bits, corrected) = match bptc::decode_128_77(&self.fragments.bits) {
                Some((bits, n)) => (bits, n as i32),
                None => (
                    bptc::extract_128_77(&bptc::deinterleave_128_77(&self.fragments.bits)),
                    -2,
                ),
            };
            let mut flc = lc::create_full(
                bits.to_vec(),
                timestamp_ms,
                self.timeslot,
                false,
                corrected,
                masks,
            );
            if corrected < 0 {
                flc.valid = false;
            }
            Some(flc)
        } else {
            None
        };
        self.fragments.reset();
        message
    }
}

/// Multi-block CSBK reassembly per timeslot. Ports `MBCAssembler`.
#[derive(Debug, Default)]
struct MbcAssembler {
    headers: [Option<Csbk>; 2],
    blocks: [Vec<Vec<u8>>; 2],
}

impl MbcAssembler {
    fn index(timeslot: u8) -> Option<usize> {
        match timeslot {
            1 => Some(0),
            2 => Some(1),
            _ => None,
        }
    }

    fn process_header(&mut self, header: &Csbk) {
        if header.burst.valid {
            self.reset(header.burst.timeslot);
            if let Some(i) = Self::index(header.burst.timeslot) {
                self.headers[i] = Some(header.clone());
            }
        }
    }

    /// The assembled CSBK once the last block is in.
    fn process_continuation(&mut self, block: &DataBurst) -> Option<Csbk> {
        let i = Self::index(block.timeslot)?;
        self.blocks[i].push(block.bits.clone());
        if block.bits[0] != 1 {
            return None;
        }
        let multi = self.headers[i]
            .as_ref()
            .map(|header| csbk::create_multi(header, self.blocks[i].clone()));
        self.reset(block.timeslot);
        multi
    }

    fn reset(&mut self, timeslot: u8) {
        if let Some(i) = Self::index(timeslot) {
            self.headers[i] = None;
            self.blocks[i].clear();
        }
    }
}

#[cfg(test)]
#[path = "processor_tests.rs"]
mod tests;

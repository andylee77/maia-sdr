//! DMR sync patterns and the soft sync detector (SDRTrunk
//! `DMRSyncPattern`, `DMRSoftSyncDetectorScalar`, `DMRSyncModeMonitor`).

use crate::dsp::fsk4::ideal_phase;

/// A burst's sync, or the pseudo pattern of a sync-less voice burst B-F.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DmrSyncPattern {
    BaseStationData,
    BaseStationVoice,
    BsVoiceFrameB,
    BsVoiceFrameC,
    BsVoiceFrameD,
    BsVoiceFrameE,
    BsVoiceFrameF,
    MobileStationData,
    MobileStationVoice,
    MsVoiceFrameB,
    MsVoiceFrameC,
    MsVoiceFrameD,
    MsVoiceFrameE,
    MsVoiceFrameF,
    DirectEmptyTimeslot,
    DirectDataTimeslot1,
    DirectDataTimeslot2,
    DirectVoiceTimeslot1,
    DirectVoiceTimeslot2,
    DirectVoiceFrameB,
    DirectVoiceFrameC,
    DirectVoiceFrameD,
    DirectVoiceFrameE,
    DirectVoiceFrameF,
    Unknown,
}

use DmrSyncPattern::*;

impl DmrSyncPattern {
    /// The 48-bit sync word, for the patterns that have one.
    pub fn pattern(self) -> Option<u64> {
        Some(match self {
            BaseStationData => 0xDFF5_7D75_DF5D,
            BaseStationVoice => 0x755F_D7DF_75F7,
            MobileStationData => 0xD5D7_F77F_D757,
            MobileStationVoice => 0x7F7D_5DD5_7DFD,
            DirectDataTimeslot1 => 0xF7FD_D5DD_FD55,
            DirectDataTimeslot2 => 0xD755_7F5F_F7F5,
            DirectVoiceTimeslot1 => 0x5D57_7F77_57FF,
            DirectVoiceTimeslot2 => 0x7DFF_D5F5_5D5F,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            BaseStationData => "BS DATA",
            BaseStationVoice => "BS VOICE A",
            BsVoiceFrameB => "BS VOICE B",
            BsVoiceFrameC => "BS VOICE C",
            BsVoiceFrameD => "BS VOICE D",
            BsVoiceFrameE => "BS VOICE E",
            BsVoiceFrameF => "BS VOICE F",
            MobileStationData => "MS DATA",
            MobileStationVoice => "MS VOICE A",
            MsVoiceFrameB => "MS VOICE B",
            MsVoiceFrameC => "MS VOICE C",
            MsVoiceFrameD => "MS VOICE D",
            MsVoiceFrameE => "MS VOICE E",
            MsVoiceFrameF => "MS VOICE F",
            DirectEmptyTimeslot => "DIRECT EMPTY TIMESLOT",
            DirectDataTimeslot1 => "DIRECT DATA TS 1",
            DirectDataTimeslot2 => "DIRECT DATA TS 2",
            DirectVoiceTimeslot1 => "DIRECT VOICE A TS 1",
            DirectVoiceTimeslot2 => "DIRECT VOICE A TS 2",
            DirectVoiceFrameB => "DIRECT VOICE B",
            DirectVoiceFrameC => "DIRECT VOICE C",
            DirectVoiceFrameD => "DIRECT VOICE D",
            DirectVoiceFrameE => "DIRECT VOICE E",
            DirectVoiceFrameF => "DIRECT VOICE F",
            Unknown => "UNKNOWN",
        }
    }

    /// Bursts that start with a CACH (base station outbound).
    pub fn has_cach(self) -> bool {
        matches!(
            self,
            BaseStationData | BaseStationVoice | BsVoiceFrameB | BsVoiceFrameC | BsVoiceFrameD | BsVoiceFrameE
                | BsVoiceFrameF
        )
    }

    pub fn is_direct_ts1(self) -> bool {
        matches!(self, DirectVoiceTimeslot1 | DirectDataTimeslot1)
    }

    pub fn is_direct_ts2(self) -> bool {
        matches!(self, DirectVoiceTimeslot2 | DirectDataTimeslot2)
    }

    pub fn is_direct_voice(self) -> bool {
        matches!(self, DirectVoiceFrameB | DirectVoiceFrameC | DirectVoiceFrameD | DirectVoiceFrameE | DirectVoiceFrameF)
    }

    pub fn is_direct(self) -> bool {
        self.is_direct_ts1() || self.is_direct_ts2() || self.is_direct_voice()
    }

    pub fn is_mobile_station_sync_pattern(self) -> bool {
        matches!(
            self,
            MobileStationData | MobileStationVoice | MsVoiceFrameB | MsVoiceFrameC | MsVoiceFrameD | MsVoiceFrameE
                | MsVoiceFrameF
        )
    }

    pub fn is_voice_pattern(self) -> bool {
        matches!(
            self,
            BaseStationVoice | BsVoiceFrameB | BsVoiceFrameC | BsVoiceFrameD | BsVoiceFrameE | BsVoiceFrameF
                | MobileStationVoice | MsVoiceFrameB | MsVoiceFrameC | MsVoiceFrameD | MsVoiceFrameE
                | MsVoiceFrameF | DirectVoiceTimeslot1 | DirectVoiceTimeslot2 | DirectEmptyTimeslot
                | DirectVoiceFrameB | DirectVoiceFrameC | DirectVoiceFrameD | DirectVoiceFrameE
                | DirectVoiceFrameF
        )
    }

    /// The pseudo pattern of the next burst of a voice superframe.
    pub fn next_voice(self) -> DmrSyncPattern {
        match self {
            BaseStationVoice => BsVoiceFrameB,
            BsVoiceFrameB => BsVoiceFrameC,
            BsVoiceFrameC => BsVoiceFrameD,
            BsVoiceFrameD => BsVoiceFrameE,
            BsVoiceFrameE => BsVoiceFrameF,
            MobileStationVoice => MsVoiceFrameB,
            MsVoiceFrameB => MsVoiceFrameC,
            MsVoiceFrameC => MsVoiceFrameD,
            MsVoiceFrameD => MsVoiceFrameE,
            MsVoiceFrameE => MsVoiceFrameF,
            DirectVoiceTimeslot1 | DirectVoiceTimeslot2 => DirectVoiceFrameB,
            DirectVoiceFrameB => DirectVoiceFrameC,
            DirectVoiceFrameC => DirectVoiceFrameD,
            DirectVoiceFrameD => DirectVoiceFrameE,
            DirectVoiceFrameE => DirectVoiceFrameF,
            DirectEmptyTimeslot => DirectEmptyTimeslot,
            _ => Unknown,
        }
    }

    /// The 24 sync dibits, first sent first (01 = +3, 11 = -3).
    pub fn to_dibits(self) -> [u8; 24] {
        let value = self.pattern().unwrap_or(0);
        let mut d = [0u8; 24];
        for x in 0..24 {
            d[23 - x] = if (value >> (2 * x)) & 3 == 1 { 1 } else { 3 };
        }
        d
    }

    /// The ideal phases of the sync dibits.
    pub fn to_symbols(self) -> [f32; 24] {
        let mut s = [0.0f32; 24];
        for (x, d) in self.to_dibits().iter().enumerate() {
            s[x] = ideal_phase(*d);
        }
        s
    }
}

/// Which sync families the detector correlates against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmrSyncDetectMode {
    Automatic,
    BaseOnly,
    MobileOnly,
    DirectOnly,
}

/// SDRTrunk `DMRSoftSyncDetectorScalar`: correlates the last 24 soft symbols
/// with the sync patterns of the current mode.
pub struct DmrSoftSyncDetector {
    symbols: [f32; 48],
    pointer: usize,
    mode: DmrSyncDetectMode,
    detected: DmrSyncPattern,
}

/// Each mode's candidate patterns, in SDRTrunk's tie order.
fn candidates(mode: DmrSyncDetectMode) -> &'static [DmrSyncPattern] {
    match mode {
        DmrSyncDetectMode::Automatic => &[
            BaseStationData,
            BaseStationVoice,
            MobileStationData,
            MobileStationVoice,
            DirectDataTimeslot1,
            DirectDataTimeslot2,
            DirectVoiceTimeslot1,
            DirectVoiceTimeslot2,
        ],
        DmrSyncDetectMode::BaseOnly => &[BaseStationVoice, BaseStationData],
        DmrSyncDetectMode::MobileOnly => &[MobileStationVoice, MobileStationData],
        DmrSyncDetectMode::DirectOnly => {
            &[DirectDataTimeslot1, DirectDataTimeslot2, DirectVoiceTimeslot1, DirectVoiceTimeslot2]
        }
    }
}

impl Default for DmrSoftSyncDetector {
    fn default() -> Self {
        DmrSoftSyncDetector {
            symbols: [0.0; 48],
            pointer: 0,
            mode: DmrSyncDetectMode::Automatic,
            detected: BaseStationData,
        }
    }
}

impl DmrSoftSyncDetector {
    pub fn reset(&mut self) {
        self.symbols = [0.0; 48];
        self.pointer = 0;
    }

    pub fn set_mode(&mut self, mode: DmrSyncDetectMode) {
        self.mode = mode;
    }

    pub fn detected_pattern(&self) -> DmrSyncPattern {
        self.detected
    }

    pub fn process(&mut self, symbol: f32) {
        self.symbols[self.pointer] = symbol;
        self.symbols[self.pointer + 24] = symbol;
        self.pointer = (self.pointer + 1) % 24;
    }

    pub fn process_and_calculate(&mut self, symbol: f32) -> f32 {
        self.process(symbol);
        self.calculate()
    }

    /// Best correlation score of the mode's patterns; sets the detected pattern.
    pub fn calculate(&mut self) -> f32 {
        let window = &self.symbols[self.pointer..self.pointer + 24];
        let list = candidates(self.mode);
        let mut best = f32::MIN;
        for (n, p) in list.iter().enumerate() {
            let ideal = p.to_symbols();
            let score: f32 = ideal.iter().zip(window).map(|(a, b)| a * b).sum();
            // SDRTrunk takes the first candidate, then any strictly better one.
            if n == 0 || score > best {
                best = score;
                self.detected = *p;
            }
        }
        best
    }
}

/// SDRTrunk `DMRSyncModeMonitor`: once one sync family leads the others by
/// more than 10 detections, the detectors correlate against that family only.
#[derive(Default)]
pub struct DmrSyncModeMonitor {
    base: u32,
    mobile: u32,
    direct: u32,
    fixed: bool,
}

const DOMINANT_THRESHOLD: i64 = 10;

impl DmrSyncModeMonitor {
    /// A fixed mode (a traffic channel is known to be a base station).
    pub fn fix(&mut self) {
        self.fixed = true;
    }

    /// Counts a detection; returns a new mode when one family dominates.
    pub fn detected(&mut self, pattern: DmrSyncPattern) -> Option<DmrSyncDetectMode> {
        if self.fixed {
            return None;
        }
        match pattern {
            BaseStationData | BaseStationVoice => self.base += 1,
            MobileStationData | MobileStationVoice => self.mobile += 1,
            DirectDataTimeslot1 | DirectDataTimeslot2 | DirectVoiceTimeslot1 | DirectVoiceTimeslot2 => {
                self.direct += 1
            }
            _ => {}
        }
        let (b, m, d) = (self.base as i64, self.mobile as i64, self.direct as i64);
        let mode = if b - m > DOMINANT_THRESHOLD && b - d > DOMINANT_THRESHOLD {
            DmrSyncDetectMode::BaseOnly
        } else if m - b > DOMINANT_THRESHOLD && m - d > DOMINANT_THRESHOLD {
            DmrSyncDetectMode::MobileOnly
        } else if d - b > DOMINANT_THRESHOLD && d - m > DOMINANT_THRESHOLD {
            DmrSyncDetectMode::DirectOnly
        } else {
            return None;
        };
        self.fixed = true;
        Some(mode)
    }
}

#[cfg(test)]
#[path = "sync_tests.rs"]
mod tests;

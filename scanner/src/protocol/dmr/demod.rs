//! DMR 4FSK demodulation in software, from a 50 kSPS DDC's IQ.
//!
//! A port of SDRTrunk's DMR receive chain:
//! - `DMRDecoder.receive`: decimate to 25 kSPS, baseband low-pass, RRC on I
//!   and Q, then the differential demodulator shared with the P25 C4FM
//!   receiver (`protocol::p25::c4fm`). SDRTrunk low-passes before decimating;
//!   the other order costs a quarter as much (see `filters::LPF_DMR_25K`).
//! - `DMRSoftSymbolProcessor`: symbol timing from the sync patterns only (a
//!   primary detector and one lagging by half a symbol until the first sync,
//!   then a check at every burst's sync position), an equaliser (balance +
//!   gain) learned from each sync, and a 90-dibit delay line re-sampled at a
//!   sync so the CACH and first payload half get the corrected timing.
//!
//! A deviation in the symbol processor: SDRTrunk's `receive` re-applies the
//! equaliser to the last few samples of every buffer load (its loop starts at
//! the read pointer, not the end of the previous load). Here each sample is
//! equalised once.

use super::filters::{root_raised_cosine, LPF_DMR_25K};
use super::sync::{DmrSoftSyncDetector, DmrSyncModeMonitor, DmrSyncPattern};
use crate::dsp::fsk4::{ideal_phase, linear, to_symbol, DifferentialDemod, Fir};
use crate::dsp::taps::HALFBAND_63;

pub const SYMBOL_RATE: f64 = 4800.0;
/// Rate of the IQ this decoder takes (the DDC output).
pub const INPUT_RATE_HZ: f64 = 50_000.0;
/// Rate after the half-band (SDRTrunk decimates to below 38.4 kSPS).
pub const DEMOD_RATE_HZ: f64 = 25_000.0;

/// Dibits in one burst (CACH + payload + sync/EMB + payload).
pub const BURST_DIBITS: u64 = 144;

/// Where demodulated dibits go: the DMR framer.
pub trait DmrSymbolSink {
    /// The next dibit, out of the 90-dibit delay line.
    fn receive(&mut self, dibit: u8);
    /// A sync ended at the last dibit that entered the delay line, so the
    /// next dibit out is the first CACH dibit of that burst.
    fn sync_detected(&mut self, pattern: DmrSyncPattern);
    /// A voice superframe is being assembled (bursts B-F carry no sync).
    fn is_voice_super_frame(&self) -> bool;
}

const BUFFER_PROTECTED_REGION_DIBITS: f64 = 92.0;
const BUFFER_WORKSPACE_LENGTH_DIBITS: f64 = 25.0;
const BUFFER_LENGTH_DIBITS: f64 = BUFFER_PROTECTED_REGION_DIBITS + BUFFER_WORKSPACE_LENGTH_DIBITS;
const EQUALIZER_LOOP_GAIN: f32 = 0.15;
const MAXIMUM_EQUALIZER_BALANCE: f32 = std::f32::consts::PI / 3.0;
const MAXIMUM_EQUALIZER_GAIN: f32 = 1.25;
const MAXIMUM_POSITIVE_SAMPLE_PHASE: f32 = 3.5;
const MAXIMUM_NEGATIVE_SAMPLE_PHASE: f32 = -3.5;
const SAMPLES_PER_SYMBOL_ALLOWABLE_DEVIATION: f64 = 0.005;
const SYNC_DETECTION_THRESHOLD: f32 = 60.0;
const SYNC_OPTIMIZED_THRESHOLD: f32 = 80.0;
const SYNC_EQUALIZED_THRESHOLD: f32 = 100.0;
/// Coarse sync: the optimised correlation must reach this.
const SYNC_COARSE_THRESHOLD: f32 = 95.0;
const TWO_PI: f32 = 2.0 * std::f32::consts::PI;
/// CACH (12) + message prefix (54) + sync (24).
const DELAY_LINE_DIBITS: usize = 90;

/// SDRTrunk `DibitDelayLine`.
struct DibitDelayLine {
    line: Vec<u8>,
    pointer: usize,
}

impl DibitDelayLine {
    fn new(length: usize) -> Self {
        DibitDelayLine { line: vec![0; length], pointer: 0 }
    }

    /// Inserts a dibit and returns the one it displaces.
    fn insert(&mut self, dibit: u8) -> u8 {
        let ejected = self.line[self.pointer];
        self.line[self.pointer] = dibit;
        self.pointer = (self.pointer + 1) % self.line.len();
        ejected
    }

    /// Overwrites the most recent `dibits.len()` entries.
    fn update(&mut self, dibits: &[u8]) {
        let len = self.line.len();
        self.pointer = (self.pointer + len - dibits.len() % len) % len;
        for d in dibits {
            self.line[self.pointer] = *d;
            self.pointer = (self.pointer + 1) % len;
        }
    }
}

/// Demodulation statistics (diagnostics / tests).
#[derive(Debug, Clone, Copy, Default)]
pub struct DmrDemodStats {
    pub symbols: u64,
    /// Syncs found from scratch (coarse timing search).
    pub coarse_syncs: u64,
    /// Syncs confirmed at the expected burst position.
    pub fine_syncs: u64,
    /// Expected syncs that were not there (fine sync dropped).
    pub fine_sync_losses: u64,
}

/// SDRTrunk `DMRSoftSymbolProcessor`.
pub struct DmrSoftSymbolProcessor {
    mode_monitor: DmrSyncModeMonitor,
    sync_detector: DmrSoftSyncDetector,
    sync_detector_secondary: DmrSoftSyncDetector,
    delay_line: DibitDelayLine,
    fine_sync: bool,
    equalizer_initialized: bool,
    noise_std_threshold: f64,
    secondary_sync_offset: f64,
    samples_per_symbol: f64,
    observed_samples_per_symbol: f64,
    sample_point: f64,
    equalizer_balance: f32,
    equalizer_gain: f32,
    optimize_fine_increment: f64,
    buffer: Vec<f32>,
    reserved_region: usize,
    load_pointer: usize,
    buffer_pointer: usize,
    /// Samples before this index have been unwrapped and equalised.
    equalized_to: usize,
    workspace_length: usize,
    symbols_since_last_sync: u64,
    pub stats: DmrDemodStats,
}

impl DmrSoftSymbolProcessor {
    pub fn new(samples_per_symbol: f64) -> Self {
        let sps = samples_per_symbol;
        let max_sps = sps * (1.0 + SAMPLES_PER_SYMBOL_ALLOWABLE_DEVIATION);
        let load = (BUFFER_PROTECTED_REGION_DIBITS * sps).ceil() as usize;
        DmrSoftSymbolProcessor {
            mode_monitor: DmrSyncModeMonitor::default(),
            sync_detector: DmrSoftSyncDetector::default(),
            sync_detector_secondary: DmrSoftSyncDetector::default(),
            delay_line: DibitDelayLine::new(DELAY_LINE_DIBITS),
            fine_sync: false,
            equalizer_initialized: false,
            // 120 % of the optimal sample-to-sample deviation.
            noise_std_threshold: ideal_phase(1) as f64 * 2.0 / sps * 1.2,
            secondary_sync_offset: sps / 2.0,
            samples_per_symbol: sps,
            observed_samples_per_symbol: sps,
            sample_point: sps,
            equalizer_balance: 0.0,
            equalizer_gain: 1.0,
            // Fine timing steps of 0.4 % of a symbol.
            optimize_fine_increment: sps * 0.004,
            buffer: vec![0.0; (BUFFER_LENGTH_DIBITS * sps).ceil() as usize],
            reserved_region: (max_sps / 2.0 + max_sps / 10.0).ceil() as usize,
            load_pointer: load,
            buffer_pointer: load,
            equalized_to: load,
            workspace_length: (BUFFER_WORKSPACE_LENGTH_DIBITS * sps).ceil() as usize,
            symbols_since_last_sync: 0,
            stats: DmrDemodStats::default(),
        }
    }

    /// Traffic channels are known base stations: correlate BS syncs only.
    pub fn set_base_station_mode(&mut self) {
        self.mode_monitor.fix();
        self.sync_detector.set_mode(super::sync::DmrSyncDetectMode::BaseOnly);
        self.sync_detector_secondary.set_mode(super::sync::DmrSyncDetectMode::BaseOnly);
    }

    /// Equaliser balance: the carrier offset in radians per symbol.
    pub fn equalizer_balance(&self) -> f32 {
        self.equalizer_balance
    }

    pub fn equalizer_gain(&self) -> f32 {
        self.equalizer_gain
    }

    pub fn has_fine_sync(&self) -> bool {
        self.fine_sync
    }

    fn buf(&self, i: usize) -> f32 {
        self.buffer.get(i).copied().unwrap_or(0.0)
    }

    fn interpolate(&self, position: f64) -> f32 {
        if position < 0.0 {
            return 0.0;
        }
        let i = position.floor();
        let n = i as usize;
        linear(self.buf(n), self.buf(n + 1), position - i)
    }

    /// Demodulate differential-phase samples; dibits go to `sink`.
    pub fn receive(&mut self, samples: &[f32], sink: &mut impl DmrSymbolSink) {
        let mut samples_pointer = 0usize;
        while samples_pointer < samples.len() {
            if self.load_pointer == self.buffer.len() {
                let ws = self.workspace_length;
                self.buffer.copy_within(ws.., 0);
                self.load_pointer -= ws;
                self.buffer_pointer -= ws;
                self.equalized_to = self.equalized_to.saturating_sub(ws);
            }
            let copy = (self.buffer.len() - self.load_pointer).min(samples.len() - samples_pointer);
            self.buffer[self.load_pointer..self.load_pointer + copy]
                .copy_from_slice(&samples[samples_pointer..samples_pointer + copy]);
            samples_pointer += copy;
            self.load_pointer += copy;

            for x in self.equalized_to.max(1)..self.load_pointer {
                // Unwrap phases across the +-pi seam.
                if self.buffer[x - 1] > 1.5 && self.buffer[x] < -1.5 {
                    self.buffer[x] += TWO_PI;
                } else if self.buffer[x - 1] < -1.5 && self.buffer[x] > 1.5 {
                    self.buffer[x] -= TWO_PI;
                }
                let s = (self.buffer[x] + self.equalizer_balance) * self.equalizer_gain;
                // Slightly beyond +-pi so the optimiser and equaliser still see
                // the true error; to_symbol maps such values correctly.
                self.buffer[x] = s.clamp(MAXIMUM_NEGATIVE_SAMPLE_PHASE, MAXIMUM_POSITIVE_SAMPLE_PHASE);
            }
            self.equalized_to = self.load_pointer;

            while self.buffer_pointer < self.load_pointer.saturating_sub(self.reserved_region) {
                self.buffer_pointer += 1;
                self.sample_point -= 1.0;
                if self.sample_point >= 1.0 {
                    continue;
                }
                if self.symbols_since_last_sync > BURST_DIBITS {
                    self.fine_sync = false;
                }
                let soft = self.interpolate(self.buffer_pointer as f64 + self.sample_point);
                self.sync_detector.process(soft);
                let ejected = self.delay_line.insert(to_symbol(soft));
                sink.receive(ejected);
                self.symbols_since_last_sync += 1;
                self.stats.symbols += 1;

                if self.fine_sync {
                    if self.symbols_since_last_sync >= BURST_DIBITS {
                        if sink.is_voice_super_frame() {
                            self.symbols_since_last_sync -= BURST_DIBITS;
                        } else {
                            let primary_score = self.sync_detector.calculate();
                            let pattern = self.sync_detector.detected_pattern();
                            if primary_score > SYNC_DETECTION_THRESHOLD && self.optimize_fine(pattern) {
                                if let Some(mode) = self.mode_monitor.detected(pattern) {
                                    self.sync_detector.set_mode(mode);
                                    self.sync_detector_secondary.set_mode(mode);
                                }
                                self.stats.fine_syncs += 1;
                                sink.sync_detected(pattern);
                                self.symbols_since_last_sync = 0;
                            } else {
                                self.stats.fine_sync_losses += 1;
                                self.fine_sync = false;
                                self.sync_detector_secondary.reset();
                                let s = self.interpolate(
                                    self.buffer_pointer as f64 + self.sample_point - self.secondary_sync_offset,
                                );
                                self.sync_detector_secondary.process(s);
                            }
                        }
                    }
                } else {
                    let primary_score = self.sync_detector.calculate();
                    let s = self
                        .interpolate(self.buffer_pointer as f64 + self.sample_point - self.secondary_sync_offset);
                    let secondary_score = self.sync_detector_secondary.process_and_calculate(s);
                    let primary = self.sync_detector.detected_pattern();
                    let secondary = self.sync_detector_secondary.detected_pattern();
                    if primary_score > SYNC_DETECTION_THRESHOLD && self.optimize_coarse(primary, 0.0) {
                        self.stats.coarse_syncs += 1;
                        sink.sync_detected(primary);
                        self.fine_sync = true;
                        self.symbols_since_last_sync = 0;
                    } else if secondary_score > SYNC_DETECTION_THRESHOLD
                        && self.optimize_coarse(secondary, -self.secondary_sync_offset)
                    {
                        self.stats.coarse_syncs += 1;
                        sink.sync_detected(secondary);
                        self.fine_sync = true;
                        self.symbols_since_last_sync = 0;
                    }
                }
                self.sample_point += self.observed_samples_per_symbol;
            }
        }
    }

    /// The sample-to-sample deviation over the detected sync is above what a
    /// modulated signal gives: noise (SDRTrunk `isNoisy`).
    fn is_noisy(&self, offset: f64) -> bool {
        let start = (offset - 23.0 * self.observed_samples_per_symbol).floor();
        if start < 0.0 {
            return true;
        }
        let start = start as usize;
        let end = (offset.ceil().max(0.0) as usize).min(self.buffer.len() - 1);
        if end <= start + 1 {
            return false;
        }
        let n = (end - start) as f64;
        let (mut sum, mut sum_sq) = (0.0f64, 0.0f64);
        for i in start..end {
            let d = (self.buffer[i] - self.buffer[i + 1]) as f64;
            sum += d;
            sum_sq += d * d;
        }
        let mean = sum / n;
        let var = (sum_sq - n * mean * mean) / (n - 1.0);
        var.max(0.0).sqrt() > self.noise_std_threshold
    }

    /// Moves `sample_point` into [0, 1] by moving the buffer pointer.
    fn normalize_sample_point(&mut self) {
        while self.sample_point < 0.0 {
            self.sample_point += 1.0;
            self.buffer_pointer -= 1;
        }
        while self.sample_point > 1.0 {
            self.sample_point -= 1.0;
            self.buffer_pointer += 1;
        }
    }

    /// Timing search after a fresh detection (SDRTrunk `optimizeCoarse`).
    fn optimize_coarse(&mut self, pattern: DmrSyncPattern, additional_offset: f64) -> bool {
        let offset = self.buffer_pointer as f64 + self.sample_point + additional_offset;
        if self.is_noisy(offset) {
            return false;
        }
        let sps = self.observed_samples_per_symbol;
        let mut step = self.samples_per_symbol / 8.0;
        let step_min = self.optimize_fine_increment;
        let mut adjustment = 0.0f64;
        let adjustment_max = self.samples_per_symbol / 4.0;
        let mut score_center = self.score(offset, sps, pattern);
        let mut score_left = self.score(offset - step, sps, pattern);
        let mut score_right = self.score(offset + step, sps, pattern);
        while step > step_min && adjustment.abs() <= adjustment_max {
            if score_left > score_right && score_left > score_center {
                adjustment -= step;
                score_right = score_center;
                score_center = score_left;
                score_left = self.score(offset + adjustment - step, sps, pattern);
            } else if score_right > score_left && score_right > score_center {
                adjustment += step;
                score_left = score_center;
                score_center = score_right;
                score_right = self.score(offset + adjustment + step, sps, pattern);
            } else {
                step *= 0.5;
                if step > step_min {
                    score_left = self.score(offset + adjustment - step, sps, pattern);
                    score_right = self.score(offset + adjustment + step, sps, pattern);
                }
            }
        }
        if score_center < SYNC_COARSE_THRESHOLD {
            return false;
        }
        adjustment += additional_offset;
        self.sample_point += adjustment;
        self.normalize_sample_point();

        let resample = !self.equalizer_initialized || adjustment.abs() > 0.25;
        self.update_equalizer(pattern);

        if resample {
            // Re-read the 66 dibits before the sync (CACH + message prefix)
            // with the corrected timing and equaliser; the sync itself is
            // taken as sent.
            let mut pointer = self.buffer_pointer as f64 + self.sample_point - 89.0 * sps;
            for _ in 0..66 {
                let d = if pointer >= 0.0 { to_symbol(self.interpolate(pointer)) } else { 1 };
                self.delay_line.insert(d);
                pointer += sps;
            }
            for d in pattern.to_dibits() {
                self.delay_line.insert(d);
            }
        } else {
            // Overwrite the captured sync so it carries no bit errors.
            self.delay_line.update(&pattern.to_dibits());
        }
        true
    }

    /// +-0.4 % timing nudge at an expected sync (SDRTrunk `optimizeFine`).
    fn optimize_fine(&mut self, pattern: DmrSyncPattern) -> bool {
        let sps = self.observed_samples_per_symbol;
        let offset = self.buffer_pointer as f64 + self.sample_point;
        let current = self.score(offset, sps, pattern);
        let mut candidate = self.score(offset - self.optimize_fine_increment, sps, pattern);
        let mut adjusted = false;
        if candidate > current {
            self.sample_point -= self.optimize_fine_increment;
            adjusted = true;
        } else {
            candidate = self.score(offset + self.optimize_fine_increment, sps, pattern);
            if candidate > current {
                self.sample_point += self.optimize_fine_increment;
                adjusted = true;
            }
        }
        if adjusted {
            self.normalize_sample_point();
        }
        // SDRTrunk compares the last candidate's score, adjusted or not.
        if candidate > SYNC_OPTIMIZED_THRESHOLD {
            self.update_equalizer(pattern);
        }
        candidate > SYNC_EQUALIZED_THRESHOLD
    }

    /// Balance and gain from the sync symbols (SDRTrunk `updateEqualizer`).
    fn update_equalizer(&mut self, pattern: DmrSyncPattern) {
        let symbols = pattern.to_symbols();
        let sps = self.observed_samples_per_symbol;
        let mut start = self.buffer_pointer as f64 + self.sample_point;
        let s = self.interpolate(start);
        let mut balance = s - symbols[23];
        let mut gain = symbols[23].abs() - s.abs();
        start -= 23.0 * sps;
        for symbol in symbols.iter().take(23) {
            if start >= 0.0 {
                let s = self.interpolate(start);
                balance += s - symbol;
                gain += symbol.abs() - s.abs();
            }
            start += sps;
        }
        balance /= -24.0;
        gain /= 24.0 * ideal_phase(1);
        if self.equalizer_initialized {
            self.equalizer_balance += balance * EQUALIZER_LOOP_GAIN;
            self.equalizer_gain += gain * EQUALIZER_LOOP_GAIN;
        } else {
            self.equalizer_balance += balance;
            self.equalizer_gain += gain;
        }
        self.equalizer_balance = self.equalizer_balance.clamp(-MAXIMUM_EQUALIZER_BALANCE, MAXIMUM_EQUALIZER_BALANCE);
        self.equalizer_gain = self.equalizer_gain.clamp(1.0, MAXIMUM_EQUALIZER_GAIN);
        if !self.equalizer_initialized {
            // Apply the first settings to the buffered samples so the burst
            // can be re-sampled.
            let (b, g) = (self.equalizer_balance, self.equalizer_gain);
            for x in 0..self.buffer_pointer.min(self.buffer.len()) {
                self.buffer[x] = (self.buffer[x] + b) * g;
            }
            self.equalizer_initialized = true;
        }
    }

    /// Correlation of the 24 symbols ending at `offset` with a sync pattern.
    fn score(&self, offset: f64, samples_per_symbol: f64, pattern: DmrSyncPattern) -> f32 {
        let symbols = pattern.to_symbols();
        let max_pointer = self.buffer.len() - 1;
        let mut pointer = offset - samples_per_symbol * 23.0;
        let mut score = 0.0f32;
        for symbol in symbols.iter() {
            let i = pointer.floor();
            let soft = if i >= 0.0 && (i as usize) < max_pointer {
                let n = i as usize;
                linear(self.buffer[n], self.buffer[n + 1], pointer - i)
            } else {
                0.0
            };
            score += soft * symbol;
            pointer += samples_per_symbol;
        }
        score
    }
}

/// The whole DMR receive chain: 50 kSPS IQ in, dibits out.
pub struct DmrDemodulator {
    dec_i: Fir,
    dec_q: Fir,
    lpf_i: Fir,
    lpf_q: Fir,
    rrc_i: Fir,
    rrc_q: Fir,
    diff: DifferentialDemod,
    pub symbols: DmrSoftSymbolProcessor,
    scratch: [Vec<f32>; 4],
}

impl Default for DmrDemodulator {
    fn default() -> Self {
        Self::new()
    }
}

impl DmrDemodulator {
    pub fn new() -> Self {
        let sps = DEMOD_RATE_HZ / SYMBOL_RATE;
        // DMRDecoder: alpha = 5760 / decimated rate, floor(-44 alpha + 33)
        // symbols, rounded up to even.
        let alpha = (5760.0 / DEMOD_RATE_HZ) as f32;
        let mut symbols = ((-44.0 * alpha) + 33.0).floor() as usize;
        symbols += symbols % 2;
        let rrc = root_raised_cosine(sps, symbols, alpha);
        DmrDemodulator {
            dec_i: Fir::new(&HALFBAND_63, true),
            dec_q: Fir::new(&HALFBAND_63, true),
            lpf_i: Fir::new(&LPF_DMR_25K, false),
            lpf_q: Fir::new(&LPF_DMR_25K, false),
            rrc_i: Fir::new(&rrc, false),
            rrc_q: Fir::new(&rrc, false),
            diff: DifferentialDemod::new(sps),
            symbols: DmrSoftSymbolProcessor::new(sps),
            scratch: Default::default(),
        }
    }

    /// Feed interleaved 16-bit IQ (the DDC ring format: I, Q per sample).
    pub fn process_iq_i16(&mut self, iq: &[i16], sink: &mut impl DmrSymbolSink) {
        let n = iq.len() / 2;
        let i: Vec<f32> = (0..n).map(|k| iq[2 * k] as f32).collect();
        let q: Vec<f32> = (0..n).map(|k| iq[2 * k + 1] as f32).collect();
        self.process_iq(&i, &q, sink);
    }

    pub fn process_iq(&mut self, i: &[f32], q: &[f32], sink: &mut impl DmrSymbolSink) {
        let [a, b, c, d] = &mut self.scratch;
        a.clear();
        b.clear();
        self.dec_i.process(i, a);
        self.dec_q.process(q, b);
        c.clear();
        d.clear();
        self.lpf_i.process(a, c);
        self.lpf_q.process(b, d);
        a.clear();
        b.clear();
        self.rrc_i.process(c, a);
        self.rrc_q.process(d, b);
        c.clear();
        self.diff.demodulate(a, b, c);
        let phases = std::mem::take(c);
        self.symbols.receive(&phases, sink);
        self.scratch[2] = phases;
    }
}

#[cfg(test)]
#[path = "demod_tests.rs"]
mod tests;

//! BIST tone checker: an AD9361 BIST tone at k*fs/32 makes the sample
//! stream exactly periodic with period 32. The first period is taken as the
//! reference; every later sample must match it at the running phase. A
//! break is classified by the phase jump that re-locks the stream:
//! `bit_error` (same phase resumes), `word_gap` (phase moved), `splice`
//! (inside an epoch window). Laps are multiples of 32 samples and are
//! therefore invisible to this pattern (reported in the summary).

use super::{Anomaly, CheckCfg, ChunkMeta, Class, Geometry, Recorder, StreamCheck};
use crate::util::{hexn, sext12};
use serde_json::{json, Value};

const MASK: u32 = 0x0FFF_0FFF;
const RELOCK_WINDOW: usize = 8;

pub struct ToneChecker {
    geom: Geometry,
    cfg: CheckCfg,
    period: usize,
    tol: u32,
    reference: Vec<u32>,
    phase: usize,
    global_base: u64,
    epoch_until: u64,
    pub rec: Recorder,
    units: u64,
    ok: u64,
    breaks: u64,
    lost_units: u128,
}

impl ToneChecker {
    pub fn new(geom: Geometry, cfg: CheckCfg, period: usize, tol: u32) -> ToneChecker {
        let max = cfg.max_records;
        ToneChecker {
            geom,
            cfg,
            period: period.max(2),
            tol,
            reference: Vec::new(),
            phase: 0,
            global_base: 0,
            epoch_until: 0,
            rec: Recorder::new(max),
            units: 0,
            ok: 0,
            breaks: 0,
            lost_units: 0,
        }
    }

    #[inline]
    fn close(&self, a: u32, b: u32) -> bool {
        if self.tol == 0 {
            return (a & MASK) == (b & MASK);
        }
        let di = (sext12(a & 0xFFF) - sext12(b & 0xFFF)).unsigned_abs();
        let dq = (sext12((a >> 16) & 0xFFF) - sext12((b >> 16) & 0xFFF)).unsigned_abs();
        di <= self.tol && dq <= self.tol
    }

    fn anomaly(&self, class: Class, meta: &ChunkMeta, i: usize, exp: u32, act: u32) -> Anomaly {
        Anomaly {
            class,
            offset: meta.byte_offset + (i * 4) as u64,
            chunk: meta.seq,
            subbuf: meta.subbuf,
            unit: i as u64,
            expected: Some(exp as u64),
            actual: act as u64,
            expected_pos: Some(self.phase as u64),
            actual_pos: None,
            delta: None,
            laps: None,
            bits: None,
            len_units: None,
            t_s: self.rec.t_of(meta),
            note: None,
        }
    }
}

fn sample(data: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(data[i * 4..i * 4 + 4].try_into().unwrap())
}

impl StreamCheck for ToneChecker {
    fn process(&mut self, data: &[u8], meta: &ChunkMeta) {
        let n = data.len() / 4;
        let p = self.period;
        if self.rec.t0_ns.is_none() {
            self.rec.t0_ns = meta.wake_ts_ns;
        }
        if meta.epoch {
            self.reference.clear();
            self.epoch_until = self.global_base
                + self.cfg.epoch_window_units.unwrap_or_else(|| self.geom.subbuf_units());
        }
        let mut i = 0;
        while i < n {
            let s = sample(data, i);
            if self.reference.len() < p {
                self.reference.push(s);
                self.phase = self.reference.len() % p;
                i += 1;
                continue;
            }
            let exp = self.reference[self.phase];
            if self.close(s, exp) {
                self.ok += 1;
                self.phase = (self.phase + 1) % p;
                i += 1;
                continue;
            }
            self.breaks += 1;
            let g = self.global_base + i as u64;
            // Single corrupted sample?
            if i + 1 < n && self.close(sample(data, i + 1), self.reference[(self.phase + 1) % p]) {
                let mut a = self.anomaly(Class::BitError, meta, i, exp, s);
                a.bits = Some(((s ^ exp) & MASK).count_ones());
                self.rec.push(&a);
                self.phase = (self.phase + 1) % p;
                i += 1;
                continue;
            }
            // Re-lock: find the phase that explains the next samples.
            let w = RELOCK_WINDOW.min(n - i);
            let found = (0..p).find(|&ph| {
                (0..w).all(|j| self.close(sample(data, i + j), self.reference[(ph + j) % p]))
            });
            match found {
                Some(ph) => {
                    let delta = (ph + p - self.phase) % p;
                    let class = if g < self.epoch_until {
                        Class::Splice
                    } else if delta == 0 {
                        Class::BitError
                    } else {
                        Class::WordGap
                    };
                    let mut a = self.anomaly(class, meta, i, exp, s);
                    a.delta = Some(delta as i128);
                    a.note = Some("phase jump modulo the tone period");
                    if i == 0 && meta.boundary {
                        a.note = Some("phase jump at sub-buffer boundary (modulo the tone period)");
                    }
                    self.rec.push(&a);
                    if class == Class::WordGap {
                        self.lost_units += delta as u128;
                    }
                    self.phase = (ph + 1) % p;
                }
                None => {
                    let mut a = self.anomaly(Class::BitError, meta, i, exp, s);
                    a.note = Some("no phase matches");
                    self.rec.push(&a);
                    self.phase = (self.phase + 1) % p;
                }
            }
            i += 1;
        }
        self.units += n as u64;
        self.global_base += n as u64;
    }

    fn summary(&mut self) -> Value {
        self.rec.flush();
        json!({
            "pattern": "tone",
            "unit_bytes": 4,
            "units_checked": self.units,
            "units_ok": self.ok,
            "breaks": self.breaks,
            "lost_units": self.lost_units as u64,
            "lost_bytes": (self.lost_units * 4) as u64,
            "counts": self.rec.counts_json(),
            "anomalies_total": self.rec.total,
            "model": {"period_samples": self.period, "tolerance_lsb": self.tol,
                      "reference": self.reference.iter().map(|v| hexn(*v as u64)).collect::<Vec<_>>(),
                      "note": "laps (multiples of the period) are invisible to the tone pattern"},
            "synced": self.reference.len() == self.period,
        })
    }

    fn recorder(&mut self) -> &mut Recorder {
        &mut self.rec
    }

    fn unit_bytes(&self) -> usize {
        4
    }

    fn lost_units(&self) -> u128 {
        self.lost_units
    }
}

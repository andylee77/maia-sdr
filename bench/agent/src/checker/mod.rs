//! Streaming ring-buffer checker (design doc F3 / section 7).
//!
//! The stream is fed chunk by chunk (normally one sub-buffer per chunk, in
//! the order the reader copied them). Each unit is mapped to a sequence
//! position by a pattern model; any discontinuity is classified:
//!
//! | class        | rule |
//! |--------------|------|
//! | `bit_error`  | value within `bit_err_max_bits` of the expected value and the stream continues, or an undecodable unit |
//! | `word_gap`   | forward jump inside a sub-buffer (or at a boundary when it is not a whole number of laps) |
//! | `lap`        | forward jump of k x ring size at a sub-buffer boundary (reader lapped by the writer) |
//! | `torn`       | backward jump into old-lap data inside a sub-buffer (copy raced the writer / stale data); the region ends when the stream returns to the original sequence |
//! | `stale_line` | one 32-byte aligned chunk exactly k laps old, then the stream resumes (stale cache line) |
//! | `splice`     | any discontinuity within the epoch window after an enable epoch, or a tag change |
//! | `repeat`     | small backward jump (<= one sub-buffer): duplicated block |
//!
//! All position arithmetic is modulo the pattern period, so periodic
//! patterns (pn0fn: 65535 samples) still classify laps by their residue.

pub mod models;
pub mod pn1;
pub mod tone;

use crate::util::hexn;
use models::Model;
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    WordGap = 0,
    Lap = 1,
    Torn = 2,
    StaleLine = 3,
    Splice = 4,
    BitError = 5,
    Repeat = 6,
}

pub const CLASS_NAMES: [&str; 7] = [
    "word_gap",
    "lap",
    "torn",
    "stale_line",
    "splice",
    "bit_error",
    "repeat",
];

impl Class {
    pub fn name(self) -> &'static str {
        CLASS_NAMES[self as usize]
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub unit_bytes: usize,
    pub subbuf_bytes: usize,
    pub num_subbufs: usize,
    pub line_bytes: usize,
    /// Ring size when it is not subbuf x num (ring v2: SIZE_BURSTS x 128 B).
    pub ring_bytes_override: Option<u64>,
}

impl Geometry {
    pub fn new(unit_bytes: usize, subbuf_bytes: usize, num_subbufs: usize) -> Geometry {
        Geometry {
            unit_bytes,
            subbuf_bytes,
            num_subbufs,
            line_bytes: 32,
            ring_bytes_override: None,
        }
    }
    pub fn subbuf_units(&self) -> u64 {
        (self.subbuf_bytes / self.unit_bytes) as u64
    }
    pub fn ring_units(&self) -> u64 {
        match self.ring_bytes_override {
            Some(b) => b / self.unit_bytes as u64,
            None => self.subbuf_units() * self.num_subbufs as u64,
        }
    }
    pub fn line_units(&self) -> usize {
        (self.line_bytes / self.unit_bytes).max(1)
    }
}

#[derive(Clone, Debug, Default)]
pub struct ChunkMeta {
    /// Chunk sequence number (read order).
    pub seq: u64,
    /// Ring sub-buffer index the chunk came from, if known.
    pub subbuf: Option<u32>,
    /// Byte offset of the chunk in the stream / capture file.
    pub byte_offset: u64,
    /// Monotonic time the reader woke up for this chunk.
    pub wake_ts_ns: Option<u64>,
    /// The chunk starts at a sub-buffer boundary.
    pub boundary: bool,
    /// An enable epoch starts with this chunk (splice window opens).
    pub epoch: bool,
    /// Units the reader knows it skipped before this chunk (ring v2 GAP).
    pub declared_gap_units: u64,
}

#[derive(Clone, Debug)]
pub struct CheckCfg {
    pub bit_err_max_bits: u32,
    pub max_lap_k: u64,
    /// Units after an epoch in which discontinuities are `splice`
    /// (default: one sub-buffer).
    pub epoch_window_units: Option<u64>,
    pub max_records: usize,
    /// Skip ring v2 header/pad words ({0xF1B0|0xF1B1, ...}).
    pub skip_ringv2_meta: bool,
}

impl Default for CheckCfg {
    fn default() -> Self {
        CheckCfg {
            bit_err_max_bits: 3,
            max_lap_k: 256,
            epoch_window_units: None,
            max_records: 32,
            skip_ringv2_meta: false,
        }
    }
}

/// One classified anomaly.
#[derive(Clone, Debug)]
pub struct Anomaly {
    pub class: Class,
    pub offset: u64,
    pub chunk: u64,
    pub subbuf: Option<u32>,
    pub unit: u64,
    pub expected: Option<u64>,
    pub actual: u64,
    pub expected_pos: Option<u64>,
    pub actual_pos: Option<u64>,
    pub delta: Option<i128>,
    pub laps: Option<u64>,
    pub bits: Option<u32>,
    pub len_units: Option<u64>,
    pub t_s: Option<f64>,
    pub note: Option<&'static str>,
}

impl Anomaly {
    pub fn to_json(&self) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("class".into(), json!(self.class.name()));
        m.insert("t".into(), json!(self.t_s.map(|t| (t * 1e6).round() / 1e6)));
        m.insert("offset".into(), json!(self.offset));
        m.insert("chunk".into(), json!(self.chunk));
        m.insert("subbuf".into(), json!(self.subbuf));
        m.insert("unit".into(), json!(self.unit));
        m.insert("expected".into(), json!(self.expected.map(hexn)));
        m.insert("actual".into(), json!(hexn(self.actual)));
        m.insert("expected_pos".into(), json!(self.expected_pos));
        m.insert("actual_pos".into(), json!(self.actual_pos));
        if let Some(d) = self.delta {
            m.insert("delta_units".into(), json!(d as i64));
        }
        if let Some(k) = self.laps {
            m.insert("laps".into(), json!(k));
        }
        if let Some(b) = self.bits {
            m.insert("bits".into(), json!(b));
        }
        if let Some(l) = self.len_units {
            m.insert("len_units".into(), json!(l));
        }
        if let Some(n) = self.note {
            m.insert("detail".into(), json!(n));
        }
        Value::Object(m)
    }
}

/// Counts anomalies, keeps the first N and streams all of them to JSONL.
pub struct Recorder {
    pub counts: [u64; 7],
    pub records: Vec<Value>,
    pub max_records: usize,
    sink: Option<std::io::BufWriter<std::fs::File>>,
    pub sink_path: Option<PathBuf>,
    pub total: u64,
    pub t0_ns: Option<u64>,
    pub sink_error: Option<String>,
}

impl Recorder {
    pub fn new(max_records: usize) -> Recorder {
        Recorder {
            counts: [0; 7],
            records: Vec::new(),
            max_records,
            sink: None,
            sink_path: None,
            total: 0,
            t0_ns: None,
            sink_error: None,
        }
    }

    /// Streams every anomaly to `path` (JSON lines).
    pub fn open_sink(&mut self, path: PathBuf) -> std::io::Result<()> {
        let f = std::fs::File::create(&path)?;
        self.sink = Some(std::io::BufWriter::new(f));
        self.sink_path = Some(path);
        Ok(())
    }

    pub fn push(&mut self, a: &Anomaly) {
        self.counts[a.class as usize] += 1;
        self.total += 1;
        let v = a.to_json();
        if let Some(s) = self.sink.as_mut() {
            if let Err(e) = writeln!(s, "{v}") {
                self.sink_error.get_or_insert(e.to_string());
            }
        }
        if self.records.len() < self.max_records {
            self.records.push(v);
        }
    }

    pub fn flush(&mut self) {
        if let Some(s) = self.sink.as_mut() {
            let _ = s.flush();
        }
    }

    pub fn counts_json(&self) -> Value {
        let mut m = serde_json::Map::new();
        for (i, n) in CLASS_NAMES.iter().enumerate() {
            m.insert(n.to_string(), json!(self.counts[i]));
        }
        Value::Object(m)
    }

    pub fn t_of(&self, meta: &ChunkMeta) -> Option<f64> {
        match (self.t0_ns, meta.wake_ts_ns) {
            (Some(t0), Some(t)) => Some(t.saturating_sub(t0) as f64 * 1e-9),
            _ => None,
        }
    }
}

pub trait StreamCheck {
    fn process(&mut self, data: &[u8], meta: &ChunkMeta);
    fn summary(&mut self) -> Value;
    fn recorder(&mut self) -> &mut Recorder;
    fn unit_bytes(&self) -> usize;
    /// Units lost to forward jumps (gaps + laps), excluding declared gaps.
    fn lost_units(&self) -> u128;
}

#[inline]
fn unit_at(data: &[u8], ub: usize, i: usize) -> u64 {
    let o = i * ub;
    if ub == 8 {
        u64::from_le_bytes(data[o..o + 8].try_into().unwrap())
    } else {
        u32::from_le_bytes(data[o..o + 4].try_into().unwrap()) as u64
    }
}

#[inline]
fn add_mod(a: u64, b: u64, p: u128) -> u64 {
    ((a as u128 + b as u128) % p) as u64
}

#[inline]
fn sub_mod(a: u64, b: u64, p: u128) -> u128 {
    (a as u128 % p + p - b as u128 % p) % p
}

struct TornState {
    resume_e: u64,
    start_g: u64,
}

#[derive(Default, Debug, Clone)]
pub struct Stats {
    pub units: u64,
    pub ok_units: u64,
    pub unsynced_units: u64,
    pub lost_units: u128,
    pub declared_lost_units: u64,
    pub torn_units: u64,
    pub meta_units: u64,
    pub chunks: u64,
    pub resyncs: u64,
    pub headers: u64,
    pub pads: u64,
    pub last_header: Option<u64>,
    /// Header/pad words whose "next data word index" disagreed with the
    /// position of the data word that followed.
    pub meta_index_mismatch: u64,
    pub meta_index_checked: u64,
}

pub struct Checker<M: Model> {
    pub model: M,
    pub geom: Geometry,
    pub cfg: CheckCfg,
    period: u128,
    /// `period` as u64 when < 2^64 (fast-path increment without u128 math).
    pmod: Option<u64>,
    expected: Option<u64>,
    global_base: u64,
    epoch_until: u64,
    torn: Option<TornState>,
    session: Option<u32>,
    primed: bool,
    pub rec: Recorder,
    pub stats: Stats,
}

impl<M: Model> Checker<M> {
    pub fn new(model: M, geom: Geometry, cfg: CheckCfg) -> Checker<M> {
        let period = model.period();
        let max = cfg.max_records;
        Checker {
            model,
            geom,
            cfg,
            period,
            pmod: if period >= models::P64 { None } else { Some(period as u64) },
            expected: None,
            global_base: 0,
            epoch_until: 0,
            torn: None,
            session: None,
            primed: false,
            rec: Recorder::new(max),
            stats: Stats::default(),
        }
    }

    fn lap_multiple(&self, d: u128) -> Option<u64> {
        let ring = self.geom.ring_units() as u128;
        if ring == 0 || d == 0 {
            return None;
        }
        let max_k = self.cfg.max_lap_k.max(1);
        if self.period >= ring * (max_k as u128 + 1) {
            if d % ring == 0 {
                let k = (d / ring) as u64;
                if k >= 1 && k <= max_k {
                    return Some(k);
                }
            }
            return None;
        }
        (1..=max_k).find(|k| (*k as u128 * ring) % self.period == d)
    }

    #[allow(clippy::too_many_arguments)]
    fn anomaly(
        &self,
        class: Class,
        meta: &ChunkMeta,
        i: usize,
        e: Option<u64>,
        u: u64,
        p: Option<u64>,
    ) -> Anomaly {
        Anomaly {
            class,
            offset: meta.byte_offset + (i * self.geom.unit_bytes) as u64,
            chunk: meta.seq,
            subbuf: meta.subbuf,
            unit: i as u64,
            expected: e.map(|e| self.model.value(e)),
            actual: u,
            expected_pos: e,
            actual_pos: p,
            delta: None,
            laps: None,
            bits: None,
            len_units: None,
            t_s: self.rec.t_of(meta),
            note: None,
        }
    }

    /// Next position (e < period); no 128-bit division on the hot path.
    #[inline(always)]
    fn inc(&self, e: u64) -> u64 {
        match self.pmod {
            None => e.wrapping_add(1),
            Some(p) => {
                let n = e + 1;
                if n >= p {
                    0
                } else {
                    n
                }
            }
        }
    }

    fn resync_at(&mut self, u: u64, p: u64) {
        self.model.resync(u, p);
        self.expected = Some(add_mod(p, 1, self.period));
        self.stats.resyncs += 1;
    }

    /// Slow path at unit `i` (mismatch against expected `e`). Returns the
    /// index of the next unit to process.
    fn slow(&mut self, data: &[u8], n: usize, i: usize, u: u64, e: u64, meta: &ChunkMeta) -> usize {
        let ub = self.geom.unit_bytes;
        let g = self.global_base + i as u64;
        let at_boundary = i == 0 && meta.boundary;
        let in_epoch = g < self.epoch_until;
        let p_opt = self.model.position(u);
        let exp_val = self.model.value(e);
        let e1 = add_mod(e, 1, self.period);
        let next_pos = if i + 1 < n {
            Some(self.model.position(unit_at(data, ub, i + 1)))
        } else {
            None
        };
        let next_continues = match next_pos {
            Some(np) => np == Some(e1),
            None => true,
        };

        // Session (tag) change: always a splice.
        if let (Some(s_new), Some(s_cur)) = (self.model.session(u), self.session) {
            if s_new != s_cur {
                let mut a = self.anomaly(Class::Splice, meta, i, Some(e), u, p_opt);
                a.note = Some("tag changed");
                self.rec.push(&a);
                self.session = Some(s_new);
                if let Some(p) = p_opt {
                    self.resync_at(u, p);
                }
                self.torn = None;
                return i + 1;
            }
        }

        let bits = (u ^ exp_val).count_ones();
        let p = match p_opt {
            None => {
                // Undecodable unit: corrupted data.
                let mut a = self.anomaly(Class::BitError, meta, i, Some(e), u, None);
                a.bits = Some(bits);
                a.note = Some("undecodable unit");
                self.rec.push(&a);
                self.model.skip(e);
                self.expected = Some(e1);
                return i + 1;
            }
            Some(p) => p,
        };
        let d = sub_mod(p, e, self.period);
        if d == 0 || (bits <= self.cfg.bit_err_max_bits && next_continues) {
            let mut a = self.anomaly(Class::BitError, meta, i, Some(e), u, Some(p));
            a.bits = Some(bits);
            if d == 0 {
                a.note = Some("position ok, payload mismatch");
            }
            self.rec.push(&a);
            self.model.skip(e);
            self.expected = Some(e1);
            return i + 1;
        }

        // End of a torn region: the stream returns to the original sequence.
        if let Some(t) = &self.torn {
            let since = g - t.start_g;
            let target = add_mod(t.resume_e, since, self.period);
            if p == target {
                self.stats.torn_units += since;
                self.torn = None;
                self.resync_at(u, p);
                return i + 1;
            }
            if since > self.geom.ring_units() {
                self.torn = None;
            }
        }

        let half = self.period / 2;
        if in_epoch {
            let mut a = self.anomaly(Class::Splice, meta, i, Some(e), u, Some(p));
            a.delta = Some(if d <= half { d as i128 } else { -((self.period - d) as i128) });
            self.rec.push(&a);
            self.torn = None;
            self.resync_at(u, p);
            return i + 1;
        }

        // Lap: a whole number of rings skipped at a sub-buffer boundary.
        // Checked before the direction test because for periodic patterns
        // the lap residue can exceed half the period.
        if at_boundary {
            if let Some(k) = self.lap_multiple(d) {
                let mut a = self.anomaly(Class::Lap, meta, i, Some(e), u, Some(p));
                let lost = k as u128 * self.geom.ring_units() as u128;
                a.delta = Some(lost as i128);
                a.laps = Some(k);
                self.rec.push(&a);
                self.stats.lost_units += lost;
                self.torn = None;
                self.resync_at(u, p);
                return i + 1;
            }
        }

        let bd = (self.period - d) % self.period;
        let laps_b = self.lap_multiple(bd);
        if laps_b.is_none() && d <= half {
            // Forward jump that is not a lap.
            let mut a = self.anomaly(Class::WordGap, meta, i, Some(e), u, Some(p));
            a.delta = Some(d as i128);
            if at_boundary {
                a.note = Some("at sub-buffer boundary");
            }
            self.rec.push(&a);
            self.stats.lost_units += d;
            self.torn = None;
            self.resync_at(u, p);
            return i + 1;
        }

        // Backward jump (old-lap data, stale line, repeat).
        let lu = self.geom.line_units();
        if let Some(k) = laps_b {
            let aligned = (meta.byte_offset as usize / ub + i) % lu == 0;
            if aligned && i + lu <= n {
                let old = k as u128 * self.geom.ring_units() as u128 % self.period;
                let line_old = (0..lu).all(|j| {
                    let want = (add_mod(e, j as u64, self.period) as u128 + self.period - old) % self.period;
                    self.model.position(unit_at(data, ub, i + j)) == Some(want as u64)
                });
                let resumes = i + lu == n
                    || self.model.position(unit_at(data, ub, i + lu))
                        == Some(add_mod(e, lu as u64, self.period));
                if line_old && resumes {
                    let mut a = self.anomaly(Class::StaleLine, meta, i, Some(e), u, Some(p));
                    a.laps = Some(k);
                    a.len_units = Some(lu as u64);
                    self.rec.push(&a);
                    self.model.desync();
                    self.expected = Some(add_mod(e, lu as u64, self.period));
                    return i + lu;
                }
            }
            let mut a = self.anomaly(Class::Torn, meta, i, Some(e), u, Some(p));
            a.laps = Some(k);
            a.delta = Some(-(bd as i128));
            a.note = Some("old-lap data");
            self.rec.push(&a);
            self.torn = Some(TornState {
                resume_e: e,
                start_g: g,
            });
            self.resync_at(u, p);
            return i + 1;
        }
        if bd <= self.geom.subbuf_units() as u128 {
            let mut a = self.anomaly(Class::Repeat, meta, i, Some(e), u, Some(p));
            a.delta = Some(-(bd as i128));
            a.len_units = Some(bd as u64);
            self.rec.push(&a);
            self.resync_at(u, p);
            return i + 1;
        }
        let mut a = self.anomaly(Class::Torn, meta, i, Some(e), u, Some(p));
        a.delta = Some(-(bd as i128));
        a.note = Some("backward jump");
        self.rec.push(&a);
        self.torn = Some(TornState {
            resume_e: e,
            start_g: g,
        });
        self.resync_at(u, p);
        i + 1
    }
}

impl<M: Model> StreamCheck for Checker<M> {
    fn process(&mut self, data: &[u8], meta: &ChunkMeta) {
        let ub = self.geom.unit_bytes;
        let n = data.len() / ub;
        if self.rec.t0_ns.is_none() {
            self.rec.t0_ns = meta.wake_ts_ns;
        }
        if !self.primed {
            self.model.prime(data);
            self.primed = true;
        }
        self.stats.chunks += 1;
        if meta.epoch {
            self.expected = None;
            self.torn = None;
            self.session = None;
            self.model.desync();
            self.epoch_until = self.global_base
                + self
                    .cfg
                    .epoch_window_units
                    .unwrap_or_else(|| self.geom.subbuf_units());
        }
        if meta.declared_gap_units > 0 {
            self.stats.declared_lost_units += meta.declared_gap_units;
            if let Some(e) = self.expected {
                self.expected = Some(add_mod(e, meta.declared_gap_units, self.period));
                self.model.desync();
            }
        }
        let skip_meta = self.cfg.skip_ringv2_meta && ub == 8;
        let mut i = 0usize;
        while i < n {
            let u = unit_at(data, ub, i);
            if skip_meta {
                let tag = u >> 48;
                if tag == 0xF1B0 || tag == 0xF1B1 {
                    self.stats.meta_units += 1;
                    if tag == 0xF1B0 {
                        self.stats.headers += 1;
                        self.stats.last_header = Some(u);
                    } else {
                        self.stats.pads += 1;
                    }
                    // Bits [31:0] carry the index of the next data word.
                    let mut j = i + 1;
                    while j < n {
                        let v = unit_at(data, ub, j);
                        let t = v >> 48;
                        if t != 0xF1B0 && t != 0xF1B1 {
                            if let Some(p) = self.model.position(v) {
                                self.stats.meta_index_checked += 1;
                                if (p & 0xFFFF_FFFF) != (u & 0xFFFF_FFFF) {
                                    self.stats.meta_index_mismatch += 1;
                                }
                            }
                            break;
                        }
                        j += 1;
                    }
                    i += 1;
                    continue;
                }
            }
            match self.expected {
                Some(e) => {
                    if self.model.matches(u, e) {
                        self.stats.ok_units += 1;
                        self.expected = Some(self.inc(e));
                        i += 1;
                    } else {
                        i = self.slow(data, n, i, u, e, meta);
                    }
                }
                None => {
                    match self.model.position(u) {
                        Some(p) => {
                            if self.session.is_none() {
                                self.session = self.model.session(u);
                            }
                            self.model.resync(u, p);
                            self.expected = Some(add_mod(p, 1, self.period));
                            self.stats.ok_units += 1;
                        }
                        None => self.stats.unsynced_units += 1,
                    }
                    i += 1;
                }
            }
        }
        self.stats.units += n as u64;
        self.global_base += n as u64;
    }

    fn summary(&mut self) -> Value {
        self.rec.flush();
        let s = &self.stats;
        let ub = self.geom.unit_bytes as u128;
        json!({
            "pattern": self.model.name(),
            "unit_bytes": self.geom.unit_bytes,
            "units_checked": s.units,
            "units_ok": s.ok_units,
            "units_unsynced": s.unsynced_units,
            "chunks": s.chunks,
            "resyncs": s.resyncs,
            "lost_units": s.lost_units as u64,
            "lost_bytes": (s.lost_units * ub) as u64,
            "declared_lost_units": s.declared_lost_units,
            "torn_units": s.torn_units,
            "meta_words": s.meta_units,
            "ringv2_headers": s.headers,
            "ringv2_pads": s.pads,
            "last_ringv2_header": s.last_header.map(hexn),
            "meta_index_checked": s.meta_index_checked,
            "meta_index_mismatch": s.meta_index_mismatch,
            "counts": self.rec.counts_json(),
            "anomalies_total": self.rec.total,
            "model": self.model.info(),
            "synced": self.expected.is_some(),
        })
    }

    fn recorder(&mut self) -> &mut Recorder {
        &mut self.rec
    }

    fn unit_bytes(&self) -> usize {
        self.geom.unit_bytes
    }

    fn lost_units(&self) -> u128 {
        self.stats.lost_units
    }
}

pub const PATTERNS: &[&str] = &["pn0fn", "ramp64", "tagged", "prbs31", "iqramp", "tone"];

/// Unit size of a pattern.
pub fn pattern_unit_bytes(pattern: &str) -> Option<usize> {
    match pattern {
        "ramp64" | "tagged" | "prbs31" => Some(8),
        "pn0fn" | "iqramp" | "tone" => Some(4),
        _ => None,
    }
}

pub struct PatternOpts {
    pub iq_swap: Option<bool>,
    pub tone_tol: u32,
    pub tone_period: usize,
    pub ring_bytes_override: Option<u64>,
}

impl Default for PatternOpts {
    fn default() -> Self {
        PatternOpts {
            iq_swap: None,
            tone_tol: 0,
            tone_period: 32,
            ring_bytes_override: None,
        }
    }
}

/// Builds the checker for a pattern name.
pub fn make_checker(
    pattern: &str,
    subbuf_bytes: usize,
    num_subbufs: usize,
    cfg: CheckCfg,
    opts: &PatternOpts,
) -> Option<Box<dyn StreamCheck>> {
    let ub = pattern_unit_bytes(pattern)?;
    let mut geom = Geometry::new(ub, subbuf_bytes, num_subbufs);
    geom.ring_bytes_override = opts.ring_bytes_override;
    Some(match pattern {
        "ramp64" => Box::new(Checker::new(models::Ramp64, geom, cfg)),
        "tagged" => Box::new(Checker::new(models::Tagged::default(), geom, cfg)),
        "prbs31" => Box::new(Checker::new(models::Prbs31::default(), geom, cfg)),
        "iqramp" => Box::new(Checker::new(models::IqRamp, geom, cfg)),
        "pn0fn" => {
            let mut m = models::Pn0::new();
            if let Some(s) = opts.iq_swap {
                m.swap = s;
                m.swap_auto = false;
            }
            let mut c = Checker::new(m, geom, cfg);
            if opts.iq_swap.is_some() {
                c.primed = true;
            }
            Box::new(c)
        }
        "tone" => Box::new(tone::ToneChecker::new(geom, cfg, opts.tone_period, opts.tone_tol)),
        _ => return None,
    })
}

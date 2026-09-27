//! Synthetic ring captures with injected anomalies (host tests and the
//! `ring synth` subcommand used by the qemu smoke test).
//!
//! The capture is a sequence of sub-buffer chunks as a reader would have
//! copied them. Injection spec (comma separated, sub-buffer index `sb`
//! counts chunks in the capture, `at`/`len` are units):
//!
//! * `gap@sb:at:len`     - `len` units lost from unit `at` onward
//! * `lap@sb:k`          - chunk `sb` is k rings newer (reader lapped)
//! * `torn@sb:at`        - units `at..end` of chunk `sb` are one lap old
//! * `stale@sb:at`       - one 32-byte line at unit `at` is one lap old
//! * `splice@sb:len`     - enable epoch at `sb`: `len` units of the old
//!   session, then a new session restarting at position 0 (new tag)
//! * `biterr@sb:at:mask` - XOR `mask` into unit `at`
//! * `repeat@sb:at:len`  - units `at..at+len` repeat the previous `len` units

use crate::checker::models::{prbs31_next, Pn0, PRBS31_SEED};
use crate::checker::pattern_unit_bytes;
use crate::util::parse_u64;
use serde_json::{json, Value};

#[derive(Clone, Debug, PartialEq)]
pub enum Inject {
    Gap { sb: u64, at: u64, len: u64 },
    Lap { sb: u64, laps: u64 },
    Torn { sb: u64, at: u64 },
    Stale { sb: u64, at: u64 },
    Splice { sb: u64, len: u64 },
    BitErr { sb: u64, at: u64, mask: u64 },
    Repeat { sb: u64, at: u64, len: u64 },
}

impl Inject {
    pub fn class(&self) -> &'static str {
        match self {
            Inject::Gap { .. } => "word_gap",
            Inject::Lap { .. } => "lap",
            Inject::Torn { .. } => "torn",
            Inject::Stale { .. } => "stale_line",
            Inject::Splice { .. } => "splice",
            Inject::BitErr { .. } => "bit_error",
            Inject::Repeat { .. } => "repeat",
        }
    }
    fn sb(&self) -> u64 {
        match self {
            Inject::Gap { sb, .. }
            | Inject::Lap { sb, .. }
            | Inject::Torn { sb, .. }
            | Inject::Stale { sb, .. }
            | Inject::Splice { sb, .. }
            | Inject::BitErr { sb, .. }
            | Inject::Repeat { sb, .. } => *sb,
        }
    }
}

pub fn parse_injections(spec: &str) -> Result<Vec<Inject>, String> {
    let mut out = Vec::new();
    for item in spec.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let (kind, args) = item
            .split_once('@')
            .ok_or_else(|| format!("bad injection '{item}' (want kind@sb:...)"))?;
        let nums: Vec<u64> = args
            .split(':')
            .map(|x| parse_u64(x).ok_or_else(|| format!("bad number in '{item}'")))
            .collect::<Result<_, _>>()?;
        let need = |n: usize| -> Result<(), String> {
            if nums.len() == n {
                Ok(())
            } else {
                Err(format!("'{item}' needs {n} numbers"))
            }
        };
        out.push(match kind {
            "gap" => {
                need(3)?;
                Inject::Gap { sb: nums[0], at: nums[1], len: nums[2] }
            }
            "lap" => {
                need(2)?;
                Inject::Lap { sb: nums[0], laps: nums[1] }
            }
            "torn" => {
                need(2)?;
                Inject::Torn { sb: nums[0], at: nums[1] }
            }
            "stale" => {
                need(2)?;
                Inject::Stale { sb: nums[0], at: nums[1] }
            }
            "splice" => {
                need(2)?;
                Inject::Splice { sb: nums[0], len: nums[1] }
            }
            "biterr" => {
                need(3)?;
                Inject::BitErr { sb: nums[0], at: nums[1], mask: nums[2] }
            }
            "repeat" => {
                need(3)?;
                Inject::Repeat { sb: nums[0], at: nums[1], len: nums[2] }
            }
            _ => return Err(format!("unknown injection kind '{kind}'")),
        });
    }
    Ok(out)
}

/// A reasonable default injection set for a capture of `subbufs` chunks.
pub fn default_injections(pattern: &str, subbufs: u64, ups: u64) -> Vec<Inject> {
    let line = if pattern_unit_bytes(pattern) == Some(8) { 4 } else { 8 };
    let mut v = vec![
        Inject::Gap { sb: 2, at: ups / 3, len: 7 },
        Inject::Lap { sb: 4, laps: 1 },
        Inject::Torn { sb: 6, at: ups / 2 },
        Inject::Stale { sb: 8, at: line * 10 },
        Inject::Splice { sb: 10, len: ups / 4 },
        Inject::BitErr { sb: 12, at: 33, mask: 0x4 },
        Inject::Repeat { sb: 14, at: ups / 2, len: 24 },
    ];
    if pattern == "tone" {
        // Only phase breaks and bit errors are visible in a tone.
        v = vec![
            Inject::Gap { sb: 2, at: ups / 3, len: 7 },
            Inject::BitErr { sb: 12, at: 33, mask: 0x4 },
        ];
    }
    v.retain(|i| i.sb() < subbufs);
    v
}

pub struct SynthSpec {
    pub pattern: String,
    pub subbuf_bytes: usize,
    pub ring_subbufs: usize,
    pub subbufs: u64,
    pub injections: Vec<Inject>,
    pub tone_k: u32,
    pub tone_amp: f64,
}

pub struct SynthOut {
    pub data: Vec<u8>,
    /// Per-chunk sidecar records (same shape as `ring capture`).
    pub sidecar: Vec<Value>,
}

struct Gen {
    pattern: String,
    tag: u64,
    pn0: Option<Pn0>,
    prbs: Vec<u32>,
    tone: Vec<u32>,
}

impl Gen {
    fn prbs_at(&mut self, pos: u64) -> u32 {
        while self.prbs.len() as u64 <= pos {
            let next = match self.prbs.last() {
                None => crate::checker::models::prbs31_step32_ref(PRBS31_SEED).0,
                Some(w) => prbs31_next(*w),
            };
            self.prbs.push(next);
        }
        self.prbs[pos as usize]
    }

    fn unit(&mut self, pos: u64) -> u64 {
        match self.pattern.as_str() {
            "ramp64" => pos,
            "tagged" => (self.tag << 60) | (pos & ((1u64 << 56) - 1)),
            "prbs31" => ((pos & 0xFFFF_FFFF) << 32) | self.prbs_at(pos) as u64,
            "iqramp" => pos & 0xFFFF_FFFF,
            "pn0fn" => {
                let m = self.pn0.get_or_insert_with(Pn0::new).sample_at(pos);
                let i = crate::util::sext12(m & 0xFFF) as u32 & 0xFFFF;
                let q = crate::util::sext12((m >> 16) & 0xFFF) as u32 & 0xFFFF;
                (i | (q << 16)) as u64
            }
            "tone" => self.tone[(pos % self.tone.len() as u64) as usize] as u64,
            _ => 0,
        }
    }
}

pub fn tone_table(k: u32, amp: f64, period: usize) -> Vec<u32> {
    (0..period)
        .map(|n| {
            let ph = 2.0 * std::f64::consts::PI * k as f64 * n as f64 / period as f64;
            let i = (amp * ph.cos()).round() as i32;
            let q = (amp * ph.sin()).round() as i32;
            ((i as u32) & 0xFFFF) | (((q as u32) & 0xFFFF) << 16)
        })
        .collect()
}

pub fn generate(spec: &SynthSpec) -> Result<SynthOut, String> {
    let ub = pattern_unit_bytes(&spec.pattern).ok_or_else(|| format!("unknown pattern {}", spec.pattern))?;
    let ups = (spec.subbuf_bytes / ub) as u64;
    let ring = ups * spec.ring_subbufs as u64;
    let line = (32 / ub) as u64;
    let mut g = Gen {
        pattern: spec.pattern.clone(),
        tag: 0x5,
        pn0: None,
        prbs: Vec::new(),
        tone: tone_table(spec.tone_k, spec.tone_amp, 32),
    };
    // Start far enough in that one-lap-old positions stay >= 0.
    let mut next_pos: u64 = if spec.pattern == "prbs31" { 2 * ring } else { 10 * ring + 12345 };
    let mut data = Vec::with_capacity((spec.subbufs * ups) as usize * ub);
    let mut sidecar = Vec::new();
    let period_ns = 1_000_000u64;
    for sb in 0..spec.subbufs {
        let inj: Vec<&Inject> = spec.injections.iter().filter(|i| i.sb() == sb).collect();
        let mut epoch = false;
        let mut chunk: Vec<u64> = Vec::with_capacity(ups as usize);
        // Chunk-level modifications.
        for i in &inj {
            if let Inject::Lap { laps, .. } = i {
                next_pos += laps * ring;
            }
        }
        let splice = inj.iter().find_map(|i| match i {
            Inject::Splice { len, .. } => Some(*len),
            _ => None,
        });
        if let Some(len) = splice {
            epoch = true;
            // Old session: continues the old positions with the old tag.
            for _ in 0..len.min(ups) {
                chunk.push(g.unit(next_pos));
                next_pos += 1;
            }
            // New session: restart at 0 with a new tag.
            g.tag = (g.tag + 1) & 0xF;
            next_pos = if spec.pattern == "prbs31" { 0 } else { 3 * ring };
            // Old-lap references for the new session must stay valid.
            if spec.pattern == "prbs31" {
                next_pos = 2 * ring + 777;
            }
        }
        while (chunk.len() as u64) < ups {
            let j = chunk.len() as u64;
            let mut handled = false;
            for i in &inj {
                match i {
                    Inject::Gap { at, len, .. } if *at == j => {
                        next_pos += len;
                    }
                    Inject::Repeat { at, len, .. } if *at == j && j >= *len => {
                        let start = chunk.len() - *len as usize;
                        let dup: Vec<u64> = chunk[start..].to_vec();
                        for v in dup {
                            if (chunk.len() as u64) < ups {
                                chunk.push(v);
                            }
                        }
                        handled = true;
                    }
                    Inject::Torn { at, .. } if *at == j => {
                        let mut p = next_pos - ring;
                        while (chunk.len() as u64) < ups {
                            chunk.push(g.unit(p));
                            p += 1;
                            next_pos += 1;
                        }
                        handled = true;
                    }
                    Inject::Stale { at, .. } if *at == j => {
                        for k in 0..line {
                            if (chunk.len() as u64) < ups {
                                chunk.push(g.unit(next_pos + k - ring));
                            }
                        }
                        next_pos += line;
                        handled = true;
                    }
                    _ => {}
                }
            }
            if handled {
                continue;
            }
            chunk.push(g.unit(next_pos));
            next_pos += 1;
        }
        for i in &inj {
            if let Inject::BitErr { at, mask, .. } = i {
                if let Some(v) = chunk.get_mut(*at as usize) {
                    *v ^= mask;
                }
            }
        }
        for v in &chunk {
            if ub == 8 {
                data.extend_from_slice(&v.to_le_bytes());
            } else {
                data.extend_from_slice(&(*v as u32).to_le_bytes());
            }
        }
        let slot = sb % spec.ring_subbufs as u64;
        sidecar.push(json!({
            "index": slot, "seq": sb, "wake_ts_ns": 1_000_000_000u64 + sb * period_ns,
            "last_buffer": slot, "backlog": 0, "epoch": epoch,
            "offset": sb * spec.subbuf_bytes as u64,
        }));
    }
    Ok(SynthOut { data, sidecar })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checker::{make_checker, CheckCfg, ChunkMeta, PatternOpts};

    fn run(pattern: &str, subbuf: usize, ring_n: usize, subbufs: u64, inj: Vec<Inject>) -> serde_json::Value {
        let spec = SynthSpec {
            pattern: pattern.into(),
            subbuf_bytes: subbuf,
            ring_subbufs: ring_n,
            subbufs,
            injections: inj,
            tone_k: 3,
            tone_amp: 1500.0,
        };
        let out = generate(&spec).unwrap();
        let mut c = make_checker(pattern, subbuf, ring_n, CheckCfg::default(), &PatternOpts::default()).unwrap();
        for (k, chunk) in out.data.chunks(subbuf).enumerate() {
            let sc = &out.sidecar[k];
            let meta = ChunkMeta {
                seq: k as u64,
                subbuf: Some(sc["index"].as_u64().unwrap() as u32),
                byte_offset: (k * subbuf) as u64,
                wake_ts_ns: sc["wake_ts_ns"].as_u64(),
                boundary: true,
                epoch: sc["epoch"].as_bool().unwrap(),
                declared_gap_units: 0,
            };
            c.process(chunk, &meta);
        }
        c.summary()
    }

    fn assert_counts(s: &serde_json::Value, want: &[(&str, u64)]) {
        for name in crate::checker::CLASS_NAMES {
            let exp = want.iter().find(|(n, _)| *n == name).map(|x| x.1).unwrap_or(0);
            assert_eq!(s["counts"][name].as_u64().unwrap(), exp, "{name}: {}", s);
        }
    }

    #[test]
    fn clean_streams_have_no_anomalies() {
        for p in ["ramp64", "tagged", "prbs31", "iqramp", "pn0fn", "tone"] {
            let s = run(p, 8192, 4, 12, vec![]);
            assert_counts(&s, &[]);
            assert!(s["synced"].as_bool().unwrap(), "{p}");
        }
    }

    #[test]
    fn every_class_detected_word_patterns() {
        for p in ["ramp64", "tagged", "prbs31"] {
            let ups = 8192 / 8;
            let inj = default_injections(p, 16, ups);
            let s = run(p, 8192, 4, 16, inj);
            assert_counts(
                &s,
                &[("word_gap", 1), ("lap", 1), ("torn", 1), ("stale_line", 1), ("splice", 1), ("bit_error", 1), ("repeat", 1)],
            );
            assert_eq!(s["lost_units"].as_u64().unwrap(), 7 + 4 * ups as u64, "{p}");
        }
    }

    #[test]
    fn every_class_detected_sample_patterns() {
        // pn0fn: 256 KiB sub-buffers x 16 -> ring = 2^20 samples = 16 mod 65535
        // (residue chosen so the injected gap/repeat lengths are not lap residues).
        for (p, sub, n) in [("iqramp", 8192usize, 4usize), ("pn0fn", 262144, 16)] {
            let ups = (sub / 4) as u64;
            let inj = default_injections(p, 16, ups);
            let s = run(p, sub, n, 16, inj);
            assert_counts(
                &s,
                &[("word_gap", 1), ("lap", 1), ("torn", 1), ("stale_line", 1), ("splice", 1), ("bit_error", 1), ("repeat", 1)],
            );
        }
    }

    #[test]
    fn tone_breaks() {
        let ups = 8192 / 4;
        let s = run("tone", 8192, 4, 16, default_injections("tone", 16, ups));
        assert_counts(&s, &[("word_gap", 1), ("bit_error", 1)]);
    }

    #[test]
    fn multi_lap_and_boundary_gap() {
        let ups = 1024u64;
        let s = run(
            "ramp64",
            8192,
            4,
            8,
            vec![Inject::Lap { sb: 3, laps: 3 }, Inject::Gap { sb: 5, at: 0, len: 100 }],
        );
        assert_counts(&s, &[("lap", 1), ("word_gap", 1)]);
        assert_eq!(s["lost_units"].as_u64().unwrap(), 3 * 4 * ups + 100);
    }

    #[test]
    fn injection_parse() {
        let v = parse_injections("gap@3:100:5,lap@5:1,torn@7:1000,stale@9:64,splice@11:500,biterr@13:10:0x4,repeat@15:200:16").unwrap();
        assert_eq!(v.len(), 7);
        assert_eq!(v[6], Inject::Repeat { sb: 15, at: 200, len: 16 });
        assert!(parse_injections("gap@1:2").is_err());
        assert!(parse_injections("bogus@1").is_err());
    }
}

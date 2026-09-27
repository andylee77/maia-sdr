//! Test-only model of `DibitPacker` + `DmaStreamRingWrite` register
//! behaviour (change 054). Shared by `dibit_ring_tests.rs` and
//! `app/dibit_airtime_tests.rs`.
//!
//! The model produces dibits at `rate` from `t0_us` (minus paused
//! intervals), packs 32 per word, 16 words per burst, keeps the AW
//! pointer exactly two bursts ahead of the burst being filled (the
//! steady state of `awvalid = enable & ~two_outstanding_bursts`), and
//! completes a 4 KiB sub-buffer (B-channel `last_buffer`) when its last
//! burst's final beat is sent.

use super::{RingGeometry, RingSnapshot, BURST_BYTES, DIBITS_PER_BURST};

#[derive(Debug, Clone)]
pub struct WriterModel {
    pub geom: RingGeometry,
    pub base: u32,
    /// Time (µs) at which dibit number `initial_dibits` was produced.
    pub t0_us: f64,
    /// Dibits produced before `t0_us` (phase / lap offset).
    pub initial_dibits: u64,
    /// Dibits per µs (4800 Hz nominal = 0.0048).
    pub rate_per_us: f64,
    /// Paused intervals `[start, end)` in µs (chain disabled).
    pub pauses: Vec<(f64, f64)>,
}

impl WriterModel {
    pub fn new(initial_dibits: u64, t0_us: f64) -> Self {
        WriterModel {
            geom: RingGeometry::P25_DIBIT,
            base: 0x1B00_0000,
            t0_us,
            initial_dibits,
            rate_per_us: 4800.0e-6,
            pauses: Vec::new(),
        }
    }

    /// Running microseconds between `t0_us` and `t_us`.
    fn running_us(&self, t_us: f64) -> f64 {
        if t_us <= self.t0_us {
            return 0.0;
        }
        let mut run = t_us - self.t0_us;
        for &(a, b) in &self.pauses {
            let a = a.max(self.t0_us);
            let b = b.min(t_us);
            if b > a {
                run -= b - a;
            }
        }
        run.max(0.0)
    }

    pub fn enabled(&self, t_us: f64) -> bool {
        !self.pauses.iter().any(|&(a, b)| t_us >= a && t_us < b)
    }

    /// Dibits produced by `t_us` (= index of the next dibit).
    pub fn produced(&self, t_us: f64) -> u64 {
        self.initial_dibits + (self.running_us(t_us) * self.rate_per_us).floor() as u64
    }

    /// Production time of dibit number `n` (n ≥ initial_dibits).
    pub fn time_of(&self, n: u64) -> f64 {
        let need_us = (n.saturating_sub(self.initial_dibits)) as f64 / self.rate_per_us;
        // Walk forward through pauses.
        let mut t = self.t0_us;
        let mut remaining = need_us;
        let mut pauses = self.pauses.clone();
        pauses.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        for (a, b) in pauses {
            if b <= t {
                continue;
            }
            let a = a.max(t);
            if t + remaining <= a {
                break;
            }
            remaining -= a - t;
            t = b;
        }
        t + remaining
    }

    /// Bursts whose final beat has been sent.
    pub fn bursts_done(&self, t_us: f64) -> u64 {
        self.produced(t_us) / DIBITS_PER_BURST
    }

    /// AW pointer (bursts issued, absolute).
    pub fn aw_count(&self, t_us: f64) -> u64 {
        self.bursts_done(t_us) + 2
    }

    /// Absolute next-address in bytes (model coordinate).
    pub fn abs_next_bytes(&self, t_us: f64) -> u64 {
        self.aw_count(t_us) * BURST_BYTES
    }

    pub fn next_address(&self, t_us: f64) -> u32 {
        let ring = self.geom.ring_bytes();
        self.base + (self.abs_next_bytes(t_us) % ring) as u32
    }

    pub fn last_buffer(&self, t_us: f64) -> u8 {
        let bursts_per_sb = self.geom.sub_buffer_bytes / BURST_BYTES;
        let completed = (self.bursts_done(t_us) / bursts_per_sb) as i64;
        (completed - 1).rem_euclid(self.geom.num_sub_buffers as i64) as u8
    }

    /// Bytes fully landed in DDR by `t_us` (whole bursts).
    pub fn landed_bytes(&self, t_us: f64) -> u64 {
        self.bursts_done(t_us) * BURST_BYTES
    }

    pub fn snapshot(&self, t_us: u64) -> RingSnapshot {
        let t = t_us as f64;
        RingSnapshot {
            next_address: self.next_address(t),
            last_buffer: self.last_buffer(t),
            chain_enabled: self.enabled(t),
            t_us,
        }
    }
}

/// Small deterministic PRNG (xorshift64*) for poll jitter.
pub struct Rng(pub u64);

impl Rng {
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

//! The core's DMA rings, as the hardware presents them.
//!
//! The gateware writes each ring in 128-byte AXI bursts, the AW channel running at most two
//! bursts ahead of the data; `*_next` reads the address of the next burst to be issued, and
//! `last_buffer` the most recently completed sub-buffer. A dibit ring holds four dibits a byte at
//! 4800 dibits/s. The position tracking built on this (the reader, the production clock) lives
//! in `radio::streams`.

/// Bytes per AXI burst (16 beats of 8 bytes).
pub const BURST_BYTES: u64 = 128;
/// The burst being filled starts this far below the next address.
pub const LEAD_BYTES: u64 = 2 * BURST_BYTES;
pub const DIBITS_PER_BYTE: u64 = 4;
pub const NOMINAL_DIBIT_RATE_HZ: f64 = 4800.0;
pub const NOMINAL_BYTE_RATE_HZ: f64 = NOMINAL_DIBIT_RATE_HZ / DIBITS_PER_BYTE as f64;

static MONO_ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Process-wide monotonic microseconds: the time base of ring snapshots.
pub fn mono_us() -> u64 {
    MONO_ORIGIN.get_or_init(std::time::Instant::now).elapsed().as_micros() as u64
}

/// The instant of a `mono_us` value.
pub fn mono_instant(us: u64) -> std::time::Instant {
    *MONO_ORIGIN.get_or_init(std::time::Instant::now) + std::time::Duration::from_micros(us)
}

/// Shape of one ring, from maia-kmod's sysfs attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingGeometry {
    pub sub_buffer_bytes: u64,
    pub num_sub_buffers: u64,
}

impl RingGeometry {
    /// Both P25 dibit rings: 8 × 4 KiB.
    #[cfg(test)]
    pub const P25_DIBIT: RingGeometry = RingGeometry { sub_buffer_bytes: 4096, num_sub_buffers: 8 };

    pub fn ring_bytes(&self) -> u64 {
        self.sub_buffer_bytes * self.num_sub_buffers
    }

    /// A power-of-two ring of whole bursts with at least two sub-buffers (the gateware's rule).
    pub fn is_valid(&self) -> bool {
        self.num_sub_buffers >= 2
            && self.sub_buffer_bytes >= BURST_BYTES
            && self.sub_buffer_bytes % BURST_BYTES == 0
            && self.ring_bytes().is_power_of_two()
    }

    /// Ring offset of a bus address (the ring base is aligned to the ring size).
    pub fn offset_of(&self, address: u32) -> u64 {
        address as u64 & (self.ring_bytes() - 1)
    }

    pub fn base_of(&self, address: u32) -> u32 {
        (address as u64 & !(self.ring_bytes() - 1)) as u32
    }

    pub fn sub_buffer_of(&self, offset: u64) -> u64 {
        (offset % self.ring_bytes()) / self.sub_buffer_bytes
    }

    /// Seconds for the writer to lap the ring at the nominal rate.
    pub fn lap_budget_secs(&self) -> f64 {
        self.ring_bytes() as f64 / NOMINAL_BYTE_RATE_HZ
    }
}

/// `last_buffer` must sit just behind the sub-buffer the next burst targets: an independent
/// check of the lap phase.
pub fn phase_consistent(geom: &RingGeometry, next_offset: u64, last_buffer: u8) -> bool {
    let n = geom.num_sub_buffers;
    let next_sub = geom.sub_buffer_of(next_offset);
    let completed_next = (last_buffer as u64 + 1) % n;
    completed_next == next_sub || completed_next == (next_sub + n - 1) % n
}

/// One reading of a dibit ring's registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingSnapshot {
    pub next_address: u32,
    pub last_buffer: u8,
    /// The chain's LSM enable, read in the same snapshot.
    pub chain_enabled: bool,
    pub t_us: u64,
}

/// `len` bytes at `offset` of sub-buffer `sub_buffer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyPiece {
    pub sub_buffer: usize,
    pub offset: usize,
    pub len: usize,
}

/// The absolute byte range `[start, end)` as pieces per sub-buffer, in ring order (at most one
/// lap).
pub fn copy_plan(geom: &RingGeometry, start: u64, end: u64) -> Vec<CopyPiece> {
    let mut out = Vec::new();
    let ring = geom.ring_bytes();
    let end = end.min(start + ring);
    let mut cur = start;
    while cur < end {
        let off = cur % ring;
        let in_sub = off % geom.sub_buffer_bytes;
        let len = (geom.sub_buffer_bytes - in_sub).min(end - cur);
        out.push(CopyPiece {
            sub_buffer: (off / geom.sub_buffer_bytes) as usize,
            offset: in_sub as usize,
            len: len as usize,
        });
        cur += len;
    }
    out
}

/// Whole sub-buffers completed since the last call: the indices after `last_seen` up to
/// `current` (a sub-buffer index register). The first call only records where the ring is.
pub fn completed_since(num_buffers: usize, last_seen: &mut Option<u32>, current: u32) -> Vec<usize> {
    if num_buffers == 0 {
        return Vec::new();
    }
    let mask = (num_buffers - 1) as u32;
    let current_idx = (current & mask) as usize;
    let Some(prev) = last_seen.replace(current) else {
        return Vec::new();
    };
    let prev_idx = (prev & mask) as usize;
    let mut out = Vec::new();
    let mut idx = prev_idx;
    while idx != current_idx {
        idx = (idx + 1) % num_buffers;
        out.push(idx);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const RING: u64 = 32768;

    #[test]
    fn geometry_of_the_p25_dibit_rings() {
        let g = RingGeometry::P25_DIBIT;
        assert!(g.is_valid());
        assert_eq!(g.ring_bytes(), RING);
        assert_eq!(g.offset_of(0x1B00_0100), 256);
        assert_eq!(g.offset_of(0x1B00_7F80), RING - 128);
        assert_eq!(g.base_of(0x1B00_7F80), 0x1B00_0000);
        assert_eq!((g.sub_buffer_of(4095), g.sub_buffer_of(4096)), (0, 1));
        assert!((g.lap_budget_secs() - 27.306).abs() < 0.01);
        assert!(!RingGeometry { sub_buffer_bytes: 4000, num_sub_buffers: 8 }.is_valid());
    }

    #[test]
    fn lap_phase_check() {
        let g = RingGeometry::P25_DIBIT;
        assert!(phase_consistent(&g, 256, 7), "fresh start: nothing completed");
        let off = 3 * 4096 + 512;
        assert!(phase_consistent(&g, off, 2) && phase_consistent(&g, off, 1));
        assert!(!phase_consistent(&g, off, 5) && !phase_consistent(&g, off, 3));
        assert!(phase_consistent(&g, 128, 6) && phase_consistent(&g, 128, 7));
        assert!(!phase_consistent(&g, 128, 0));
    }

    #[test]
    fn copies_split_at_sub_buffers_and_the_ring_end() {
        let g = RingGeometry::P25_DIBIT;
        assert_eq!(copy_plan(&g, RING + 256, RING + 384), vec![CopyPiece { sub_buffer: 0, offset: 256, len: 128 }]);
        assert!(copy_plan(&g, 10, 10).is_empty() && copy_plan(&g, 10, 5).is_empty());
        assert_eq!(
            copy_plan(&g, 4096 - 128, 4096 + 128),
            vec![CopyPiece { sub_buffer: 0, offset: 4096 - 128, len: 128 }, CopyPiece { sub_buffer: 1, offset: 0, len: 128 }]
        );
        let s = 5 * RING - 256;
        assert_eq!(
            copy_plan(&g, s, s + 512),
            vec![CopyPiece { sub_buffer: 7, offset: 4096 - 256, len: 256 }, CopyPiece { sub_buffer: 0, offset: 0, len: 256 }]
        );
    }

    #[test]
    fn completed_sub_buffers_follow_the_index_register() {
        let mut last = None;
        assert!(completed_since(8, &mut last, 5).is_empty(), "the first call only anchors");
        assert!(completed_since(8, &mut last, 5).is_empty());
        assert_eq!(completed_since(8, &mut last, 7), vec![6, 7]);
        assert_eq!(completed_since(8, &mut last, 1), vec![0, 1]);
        assert!(completed_since(0, &mut last, 3).is_empty());
    }
}

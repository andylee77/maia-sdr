//! The radio core in the FPGA: its registers (through UIO), its DMA rings (through maia-kmod)
//! and its interrupt.
//!
//! The core has lanes, each a DDC (to 50 kSPS) whose IQ it cuts into tagged packets
//! (`packet`); one ring carries every lane's packets. Lane 0 is the control channel's, the
//! others the traffic lanes. Beside them: a wideband spectrometer, whose frames carry a header
//! (`frame`), a raw IQ capture ring, which starts afresh at each enable with the sample index of
//! its first sample and can stop itself after a trigger, and (core 2.1) an 8-VSB receiver
//! (`vsb`) whose codewords and snapshots come through the lane ring.

pub mod frame;
#[cfg(target_os = "linux")]
pub mod irq;
pub mod lane;
pub mod packet;
pub mod regs;
pub mod vsb;

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

pub use frame::FrameHeader;
pub use lane::LaneDdc;
pub use packet::{Header, Parsed};
pub use regs::nco_to_freq;
pub use vsb::VsbStatus;

use regs::{LaneBank, Registers, LANE_BANKS};

/// Expected `product_id` ("rad1").
pub const PRODUCT_ID: u32 = 0x7261_6431;
/// The major version of the core's map this scanner reads.
pub const MAJOR_VERSION: u8 = 2;
/// The control channel's lane.
pub const CONTROL_LANE: usize = 0;
/// Bytes of one capture sample (I and Q, 16 bits each).
pub const CAPTURE_SAMPLE_BYTES: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Version {
    pub major: u8,
    pub minor: u8,
    pub bugfix: u8,
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.bugfix)
    }
}

/// What the core says it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Identity {
    pub version: Version,
    pub platform: u8,
    /// Lanes, the control channel's included.
    pub lanes: usize,
    /// The 8-VSB receiver is in the core.
    pub vsb: bool,
}

/// The lane ring's registers, in one reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct RingStatus {
    pub enabled: bool,
    pub last_buffer: u16,
    pub next_address: u32,
}

/// The capture's registers, in one reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct CaptureStatus {
    pub enabled: bool,
    /// Sub-buffers completed since the enable.
    pub buffers: u32,
    /// The capture stopped itself after a trigger.
    pub stopped: bool,
    /// The AD9361 sample index of the capture's first sample (valid once a sub-buffer is done).
    pub start_index: u64,
    /// Words were lost since the capture started: its samples are no longer contiguous.
    pub overflow: bool,
}

/// What one reading of the capture ring found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CaptureRead {
    /// Sub-buffers handed over.
    pub read: u32,
    /// Sub-buffers the ring overwrote before or while they were read (the reader fell behind).
    pub lapped: u32,
}

pub struct RadioCore {
    regs: Registers,
    identity: Identity,
    /// The capture's overflow, kept across readings (its register clears on read).
    capture_overflow: AtomicBool,
    /// Sub-buffers of the capture read since its enable.
    capture_read: u32,
    #[cfg(target_os = "linux")]
    dma: dma::Dma,
}

impl RadioCore {
    pub fn identity(&self) -> Identity {
        self.identity
    }

    /// A lane's DDC and packets; `None` past the core's lanes.
    pub fn lane(&self, n: usize) -> Option<LaneDdc<'_>> {
        (n < self.identity.lanes).then(|| LaneDdc { regs: &self.regs, bank: LaneBank(n) })
    }

    pub fn set_ring(&self, on: bool) {
        self.regs.lanes_ring_control().modify(|_, w| w.enable().bit(on));
    }

    pub fn ring_status(&self) -> RingStatus {
        RingStatus {
            enabled: self.regs.lanes_ring_control().read().enable().bit(),
            last_buffer: self.regs.lanes_ring_status().read().last_buffer().bits(),
            next_address: self.regs.lanes_ring_next_address().read().next_address().bits(),
        }
    }

    /// The AD9361 samples since the core's reset (the clock of the packets' sample index).
    pub fn sample_count(&self) -> u64 {
        // Reading the low word latches the high one.
        let lo = self.regs.sample_count_lo().read().count().bits();
        let hi = self.regs.sample_count_hi().read().count().bits();
        (hi as u64) << 32 | lo as u64
    }

    /// AD9361 samples at full scale since the core's reset.
    pub fn adc_clips(&self) -> u32 {
        self.regs.adc_clips().read().count().bits()
    }

    pub fn set_spectrometer(&self, on: bool) {
        self.regs.spec_control().modify(|_, w| w.spec_enable().bit(on));
    }

    /// FFT frames averaged into one spectrum (10 bits), from the next frame.
    pub fn set_spectrometer_integrations(&self, n: u16) {
        self.regs.spec_control().modify(|_, w| unsafe { w.spec_num_integrations().bits(n & 0x3FF) });
    }

    /// Peak hold instead of averaged power, or each frame in the other mode from the frame
    /// before (`alternate`), from the next frame. Each frame's header says its mode.
    pub fn set_spectrum_mode(&self, peak: bool, alternate: bool) {
        self.regs.spec_control().modify(|_, w| w.spec_peak_detect().bit(peak).spec_alternate().bit(alternate));
    }

    /// Start a capture: the ring starts afresh at its first sub-buffer, with the sample index
    /// of its first sample (the enable's rise; a capture running is stopped first).
    pub fn start_capture(&mut self) {
        self.regs.capture_control().modify(|_, w| w.enable().clear_bit());
        self.regs.capture_control().modify(|_, w| w.enable().set_bit());
        // The overflow of the capture before, cleared by the read.
        let _ = self.regs.capture_status().read();
        self.capture_overflow.store(false, Ordering::Relaxed);
        self.capture_read = 0;
    }

    /// Let the capture finish the sub-buffer it is writing and `stop_after` more, then stop.
    pub fn trigger_capture(&self, stop_after: u8) {
        self.regs.capture_control().modify(|_, w| unsafe { w.trigger().set_bit().stop_after().bits(stop_after & 0xF) });
    }

    pub fn stop_capture(&self) {
        self.regs.capture_control().modify(|_, w| w.enable().clear_bit());
    }

    pub fn capture_status(&self) -> CaptureStatus {
        let status = self.regs.capture_status().read();
        let overflow = self.capture_overflow.fetch_or(status.overflow().bit(), Ordering::Relaxed) | status.overflow().bit();
        // Reading the low word latches the high one.
        let lo = self.regs.capture_start_lo().read().index().bits();
        let hi = self.regs.capture_start_hi().read().index().bits();
        CaptureStatus {
            enabled: self.regs.capture_control().read().enable().bit(),
            buffers: self.regs.capture_buffers().read().count().bits(),
            stopped: status.stopped().bit(),
            start_index: (hi as u64) << 32 | lo as u64,
            overflow,
        }
    }

    /// A core with its registers in plain memory and no rings (host tests).
    #[cfg(all(test, not(target_os = "linux")))]
    pub fn in_memory(lanes: usize) -> RadioCore {
        let version = Version { major: MAJOR_VERSION, minor: 1, bugfix: 0 };
        RadioCore {
            regs: Registers::in_memory(),
            identity: Identity { version, platform: 0, lanes, vsb: true },
            capture_overflow: AtomicBool::new(false),
            capture_read: 0,
        }
    }
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

/// The capture's sub-buffers to read: those after `read` up to `buffers` (completed since the
/// enable), less the ones the ring may have overwritten. Sub-buffer `buffers` is being written,
/// and while its last write responses are on their way the next one may have begun, so the
/// newest `num_buffers - 2` completed are whole. Returns the range and how many were lost.
pub fn capture_pending(num_buffers: u32, read: u32, buffers: u32) -> (std::ops::Range<u32>, u32) {
    let from = read.max(capture_oldest_whole(num_buffers, buffers));
    (from..buffers.max(from), from - read.min(from))
}

/// The oldest sub-buffer the writer cannot have reached when `buffers` are complete.
fn capture_oldest_whole(num_buffers: u32, buffers: u32) -> u32 {
    (buffers + 2).saturating_sub(num_buffers)
}

/// A frame the spectrometer wrote, checked.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    pub header: FrameHeader,
    /// The frame as written (header bins included).
    pub bytes: &'a [u8],
}

#[cfg(target_os = "linux")]
mod dma {
    use anyhow::{Context, Result};

    use super::frame::{self, FRAME_BYTES};
    use super::packet::{self, Fault, Parsed, PACKET_BYTES};
    use super::{capture_pending, completed_since, CaptureRead, Frame, RadioCore, CAPTURE_SAMPLE_BYTES};
    use crate::hardware::mmio::RxBuffer;

    pub(super) struct Dma {
        lanes: RxBuffer,
        lanes_last: Option<u32>,
        spectrum: RxBuffer,
        spectrum_last: Option<u32>,
        capture: RxBuffer,
    }

    impl Dma {
        pub(super) async fn open() -> Result<Dma> {
            let lanes = RxBuffer::open("p25-lanes").await.context("the lane ring")?;
            anyhow::ensure!(
                lanes.buffer_size() % PACKET_BYTES == 0,
                "lane ring sub-buffers of {} bytes do not hold whole packets",
                lanes.buffer_size()
            );
            let spectrum = RxBuffer::open("p25-wideband-spec").await.context("the spectrum ring")?;
            anyhow::ensure!(
                spectrum.buffer_size() == FRAME_BYTES,
                "spectrum ring sub-buffers of {} bytes, frames are {FRAME_BYTES}",
                spectrum.buffer_size()
            );
            let capture = RxBuffer::open("p25-wideband-iq").await.context("the capture ring")?;
            Ok(Dma { lanes, lanes_last: None, spectrum, spectrum_last: None, capture })
        }
    }

    impl RadioCore {
        /// Every packet in the lane ring's sub-buffers completed since the last call (cache
        /// invalidated), in ring order: the lanes' and the 8-VSB block's. The first call only
        /// finds the ring's position.
        pub fn read_packets(&mut self, mut each: impl FnMut(Result<Parsed<'_>, Fault>)) {
            let current = self.regs.lanes_ring_status().read().last_buffer().bits() as u32;
            let d = &mut self.dma;
            for idx in completed_since(d.lanes.num_buffers(), &mut d.lanes_last, current) {
                if let Err(e) = d.lanes.cache_invalidate(idx) {
                    tracing::warn!("lane ring sub-buffer {idx}: {e}");
                    continue;
                }
                for bytes in d.lanes.buffer(idx).chunks_exact(PACKET_BYTES) {
                    each(packet::parse_any(bytes));
                }
            }
        }

        /// Every spectrum frame completed since the last call (cache invalidated), in order,
        /// checked, the newest marked. The first call only finds the ring's position.
        pub fn read_frames(&mut self, mut each: impl FnMut(Result<Frame<'_>, frame::Fault>, bool)) {
            let current = self.regs.spec_status().read().spec_last_buffer().bits() as u32;
            let d = &mut self.dma;
            let done = completed_since(d.spectrum.num_buffers(), &mut d.spectrum_last, current);
            let newest = done.last().copied();
            for idx in done {
                if let Err(e) = d.spectrum.cache_invalidate(idx) {
                    tracing::warn!("spectrum sub-buffer {idx}: {e}");
                    continue;
                }
                let bytes = d.spectrum.buffer(idx);
                each(frame::parse(bytes).map(|header| Frame { header, bytes }), Some(idx) == newest);
            }
        }

        /// The capture's sub-buffers completed since the last call (cache invalidated), in
        /// order, each with the sample index of its first sample: raw IQ at the AD9361's rate,
        /// interleaved little-endian i16 I, Q.
        pub fn read_capture(&mut self, mut each: impl FnMut(u64, &[u8])) -> CaptureRead {
            let status = self.capture_status();
            let n = self.dma.capture.num_buffers() as u32;
            let per = (self.dma.capture.buffer_size() / CAPTURE_SAMPLE_BYTES) as u64;
            let (pending, mut lapped) = capture_pending(n, self.capture_read, status.buffers);
            let first = pending.start;
            let mut read = 0;
            for k in pending {
                let slot = (k % n) as usize;
                if let Err(e) = self.dma.capture.cache_invalidate(slot) {
                    tracing::warn!("capture sub-buffer {slot}: {e}");
                    continue;
                }
                each(status.start_index + k as u64 * per, self.dma.capture.buffer(slot));
                read += 1;
            }
            self.capture_read = first + read;
            if read > 0 {
                // Sub-buffers the ring began to overwrite while they were copied.
                let now = self.regs.capture_buffers().read().count().bits();
                lapped += super::capture_oldest_whole(n, now).saturating_sub(first).min(read);
            }
            CaptureRead { read, lapped }
        }

        /// Once the capture has stopped after a trigger: its whole ring, oldest first (the
        /// newest sub-buffers the ring holds), each with the sample index of its first sample.
        /// Nothing while it runs.
        pub fn read_stopped_capture(&mut self, mut each: impl FnMut(u64, &[u8])) -> super::CaptureStatus {
            let status = self.capture_status();
            if !status.stopped {
                return status;
            }
            let n = self.dma.capture.num_buffers() as u32;
            let per = (self.dma.capture.buffer_size() / CAPTURE_SAMPLE_BYTES) as u64;
            for k in status.buffers.saturating_sub(n)..status.buffers {
                let slot = (k % n) as usize;
                if let Err(e) = self.dma.capture.cache_invalidate(slot) {
                    tracing::warn!("capture sub-buffer {slot}: {e}");
                    continue;
                }
                each(status.start_index + k as u64 * per, self.dma.capture.buffer(slot));
            }
            self.capture_read = status.buffers;
            status
        }
    }
}

#[cfg(target_os = "linux")]
impl RadioCore {
    /// Open the core: check its id, read its version and lanes, release the SDR reset and open
    /// its rings.
    pub async fn take() -> anyhow::Result<(RadioCore, irq::Interrupts)> {
        use anyhow::Context;

        let uio = crate::hardware::mmio::Uio::open("p25-core").await?;
        let mapping = std::sync::Arc::new(uio.map(0).await.context("p25-core registers")?);
        let regs = Registers::mapped(mapping);
        let id = regs.product_id().read().product_id().bits();
        anyhow::ensure!(id == PRODUCT_ID, "FPGA product id 0x{id:08x}, expected 0x{PRODUCT_ID:08x} (the radio core)");
        let v = regs.version().read();
        let version = Version { major: v.major().bits(), minor: v.minor().bits(), bugfix: v.bugfix().bits() };
        anyhow::ensure!(version.major == MAJOR_VERSION, "radio core {version}: this scanner reads {MAJOR_VERSION}.x");
        let caps = regs.capabilities().read();
        let lanes = caps.lanes().bits() as usize;
        anyhow::ensure!((2..=LANE_BANKS).contains(&lanes), "radio core {version} reports {lanes} lanes");
        let identity = Identity { version, platform: v.platform().bits(), lanes, vsb: caps.vsb().bit() };
        regs.control().modify(|_, w| w.sdr_reset().clear_bit());

        let dma = dma::Dma::open().await?;
        tracing::info!("radio core {version}, {lanes} lanes{}", if identity.vsb { ", 8-VSB" } else { "" });
        let interrupts = irq::Interrupts::new(uio, regs.clone());
        let core = RadioCore { regs, identity, capture_overflow: AtomicBool::new(false), capture_read: 0, dma };
        Ok((core, interrupts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn lanes_past_the_core_s_are_absent() {
        let core = RadioCore::in_memory(3);
        assert!(core.lane(2).is_some());
        assert!(core.lane(3).is_none());
    }

    #[test]
    fn completed_sub_buffers_follow_the_index_round_the_ring() {
        let mut last = None;
        assert!(completed_since(512, &mut last, 5).is_empty(), "the first call anchors");
        assert_eq!(completed_since(512, &mut last, 7), vec![6, 7]);
        assert_eq!(completed_since(512, &mut last, 1), (8..512).chain(0..=1).collect::<Vec<_>>());
        assert!(completed_since(512, &mut last, 1).is_empty());
    }

    #[test]
    fn the_capture_s_whole_sub_buffers_are_the_newest_fourteen() {
        // Sixteen sub-buffers: sub-buffer 14 complete, 15 being written, 16 (slot 0) perhaps
        // begun.
        assert_eq!(capture_pending(16, 0, 0), (0..0, 0));
        assert_eq!(capture_pending(16, 0, 3), (0..3, 0));
        assert_eq!(capture_pending(16, 3, 14), (3..14, 0));
        assert_eq!(capture_pending(16, 0, 15), (1..15, 1), "sub-buffer 0 may be overwritten");
        assert_eq!(capture_pending(16, 2, 40), (26..40, 24));
        assert_eq!(capture_pending(16, 40, 40), (40..40, 0));
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn a_capture_s_start_clears_its_overflow_and_count() {
        let mut core = RadioCore::in_memory(3);
        core.capture_overflow.store(true, Ordering::Relaxed);
        core.capture_read = 7;
        core.start_capture();
        assert!(core.capture_status().enabled);
        assert!(!core.capture_status().overflow);
        assert_eq!(core.capture_read, 0);
        core.stop_capture();
        assert!(!core.capture_status().enabled);
    }
}

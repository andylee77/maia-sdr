//! The radio core in the FPGA: its registers (through UIO), its DMA rings (through maia-kmod)
//! and its interrupt.
//!
//! The core has lanes, each a DDC (to 50 kSPS) whose IQ it cuts into tagged packets
//! (`packet`); one ring carries every lane's packets. Lane 0 is the control channel's, the
//! others the traffic lanes. Beside them: a wideband spectrometer and a raw IQ capture ring.

#[cfg(target_os = "linux")]
pub mod irq;
pub mod lane;
pub mod packet;
pub mod regs;

use std::fmt;

use serde::Serialize;

pub use lane::LaneDdc;
pub use packet::Header;
pub use regs::nco_to_freq;

use regs::{LaneBank, Registers, LANE_BANKS};

/// Expected `product_id` ("rad1").
pub const PRODUCT_ID: u32 = 0x7261_6431;
/// The control channel's lane.
pub const CONTROL_LANE: usize = 0;

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
}

/// The lane ring's registers, in one reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct RingStatus {
    pub enabled: bool,
    pub last_buffer: u8,
    pub next_address: u32,
}

pub struct RadioCore {
    regs: Registers,
    identity: Identity,
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

    /// FFT frames averaged into one spectrum (10 bits).
    pub fn set_spectrometer_integrations(&self, n: u16) {
        self.regs.spec_control().modify(|_, w| unsafe { w.spec_num_integrations().bits(n & 0x3FF) });
    }

    /// Peak hold instead of averaged power.
    pub fn set_spectrometer_peak(&self, on: bool) {
        self.regs.spec_control().modify(|_, w| w.spec_peak_detect().bit(on));
    }

    /// A core with its registers in plain memory and no rings (host tests).
    #[cfg(all(test, not(target_os = "linux")))]
    pub fn in_memory(lanes: usize) -> RadioCore {
        let version = Version { major: 1, minor: 0, bugfix: 0 };
        RadioCore { regs: Registers::in_memory(), identity: Identity { version, platform: 0, lanes } }
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

#[cfg(target_os = "linux")]
mod dma {
    use anyhow::{Context, Result};

    use super::packet::{self, Fault, Packet, PACKET_BYTES};
    use super::{completed_since, RadioCore};
    use crate::hardware::mmio::RxBuffer;

    pub(super) struct Dma {
        lanes: RxBuffer,
        lanes_last: Option<u32>,
        spectrum: RxBuffer,
        spectrum_last: Option<u8>,
        capture: RxBuffer,
        capture_last: Option<u32>,
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
            let capture = RxBuffer::open("p25-wideband-iq").await.context("the capture ring")?;
            Ok(Dma { lanes, lanes_last: None, spectrum, spectrum_last: None, capture, capture_last: None })
        }
    }

    impl RadioCore {
        /// Every packet in the lane ring's sub-buffers completed since the last call (cache
        /// invalidated), in ring order. The first call only finds the ring's position.
        pub fn read_packets(&mut self, mut each: impl FnMut(Result<Packet<'_>, Fault>)) {
            let current = self.regs.lanes_ring_status().read().last_buffer().bits() as u32;
            let d = &mut self.dma;
            for idx in completed_since(d.lanes.num_buffers(), &mut d.lanes_last, current) {
                if let Err(e) = d.lanes.cache_invalidate(idx) {
                    tracing::warn!("lane ring sub-buffer {idx}: {e}");
                    continue;
                }
                for bytes in d.lanes.buffer(idx).chunks_exact(PACKET_BYTES) {
                    each(packet::parse(bytes));
                }
            }
        }

        /// The raw IQ capture on or off. On, completed sub-buffers are counted from now.
        pub fn set_capture(&mut self, on: bool) {
            if on {
                let current = self.regs.wideband_iq_dma_status().read().last_buffer().bits() as u32;
                self.dma.capture_last = Some(current);
            }
            self.regs.wideband_iq_dma_control().modify(|_, w| w.wideband_iq_enable().bit(on));
        }

        /// The capture ring's sub-buffers completed since the last call (cache invalidated), in
        /// order: raw IQ at the AD9361's rate, interleaved little-endian i16 I, Q. Returns how
        /// many completed; as many as the ring holds or more means the oldest were overwritten.
        pub fn read_capture(&mut self, mut each: impl FnMut(&[u8])) -> usize {
            let current = self.regs.wideband_iq_dma_status().read().last_buffer().bits() as u32;
            let d = &mut self.dma;
            let done = completed_since(d.capture.num_buffers(), &mut d.capture_last, current);
            for &idx in &done {
                if let Err(e) = d.capture.cache_invalidate(idx) {
                    tracing::warn!("capture sub-buffer {idx}: {e}");
                    continue;
                }
                each(d.capture.buffer(idx));
            }
            done.len()
        }

        /// The capture ring's sub-buffers.
        pub fn capture_buffers(&self) -> usize {
            self.dma.capture.num_buffers()
        }

        /// The latest completed wideband spectrum (packed mantissa and exponent), once.
        pub fn read_spectrum(&mut self) -> Option<&[u8]> {
            let last = self.regs.spec_status().read().spec_last_buffer().bit() as u8;
            if self.dma.spectrum_last == Some(last) {
                return None;
            }
            self.dma.spectrum_last = Some(last);
            if let Err(e) = self.dma.spectrum.cache_invalidate(last as usize) {
                tracing::warn!("spectrum sub-buffer {last}: {e}");
                return None;
            }
            Some(self.dma.spectrum.buffer(last as usize))
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
        anyhow::ensure!(version.major == 1, "radio core {version}: this scanner reads 1.x");
        let lanes = regs.capabilities().read().lanes().bits() as usize;
        anyhow::ensure!((2..=LANE_BANKS).contains(&lanes), "radio core {version} reports {lanes} lanes");
        let identity = Identity { version, platform: v.platform().bits(), lanes };
        regs.control().modify(|_, w| w.sdr_reset().clear_bit());

        let dma = dma::Dma::open().await?;
        tracing::info!("radio core {version}, {lanes} lanes");
        let interrupts = irq::Interrupts::new(uio, regs.clone());
        Ok((RadioCore { regs, identity, dma }, interrupts))
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
        assert!(completed_since(128, &mut last, 5).is_empty(), "the first call anchors");
        assert_eq!(completed_since(128, &mut last, 7), vec![6, 7]);
        assert_eq!(completed_since(128, &mut last, 1), (8..128).chain(0..=1).collect::<Vec<_>>());
        assert!(completed_since(128, &mut last, 1).is_empty());
    }
}

//! The P25 core in the FPGA: its registers (through UIO), its DMA rings (through maia-kmod) and
//! its interrupt.
//!
//! The core has a control chain and one or two traffic chains ("lanes"), each a DDC plus an LSM
//! demodulator writing a dibit ring; IQ taps after the control and traffic DDCs and before the
//! LSM slicer; and a wideband spectrometer. The second lane exists from core
//! 0.3.0 and is never touched on an older core.

pub mod chain;
#[cfg(target_os = "linux")]
pub mod irq;
pub mod regs;
pub mod rings;

use std::fmt;

pub use chain::Chain;
pub use regs::{nco_to_freq, Bank, LsmControl, LsmStatus};
pub use rings::RingSnapshot;

use crate::hardware::core_version::CoreVersion;
use regs::Registers;

/// Expected `product_id` ("p25f").
pub const PRODUCT_ID: u32 = 0x7032_3566;

/// A traffic chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Lane {
    One,
    Two,
}

impl Lane {
    pub const ALL: [Lane; 2] = [Lane::One, Lane::Two];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn number(self) -> u8 {
        self as u8 + 1
    }

    pub fn name(self) -> &'static str {
        match self {
            Lane::One => "lane 1",
            Lane::Two => "lane 2",
        }
    }

    fn bank(self) -> Bank {
        match self {
            Lane::One => Bank::Traffic,
            Lane::Two => Bank::Traffic2,
        }
    }
}

impl fmt::Display for Lane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lane {}", self.number())
    }
}

/// The IQ taps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IqRing {
    /// After the control DDC (50 kSPS).
    Control,
    /// After lane one's DDC (50 kSPS).
    Traffic,
    /// Inside the control LSM, before the slicer (2 samples per symbol).
    PreDiff,
}

/// The dibit rings: the control chain's and each lane's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DibitRing {
    Control,
    Lane(Lane),
}

pub struct P25Core {
    regs: Registers,
    version: CoreVersion,
    lane_two: bool,
    #[cfg(target_os = "linux")]
    dma: dma::Dma,
}

impl P25Core {
    pub fn version(&self) -> CoreVersion {
        self.version
    }

    pub fn lanes(&self) -> impl Iterator<Item = Lane> + '_ {
        Lane::ALL.into_iter().filter(move |l| *l == Lane::One || self.lane_two)
    }

    pub fn control(&self) -> Chain<'_> {
        Chain { regs: &self.regs, bank: Bank::Control }
    }

    /// A lane's chain; `None` for a lane the core does not have.
    pub fn lane(&self, lane: Lane) -> Option<Chain<'_>> {
        (lane == Lane::One || self.lane_two).then(|| Chain { regs: &self.regs, bank: lane.bank() })
    }

    pub fn set_iq_enable(&self, ring: IqRing, on: bool) {
        let r = &self.regs;
        match ring {
            IqRing::Control => r.iq_dma_control().modify(|_, w| w.iq_enable().bit(on)),
            IqRing::Traffic => r.traffic_iq_dma_control().modify(|_, w| w.traffic_iq_enable().bit(on)),
            IqRing::PreDiff => r.pre_diff_iq_dma_control().modify(|_, w| w.pre_diff_iq_enable().bit(on)),
        };
    }

    /// The ring's latest completed sub-buffer.
    fn iq_last_buffer(&self, ring: IqRing) -> u32 {
        let r = &self.regs;
        (match ring {
            IqRing::Control => r.iq_dma_status().read().last_buffer().bits(),
            IqRing::Traffic => r.traffic_iq_dma_status().read().traffic_iq_last_buffer().bits(),
            IqRing::PreDiff => r.pre_diff_iq_dma_status().read().last_buffer().bits(),
        }) as u32
    }

    /// The chain behind a dibit ring, if present.
    fn dibit_chain(&self, ring: DibitRing) -> Option<Chain<'_>> {
        match ring {
            DibitRing::Control => Some(self.control()),
            DibitRing::Lane(l) => self.lane(l),
        }
    }

    /// One reading for the dibit reader: plain-read registers only (never the read-to-clear
    /// status). `None` for an absent lane.
    pub fn dibit_snapshot(&self, ring: DibitRing) -> Option<RingSnapshot> {
        let chain = self.dibit_chain(ring)?;
        let next_address = chain.dibit_next();
        let t_us = rings::mono_us();
        Some(RingSnapshot {
            next_address,
            last_buffer: chain.dibit_last_buffer(),
            chain_enabled: chain.lsm_enabled(),
            t_us,
        })
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

    #[cfg(test)]
    pub fn in_memory(version: CoreVersion) -> P25Core {
        P25Core {
            regs: Registers::in_memory(),
            version,
            lane_two: version.has_traffic2_chain(),
        }
    }
}

#[cfg(target_os = "linux")]
mod dma {
    use anyhow::{Context, Result};

    use super::rings::{completed_since, copy_plan, RingGeometry};
    use super::{DibitRing, IqRing, Lane, P25Core};
    use crate::hardware::mmio::RxBuffer;

    struct Ring {
        buffer: RxBuffer,
        last_seen: Option<u32>,
    }

    impl Ring {
        async fn open(name: &str) -> Result<Ring> {
            let buffer = RxBuffer::open(name).await.with_context(|| format!("DMA ring {name}"))?;
            Ok(Ring { buffer, last_seen: None })
        }

        fn geometry(&self) -> RingGeometry {
            RingGeometry {
                sub_buffer_bytes: self.buffer.buffer_size() as u64,
                num_sub_buffers: self.buffer.num_buffers() as u64,
            }
        }
    }

    pub(super) struct Dma {
        iq: [Ring; 3],
        dibit: [Option<Ring>; 3],
        spectrum: Ring,
        spectrum_last: Option<u8>,
    }

    impl Dma {
        pub(super) async fn open(lane_two: bool) -> Result<Dma> {
            Ok(Dma {
                iq: [
                    Ring::open("p25-iq").await?,
                    Ring::open("p25-traffic-iq").await?,
                    Ring::open("p25-pre-diff-iq").await?,
                ],
                dibit: [
                    Some(Ring::open("p25-lsm-dibit").await?),
                    Some(Ring::open("p25-traffic-lsm-dibit").await?),
                    if lane_two { Some(Ring::open("p25-traffic2-lsm-dibit").await?) } else { None },
                ],
                spectrum: Ring::open("p25-wideband-spec").await?,
                spectrum_last: None,
            })
        }

        fn dibit(&self, ring: DibitRing) -> Option<&Ring> {
            let i = match ring {
                DibitRing::Control => 0,
                DibitRing::Lane(Lane::One) => 1,
                DibitRing::Lane(Lane::Two) => 2,
            };
            self.dibit[i].as_ref()
        }
    }

    fn iq_index(ring: IqRing) -> usize {
        match ring {
            IqRing::Control => 0,
            IqRing::Traffic => 1,
            IqRing::PreDiff => 2,
        }
    }

    impl P25Core {
        /// IQ sub-buffers completed since the last call (cache invalidated).
        pub fn read_iq(&mut self, ring: IqRing) -> Vec<&[u8]> {
            let current = self.iq_last_buffer(ring);
            let r = &mut self.dma.iq[iq_index(ring)];
            let mut ready = Vec::new();
            for idx in completed_since(r.buffer.num_buffers(), &mut r.last_seen, current) {
                if let Err(e) = r.buffer.cache_invalidate(idx) {
                    tracing::warn!("{ring:?} IQ sub-buffer {idx}: {e}");
                    break;
                }
                ready.push(idx);
            }
            let r = &self.dma.iq[iq_index(ring)];
            ready.into_iter().map(|i| r.buffer.buffer(i)).collect()
        }

        pub fn dibit_geometry(&self, ring: DibitRing) -> Option<RingGeometry> {
            self.dma.dibit(ring).map(Ring::geometry)
        }

        /// Copy the absolute byte range `[start, end)` of a dibit ring into `out`. The caller
        /// knows the range has landed; every sub-buffer it touches is invalidated first.
        pub fn copy_dibits(&self, ring: DibitRing, start: u64, end: u64, out: &mut Vec<u8>) -> Result<()> {
            let r = self.dma.dibit(ring).with_context(|| format!("{ring:?} is not present"))?;
            let plan = copy_plan(&r.geometry(), start, end);
            let mut invalidated = Vec::with_capacity(2);
            for piece in &plan {
                if !invalidated.contains(&piece.sub_buffer) {
                    r.buffer.cache_invalidate(piece.sub_buffer)?;
                    invalidated.push(piece.sub_buffer);
                }
            }
            for piece in &plan {
                out.extend_from_slice(&r.buffer.buffer(piece.sub_buffer)[piece.offset..piece.offset + piece.len]);
            }
            Ok(())
        }

        /// The latest completed wideband spectrum (packed mantissa and exponent), once.
        pub fn read_spectrum(&mut self) -> Option<&[u8]> {
            let last = self.regs.spec_status().read().spec_last_buffer().bit() as u8;
            if self.dma.spectrum_last == Some(last) {
                return None;
            }
            self.dma.spectrum_last = Some(last);
            if let Err(e) = self.dma.spectrum.buffer.cache_invalidate(last as usize) {
                tracing::warn!("spectrum sub-buffer {last}: {e}");
                return None;
            }
            Some(self.dma.spectrum.buffer.buffer(last as usize))
        }
    }
}

#[cfg(target_os = "linux")]
impl P25Core {
    /// Open the core: check its id, read its version, release the SDR reset and open its rings.
    pub async fn take() -> anyhow::Result<(P25Core, irq::Interrupts)> {
        use anyhow::Context;

        let uio = crate::hardware::mmio::Uio::open("p25-core").await?;
        let mapping = std::sync::Arc::new(uio.map(0).await.context("p25-core registers")?);
        let regs = Registers::mapped(mapping);
        let id = regs.product_id().read().product_id().bits();
        anyhow::ensure!(id == PRODUCT_ID, "FPGA product id 0x{id:08x}, expected 0x{PRODUCT_ID:08x}");
        let v = regs.version().read();
        let version = CoreVersion::new(v.major().bits(), v.minor().bits(), v.bugfix().bits());
        regs.control().modify(|_, w| w.sdr_reset().clear_bit());

        let mut lane_two = version.has_traffic2_chain();
        let dma = match dma::Dma::open(lane_two).await {
            Ok(d) => d,
            Err(e) if lane_two => {
                tracing::warn!("core {version}: lane two's ring did not open ({e:#}); one lane");
                lane_two = false;
                dma::Dma::open(false).await?
            }
            Err(e) => return Err(e),
        };
        tracing::info!("P25 core {version}, {} traffic lane(s)", 1 + lane_two as usize);
        let interrupts = irq::Interrupts::new(uio, regs.clone());
        Ok((P25Core { regs, version, lane_two, dma }, interrupts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_two_exists_only_on_cores_that_have_it() {
        let old = P25Core::in_memory(CoreVersion::new(0, 2, 0));
        assert!(old.lane(Lane::Two).is_none());
        assert_eq!(old.lanes().collect::<Vec<_>>(), vec![Lane::One]);
        assert!(old.dibit_snapshot(DibitRing::Lane(Lane::Two)).is_none());
        let new = P25Core::in_memory(CoreVersion::new(0, 3, 0));
        assert!(new.lane(Lane::Two).is_some());
        assert_eq!(new.lanes().count(), 2);
    }
}

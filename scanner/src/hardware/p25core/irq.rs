//! The core's interrupt: one UIO interrupt, with a bit per DMA ring that completed a
//! sub-buffer. Readers wait on the ring's notifier; the counts feed the diagnostics.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

use super::regs::Registers;
use super::{DibitRing, IqRing, Lane};
use crate::hardware::mmio::Uio;

/// Interrupt counts since start.
#[derive(Debug, Default)]
pub struct IrqCounts {
    pub total: AtomicU64,
    pub iq: AtomicU64,
    pub traffic_iq: AtomicU64,
    pub pre_diff_iq: AtomicU64,
    pub wideband_iq: AtomicU64,
    pub control_dibit: AtomicU64,
    pub lane1_dibit: AtomicU64,
    pub lane2_dibit: AtomicU64,
}

pub struct Interrupts {
    uio: Uio,
    regs: Registers,
    counts: Arc<IrqCounts>,
    iq: Arc<Notify>,
    pre_diff_iq: Arc<Notify>,
    wideband_iq: Arc<Notify>,
    dibit: [Arc<Notify>; 3],
}

impl Interrupts {
    pub(super) fn new(uio: Uio, regs: Registers) -> Interrupts {
        Interrupts {
            uio,
            regs,
            counts: Arc::default(),
            iq: Arc::default(),
            pre_diff_iq: Arc::default(),
            wideband_iq: Arc::default(),
            dibit: Default::default(),
        }
    }

    pub fn counts(&self) -> Arc<IrqCounts> {
        self.counts.clone()
    }

    /// Notified when the ring completes a sub-buffer. The traffic IQ ring raises no
    /// notification (its reader polls).
    pub fn iq_waiter(&self, ring: IqRing) -> Option<Arc<Notify>> {
        match ring {
            IqRing::Control => Some(self.iq.clone()),
            IqRing::PreDiff => Some(self.pre_diff_iq.clone()),
            IqRing::Wideband => Some(self.wideband_iq.clone()),
            IqRing::Traffic => None,
        }
    }

    pub fn dibit_waiter(&self, ring: DibitRing) -> Arc<Notify> {
        let i = match ring {
            DibitRing::Control => 0,
            DibitRing::Lane(Lane::One) => 1,
            DibitRing::Lane(Lane::Two) => 2,
        };
        self.dibit[i].clone()
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        let bump = |c: &AtomicU64| c.fetch_add(1, Ordering::Relaxed);
        loop {
            self.uio.irq_enable().await?;
            self.uio.irq_wait().await?;
            let i = self.regs.interrupts().read();
            bump(&self.counts.total);
            for (raised, count, notify) in [
                (i.iq_dma().bit(), &self.counts.iq, Some(&self.iq)),
                (i.pre_diff_iq_dma().bit(), &self.counts.pre_diff_iq, Some(&self.pre_diff_iq)),
                (i.wideband_iq_dma().bit(), &self.counts.wideband_iq, Some(&self.wideband_iq)),
                (i.traffic_iq_dma().bit(), &self.counts.traffic_iq, None),
                (i.lsm_dibit_dma().bit(), &self.counts.control_dibit, Some(&self.dibit[0])),
                (i.traffic_lsm_dibit_dma().bit(), &self.counts.lane1_dibit, Some(&self.dibit[1])),
                (i.traffic2_lsm_dibit_dma().bit(), &self.counts.lane2_dibit, Some(&self.dibit[2])),
            ] {
                if raised {
                    bump(count);
                    if let Some(n) = notify {
                        n.notify_one();
                    }
                }
            }
        }
    }
}

//! The core's interrupt: one UIO interrupt, with a sticky bit per ring (lanes, spectrum,
//! capture) that completed a sub-buffer. The readers poll the rings on a timer, so the interrupt
//! is only acknowledged.

use super::regs::Registers;
use crate::hardware::mmio::Uio;

pub struct Interrupts {
    uio: Uio,
    regs: Registers,
}

impl Interrupts {
    pub(super) fn new(uio: Uio, regs: Registers) -> Interrupts {
        Interrupts { uio, regs }
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        loop {
            self.uio.irq_enable().await?;
            self.uio.irq_wait().await?;
            // The bits clear on read.
            let _ = self.regs.interrupts().read();
        }
    }
}

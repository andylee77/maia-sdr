#
# Fishball hardware validation (hwval) - PL device DNA reader
#
# Reads the 57-bit Zynq-7000 / 7-series device DNA once after reset through
# the DNA_PORT primitive, so the bench can identify a board independently
# of the SD card (the IIO serial follows the card).
#
# SPDX-License-Identifier: MIT
#

from amaranth import *
import amaranth.back.verilog


DNA_BITS = 57

# Value returned by the simulation model (and SIM_DNA_VALUE of the
# primitive, which only matters in Vivado simulation).
DEFAULT_SIM_DNA = 0x0_1234_5678_9ABC_DE


class DnaReader(Elaboratable):
    """Device DNA reader (DNA_PORT).

    Runs in the ``sync`` domain (rename it to the AXI-Lite domain). The
    DNA_PORT clock is a divided register-generated clock: ``dna_clk`` is
    high for 4 and low for 4 domain cycles (12.5 MHz from 100 MHz, well
    below the 100 MHz DNA_PORT maximum). ``READ``/``SHIFT`` change and
    ``DOUT`` is sampled only while ``dna_clk`` is low, 4 domain cycles away
    from its rising edge, so the primitive interface has multi-cycle
    setup/hold margins by construction.

    Sequence after reset: one rising edge with READ = 1 loads the DNA and
    presents bit 56 on DOUT; then DOUT is sampled and SHIFT = 1 on the
    following edges, MSB first, 57 samples in total. ``valid`` is set
    when the whole value has been shifted in (about 470 cycles after
    reset).

    Parameters
    ----------
    sim : bool
        Replace the DNA_PORT primitive (which pysim cannot simulate) by a
        behavioural model that returns ``sim_value``.
    sim_value : int
        57-bit value of the simulation model and SIM_DNA_VALUE.

    Attributes
    ----------
    dna : Signal(57), out
        Device DNA (valid when ``valid`` is high).
    valid : Signal(), out
    dna_clk, dna_read, dna_shift, dna_dout : Signal()
        DNA_PORT interface (exposed for observation in tests).
    """
    DIV_LOG2 = 3

    def __init__(self, *, sim=False, sim_value=DEFAULT_SIM_DNA):
        if not 0 <= sim_value < 2**DNA_BITS:
            raise ValueError('sim_value must fit in 57 bits')
        self.sim = sim
        self.sim_value = sim_value
        self.dna = Signal(DNA_BITS)
        self.valid = Signal()
        self.dna_clk = Signal()
        self.dna_read = Signal()
        self.dna_shift = Signal()
        self.dna_dout = Signal()

    def elaborate(self, platform):
        m = Module()

        # ── DNA_PORT (or model) ─────────────────────────────────
        dna_clk = self.dna_clk
        dna_read = self.dna_read
        dna_shift = self.dna_shift
        dna_dout = self.dna_dout
        cnt = Signal(self.DIV_LOG2)
        m.d.sync += [
            cnt.eq(cnt + 1),
            # dna_clk rises at the edge where cnt goes 4 -> 5 (registered
            # copy of cnt[2]) and falls at the edge where cnt goes 0 -> 1.
            dna_clk.eq(cnt[-1]),
        ]
        if self.sim:
            sh = Signal(DNA_BITS, reset_less=True)
            m.d.comb += dna_dout.eq(sh[-1])
            # the model updates at the fabric edge where dna_clk rises
            rise = cnt == (1 << (self.DIV_LOG2 - 1))
            with m.If(rise):
                with m.If(dna_read):
                    m.d.sync += sh.eq(self.sim_value)
                with m.Elif(dna_shift):
                    m.d.sync += sh.eq(Cat(C(0, 1), sh[:-1]))
        else:
            m.submodules.dna_port = Instance(
                'DNA_PORT',
                p_SIM_DNA_VALUE=C(self.sim_value, DNA_BITS),
                i_CLK=dna_clk,
                i_READ=dna_read,
                i_SHIFT=dna_shift,
                i_DIN=C(0, 1),
                o_DOUT=dna_dout)

        # ── Sequencer (acts when cnt == 0: dna_clk low, 4 cycles after
        # the previous rising edge and 4 cycles before the next one) ──
        step = Signal()
        m.d.comb += step.eq(cnt == 0)
        started = Signal()
        nbits = Signal(range(DNA_BITS + 1))
        shreg = Signal(DNA_BITS)
        with m.If(step & ~self.valid):
            with m.If(~started):
                m.d.sync += [
                    started.eq(1),
                    dna_read.eq(1),
                ]
            with m.Else():
                m.d.sync += [
                    dna_read.eq(0),
                    shreg.eq(Cat(dna_dout, shreg[:-1])),
                    nbits.eq(nbits + 1),
                ]
                with m.If(nbits == DNA_BITS - 1):
                    m.d.sync += [
                        dna_shift.eq(0),
                        self.valid.eq(1),
                    ]
                with m.Else():
                    m.d.sync += dna_shift.eq(1)
        m.d.comb += self.dna.eq(shreg)

        return m


if __name__ == '__main__':
    dna = DnaReader()
    print(amaranth.back.verilog.convert(dna, ports=[dna.dna, dna.valid]))

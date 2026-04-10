#
# Fishball P25 - Dibit Packer for DMA
#
# Packs 2-bit dibit symbols into 64-bit words for AXI DMA transfer to PS.
# 32 dibits per 64-bit word. At 4800 sym/sec = 150 DMA words/sec.
#
# Interleaves timestamp words for PS timing alignment.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *


class DibitPacker(Elaboratable):
    """Pack dibit stream into 64-bit DMA words

    Packs 32 consecutive 2-bit dibits into a 64-bit word for DMA transfer.
    Uses AXI4-Stream-like handshaking (data_valid / stream_ready) for
    backpressure from DmaStreamWrite.

    At 4800 sym/sec = 150 words/sec — negligible bandwidth, so backpressure
    should never fire in practice, but we handle it correctly.

    Inputs (sync domain):
        dibit_in: 2-bit dibit symbol
        symbol_strobe: symbol valid strobe
        stream_ready: DMA ready for next word (backpressure)

    Outputs (sync domain):
        data_out: 64-bit packed word (32 dibits)
        data_valid: word ready for DMA (AXI4-Stream valid)
        overflow: **one-cycle pulse** asserted when a complete 32-dibit
            word is latched while the previous word is still waiting
            for ``stream_ready``. Must NOT be a latched level: the
            ``Rsticky`` register-layer wrapper (``maia_hdl.register``)
            already handles accumulation + PS read-clear, and does so
            by snapshotting the *current input value* on read
            (``sticky := input``), so a latched level would get
            re-accumulated on the very next cycle and the PS could
            never clear the sticky. Phase 6C manifested this bug in
            `iq_packer` (where back-pressure actually fires on ring
            wrap); `dibit_packer` has the same latent bug but has
            never been observed in practice because the dibit rate
            (~1.28 KB/s) is orders of magnitude below the HP1 budget
            so the trigger condition effectively never fires. Fixed
            anyway for consistency with `iq_packer`. See
            doc/changes/020_iq_dibit_packer_overflow_pulse.md.
    """
    def __init__(self):
        # Inputs
        self.dibit_in = Signal(2)
        self.symbol_strobe = Signal()
        self.stream_ready = Signal(reset=1)  # default ready

        # Outputs
        self.data_out = Signal(64)
        self.data_valid = Signal()
        self.overflow = Signal()

    def elaborate(self, platform):
        m = Module()

        # 32 dibits packed into 64 bits: dibit[0] in bits [1:0],
        # dibit[1] in bits [3:2], ..., dibit[31] in bits [63:62]
        shift_reg = Signal(64, reset_less=True)
        count = Signal(range(32), reset=0)

        # Output holding register + valid flag
        # data_valid stays high until stream_ready handshake completes
        holding_valid = Signal()

        # Handshake: clear valid when DMA accepts the word
        with m.If(holding_valid & self.stream_ready):
            m.d.sync += holding_valid.eq(0)

        # Default overflow to 0 every cycle so the only time it is
        # high is the single cycle after a trigger event -- see the
        # class docstring for why this MUST be a pulse and not a
        # latched level.
        m.d.sync += self.overflow.eq(0)

        with m.If(self.symbol_strobe):
            # Shift new dibit in
            m.d.sync += [
                shift_reg.eq(Cat(shift_reg[2:], self.dibit_in)),
                count.eq(count + 1),
            ]

            # When we've accumulated 32 dibits, latch output word
            with m.If(count == 31):
                # If previous word hasn't been accepted yet, emit a
                # one-cycle overflow pulse. The Rsticky register
                # wrapper in maia_hdl.register.Registers takes care
                # of accumulation + PS read-clear.
                with m.If(holding_valid):
                    m.d.sync += self.overflow.eq(1)
                m.d.sync += [
                    self.data_out.eq(Cat(shift_reg[2:], self.dibit_in)),
                    holding_valid.eq(1),
                    count.eq(0),
                ]

        m.d.comb += self.data_valid.eq(holding_valid)

        return m

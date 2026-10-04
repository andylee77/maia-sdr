#
# Fishball P25 - IQ packer: two IQ samples into each 64-bit word of a DmaStreamRingWrite ring.
# The radio core packs the AD9361's raw samples (12 bits, sign-extended) for the capture ring;
# hwval's replica of that ring uses it the same way.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *


class IQPacker(Elaboratable):
    """Pack an IQ stream into 64-bit DMA words, two samples a word.

    The first sample of a pair lands in the low half of the word, the second in the high half::

        bit 63                                                              bit 0
        +-----------------+-----------------+-----------------+-----------------+
        |    im[1] s16    |    re[1] s16    |    im[0] s16    |    re[0] s16    |
        +-----------------+-----------------+-----------------+-----------------+
               63..48            47..32            31..16            15..0
                       sample 1                          sample 0
                       (later)                           (earlier)

    The PS reads each word as four little-endian ``int16`` values, ``re0, im0, re1, im1``:
    interleaved IQ.

    There is one holding register and no FIFO: a word not yet taken when the next pair is
    complete is overwritten.

    Inputs (sync domain):
        re_in, im_in: signed(16)
        strobe_in: a one-cycle pulse for each new (re_in, im_in) pair
        stream_ready: back-pressure from the DMA (ready by default, for tests in isolation)

    Outputs (sync domain):
        data_out: the packed word
        data_valid: AXI4-Stream valid; high from the word's latch until ``stream_ready`` takes it
        overflow: a one-cycle pulse when a word is latched over one still waiting. It must be a
            pulse, not a level: the ``Rsticky`` register field accumulates it and clears on read
            by sampling its input, so a held level would set it again the cycle after a read.
    """
    def __init__(self):
        # Inputs
        self.re_in = Signal(signed(16))
        self.im_in = Signal(signed(16))
        self.strobe_in = Signal()
        self.stream_ready = Signal(reset=1)  # default ready

        # Outputs
        self.data_out = Signal(64)
        self.data_valid = Signal()
        self.overflow = Signal()

    def elaborate(self, platform):
        m = Module()

        # Phase: 0 = next strobe fills the LOW half (sample 0)
        #        1 = next strobe fills the HIGH half (sample 1) and
        #            triggers a hand-off to the DMA stream
        phase = Signal(reset=0)

        # Shadow holding the first (low-half) sample as a packed
        # 32-bit value: {im_in[15:0], re_in[15:0]}
        low_half = Signal(32, reset_less=True)

        # Output holding register + valid flag. Valid stays asserted
        # until the DMA accepts the word via stream_ready.
        holding_valid = Signal()

        # Handshake: clear valid when DMA accepts the word.
        with m.If(holding_valid & self.stream_ready):
            m.d.sync += holding_valid.eq(0)

        # Default overflow to 0 every cycle so the only time it is
        # high is the single cycle after a trigger event -- see the
        # class docstring for why this MUST be a pulse and not a
        # latched level.
        m.d.sync += self.overflow.eq(0)

        with m.If(self.strobe_in):
            with m.If(phase == 0):
                # Latch the first sample into the low half and flip
                # phase. No DMA traffic yet.
                m.d.sync += [
                    low_half.eq(Cat(self.re_in, self.im_in)),
                    phase.eq(1),
                ]
            with m.Else():
                # Assemble the second sample into the high half and
                # combine with the shadowed first sample to produce
                # the full 64-bit word. Reset phase for the next pair.
                m.d.sync += [
                    self.data_out.eq(
                        Cat(low_half, self.re_in, self.im_in)),
                    holding_valid.eq(1),
                    phase.eq(0),
                ]
                # If the previous word still hasn't been accepted by
                # the DMA, we are about to overwrite it -- emit a
                # one-cycle overflow pulse. The Rsticky register
                # wrapper in maia_hdl.register.Registers takes care
                # of accumulation + PS read-clear.
                with m.If(holding_valid):
                    m.d.sync += self.overflow.eq(1)

        m.d.comb += self.data_valid.eq(holding_valid)

        return m

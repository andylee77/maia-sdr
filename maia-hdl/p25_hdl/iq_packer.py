#
# Fishball P25 - IQ Packer for DMA (Phase 6C)
#
# Packs post-DDC IQ samples (16-bit signed I + 16-bit signed Q at
# 62.5 kSPS) into 64-bit DMA words for transfer to PS DRAM via the
# DmaStreamRingWrite ring buffer. Two consecutive samples per word.
#
# Modeled directly on dibit_packer.py — same handshake convention
# (data_valid + stream_ready + sticky overflow), same sync-domain
# semantics. The differences are: input width (16+16 instead of 2),
# pack count (2 samples instead of 32 dibits), and the byte rate
# (~250 KB/s instead of ~1.28 KB/s).
#
# See doc/P25_ADDRESS_MAP.md for the full address-space picture
# (DDR carve-outs, register banks, IRQ assignments).
#
# SPDX-License-Identifier: MIT
#

from amaranth import *


class IQPacker(Elaboratable):
    """Pack post-DDC IQ stream into 64-bit DMA words.

    Buffers two consecutive (re, im) sample pairs and emits one
    64-bit AXI4-Stream word per pair-of-strobes. The first sample
    seen lands in the LOW half of the word; the second lands in
    the HIGH half. Bit layout of ``data_out``::

        bit 63                                                              bit 0
        +-----------------+-----------------+-----------------+-----------------+
        |    im[1] s16    |    re[1] s16    |    im[0] s16    |    re[0] s16    |
        +-----------------+-----------------+-----------------+-----------------+
               63..48            47..32            31..16            15..0
                       sample 1                          sample 0
                       (later)                           (earlier)

    Each 16-bit field is two's complement, matching ``DDC.re_out`` /
    ``DDC.im_out``. The PS-side reader interprets each 64-bit DMA
    word as four little-endian ``int16`` values in the order
    ``re0, im0, re1, im1`` (i.e. the natural byte order of an
    interleaved-IQ buffer).

    Upstream rate assumption: the control DDC produces post-decimation
    samples at 62.5 kSPS (8 MSPS / 128x), so this packer emits one
    64-bit word every 32 us. The downstream ``DmaStreamRingWrite`` is
    almost always ready (HP1 budget at ~1.7 GB/s easily absorbs the
    ~250 KB/s byte rate), but the overflow flag is still wired up
    so we can detect AXI starvation in the field via the
    ``iq_dma_status.iq_overflow`` register bit.

    Inputs (sync domain):
        re_in: signed(16) — DDC re_out
        im_in: signed(16) — DDC im_out
        strobe_in: 1-cycle pulse from DDC strobe_out marking each
            new (re_in, im_in) pair
        stream_ready: backpressure from DmaStreamRingWrite (defaults
            to ready so the packer can be tested in isolation)

    Outputs (sync domain):
        data_out: 64-bit packed word containing two IQ pairs
        data_valid: AXI4-Stream valid; rises when a new word is
            latched and stays high until ``stream_ready`` accepts it
        overflow: **one-cycle pulse** — asserts for exactly one
            cycle when a new word is latched while the previous
            word is still waiting for ``stream_ready``. Must NOT be
            a latched level: the ``Rsticky`` register-layer wrapper
            (maia_hdl.register.Registers) already handles
            accumulation + read-clear, and does so by snapshotting
            the *current input value* on read (sticky := input), so
            a latched level would get re-accumulated on the very
            next cycle and the PS could never clear the sticky.
            This was the Phase 6C iq_dma spurious-overflow bug that
            caused the p25-httpd LSM pipeline to reset every
            sub-buffer. See doc/changes/020_iq_dibit_packer_overflow_pulse.md.
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

        # Output holding register + valid flag, identical pattern
        # to DibitPacker. Valid stays asserted until the DMA accepts
        # the word via stream_ready.
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

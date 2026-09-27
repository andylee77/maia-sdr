#
# Fishball hwval - test pattern sources for the ring writers
#
# RateGen paces the sources, WordPattern feeds ring v2 with 64-bit words
# and SamplePattern feeds the legacy (production-replica) ring with IQ
# samples. See doc/HW_VALIDATION_SUITE.md sections 6.4 and 7.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *


PRBS31_SEED = 0x7FFF_FFFF


def prbs31_step32(state):
    """Advance the PRBS31 generator by one 32-bit word.

    Returns ``(word, new_state)``. See :func:`prbs31_words`.
    """
    word = 0
    for _ in range(32):
        bit = ((state >> 30) ^ (state >> 27)) & 1
        state = ((state << 1) | bit) & 0x7FFF_FFFF
        word = (word << 1) | bit
    return word, state


def prbs31_words(n, seed=PRBS31_SEED):
    """Reference PRBS31 word sequence used by ``WordPattern`` mode 3.

    Generator polynomial x^31 + x^28 + 1 as a Fibonacci LFSR over a 31-bit
    state ``s`` (bit 0 = most recent bit). One step is::

        bit = s[30] ^ s[27]
        s   = ((s << 1) | bit) & 0x7FFFFFFF

    A word is 32 consecutive steps, the first generated bit landing in
    bit 31 of the word (MSB first). The state is loaded with ``seed``
    (nonzero) on ``clear``, and word k (k = 0, 1, ...) is the k-th block
    of 32 bits. After a word the state equals ``word & 0x7FFFFFFF``, so a
    checker can resynchronise from any single received word:
    ``prbs31_step32(prev_word & 0x7FFFFFFF)[0]`` is the next word.
    """
    state = seed & 0x7FFF_FFFF
    if state == 0:
        raise ValueError('PRBS31 seed must be nonzero')
    out = []
    for _ in range(n):
        word, state = prbs31_step32(state)
        out.append(word)
    return out


def rate_inc(rate_hz, clk_hz):
    """RateGen ``inc`` value for a strobe rate at a given clock."""
    return min(round(rate_hz / clk_hz * 2**32), 2**32 - 1)


class RateGen(Elaboratable):
    """Fractional strobe generator.

    A 32-bit phase accumulator adds ``inc`` every cycle while ``enable`` is
    high and emits a one-cycle ``strobe`` (registered) on every carry, so
    the strobe rate is ``inc / 2**32 * f_clk``. ``inc = 0xFFFFFFFF`` gives a
    strobe on every cycle except the first. The accumulator is cleared
    while ``enable`` is low, so the strobe pattern after enable is
    deterministic.

    Attributes
    ----------
    inc : Signal(32), in
        Phase increment.
    enable : Signal(), in
        Run the accumulator.
    strobe : Signal(), out
        One-cycle pulse per carry.
    """
    def __init__(self):
        self.inc = Signal(32)
        self.enable = Signal()
        self.strobe = Signal()

    def elaborate(self, platform):
        m = Module()
        acc = Signal(32)
        acc_next = Signal(33)
        m.d.comb += acc_next.eq(acc + self.inc)
        with m.If(self.enable):
            m.d.sync += [
                acc.eq(acc_next[:32]),
                self.strobe.eq(acc_next[32]),
            ]
        with m.Else():
            m.d.sync += [
                acc.eq(0),
                self.strobe.eq(0),
            ]
        return m


class WordPattern(Elaboratable):
    """64-bit word source for ring v2.

    One word is produced per ``strobe`` (modes 1-3) or per ``live_valid``
    (mode 4). Outputs are registered: ``valid`` pulses for one cycle per
    word, one cycle after the strobe. ``seq`` is the number of words
    produced since ``clear`` and is also output as ``count``.

    Modes:

    - 0 off: no words.
    - 1 ramp64: ``data = seq``.
    - 2 tagged: ``data = {tag[3:0], 4'b0000, seq[55:0]}``.
    - 3 prbs31: ``data = {seq[31:0], prbs[31:0]}``, where ``prbs`` is the
      next word of :func:`prbs31_words` (seed ``PRBS31_SEED`` loaded on
      ``clear``). The LFSR only advances on words produced in this mode,
      so the reference sequence holds for a run that is cleared and then
      stays in mode 3.
    - 4 live: ``data = live_data`` for every ``live_valid`` (the strobe is
      ignored).

    ``clear`` resets ``seq``/``count`` and the LFSR and suppresses the
    word of that cycle.

    Attributes
    ----------
    mode : Signal(3), in
    tag : Signal(4), in
    strobe : Signal(), in
        Word request (from RateGen).
    live_data : Signal(64), in
    live_valid : Signal(), in
    clear : Signal(), in
    data : Signal(64), out
    valid : Signal(), out
    count : Signal(64), out
        Words produced since clear.
    """
    OFF = 0
    RAMP64 = 1
    TAGGED = 2
    PRBS31 = 3
    LIVE = 4

    def __init__(self, prbs_seed=PRBS31_SEED):
        if prbs_seed & 0x7FFF_FFFF == 0:
            raise ValueError('PRBS31 seed must be nonzero')
        self.prbs_seed = prbs_seed & 0x7FFF_FFFF

        self.mode = Signal(3)
        self.tag = Signal(4)
        self.strobe = Signal()
        self.live_data = Signal(64)
        self.live_valid = Signal()
        self.clear = Signal()
        self.data = Signal(64)
        self.valid = Signal()
        self.count = Signal(64)

    def elaborate(self, platform):
        m = Module()
        seq = self.count

        # PRBS31: 32 LFSR steps unrolled (each new bit is the XOR of two
        # earlier bits, so the logic depth stays at two XOR levels).
        lfsr = Signal(31, init=self.prbs_seed)
        s = [lfsr[i] for i in range(31)]
        word_bits = []
        for _ in range(32):
            bit = s[30] ^ s[27]
            s = [bit] + s[:30]
            word_bits.append(bit)
        prbs_word = Cat(*reversed(word_bits))  # first bit -> MSB
        lfsr_next = Cat(*s)

        produce = Signal()
        m.d.comb += produce.eq(
            Mux(self.mode == self.LIVE, self.live_valid,
                self.strobe & (self.mode != self.OFF)
                & (self.mode <= self.PRBS31)))

        m.d.sync += self.valid.eq(produce & ~self.clear)
        with m.If(produce):
            m.d.sync += seq.eq(seq + 1)
            with m.Switch(self.mode):
                with m.Case(self.RAMP64):
                    m.d.sync += self.data.eq(seq)
                with m.Case(self.TAGGED):
                    m.d.sync += self.data.eq(
                        Cat(seq[:56], C(0, 4), self.tag))
                with m.Case(self.PRBS31):
                    m.d.sync += [
                        self.data.eq(Cat(prbs_word, seq[:32])),
                        lfsr.eq(lfsr_next),
                    ]
                with m.Case(self.LIVE):
                    m.d.sync += self.data.eq(self.live_data)

        with m.If(self.clear):
            m.d.sync += [
                seq.eq(0),
                lfsr.eq(self.prbs_seed),
            ]

        return m


class SamplePattern(Elaboratable):
    """IQ sample source for the legacy (production-replica) ring.

    Outputs are registered; ``strobe_out`` pulses for one cycle per
    sample.

    Modes:

    - 0 off: no samples.
    - 1 ramp: a 32-bit counter ``c`` (0 after clear) is emitted as
      ``re = c[15:0]``, ``im = c[31:16]`` and incremented per sample.
      Through ``IQPacker`` (sample 0 in the low half) the DMA words are
      ``{c+1, c}``, i.e. ``word = (c + 1) << 32 | c`` with ``c`` even when
      the packer phase is aligned.
    - 2 live: ``live_re``/``live_im`` for every ``live_strobe`` (the
      ``strobe`` input is ignored).

    ``count`` is the number of samples produced since ``clear``.

    Attributes
    ----------
    mode : Signal(2), in
    strobe : Signal(), in
    live_re, live_im : Signal(16), in
    live_strobe : Signal(), in
    clear : Signal(), in
    re, im : Signal(16), out
    strobe_out : Signal(), out
    count : Signal(64), out
    """
    OFF = 0
    RAMP = 1
    LIVE = 2

    def __init__(self):
        self.mode = Signal(2)
        self.strobe = Signal()
        self.live_re = Signal(16)
        self.live_im = Signal(16)
        self.live_strobe = Signal()
        self.clear = Signal()
        self.re = Signal(16)
        self.im = Signal(16)
        self.strobe_out = Signal()
        self.count = Signal(64)

    def elaborate(self, platform):
        m = Module()
        ramp = Signal(32)

        produce = Signal()
        m.d.comb += produce.eq(
            Mux(self.mode == self.LIVE, self.live_strobe,
                self.strobe & (self.mode == self.RAMP)))

        m.d.sync += self.strobe_out.eq(produce & ~self.clear)
        with m.If(produce):
            m.d.sync += self.count.eq(self.count + 1)
            with m.If(self.mode == self.LIVE):
                m.d.sync += [
                    self.re.eq(self.live_re),
                    self.im.eq(self.live_im),
                ]
            with m.Else():
                m.d.sync += [
                    self.re.eq(ramp[:16]),
                    self.im.eq(ramp[16:]),
                    ramp.eq(ramp + 1),
                ]

        with m.If(self.clear):
            m.d.sync += [
                self.count.eq(0),
                ramp.eq(0),
            ]

        return m

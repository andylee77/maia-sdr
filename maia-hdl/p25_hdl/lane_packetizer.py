#
# Fishball P25 - one lane's IQ packets for the lane ring (doc/changes/079_general_radio_core.md,
# "The lane packet").
#
# A packet is 512 words of 64 bits: an 8-word header, then 504 words of IQ (1008 samples, two a
# word, the earlier in the low half). The lane fills one slot of block RAM while the other holds
# a closed packet for the ring. A packet closes when it is full, when a sample arrives with another
# tag (that sample starts the next packet), or when the lane is disabled. With no free slot the
# lane drops samples and its next packet says so. The header lives in registers and is put
# together as the ring reads it.
#
# Samples must be at least two cycles apart (the DDC's are hundreds apart): a tag change takes one
# cycle to close the packet before the sample that caused it is stored.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *
from amaranth.lib.memory import Memory

MAGIC = 0x5243
FORMAT = 1
PACKET_WORDS = 512
HEADER_WORDS = 8
PAYLOAD_WORDS = PACKET_WORDS - HEADER_WORDS
MAX_SAMPLES = 2 * PAYLOAD_WORDS

FLAG_LOST = 1 << 0
FLAG_RETUNED = 1 << 1
FLAG_LAST = 1 << 2


def fold(word):
    """XOR of a 64-bit word's two 32-bit halves."""
    return (word ^ (word >> 32)) & 0xFFFF_FFFF


def iq_word(re0, im0, re1=0, im1=0):
    """A payload word (Python reference): two samples, the earlier in the low half."""
    return ((re0 & 0xFFFF) | ((im0 & 0xFFFF) << 16) | ((re1 & 0xFFFF) << 32)
            | ((im1 & 0xFFFF) << 48))


def header_words(*, lane, flags, count, tag, sample_index, power, peak, nco, sequence,
                 adc_clips, payload):
    """A packet's 8 header words (Python reference). `payload` is its IQ words."""
    words = [
        MAGIC | (FORMAT << 16) | (lane << 20) | (flags << 24) | (count << 32) | (tag << 48),
        sample_index & (2**64 - 1),
        (power & (2**48 - 1)) | (peak << 48),
        (nco & (2**28 - 1)) | (sequence << 32),
        adc_clips & 0xFFFF_FFFF,
        0,
        0,
    ]
    check = 0
    for w in words + list(payload):
        check ^= fold(w)
    return words + [check]


def _fold(word):
    return word[:32] ^ word[32:]


class LanePacketizer(Elaboratable):
    """One lane's packets.

    Parameters
    ----------
    lane : int
        The lane number in the header.

    Inputs (sync):
        re_in, im_in : signed(16)   a sample, valid with ``strobe_in``
        strobe_in                   one cycle per sample
        enable                      the lane makes packets; a fall closes the packet in progress
        tag : Signal(16)            the lane's tag
        nco : Signal(28)            the lane's NCO word
        sample_index : Signal(64)   the AD9361 sample count when the sample was made
        adc_clips : Signal(32)      the running count of AD9361 samples at full scale

    Read side (sync), for the lane ring:
        ready                       a closed packet is waiting
        rd_addr : Signal(9)         the word wanted, valid with ``rd_en``
        rd_en                       read ``rd_addr``; ``rd_data`` holds it from the next cycle
                                    until the next ``rd_en``
        rd_data : Signal(64)
        release                     one cycle: the waiting packet has been read

    Status:
        lost                        one cycle: a sample was dropped
    """
    def __init__(self, lane: int):
        self.lane = lane
        self.re_in = Signal(signed(16))
        self.im_in = Signal(signed(16))
        self.strobe_in = Signal()
        self.enable = Signal()
        self.tag = Signal(16)
        self.nco = Signal(28)
        self.sample_index = Signal(64)
        self.adc_clips = Signal(32)

        self.ready = Signal()
        self.rd_addr = Signal(9)
        self.rd_en = Signal()
        self.rd_data = Signal(64)
        self.release = Signal()
        self.lost = Signal()

    def elaborate(self, platform):
        m = Module()

        m.submodules.mem = mem = Memory(shape=64, depth=2 * PACKET_WORDS, init=[])
        wr = mem.write_port()
        rd = mem.read_port()

        # The sample waiting to be stored, with its metadata.
        s_valid = Signal()
        s_re = Signal(signed(16))
        s_im = Signal(signed(16))
        s_tag = Signal(16)
        s_nco = Signal(28)
        s_index = Signal(64)
        s_clips = Signal(32)
        with m.If(self.strobe_in & self.enable):
            m.d.sync += [
                s_valid.eq(1), s_re.eq(self.re_in), s_im.eq(self.im_in), s_tag.eq(self.tag),
                s_nco.eq(self.nco), s_index.eq(self.sample_index), s_clips.eq(self.adc_clips),
            ]
        enable_q = Signal()
        disable_pending = Signal()
        m.d.sync += enable_q.eq(self.enable)
        with m.If(enable_q & ~self.enable):
            m.d.sync += disable_pending.eq(1)

        # Slots: `fill` takes samples while it is not full; the ring reads `rd`.
        fill = Signal()
        rd_slot = Signal()
        full = Signal(2)
        m.d.comb += self.ready.eq(full.bit_select(rd_slot, 1))
        with m.If(self.release):
            m.d.sync += [full.bit_select(rd_slot, 1).eq(0), rd_slot.eq(~rd_slot)]
        can_fill = ~full.bit_select(fill, 1)

        # The packet being filled.
        count = Signal(range(MAX_SAMPLES + 1))
        half_re = Signal(signed(16))
        half_im = Signal(signed(16))
        p_tag = Signal(16)
        p_index = Signal(64)
        p_nco = Signal(28)
        p_clips = Signal(32)
        p_flags = Signal(8)
        p_power = Signal(48)
        p_peak = Signal(16)
        p_xor = Signal(32)
        lost_pending = Signal()
        last_tag = Signal(16)
        any_packet = Signal()
        sequence = Signal(16)
        with m.If(self.enable & ~enable_q):
            m.d.sync += any_packet.eq(0)

        # Closed packets' headers, per slot.
        def slots(width, name):
            return Array(Signal(width, name=f'{name}{i}') for i in range(2))
        h_flags, h_count, h_tag = slots(8, 'h_flags'), slots(11, 'h_count'), slots(16, 'h_tag')
        h_index, h_power, h_peak = slots(64, 'h_index'), slots(48, 'h_power'), slots(16, 'h_peak')
        h_nco, h_seq, h_clips = slots(28, 'h_nco'), slots(16, 'h_seq'), slots(32, 'h_clips')
        h_xor = slots(32, 'h_xor')

        abs_re = Mux(s_re < 0, -s_re, s_re)[:16]
        abs_im = Mux(s_im < 0, -s_im, s_im)[:16]
        biggest = Mux(abs_re > abs_im, abs_re, abs_im)
        energy = Signal(32)
        m.d.comb += energy.eq((s_re * s_re + s_im * s_im)[:32])

        half_word = Cat(half_re, half_im, C(0, 32))
        pair_word = Cat(half_re, half_im, s_re, s_im)
        word_addr = Cat(((count >> 1) + HEADER_WORDS)[:9], fill)

        power_with = Signal(48)
        peak_with = Signal(16)
        m.d.comb += [
            power_with.eq(p_power + energy),
            peak_with.eq(Mux(biggest > p_peak, biggest, p_peak)),
        ]

        def close(flags_extra, count_value, xor_value, power=p_power, peak=p_peak):
            return [
                h_flags[fill].eq(p_flags | flags_extra),
                h_count[fill].eq(count_value),
                h_tag[fill].eq(p_tag),
                h_index[fill].eq(p_index),
                h_power[fill].eq(power),
                h_peak[fill].eq(peak),
                h_nco[fill].eq(p_nco),
                h_seq[fill].eq(sequence),
                h_clips[fill].eq(p_clips),
                h_xor[fill].eq(xor_value),
                full.bit_select(fill, 1).eq(1),
                fill.eq(~fill),
                sequence.eq(sequence + 1),
                last_tag.eq(p_tag),
                any_packet.eq(1),
                count.eq(0),
            ]

        def close_with_half(flags_extra):
            # A packet with an odd count: its last word holds one sample, written now.
            stmts = close(flags_extra, count, Mux(count[0], p_xor ^ _fold(half_word), p_xor))
            with m.If(count[0]):
                m.d.comb += [wr.en.eq(1), wr.addr.eq(word_addr), wr.data.eq(half_word)]
            return stmts

        with m.If(s_valid):
            with m.If((count != 0) & (s_tag != p_tag)):
                # Another tag: close, and store this sample next cycle.
                m.d.sync += close_with_half(0)
            with m.Elif(~can_fill):
                m.d.comb += self.lost.eq(1)
                m.d.sync += [lost_pending.eq(1), s_valid.eq(0)]
            with m.Else():
                m.d.sync += s_valid.eq(0)
                with m.If(count == 0):
                    m.d.sync += [
                        p_tag.eq(s_tag), p_index.eq(s_index), p_nco.eq(s_nco),
                        p_clips.eq(s_clips),
                        p_flags.eq(Mux(lost_pending, FLAG_LOST, 0)
                                   | Mux(~any_packet | (s_tag != last_tag), FLAG_RETUNED, 0)),
                        p_power.eq(energy), p_peak.eq(biggest), p_xor.eq(0),
                        lost_pending.eq(0),
                    ]
                with m.Else():
                    m.d.sync += [p_power.eq(power_with), p_peak.eq(peak_with)]
                with m.If(count[0]):
                    # The second sample of a word.
                    m.d.comb += [wr.en.eq(1), wr.addr.eq(word_addr), wr.data.eq(pair_word)]
                    with m.If(count + 1 == MAX_SAMPLES):
                        # Full: this sample is the packet's last.
                        m.d.sync += close(0, MAX_SAMPLES, p_xor ^ _fold(pair_word),
                                          power_with, peak_with)
                    with m.Else():
                        m.d.sync += [p_xor.eq(p_xor ^ _fold(pair_word)), count.eq(count + 1)]
                with m.Else():
                    m.d.sync += [half_re.eq(s_re), half_im.eq(s_im), count.eq(count + 1)]
        with m.Elif(disable_pending):
            m.d.sync += disable_pending.eq(0)
            with m.If(count != 0):
                m.d.sync += close_with_half(FLAG_LAST)
        # A sample captured this cycle is processed from the next one.
        with m.If(self.strobe_in & self.enable):
            m.d.sync += s_valid.eq(1)

        # Read side: the header from registers, the IQ from the slot, words past the samples zero.
        slot = rd_slot
        hw = Array([
            Cat(C(MAGIC, 16), C(FORMAT, 4), C(self.lane, 4), h_flags[slot], h_count[slot],
                C(0, 5), h_tag[slot]),
            h_index[slot],
            Cat(h_power[slot], h_peak[slot]),
            Cat(h_nco[slot], C(0, 4), h_seq[slot], C(0, 16)),
            Cat(h_clips[slot], C(0, 32)),
            C(0, 64),
            C(0, 64),
            C(0, 64),
        ])
        check = Signal(32)
        m.d.comb += check.eq(h_xor[slot] ^ _fold(hw[0]) ^ _fold(hw[1]) ^ _fold(hw[2])
                             ^ _fold(hw[3]) ^ _fold(hw[4]))
        words = Signal(range(PACKET_WORDS + 1))
        m.d.comb += words.eq(HEADER_WORDS + ((h_count[slot] + 1) >> 1))
        m.d.comb += [rd.addr.eq(Cat(self.rd_addr, slot)), rd.en.eq(self.rd_en)]
        r_header = Signal(64)
        r_is_header = Signal()
        r_zero = Signal()
        with m.If(self.rd_en):
            m.d.sync += [
                r_is_header.eq(self.rd_addr < HEADER_WORDS),
                r_header.eq(Mux(self.rd_addr == HEADER_WORDS - 1, check,
                                hw[self.rd_addr[:3]])),
                r_zero.eq(self.rd_addr >= words),
            ]
        m.d.comb += self.rd_data.eq(Mux(r_is_header, r_header, Mux(r_zero, 0, rd.data)))
        return m

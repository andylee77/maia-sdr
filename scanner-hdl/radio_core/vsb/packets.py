#
# Fishball radio core - the 8-VSB receiver's packets for the lane ring (079's lane packet, 4 KB of
# 64-bit words, lane 15), in sync. Each source has two slots of block RAM: one fills while the
# other waits for the ring, which reads it as it reads a lane's (lane_packetizer.py's read side).
#
# Codewords, format 2 (pl/packets.rs): an 8-word header, then 19 codewords of 26 words each, 207
# bytes and the flag. The header describes the first codeword's field:
#
#   0  magic, format 2, lane 15, flags (bit 0 lost, bit 1 segments counted, bit 2 fields counted,
#      bit 3 the timing loop tracking at the field sync, bit 4 a run after the fields were lost),
#      codewords, sequence
#   1  the AD9361 sample index at the field sync
#   2  the field, the codeword's place in it, the tap sets loaded before it
#   3  the error energy of the field before it, in 1/256 of a level squared
#   4  the carrier loop's frequency and the timing loop's rate at the field sync
#   5  the gain word, the field sync segment's sync correlation
#   7  check: the XOR of the packet's other 32-bit halves
#
# Snapshots, format 3 (doc/changes/085): the equalizer's input around a field sync, 9,984 values
# (the segment before the sync, the field sync segment, 10 data segments) in five packets of up to
# 2,016 values, four a word. The header: magic, format 3, lane 15, flags (bit 0 lost, bits 1-3 as
# format 2's, bit 4 the field sync inverted, bit 5 found), values, sequence; the sample index at the
# field sync; the field, the first value's place in the snapshot, the tap sets loaded before it.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *
from amaranth.lib.memory import Memory

from .tables import FIELD_SEGMENTS, SEGMENT

MAGIC = 0x5243
LANE = 15
PACKET_WORDS = 512
HEADER_WORDS = 8
CODEWORDS = 19
SLOT_WORDS = 26
SNAPSHOT_SEGMENTS = 12
SNAPSHOT_VALUES = SNAPSHOT_SEGMENTS * SEGMENT
VALUES_PER_PACKET = (PACKET_WORDS - HEADER_WORDS) * 4


def _fold(word):
    return word[:32] ^ word[32:]


def _sign_extend(value, width):
    return Cat(value, value[-1].replicate(width - len(value)))


class FieldInfo:
    """What a header needs of a field: a slot of the block's table."""
    def __init__(self, name):
        self.field = Signal(32, name=f'{name}_field')
        self.index = Signal(64, name=f'{name}_index')
        self.freq = Signal(32, name=f'{name}_freq')
        self.rate = Signal(32, name=f'{name}_rate')
        self.gain = Signal(12, name=f'{name}_gain')
        self.sync = Signal(signed(16), name=f'{name}_sync')
        self.tracking = Signal(name=f'{name}_tracking')
        self.inverted = Signal(name=f'{name}_inverted')
        self.found = Signal(name=f'{name}_found')
        self.loaded = Signal(16, name=f'{name}_loaded')
        self.energy_field = Signal(32, name=f'{name}_energy_field')
        self.energy = Signal(48, name=f'{name}_energy')

    def fields(self):
        return [self.field, self.index, self.freq, self.rate, self.gain, self.sync, self.tracking,
                self.inverted, self.found, self.loaded, self.energy_field, self.energy]


class _Slots(Elaboratable):
    """Two slots of packet RAM with the lane ring's read side. The filling side writes bytes
    (`w_en`, `w_addr` a word, `w_data`, `w_lanes` the bytes written) and closes its slot with
    header words 0-6, its payload's XOR and its words (`close`); words past them read as zero."""
    def __init__(self):
        self.w_en = Signal()
        self.w_addr = Signal(9)
        self.w_data = Signal(64)
        self.w_lanes = Signal(8)
        self.can_fill = Signal()
        self.close = Signal()
        self.close_header = [Signal(64, name=f'close_h{i}') for i in range(7)]
        self.close_xor = Signal(32)
        self.close_words = Signal(10)

        self.ready = Signal()
        self.rd_addr = Signal(9)
        self.rd_en = Signal()
        self.rd_data = Signal(64)
        self.release = Signal()

    def elaborate(self, platform):
        m = Module()
        m.submodules.mem = mem = Memory(shape=64, depth=2 * PACKET_WORDS, init=[])
        wr = mem.write_port(granularity=8)
        rd = mem.read_port(transparent_for=())
        fill = Signal()
        m.d.comb += [wr.addr.eq(Cat(self.w_addr, fill)), wr.data.eq(self.w_data),
                     wr.en.eq(Mux(self.w_en, self.w_lanes, 0))]

        full = Signal(2)
        rd_slot = Signal()
        m.d.comb += [self.ready.eq(full.bit_select(rd_slot, 1)),
                     self.can_fill.eq(~full.bit_select(fill, 1))]
        with m.If(self.release):
            m.d.sync += [full.bit_select(rd_slot, 1).eq(0), rd_slot.eq(~rd_slot)]

        header = [Array(Signal(64, name=f'h{w}_{s}') for s in range(2)) for w in range(7)]
        xors = Array(Signal(32, name=f'xor{s}') for s in range(2))
        words = Array(Signal(10, name=f'words{s}') for s in range(2))
        with m.If(self.close):
            m.d.sync += [full.bit_select(fill, 1).eq(1), fill.eq(~fill),
                         xors[fill].eq(self.close_xor), words[fill].eq(self.close_words)]
            for w in range(7):
                m.d.sync += header[w][fill].eq(self.close_header[w])

        slot = rd_slot
        check = Signal(32)
        m.d.comb += check.eq(xors[slot] ^ _fold(header[0][slot]) ^ _fold(header[1][slot])
                             ^ _fold(header[2][slot]) ^ _fold(header[3][slot])
                             ^ _fold(header[4][slot]) ^ _fold(header[5][slot])
                             ^ _fold(header[6][slot]))
        hw = Array([header[w][slot] for w in range(7)] + [Cat(check, C(0, 32))])
        m.d.comb += [rd.addr.eq(Cat(self.rd_addr, slot)), rd.en.eq(self.rd_en)]
        r_header = Signal(64)
        r_is_header = Signal()
        r_zero = Signal()
        with m.If(self.rd_en):
            m.d.sync += [
                r_is_header.eq(self.rd_addr < HEADER_WORDS),
                r_header.eq(hw[self.rd_addr[:3]]),
                r_zero.eq(self.rd_addr >= words[slot]),
            ]
        m.d.comb += self.rd_data.eq(Mux(r_is_header, r_header, Mux(r_zero, 0, rd.data)))
        return m


class _ReadSide:
    def _expose(self, slots):
        self.slots = slots
        self.ready = slots.ready
        self.rd_addr = slots.rd_addr
        self.rd_en = slots.rd_en
        self.rd_data = slots.rd_data
        self.release = slots.release


class CodewordPackets(Elaboratable, _ReadSide):
    """The codewords in format 2 packets.

    Attributes
    ----------
    in_valid, in_byte, in_place, in_codeword, in_field, in_end, in_flag, in_rerun : in
        The decoder's codeword bytes (decoder.py).
    flush : in
        Closes the packet being filled (a new run starts).
    info, info_before : FieldInfo, in
        The table's slots for the field ``in_field`` names and for the field before it; read at a
        packet's first byte.
    lost : out
        One cycle per codeword the slots could not take.
    ready, rd_addr, rd_en, rd_data, release
        The lane ring's read side.
    """
    def __init__(self):
        self.in_valid = Signal()
        self.in_byte = Signal(8)
        self.in_place = Signal(8)
        self.in_codeword = Signal(9)
        self.in_field = Signal(32)
        self.in_end = Signal()
        self.in_flag = Signal()
        self.in_rerun = Signal()
        self.flush = Signal()
        self.info = FieldInfo('info')
        self.info_before = FieldInfo('before')
        self.lost = Signal()
        self._expose(_Slots())

    def elaborate(self, platform):
        m = Module()
        m.submodules.slots = s = self.slots

        count = Signal(range(CODEWORDS))        # codewords done in the packet being filled
        base = Signal(9, init=HEADER_WORDS)     # the codeword's first word
        xor = Signal(32)
        sequence = Signal(16)
        lost_pending = Signal()
        dropping = Signal()
        h = [Signal(64, name=f'p_h{w}') for w in range(1, 7)]
        flags = Signal(8)

        opening = self.in_valid & (self.in_place == 0) & (count == 0)
        drop = Mux(opening, ~s.can_fill, dropping)
        writing = self.in_valid & ~drop
        # The byte at its place; the last with the flag in the byte after it (the same word).
        lane = self.in_place[:3]
        m.d.comb += [
            s.w_en.eq(writing),
            s.w_addr.eq(base + (self.in_place >> 3)),
            s.w_data.eq(Mux(self.in_end, Cat(C(0, 48), self.in_byte, self.in_flag, C(0, 7)),
                            self.in_byte << (lane * 8))),
            s.w_lanes.eq(Mux(self.in_end, 0b1100_0000, C(1, 8) << lane)),
        ]
        byte_xor = Signal(32)
        m.d.comb += byte_xor.eq(Mux(self.in_end, (self.in_byte << 16) | (self.in_flag << 24),
                                    self.in_byte << (self.in_place[:2] * 8)))

        i = self.info
        before = self.info_before
        with m.If(opening):
            m.d.sync += dropping.eq(~s.can_fill)
            with m.If(s.can_fill):
                m.d.sync += [
                    flags.eq(Cat(lost_pending, C(1, 1), C(1, 1), i.tracking, self.in_rerun, C(0, 3))),
                    h[0].eq(i.index),
                    h[1].eq(Cat(self.in_field, self.in_codeword, C(0, 7), i.loaded)),
                    h[2].eq(Mux(before.energy_field == self.in_field - 1, (before.energy + 8) >> 4, 0)),
                    h[3].eq(Cat(i.freq, i.rate)),
                    h[4].eq(Cat(i.gain, C(0, 20), _sign_extend(i.sync, 32))),
                    h[5].eq(0),
                    lost_pending.eq(0),
                ]
            with m.Else():
                m.d.sync += lost_pending.eq(1)
        with m.If(writing):
            m.d.sync += xor.eq(Mux(opening, 0, xor) ^ byte_xor)

        ending = self.in_valid & self.in_end
        last = count == CODEWORDS - 1
        m.d.comb += self.lost.eq(ending & drop)
        with m.If(ending):
            m.d.sync += [count.eq(Mux(last, 0, count + 1)),
                         base.eq(Mux(last, HEADER_WORDS, base + SLOT_WORDS))]

        # A full packet closes with its last codeword, a partial one on a flush.
        closing = ending & last & ~drop
        flushing = self.flush & (count != 0) & ~dropping
        codewords = Signal(16)
        m.d.comb += codewords.eq(Mux(closing, CODEWORDS, count))
        m.d.comb += [
            s.close.eq(closing | flushing),
            s.close_xor.eq(Mux(closing, xor ^ byte_xor, xor)),
            s.close_words.eq(HEADER_WORDS + SLOT_WORDS * codewords),
            s.close_header[0].eq(Cat(C(MAGIC, 16), C(2, 4), C(LANE, 4), flags, codewords, sequence)),
            *[s.close_header[w].eq(h[w - 1]) for w in range(1, 7)],
        ]
        with m.If(s.close):
            m.d.sync += sequence.eq(sequence + 1)
        with m.If(self.flush):
            m.d.sync += [count.eq(0), base.eq(HEADER_WORDS), dropping.eq(0)]
        return m


class SnapshotPackets(Elaboratable, _ReadSide):
    """The equalizer's input around a field sync, on request, in format 3 packets.

    Attributes
    ----------
    request : in
        Take the next snapshot: from the start of the next segment before a field sync.
    in_valid, in_y, in_fields, in_segment, in_place, in_field : in
        The front end's y and its place.
    info : FieldInfo, in
        The table's slot for the snapshot's field (``in_field`` + 1 while the snapshot runs).
    pending : out
        A snapshot is asked for or running.
    lost : out
        One cycle per value the slots could not take.
    """
    def __init__(self):
        self.request = Signal()
        self.in_valid = Signal()
        self.in_y = Signal(signed(12))
        self.in_fields = Signal()
        self.in_segment = Signal(9)
        self.in_place = Signal(10)
        self.in_field = Signal(32)
        self.info = FieldInfo('snap')
        self.snap_field = Signal(32)
        self.pending = Signal()
        self.lost = Signal()
        self._expose(_Slots())

    def elaborate(self, platform):
        m = Module()
        m.submodules.slots = s = self.slots

        asked = Signal()
        running = Signal()
        value = Signal(range(SNAPSHOT_VALUES))      # the next value's place in the snapshot
        in_packet = Signal(range(VALUES_PER_PACKET))
        packet_first = Signal(16)
        word = Signal(48)
        xor = Signal(32)
        sequence = Signal(16)
        lost = Signal()
        dropping = Signal()
        m.d.comb += self.pending.eq(asked | running)
        with m.If(self.request):
            m.d.sync += asked.eq(1)

        begin = (asked & self.in_valid & self.in_fields & (self.in_segment == FIELD_SEGMENTS - 1)
                 & (self.in_place == 0))
        with m.If(begin):
            m.d.sync += [asked.eq(0), running.eq(1), packet_first.eq(0),
                         self.snap_field.eq(self.in_field + 1), lost.eq(0), dropping.eq(~s.can_fill)]
        take = self.in_valid & (begin | running)
        # The value's place in the snapshot and in its packet: at the start, the first.
        place = Signal.like(value)
        place_in_packet = Signal.like(in_packet)
        m.d.comb += [place.eq(Mux(begin, 0, value)), place_in_packet.eq(Mux(begin, 0, in_packet))]
        opening = take & (place_in_packet == 0)
        drop = Mux(opening, ~s.can_fill, dropping)
        lane = place_in_packet[:2]
        full_word = Cat(word, _sign_extend(self.in_y, 16))
        m.d.comb += [
            s.w_en.eq(take & ~drop & (lane == 3)),
            s.w_addr.eq(HEADER_WORDS + (place_in_packet >> 2)),
            s.w_data.eq(full_word),
            s.w_lanes.eq(0xFF),
        ]
        last_value = place == SNAPSHOT_VALUES - 1
        last_in_packet = (place_in_packet == VALUES_PER_PACKET - 1) | last_value
        # A packet's last word may hold fewer than four values: written as the packet closes.
        partial = Signal(64)
        m.d.comb += partial.eq(Mux(lane == 0, _sign_extend(self.in_y, 16),
                               Mux(lane == 1, Cat(word[32:48], _sign_extend(self.in_y, 16)),
                                   Cat(word[16:48], _sign_extend(self.in_y, 16)))))
        with m.If(take & ~drop & last_in_packet & (lane != 3)):
            m.d.comb += [s.w_en.eq(1), s.w_data.eq(partial)]
        with m.If(take):
            m.d.sync += [word.eq(Cat(word[16:], _sign_extend(self.in_y, 16))),
                         value.eq(place + 1), in_packet.eq(place_in_packet + 1)]
            with m.If(opening):
                m.d.sync += [dropping.eq(~s.can_fill), xor.eq(0)]
                with m.If(~s.can_fill):
                    m.d.sync += lost.eq(1)
            with m.If((lane == 3) & ~drop):
                m.d.sync += xor.eq(Mux(opening, 0, xor) ^ _fold(full_word))
        m.d.comb += self.lost.eq(take & drop)

        closing = take & last_in_packet & ~drop
        values = Signal(16)
        m.d.comb += values.eq(place_in_packet + 1)
        i = self.info
        flags = Cat(lost, C(1, 1), C(1, 1), i.tracking, i.inverted, i.found, C(0, 2))
        before = Mux(opening, 0, xor)
        xor_end = Mux(lane == 3, before ^ _fold(full_word), before ^ _fold(partial))
        m.d.comb += [
            s.close.eq(closing),
            s.close_xor.eq(xor_end),
            s.close_words.eq(HEADER_WORDS + ((values + 3) >> 2)),
            s.close_header[0].eq(Cat(C(MAGIC, 16), C(3, 4), C(LANE, 4), flags, values, sequence)),
            s.close_header[1].eq(i.index),
            s.close_header[2].eq(Cat(self.snap_field, packet_first, i.loaded)),
            *[s.close_header[w].eq(0) for w in range(3, 7)],
        ]
        with m.If(take & last_in_packet):
            m.d.sync += [in_packet.eq(0), packet_first.eq(place + 1)]
            with m.If(last_value):
                m.d.sync += running.eq(0)
        with m.If(s.close):
            m.d.sync += sequence.eq(sequence + 1)
        return m

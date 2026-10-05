#
# Fishball radio core - the 8-VSB decoder from the equalizer's output to codewords (pl/decoder.rs,
# pl/mod.rs `back`), in sync.
#
# A run starts at a counted field's first symbol and lasts while the fields stay counted; the
# first run's first field is the model's field 0. In a run every data symbol (segments 1-312,
# places 4-831) goes to the trellis decoders (trellis.py), and each field's error energy, the
# squared distance of its data symbols to the nearest level, is summed for the packets. The
# decoders' bytes finish a group of 12 at a time, the groups in order, so a 64-byte reorder
# buffer hands the deinterleaver (deinterleaver.py) the field's bytes in order.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *
from amaranth.lib.memory import Memory

from .deinterleaver import Deinterleaver
from .tables import DATA_SEGMENTS, SYNC_SYMBOLS
from .trellis import TrellisDecoders

E_WIDTH = 12
LEVEL = 1 << 6
REORDER = 64


class Decoder(Elaboratable):
    """The decoder.

    Attributes
    ----------
    in_valid, in_e : in
        The equalizer's output, with its symbol's place: ``in_fields`` (fields counted),
        ``in_segment``, ``in_place``, ``in_field`` (the front end's field number) and
        ``in_loaded`` (tap sets loaded by then).
    out_* : out
        The deinterleaver's codeword bytes (deinterleaver.py), with ``out_field`` the front end's
        field number and ``out_rerun`` set from a run after the first.
    energy_valid, energy_field, energy : out
        A field's error energy (in the equalizer's output units squared), at the next field's start.
    field_start, start_field, start_loaded : out
        One cycle at a counted field's first symbol: its number and the tap sets loaded by then.
    restart : out
        One cycle: a run starts.
    running : out
    """
    def __init__(self):
        self.in_valid = Signal()
        self.in_e = Signal(signed(E_WIDTH))
        self.in_fields = Signal()
        self.in_segment = Signal(9)
        self.in_place = Signal(10)
        self.in_field = Signal(32)
        self.in_loaded = Signal(16)

        self.trellis = TrellisDecoders()
        self.deinterleaver = Deinterleaver()

        self.out_valid = Signal()
        self.out_byte = Signal(8)
        self.out_place = Signal(8)
        self.out_codeword = Signal(9)
        self.out_field = Signal(32)
        self.out_end = Signal()
        self.out_flag = Signal()
        self.out_rerun = Signal()

        self.energy_valid = Signal()
        self.energy_field = Signal(32)
        self.energy = Signal(48)
        self.field_start = Signal()
        self.start_field = Signal(32)
        self.start_loaded = Signal(16)
        self.restart = Signal()
        self.running = Signal()

    def elaborate(self, platform):
        m = Module()
        m.submodules.trellis = tr = self.trellis
        m.submodules.deinterleaver = di = self.deinterleaver

        # ── Runs ─────────────────────────────────────────────────────────────────────────
        at_field = self.in_valid & self.in_fields & (self.in_segment == 0) & (self.in_place == 0)
        run_field = Signal(32)
        runs = Signal(2)
        restart = self.restart
        m.d.comb += restart.eq(at_field & ~self.running)
        with m.If(restart):
            m.d.sync += [self.running.eq(1), run_field.eq(self.in_field),
                         runs.eq(Mux(runs == 3, 3, runs + 1))]
        with m.Elif(self.in_valid & ~self.in_fields):
            m.d.sync += self.running.eq(0)
        m.d.comb += [tr.restart.eq(restart), di.restart.eq(restart)]

        data = (self.in_valid & self.in_fields & (self.running | restart) & (self.in_segment != 0)
                & (self.in_place >= SYNC_SYMBOLS))
        m.d.comb += [tr.in_valid.eq(data), tr.in_e.eq(self.in_e),
                     tr.in_k.eq(self.in_place - SYNC_SYMBOLS)]

        # ── Each field's error energy, and its tap count ─────────────────────────────────
        level = Signal(range(8))
        idx = Signal(signed(E_WIDTH + 2))
        m.d.comb += [idx.eq((self.in_e + 8 * LEVEL) >> 7),
                     level.eq(Mux(idx < 0, 0, Mux(idx > 7, 7, idx)))]
        err = Signal(signed(E_WIDTH + 2))
        m.d.comb += err.eq(self.in_e - (level * 2 - 7) * LEVEL)
        e_q = Signal(signed(E_WIDTH + 2))
        e_data = Signal()
        sq = Signal(2 * (E_WIDTH + 2))
        sq_data = Signal()
        m.d.sync += [e_q.eq(err), e_data.eq(data), sq.eq(e_q * e_q), sq_data.eq(e_data)]
        acc = Signal(48)
        acc_field = Signal(32)
        with m.If(sq_data):
            m.d.sync += acc.eq(acc + sq)
        m.d.sync += [self.energy_valid.eq(0), self.field_start.eq(0)]
        with m.If(at_field):
            m.d.sync += [self.field_start.eq(1), self.start_field.eq(self.in_field),
                         self.start_loaded.eq(self.in_loaded), acc_field.eq(self.in_field)]
            with m.If(self.running):
                # The field before's sum is whole: its last squares left the pipeline 832 symbols ago.
                m.d.sync += [self.energy_valid.eq(1), self.energy_field.eq(acc_field),
                             self.energy.eq(acc)]
            m.d.sync += acc.eq(0)

        # ── Reorder ──────────────────────────────────────────────────────────────────────
        m.submodules.reorder = reorder = Memory(shape=8, depth=REORDER, init=[])
        rw = reorder.write_port()
        rr = reorder.read_port(domain='comb')
        present = Signal(REORDER)
        rd_slot = Signal(range(REORDER))
        wr_slot = Signal(range(REORDER))
        m.d.comb += [
            wr_slot.eq(Cat(C(0, 3), tr.byte_field) + tr.byte_index),
            rw.en.eq(tr.byte_valid), rw.addr.eq(wr_slot), rw.data.eq(tr.byte),
            rr.addr.eq(rd_slot),
        ]
        take = present.bit_select(rd_slot, 1)
        m.d.comb += [di.in_valid.eq(take), di.in_byte.eq(rr.data)]
        with m.If(take):
            m.d.sync += [present.bit_select(rd_slot, 1).eq(0), rd_slot.eq(rd_slot + 1)]
        with m.If(tr.byte_valid):
            m.d.sync += present.bit_select(wr_slot, 1).eq(1)
        with m.If(restart):
            m.d.sync += [present.eq(0), rd_slot.eq(0)]

        m.d.comb += [
            self.out_valid.eq(di.out_valid), self.out_byte.eq(di.out_byte),
            self.out_place.eq(di.out_place), self.out_codeword.eq(di.out_codeword),
            self.out_field.eq(run_field + di.out_field), self.out_end.eq(di.out_end),
            self.out_flag.eq(di.out_flag), self.out_rerun.eq(runs > 1),
        ]
        return m

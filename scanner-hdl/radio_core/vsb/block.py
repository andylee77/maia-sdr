#
# Fishball radio core - the 8-VSB receiver block (doc/changes/085): the AD9361's samples to
# codewords and snapshots in the lane ring.
#
#   sync                         clk3x                                      sync
#   samples --(common edge)--> front.py -> y -> equalizer.py --(toggle)--> decoder.py -> codeword packets
#                                       |                    \-(toggle)--> snapshot packets
#                                       \-- field records ----(toggle)--> field table (headers)
#
# The front end and the equalizer run at clk3x; the decoder, the packets and the registers in sync.
# The samples cross into clk3x at the block's common edge, as the DDCs take them; the equalizer's
# output, the front end's y and its field records cross back each as a register held between items
# (at least 16 clk3x cycles apart) with a toggle, as the DDCs' outputs do. The clocks come from one
# MMCM and Vivado times these paths. Pulses from the registers reach clk3x at the common edge.
#
# `enable` low holds the whole block in reset; when it rises the block waits a few cycles for the
# clk3x half to leave its reset, then takes samples, the first one's AD9361 sample index the base
# of every header's index.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *

from .decoder import Decoder
from .equalizer import Equalizer
from .front import SEARCH_SEGMENTS, SETTLE, Front
from .packets import CodewordPackets, FieldInfo, SnapshotPackets

TAG_WIDTH = 1 + 9 + 10 + 32
WARM = 4


class VsbBlock(Elaboratable):
    """The 8-VSB receiver.

    Attributes
    ----------
    re_in, im_in, strobe_in : in (sync)
        The AD9361's samples; ``sample_count`` the index of the one in ``re_in``/``im_in``.
    common_edge : in (clk3x)
        The block's own common edge.
    enable, step0, carrier0 : in (sync)
        The registers (doc/changes/085, "Registers").
    snapshot, taps_arm : in (sync), one-cycle pulses
    mf_wen, mf_addr, mf_data : in (sync)
        A matched-filter table word.
    tap_wen, tap_addr, tap_data : in (sync)
        A tap into the idle set.
    status_* , taps_loaded, field_count, lost : out (sync)
    codewords, snapshots
        The packet sources (packets.py) for the lane ring.
    """
    def __init__(self, settle=SETTLE, search_segments=SEARCH_SEGMENTS):
        self.re_in = Signal(signed(12))
        self.im_in = Signal(signed(12))
        self.strobe_in = Signal()
        self.sample_count = Signal(64)
        self.common_edge = Signal()

        self.enable = Signal()
        self.step0 = Signal(32)
        self.carrier0 = Signal(signed(32))
        self.snapshot = Signal()
        self.taps_arm = Signal()
        self.mf_wen = Signal()
        self.mf_addr = Signal(9)
        self.mf_data = Signal(60)
        self.tap_wen = Signal()
        self.tap_addr = Signal(7)
        self.tap_data = Signal(signed(16))

        self.status_segments = Signal()
        self.status_fields = Signal()
        self.status_tracking = Signal()
        self.status_armed = Signal()
        self.status_snapshot = Signal()
        self.status_overrun = Signal()
        self.taps_loaded = Signal(16)
        self.field_count = Signal(32)
        self.lost = Signal(32)
        self.base = Signal(64)

        self.front = Front(settle, search_segments)
        self.equalizer = Equalizer(TAG_WIDTH)
        self.decoder = Decoder()
        self.codewords = CodewordPackets()
        self.snapshots = SnapshotPackets()

    def elaborate(self, platform):
        m = Module()

        # ── Resets: `enable` low holds both halves ───────────────────────────────────────
        en3_q = Signal()
        en3 = Signal()
        m.d.clk3x += [en3_q.eq(self.enable), en3.eq(en3_q)]
        to3 = DomainRenamer({'sync': 'clk3x'})
        m.submodules.front = front_m = ResetInserter({'clk3x': ~en3})(to3(self.front))
        m.submodules.equalizer = eq_m = ResetInserter({'clk3x': ~en3})(to3(self.equalizer))
        m.submodules.decoder = ResetInserter({'sync': ~self.enable})(self.decoder)
        m.submodules.codewords = ResetInserter({'sync': ~self.enable})(self.codewords)
        m.submodules.snapshots = ResetInserter({'sync': ~self.enable})(self.snapshots)
        front = self.front
        eq = self.equalizer
        dec = self.decoder
        cw = self.codewords
        sn = self.snapshots

        # ── The samples into clk3x ───────────────────────────────────────────────────────
        warm = Signal(range(WARM + 1))
        with m.If(~self.enable):
            m.d.sync += warm.eq(0)
        with m.Elif(warm < WARM):
            m.d.sync += warm.eq(warm + 1)
        accept = self.enable & (warm == WARM)
        started = Signal()
        s_re = Signal(signed(12), reset_less=True)
        s_im = Signal(signed(12), reset_less=True)
        s_strobe = Signal()
        m.d.sync += [s_re.eq(self.re_in), s_im.eq(self.im_in), s_strobe.eq(self.strobe_in & accept)]
        with m.If(self.strobe_in & accept & ~started):
            m.d.sync += [started.eq(1), self.base.eq(self.sample_count)]
        with m.If(~self.enable):
            m.d.sync += started.eq(0)
        m.d.comb += [
            front.wr_en.eq(self.common_edge & s_strobe),
            front.wr_re.eq(s_re), front.wr_im.eq(s_im),
            front.step0.eq(self.step0), front.carrier0.eq(self.carrier0),
            front.tw_en.eq(self.common_edge & self.mf_wen),
            front.tw_addr.eq(self.mf_addr), front.tw_data.eq(self.mf_data),
            eq.tap_wen.eq(self.common_edge & self.tap_wen),
            eq.tap_addr.eq(self.tap_addr), eq.tap_data.eq(self.tap_data),
            eq.arm.eq(self.common_edge & self.taps_arm),
            eq.in_valid.eq(front.y_valid), eq.in_y.eq(front.y),
            eq.in_tag.eq(Cat(front.y_fields, front.y_segment, front.y_place, front.y_field)),
        ]

        # ── clk3x to sync: a held register and a toggle per item ─────────────────────────
        def cross(valid, payload, name):
            toggle = Signal(name=f'{name}_toggle')
            hold = Signal(len(payload), name=f'{name}_hold', reset_less=True)
            with m.If(valid):
                m.d.clk3x += [toggle.eq(~toggle), hold.eq(payload)]
            seen = Signal(name=f'{name}_seen')
            strobe = Signal(name=f'{name}_strobe')
            out = Signal(len(payload), name=f'{name}_out', reset_less=True)
            m.d.sync += [seen.eq(toggle), strobe.eq(toggle ^ seen)]
            with m.If(toggle ^ seen):
                m.d.sync += out.eq(hold)
            return strobe, out

        e_strobe, e_out = cross(eq.out_valid, Cat(eq.out_e, eq.out_tag, eq.out_loaded), 'e')
        y_strobe, y_out = cross(front.y_valid,
                                Cat(front.y, front.y_fields, front.y_segment, front.y_place, front.y_field),
                                'y')
        record = Cat(front.field_no, front.field_index, front.field_found, front.field_inverted,
                     front.field_freq, front.field_rate, front.field_gain, front.field_sync,
                     front.field_tracking)
        r_strobe, r_out = cross(front.field_valid, record, 'record')

        # ── The decoder and the snapshots ────────────────────────────────────────────────
        tag = e_out[12:12 + TAG_WIDTH]
        m.d.comb += [
            dec.in_valid.eq(e_strobe), dec.in_e.eq(e_out[:12]),
            dec.in_fields.eq(tag[0]), dec.in_segment.eq(tag[1:10]), dec.in_place.eq(tag[10:20]),
            dec.in_field.eq(tag[20:52]), dec.in_loaded.eq(e_out[12 + TAG_WIDTH:]),
            sn.request.eq(self.snapshot),
            sn.in_valid.eq(y_strobe), sn.in_y.eq(y_out[:12]), sn.in_fields.eq(y_out[12]),
            sn.in_segment.eq(y_out[13:22]), sn.in_place.eq(y_out[22:32]), sn.in_field.eq(y_out[32:64]),
            cw.in_valid.eq(dec.out_valid), cw.in_byte.eq(dec.out_byte), cw.in_place.eq(dec.out_place),
            cw.in_codeword.eq(dec.out_codeword), cw.in_field.eq(dec.out_field),
            cw.in_end.eq(dec.out_end), cw.in_flag.eq(dec.out_flag), cw.in_rerun.eq(dec.out_rerun),
            cw.flush.eq(dec.restart),
        ]

        # ── The field table: four fields' header values ──────────────────────────────────
        slots = [FieldInfo(f'slot{i}') for i in range(4)]
        r_field = r_out[:32]
        rec = dict(index=r_out[32:96], found=r_out[96], inverted=r_out[97], freq=r_out[98:130],
                   rate=r_out[130:162], gain=r_out[162:174], sync=r_out[174:190], tracking=r_out[190])
        with m.If(r_strobe):
            with m.Switch(r_field[:2]):
                for i, s in enumerate(slots):
                    with m.Case(i):
                        m.d.sync += [s.field.eq(r_field), s.index.eq(self.base + rec['index']),
                                     s.found.eq(rec['found']), s.inverted.eq(rec['inverted']),
                                     s.freq.eq(rec['freq']), s.rate.eq(rec['rate']),
                                     s.gain.eq(rec['gain']), s.sync.eq(rec['sync']),
                                     s.tracking.eq(rec['tracking'])]
        with m.If(dec.field_start):
            with m.Switch(dec.start_field[:2]):
                for i, s in enumerate(slots):
                    with m.Case(i):
                        m.d.sync += s.loaded.eq(dec.start_loaded)
        with m.If(dec.energy_valid):
            with m.Switch(dec.energy_field[:2]):
                for i, s in enumerate(slots):
                    with m.Case(i):
                        m.d.sync += [s.energy_field.eq(dec.energy_field), s.energy.eq(dec.energy)]

        def read(info, field):
            with m.Switch(field[:2]):
                for i, s in enumerate(slots):
                    with m.Case(i):
                        m.d.comb += [a.eq(b) for a, b in zip(info.fields(), s.fields())]

        read(cw.info, cw.in_field)
        read(cw.info_before, cw.in_field - 1)
        read(sn.info, sn.snap_field)

        # ── Status ───────────────────────────────────────────────────────────────────────
        m.d.sync += [
            self.status_segments.eq(front.counting), self.status_fields.eq(front.fields_counted),
            self.status_tracking.eq(front.tracking), self.status_overrun.eq(front.overrun),
            self.status_armed.eq(eq.armed), self.taps_loaded.eq(eq.loaded),
            self.status_snapshot.eq(sn.pending),
        ]
        with m.If(r_strobe):
            m.d.sync += self.field_count.eq(r_field + 1)
        with m.If(cw.lost | sn.lost):
            m.d.sync += self.lost.eq(self.lost + 1)
        with m.If(~self.enable):
            m.d.sync += [self.field_count.eq(0), self.lost.eq(0)]
        return m

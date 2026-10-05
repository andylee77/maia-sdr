#
# Fishball radio core - the 8-VSB receiver over whole fields in Verilator against the model
# (doc/changes/085). Slow and needs a full vector set: MAIA_HDL_SLOW_TESTS=1 and VSB_VECTORS (a
# folder the model's `vectors_for_the_hdl` wrote), and the Verilator image (test/verilator.py).
#
# SPDX-License-Identifier: MIT
#

import os
import pathlib
import unittest

from amaranth import *

from . import verilator

SLOW = os.environ.get('MAIA_HDL_SLOW_TESTS') == '1'
VECTORS = os.environ.get('VSB_VECTORS')
BUILD = verilator.REPO / 'runs' / 'verilator'


def front_ports(front):
    sig = [front.wr_en, front.wr_re, front.wr_im, front.step0, front.carrier0, front.tw_en,
           front.tw_addr, front.tw_data,
           front.y_valid, front.y, front.y_phase, front.y_place, front.y_fields, front.y_segment,
           front.y_field, front.y_n, front.y_pos, *front.t_filtered, front.t_gain, *front.t_gained,
           front.t_theta, *front.t_turned, front.t_dc,
           front.side_valid, front.side_late, front.side_n, *front.side_filtered, front.side_theta,
           front.side_value,
           front.seg_valid, front.seg_start, front.seg_pos, front.seg_freq, front.seg_rate,
           front.seg_gain, front.seg_dc, front.seg_sync, front.seg_timing, front.seg_pn511,
           front.field_valid, front.field_no, front.field_start, front.field_index,
           front.field_found, front.field_inverted, front.field_freq, front.field_rate,
           front.field_gain, front.field_sync, front.field_tracking, front.field_lost,
           front.counting, front.fields_counted, front.tracking, front.overrun]
    return sig


class BlockTop(Elaboratable):
    """The 8-VSB block with its common edge, its packet sources' read sides as ports."""
    def __init__(self):
        from radio_core.vsb.block import VsbBlock
        self.block = VsbBlock()
        b = self.block
        self.cw = dict(ready=Signal(name='cw_ready'), rd_addr=Signal(9, name='cw_rd_addr'),
                       rd_en=Signal(name='cw_rd_en'), rd_data=Signal(64, name='cw_rd_data'),
                       release=Signal(name='cw_release'))
        self.sn = dict(ready=Signal(name='sn_ready'), rd_addr=Signal(9, name='sn_rd_addr'),
                       rd_en=Signal(name='sn_rd_en'), rd_data=Signal(64, name='sn_rd_data'),
                       release=Signal(name='sn_release'))
        self.sync = ClockDomain('sync')
        self.clk3x = ClockDomain('clk3x')
        self.ports = [self.sync.clk, self.sync.rst, self.clk3x.clk, self.clk3x.rst,
                      b.re_in, b.im_in, b.strobe_in, b.sample_count, b.enable, b.step0, b.carrier0,
                      b.snapshot, b.taps_arm, b.mf_wen, b.mf_addr, b.mf_data, b.tap_wen, b.tap_addr,
                      b.tap_data, b.status_segments, b.status_fields, b.status_tracking,
                      b.status_armed, b.status_snapshot, b.status_overrun, b.taps_loaded,
                      b.field_count, b.lost, *self.cw.values(), *self.sn.values()]

    def elaborate(self, platform):
        from radio_core.common_edge import CommonEdge
        m = Module()
        m.domains += [self.sync, self.clk3x]
        m.submodules.block = b = self.block
        m.submodules.edge = edge = CommonEdge('sync', 'clk3x', 3, 'ce3x_vsb')
        m.d.comb += b.common_edge.eq(edge.common_edge)
        for src, sig in ((b.codewords, self.cw), (b.snapshots, self.sn)):
            m.d.comb += [sig['ready'].eq(src.ready), src.rd_addr.eq(sig['rd_addr']),
                         src.rd_en.eq(sig['rd_en']), sig['rd_data'].eq(src.rd_data),
                         src.release.eq(sig['release'])]
        return m


@unittest.skipUnless(SLOW and VECTORS and verilator.available(),
                     'whole fields: MAIA_HDL_SLOW_TESTS=1, VSB_VECTORS and the Verilator image')
class TestWholeBlock(unittest.TestCase):
    """The block from a capture's samples to the lane ring's packets: every codeword packet the
    model makes, word for word, the model's tap loads applied as it applies them, and every value
    of the snapshots taken for its solves."""
    def test_the_block(self):
        top = BlockTop()
        name = pathlib.Path(VECTORS).name
        out = BUILD / 'block' / name
        result = out / f'{name}.json'
        verilator.run(top, top.ports, 'block', 'block_tb.cpp',
                      [verilator.in_container(VECTORS), verilator.in_container(result)], out)
        s = verilator.summary(result)
        print(s)
        self.assertEqual(s['bad_packets'], 0, s['first'])
        self.assertEqual(s['compared'], s['model_packets'])
        self.assertEqual(s['bad_values'], 0, s['snapshot_first'])
        # A snapshot for each of the model's tap sets, five packets each at their places.
        loads = (pathlib.Path(VECTORS) / 'loads.bin').stat().st_size // 264
        self.assertEqual(s['snapshot_packets'], 5 * loads)
        self.assertEqual([h[1] for h in s['snapshot_headers']],
                         [0, 2016, 4032, 6048, 8064] * loads)
        self.assertEqual([h[2] for h in s['snapshot_headers']],
                         [2016, 2016, 2016, 2016, 1920] * loads)
        self.assertEqual(s['taps_loaded'], loads)
        self.assertEqual((s['lost'], s['overrun']), (0, 0))


@unittest.skipUnless(SLOW and VECTORS and verilator.available(),
                     'whole fields: MAIA_HDL_SLOW_TESTS=1, VSB_VECTORS and the Verilator image')
class TestWholeFront(unittest.TestCase):
    """The front end on a capture from the reset through its fields: every symbol's stages and
    place, every early and late evaluation, every segment's and field's record."""
    def test_the_front_end(self):
        from radio_core.vsb.front import Front
        front = Front()
        name = pathlib.Path(VECTORS).name
        out = BUILD / 'front' / name
        result = out / f'{name}.json'
        verilator.run(front, front_ports(front), 'front', 'front_tb.cpp',
                      [verilator.in_container(VECTORS), verilator.in_container(result), '100000000'],
                      out)
        s = verilator.summary(result)
        print(s)
        for part in ('symbol', 'place', 'side', 'segment', 'field'):
            self.assertEqual(s[part]['mismatches'], 0, f'{part}: {s[part]["first"]}')
        self.assertGreater(s['fields'], 0)
        self.assertEqual(s['overrun'], 0)


if __name__ == '__main__':
    unittest.main()

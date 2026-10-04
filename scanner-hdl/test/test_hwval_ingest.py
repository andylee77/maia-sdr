#
# Fishball hwval -- IngestMonitor / IngestCDC tests
#
# Covers:
#
#   1.  PRBS reference definitions: pn_step_masks(24, ...) reproduces the
#       ADI pn1fn PRBS_P09 / PRBS_P11 tables (parsed from the adi-hdl
#       Verilog when the submodule is present), and the reference
#       generators lock a cycle-accurate Python port of
#       axi_ad9361_rx_pnmon + ad_pnmon with no errors.
#   2.  Window statistics vs numpy on random data with valid gaps
#       (honor_valid 1 and 0), sample / gap / gap-run counters.
#   3.  Stuck-bit masks, clip count, empty-window values.
#   4.  Snap windows: clear_on_snap splits a live stream with no sample
#       lost or double counted; without it the window is cumulative.
#   5.  PRBS checker (both modes): lock with 0 errors, exact error count
#       for single-sample corruptions, one dropped sample -> 1 OOS event,
#       16 errors, resync; valid gaps with honor_valid 1 are transparent.
#   6.  IngestCDC: no loss when the output clock is faster, honor_valid,
#       overflow accounting when the output clock is much slower.
#

import os
import re
import unittest

import numpy as np
from amaranth import *
from amaranth.sim import Simulator

from hwval_hdl.ingest import (
    IngestMonitor, IngestCDC, ad9361_bist_prbs, pn9_pn11, pn0fn, bitrev12,
    pn_step, pn_step_masks, PN9_TAPS, PN11_TAPS)


ADI_PNMON = os.path.join(
    os.path.dirname(__file__), '..', '..', 'maia-hdl', 'adi-hdl', 'library', 'axi_ad9361',
    'axi_ad9361_rx_pnmon.v')


def run_sim(dut, benches, clocks):
    sim = Simulator(dut)
    for domain, period in clocks.items():
        sim.add_clock(period, domain=domain)
    for bench in benches:
        sim.add_testbench(bench)
    sim.run()


def to_signed12(x):
    x &= 0xfff
    return x - 4096 if x & 0x800 else x


# ---------------------------------------------------------------------------
# Python port of the ADI PN monitor (reference for the reference)
# ---------------------------------------------------------------------------

class AdiPnmonModel:
    """Cycle-accurate port of axi_ad9361_rx_pnmon + ad_pnmon.

    One instance per channel, like the ADI core. pn1 uses
    pn_step_masks(24, taps), which test_pn1fn_tables checks against the
    Verilog.
    """
    def __init__(self, q_or_i_n, prbs_sel, pnseq_sel):
        self.q_or_i_n = q_or_i_n
        self.masks = pn_step_masks(24, PN9_TAPS if prbs_sel == 0
                                   else PN11_TAPS)
        self.pnseq_sel = pnseq_sel
        r = dict(pn0_valid=0, pn0_data=0, pn0_valid_in=0, pn0_data_in=0,
                 pn0_data_pn=0, pn1_valid_t=0, pn1_data_d=0,
                 pn1_valid_in=0, pn1_data_in=0, pn1_data_pn=0,
                 pn_valid_in=0, pn_data_in=0, pn_data_pn=0,
                 valid_d=0, match_d=0, match_z=0, oos=0, err=0,
                 oos_count=0)
        self.r = r

    def clock(self, valid, data_i, data_q):
        r = self.r
        n = dict(r)
        i_s = data_q if self.q_or_i_n else data_i
        q_s = data_i if self.q_or_i_n else data_q
        q_rev = bitrev12(q_s)
        pn0_data_s = (i_s << 4) | (q_rev & 0xf)
        iq_match = (i_s & 0xff) == (q_rev >> 4)
        pn0_data_pn_s = r['pn0_data_in'] if r['oos'] else r['pn0_data_pn']
        n['pn0_valid'] = valid
        n['pn0_data'] = pn0_data_s if iq_match else 0xdead
        n['pn0_valid_in'] = r['pn0_valid']
        if r['pn0_valid']:
            n['pn0_data_in'] = r['pn0_data']
            n['pn0_data_pn'] = pn0fn(pn0_data_pn_s)
        pn1_valid_s = r['pn1_valid_t'] & valid
        pn1_data_pn_s = r['pn1_data_in'] if r['oos'] else r['pn1_data_pn']
        if valid:
            n['pn1_valid_t'] = 1 - r['pn1_valid_t']
            n['pn1_data_d'] = data_i
        n['pn1_valid_in'] = pn1_valid_s
        if pn1_valid_s:
            n['pn1_data_in'] = (r['pn1_data_d'] << 12) | data_i
            n['pn1_data_pn'] = pn_step(pn1_data_pn_s, self.masks)
        if self.pnseq_sel == 9:
            n['pn_valid_in'] = r['pn1_valid_in']
            n['pn_data_in'] = r['pn1_data_in']
            n['pn_data_pn'] = r['pn1_data_pn']
        else:
            n['pn_valid_in'] = r['pn0_valid_in']
            n['pn_data_in'] = ((r['pn0_data_in'] & 0xff) << 16
                               | r['pn0_data_in'])
            n['pn_data_pn'] = ((r['pn0_data_pn'] & 0xff) << 16
                               | r['pn0_data_pn'])
        # ad_pnmon (DATA_WIDTH 24, OOS_THRESHOLD 16, no zero masking)
        match_d_s = r['pn_data_in'] == r['pn_data_pn']
        match_z_s = r['pn_data_in'] == 0
        match_s = r['match_d'] and not r['match_z']
        update_s = not (r['oos'] ^ match_s)
        err_s = not (r['oos'] or match_s)
        n['valid_d'] = r['pn_valid_in']
        n['match_d'] = match_d_s
        n['match_z'] = match_z_s
        if r['valid_d']:
            n['err'] = int(err_s)
            if update_s and r['oos_count'] >= 15:
                n['oos'] = 1 - r['oos']
            n['oos_count'] = (r['oos_count'] + 1) % 16 if update_s else 0
        self.r = n
        return n['oos'], n['err']


def run_adi_model(iq, pnseq_sel):
    """Feed (i, q) samples (valid every cycle) to both ADI channel
    monitors; return per-channel lists of (oos, err) per cycle."""
    ch_i = AdiPnmonModel(0, 0, pnseq_sel)
    ch_q = AdiPnmonModel(1, 1, pnseq_sel)
    out_i, out_q = [], []
    for i, q in iq:
        out_i.append(ch_i.clock(1, i, q))
        # the Q channel core gets adc_data = Q, adc_data_q = I
        out_q.append(ch_q.clock(1, q, i))
    return out_i, out_q


class TestPrbsReference(unittest.TestCase):
    @unittest.skipUnless(os.path.exists(ADI_PNMON),
                         'adi-hdl submodule not present')
    def test_pn1fn_tables(self):
        with open(ADI_PNMON) as f:
            text = f.read()
        for case, taps in [('PRBS_P09', PN9_TAPS), ('PRBS_P11', PN11_TAPS)]:
            block = text.split(f'{case}: begin')[1].split('end')[0]
            masks = [0] * 24
            for bit, expr in re.findall(
                    r'dout\[\s*(\d+)\]\s*=\s*([^;]+);', block):
                mask = 0
                for k in re.findall(r'din\[\s*(\d+)\]', expr):
                    mask ^= 1 << int(k)
                masks[int(bit)] = mask
            self.assertEqual(masks, pn_step_masks(24, taps), case)

    def test_bist_prbs_structure(self):
        seq = list(ad9361_bist_prbs(70000, seed=0x1234))
        prev = None
        for i, q in seq:
            q_rev = bitrev12(q)
            self.assertEqual(i & 0xff, q_rev >> 4)
            s = (i << 4) | (q_rev & 0xf)
            if prev is not None:
                self.assertEqual(s, pn0fn(prev))
            prev = s
        self.assertEqual(seq[0], seq[65535])  # maximal length
        self.assertNotEqual(seq[0], seq[1])

    def test_pn9_pn11_structure(self):
        seq = list(pn9_pn11(2100))
        self.assertEqual(seq[0], (0xfff, 0xfff))
        self.assertEqual(seq[1], (0xfff, 0xfff))
        m9 = pn_step_masks(12, PN9_TAPS)
        m11 = pn_step_masks(12, PN11_TAPS)
        for k in range(2, len(seq)):
            self.assertEqual(seq[k][0], pn_step(seq[k - 1][0], m9))
            self.assertEqual(seq[k][1], pn_step(seq[k - 1][1], m11))
        # PN9 period is 511 bits; 511 samples of 12 bits = 12 periods
        self.assertEqual(seq[2][0], seq[2 + 511][0])
        self.assertEqual(seq[2][1], seq[2 + 2047][1])

    def test_adi_model_locks_on_references(self):
        # pn_sel 0: both channel monitors check the BIST PRBS
        oi, oq = run_adi_model(ad9361_bist_prbs(400, seed=0x0bad), 0)
        for out in (oi, oq):
            self.assertTrue(all(o == (0, 0) for o in out[100:]))
            self.assertEqual(out[-1], (0, 0))
        # corrupted input must be flagged by the ADI model too
        bad = list(ad9361_bist_prbs(400, seed=0x0bad))
        for k in range(200, 260):
            bad[k] = (bad[k][0] ^ 0x010, bad[k][1])
        oi, _ = run_adi_model(bad, 0)
        self.assertTrue(any(err for _, err in oi[200:270]))
        self.assertEqual(oi[-1], (0, 0))
        # pn_sel 9: channel I checks PN9, channel Q checks PN11
        oi, oq = run_adi_model(pn9_pn11(600), 9)
        for out in (oi, oq):
            self.assertTrue(all(o == (0, 0) for o in out[100:]))
        # the ADI model rejects the swapped assignment
        swapped = [(q, i) for i, q in pn9_pn11(600)]
        oi, oq = run_adi_model(swapped, 9)
        self.assertEqual(oi[-1][0], 1)
        self.assertEqual(oq[-1][0], 1)


# ---------------------------------------------------------------------------
# IngestMonitor
# ---------------------------------------------------------------------------

class MonitorHarness:
    """Test-bench helpers for IngestMonitor in the `sampling` domain."""
    def __init__(self, dut):
        self.dut = dut

    async def pulse(self, ctx, sig):
        ctx.set(sig, 1)
        await ctx.tick('sampling')
        ctx.set(sig, 0)

    async def feed(self, ctx, iq, valid, stats=True, prbs_hold=False):
        """Present samples so that exactly `iq` / `valid` reach the
        stats counters (stats_enable is aligned with the 2-stage
        pipeline ahead of stage C). Assumes stats_enable == 0 on entry.
        """
        dut = self.dut
        n = len(iq)
        seq = [((0, 0), 1)] * 2 + list(zip(iq, valid)) + [((0, 0), 1)] * 3
        for k, ((i, q), v) in enumerate(seq):
            j = k - 2   # index into iq
            ctx.set(dut.re_in, int(i) & 0xfff)
            ctx.set(dut.im_in, int(q) & 0xfff)
            ctx.set(dut.valid_in, int(v))
            if stats:
                # input j is counted iff stats_enable is high while input
                # j + 2 is presented
                ctx.set(dut.stats_enable, int(2 <= j < n + 2))
            await ctx.tick('sampling')
        ctx.set(dut.valid_in, 0)
        ctx.set(dut.stats_enable, 0)
        for _ in range(4):
            await ctx.tick('sampling')

    async def snap(self, ctx):
        ctx.set(self.dut.snap, 1)
        await ctx.tick('sampling')
        ctx.set(self.dut.snap, 0)
        assert ctx.get(self.dut.snap_done)

    def read_window(self, ctx):
        dut = self.dut
        names = ['i_min', 'i_max', 'q_min', 'q_max', 'i_sum', 'q_sum',
                 'i_sumsq', 'q_sumsq', 'clip_count', 'i_or_mask',
                 'i_and_mask', 'q_or_mask', 'q_and_mask', 'win_samples']
        return {name: ctx.get(getattr(dut, name)) for name in names}


def expected_window(i_vals, q_vals):
    i_vals = np.asarray(i_vals, dtype=np.int64)
    q_vals = np.asarray(q_vals, dtype=np.int64)
    if len(i_vals) == 0:
        return dict(i_min=2047, i_max=-2048, q_min=2047, q_max=-2048,
                    i_sum=0, q_sum=0, i_sumsq=0, q_sumsq=0, clip_count=0,
                    i_or_mask=0, i_and_mask=0xfff, q_or_mask=0,
                    q_and_mask=0xfff, win_samples=0)

    def clip(x):
        return np.abs(x) >= 2047

    or_i = and_i = None
    for x in i_vals:
        u = int(x) & 0xfff
        or_i = u if or_i is None else or_i | u
        and_i = u if and_i is None else and_i & u
    or_q = and_q = None
    for x in q_vals:
        u = int(x) & 0xfff
        or_q = u if or_q is None else or_q | u
        and_q = u if and_q is None else and_q & u
    return dict(
        i_min=int(i_vals.min()), i_max=int(i_vals.max()),
        q_min=int(q_vals.min()), q_max=int(q_vals.max()),
        i_sum=int(i_vals.sum()), q_sum=int(q_vals.sum()),
        i_sumsq=int((i_vals**2).sum()), q_sumsq=int((q_vals**2).sum()),
        clip_count=int((clip(i_vals) | clip(q_vals)).sum()),
        i_or_mask=or_i, i_and_mask=and_i, q_or_mask=or_q, q_and_mask=and_q,
        win_samples=len(i_vals))


class TestIngestMonitorStats(unittest.TestCase):
    period = 1 / 61.44e6

    def run_stats(self, honor_valid, seed):
        rng = np.random.default_rng(seed)
        n = 1200
        i_vals = rng.integers(-2048, 2048, size=n)
        q_vals = rng.integers(-2048, 2048, size=n)
        # sprinkle extremes to exercise min/max/clip
        i_vals[10] = -2048
        q_vals[20] = 2047
        i_vals[30] = -2047
        valid = (rng.random(n) < 0.75).astype(int)
        valid[:3] = [1, 0, 0]
        dut = IngestMonitor()
        h = MonitorHarness(dut)

        async def bench(ctx):
            ctx.set(dut.honor_valid, honor_valid)
            ctx.set(dut.clear_on_snap, 1)
            await h.pulse(ctx, dut.clear)
            await h.feed(ctx, list(zip(i_vals, q_vals)), list(valid))
            await h.snap(ctx)
            sel = valid.astype(bool) if honor_valid else np.ones(n, bool)
            exp = expected_window(i_vals[sel], q_vals[sel])
            got = h.read_window(ctx)
            self.assertEqual(got, exp)
            self.assertEqual(ctx.get(dut.samples), int(sel.sum()))
            self.assertEqual(ctx.get(dut.valid_gap_cycles),
                             int((valid == 0).sum()))
            runs = int(np.sum((valid[1:] == 0) & (valid[:-1] == 1))
                       + (valid[0] == 0))
            self.assertEqual(ctx.get(dut.valid_gap_runs), runs)
            # second snap with nothing new: empty window
            await h.snap(ctx)
            self.assertEqual(h.read_window(ctx), expected_window([], []))
            # running counters are not affected by snap
            self.assertEqual(ctx.get(dut.samples), int(sel.sum()))
            await h.pulse(ctx, dut.clear)
            await ctx.tick('sampling')
            self.assertEqual(ctx.get(dut.samples), 0)
            self.assertEqual(ctx.get(dut.valid_gap_cycles), 0)
            self.assertEqual(ctx.get(dut.win_samples), 0)
            self.assertEqual(ctx.get(dut.i_min), 2047)

        run_sim(dut, [bench], {'sampling': self.period})

    def test_stats_honor_valid(self):
        self.run_stats(honor_valid=1, seed=1)

    def test_stats_every_cycle(self):
        self.run_stats(honor_valid=0, seed=2)

    def test_stuck_bits_and_clip(self):
        rng = np.random.default_rng(3)
        n = 300
        i_vals = rng.integers(-2048, 2048, size=n)
        q_vals = rng.integers(-2048, 2048, size=n)
        # I bit 3 stuck at 0, Q bit 7 stuck at 1
        i_vals = np.array([to_signed12(int(x) & ~0x008) for x in i_vals])
        q_vals = np.array([to_signed12(int(x) | 0x080) for x in q_vals])
        clip_vals = [2047, -2047, -2048, 2046, -2046, 0]
        i_vals[:6] = clip_vals
        i_vals[:6] = [to_signed12(int(x) & ~0x008) for x in i_vals[:6]]
        q_vals[6:12] = [to_signed12(int(x) | 0x080) for x in clip_vals]
        dut = IngestMonitor()
        h = MonitorHarness(dut)

        async def bench(ctx):
            ctx.set(dut.honor_valid, 1)
            await h.pulse(ctx, dut.clear)
            await h.feed(ctx, list(zip(i_vals, q_vals)), [1] * n)
            await h.snap(ctx)
            got = h.read_window(ctx)
            exp = expected_window(i_vals, q_vals)
            self.assertEqual(got, exp)
            self.assertEqual(got['i_or_mask'] & 0x008, 0)
            self.assertEqual(got['q_and_mask'] & 0x080, 0x080)
            self.assertNotEqual(got['i_or_mask'] | 0x008, got['i_or_mask'])

        run_sim(dut, [bench], {'sampling': self.period})

    def test_clip_count_exact(self):
        i_vals = [2047, -2047, -2048, 2046, -2046, 0, 5, 2047]
        q_vals = [0, 0, 0, 0, 0, -2048, 2047, 2047]
        # clipped (I or Q): rows 0, 1, 2, 5, 6, 7
        dut = IngestMonitor()
        h = MonitorHarness(dut)

        async def bench(ctx):
            ctx.set(dut.honor_valid, 1)
            await h.feed(ctx, list(zip(i_vals, q_vals)), [1] * len(i_vals))
            await h.snap(ctx)
            self.assertEqual(ctx.get(dut.clip_count), 6)

        run_sim(dut, [bench], {'sampling': self.period})

    def test_snap_windows(self):
        rng = np.random.default_rng(4)
        n = 400
        i_vals = rng.integers(-2048, 2048, size=n)
        q_vals = rng.integers(-2048, 2048, size=n)
        for clear_on_snap in (1, 0):
            dut = IngestMonitor()
            h = MonitorHarness(dut)

            async def bench(ctx, dut=dut, h=h, clear_on_snap=clear_on_snap):
                ctx.set(dut.honor_valid, 0)
                ctx.set(dut.clear_on_snap, clear_on_snap)
                await h.pulse(ctx, dut.clear)
                windows = []
                # every cycle is a sample (honor_valid 0); stats_enable
                # is aligned with the pipeline so exactly inputs 0..n-1
                # are counted (see MonitorHarness.feed)
                for k in range(n + 2):
                    ctx.set(dut.re_in, int(i_vals[k]) & 0xfff if k < n else 0)
                    ctx.set(dut.im_in, int(q_vals[k]) & 0xfff if k < n else 0)
                    ctx.set(dut.stats_enable, int(k >= 2))
                    ctx.set(dut.snap, int(k in (97, 250)))
                    await ctx.tick('sampling')
                    if k in (97, 250):
                        windows.append(h.read_window(ctx))
                ctx.set(dut.snap, 0)
                ctx.set(dut.stats_enable, 0)
                for _ in range(4):
                    await ctx.tick('sampling')
                await h.snap(ctx)
                windows.append(h.read_window(ctx))
                total = ctx.get(dut.samples)
                self.assertEqual(total, n)
                if clear_on_snap:
                    self.assertEqual(sum(w['win_samples'] for w in windows),
                                     n)
                    self.assertEqual(sum(w['i_sum'] for w in windows),
                                     int(i_vals.sum()))
                    self.assertEqual(sum(w['q_sumsq'] for w in windows),
                                     int((q_vals**2).sum()))
                    # windows are contiguous slices of the input
                    s0 = windows[0]['win_samples']
                    self.assertEqual(windows[0]['i_sum'],
                                     int(i_vals[:s0].sum()))
                    self.assertEqual(windows[0]['i_max'],
                                     int(i_vals[:s0].max()))
                else:
                    self.assertEqual(windows[-1], expected_window(
                        i_vals, q_vals))
                    self.assertLess(windows[0]['win_samples'],
                                    windows[1]['win_samples'])

            run_sim(dut, [bench], {'sampling': self.period})


class TestIngestMonitorPrbs(unittest.TestCase):
    period = 1 / 61.44e6

    def reference(self, mode, n):
        if mode == 0:
            return list(ad9361_bist_prbs(n, seed=0x5a5a))
        return list(pn9_pn11(n))

    def run_prbs(self, mode, iq, valid=None, honor_valid=1,
                 check_every=None):
        dut = IngestMonitor()
        result = {}

        async def bench(ctx):
            ctx.set(dut.prbs_mode, mode)
            ctx.set(dut.prbs_enable, 1)
            ctx.set(dut.honor_valid, honor_valid)
            for k, (i, q) in enumerate(iq):
                ctx.set(dut.re_in, i)
                ctx.set(dut.im_in, q)
                ctx.set(dut.valid_in, 1 if valid is None else valid[k])
                await ctx.tick('sampling')
                if check_every is not None:
                    check_every(ctx, dut, k)
            ctx.set(dut.valid_in, 0)
            for _ in range(4):
                await ctx.tick('sampling')
            for name in ['prbs_checked', 'prbs_errors', 'prbs_oos_events',
                         'prbs_in_sync']:
                result[name] = ctx.get(getattr(dut, name))

        run_sim(dut, [bench], {'sampling': self.period})
        return result

    def check_lock(self, mode):
        n = 600
        iq = self.reference(mode, n)
        r = self.run_prbs(mode, iq)
        self.assertEqual(r['prbs_in_sync'], 1)
        self.assertEqual(r['prbs_errors'], 0)
        self.assertEqual(r['prbs_oos_events'], 0)
        # lock needs 16 consecutive matches after the first prediction
        self.assertGreaterEqual(r['prbs_checked'], n - 20)
        self.assertLessEqual(r['prbs_checked'], n - 16)

    def test_bist_lock(self):
        self.check_lock(0)

    def test_pn9_pn11_lock(self):
        self.check_lock(1)

    def check_corruptions(self, mode):
        rng = np.random.default_rng(10 + mode)
        n = 1500
        iq = self.reference(mode, n)
        positions = sorted(rng.choice(np.arange(100, n - 50, 3), size=40,
                                      replace=False))
        # a burst of 5 consecutive corruptions (< 16: stays in sync)
        burst = list(range(1200, 1205))
        for k in list(positions) + burst:
            i, q = iq[k]
            bit = int(rng.integers(0, 24))
            if bit < 12:
                i ^= 1 << bit
            else:
                q ^= 1 << (bit - 12)
            iq[k] = (i, q)
        n_bad = len(set(positions) | set(burst))
        r = self.run_prbs(mode, iq)
        self.assertEqual(r['prbs_errors'], n_bad)
        self.assertEqual(r['prbs_oos_events'], 0)
        self.assertEqual(r['prbs_in_sync'], 1)

    def test_bist_corruptions(self):
        self.check_corruptions(0)

    def test_pn9_pn11_corruptions(self):
        self.check_corruptions(1)

    def check_drop(self, mode):
        n = 800
        iq = self.reference(mode, n + 1)
        del iq[300]
        sync_trace = []

        def check(ctx, dut, k):
            sync_trace.append(ctx.get(dut.prbs_in_sync))

        r = self.run_prbs(mode, iq, check_every=check)
        self.assertEqual(r['prbs_oos_events'], 1)
        self.assertEqual(r['prbs_errors'], 16)
        self.assertEqual(r['prbs_in_sync'], 1)
        # sync lost after the drop and regained within ~40 samples
        lost = [k for k in range(1, n) if sync_trace[k - 1]
                and not sync_trace[k]]
        self.assertEqual(len(lost), 1)
        self.assertTrue(300 < lost[0] < 330)
        self.assertTrue(all(sync_trace[lost[0] + 40:]))

    def test_bist_drop(self):
        self.check_drop(0)

    def test_pn9_pn11_drop(self):
        self.check_drop(1)

    def test_valid_gaps(self):
        rng = np.random.default_rng(20)
        for mode in (0, 1):
            ref = self.reference(mode, 500)
            # expand with random invalid cycles carrying garbage
            iq, valid = [], []
            for k, s in enumerate(ref):
                # garbage before at least every 8th sample, so the stream
                # seen with honor_valid = 0 never has 16 clean samples
                if k % 8 == 0:
                    iq.append((int(rng.integers(1, 4096)),
                               int(rng.integers(1, 4096))))
                    valid.append(0)
                while rng.random() < 0.4:
                    iq.append((int(rng.integers(0, 4096)),
                               int(rng.integers(0, 4096))))
                    valid.append(0)
                iq.append(s)
                valid.append(1)
            r = self.run_prbs(mode, iq, valid, honor_valid=1)
            self.assertEqual(r['prbs_in_sync'], 1)
            self.assertEqual(r['prbs_errors'], 0)
            # same stream with honor_valid = 0 never locks
            r = self.run_prbs(mode, iq, valid, honor_valid=0)
            self.assertEqual(r['prbs_in_sync'], 0)
            self.assertEqual(r['prbs_checked'], 0)

    def test_zero_and_constant_input_never_lock(self):
        for mode in (0, 1):
            for value in (0, 0xfff, 0x555):
                r = self.run_prbs(mode, [(value, value)] * 100)
                self.assertEqual(r['prbs_in_sync'], 0, (mode, value))

    def test_disable_drops_sync(self):
        dut = IngestMonitor()
        iq = list(ad9361_bist_prbs(100, seed=3))

        async def bench(ctx):
            ctx.set(dut.prbs_enable, 1)
            ctx.set(dut.valid_in, 1)
            for i, q in iq:
                ctx.set(dut.re_in, i)
                ctx.set(dut.im_in, q)
                await ctx.tick('sampling')
            await ctx.tick('sampling')
            self.assertEqual(ctx.get(dut.prbs_in_sync), 1)
            ctx.set(dut.prbs_enable, 0)
            await ctx.tick('sampling')
            self.assertEqual(ctx.get(dut.prbs_in_sync), 0)
            checked = ctx.get(dut.prbs_checked)
            ctx.set(dut.clear, 1)
            await ctx.tick('sampling')
            ctx.set(dut.clear, 0)
            self.assertGreater(checked, 0)
            self.assertEqual(ctx.get(dut.prbs_checked), 0)

        run_sim(dut, [bench], {'sampling': self.period})


# ---------------------------------------------------------------------------
# IngestCDC
# ---------------------------------------------------------------------------

class TestIngestCDC(unittest.TestCase):
    def run_cdc(self, samples, valid, honor_valid, i_period, o_period,
                drain_cycles):
        dut = IngestCDC('sampling', 'sync', 12, sim=True)
        received = []
        status = {}
        done = [False]

        async def writer(ctx):
            ctx.set(dut.honor_valid, 1)
            for _ in range(4):
                await ctx.tick('sampling')
            ctx.set(dut.honor_valid, honor_valid)
            for (i, q), v in zip(samples, valid):
                ctx.set(dut.re_in, i)
                ctx.set(dut.im_in, q)
                ctx.set(dut.valid_in, int(v))
                await ctx.tick('sampling')
            ctx.set(dut.valid_in, 0)
            ctx.set(dut.honor_valid, 1)
            done[0] = True
            await ctx.tick('sampling')
            status['wrerr'] = ctx.get(dut.wrerr_count)
            status['full'] = ctx.get(dut.full_cycles)

        async def reader(ctx):
            idle = 0
            while not done[0] or idle < drain_cycles:
                await ctx.tick('sync')
                if ctx.get(dut.strobe_out):
                    received.append((ctx.get(dut.re_out),
                                     ctx.get(dut.im_out)))
                    idle = 0
                else:
                    idle += 1

        run_sim(dut, [writer, reader],
                {'sampling': i_period, 'sync': o_period})
        return received, status

    def test_no_loss_faster_output(self):
        rng = np.random.default_rng(30)
        n = 2000
        samples = [(int(a), int(b)) for a, b in
                   rng.integers(0, 4096, size=(n, 2))]
        valid = list((rng.random(n) < 0.7).astype(int))
        received, status = self.run_cdc(samples, valid, 1, 20e-9, 16e-9, 20)
        expected = [s for s, v in zip(samples, valid) if v]
        self.assertEqual(received, expected)
        self.assertEqual(status['wrerr'], 0)
        self.assertEqual(status['full'], 0)

    def test_every_cycle_when_not_honoring_valid(self):
        rng = np.random.default_rng(31)
        n = 1000
        samples = [(int(a), int(b)) for a, b in
                   rng.integers(0, 4096, size=(n, 2))]
        valid = list((rng.random(n) < 0.5).astype(int))
        received, status = self.run_cdc(samples, valid, 0, 20e-9, 16e-9, 20)
        self.assertEqual(received, samples)
        self.assertEqual(status['wrerr'], 0)

    def test_overflow_counted(self):
        n = 3000
        samples = [(k & 0xfff, (k >> 12) & 0xfff) for k in range(n)]
        valid = [1] * n
        # output 20x slower than input: the 512-deep FIFO overflows
        received, status = self.run_cdc(samples, valid, 1, 10e-9, 200e-9,
                                        1000)
        self.assertGreater(status['wrerr'], 0)
        self.assertGreater(status['full'], 0)
        # conservation: every sample was either delivered or counted
        self.assertEqual(len(received) + status['wrerr'], n)
        # delivered samples are in order, and the first 512 are intact
        idx = [s[0] | (s[1] << 12) for s in received]
        self.assertEqual(idx, sorted(idx))
        self.assertEqual(idx[:512], list(range(512)))


if __name__ == '__main__':
    unittest.main()

#
# Fishball hwval - AxiMemTester tests
#
# Simulates hwval_hdl.axi_memtest.AxiMemTester against the randomized AXI3
# read/write subordinate model in hwval_axi_rwmodel.py. Every simulation
# checks the AXI rules in the model (VALID/payload held until handshake,
# WLAST placement, 4 KiB crossings, window, outstanding limit, WVALID
# never before AWVALID).
#
# SPDX-License-Identifier: MIT
#

import random
import unittest

from amaranth import Module, Signal
from amaranth.sim import Simulator

from maia_hdl import axi
from hwval_hdl.axi_memtest import (
    AxiMemTester, expected_beat, prbs_word, byte_lane_expected,
    byte_lane_strobe, pattern_pass_index, lat_bin, LAT_BINS,
    MODE_WRITE, MODE_READ_VERIFY, MODE_WRITE_VERIFY, MODE_READ,
    MODE_BYTE_LANE,
    PAT_ADDRESS, PAT_WALK1, PAT_WALK0, PAT_CHECKER, PAT_PRBS, PAT_ZERO,
    PAT_ONES, PAT_TOGGLE, NUM_PATTERNS, VALID_BURST_LENS)

from .hwval_axi_rwmodel import AxiRWModel, AxiProtocolError, SLVERR


GUARD_LO = 0x2400_0000
GUARD_HI = 0x2800_0000
# Straddles a 4 KiB boundary (0x2400_1000) with 128-byte aligned bursts
BASE = 0x2400_0F00
SEED = 0x1234_5678

OUTPUTS = [
    'busy', 'done', 'error', 'pass_count', 'bytes_wr', 'bytes_rd', 'cycles',
    'err_count', 'first_err_addr', 'first_err_exp', 'first_err_act',
    'err_lanes', 'bresp_err', 'rresp_err', 'wlat_max', 'rlat_max',
    'guard_blocked',
]


def cfg_of(**kw):
    cfg = dict(mode=MODE_WRITE, pattern=PAT_ADDRESS, burst_len=16, mo=4,
               stop=0, base=BASE, size=1024, passes=1, idle=0, seed=SEED,
               guard_lo=GUARD_LO, guard_hi=GUARD_HI)
    cfg.update(kw)
    return cfg


class TestExpectedBeat(unittest.TestCase):
    """Pure-Python pattern reference."""

    def test_address(self):
        a = 0x2400_0008
        self.assertEqual(expected_beat(PAT_ADDRESS, a, 5, 99),
                         (0x2400_0008 << 32) | 0xDBFF_FFF7)

    def test_walking(self):
        for p in range(3):
            for a in range(0, 8 * 130, 8):
                w1 = expected_beat(PAT_WALK1, a, p, 0)
                self.assertEqual(w1, 1 << ((a // 8 + p) % 64))
                self.assertEqual(expected_beat(PAT_WALK0, a, p, 0),
                                 w1 ^ (2**64 - 1))

    def test_checker_toggle_const(self):
        self.assertEqual(expected_beat(PAT_CHECKER, 0, 0, 0),
                         0xAAAA_AAAA_AAAA_AAAA)
        self.assertEqual(expected_beat(PAT_CHECKER, 8, 0, 0),
                         0x5555_5555_5555_5555)
        self.assertEqual(expected_beat(PAT_CHECKER, 0, 1, 0),
                         0x5555_5555_5555_5555)
        self.assertEqual(expected_beat(PAT_TOGGLE, 0, 0, 0), 0)
        self.assertEqual(expected_beat(PAT_TOGGLE, 8, 0, 0), 2**64 - 1)
        self.assertEqual(expected_beat(PAT_ZERO, 8, 3, 1), 0)
        self.assertEqual(expected_beat(PAT_ONES, 8, 3, 1), 2**64 - 1)
        with self.assertRaises(ValueError):
            expected_beat(NUM_PATTERNS, 0, 0, 0)

    def test_prbs(self):
        a = 0x2400_0100
        x = prbs_word(a, 0, SEED)
        d = expected_beat(PAT_PRBS, a, 0, SEED)
        self.assertEqual(d >> 32, x)
        rot = ((x << 16) | (x >> 16)) & 0xFFFF_FFFF
        self.assertEqual(d & 0xFFFF_FFFF, rot ^ 0xA5A5_A5A5)
        self.assertNotEqual(d, expected_beat(PAT_PRBS, a, 1, SEED))
        self.assertNotEqual(d, expected_beat(PAT_PRBS, a, 0, SEED + 1))
        # Consecutive beats toggle about half of the 64 lines
        hd = [bin(expected_beat(PAT_PRBS, a, 0, SEED)
                  ^ expected_beat(PAT_PRBS, a + 8, 0, SEED)).count('1')
              for a in range(GUARD_LO, GUARD_LO + 8 * 2048, 8)]
        self.assertGreater(sum(hd) / len(hd), 28)

    def test_byte_lane_and_pass_index(self):
        for lane in range(8):
            a = 0x2400_0000 + 8 * lane
            self.assertEqual(byte_lane_strobe(a), 1 << lane)
            self.assertEqual(byte_lane_expected(a), 0xFF << (8 * lane))
        self.assertEqual(pattern_pass_index(MODE_READ_VERIFY, 7), 0)
        self.assertEqual(pattern_pass_index(MODE_WRITE_VERIFY, 7), 7)

    def test_lat_bin(self):
        self.assertEqual([lat_bin(x) for x in (0, 1, 2, 3, 4, 7, 8)],
                         [0, 0, 1, 1, 2, 2, 3])
        self.assertEqual(lat_bin(2**15 - 1), 14)
        self.assertEqual(lat_bin(2**15), 15)
        self.assertEqual(lat_bin(2**31), 15)


class TestAxiMemTester(unittest.TestCase):

    # -- harness -------------------------------------------------------------

    def setup_dut(self, *, dut_max_outstanding=8, mo=None, **model_kw):
        self.dut = AxiMemTester(max_outstanding=dut_max_outstanding)
        model_kw.setdefault('window', (GUARD_LO, GUARD_HI))
        model_kw.setdefault('forbid_w_before_aw', True)
        if mo is not None:
            model_kw.setdefault('max_outstanding', mo)
        self.model = AxiRWModel(self.dut.axi, **model_kw)
        return self.dut, self.model

    def simulate(self, bench):
        sim = Simulator(self.dut)
        sim.add_clock(8e-9)
        sim.add_testbench(self.model.process, background=True)
        sim.add_testbench(bench)
        sim.run()

    def set_cfg(self, ctx, cfg):
        dut = self.dut
        ctx.set(dut.mode, cfg['mode'])
        ctx.set(dut.pattern, cfg['pattern'])
        ctx.set(dut.burst_len, cfg['burst_len'])
        ctx.set(dut.max_outstanding_cfg, cfg['mo'])
        ctx.set(dut.stop_on_error, cfg['stop'])
        ctx.set(dut.base, cfg['base'])
        ctx.set(dut.size, cfg['size'])
        ctx.set(dut.passes, cfg['passes'])
        ctx.set(dut.idle_cycles, cfg['idle'])
        ctx.set(dut.seed, cfg['seed'])
        ctx.set(dut.guard_lo, cfg['guard_lo'])
        ctx.set(dut.guard_hi, cfg['guard_hi'])

    async def go(self, ctx, cfg, *, clear=True, abort_after=None,
                 timeout=60000):
        """Start a run and wait for done; returns the status outputs."""
        dut = self.dut
        self.set_cfg(ctx, cfg)
        ctx.set(dut.start, 1)
        ctx.set(dut.clear, int(clear))
        await ctx.tick()
        ctx.set(dut.start, 0)
        ctx.set(dut.clear, 0)
        self.assertEqual(ctx.get(dut.done), 0)
        irqs = 0
        n = 0
        while True:
            if ctx.get(dut.irq):
                irqs += 1
                self.assertEqual(ctx.get(dut.done), 1)
            if ctx.get(dut.done):
                break
            ctx.set(dut.abort, int(abort_after is not None
                                   and n == abort_after))
            await ctx.tick()
            n += 1
            if n > timeout:
                self.fail(f'run did not finish in {timeout} cycles')
        ctx.set(dut.abort, 0)
        self.assertEqual(ctx.get(dut.busy), 0)
        for _ in range(3):
            await ctx.tick()
            irqs += ctx.get(dut.irq)
        res = {k: ctx.get(getattr(dut, k)) for k in OUTPUTS}
        res['irqs'] = irqs
        res['run_cycles'] = n
        return res

    async def read_hist(self, ctx, read):
        vals = []
        for b in range(LAT_BINS):
            ctx.set(self.dut.hist_sel, (int(read) << 4) | b)
            for _ in range(3):
                await ctx.tick()
            vals.append(ctx.get(self.dut.hist_val))
        return vals

    def run_one(self, cfg, *, model_kw=None, preload=None, hist=False,
                dut_max_outstanding=8, abort_after=None, check_mo=True):
        model_kw = dict(model_kw or {})
        self.setup_dut(dut_max_outstanding=dut_max_outstanding,
                       mo=cfg['mo'] if check_mo else None, **model_kw)
        if preload is not None:
            preload(self.model)
        out = {}

        async def bench(ctx):
            out.update(await self.go(ctx, cfg, abort_after=abort_after))
            if hist:
                out['whist'] = await self.read_hist(ctx, False)
                out['rhist'] = await self.read_hist(ctx, True)

        self.simulate(bench)
        return out, self.model

    def check_clean(self, res, model):
        self.assertEqual(res['done'], 1)
        self.assertEqual(res['busy'], 0)
        self.assertEqual(res['error'], 0)
        self.assertEqual(res['err_count'], 0)
        self.assertEqual(res['err_lanes'], 0)
        self.assertEqual(res['bresp_err'], 0)
        self.assertEqual(res['rresp_err'], 0)
        self.assertEqual(res['guard_blocked'], 0)
        self.assertEqual(res['irqs'], 1)
        self.assertTrue(model.idle())

    def check_memory(self, model, cfg, fn):
        base, size = cfg['base'], cfg['size']
        for a in range(base, base + size, 8):
            got = model.read_beat(a)
            exp = fn(a)
            if got != exp:
                self.fail(f'beat {a:#x}: memory {got:#018x}, '
                          f'expected {exp:#018x}')
        self.assertTrue(all(base <= k < base + size for k in model.mem))

    # -- modes ----------------------------------------------------------------

    def test_write_only_matches_reference(self):
        """Mode 0: memory holds exactly expected_beat() for every pattern."""
        blens = [16, 8, 4, 2, 1, 16, 4, 8]
        mos = [4, 1, 8, 2, 3, 8, 2, 5]
        for pat in range(NUM_PATTERNS):
            with self.subTest(pattern=pat):
                cfg = cfg_of(mode=MODE_WRITE, pattern=pat,
                             burst_len=blens[pat], mo=mos[pat], size=512)
                res, model = self.run_one(cfg, model_kw=dict(seed=pat))
                self.check_clean(res, model)
                self.check_memory(
                    model, cfg, lambda a: expected_beat(pat, a, 0, SEED))
                self.assertEqual(res['bytes_wr'], 512)
                self.assertEqual(res['bytes_rd'], 0)
                self.assertEqual(res['pass_count'], 1)
                self.assertEqual(len(model.aw_log), 512 // (8 * blens[pat]))
                self.assertEqual(len(model.ar_log), 0)
                self.assertEqual(
                    [x[1] for x in model.aw_log],
                    list(range(BASE, BASE + 512, 8 * blens[pat])))
                self.assertTrue(all(x[2] == blens[pat] - 1
                                    for x in model.aw_log))

    def test_write_multi_pass(self):
        """Mode 0, 3 passes: memory holds pass index 2 data."""
        for pat in (PAT_WALK1, PAT_CHECKER, PAT_PRBS):
            with self.subTest(pattern=pat):
                cfg = cfg_of(mode=MODE_WRITE, pattern=pat, passes=3,
                             size=256, mo=3)
                res, model = self.run_one(cfg, model_kw=dict(seed=10 + pat))
                self.check_clean(res, model)
                self.assertEqual(res['pass_count'], 3)
                self.assertEqual(res['bytes_wr'], 3 * 256)
                self.check_memory(
                    model, cfg, lambda a: expected_beat(pat, a, 2, SEED))

    def test_write_verify_clean(self):
        """Mode 2 over patterns, burst lengths and outstanding limits."""
        combos = [
            # pattern, burst_len, mo, passes
            (PAT_ADDRESS, 16, 4, 2),
            (PAT_WALK1, 8, 2, 2),
            (PAT_WALK0, 4, 8, 1),
            (PAT_CHECKER, 2, 3, 1),
            (PAT_PRBS, 16, 8, 3),
            (PAT_ZERO, 1, 1, 1),
            (PAT_ONES, 16, 1, 1),
            (PAT_TOGGLE, 4, 4, 2),
        ]
        early = 0
        for i, (pat, blen, mo, passes) in enumerate(combos):
            with self.subTest(pattern=pat, burst_len=blen, mo=mo):
                size = 512
                cfg = cfg_of(mode=MODE_WRITE_VERIFY, pattern=pat,
                             burst_len=blen, mo=mo, passes=passes, size=size)
                res, model = self.run_one(cfg, model_kw=dict(seed=100 + i))
                self.check_clean(res, model)
                self.assertEqual(res['pass_count'], passes)
                self.assertEqual(res['bytes_wr'], passes * size)
                self.assertEqual(res['bytes_rd'], passes * size)
                self.assertEqual(model.w_beats * 8, passes * size)
                self.assertEqual(model.r_beats * 8, passes * size)
                self.check_memory(
                    model, cfg,
                    lambda a: expected_beat(pat, a, passes - 1, SEED))
                # Writes and reads never overlap: each pass writes the
                # window, then reads it
                nb = size // (8 * blen)
                for p in range(passes):
                    last_b = model.b_log[(p + 1) * nb - 1]
                    first_ar = model.ar_log[p * nb][0]
                    self.assertLess(last_b, first_ar)
                early += model.w_early_beats
        # W does not wait for AWREADY (AXI write dependency rule)
        self.assertGreater(early, 0)

    def test_read_verify_preloaded(self):
        """Mode 1 verifies pre-existing contents (pass index 0)."""
        for pat in (PAT_ADDRESS, PAT_PRBS, PAT_WALK1, PAT_TOGGLE):
            with self.subTest(pattern=pat):
                cfg = cfg_of(mode=MODE_READ_VERIFY, pattern=pat, passes=2,
                             size=512, mo=6, burst_len=8)

                def preload(model, pat=pat):
                    model.fill_beats(
                        BASE, 512, lambda a: expected_beat(pat, a, 0, SEED))

                res, model = self.run_one(cfg, model_kw=dict(seed=pat),
                                          preload=preload)
                self.check_clean(res, model)
                self.assertEqual(res['pass_count'], 2)
                self.assertEqual(res['bytes_rd'], 2 * 512)
                self.assertEqual(res['bytes_wr'], 0)
                self.assertEqual(len(model.aw_log), 0)

    def test_read_verify_wrong_pass_detected(self):
        """Memory holding pass-1 walking-1 data fails mode 1 on every beat."""
        cfg = cfg_of(mode=MODE_READ_VERIFY, pattern=PAT_WALK1, size=256)

        def preload(model):
            model.fill_beats(
                BASE, 256, lambda a: expected_beat(PAT_WALK1, a, 1, SEED))

        res, model = self.run_one(cfg, model_kw=dict(seed=3),
                                  preload=preload)
        self.assertEqual(res['err_count'], 256 // 8)
        self.assertEqual(res['error'], 1)
        self.assertEqual(res['first_err_addr'], BASE)
        self.assertEqual(res['first_err_exp'],
                         expected_beat(PAT_WALK1, BASE, 0, SEED))
        self.assertEqual(res['first_err_act'],
                         expected_beat(PAT_WALK1, BASE, 1, SEED))
        lanes = 0
        for a in range(BASE, BASE + 256, 8):
            lanes |= (expected_beat(PAT_WALK1, a, 0, SEED)
                      ^ expected_beat(PAT_WALK1, a, 1, SEED))
        self.assertEqual(res['err_lanes'], lanes)

    def test_read_only(self):
        """Mode 3: reads only, no compare (an invalid pattern is ignored)."""
        rnd = random.Random(5)
        garbage = {}

        def fill(addr):
            return garbage.setdefault(addr, rnd.randrange(256))

        cfg = cfg_of(mode=MODE_READ, pattern=9, passes=3, size=512, mo=8)
        res, model = self.run_one(cfg, model_kw=dict(seed=7, fill=fill))
        self.check_clean(res, model)
        self.assertEqual(res['pass_count'], 3)
        self.assertEqual(res['bytes_rd'], 3 * 512)
        self.assertEqual(res['bytes_wr'], 0)
        self.assertEqual(len(model.aw_log), 0)
        self.assertEqual(len(model.ar_log), 3 * 512 // 128)

    def test_byte_lane(self):
        """Mode 4: zeros, one-hot WSTRB ones, verify one 0xFF byte/beat."""
        for blen in (16, 1):
            with self.subTest(burst_len=blen):
                cfg = cfg_of(mode=MODE_BYTE_LANE, burst_len=blen, size=512,
                             mo=4, pattern=15)
                res, model = self.run_one(cfg, model_kw=dict(seed=blen))
                self.check_clean(res, model)
                self.assertEqual(res['bytes_wr'], 2 * 512)
                self.assertEqual(res['bytes_rd'], 512)
                self.assertEqual(res['pass_count'], 1)
                self.check_memory(model, cfg, byte_lane_expected)

    def test_byte_lane_stuck_dm(self):
        """Mode 4 catches a DM line stuck enabled on byte lane 2."""
        cfg = cfg_of(mode=MODE_BYTE_LANE, size=512, base=GUARD_LO + 0x400)
        res, model = self.run_one(
            cfg, model_kw=dict(seed=4, wstrb_stuck_on=1 << 2))
        self.assertEqual(res['err_count'], (512 // 8) * 7 // 8)
        self.assertEqual(res['err_lanes'], 0xFF << 16)
        self.assertEqual(res['first_err_addr'], cfg['base'])
        self.assertEqual(res['first_err_exp'], 0xFF)
        self.assertEqual(res['first_err_act'], 0xFF | (0xFF << 16))
        self.assertEqual(res['error'], 1)

    # -- errors ---------------------------------------------------------------

    def test_single_bit_error(self):
        """An injected read bit flip is reported exactly."""
        addr = BASE + 0x1A8
        mask = 1 << 37
        cfg = cfg_of(mode=MODE_WRITE_VERIFY, pattern=PAT_PRBS, size=1024)
        res, model = self.run_one(
            cfg, preload=lambda m: m.read_flips.update({addr: mask}),
            model_kw=dict(seed=11))
        exp = expected_beat(PAT_PRBS, addr, 0, SEED)
        self.assertEqual(res['err_count'], 1)
        self.assertEqual(res['error'], 1)
        self.assertEqual(res['done'], 1)
        self.assertEqual(res['first_err_addr'], addr)
        self.assertEqual(res['first_err_exp'], exp)
        self.assertEqual(res['first_err_act'], exp ^ mask)
        self.assertEqual(res['err_lanes'], mask)
        self.assertEqual(res['pass_count'], 1)
        self.assertEqual(res['bytes_rd'], 1024)
        self.assertEqual(res['bresp_err'], 0)
        self.assertEqual(res['rresp_err'], 0)

    def test_two_errors_lanes_or(self):
        """err_lanes ORs all errors; first_err_* keeps the first one."""
        a1, m1 = BASE + 0x40, 1 << 3
        a2, m2 = BASE + 0x2F8, (1 << 60) | (1 << 17)
        cfg = cfg_of(mode=MODE_READ_VERIFY, pattern=PAT_ADDRESS, size=1024,
                     burst_len=4)

        def preload(model):
            model.fill_beats(
                BASE, 1024, lambda a: expected_beat(PAT_ADDRESS, a, 0, SEED))
            model.read_flips.update({a1: m1, a2: m2})

        res, model = self.run_one(cfg, preload=preload,
                                  model_kw=dict(seed=12))
        self.assertEqual(res['err_count'], 2)
        self.assertEqual(res['first_err_addr'], a1)
        exp = expected_beat(PAT_ADDRESS, a1, 0, SEED)
        self.assertEqual(res['first_err_exp'], exp)
        self.assertEqual(res['first_err_act'], exp ^ m1)
        self.assertEqual(res['err_lanes'], m1 | m2)

    def test_slverr_counted(self):
        """BRESP and RRESP SLVERR are counted and flag the run."""
        cfg = cfg_of(mode=MODE_WRITE_VERIFY, pattern=PAT_CHECKER, size=1024)

        def preload(model):
            model.bresp_err_addrs.add(BASE + 128)
            model.rresp_err_addrs.update({BASE + 256 + 8, BASE + 512})

        res, model = self.run_one(cfg, preload=preload,
                                  model_kw=dict(seed=13))
        self.assertEqual(res['bresp_err'], 1)
        self.assertEqual(res['rresp_err'], 2)
        self.assertEqual(res['err_count'], 0)
        self.assertEqual(res['error'], 1)
        self.assertEqual(res['done'], 1)
        self.assertEqual(res['bytes_rd'], 1024)
        self.assertTrue(model.idle())

    def test_stop_on_error(self):
        """stop_on_error ends the run early after draining in-flight bursts."""
        size = 8192
        addr = BASE + 64
        cfg = cfg_of(mode=MODE_WRITE_VERIFY, pattern=PAT_WALK0, size=size,
                     stop=1, passes=2)
        res, model = self.run_one(
            cfg, preload=lambda m: m.read_flips.update({addr: 1 << 5}),
            model_kw=dict(seed=14))
        self.assertEqual(res['done'], 1)
        self.assertEqual(res['error'], 1)
        self.assertEqual(res['err_count'], 1)
        self.assertEqual(res['first_err_addr'], addr)
        self.assertEqual(res['bytes_wr'], size)
        self.assertLess(res['bytes_rd'], size // 4)
        self.assertEqual(res['bytes_rd'], 8 * model.r_beats)
        self.assertEqual(res['pass_count'], 0)
        self.assertTrue(model.idle())

        # Without stop_on_error the pass completes
        cfg['stop'] = 0
        cfg['passes'] = 1
        res, model = self.run_one(
            cfg, preload=lambda m: m.read_flips.update({addr: 1 << 5}),
            model_kw=dict(seed=14))
        self.assertEqual(res['bytes_rd'], size)
        self.assertEqual(res['pass_count'], 1)
        self.assertEqual(res['err_count'], 1)

        # An error on the last beat halts after a complete pass, which
        # still counts
        last = BASE + 1024 - 8
        cfg = cfg_of(mode=MODE_READ_VERIFY, pattern=PAT_ZERO, size=1024,
                     stop=1, passes=3)
        res, model = self.run_one(
            cfg, preload=lambda m: m.read_flips.update({last: 1}),
            model_kw=dict(seed=19))
        self.assertEqual(res['pass_count'], 1)
        self.assertEqual(res['bytes_rd'], 1024)
        self.assertEqual(res['first_err_addr'], last)
        self.assertEqual(res['error'], 1)

    def test_stop_on_bresp_error(self):
        """A BRESP error with stop_on_error stops before the read phase."""
        size = 8192
        cfg = cfg_of(mode=MODE_WRITE_VERIFY, pattern=PAT_ONES, size=size,
                     stop=1)
        res, model = self.run_one(
            cfg, preload=lambda m: m.bresp_err_addrs.add(BASE),
            model_kw=dict(seed=15))
        self.assertEqual(res['error'], 1)
        self.assertEqual(res['bresp_err'], 1)
        self.assertEqual(res['bytes_rd'], 0)
        self.assertLess(res['bytes_wr'], size // 2)
        self.assertEqual(res['bytes_wr'] % 128, 0)
        self.assertTrue(model.idle())

    # -- control --------------------------------------------------------------

    def test_abort_mid_run(self):
        """abort drains in-flight bursts; the tester can run again after."""
        self.setup_dut(mo=4, seed=16)
        dut, model = self.dut, self.model
        out = {}

        async def bench(ctx):
            cfg = cfg_of(mode=MODE_WRITE_VERIFY, pattern=PAT_PRBS,
                         size=65536, passes=0)
            out['abort'] = await self.go(ctx, cfg, abort_after=700)
            out['model_idle'] = model.idle()
            out['w_beats'] = model.w_beats
            out['r_beats'] = model.r_beats
            # Infinite read-only run aborted after several passes
            cfg = cfg_of(mode=MODE_READ, size=256, passes=0)
            out['abort_rd'] = await self.go(ctx, cfg, abort_after=1500)
            out['model_idle2'] = model.idle()
            # A normal run afterwards
            cfg = cfg_of(mode=MODE_WRITE_VERIFY, pattern=PAT_ADDRESS,
                         size=512)
            out['after'] = await self.go(ctx, cfg)

        self.simulate(bench)
        res = out['abort']
        self.assertTrue(out['model_idle'])
        self.assertEqual(res['done'], 1)
        self.assertEqual(res['error'], 0)
        self.assertEqual(res['irqs'], 1)
        self.assertEqual(res['pass_count'], 0)
        self.assertGreater(res['bytes_wr'], 0)
        self.assertLess(res['bytes_wr'], 65536)
        self.assertEqual(res['bytes_wr'] % 128, 0)
        self.assertEqual(res['bytes_wr'], 8 * out['w_beats'])
        self.assertEqual(res['bytes_rd'], 0)
        self.assertLess(res['run_cycles'], 700 + 400)

        res = out['abort_rd']
        self.assertTrue(out['model_idle2'])
        self.assertGreaterEqual(res['pass_count'], 2)
        self.assertEqual(res['bytes_rd'] % 128, 0)
        self.assertEqual(res['error'], 0)

        res = out['after']
        self.check_clean(res, model)
        self.assertEqual(res['bytes_wr'], 512)
        self.assertEqual(res['bytes_rd'], 512)
        self.assertEqual(res['pass_count'], 1)

    def test_guard_refusal(self):
        """Out-of-guard, zero, misaligned or invalid configs are refused."""
        self.setup_dut()
        model = self.model
        bad = [
            cfg_of(base=GUARD_LO - 128),
            cfg_of(base=GUARD_HI - 1024, size=2048),
            cfg_of(size=0),
            cfg_of(base=BASE + 64),
            cfg_of(size=1024 + 64),
            cfg_of(burst_len=3),
            cfg_of(burst_len=0),
            cfg_of(mode=5),
            cfg_of(mode=MODE_WRITE, pattern=8),
            cfg_of(mode=MODE_WRITE_VERIFY, pattern=15),
            cfg_of(base=0xFFFF_FF00, size=512, guard_hi=0xFFFF_FFFF),
        ]
        out = []

        async def bench(ctx):
            for i, cfg in enumerate(bad):
                out.append(await self.go(ctx, cfg, clear=False))
            # Valid edge cases: window ending exactly at guard_hi and a
            # 64-byte aligned base with 8-beat bursts
            out.append(await self.go(
                ctx, cfg_of(base=GUARD_HI - 1024, size=1024), clear=False))
            out.append(await self.go(
                ctx, cfg_of(base=BASE + 64, burst_len=8, size=256),
                clear=False))
            ctx.set(self.dut.clear, 1)
            await ctx.tick()
            ctx.set(self.dut.clear, 0)
            await ctx.tick()
            out.append({k: ctx.get(getattr(self.dut, k))
                        for k in ('guard_blocked', 'done', 'error')})

        self.simulate(bench)
        for i, res in enumerate(out[:len(bad)]):
            with self.subTest(case=i):
                self.assertEqual(res['done'], 1)
                self.assertEqual(res['error'], 1)
                self.assertEqual(res['guard_blocked'], i + 1)
                self.assertEqual(res['irqs'], 1)
                self.assertLess(res['run_cycles'], 8)
        # Only the two valid runs issued bursts
        self.assertEqual(len(model.aw_log), 1024 // 128 + 256 // 64)
        self.assertEqual(len(model.ar_log), 0)
        for res in out[len(bad):len(bad) + 2]:
            self.assertEqual(res['error'], 0)
            self.assertEqual(res['guard_blocked'], len(bad))
        self.assertEqual(out[-1], dict(guard_blocked=0, done=0, error=0))

    def test_counters_accumulate_until_clear(self):
        """start does not clear counters; clear does (and done/error)."""
        addr = BASE + 8
        self.setup_dut(seed=17)
        self.model.read_flips[addr] = 1 << 63
        out = []

        async def bench(ctx):
            cfg = cfg_of(mode=MODE_WRITE_VERIFY, pattern=PAT_ZERO, size=256)
            out.append(await self.go(ctx, cfg, clear=True))
            out.append(await self.go(ctx, cfg, clear=False))
            ctx.set(self.dut.clear, 1)
            await ctx.tick()
            ctx.set(self.dut.clear, 0)
            await ctx.tick()
            res = {k: ctx.get(getattr(self.dut, k)) for k in OUTPUTS}
            res['whist'] = await self.read_hist(ctx, False)
            res['rhist'] = await self.read_hist(ctx, True)
            out.append(res)

        self.simulate(bench)
        r1, r2, r3 = out
        self.assertEqual(r1['err_count'], 1)
        self.assertEqual(r2['err_count'], 2)
        self.assertEqual(r2['pass_count'], 2)
        self.assertEqual(r2['bytes_wr'], 512)
        self.assertEqual(r2['bytes_rd'], 512)
        self.assertGreater(r2['cycles'], r1['cycles'])
        self.assertEqual(r2['first_err_addr'], addr)
        self.assertEqual(r2['error'], 1)
        for k in OUTPUTS:
            self.assertEqual(r3[k], 0, k)
        self.assertEqual(r3['whist'], [0] * LAT_BINS)
        self.assertEqual(r3['rhist'], [0] * LAT_BINS)

    def test_outstanding_limit(self):
        """The tester keeps max_outstanding_cfg bursts in flight."""
        slow = dict(b_delay=(30, 60), r_delay=(30, 60), p_awready=0.9,
                    p_wready=0.95, p_arready=0.9)
        cases = [
            # mo_cfg, effective limit
            (8, 8),
            (3, 3),
            (0, 1),
            (15, 8),
        ]
        for mo_cfg, eff in cases:
            with self.subTest(mo=mo_cfg):
                cfg = cfg_of(mode=MODE_WRITE_VERIFY, pattern=PAT_PRBS,
                             size=2048, mo=mo_cfg, burst_len=4)
                res, model = self.run_one(
                    cfg, model_kw=dict(slow, seed=mo_cfg,
                                       max_outstanding=eff),
                    check_mo=False)
                self.check_clean(res, model)
                self.assertEqual(model.w_out_max, eff)
                self.assertEqual(model.r_out_max, eff)

    def test_random_back_to_back(self):
        """Random configs back to back, checked against a Python shadow."""
        rnd = random.Random(2026)
        self.setup_dut(seed=77, b_delay=(1, 30), r_delay=(1, 30))
        model = self.model
        region = 8192
        shadow = {}
        runs = []
        for _ in range(16):
            blen = rnd.choice(VALID_BURST_LENS)
            bb = 8 * blen
            size = bb * rnd.randint(1, max(1, 640 // bb))
            base = GUARD_LO + bb * rnd.randrange((region - size) // bb + 1)
            runs.append(cfg_of(
                mode=rnd.choice([0, 1, 1, 2, 2, 3, 4]),
                pattern=rnd.randrange(NUM_PATTERNS), burst_len=blen,
                mo=rnd.randrange(16), passes=rnd.randint(1, 2),
                idle=rnd.choice([0, 0, 0, 1, 7]), seed=rnd.getrandbits(32),
                base=base, size=size))
        results = []

        async def bench(ctx):
            for cfg in runs:
                model.max_outstanding = min(max(cfg['mo'], 1), 8)
                results.append(await self.go(ctx, cfg))

        def predict(cfg):
            mode, pat, passes = cfg['mode'], cfg['pattern'], cfg['passes']
            addrs = range(cfg['base'], cfg['base'] + cfg['size'], 8)
            size = cfg['size']
            exp = dict(err_count=0, err_lanes=0, bytes_wr=0, bytes_rd=0,
                       first_err_addr=0, first_err_exp=0, first_err_act=0)
            if mode in (MODE_WRITE, MODE_WRITE_VERIFY):
                exp['bytes_wr'] = passes * size
                for a in addrs:
                    shadow[a] = expected_beat(pat, a, passes - 1,
                                              cfg['seed'])
            if mode == MODE_BYTE_LANE:
                exp['bytes_wr'] = 2 * passes * size
                for a in addrs:
                    shadow[a] = byte_lane_expected(a)
            if mode != MODE_WRITE:
                exp['bytes_rd'] = passes * size
            if mode == MODE_READ_VERIFY:
                first = None
                for a in addrs:
                    e = expected_beat(pat, a, 0, cfg['seed'])
                    act = shadow.get(a, 0)
                    if e != act:
                        exp['err_count'] += passes
                        exp['err_lanes'] |= e ^ act
                        if first is None:
                            first = (a, e, act)
                if first is not None:
                    (exp['first_err_addr'], exp['first_err_exp'],
                     exp['first_err_act']) = first
            exp['error'] = int(exp['err_count'] != 0)
            exp['pass_count'] = passes
            return exp

        self.simulate(bench)
        for i, (cfg, res) in enumerate(zip(runs, results)):
            with self.subTest(run=i, cfg=cfg):
                exp = predict(cfg)
                for k, v in exp.items():
                    self.assertEqual(res[k], v, k)
                self.assertEqual(res['bresp_err'], 0)
                self.assertEqual(res['rresp_err'], 0)
                self.assertEqual(res['irqs'], 1)
        for a in range(GUARD_LO, GUARD_LO + region, 8):
            self.assertEqual(model.read_beat(a), shadow.get(a, 0), hex(a))
        self.assertTrue(model.idle())

    # -- performance ----------------------------------------------------------

    def test_bandwidth_zero_latency(self):
        """16-beat bursts, 4 outstanding: >= 80 % of one beat per cycle."""
        size = 8192
        for mode in (MODE_WRITE, MODE_READ, MODE_WRITE_VERIFY):
            with self.subTest(mode=mode):
                cfg = cfg_of(mode=mode, pattern=PAT_PRBS, size=size, mo=4,
                             burst_len=16)
                res, model = self.run_one(
                    cfg, model_kw=dict(zero_latency=True))
                self.check_clean(res, model)
                beats = (res['bytes_wr'] + res['bytes_rd']) // 8
                eff = beats / res['cycles']
                self.assertGreaterEqual(eff, 0.8, f'efficiency {eff:.3f}')

    def test_idle_cycles_throttle(self):
        """idle_cycles spaces address issues by burst_len + idle_cycles."""
        size = 4096
        for mode in (MODE_WRITE, MODE_READ):
            with self.subTest(mode=mode):
                cfg = cfg_of(mode=mode, pattern=PAT_TOGGLE, size=size, mo=4,
                             burst_len=16, idle=16)
                res, model = self.run_one(
                    cfg, model_kw=dict(zero_latency=True))
                self.check_clean(res, model)
                log = model.aw_log if mode == MODE_WRITE else model.ar_log
                gaps = [b[0] - a[0] for a, b in zip(log, log[1:])]
                self.assertEqual(min(gaps), 32)
                beats = (res['bytes_wr'] + res['bytes_rd']) // 8
                eff = beats / res['cycles']
                self.assertGreater(eff, 0.4)
                self.assertLess(eff, 0.52)

    def check_hist(self, res, model):
        wl = [b - a[0] for a, b in zip(model.aw_log, model.b_log)]
        rl = [b - a[0] for a, b in zip(model.ar_log, model.rlast_log)]
        self.assertEqual(res['wlat_max'], max(wl))
        self.assertEqual(res['rlat_max'], max(rl))
        whist = [0] * LAT_BINS
        rhist = [0] * LAT_BINS
        for x in wl:
            whist[lat_bin(x)] += 1
        for x in rl:
            rhist[lat_bin(x)] += 1
        self.assertEqual(res['whist'], whist)
        self.assertEqual(res['rhist'], rhist)
        return wl, rl

    def test_latency_histogram(self):
        """Latency max and log2 histograms match the model's handshakes."""
        cfg = cfg_of(mode=MODE_WRITE_VERIFY, pattern=PAT_CHECKER, size=2048,
                     burst_len=8, mo=4, passes=2)
        res, model = self.run_one(
            cfg, hist=True,
            model_kw=dict(seed=18, b_delay=(1, 70), r_delay=(1, 90)))
        self.check_clean(res, model)
        wl, rl = self.check_hist(res, model)
        # Several bins populated
        self.assertGreaterEqual(sum(1 for x in res['whist'] if x), 2)
        self.assertGreaterEqual(sum(1 for x in res['rhist'] if x), 2)
        self.assertEqual(sum(res['whist']), 2 * 2048 // 64)
        self.assertEqual(sum(res['rhist']), 2 * 2048 // 64)

    def test_latency_zero_latency_model(self):
        """Deterministic latencies with the zero-latency model."""
        cfg = cfg_of(mode=MODE_WRITE_VERIFY, pattern=PAT_ADDRESS, size=1024,
                     burst_len=16, mo=1)
        res, model = self.run_one(cfg, hist=True,
                                  model_kw=dict(zero_latency=True))
        self.check_clean(res, model)
        wl, rl = self.check_hist(res, model)
        # AR -> RLAST is exactly 16 beats with the zero-latency model
        self.assertEqual(set(rl), {16})
        self.assertEqual(res['rhist'][4], 1024 // 128)


class TestAxiRWModel(unittest.TestCase):
    """Self-test of the subordinate model (checks fire, WSTRB, ordering)."""

    def run_mgr(self, bench, **kw):
        self.axi = axi.AxiInterface(
            axi.AxiDevice.MANAGER,
            [axi.AxiChannel(axi.AxiDirection.WRITE, 32, 64),
             axi.AxiChannel(axi.AxiDirection.READ, 32, 64)],
            axi.AxiVersion.AXI3)
        self.model = AxiRWModel(self.axi, **kw)
        m = Module()
        toggle = Signal()
        m.d.sync += toggle.eq(~toggle)   # creates the sync domain
        sim = Simulator(m)
        sim.add_clock(8e-9)
        sim.add_testbench(bench)
        sim.add_testbench(self.model.process, background=True)
        sim.run()

    async def aw(self, ctx, addr, length, *, change_after=None, size=3):
        a = self.axi
        ctx.set(a.awaddr, addr)
        ctx.set(a.awlen, length)
        ctx.set(a.awsize, size)
        ctx.set(a.awburst, 1)
        ctx.set(a.awcache, 0b0011)
        ctx.set(a.awvalid, 1)
        n = 0
        while True:
            _, _, rdy = await ctx.tick().sample(a.awready)
            n += 1
            if rdy:
                break
            if change_after is not None and n == change_after:
                ctx.set(a.awaddr, addr + 8)
        ctx.set(a.awvalid, 0)

    async def w(self, ctx, beats):
        a = self.axi
        for data, strb, last in beats:
            ctx.set(a.wdata, data)
            ctx.set(a.wstrb, strb)
            ctx.set(a.wlast, last)
            ctx.set(a.wvalid, 1)
            while True:
                _, _, rdy = await ctx.tick().sample(a.wready)
                if rdy:
                    break
        ctx.set(a.wvalid, 0)

    async def b(self, ctx):
        a = self.axi
        ctx.set(a.bready, 1)
        while True:
            _, _, v, resp = await ctx.tick().sample(a.bvalid, a.bresp)
            if v:
                ctx.set(a.bready, 0)
                return resp

    def test_payload_change_detected(self):
        async def bench(ctx):
            await self.aw(ctx, 0x1000, 0, change_after=2)

        with self.assertRaisesRegex(AxiProtocolError, 'AW changed'):
            self.run_mgr(bench, p_awready=0.0)

    def test_4k_crossing_detected(self):
        async def bench(ctx):
            await self.aw(ctx, 0x1FC0, 15)

        with self.assertRaisesRegex(AxiProtocolError, 'crosses 4 KiB'):
            self.run_mgr(bench, zero_latency=True)

    def test_bad_size_detected(self):
        async def bench(ctx):
            await self.aw(ctx, 0x1000, 0, size=2)

        with self.assertRaisesRegex(AxiProtocolError, 'size'):
            self.run_mgr(bench, zero_latency=True)

    def test_wlast_detected(self):
        async def bench(ctx):
            await self.aw(ctx, 0x1000, 3)
            await self.w(ctx, [(i, 0xFF, int(i == 2)) for i in range(4)])

        with self.assertRaisesRegex(AxiProtocolError, 'WLAST'):
            self.run_mgr(bench, zero_latency=True)

    def test_w_before_awvalid_detected(self):
        async def bench(ctx):
            await self.w(ctx, [(1, 0xFF, 1)])

        with self.assertRaisesRegex(AxiProtocolError, 'before its AWVALID'):
            self.run_mgr(bench, zero_latency=True, forbid_w_before_aw=True)

    def test_wstrb_w_first_and_slverr(self):
        out = {}

        async def bench(ctx):
            self.model.bresp_err_addrs.add(0x2000)
            # W data before the AW is legal and is matched when AW arrives
            await self.w(ctx, [(0x1111_2222_3333_4444, 0x0F, 0),
                               (0xAAAA_BBBB_CCCC_DDDD, 0x81, 1)])
            await self.aw(ctx, 0x2000, 1)
            out['bresp'] = await self.b(ctx)

        self.run_mgr(bench, seed=1, fill=lambda addr: 0x5A)
        m = self.model
        self.assertEqual(out['bresp'], SLVERR)
        self.assertEqual(m.read_beat(0x2000), 0x5A5A_5A5A_3333_4444)
        self.assertEqual(m.read_beat(0x2008), 0xAA5A_5A5A_5A5A_5ADD)
        self.assertEqual(m.w_early_beats, 2)

    def test_reads_in_order(self):
        out = []

        async def bench(ctx):
            a = self.axi
            self.model.fill_beats(0x3000, 256, lambda x: x * 3)
            self.model.rresp_err_addrs.add(0x3088)
            ctx.set(a.rready, 1)
            ctx.set(a.arsize, 3)
            ctx.set(a.arburst, 1)
            ctx.set(a.arcache, 0b0011)
            for addr, length in ((0x3000, 3), (0x3040, 0), (0x3080, 7)):
                ctx.set(a.araddr, addr)
                ctx.set(a.arlen, length)
                ctx.set(a.arvalid, 1)
                while True:
                    _, _, rdy, rv, rd, rl, rr = await ctx.tick().sample(
                        a.arready, a.rvalid, a.rdata, a.rlast, a.rresp)
                    if rv:
                        out.append((rd, rl, rr))
                    if rdy:
                        break
            ctx.set(a.arvalid, 0)
            while len(out) < 13:
                _, _, rv, rd, rl, rr = await ctx.tick().sample(
                    a.rvalid, a.rdata, a.rlast, a.rresp)
                if rv:
                    out.append((rd, rl, rr))

        self.run_mgr(bench, seed=3, p_arready=0.9)
        addrs = ([0x3000 + 8 * i for i in range(4)] + [0x3040]
                 + [0x3080 + 8 * i for i in range(8)])
        lasts = [0, 0, 0, 1, 1] + [0] * 7 + [1]
        self.assertEqual([x[0] for x in out], [x * 3 for x in addrs])
        self.assertEqual([x[1] for x in out], lasts)
        self.assertEqual([x[2] for x in out],
                         [0] * 6 + [SLVERR] + [0] * 6)
        self.assertGreaterEqual(self.model.r_out_max, 2)


if __name__ == '__main__':
    unittest.main()

#
# Fishball P25 -- LSM NID pipeline integration test
#
# Phase 6E.8b. Drives constructed dibit streams through
# `LsmNidPipeline` and verifies the full sync-detect ->
# NID-extract -> BCH-decode chain produces the right
# (NAC, DUID) results. This is the integration test for the
# new control logic in 6E.8 (sync hit -> BCH start handshake +
# drop counter); the demod loop is not exercised here because
# it's already covered by `test_lsm_demod_loop`.
#
# Sim cost
# --------
# Each NID -> BCH decode is ~65,538 sync ticks at the BCH
# decoder's serial sweep rate. amaranth-sim runs at ~5,000
# ticks/sec on this machine, so each integration trial is ~13
# seconds of sim time. We deliberately run **just one clean
# integration trial** by default. If a future bug needs more
# coverage, drop additional trials behind `MAIA_HDL_SLOW_TESTS=1`.
#
# SPDX-License-Identifier: MIT
#

import unittest

from amaranth import *
from amaranth.sim import Simulator

from p25_hdl.lsm_nid_pipeline import LsmNidPipeline
from p25_hdl.lsm_sync_nid_extract import (
    FRAME_SYNC_DIBIT_PATTERN,
    FRAME_SYNC_DIBITS,
    NID_TRANSMITTED_DIBITS,
    NID_STATUS_DIBIT_INDEX,
)
from p25_hdl.lsm_nid_bch_fec import encode_nid


def _sync_pattern_dibits():
    return [
        (FRAME_SYNC_DIBIT_PATTERN >> ((FRAME_SYNC_DIBITS - 1 - x) * 2)) & 0x3
        for x in range(FRAME_SYNC_DIBITS)
    ]


def _build_clean_stream(nac, duid, *, status_dibit=0):
    """Sync pattern + 33-dibit NID window with `status_dibit`
    spliced in at index 11."""
    nid = encode_nid(nac, duid)
    nid_dibits = [(nid >> ((31 - j) * 2)) & 0x3 for j in range(32)]

    out = list(_sync_pattern_dibits())
    payload_idx = 0
    for j in range(NID_TRANSMITTED_DIBITS):
        if j == NID_STATUS_DIBIT_INDEX:
            out.append(status_dibit & 0x3)
        else:
            out.append(nid_dibits[payload_idx])
            payload_idx += 1
    return out


class TestLsmNidPipelineIntegration(unittest.TestCase):

    def test_clean_sync_and_bch_decode(self):
        """End-to-end: drive a clean sync+NID dibit stream
        through `LsmNidPipeline` and verify the BCH decoder
        produces the right (NAC, DUID, n_errors=0, valid=1).
        Also verifies the sync_distance is latched and held
        across the ~656 us BCH sweep, the drop counter stays
        at 0, and `nid_event_strobe` fires exactly once."""
        nac, duid = 0x8A1, 7
        dibits = _build_clean_stream(nac, duid)

        result = {}
        nid_strobes_seen = 0

        async def bench(ctx):
            nonlocal nid_strobes_seen

            # Drive the dibit stream. Two ticks per dibit -- one
            # with strobe high, one with strobe low. The BCH
            # decoder is busy for ~65k ticks AFTER the last
            # dibit, so we then drain enough cycles to let
            # `nid_event_strobe` fire.
            for d in dibits:
                ctx.set(dut.dibit_in, d)
                ctx.set(dut.dibit_strobe, 1)
                await ctx.tick()
                ctx.set(dut.dibit_strobe, 0)
                await ctx.tick()

            # Drain up to 80,000 ticks to cover the worst-case
            # BCH sweep length plus a few extra cycles for the
            # event-strobe pulse.
            for _ in range(80_000):
                await ctx.tick()
                if ctx.get(dut.nid_event_strobe):
                    nid_strobes_seen += 1
                    result["nac"] = ctx.get(dut.nac_out)
                    result["duid"] = ctx.get(dut.duid_out)
                    result["n_errors"] = ctx.get(dut.n_errors_out)
                    result["valid"] = ctx.get(dut.valid_out)
                    result["sync_distance"] = ctx.get(
                        dut.sync_distance_out)
                    # Don't break -- keep counting strobes so we
                    # catch any spurious double-fire.
            result["drop_count"] = ctx.get(dut.nid_drop_count)

        dut = LsmNidPipeline()
        sim = Simulator(dut)
        sim.add_clock(10e-9)
        sim.add_testbench(bench)
        sim.run()

        self.assertEqual(
            nid_strobes_seen, 1,
            f"expected exactly 1 nid_event_strobe, "
            f"got {nid_strobes_seen}")
        self.assertEqual(
            result["nac"], nac,
            f"NAC mismatch: got 0x{result['nac']:03X}")
        self.assertEqual(result["duid"], duid)
        self.assertEqual(
            result["n_errors"], 0,
            f"clean codeword should have 0 errors, "
            f"got {result['n_errors']}")
        self.assertEqual(result["valid"], 1)
        self.assertEqual(
            result["sync_distance"], 0,
            f"clean sync should have distance 0, "
            f"got {result['sync_distance']}")
        self.assertEqual(
            result["drop_count"], 0,
            f"no NIDs should be dropped on a single decode, "
            f"got {result['drop_count']}")


if __name__ == '__main__':
    unittest.main()

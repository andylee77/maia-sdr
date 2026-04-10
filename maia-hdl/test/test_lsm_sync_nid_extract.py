#
# Fishball P25 -- LSM hard sync detector + NID extractor tests
#
# Phase 6E.8a. Drives `LsmSyncNidExtract` with hand-built dibit
# streams and verifies:
#
#   - clean sync pattern + clean NID -> one nid_strobe with the
#     right nid_out and distance==0
#   - 1-dibit error in the sync pattern -> still triggers
#     (within SYNC_THRESHOLD=4) with the right distance
#   - status dibit at index 11 is skipped (varying it doesn't
#     affect nid_out)
#   - the in_nid_window output tracks the FSM state
#   - back-to-back syncs in one stream -> two nid_strobes
#   - sync detection requires the register to be filled (no
#     false-trigger on a partial register early in the stream)
#
# These tests are CHEAP -- no BCH FEC sweep, just dibit shifts.
# Each test runs in well under a second of sim time.
#
# SPDX-License-Identifier: MIT
#

import unittest

from amaranth import *
from amaranth.sim import Simulator

from p25_hdl.lsm_sync_nid_extract import (
    LsmSyncNidExtract,
    FRAME_SYNC_DIBIT_PATTERN,
    FRAME_SYNC_DIBITS,
    NID_TRANSMITTED_DIBITS,
    NID_STATUS_DIBIT_INDEX,
    NID_BITS,
)
from p25_hdl.lsm_nid_bch_fec import encode_nid


# ----------------------------------------------------------------
# Helpers to build dibit streams that match the Rust test harness
# in `p25-httpd/src/lsm/sync.rs::tests`.
# ----------------------------------------------------------------

def sync_pattern_dibits():
    """Return the 24 dibits of FRAME_SYNC_DIBIT_PATTERN, MSB-first.
    Matches `dibits[x] = (PATTERN >> ((23-x)*2)) & 0x3` in the
    Rust test."""
    return [
        (FRAME_SYNC_DIBIT_PATTERN >> ((FRAME_SYNC_DIBITS - 1 - x) * 2)) & 0x3
        for x in range(FRAME_SYNC_DIBITS)
    ]


def nid_payload_dibits(nid_64bit):
    """Split a 64-bit NID into 32 dibits MSB-first.
    The byte at the MSB end of the codeword is the first dibit."""
    return [(nid_64bit >> ((31 - j) * 2)) & 0x3 for j in range(32)]


def build_nid_window(nid_64bit, *, status_dibit=0):
    """Build the 33-dibit transmitted NID window: 32 NID payload
    dibits with one status dibit spliced in at index 11."""
    payload = nid_payload_dibits(nid_64bit)
    out = []
    payload_idx = 0
    for j in range(NID_TRANSMITTED_DIBITS):
        if j == NID_STATUS_DIBIT_INDEX:
            out.append(status_dibit & 0x3)
        else:
            out.append(payload[payload_idx])
            payload_idx += 1
    assert payload_idx == 32
    return out


def build_clean_stream(nac, duid, *,
                       leading_noise=0,
                       trailing=0,
                       status_dibit=0):
    """Build a complete dibit stream: optional leading noise,
    sync pattern, NID window, optional trailing dibits."""
    nid = encode_nid(nac, duid)
    return (
        [0] * leading_noise
        + sync_pattern_dibits()
        + build_nid_window(nid, status_dibit=status_dibit)
        + [0] * trailing
    )


# ----------------------------------------------------------------
# Sim harness
# ----------------------------------------------------------------

def _drive_dibits(dut, dibits, *, max_extra_cycles=20):
    """Drive a dibit stream into the DUT and return all
    (nid_out, nid_distance) tuples emitted on nid_strobe.
    Also drains a few cycles after the last dibit so any final
    EMIT pulse has time to fire."""
    events = []

    async def bench(ctx):
        for d in dibits:
            ctx.set(dut.dibit_in, d)
            ctx.set(dut.dibit_strobe, 1)
            await ctx.tick()
            ctx.set(dut.dibit_strobe, 0)
            # Give the FSM one idle cycle between dibits so the
            # EMIT state can drain when the last dibit of a NID
            # window has just been consumed. This matches the
            # natural timing in the real LsmDemodLoop where dibits
            # come at the symbol rate (~31250 Hz) -- many sysclk
            # cycles apart.
            await ctx.tick()
            if ctx.get(dut.nid_strobe):
                events.append((
                    ctx.get(dut.nid_out),
                    ctx.get(dut.nid_distance),
                ))
        # Drain any pending strobe.
        for _ in range(max_extra_cycles):
            await ctx.tick()
            if ctx.get(dut.nid_strobe):
                events.append((
                    ctx.get(dut.nid_out),
                    ctx.get(dut.nid_distance),
                ))

    sim = Simulator(dut)
    sim.add_clock(10e-9)
    sim.add_testbench(bench)
    sim.run()
    return events


class TestLsmSyncNidExtract(unittest.TestCase):

    def test_clean_sync_and_nid_extraction(self):
        """Sync pattern + clean NID for NAC=0x8A1, DUID=7. Must
        emit exactly one nid_strobe with distance==0 and a
        nid_out matching encode_nid(0x8A1, 7)."""
        nac, duid = 0x8A1, 7
        dibits = build_clean_stream(nac, duid)
        dut = LsmSyncNidExtract()
        events = _drive_dibits(dut, dibits)

        self.assertEqual(len(events), 1,
                         f"expected 1 sync event, got {len(events)}")
        nid_out, dist = events[0]
        self.assertEqual(dist, 0,
                         f"expected distance 0, got {dist}")
        self.assertEqual(
            nid_out, encode_nid(nac, duid),
            f"expected nid 0x{encode_nid(nac, duid):016X}, "
            f"got 0x{nid_out:016X}")

    def test_one_dibit_error_in_sync_still_triggers(self):
        """Flip one bit of the second sync dibit -- this is well
        within SYNC_THRESHOLD=4 and must still produce a sync
        event. The NID is unaffected, so it must still decode
        correctly. Mirrors `hard_detector_tolerates_one_dibit_error_in_sync`
        in the Rust test suite."""
        nac, duid = 0x000, 0
        dibits = build_clean_stream(nac, duid)
        # Flip bit 1 (the high bit) of dibit 1 of the sync. The
        # original is 01, so the corrupted value is 11 -> Hamming
        # distance contribution = 1.
        dibits[1] ^= 0b10
        dut = LsmSyncNidExtract()
        events = _drive_dibits(dut, dibits)

        self.assertEqual(len(events), 1,
                         f"expected 1 sync event despite 1-bit "
                         f"error, got {len(events)}")
        nid_out, dist = events[0]
        self.assertGreaterEqual(dist, 1)
        self.assertLessEqual(dist, 2,
                             f"single-dibit flip should yield "
                             f"distance 1 or 2, got {dist}")
        self.assertEqual(nid_out, encode_nid(nac, duid))

    def test_status_dibit_is_skipped(self):
        """Build two streams identical except for the status
        dibit at index 11 of the NID window. The extracted NID
        must be the same in both."""
        nac, duid = 0x534, 2

        events_a = _drive_dibits(
            LsmSyncNidExtract(),
            build_clean_stream(nac, duid, status_dibit=0))
        events_b = _drive_dibits(
            LsmSyncNidExtract(),
            build_clean_stream(nac, duid, status_dibit=3))

        self.assertEqual(len(events_a), 1)
        self.assertEqual(len(events_b), 1)
        self.assertEqual(events_a[0][0], events_b[0][0],
                         "varying the status dibit must not "
                         "affect the extracted NID")
        self.assertEqual(events_a[0][0], encode_nid(nac, duid))

    def test_back_to_back_sync_events(self):
        """Two complete sync+NID windows in one stream. The
        decoder must emit two nid_strobes."""
        dibits = (
            build_clean_stream(0x8A1, 7)
            + build_clean_stream(0x123, 5)
        )
        dut = LsmSyncNidExtract()
        events = _drive_dibits(dut, dibits)

        self.assertEqual(len(events), 2,
                         f"expected 2 sync events, got {len(events)}")
        self.assertEqual(events[0][0], encode_nid(0x8A1, 7))
        self.assertEqual(events[0][1], 0)
        self.assertEqual(events[1][0], encode_nid(0x123, 5))
        self.assertEqual(events[1][1], 0)

    def test_no_false_trigger_before_register_fills(self):
        """Drive 23 dibits of arbitrary content (not enough to
        fill the 24-dibit window). The detector must NOT fire
        even if the partial-fill XOR happens to land within the
        threshold. Then drive a clean sync+NID and verify it
        still triggers exactly once."""
        # 23 dibits of zero -- intentionally NOT a sync pattern,
        # just confirming the fill gate works.
        dibits = [0] * 23 + build_clean_stream(0x8A1, 7)
        dut = LsmSyncNidExtract()
        events = _drive_dibits(dut, dibits)

        self.assertEqual(len(events), 1,
                         f"expected exactly 1 sync event after "
                         f"fill+sync, got {len(events)}")
        self.assertEqual(events[0][0], encode_nid(0x8A1, 7))
        self.assertEqual(events[0][1], 0)

    def test_in_nid_window_tracks_state(self):
        """`in_nid_window` should be 0 in IDLE, 1 during the
        33-dibit NID collection, and 0 again afterwards."""
        dibits = build_clean_stream(0x8A1, 7, trailing=4)
        dut = LsmSyncNidExtract()

        nid_window_log = []

        async def bench(ctx):
            await ctx.tick()
            nid_window_log.append(("idle", ctx.get(dut.in_nid_window)))
            for d in dibits:
                ctx.set(dut.dibit_in, d)
                ctx.set(dut.dibit_strobe, 1)
                await ctx.tick()
                ctx.set(dut.dibit_strobe, 0)
                await ctx.tick()
                nid_window_log.append((d, ctx.get(dut.in_nid_window)))
            for _ in range(4):
                await ctx.tick()
                nid_window_log.append(("after", ctx.get(dut.in_nid_window)))

        sim = Simulator(dut)
        sim.add_clock(10e-9)
        sim.add_testbench(bench)
        sim.run()

        # Initial idle: in_nid_window must be 0.
        self.assertEqual(nid_window_log[0][1], 0)
        # Last log entry: in_nid_window must be back to 0.
        self.assertEqual(nid_window_log[-1][1], 0)
        # Somewhere in the middle in_nid_window must have been 1
        # (for the 33-dibit NID collection window).
        self.assertTrue(
            any(v == 1 for _, v in nid_window_log),
            "in_nid_window never went high")


if __name__ == '__main__':
    unittest.main()

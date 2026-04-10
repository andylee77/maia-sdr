#
# Fishball P25 -- LSM NID BCH(63,16,11) FEC tests
#
# Phase 6E.7. Drives `LsmNidBchFec` with codewords built by the
# in-module software encoder (which is bit-exact with the Rust
# encoder in `p25-httpd/src/lsm/nid_fec.rs`) and verifies the
# HDL decoder matches.
#
# Sim cost
# --------
# Each HDL decode is ~65538 sync ticks; `amaranth-sim` clocks at
# ~5k ticks/sec on this machine, so one decode is ~13 seconds.
# That makes the suite very expensive if we naively port the Rust
# `error_correction_sweep_up_to_t11` test (~150 decodes -> ~30
# minutes). Instead we run a deliberately small set of decodes
# that cover the algorithm-critical paths and rely on
# `nid_fec.rs::tests` for the broad coverage:
#
#   - encoder reference matches the SDRTrunk golden vector       (0 decodes)
#   - clean codeword decode -> 0 errors                           (1 decode)
#   - single-bit error in a few representative positions          (3 decodes)
#   - error correction at t=1 / t=6 / t=11                        (3 decodes)
#   - decoder is reusable across consecutive starts               (covered above)
#   - `done` strobe is one cycle and `busy` tracks the state      (covered above)
#
# Default: ~7 decodes, ~90 seconds.
#
# Set `MAIA_HDL_SLOW_TESTS=1` in the environment to additionally
# run the full sweep that mirrors the Rust test verbatim
# (5 trials per error level x 11 levels + all 64 single-bit
# positions = 119 decodes, ~25 minutes). This is meant for the
# pre-bitstream-bake confidence pass, not for routine TDD.
#
# SPDX-License-Identifier: MIT
#

import os
import unittest

from amaranth import *
from amaranth.sim import Simulator

from p25_hdl.lsm_nid_bch_fec import (
    LsmNidBchFec,
    encode_nid,
    CODE_BITS,
    T_MAX_ERRORS,
)


SLOW = os.environ.get("MAIA_HDL_SLOW_TESTS", "0") == "1"


# ----------------------------------------------------------------
# Deterministic xorshift64 PRNG -- mirrors the Rust test PRNG so
# the slow-mode HDL run picks the same bit-flip patterns the Rust
# unit tests in `nid_fec.rs` exercise.
# ----------------------------------------------------------------

class _XorShift64:
    def __init__(self, seed):
        self.s = seed if seed != 0 else 1

    def next_u64(self):
        x = self.s
        x ^= (x << 13) & 0xFFFF_FFFF_FFFF_FFFF
        x ^= (x >> 7) & 0xFFFF_FFFF_FFFF_FFFF
        x ^= (x << 17) & 0xFFFF_FFFF_FFFF_FFFF
        self.s = x
        return x

    def next_u32(self):
        return self.next_u64() & 0xFFFF_FFFF


def _flip_bits(base, positions):
    """Flip the listed `positions` (0..62, SDRTrunk numbering) in
    the 64-bit codeword `base`. Position 0 is bit 63 of the u64,
    position 62 is bit 1 -- bit 0 (the unused parity LSB) is
    deliberately left untouched, matching SDRTrunk's test."""
    out = base
    for p in positions:
        out ^= 1 << (CODE_BITS - 1 - p)
    return out


# ----------------------------------------------------------------
# Simulation harness
# ----------------------------------------------------------------

def _run_decodes(dut, received_nids, *, max_cycles_per=70000):
    """Drive several consecutive decodes through one DUT instance
    and return a list of (nac, duid, n_errors, valid) tuples.
    Reusing one DUT exercises the start-after-done path."""

    results = []

    async def bench(ctx):
        for nid in received_nids:
            ctx.set(dut.received_nid, nid & ((1 << CODE_BITS) - 1))
            ctx.set(dut.start, 1)
            await ctx.tick()
            ctx.set(dut.start, 0)
            for _ in range(max_cycles_per):
                await ctx.tick()
                if ctx.get(dut.done):
                    results.append((
                        ctx.get(dut.nac_out),
                        ctx.get(dut.duid_out),
                        ctx.get(dut.n_errors_out),
                        ctx.get(dut.valid_out),
                    ))
                    break
            else:
                raise AssertionError(
                    f"decoder timed out on nid 0x{nid:016X}")

    sim = Simulator(dut)
    sim.add_clock(10e-9)
    sim.add_testbench(bench)
    sim.run()
    return results


class TestLsmNidBchFecEncoderRef(unittest.TestCase):
    """Pure-software checks on the encoder reference. No sim cost.
    These run in milliseconds and catch any future drift in the
    generator matrix or the bit-ordering convention."""

    def test_encoder_reference_matches_sdrtrunk_vector(self):
        """`encode_nid(1, 0)` must match the SDRTrunk-published
        BCH golden vector. If this fails the generator matrix or
        the bit ordering is wrong and nothing else here is
        meaningful."""
        cw = encode_nid(1, 0)
        self.assertEqual(
            cw, 0x0010_3185_B7E9_E224,
            f"encoder produced 0x{cw:016X}, "
            f"expected 0x00103185B7E9E224")

    def test_encoder_data_field_layout(self):
        """The 16 data bits should land in the high 16 bits of
        the codeword (bits 48..63), with NAC in 52..63 and DUID
        in 48..51 -- nac_out / duid_out in the HDL slice this
        same way."""
        cw = encode_nid(0x8A1, 7)
        data_field = (cw >> 48) & 0xFFFF
        self.assertEqual(data_field, (0x8A1 << 4) | 7)


class TestLsmNidBchFecHdl(unittest.TestCase):
    """Sim-driven checks on the HDL decoder. Each test method
    above the class costs ~13s of sim per decode."""

    def test_clean_codeword_and_done_strobe(self):
        """Drive one clean codeword and confirm the decoder
        produces (NAC, DUID, 0 errors, valid). Also captures the
        `done` pulse width and the `busy` waveform across the
        full decode in the same simulation, since spinning up a
        second simulator just for those would double sim time."""
        nac, duid = 0x8A1, 7
        cw = encode_nid(nac, duid)

        seen = {}

        async def bench(ctx):
            # Idle baseline.
            await ctx.tick()
            self.assertEqual(ctx.get(dut.busy), 0,
                             "busy must start at 0 before start")

            ctx.set(dut.received_nid, cw)
            ctx.set(dut.start, 1)
            await ctx.tick()
            ctx.set(dut.start, 0)

            # Sample busy a few cycles in -- it must be high.
            for _ in range(5):
                await ctx.tick()
            self.assertEqual(
                ctx.get(dut.busy), 1,
                "busy must be 1 once the sweep is running")

            done_high_cycles = 0
            seen_done = False
            for _ in range(70000):
                await ctx.tick()
                if ctx.get(dut.done):
                    if not seen_done:
                        seen["nac"] = ctx.get(dut.nac_out)
                        seen["duid"] = ctx.get(dut.duid_out)
                        seen["n_errors"] = ctx.get(dut.n_errors_out)
                        seen["valid"] = ctx.get(dut.valid_out)
                    done_high_cycles += 1
                    seen_done = True
                elif seen_done:
                    break
            self.assertTrue(seen_done, "decoder never pulsed done")
            self.assertEqual(
                done_high_cycles, 1,
                f"done was high for {done_high_cycles} cycles, "
                f"expected exactly 1")

            # One cycle after done, busy must be 0 again.
            await ctx.tick()
            self.assertEqual(
                ctx.get(dut.busy), 0,
                "busy must drop back to 0 after done")

        dut = LsmNidBchFec()
        sim = Simulator(dut)
        sim.add_clock(10e-9)
        sim.add_testbench(bench)
        sim.run()

        self.assertEqual(seen["nac"], nac)
        self.assertEqual(seen["duid"], duid)
        self.assertEqual(seen["n_errors"], 0)
        self.assertEqual(seen["valid"], 1)

    def test_single_bit_error_sample_positions(self):
        """Flip exactly one bit at a small set of representative
        positions (the four corners of the 16/48-bit field
        boundary) and confirm the decoder corrects each one. Also
        verifies the decoder is reusable across decodes."""
        nac, duid = 0x8A1, 7
        base = encode_nid(nac, duid)
        # Hit the data MSB, the data/parity boundary, and the
        # parity LSB+1 (we never flip parity bit 0; see the
        # SDRTrunk-numbering note in `_flip_bits`).
        sample_positions = [0, 15, 16, 47]

        dut = LsmNidBchFec()
        nids = [base ^ (1 << (CODE_BITS - 1 - p))
                for p in sample_positions]
        results = _run_decodes(dut, nids)

        self.assertEqual(len(results), len(sample_positions))
        for pos, (got_nac, got_duid, got_errs, got_valid) in zip(
                sample_positions, results):
            self.assertEqual(
                (got_nac, got_duid, got_errs, got_valid),
                (nac, duid, 1, 1),
                f"single-bit flip at SDRTrunk pos {pos}: got "
                f"nac=0x{got_nac:03X} duid=0x{got_duid:X} "
                f"errs={got_errs} valid={got_valid}")

    def test_error_correction_at_t1_t6_t11(self):
        """Three corrupted codewords -- one at the easy edge (t=1),
        one mid-range (t=6), one at the corner of the
        unique-decoding sphere (t=11). Each is built with a
        deterministic xorshift PRNG seed so re-runs are
        reproducible. The full sweep across error counts 1..11
        with multiple trials per level lives in
        `nid_fec.rs::tests::error_correction_sweep_up_to_t11` --
        the HDL test only needs to confirm the implementation
        agrees on a representative sample."""
        nac, duid = 0x8A1, 7
        base = encode_nid(nac, duid)
        rng = _XorShift64(0xDEAD_BEEF_CAFE_BABE)

        nids = []
        expected = []
        for n_errors in (1, 6, 11):
            positions = []
            while len(positions) < n_errors:
                p = rng.next_u32() % 63
                if p not in positions:
                    positions.append(p)
            nids.append(_flip_bits(base, positions))
            expected.append(n_errors)

        dut = LsmNidBchFec()
        results = _run_decodes(dut, nids)

        self.assertEqual(len(results), 3)
        for n_errors, (got_nac, got_duid, got_errs, got_valid) in zip(
                expected, results):
            self.assertEqual(
                got_nac, nac,
                f"{n_errors}-error decode: nac mismatch "
                f"got 0x{got_nac:03X}")
            self.assertEqual(got_duid, duid)
            self.assertEqual(
                got_errs, n_errors,
                f"expected {n_errors} corrected errors, "
                f"got {got_errs}")
            self.assertEqual(got_valid, 1)


@unittest.skipUnless(SLOW, "set MAIA_HDL_SLOW_TESTS=1 to enable")
class TestLsmNidBchFecSlowSweep(unittest.TestCase):
    """Optional thorough sweep -- mirrors the Rust test exactly.
    Costs ~25 minutes of sim time on this machine, so it's gated
    behind an env var. Run before bitstream bake / on a CI box."""

    def test_error_correction_sweep_up_to_t11(self):
        nac, duid = 0x8A1, 7
        base = encode_nid(nac, duid)
        rng = _XorShift64(0xDEAD_BEEF_CAFE_BABE)
        TRIALS_PER_LEVEL = 5

        nids = []
        expected_n_errors = []
        for n_errors in range(1, T_MAX_ERRORS + 1):
            for _ in range(TRIALS_PER_LEVEL):
                positions = []
                while len(positions) < n_errors:
                    p = rng.next_u32() % 63
                    if p not in positions:
                        positions.append(p)
                nids.append(_flip_bits(base, positions))
                expected_n_errors.append(n_errors)

        dut = LsmNidBchFec()
        results = _run_decodes(dut, nids)
        self.assertEqual(len(results), len(nids))

        for idx, ((got_nac, got_duid, got_errs, got_valid), exp_errs) \
                in enumerate(zip(results, expected_n_errors)):
            self.assertEqual(got_nac, nac, f"trial {idx}")
            self.assertEqual(got_duid, duid, f"trial {idx}")
            self.assertEqual(
                got_errs, exp_errs,
                f"trial {idx}: expected {exp_errs} got {got_errs}")
            self.assertEqual(got_valid, 1, f"trial {idx}")

    def test_all_64_single_bit_positions(self):
        nac, duid = 0x8A1, 7
        base = encode_nid(nac, duid)
        dut = LsmNidBchFec()
        nids = [base ^ (1 << bit) for bit in range(CODE_BITS)]
        results = _run_decodes(dut, nids)
        self.assertEqual(len(results), CODE_BITS)
        for bit, (got_nac, got_duid, got_errs, got_valid) in enumerate(
                results):
            self.assertEqual(
                (got_nac, got_duid, got_errs, got_valid),
                (nac, duid, 1, 1),
                f"bit {bit}")


if __name__ == '__main__':
    unittest.main()

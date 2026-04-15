#
# Fishball P25 -- LSM hard sync detector + status-skipping NID extractor
#
# Phase 6E.8 of the LSM HDL port. Streaming HDL equivalent of
# `find_sync_events_hard()` + `extract_nid_skipping_status()` in
# `p25-httpd/src/lsm/sync.rs`. Consumes the dibit stream out of
# `LsmDemodLoop` and emits a 64-bit NID word per sync hit, ready
# for `LsmNidBchFec`.
#
# Pipeline
# --------
#
#   dibit_in/dibit_strobe (from LsmDemodLoop)
#                  |
#                  v
#       48-bit shift register
#                  |
#                  v
#       popcount(reg ^ FRAME_SYNC_PATTERN) <= 4 ?
#                  | yes
#                  v
#       enter COLLECT_NID for 33 dibits
#                  |
#                  v
#       skip index 11 (status dibit), pack remaining 32 dibits
#       into a 64-bit NID word, MSB-first
#                  |
#                  v
#       nid_out + nid_strobe
#
# Why hard detector and not soft
# ------------------------------
# The Rust `find_sync_events_soft` does an inner product against
# the 24 ideal `+/- 3PI/4` symbol phases of the sync pattern. That
# requires the soft phase output (`atan2`) of the differential
# demodulator -- which we deliberately did not compute in 6E.5
# (the slicer just returns `Cat(i_sign, q_sign)`). Implementing
# the soft detector in HDL would either need a CORDIC vector mode
# in front of the slicer or a separate quantised-phase tap. Both
# are doable but neither is on the critical path for first
# bring-up. The hard detector is a literal 1:1 of what
# `lsm::sync::find_sync_events_hard` does and is the version the
# Rust comments call "the simple HDL-friendly version".
#
# If the hard detector ever turns out to under-perform on real
# RF (e.g. simulcast capture with a lot of multipath), the soft
# detector slots in as a sub-phase 6E.8.5: add an `atan2` tap to
# `LsmDiffDemodSlicer`, build a 24-tap fixed-point inner-product
# correlator, and OR the two detectors' nid_strobes together.
#
# Sync register bit ordering
# --------------------------
# Matches `lsm::sync::find_sync_events_hard` exactly: each new
# dibit is shifted into the LSB and the oldest dibit drops off
# the MSB end (after a 24-dibit fill). The pattern constant
# `FRAME_SYNC_DIBIT_PATTERN = 0x5575_F5FF_77FF` is the same
# bit-packing SDRTrunk uses (`P25P1SyncDetector.SYNC_PATTERN`)
# and matches TIA-102.BAAA. Dibit 0 of the pattern is bits
# 47..46, dibit 23 is bits 1..0.
#
# NID assembly bit ordering
# -------------------------
# Mirrors `extract_nid_skipping_status()` in `lsm::sync`:
#
#     for j in 0..33:
#         if j == 11:
#             continue                  # status dibit
#         nid_bits = (nid_bits << 2) | dibit
#
# After 33 dibits the resulting 64-bit `nid_bits` has the NAC in
# bits 63..52, the DUID in bits 51..48, and the 48 BCH parity
# bits in 47..0 -- exactly the format `LsmNidBchFec` expects.
#
# State machine
# -------------
#
#     IDLE          -- shift dibits, wait for fill, check threshold
#       on dibit_strobe and reg_full and dist <= SYNC_THRESHOLD:
#         -> COLLECT_NID, dibit_count <= 0, nid_word <= 0
#
#     COLLECT_NID   -- accumulate the 33 dibits of the NID window
#       on dibit_strobe:
#         if dibit_count != STATUS_DIBIT_INDEX:
#             nid_word <= (nid_word << 2) | dibit
#         dibit_count <= dibit_count + 1
#         if dibit_count + 1 == NID_TRANSMITTED_DIBITS:
#             pulse nid_strobe (with nid_word, distance latched in IDLE)
#             clear sync register and reg_full
#             -> IDLE
#
# Clearing the sync register on exit matches the Rust loop's
# `sync_register = 0` post-hit reset, which prevents
# false-triggering on dibits that look syncish but are really NID
# tail bytes.
#
# I/O
# ---
# Inputs (sync domain):
#     dibit_in     : Signal(2)
#     dibit_strobe : Signal()
#
# Outputs (sync domain):
#     nid_out         : Signal(64)   -- valid when nid_strobe is high
#     nid_distance    : Signal(7)    -- Hamming dist of the sync hit
#     nid_strobe      : Signal()     -- 1-cycle pulse per recovered NID
#     in_nid_window   : Signal()     -- high while collecting NID dibits
#                                       (useful for the dashboard's
#                                       "have lock" indicator and for
#                                       gating downstream consumers)
#
# Resource estimate (Z7020)
# -------------------------
# - 48-bit shift register
# - 64-bit NID accumulator
# - 6-bit dibit_count, 5-bit reg_fill_count
# - 48-bit popcount tree -> 6-bit dist + compare-against-4
# - small FSM
#
# Total: <50 LUT, <130 FF, **0 BRAM, 0 DSP**.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *


# ----------------------------------------------------------------
# Constants -- mirror `lsm::sync`
# ----------------------------------------------------------------

# 48-bit P25 frame sync (24 dibits), packed dibits-MSB-first.
FRAME_SYNC_DIBIT_PATTERN = 0x5575_F5FF_77FF
FRAME_SYNC_BITS = 48
FRAME_SYNC_DIBITS = 24

# Hamming distance threshold for the hard sync detector.
SYNC_THRESHOLD = 4

# 33 dibits transmitted for the NID window, including 1 status
# dibit at index 11.
NID_TRANSMITTED_DIBITS = 33
NID_STATUS_DIBIT_INDEX = 11

# Number of dibits in the assembled NID word (33 - 1 status).
NID_PAYLOAD_DIBITS = NID_TRANSMITTED_DIBITS - 1   # 32
NID_BITS = NID_PAYLOAD_DIBITS * 2                 # 64


class LsmSyncNidExtract(Elaboratable):
    """Hard sync detector + status-skipping NID extractor.

    See module-level docstring for the architectural rationale,
    state machine, and bit-ordering conventions. This block does
    NOT include the BCH FEC decoder -- it just produces the
    64-bit NID word that `LsmNidBchFec` consumes.
    """

    def __init__(self):
        # ── Inputs ──────────────────────────────────────────────
        self.dibit_in = Signal(2)
        self.dibit_strobe = Signal()
        # Phase 8A: runtime reset. A 1-cycle pulse clears the
        # 48-bit sync shift register + fill counter + NID
        # assembly state and forces the FSM back to IDLE.
        self.reset_in = Signal()

        # ── Outputs ─────────────────────────────────────────────
        self.nid_out = Signal(NID_BITS, reset_less=True)
        self.nid_distance = Signal(7, reset_less=True)
        self.nid_strobe = Signal()
        self.in_nid_window = Signal()

    def elaborate(self, platform):
        m = Module()

        # ── Sync shift register ─────────────────────────────────
        # 48 bits, MSB end is the oldest dibit, LSB end is the
        # most-recently shifted-in dibit.
        sync_reg = Signal(FRAME_SYNC_BITS, init=0, reset_less=True)

        # Counts how many dibits have been shifted in since the
        # last clear. Capped at FRAME_SYNC_DIBITS (24); once full
        # the register is valid for threshold comparisons.
        FILL_WIDTH = (FRAME_SYNC_DIBITS).bit_length()  # 5
        reg_fill = Signal(FILL_WIDTH, init=0, reset_less=True)
        reg_full = Signal()
        m.d.comb += reg_full.eq(reg_fill >= FRAME_SYNC_DIBITS)

        # ── NID collection state ────────────────────────────────
        nid_word = Signal(NID_BITS, init=0, reset_less=True)
        # 6 bits is enough for 0..32 (we count after the dibit
        # is consumed; max value reached is NID_TRANSMITTED_DIBITS).
        DIBIT_COUNT_WIDTH = 6
        dibit_count = Signal(DIBIT_COUNT_WIDTH, init=0, reset_less=True)
        latched_distance = Signal(7, init=0, reset_less=True)

        # Default outputs (de-asserted unless explicitly raised).
        m.d.sync += self.nid_strobe.eq(0)

        with m.FSM(init="IDLE") as fsm:
            m.d.comb += self.in_nid_window.eq(
                ~fsm.ongoing("IDLE"))

            with m.State("IDLE"):
                # Phase 8A: handled by the reset override block
                # below — IDLE doesn't need an m.next change.
                with m.If(self.dibit_strobe):
                    # Shift the new dibit into the sync register
                    # and bump the fill counter (saturating at
                    # FRAME_SYNC_DIBITS).
                    new_reg = Signal(FRAME_SYNC_BITS)
                    m.d.comb += new_reg.eq(
                        Cat(self.dibit_in, sync_reg[:FRAME_SYNC_BITS - 2]))
                    m.d.sync += sync_reg.eq(new_reg)
                    with m.If(~reg_full):
                        m.d.sync += reg_fill.eq(reg_fill + 1)

                    # Threshold check uses the *new* register
                    # value. We need to compare against
                    # popcount(new_reg ^ PATTERN), not against the
                    # registered `dist` (which lags by one cycle).
                    new_diff = Signal(FRAME_SYNC_BITS)
                    m.d.comb += new_diff.eq(
                        new_reg ^ FRAME_SYNC_DIBIT_PATTERN)
                    new_dist = Signal(7)
                    m.d.comb += new_dist.eq(
                        sum(new_diff[i] for i in range(FRAME_SYNC_BITS)))

                    # Hit only if the register is fully filled --
                    # otherwise the high bits are still init=0
                    # which would land within the threshold of any
                    # zero-heavy pattern by accident.
                    fill_after = Signal(FILL_WIDTH + 1)
                    m.d.comb += fill_after.eq(reg_fill + 1)
                    full_after = Signal()
                    m.d.comb += full_after.eq(
                        fill_after >= FRAME_SYNC_DIBITS)

                    with m.If(full_after & (new_dist <= SYNC_THRESHOLD)):
                        m.d.sync += [
                            latched_distance.eq(new_dist),
                            nid_word.eq(0),
                            dibit_count.eq(0),
                        ]
                        m.next = "COLLECT_NID"

            with m.State("COLLECT_NID"):
                # Phase 8A: force back to IDLE on runtime reset.
                with m.If(self.reset_in):
                    m.next = "IDLE"
                with m.If(self.dibit_strobe):
                    # Skip the status dibit at index
                    # NID_STATUS_DIBIT_INDEX (11), append every
                    # other dibit MSB-first.
                    is_status = (dibit_count == NID_STATUS_DIBIT_INDEX)
                    with m.If(~is_status):
                        m.d.sync += nid_word.eq(
                            Cat(self.dibit_in, nid_word[:NID_BITS - 2]))
                    m.d.sync += dibit_count.eq(dibit_count + 1)

                    # Are we processing the LAST dibit of the
                    # window? Then the NID is complete *next*
                    # cycle (after the shift above settles into
                    # nid_word).
                    last_dibit = Signal()
                    m.d.comb += last_dibit.eq(
                        dibit_count == NID_TRANSMITTED_DIBITS - 1)
                    with m.If(last_dibit):
                        m.next = "EMIT"

            with m.State("EMIT"):
                # Phase 8A: force back to IDLE on runtime reset.
                with m.If(self.reset_in):
                    m.next = "IDLE"
                # One pure-emit cycle so the new nid_word value
                # registered in COLLECT_NID is visible on nid_out
                # at the same time as nid_strobe.
                m.d.sync += [
                    self.nid_out.eq(nid_word),
                    self.nid_distance.eq(latched_distance),
                    self.nid_strobe.eq(1),
                    # Clear the sync register so we don't
                    # false-trigger on the dibits we just
                    # consumed. We deliberately do NOT reset
                    # `reg_fill` -- that would impose an artificial
                    # 24-dibit refill gate that the Rust reference
                    # does not have. Rust relies on the fact that
                    # `popcount(0 ^ pattern) = 37`, so the
                    # threshold check naturally rejects the
                    # near-empty register until ~20 sync-pattern
                    # dibits have shifted in, which is what we
                    # want. This matches `lsm::sync` exactly.
                    sync_reg.eq(0),
                ]
                m.next = "IDLE"

        # ── Phase 8A runtime reset override ─────────────────────
        # Clear all the persistent state registers. The FSM
        # state-register itself is forced back to IDLE by the
        # per-state `m.next = "IDLE"` overrides above (IDLE is
        # already a no-op self-loop, so it doesn't need one).
        with m.If(self.reset_in):
            m.d.sync += [
                sync_reg.eq(0),
                reg_fill.eq(0),
                nid_word.eq(0),
                dibit_count.eq(0),
                latched_distance.eq(0),
                self.nid_out.eq(0),
                self.nid_distance.eq(0),
                self.nid_strobe.eq(0),
            ]

        return m

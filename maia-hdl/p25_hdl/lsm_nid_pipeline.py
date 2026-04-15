#
# Fishball P25 -- LSM NID pipeline (sync detect + BCH decode)
#
# Phase 6E.8 of the LSM HDL port. The dibit-to-NID half of
# `LsmDemod`, factored out so it can be integration-tested
# without dragging the IQ-to-dibit demod loop and the synthetic
# IQ golden into the test bench.
#
# Pipeline
# --------
#
#   dibits --> [LsmSyncNidExtract] --> nid_word --> [LsmNidBchFec]
#                                                          |
#                                                          v
#                                          (NAC, DUID, n_errors,
#                                           valid, sync_distance,
#                                           nid_event_strobe)
#
# This module is **the only place** in the design that owns the
# nid_strobe -> bch.start handshake and the NID-drop counter,
# so the policy lives in one tested location. `LsmDemod`
# instantiates this submodule and the demod loop side by side.
#
# Handshake policy
# ----------------
# - `nid_strobe` from the sync detector is fed to `bch.start`,
#   gated by `~bch.busy`. If a NID arrives mid-decode it is
#   silently dropped and `nid_drop_count` is bumped (saturating
#   at 0xFFFF). NIDs are spaced ~14 ms apart on real RF and the
#   BCH decode takes ~656 us, so the drop counter should always
#   be 0 in normal operation. If it ever turns out non-zero on
#   hardware, that's the signal to add a tiny FIFO between the
#   sync detector and the BCH input.
#
# - `sync_distance` from the sync detector is latched on the
#   start handshake so it survives the ~656 us BCH sweep and
#   can be presented alongside the BCH result on
#   `nid_event_strobe`.
#
# I/O
# ---
# Inputs (sync domain):
#     dibit_in     : Signal(2)
#     dibit_strobe : Signal()
#
# Outputs (sync domain):
#     nid_event_strobe  : Signal()       -- one cycle per NID
#     nac_out           : Signal(12)
#     duid_out          : Signal(4)
#     n_errors_out      : Signal(7)
#     valid_out         : Signal()
#     sync_distance_out : Signal(7)
#     in_nid_window     : Signal()
#     bch_busy          : Signal()
#     nid_drop_count    : Signal(16)
#
# SPDX-License-Identifier: MIT
#

from amaranth import *

from .lsm_sync_nid_extract import LsmSyncNidExtract
from .lsm_nid_bch_fec import LsmNidBchFec


class LsmNidPipeline(Elaboratable):
    """Sync detect + status-skip NID extract + BCH FEC.

    The dibit-to-NID half of `LsmDemod`. See module-level
    docstring for the handshake policy and resource estimate.
    """

    def __init__(self):
        # ── Inputs ──────────────────────────────────────────────
        self.dibit_in = Signal(2)
        self.dibit_strobe = Signal()
        # Phase 8A: runtime reset. Propagated to sync_nid and bch
        # and used to clear the per-NID latched_sync_distance and
        # the event output registers.
        self.reset_in = Signal()

        # ── Outputs ─────────────────────────────────────────────
        self.nid_event_strobe = Signal()
        self.nac_out = Signal(12, reset_less=True)
        self.duid_out = Signal(4, reset_less=True)
        self.n_errors_out = Signal(7, reset_less=True)
        self.valid_out = Signal(reset_less=True)
        self.sync_distance_out = Signal(7, reset_less=True)
        self.in_nid_window = Signal()
        self.bch_busy = Signal()
        self.nid_drop_count = Signal(16, init=0, reset_less=True)

    def elaborate(self, platform):
        m = Module()

        m.submodules.sync_nid = sync_nid = LsmSyncNidExtract()
        m.submodules.bch = bch = LsmNidBchFec()

        # ── Phase 8A runtime reset fan-out ──────────────────────
        m.d.comb += [
            sync_nid.reset_in.eq(self.reset_in),
            bch.reset_in.eq(self.reset_in),
        ]

        # ── Sync detect + NID assembly ─────────────────────────
        m.d.comb += [
            sync_nid.dibit_in.eq(self.dibit_in),
            sync_nid.dibit_strobe.eq(self.dibit_strobe),
            self.in_nid_window.eq(sync_nid.in_nid_window),
        ]

        # ── BCH start handshake ────────────────────────────────
        latched_sync_distance = Signal(7, reset_less=True)

        bch_start_pulse = Signal()
        m.d.comb += [
            bch.received_nid.eq(sync_nid.nid_out),
            bch_start_pulse.eq(sync_nid.nid_strobe & ~bch.busy),
            bch.start.eq(bch_start_pulse),
            self.bch_busy.eq(bch.busy),
        ]

        with m.If(bch_start_pulse):
            m.d.sync += latched_sync_distance.eq(
                sync_nid.nid_distance)
        with m.Elif(sync_nid.nid_strobe & bch.busy):
            # Saturating drop counter -- should never fire on
            # real RF.
            with m.If(self.nid_drop_count != ((1 << 16) - 1)):
                m.d.sync += self.nid_drop_count.eq(
                    self.nid_drop_count + 1)

        # ── BCH done -> latch outputs + pulse strobe ───────────
        m.d.sync += self.nid_event_strobe.eq(0)
        with m.If(bch.done):
            m.d.sync += [
                self.nac_out.eq(bch.nac_out),
                self.duid_out.eq(bch.duid_out),
                self.n_errors_out.eq(bch.n_errors_out),
                self.valid_out.eq(bch.valid_out),
                self.sync_distance_out.eq(latched_sync_distance),
                self.nid_event_strobe.eq(1),
            ]

        # ── Phase 8A runtime reset override ─────────────────────
        # Clear the latched sync distance + BCH-result latches +
        # the NID-drop counter. NOTE that the drop counter is a
        # diagnostic (should always read 0), and 8A resetting it
        # on every retune means its semantics become "dropped
        # since the last retune" instead of "dropped since boot",
        # which matches the intent of the Phase 8B PS integration
        # (one reset = one fresh decode window per call).
        with m.If(self.reset_in):
            m.d.sync += [
                latched_sync_distance.eq(0),
                self.nac_out.eq(0),
                self.duid_out.eq(0),
                self.n_errors_out.eq(0),
                self.valid_out.eq(0),
                self.sync_distance_out.eq(0),
                self.nid_event_strobe.eq(0),
                self.nid_drop_count.eq(0),
            ]

        return m

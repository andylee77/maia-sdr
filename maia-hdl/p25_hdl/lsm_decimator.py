#
# Fishball P25 -- LSM /2 streaming decimator
#
# Phase 6E.1 of the LSM HDL port. Direct Amaranth equivalent of
# `lsm::filters::StreamingDecimator2` in
# `p25-httpd/src/lsm/filters.rs`.
#
# Architecture
# ------------
# Naive /2 decimation: emit every other input sample, drop the rest.
# Safe here because the upstream Maia DDC's stage-3 FIR is a 64-tap
# Kaiser LPF that already attenuates everything outside +/-8 kHz at
# 62.5 kSPS by 166+ dB; folding the upper half (8..31.25 kHz) into
# 0..8 kHz at 31.25 kSPS adds no measurable distortion to the
# 6.25 kHz P25 channel.
#
# This block has no DSP cost. It is one register and a 1-bit
# counter. The reason it has its own Elaboratable rather than being
# folded into the next stage is that the rate change cleanly
# separates the 62.5 kSPS clock domain (matching the existing C4FM
# chain) from the 31.25 kSPS rate that drives the LPF, RRC, and
# demod loop. Keeps the FIR scheduling simple.
#
# Streaming behaviour matches the Rust reference: on the first
# strobe after reset the input sample is *emitted* (phase 0 -> 1),
# the second is dropped (phase 1 -> 0), the third is emitted, and
# so on. Equivalent to `input[0::2]` -- a /2 decimator that takes
# the even-indexed samples. Phase tracking is automatic because the
# counter is a single register that lives across all input chunks
# (the "chunks" only exist in the Rust software view; the HDL just
# sees a continuous stream of strobes).
#
# I/O width
# ---------
# 16-bit signed re/im, matching the existing post-DDC IQ width
# (`maia_hdl.ddc.DDC.re_out`/`im_out`). 6E.4 will widen these on
# the AGC side as needed.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *


class LsmDecimator2(Elaboratable):
    """/2 decimator for the LSM front-end (62.5 kSPS -> 31.25 kSPS).

    Direct port of ``lsm::filters::StreamingDecimator2`` from
    ``p25-httpd/src/lsm/filters.rs``. Drops every other input sample
    on a fixed even-grid phase that survives reset only (the phase
    counter has no external load -- it always restarts at "emit
    next" after reset).

    Inputs (sync domain):
        re_in, im_in: 16-bit signed IQ samples (any DDC output width
            up to 16 bits, sign-extended)
        strobe_in:    sample valid strobe

    Outputs (sync domain):
        re_out, im_out: same width as inputs, registered, valid only
            on the cycle ``strobe_out`` is asserted
        strobe_out:   asserted exactly once per two ``strobe_in``,
            starting on the *first* strobe_in after reset
    """

    def __init__(self, width=16):
        self.width = width

        # Inputs
        self.re_in = Signal(signed(width))
        self.im_in = Signal(signed(width))
        self.strobe_in = Signal()

        # Outputs (registered)
        self.re_out = Signal(signed(width), reset_less=True)
        self.im_out = Signal(signed(width), reset_less=True)
        self.strobe_out = Signal()

    def elaborate(self, platform):
        m = Module()

        # Phase counter: 0 = "emit this input", 1 = "drop this input".
        # Reset to 0 so the first strobe after reset emits the first
        # sample, matching `StreamingDecimator2::new()` (skip = 0).
        phase = Signal(1, init=0)

        # Default: no output strobe.
        m.d.sync += self.strobe_out.eq(0)

        with m.If(self.strobe_in):
            with m.If(phase == 0):
                # Emit this sample. Latch into the registered outputs
                # and assert strobe_out for one cycle.
                m.d.sync += [
                    self.re_out.eq(self.re_in),
                    self.im_out.eq(self.im_in),
                    self.strobe_out.eq(1),
                    phase.eq(1),
                ]
            with m.Else():
                # Drop this sample. Toggle phase, no strobe out, no
                # change to the registered output (downstream sees
                # the previous decimated sample held until the next
                # emit).
                m.d.sync += phase.eq(0)

        return m

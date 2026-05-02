#
# Fishball P25 — per-bin signal energy estimator
#
# Part of the 2026-04-30 channelizer rewrite. See
# `doc/diagnostics/2026-04-30/HDL_CHANNELIZER_PLAN.md` §4.4.
#
# Computes a running |x|^2 IIR low-pass per channel and exposes:
#   * the current energy estimate (PS-readable for debug / threshold
#     tuning)
#   * a one-bit "energy_present" flag, asserted while energy is
#     above a PS-programmable threshold.
#
# The energy_present flag is consumed by the LSM demod's TED + PLL
# update gates (see `lsm_energy_gate.py`, planned M3) so the loops
# don't random-walk on noise during inter-call gaps.
#
# Implementation: a single shared MACC walks the M channels in
# round-robin, updating one channel's accumulator per output epoch
# of the polyphase channelizer. At fs_out per channel = 125 ksps and
# M=64 channels, the per-channel update rate is 125 / 64 ≈ 2 kHz —
# fast enough for the energy gate to react in < 1 ms.
#
# IIR form:
#     energy <- energy + (|x|^2 - energy) >> alpha_log2
#
# alpha_log2 = 10 gives ~1024-sample averaging; at the per-channel
# 125 ksps tap rate that is ~8 ms time constant. The PS can override
# alpha_log2 at runtime if the default mistracks bursty signals.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *


class SignalEnergy(Elaboratable):
    """Per-bin |x|^2 IIR estimator with threshold compare.

    Parameters
    ----------
    M : int
        Number of channels to track. Each gets its own energy
        accumulator and threshold compare.
    width_in : int
        Width of each I and Q sample. Default 16. Magnitude squared
        is computed as ``re*re + im*im`` and accumulated as a
        ``2*width_in + 1``-bit unsigned magnitude (sign bits drop in
        the multiply).
    energy_width : int
        Width of the per-channel energy accumulator. Default 32.
        Trade-off: too narrow truncates the IIR's lowest update
        magnitude; too wide wastes flops. 32 covers the full dynamic
        range of a 16-bit IQ input squared (33 bits) shifted right
        by 1 plus a few headroom bits for the IIR settling.
    alpha_log2 : int
        IIR pole shift. Larger values give slower response. Runtime-
        adjustable via the ``alpha_log2`` input; the constructor
        value is the synthesis default at reset.

    Attributes
    ----------
    bin_re, bin_im : list[Signal(signed(width_in))], in
        Per-channel I and Q values. Sampled when ``strobe_in`` is
        high.
    strobe_in : Signal(), in
        Pulses high once per channelizer output epoch (when all M
        bins are fresh on the parallel inputs).
    threshold : list[Signal(energy_width)], in
        Per-channel "energy_present" threshold. PS writes these via
        the ``traffic_pipe`` register bank.
    alpha_log2 : Signal(4), in
        IIR alpha shift. PS-tunable. 0..15.
    energy : list[Signal(energy_width)], out
        Current energy estimate per channel.
    energy_present : Signal(M), out
        Bit i is high while ``energy[i] > threshold[i]``.
    """

    def __init__(self, *, M=64, width_in=16, energy_width=32,
                 alpha_log2_default=10):
        self.M = M
        self.width_in = width_in
        self.energy_width = energy_width
        self._alpha_default = alpha_log2_default

        # Inputs
        self.bin_re = [Signal(signed(width_in), name=f'bin{i}_re')
                       for i in range(M)]
        self.bin_im = [Signal(signed(width_in), name=f'bin{i}_im')
                       for i in range(M)]
        self.strobe_in = Signal()
        self.threshold = [Signal(energy_width, name=f'thr{i}')
                          for i in range(M)]
        self.alpha_log2 = Signal(4, init=alpha_log2_default)

        # Outputs
        self.energy = [Signal(energy_width, name=f'energy{i}',
                              reset_less=True)
                       for i in range(M)]
        self.energy_present = Signal(M, reset_less=True)

    def elaborate(self, platform):
        m = Module()

        # Latch the M I/Q values on each strobe so the round-robin
        # update sequencer has stable inputs across the M cycles it
        # takes to walk all channels. (Updating channels asynchronously
        # would make the energy estimate sensitive to which cycle the
        # sequencer happened to read each value.)
        latched_re = [Signal(signed(self.width_in), name=f'lr{i}',
                             reset_less=True)
                      for i in range(self.M)]
        latched_im = [Signal(signed(self.width_in), name=f'li{i}',
                             reset_less=True)
                      for i in range(self.M)]
        with m.If(self.strobe_in):
            m.d.sync += [latched_re[i].eq(self.bin_re[i])
                         for i in range(self.M)]
            m.d.sync += [latched_im[i].eq(self.bin_im[i])
                         for i in range(self.M)]

        # ── Round-robin sequencer ─────────────────────────────────
        # On strobe_in, kick off a sweep across all M channels. At one
        # update per cycle we finish in M cycles, well before the next
        # strobe (period ≥ M cycles in steady state).
        active = Signal()
        idx = Signal(range(self.M))
        with m.If(self.strobe_in):
            m.d.sync += [active.eq(1), idx.eq(0)]
        with m.If(active):
            with m.If(idx == self.M - 1):
                m.d.sync += active.eq(0)
            with m.Else():
                m.d.sync += idx.eq(idx + 1)

        # MUX inputs based on idx — yields one (re, im) pair per cycle.
        sel_re = Signal(signed(self.width_in))
        sel_im = Signal(signed(self.width_in))
        with m.Switch(idx):
            for i in range(self.M):
                with m.Case(i):
                    m.d.comb += [
                        sel_re.eq(latched_re[i]),
                        sel_im.eq(latched_im[i]),
                    ]

        # |x|^2 computed in two registered stages so we don't pay a
        # multiply + adder in one cycle. Pipeline depth is 2 from
        # `idx`/`sel_re` to `mag_sq`. Track `idx` and `active`
        # through matching delay stages so the read-modify-write
        # of self.energy hits the correct slot.
        sq_re = Signal(unsigned(2 * self.width_in), reset_less=True)
        sq_im = Signal(unsigned(2 * self.width_in), reset_less=True)
        mag_sq = Signal(unsigned(2 * self.width_in + 1), reset_less=True)
        idx_q = [Signal(range(self.M), reset_less=True) for _ in range(2)]
        active_q = Signal(2, reset_less=True)
        m.d.sync += [
            sq_re.eq(sel_re * sel_re),
            sq_im.eq(sel_im * sel_im),
            mag_sq.eq(sq_re + sq_im),
            idx_q[0].eq(idx),
            idx_q[1].eq(idx_q[0]),
            active_q.eq(Cat(active, active_q[:-1])),
        ]

        # IIR update — read current energy, compute (|x|^2 - energy)
        # >> alpha, write back.
        # mag_sq lands at idx_q[1] / active_q[1] (both 2-cycle delayed).
        sel_energy = Signal(self.energy_width)
        with m.Switch(idx_q[1]):
            for i in range(self.M):
                with m.Case(i):
                    m.d.comb += sel_energy.eq(self.energy[i])

        # Sign-extend mag_sq (always non-negative) up to the energy
        # width, subtract, arith-right by alpha. Compute as signed so
        # the >> handles negative deltas correctly.
        diff = Signal(signed(self.energy_width + 1))
        m.d.comb += diff.eq(
            Cat(mag_sq, Const(0, self.energy_width + 1 - len(mag_sq)))
            .as_signed() - sel_energy.as_signed())
        delta = Signal(signed(self.energy_width + 1))
        m.d.comb += delta.eq(diff >> self.alpha_log2)
        new_energy = Signal(self.energy_width)
        new_energy_signed = Signal(signed(self.energy_width + 2))
        m.d.comb += new_energy_signed.eq(
            sel_energy.as_signed() + delta)
        # Clamp to non-negative; the IIR can't go negative in steady
        # state but transient subtraction underflow could.
        with m.If(new_energy_signed < 0):
            m.d.comb += new_energy.eq(0)
        with m.Else():
            m.d.comb += new_energy.eq(new_energy_signed)

        # Write back into the addressed energy slot.
        with m.If(active_q[1]):
            with m.Switch(idx_q[1]):
                for i in range(self.M):
                    with m.Case(i):
                        m.d.sync += self.energy[i].eq(new_energy)

        # Threshold compare — combinational per-bin.
        for i in range(self.M):
            m.d.sync += self.energy_present[i].eq(
                self.energy[i] > self.threshold[i])

        return m

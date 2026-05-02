#
# Fishball P25 — N-of-1 target selector
#
# Part of the 2026-04-30 channelizer rewrite (M2A).
# See doc/diagnostics/2026-04-30/HDL_CHANNELIZER_PLAN.md §4.3.
#
# Routes one of N PerTargetDDC outputs to a single downstream
# consumer (the K=1 LSM demod in M2B). PS programs `sel` to choose
# which target to follow. The mux is a single register write — no
# DDC reconfigure, no FIR flush, no LSM reset. That is the entire
# point of the rewrite vs the current per-grant-retune chain.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *


class ChannelMux(Elaboratable):
    """1-of-N IQ selector for the LSM demod input.

    Parameters
    ----------
    N : int
        Number of input target streams.
    width : int
        Width of each I and Q sample.

    Attributes
    ----------
    target_re_in : list[Signal(signed(width))], in
        Per-target real input. Length N.
    target_im_in : list[Signal(signed(width))], in
        Per-target imaginary input. Length N.
    target_strobe_in : list[Signal()], in
        Per-target sample strobe. Length N.
    sel : Signal(ceil_log2(N)), in
        Selector. PS-programmable; one-cycle switch on write.
    re_out, im_out : Signal(signed(width)), out
        Selected output IQ.
    strobe_out : Signal(), out
        Selected output strobe.
    """

    def __init__(self, *, N, width=16):
        if N < 1:
            raise ValueError(f'N must be >= 1, got {N}')
        self.N = N
        self.width = width
        sel_bits = max(1, (N - 1).bit_length())
        self.sel_bits = sel_bits

        self.target_re_in = [
            Signal(signed(width), name=f'tg{i}_re_in') for i in range(N)]
        self.target_im_in = [
            Signal(signed(width), name=f'tg{i}_im_in') for i in range(N)]
        self.target_strobe_in = [
            Signal(name=f'tg{i}_strobe_in') for i in range(N)]
        self.sel = Signal(sel_bits)

        self.re_out = Signal(signed(width), reset_less=True)
        self.im_out = Signal(signed(width), reset_less=True)
        self.strobe_out = Signal(reset_less=True)

    def elaborate(self, platform):
        m = Module()

        # Combinational mux feeding registered output. The single-
        # cycle output register absorbs glitches when `sel` changes
        # mid-cycle, presenting a clean transition to downstream.
        re_mux = Signal(signed(self.width))
        im_mux = Signal(signed(self.width))
        strobe_mux = Signal()

        with m.Switch(self.sel):
            for i in range(self.N):
                with m.Case(i):
                    m.d.comb += [
                        re_mux.eq(self.target_re_in[i]),
                        im_mux.eq(self.target_im_in[i]),
                        strobe_mux.eq(self.target_strobe_in[i]),
                    ]

        m.d.sync += [
            self.re_out.eq(re_mux),
            self.im_out.eq(im_mux),
            self.strobe_out.eq(strobe_mux),
        ]

        return m

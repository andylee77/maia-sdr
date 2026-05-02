#
# Fishball P25 — traffic-side polyphase pipeline (M2A wrapper)
#
# Part of the 2026-04-30 channelizer rewrite.
# See doc/diagnostics/2026-04-30/HDL_CHANNELIZER_PLAN.md §4.6.
#
# Wraps:
#
#   M_bins polyphase bin outputs (from PolyphaseChannelizer, instantiated
#   externally in p25_top.py)
#       │
#       ▼ M-to-N selection (per-target `target_bin` reg)
#   ┌──────────────────────────────────────────────────────────────┐
#   │  N × PerTargetDDC  (mixer + decim FIR /D)                    │
#   │  Each PS-programmable: target_bin, nco_freq, enable          │
#   └──────────────────────────────────────────────────────────────┘
#       │ N decimated IQ streams @ fs_in/(M*D)
#       ├──────────────────────► SignalEnergy (per-target energy)
#       │
#       ▼  ChannelMux (1-of-N, selected by `target_sel`)
#   re_out / im_out / strobe_out  → reserved for M2B LSM input
#
# M2A scope: everything except the LSM hookup. The mux output is
# exported on the module's I/O so p25_top can route it to a debug
# IQ DMA today and the LSM in M2B.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *

from .per_target_ddc import PerTargetDDC
from .channel_mux import ChannelMux
from .signal_energy import SignalEnergy


class TrafficPipeline(Elaboratable):
    """Per-target DDC pool + mux + per-target signal_energy.

    Parameters
    ----------
    decim_fir_coeffs : list[int]
        Pre-quantized FIR coefficients for each PerTargetDDC.
    M_bins : int
        Number of upstream polyphase bins. Default 64.
    n_targets : int
        Number of allocated targets (PerTargetDDC instances).
    decimation : int
        Decimation factor inside each PerTargetDDC. Default 4.
    width : int
        Width of IQ samples through the pipeline. Default 16.
    nco_width : int
        Width of each PerTargetDDC's NCO. Default 24.
    domain_3x : str
        Name of the 3x clock domain (Mixer's cmult).

    Attributes
    ----------
    bin_re_in : list[Signal(signed(width))], in
        Per-bin real input from the upstream channelizer. Length M_bins.
    bin_im_in : list[Signal(signed(width))], in
        Per-bin imaginary input. Length M_bins.
    bin_strobe_in : Signal(), in
        Pulses high once per channelizer epoch (all bins fresh).
    common_edge_3x : Signal(), in
        Common-edge for the embedded Mixers' cmult.
    target_bin : list[Signal(ceil_log2(M_bins))], in
        Per-target bin selector. PS-programmable.
    target_nco_freq : list[Signal(signed(nco_width))], in
        Per-target NCO frequency. PS-programmable.
    target_enable : Signal(n_targets), in
        Per-target enable mask. PS-programmable.
    target_sel : Signal(ceil_log2(n_targets)), in
        Selects which target feeds the LSM output. PS-programmable.
    energy_alpha_log2 : Signal(4), in
        IIR alpha for the per-target signal_energy (default 8).
    target_energy : list[Signal(32)], out
        Per-target energy estimates (PS-readable for diag).
    target_energy_present : Signal(n_targets), out
        Per-target above-threshold flags.
    target_threshold : list[Signal(32)], in
        Per-target signal_energy threshold (PS-programmable).
    re_out, im_out : Signal(signed(width)), out
        Selected target's IQ stream. Reserved for M2B LSM input.
    strobe_out : Signal(), out
        Selected target's sample strobe.
    """

    def __init__(self, *, decim_fir_coeffs,
                 M_bins=64, n_targets=8, decimation=4,
                 width=16, nco_width=24,
                 domain_3x='clk3x'):
        if n_targets < 1:
            raise ValueError(f'n_targets must be >= 1, got {n_targets}')
        if M_bins & (M_bins - 1):
            raise ValueError(f'M_bins must be power of 2, got {M_bins}')
        self.M_bins = M_bins
        self.N = n_targets
        self.decim = decimation
        self.width = width
        self.nco_width = nco_width
        self._3x = domain_3x

        self.bin_sel_bits = max(1, (M_bins - 1).bit_length())
        self.target_sel_bits = max(1, (n_targets - 1).bit_length())

        # Sub-components.
        self.per_target_ddc = [
            PerTargetDDC(
                fir_coeffs=decim_fir_coeffs, decimation=decimation,
                width_in=width, width_out=width,
                nco_width=nco_width, domain_3x=domain_3x)
            for _ in range(n_targets)]
        self.mux = ChannelMux(N=n_targets, width=width)
        self.target_energy_mod = SignalEnergy(
            M=n_targets, width_in=width, energy_width=32,
            alpha_log2_default=8)

        # I/O — upstream from channelizer.
        self.bin_re_in = [
            Signal(signed(width), name=f'bin{i}_re_in')
            for i in range(M_bins)]
        self.bin_im_in = [
            Signal(signed(width), name=f'bin{i}_im_in')
            for i in range(M_bins)]
        self.bin_strobe_in = Signal()
        self.common_edge_3x = Signal()

        # I/O — PS programmable.
        self.target_bin = [
            Signal(self.bin_sel_bits, name=f'tg{i}_bin')
            for i in range(n_targets)]
        self.target_nco_freq = [
            Signal(signed(nco_width), name=f'tg{i}_nco')
            for i in range(n_targets)]
        self.target_enable = Signal(n_targets, init=0)
        self.target_sel = Signal(self.target_sel_bits)
        self.energy_alpha_log2 = Signal(4, init=8)
        self.target_threshold = [
            Signal(32, name=f'tg{i}_thr') for i in range(n_targets)]

        # I/O — outputs.
        self.target_energy = [
            Signal(32, name=f'tg{i}_energy') for i in range(n_targets)]
        self.target_energy_present = Signal(n_targets)
        self.re_out = Signal(signed(width))
        self.im_out = Signal(signed(width))
        self.strobe_out = Signal()

    def elaborate(self, platform):
        m = Module()

        for i, ddc in enumerate(self.per_target_ddc):
            m.submodules[f'per_target_ddc{i}'] = ddc
        m.submodules.mux = self.mux
        m.submodules.target_energy_mod = self.target_energy_mod

        # ── M-to-N bin selection (registered) ─────────────────────
        # For each target i, mux self.bin_re_in[target_bin[i]] into
        # the corresponding PerTargetDDC's input. The selected
        # value is REGISTERED before feeding the DDC — without this,
        # the 64-to-1 mux's 6 LUT levels (~2.4 ns) chain directly
        # into the per_target_ddc's mixer & sample-buffer write
        # path and blow timing closure on Z7020 -1 speed grade.
        # The 1-cycle latency is harmless: the channelizer already
        # has many cycles of pipeline ahead of this point.
        bin_strobe_q = Signal(reset_less=True)
        m.d.sync += bin_strobe_q.eq(self.bin_strobe_in)
        for i in range(self.N):
            sel_re = Signal(signed(self.width), name=f'tg{i}_sel_re')
            sel_im = Signal(signed(self.width), name=f'tg{i}_sel_im')
            with m.Switch(self.target_bin[i]):
                for b in range(self.M_bins):
                    with m.Case(b):
                        m.d.comb += [
                            sel_re.eq(self.bin_re_in[b]),
                            sel_im.eq(self.bin_im_in[b]),
                        ]
            sel_re_q = Signal(signed(self.width),
                              name=f'tg{i}_sel_re_q', reset_less=True)
            sel_im_q = Signal(signed(self.width),
                              name=f'tg{i}_sel_im_q', reset_less=True)
            m.d.sync += [
                sel_re_q.eq(sel_re),
                sel_im_q.eq(sel_im),
            ]
            ddc = self.per_target_ddc[i]
            m.d.comb += [
                ddc.re_in.eq(sel_re_q),
                ddc.im_in.eq(sel_im_q),
                ddc.strobe_in.eq(bin_strobe_q),
                ddc.enable.eq(self.target_enable[i]),
                ddc.nco_freq.eq(self.target_nco_freq[i]),
                ddc.common_edge_3x.eq(self.common_edge_3x),
            ]

        # ── Mux per-target outputs to single LSM-bound stream ─────
        for i in range(self.N):
            ddc = self.per_target_ddc[i]
            m.d.comb += [
                self.mux.target_re_in[i].eq(ddc.re_out),
                self.mux.target_im_in[i].eq(ddc.im_out),
                self.mux.target_strobe_in[i].eq(ddc.strobe_out),
            ]
        m.d.comb += [
            self.mux.sel.eq(self.target_sel),
            self.re_out.eq(self.mux.re_out),
            self.im_out.eq(self.mux.im_out),
            self.strobe_out.eq(self.mux.strobe_out),
        ]

        # ── Per-target signal energy ──────────────────────────────
        # Strobe whenever ANY target ddc produces an output. The
        # signal_energy module's round-robin sweeps all N channels
        # on each strobe; if a particular target hasn't produced
        # output (no input or disabled), its bin_re/im stays at
        # whatever the per_target_ddc held last.
        any_output_strobe = Signal()
        m.d.comb += any_output_strobe.eq(
            Cat(self.per_target_ddc[i].strobe_out
                for i in range(self.N)).any())
        m.d.comb += [
            self.target_energy_mod.strobe_in.eq(any_output_strobe),
            self.target_energy_mod.alpha_log2.eq(self.energy_alpha_log2),
        ]
        for i in range(self.N):
            ddc = self.per_target_ddc[i]
            m.d.comb += [
                self.target_energy_mod.bin_re[i].eq(ddc.re_out),
                self.target_energy_mod.bin_im[i].eq(ddc.im_out),
                self.target_energy_mod.threshold[i].eq(
                    self.target_threshold[i]),
                self.target_energy[i].eq(
                    self.target_energy_mod.energy[i]),
            ]
        m.d.comb += self.target_energy_present.eq(
            self.target_energy_mod.energy_present)

        return m

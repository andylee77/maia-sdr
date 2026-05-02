#
# Fishball P25 — per-target fine-tune DDC
#
# Part of the 2026-04-30 channelizer rewrite (M2A).
# See doc/diagnostics/2026-04-30/HDL_CHANNELIZER_PLAN.md §4.2.
#
# One PerTargetDDC instance is dedicated to a single target P25 channel.
# It consumes one polyphase bin's complex IQ at fs_in (typically
# 125 ksps for an M=64 channelizer over 8 MSPS wideband), shifts the
# residual offset (channel_center_hz - bin_center_hz, range ±62.5 kHz)
# to DC via an NCO mixer, then anti-alias filters and decimates by D
# (typically 4 → 31.25 ksps) to match the LSM input rate.
#
# Pipeline:
#
#   bin IQ (fs_in) ──► Mixer (NCO) ──► FIR /D ──► target IQ (fs_in/D)
#
# Coefficients for the decimation FIR are compile-time constants
# (not runtime-loadable). This matches the M2A scope: targets are
# allocated by the lifecycle layer at PS startup, frequencies
# updated dynamically via `nco_freq`, but the anti-alias FIR is
# fixed.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *
from amaranth.lib.memory import Memory

import numpy as np

from maia_hdl.mixer import Mixer


def design_decim_fir(n_taps: int, decimation: int,
                     fs_in_hz: float, beta: float = 6.0,
                     coeff_width: int = 16) -> list[int]:
    """Design a Kaiser-windowed lowpass for the decimation FIR.

    Cutoff is set at 0.4 × (fs_in / decimation / 2) — well inside the
    decimated Nyquist with margin for the transition band. Returns
    quantized signed integer coefficients of length n_taps.
    """
    import scipy.signal
    fs_out = fs_in_hz / decimation
    cutoff_hz = 0.4 * fs_out
    h = scipy.signal.firwin(
        n_taps, cutoff=cutoff_hz, fs=fs_in_hz,
        window=('kaiser', beta), pass_zero='lowpass')
    h /= np.max(np.abs(np.fft.fft(h, 8 * n_taps)))
    scale = (1 << (coeff_width - 1)) - 1
    q = np.round(h / np.max(np.abs(h)) * scale).astype(int)
    return [int(c) for c in q]


class PerTargetDDC(Elaboratable):
    """Mixer + decimating FIR for one allocated P25 channel.

    Parameters
    ----------
    fir_coeffs : list[int]
        Pre-quantized signed FIR coefficients. Length must be a
        multiple of `decimation`. Use `design_decim_fir()` to
        produce a default set.
    decimation : int
        Decimation factor.  Default 4 (125 ksps → 31.25 ksps).
    width_in : int
        Width of input IQ samples.  Default 16.
    width_out : int
        Width of output IQ samples.  Default 16.
    nco_width : int
        Width of the NCO frequency word.  Default 24 (~7 µHz at
        125 ksps).
    coeff_width : int
        Width of stored FIR coefficients.  Default 16.
    macc_trunc : int
        Bits to drop from the MACC accumulator before saturating to
        `width_out`. Default chosen so the FIR's nominal unity-gain
        peak coefficient (at the saturation edge) maps back to
        full-scale output.
    domain_3x : str
        Name of the 3x clock domain — Mixer's Cmult3x runs in this
        domain.

    Attributes
    ----------
    re_in, im_in : Signal(signed(width_in)), in
        Input IQ from the upstream channelizer bin.
    strobe_in : Signal(), in
        Pulses high once per input sample.
    enable : Signal(), in (init=1)
        Master gate. Low → no strobes processed, no work performed.
    nco_freq : Signal(signed(nco_width)), in
        Mixer NCO frequency, normalised so 2**nco_width corresponds
        to fs_in. PS-programmable.
    common_edge_3x : Signal(), in
        Common-edge for the Mixer's 3x cmult.
    re_out, im_out : Signal(signed(width_out)), out
        Decimated output IQ.
    strobe_out : Signal(), out
        Pulses high once per output sample.
    """

    def __init__(self, *, fir_coeffs, decimation=4,
                 width_in=16, width_out=16,
                 nco_width=24, coeff_width=16,
                 macc_trunc=None, domain_3x='clk3x'):
        if len(fir_coeffs) % decimation != 0:
            raise ValueError(
                f'len(fir_coeffs)={len(fir_coeffs)} must be multiple of '
                f'decimation={decimation}')
        self.fir_coeffs = list(fir_coeffs)
        self.n_taps = len(fir_coeffs)
        self.decim = decimation
        self.width_in = width_in
        self.width_out = width_out
        self.nco_width = nco_width
        self.coeff_width = coeff_width
        # Default truncation: keep peak |H(0)|=1 → DC tone passes
        # at unity gain. Match the convention from the prototype
        # design where peak coefficient is scale-1.
        self.macc_trunc = (
            macc_trunc if macc_trunc is not None else coeff_width - 1)
        self._3x = domain_3x

        # Mixer sub-component (uses 3x clock domain).
        self.mixer = Mixer(domain_3x, width=width_in, nco_width=nco_width)

        # Top-level I/O
        self.re_in = Signal(signed(width_in))
        self.im_in = Signal(signed(width_in))
        self.strobe_in = Signal()
        self.enable = Signal(init=1)
        self.nco_freq = Signal(signed(nco_width))
        self.common_edge_3x = Signal()

        self.re_out = Signal(signed(width_out), reset_less=True)
        self.im_out = Signal(signed(width_out), reset_less=True)
        self.strobe_out = Signal()

    def model(self, re_in, im_in, freq_norm):
        """Behavioural model for sims.

        `freq_norm` is the NCO frequency in cycles/sample (NOT the
        raw nco_freq register value). Bit-approximate: integer
        arithmetic matches HDL except for round-off in the mixer
        complex multiply, which is bounded ±1 LSB.
        """
        n = len(re_in)
        assert len(im_in) == n
        # Mix to baseband.
        phase = np.cumsum(np.full(n, freq_norm))
        c = np.cos(2 * np.pi * phase)
        s = np.sin(2 * np.pi * phase)
        re_mix = np.round(np.asarray(re_in) * c
                          + np.asarray(im_in) * s).astype(int)
        im_mix = np.round(-np.asarray(re_in) * s
                          + np.asarray(im_in) * c).astype(int)
        # FIR + decimate.
        h = np.asarray(self.fir_coeffs, dtype=np.int64)
        n_out = n // self.decim
        re_out = np.zeros(n_out, dtype=np.int64)
        im_out = np.zeros(n_out, dtype=np.int64)
        round_bias = (1 << (self.macc_trunc - 1)
                      if self.macc_trunc > 0 else 0)
        for i in range(n_out):
            base = i * self.decim
            # Window of the most-recent N_taps samples ending at
            # base+decim-1 (not yet wrapped around).
            end = base + self.decim
            start = end - self.n_taps
            if start < 0:
                continue
            re_win = re_mix[start:end]
            im_win = im_mix[start:end]
            # h is applied newest-first.
            re_out[i] = ((np.sum(re_win[::-1] * h) + round_bias)
                         >> self.macc_trunc)
            im_out[i] = ((np.sum(im_win[::-1] * h) + round_bias)
                         >> self.macc_trunc)
        return re_out, im_out

    def elaborate(self, platform):
        m = Module()
        m.submodules.mixer = self.mixer

        # Strobe gating: when enable=0 the mixer ignores inputs and
        # the FIR sequencer never starts. Mixer cell stays idle.
        gated_strobe = Signal()
        m.d.comb += gated_strobe.eq(self.strobe_in & self.enable)

        m.d.comb += [
            self.mixer.clken.eq(gated_strobe),
            self.mixer.frequency.eq(self.nco_freq),
            self.mixer.re_in.eq(self.re_in),
            self.mixer.im_in.eq(self.im_in),
            self.mixer.common_edge.eq(self.common_edge_3x),
        ]

        # Pipeline the strobe to align with the mixer's output
        # latency.  A single registered shift handles arbitrary
        # mixer.delay values.
        mixer_delay = self.mixer.delay
        if mixer_delay > 0:
            mix_strobe_q = Signal(mixer_delay, reset_less=True)
            m.d.sync += mix_strobe_q.eq(
                Cat(gated_strobe, mix_strobe_q[:-1]))
            mixed_strobe = mix_strobe_q[-1]
        else:
            mixed_strobe = gated_strobe

        # ── FIR sample buffer ─────────────────────────────────────
        # Holds the most recent n_taps complex samples in a circular
        # buffer.  Each entry packs re and im in a single 2*width word.
        sb_w = 2 * self.width_in
        sb_aw = (self.n_taps - 1).bit_length()
        sb_depth = 1 << sb_aw
        m.submodules.sample_buf = sample_buf = Memory(
            shape=sb_w, depth=sb_depth, init=[],
            attrs={'ram_style': 'block'})
        sb_rdport = sample_buf.read_port()
        sb_wrport = sample_buf.write_port()

        write_ptr = Signal(sb_aw)

        m.d.comb += [
            sb_wrport.en.eq(mixed_strobe),
            sb_wrport.addr.eq(write_ptr),
            sb_wrport.data.eq(
                Cat(self.mixer.re_out, self.mixer.im_out)),
        ]
        with m.If(mixed_strobe):
            m.d.sync += write_ptr.eq(
                Mux(write_ptr == self.n_taps - 1, 0, write_ptr + 1))

        # ── Coefficient ROM ───────────────────────────────────────
        coeff_mask = (1 << self.coeff_width) - 1
        coeff_init = [int(c) & coeff_mask for c in self.fir_coeffs]
        coeff_aw = (self.n_taps - 1).bit_length()
        m.submodules.coeff_rom = coeff_rom = Memory(
            shape=self.coeff_width, depth=1 << coeff_aw,
            init=coeff_init,
            attrs={'ram_style': 'distributed'})
        coeff_rdport = coeff_rom.read_port()

        # ── Decimation + FIR sequencer ────────────────────────────
        # Counts input strobes; on every D-th strobe, kicks off a
        # FIR pass that walks n_taps cycles.
        decim_count = Signal(range(self.decim))
        with m.If(mixed_strobe):
            m.d.sync += decim_count.eq(
                Mux(decim_count == self.decim - 1, 0, decim_count + 1))

        # Latch the anchor write_ptr at the moment we kick off so
        # mid-FIR-pass writes don't move the window beneath us.
        active = Signal()
        anchor_ptr = Signal(sb_aw)
        tap_idx = Signal(coeff_aw)
        first_tap = Signal()

        # Trigger a new FIR pass when we receive the (D-1)th input
        # strobe of a group AND we're not already busy. We tolerate
        # being slightly busy because the per-pass cycle count
        # (n_taps + small) is comfortably below the inter-strobe
        # period (decim × cycles-per-input >> n_taps for our rates).
        kick_off = Signal()
        m.d.comb += kick_off.eq(
            mixed_strobe & (decim_count == self.decim - 1) & ~active)

        with m.If(kick_off):
            m.d.sync += [
                active.eq(1),
                tap_idx.eq(0),
                first_tap.eq(1),
                # write_ptr advances on this same cycle; capture the
                # post-write value as the anchor.
                anchor_ptr.eq(
                    Mux(write_ptr == self.n_taps - 1, 0, write_ptr + 1)),
            ]
        with m.Elif(active):
            m.d.sync += first_tap.eq(0)
            with m.If(tap_idx == self.n_taps - 1):
                m.d.sync += [
                    active.eq(0),
                    tap_idx.eq(0),
                ]
            with m.Else():
                m.d.sync += tap_idx.eq(tap_idx + 1)

        # FIR read addresses: walk back from anchor_ptr-1 (newest)
        # through anchor_ptr-n_taps (oldest), with circular wrap.
        def sb_read_addr(tap):
            offset = 1 + tap
            return Mux(
                anchor_ptr >= offset,
                anchor_ptr - offset,
                anchor_ptr + (self.n_taps - offset))
        m.d.comb += [
            sb_rdport.en.eq(1),
            sb_rdport.addr.eq(sb_read_addr(tap_idx)),
            coeff_rdport.en.eq(1),
            coeff_rdport.addr.eq(tap_idx),
        ]

        # Pipeline staging: sample BRAM has 1-cycle read latency,
        # then 1 cycle for the multiply, then accumulate. Match
        # active/first_tap/last_tap with delay 1 to align with
        # the multiply.
        valid_q = Signal(2, reset_less=True)
        first_q = Signal(2, reset_less=True)
        last_q = Signal(2, reset_less=True)
        last_tap_pre = Signal()
        m.d.comb += last_tap_pre.eq(active & (tap_idx == self.n_taps - 1))
        m.d.sync += [
            valid_q.eq(Cat(active, valid_q[:-1])),
            first_q.eq(Cat(first_tap, first_q[:-1])),
            last_q.eq(Cat(last_tap_pre, last_q[:-1])),
        ]

        # MACC.
        sample_re = sb_rdport.data[:self.width_in].as_signed()
        sample_im = sb_rdport.data[self.width_in:].as_signed()
        coeff = coeff_rdport.data.as_signed()

        prod_w = self.width_in + self.coeff_width
        acc_w = prod_w + (self.n_taps).bit_length() + 2
        prod_re = Signal(signed(prod_w), reset_less=True)
        prod_im = Signal(signed(prod_w), reset_less=True)
        acc_re = Signal(signed(acc_w), reset_less=True)
        acc_im = Signal(signed(acc_w), reset_less=True)
        round_bias = (1 << (self.macc_trunc - 1)
                      if self.macc_trunc > 0 else 0)

        with m.If(valid_q[0]):
            m.d.sync += [
                prod_re.eq(sample_re * coeff),
                prod_im.eq(sample_im * coeff),
            ]
        with m.If(valid_q[1]):
            with m.If(first_q[1]):
                m.d.sync += [
                    acc_re.eq(prod_re + round_bias),
                    acc_im.eq(prod_im + round_bias),
                ]
            with m.Else():
                m.d.sync += [
                    acc_re.eq(acc_re + prod_re),
                    acc_im.eq(acc_im + prod_im),
                ]

        # Output saturation + latch.
        max_v = (1 << (self.width_out - 1)) - 1
        min_v = -(1 << (self.width_out - 1))
        result_done = Signal(reset_less=True)
        m.d.sync += result_done.eq(last_q[1] & valid_q[1])

        with m.If(result_done):
            re_shift = (acc_re >> self.macc_trunc).as_signed()
            im_shift = (acc_im >> self.macc_trunc).as_signed()
            m.d.sync += [
                self.re_out.eq(
                    Mux(re_shift > max_v, max_v,
                        Mux(re_shift < min_v, min_v, re_shift))),
                self.im_out.eq(
                    Mux(im_shift > max_v, max_v,
                        Mux(im_shift < min_v, min_v, im_shift))),
            ]
        m.d.sync += self.strobe_out.eq(result_done)

        return m

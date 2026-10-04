#
# Fishball P25 — polyphase analysis filter bank
#
# This module implements an M-branch polyphase analysis filter bank
# for the wideband AD9361 IQ stream. The output is M complex baseband
# channels, each at fs_in / M sample rate. It is critically sampled.
# Not in the radio core: 079 step 3b's bank (2x oversampled, 25 kHz
# bins) would reuse its commutator, circular buffer and shared
# multiplier.
#
# Mathematical model: a length-(M*K) prototype lowpass FIR is
# decomposed into M sub-filters where sub-filter i has taps
#     h_i[k] = h[i + k*M]   for  k = 0..K-1
# The input is fed to the M sub-filters via a commutator (sample n
# goes into branch (n mod M)). After M consecutive input samples
# arrive, all M sub-filters have a fresh tap and the M sub-filter
# outputs are processed by an M-point DFT to produce the M
# channelized outputs.
#
# Implementation notes:
#
#   * The M=64 R22SDF FFT auto-selects distributed storage at every
#     stage (max R2SDF order = 6 < 9). Zero BRAM cost for the FFT
#     itself.
#   * The polyphase sample buffer is one shared BRAM of M*K = 384
#     complex words. Read/write uses circular addressing so we never
#     copy data between branches.
#   * The polyphase taps are evaluated by a single shared MACC pair
#     time-multiplexed across the M*K = 384 multiply-accumulate ops
#     per output epoch. At fs_in=8 MSPS and a sync clock of 62.5 MHz
#     we have 500 cycles per output epoch; 384 ops + ~64 cycles for
#     FFT input plus pipeline drain leaves comfortable margin.
#   * Outputs are exposed both as a serial stream (re_out, im_out,
#     bin_index, strobe_out) and as M parallel registers latched on
#     each output epoch — downstream modules pick whichever matches
#     their access pattern.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *
from amaranth.lib.memory import Memory

import numpy as np

from maia_hdl.fft import FFT


class PolyphaseChannelizer(Elaboratable):
    """M-branch polyphase analysis filter bank.

    Parameters
    ----------
    coeffs : list[int]
        Prototype lowpass FIR coefficients, length M*K. Generated
        offline by ``tools/polyphase_proto_design.py`` and stored in
        ``scanner-hdl/radio_core/polyphase_proto_coeffs.py``.
    M : int
        Number of polyphase branches. Must be a power of 2 with
        ``log2(M) % 2 == 0`` so the R22 FFT cleanly maps. Default 64.
    K : int
        Taps per polyphase branch. Total prototype length is M*K.
        Default 6.
    width_in : int
        Width of each I and Q sample. Default 16 (matches the
        ``rxiq_cdc`` tap point in `p25_top.py`).
    width_out : int
        Width of each I and Q output sample. Default 16.
    coeff_width : int
        Width of stored coefficients (signed). Default 16.
    macc_trunc : int
        Bits to drop from the MACC output. Default chosen so the
        FIR output occupies width_out bits given the prototype
        coefficient scaling.
    domain_2x : str
        Name of the 2x clock domain, used by the FFT window.
    domain_3x : str
        Name of the 3x clock domain, used by the FFT 3x complex
        multiplier.

    Attributes
    ----------
    strobe_in : Signal(), in
        Pulses high for one cycle when ``re_in`` / ``im_in`` carry a
        valid input sample. Acts as the FIR clken.
    re_in, im_in : Signal(signed(width_in)), in
        Input IQ stream.
    common_edge_2x, common_edge_3x : Signal(), in
        Common-edge signals for the FFT (see :class:`FFT`).
    bin_re, bin_im : list[Signal(signed(width_out))], out
        Per-bin parallel outputs. Updated atomically once per output
        epoch (every M strobe_in pulses, plus the FFT pipeline delay).
    bin_strobe : Signal(), out
        Pulses high for one cycle when ``bin_re`` / ``bin_im`` are
        freshly latched.
    re_out, im_out : Signal(signed(width_out)), out
        Serial bin output stream from the FFT (the same data that
        gets fanned into ``bin_re`` / ``bin_im``).
    bin_index : Signal(log2(M)), out
        Index of the bin currently driven on ``re_out`` / ``im_out``.
    strobe_out : Signal(), out
        Pulses high for one cycle for each serial output sample.
    """

    def __init__(self, coeffs, *,
                 M=64, K=6, width_in=16, width_out=16,
                 coeff_width=16, macc_trunc=None,
                 domain_2x='clk2x', domain_3x='clk3x'):
        if M & (M - 1):
            raise ValueError(f'M must be a power of 2, got {M}')
        order_log2 = (M).bit_length() - 1
        if order_log2 % 2 != 0:
            raise ValueError(
                f'R22 FFT requires even order_log2, got {order_log2} (M={M})')
        if len(coeffs) != M * K:
            raise ValueError(
                f'coeffs must be length M*K = {M * K}, got {len(coeffs)}')

        self.M = M
        self.K = K
        self.order_log2 = order_log2
        self.width_in = width_in
        self.width_out = width_out
        self.coeff_width = coeff_width
        self.coeffs = list(coeffs)
        # Default MACC truncation: prototype coefficients normalized so
        # peak |H(f)| = 1.0. The polyphase decomposition splits that
        # gain across M branches; per-branch peak gain ~ 1/M. The
        # MACC accumulates K products of width width_in*coeff_width
        # bits with sign-bit growth ceil(log2(K)) <= log2(K)+1.
        # Truncate by (coeff_width - 1) so output stays at width_in
        # without overflow.
        self.macc_trunc = (
            macc_trunc if macc_trunc is not None else coeff_width - 1)
        self._domain_2x = domain_2x
        self._domain_3x = domain_3x

        # Inputs
        self.strobe_in = Signal()
        self.re_in = Signal(signed(width_in))
        self.im_in = Signal(signed(width_in))
        self.common_edge_2x = Signal()
        self.common_edge_3x = Signal()

        # Outputs (parallel)
        self.bin_re = [Signal(signed(width_out), name=f'bin{i}_re',
                              reset_less=True)
                       for i in range(M)]
        self.bin_im = [Signal(signed(width_out), name=f'bin{i}_im',
                              reset_less=True)
                       for i in range(M)]
        self.bin_strobe = Signal()

        # Outputs (serial — same data, before parallel demux)
        self.re_out = Signal(signed(width_out))
        self.im_out = Signal(signed(width_out))
        self.bin_index = Signal(order_log2)
        self.strobe_out = Signal()

    # ──────────────────────────────────────────────────────────────
    # Behavioural model — bit-approximate. Used by tests to compare
    # against the HDL simulation. Computation matches the HDL exactly
    # except for MACC fixed-point rounding (within ±1 LSB).
    # ──────────────────────────────────────────────────────────────
    def model(self, re_in, im_in):
        n = len(re_in)
        assert len(im_in) == n
        re_in = np.asarray(re_in, dtype=np.int64)
        im_in = np.asarray(im_in, dtype=np.int64)
        coeffs = np.asarray(self.coeffs, dtype=np.int64)

        n_epochs = n // self.M
        bins_re = np.zeros((n_epochs, self.M), dtype=np.int64)
        bins_im = np.zeros((n_epochs, self.M), dtype=np.int64)

        for ep in range(n_epochs):
            # The samples available at branch i for epoch ep are
            #   x_branch_i[k] = re_in[(ep+1)*M - 1 - i - k*M]  k=0..K-1
            # i.e. newest goes into branch 0, then 1, ... per the
            # commutator convention.
            branch_outs_re = np.zeros(self.M, dtype=np.int64)
            branch_outs_im = np.zeros(self.M, dtype=np.int64)
            for i in range(self.M):
                acc_re = 0
                acc_im = 0
                for k in range(self.K):
                    idx = (ep + 1) * self.M - 1 - i - k * self.M
                    if 0 <= idx < n:
                        c = coeffs[i + k * self.M]
                        acc_re += int(re_in[idx]) * int(c)
                        acc_im += int(im_in[idx]) * int(c)
                # round half up + arith shift right
                round_bias = 1 << (self.macc_trunc - 1) if self.macc_trunc > 0 else 0
                branch_outs_re[i] = (acc_re + round_bias) >> self.macc_trunc
                branch_outs_im[i] = (acc_im + round_bias) >> self.macc_trunc
            # M-point DFT of branch outputs. np.fft.fft uses the
            # standard X[k] = sum_n x[n] * exp(-2pi*j*n*k/N) convention.
            x = branch_outs_re.astype(np.complex128) \
                + 1j * branch_outs_im.astype(np.complex128)
            spec = np.fft.fft(x)
            bins_re[ep] = np.round(spec.real).astype(np.int64)
            bins_im[ep] = np.round(spec.imag).astype(np.int64)
        return bins_re, bins_im

    def elaborate(self, platform):
        m = Module()

        # ── Sample buffer ─────────────────────────────────────────
        # Stores the most recent M*K complex samples in a circular
        # fashion. The write port advances on each strobe_in, and the
        # MACC scheduler walks the buffer back from the latest write
        # to compute each branch's K-tap dot product.
        sample_w = 2 * self.width_in
        sbuf_depth = self.M * self.K
        m.submodules.sample_buf = sample_buf = Memory(
            shape=sample_w, depth=sbuf_depth, init=[],
            attrs={'ram_style': 'block'},
        )
        sbuf_rdport = sample_buf.read_port()
        sbuf_wrport = sample_buf.write_port()

        # The write pointer and the input strobe are decoupled from the
        # branch sequencer so that input continues to flow even while
        # the previous epoch is still being computed.
        write_ptr = Signal(range(sbuf_depth))
        epoch_phase = Signal(range(self.M))    # 0..M-1, increments per input

        m.d.comb += [
            sbuf_wrport.en.eq(self.strobe_in),
            sbuf_wrport.addr.eq(write_ptr),
            sbuf_wrport.data.eq(Cat(self.re_in, self.im_in)),
        ]
        with m.If(self.strobe_in):
            m.d.sync += [
                write_ptr.eq(
                    Mux(write_ptr == sbuf_depth - 1, 0, write_ptr + 1)),
                epoch_phase.eq(
                    Mux(epoch_phase == self.M - 1, 0, epoch_phase + 1)),
            ]

        # An "epoch start" pulses high on the cycle the M-th input of a
        # group has been written; the sequencer can then begin the
        # M*K-cycle dot-product traversal for that epoch.
        epoch_start = Signal()
        m.d.comb += epoch_start.eq(
            self.strobe_in & (epoch_phase == self.M - 1))

        # ── Coefficient ROM ───────────────────────────────────────
        # Stored in branch-major order: rom[b*K + k] = h[b + k*M].
        # The sequencer enumerates branches outermost so this layout
        # gives sequential reads inside one branch's dot product.
        coeff_init = []
        coeff_mask = (1 << self.coeff_width) - 1
        for b in range(self.M):
            for k in range(self.K):
                c = int(self.coeffs[b + k * self.M]) & coeff_mask
                coeff_init.append(c)
        m.submodules.coeff_rom = coeff_rom = Memory(
            shape=self.coeff_width, depth=self.M * self.K, init=coeff_init,
            # Distributed: 384 × 16 bits = 6 kbits, ~50 LUTs.
            attrs={'ram_style': 'distributed'},
        )
        coeff_rdport = coeff_rom.read_port()

        # ── Branch sequencer ──────────────────────────────────────
        # Walks (branch, tap) pairs to drive the MACC. State variables:
        #   active           : 1 while an epoch is being processed
        #   branch_idx       : current branch index (0..M-1)
        #   tap_idx          : current tap within branch (0..K-1)
        #   sample_addr      : sbuf read address feeding the MACC
        #   coeff_addr       : coeff_rom address feeding the MACC
        #   first_acc        : asserted on the first tap of each branch
        #
        # The MACC is a simple Mealy FSM with 1-cycle multiply +
        # accumulator; we don't pipeline it (target frequency leaves
        # plenty of slack at 62.5 MHz).
        active = Signal()
        branch_idx = Signal(range(self.M))
        tap_idx = Signal(range(self.K))
        # Captured value of write_ptr at epoch_start; the rest of the
        # epoch's reads are relative to this latched anchor so that
        # samples written DURING the epoch don't disturb the dot
        # product window.
        anchor_ptr = Signal(range(sbuf_depth))

        # Pre-compute the read offset for (branch, tap) relative to
        # anchor_ptr. Newest sample sits at anchor_ptr - 1; branch i
        # uses samples at anchor_ptr - 1 - i - k*M.
        def sbuf_read_addr(branch, tap):
            offset = 1 + branch + tap * self.M
            return Mux(
                anchor_ptr >= offset,
                anchor_ptr - offset,
                anchor_ptr + (sbuf_depth - offset))

        with m.If(epoch_start):
            m.d.sync += [
                active.eq(1),
                branch_idx.eq(0),
                tap_idx.eq(0),
                # Capture write_ptr+1 because write happens THIS cycle
                # (write_ptr is the addr being written into; the
                # incremented value is the next write).
                anchor_ptr.eq(
                    Mux(write_ptr == sbuf_depth - 1, 0, write_ptr + 1)),
            ]

        with m.If(active):
            with m.If(tap_idx == self.K - 1):
                m.d.sync += [
                    tap_idx.eq(0),
                    branch_idx.eq(
                        Mux(branch_idx == self.M - 1, 0, branch_idx + 1)),
                ]
                with m.If(branch_idx == self.M - 1):
                    m.d.sync += active.eq(0)
            with m.Else():
                m.d.sync += tap_idx.eq(tap_idx + 1)

        # MACC read addresses (computed combinationally from the
        # current FSM state; results arrive at the MACC input registers
        # one cycle later via the BRAM read port latency).
        m.d.comb += [
            sbuf_rdport.en.eq(1),
            sbuf_rdport.addr.eq(sbuf_read_addr(branch_idx, tap_idx)),
            coeff_rdport.en.eq(1),
            coeff_rdport.addr.eq(branch_idx * self.K + tap_idx),
        ]

        # Pipeline staging — the sample-buffer read port has 1-cycle
        # latency, so we delay valid/first_acc/last_tap signals by 1.
        valid_q = Signal(2, reset_less=True)
        first_q = Signal(2, reset_less=True)
        last_q = Signal(2, reset_less=True)
        first_acc = Signal()
        last_tap = Signal()
        m.d.comb += [
            first_acc.eq(active & (tap_idx == 0)),
            last_tap.eq(active & (tap_idx == self.K - 1)),
        ]
        m.d.sync += [
            valid_q.eq(Cat(active, valid_q[:-1])),
            first_q.eq(Cat(first_acc, first_q[:-1])),
            last_q.eq(Cat(last_tap, last_q[:-1])),
        ]
        # branch_idx latency-matched against the result emerging from
        # MACC (sample read latency 1 + multiply 1 = 2 cycles).
        branch_q = [Signal(range(self.M), reset_less=True) for _ in range(2)]
        m.d.sync += [branch_q[0].eq(branch_idx), branch_q[1].eq(branch_q[0])]

        # ── MACC ──────────────────────────────────────────────────
        # 1-cycle pipelined: register sample + coeff, then product +
        # accumulator update.
        sample_re = sbuf_rdport.data[:self.width_in].as_signed()
        sample_im = sbuf_rdport.data[self.width_in:].as_signed()
        coeff = coeff_rdport.data.as_signed()

        prod_w = self.width_in + self.coeff_width
        acc_w = prod_w + (self.K).bit_length() + 2
        prod_re = Signal(signed(prod_w), reset_less=True)
        prod_im = Signal(signed(prod_w), reset_less=True)
        acc_re = Signal(signed(acc_w), reset_less=True)
        acc_im = Signal(signed(acc_w), reset_less=True)
        round_bias = (1 << (self.macc_trunc - 1)) if self.macc_trunc > 0 else 0

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

        # On the last tap, the accumulator value (after this cycle's
        # add) is the branch's filtered output. Latch it on the cycle
        # AFTER last_q[1] so we read the post-add value.
        branch_done = Signal(reset_less=True)
        m.d.sync += branch_done.eq(last_q[1] & valid_q[1])
        branch_idx_done = Signal(range(self.M), reset_less=True)
        m.d.sync += branch_idx_done.eq(branch_q[1])

        branch_re = Signal(signed(self.width_out), reset_less=True)
        branch_im = Signal(signed(self.width_out), reset_less=True)
        # Saturating shift: arith-right by macc_trunc, then clamp to
        # signed(width_out). Amaranth doesn't ship a native saturating
        # cast, so we compute manually.
        max_v = (1 << (self.width_out - 1)) - 1
        min_v = -(1 << (self.width_out - 1))
        with m.If(branch_done):
            re_shift = (acc_re >> self.macc_trunc).as_signed()
            im_shift = (acc_im >> self.macc_trunc).as_signed()
            m.d.sync += [
                branch_re.eq(
                    Mux(re_shift > max_v, max_v,
                        Mux(re_shift < min_v, min_v, re_shift))),
                branch_im.eq(
                    Mux(im_shift > max_v, max_v,
                        Mux(im_shift < min_v, min_v, im_shift))),
            ]

        # ── FFT ───────────────────────────────────────────────────
        # M=64 → R22 with order_log2=6. Truncate schedule keeps the
        # FFT output at width_out bits given branch output width.
        # Per-stage growth is 2 bits in R22; M=64 has 3 R22 stages =
        # 6 bits of growth. Schedule: drop 2 bits per stage so output
        # width matches input width.
        num_pairs = self.order_log2 // 2
        target_trunc = num_pairs * 2  # input width preserved at output
        # Distribute as [1,1] per pair → drops 2 bits/stage.
        fft_truncates = [[1, 1] for _ in range(num_pairs)]
        m.submodules.fft = fft = FFT(
            self.width_out, self.order_log2, 'R22',
            width_twiddle=16, truncates=fft_truncates,
            butterfly_storage='distributed',
            twiddle_storage='lut',
            use_bram_reg=False,
            window=None,            # polyphase already applied a window
            cmult3x=True,
            domain_3x=self._domain_3x,
        )
        fft_clken = Signal()
        # Latch in the branch result, then pulse the FFT clken once.
        m.d.sync += fft_clken.eq(branch_done)
        fft_re_in = Signal(signed(self.width_out), reset_less=True)
        fft_im_in = Signal(signed(self.width_out), reset_less=True)
        with m.If(branch_done):
            m.d.sync += [
                fft_re_in.eq(branch_re),
                fft_im_in.eq(branch_im),
            ]
        m.d.comb += [
            fft.clken.eq(fft_clken),
            fft.re_in.eq(fft_re_in),
            fft.im_in.eq(fft_im_in),
            fft.common_edge_3x.eq(self.common_edge_3x),
        ]

        # ── Output demux ──────────────────────────────────────────
        # Track which output bin index is currently emerging from the
        # FFT. R22 DIF output bin order is bit-reversed; we present
        # the natural-order bin index by applying a bit-reverse on
        # the output counter so downstream consumers see bin 0 = DC,
        # bin M/2 = +Nyquist, etc.
        out_counter = Signal(self.order_log2, reset_less=True)
        with m.If(fft_clken):
            m.d.sync += out_counter.eq(out_counter + 1)
        out_bin = Signal(self.order_log2)
        m.d.comb += out_bin.eq(_bit_reverse(out_counter, self.order_log2))

        m.d.comb += [
            self.re_out.eq(fft.re_out),
            self.im_out.eq(fft.im_out),
            self.bin_index.eq(out_bin),
            self.strobe_out.eq(fft_clken),  # one-cycle behind FFT
            # Note: real strobe should come from FFT.out_last logic;
            # for now we emit a strobe per consumed FFT input. A
            # proper output-side strobe is wired in M2 once we
            # exercise the FFT pipeline drain in sim.
        ]

        # Parallel-demux registers — latch each serial output into its
        # bin slot. ``bin_strobe`` rises when the last bin (index
        # M-1) has just been latched, signalling a fresh epoch.
        with m.If(fft_clken):
            with m.Switch(out_bin):
                for i in range(self.M):
                    with m.Case(i):
                        m.d.sync += [
                            self.bin_re[i].eq(fft.re_out),
                            self.bin_im[i].eq(fft.im_out),
                        ]
        last_bin = Signal()
        m.d.comb += last_bin.eq(fft_clken & (out_bin == self.M - 1))
        m.d.sync += self.bin_strobe.eq(last_bin)

        return m


def _bit_reverse(sig, nbits):
    """Bit-reverse an n-bit signal (combinational expression)."""
    return Cat(sig[nbits - 1 - i] for i in range(nbits))

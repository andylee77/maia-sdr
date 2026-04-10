#
# Fishball P25 -- LSM front-end FIR (LPF + RRC matched filter)
#
# Phase 6E.2 / 6E.3 of the LSM HDL port. Generic real-coefficient
# complex FIR with frozen taps. Both the 83-tap baseband LPF and the
# 105-tap RRC matched filter are instantiations of this same module
# with different ``taps`` arguments.
#
# Direct port of ``apply_real_fir_complex`` and ``StreamingFir`` from
# ``p25-httpd/src/lsm/filters.rs``: a streaming, causal, length-N
# FIR applied independently to I and Q with shared real coefficients.
# The Rust streaming version threads a per-stream history vector
# across calls to keep boundary transients out of the output; in HDL
# the "history" is just the always-on shift register, so streaming
# correctness is automatic.
#
# Architecture
# ------------
# - Sample buffer: a length-N shift register for I and a length-N
#   shift register for Q. Vivado infers SLR (shift-register LUT)
#   chains, so the storage cost is roughly N LUTs per channel
#   (~166 LUTs for the 83-tap LPF, ~210 for the 105-tap RRC).
# - Coefficient ROM: a Python list converted to an Amaranth Array of
#   Const, indexed by the runtime MAC counter. Vivado infers a
#   block-RAM or distributed-RAM ROM as appropriate.
# - MAC: one multiply-accumulate per cycle, sequenced by a small
#   FSM. Two parallel MAC chains share the same coefficient lookup
#   (one for I, one for Q), so the cost is 2 DSP48E1 per FIR.
# - Output: shift right by ``input_frac_bits + coeff_frac_bits -
#   output_frac_bits`` to put the accumulator back in the output
#   Q-format, then saturate to ``output_width`` signed.
#
# Throughput
# ----------
# Input rate is 31.25 kSPS at a 62.5 MHz sync clock = 2000 cycles
# per input sample. Each output takes ``N + 2`` cycles (N MAC +
# 1 latch + 1 strobe). For the 105-tap RRC that is 107 cycles, well
# below the 2000-cycle budget. The block accepts back-to-back
# strobes from upstream as long as they are >= ``N + 2`` cycles
# apart, which the 31.25 kSPS input rate trivially satisfies.
#
# Fixed-point format choice
# -------------------------
# - Input: ``Q1.{input_width-1}`` (signed, ``input_width-1``
#   fractional bits). Default Q1.15.
# - Coeffs: ``Q1.{coeff_width-1}``. Default Q1.17 (18-bit signed,
#   matches the existing Maia DDC coefficient width). Q1.17 ULP =
#   7.6e-6, smaller than the smallest LPF tap (1.86e-4) by a factor
#   of 24, so even the worst-case relative quantisation error on
#   any tap is well under 5 %.
# - Accumulator: ``input_width + coeff_width + ceil(log2(N))``,
#   rounded up to the next byte for clarity. The 105-tap RRC needs
#   16 + 18 + 7 = 41 bits; we use 48 to give DSP48E1's native
#   accumulator width.
# - Output: ``Q1.{output_width-1}``. Default Q1.15. Saturated.
#
# DSP48E1 cost
# ------------
# 2 DSP per FIR: one for the I MAC, one for the Q MAC. Two FIRs in
# the LSM chain (LPF + RRC) -> 4 DSP total. Plus the demod-loop
# multiplies in 6E.4-6E.6 brings the LSM chain to ~12 DSP, well
# under the 220 available on the Z7020 even with the existing C4FM
# chain still present.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *


class LsmFir(Elaboratable):
    """Real-coefficient complex FIR with frozen taps for the LSM front end.

    See module docstring for the architecture rationale and fixed-point
    format. The taps are passed in as a list of Python floats and
    quantised at construction time -- the resulting integer ROM is
    stored in ``self.taps_q`` so the test side can read it back for
    inspection.

    Inputs (sync domain):
        re_in, im_in: signed ``input_width`` IQ samples
        strobe_in:    sample valid pulse (one cycle per new sample)

    Outputs (sync domain):
        re_out, im_out: signed ``output_width`` filtered samples
            (registered, valid only on the cycle ``strobe_out`` is
            asserted, then held until the next output)
        strobe_out:   output valid pulse, asserted ``N + 2`` cycles
            after each ``strobe_in``
    """

    def __init__(
        self,
        taps,
        *,
        input_width=16,
        coeff_width=18,
        output_width=16,
    ):
        self.taps = list(taps)
        self.n_taps = len(self.taps)
        if self.n_taps < 1:
            raise ValueError("FIR needs at least one tap")

        self.iw = input_width
        self.cw = coeff_width
        self.ow = output_width

        # Q-format choice: 1 sign bit + (width-1) fractional bits.
        self.input_frac_bits = input_width - 1
        self.coeff_frac_bits = coeff_width - 1
        self.output_frac_bits = output_width - 1

        # How many bits to shift the accumulator right to get back to
        # the output Q-format.
        self.shift = (
            self.input_frac_bits + self.coeff_frac_bits - self.output_frac_bits
        )

        # Quantise taps to signed coeff_width with saturation. Round
        # to nearest, ties away from zero (Python's int(round())
        # banker's rounding behaviour is fine here -- the taps are
        # already pre-designed and we just need stable quantisation).
        coeff_max = (1 << (coeff_width - 1)) - 1
        coeff_min = -(1 << (coeff_width - 1))
        scale = 1 << self.coeff_frac_bits
        self.taps_q = []
        for t in self.taps:
            q = int(round(float(t) * scale))
            if q > coeff_max:
                q = coeff_max
            elif q < coeff_min:
                q = coeff_min
            self.taps_q.append(q)

        # Accumulator width: enough headroom for sum of N
        # signed (input_width + coeff_width)-bit products.
        self.acc_width = input_width + coeff_width + (self.n_taps - 1).bit_length() + 1
        # Round up to a multiple of 8 for clarity.
        self.acc_width = ((self.acc_width + 7) // 8) * 8

        # I/O signals
        self.re_in = Signal(signed(input_width))
        self.im_in = Signal(signed(input_width))
        self.strobe_in = Signal()

        self.re_out = Signal(signed(output_width), reset_less=True)
        self.im_out = Signal(signed(output_width), reset_less=True)
        self.strobe_out = Signal()

    @staticmethod
    def _saturate(m, value, out_width):
        """Return a saturating-cast Value of ``value`` clipped to a
        signed ``out_width``-bit range.

        ``value`` is the source signed Signal/Value (any width). The
        return value is a fresh Signal of width ``out_width`` with
        the saturated result.
        """
        result = Signal(signed(out_width))
        max_val = (1 << (out_width - 1)) - 1
        min_val = -(1 << (out_width - 1))
        with m.If(value > max_val):
            m.d.comb += result.eq(max_val)
        with m.Elif(value < min_val):
            m.d.comb += result.eq(min_val)
        with m.Else():
            m.d.comb += result.eq(value)
        return result

    def elaborate(self, platform):
        m = Module()
        N = self.n_taps

        # ── Sample shift registers ─────────────────────────────────
        # Index 0 = newest sample (after the shift), index N-1 = oldest.
        # Vivado will infer SLRs (shift-register LUTs) for the
        # successive-position assignments below.
        buf_re = Array([
            Signal(signed(self.iw), reset_less=True, name=f"buf_re_{i}")
            for i in range(N)
        ])
        buf_im = Array([
            Signal(signed(self.iw), reset_less=True, name=f"buf_im_{i}")
            for i in range(N)
        ])

        # ── Coefficient ROM ────────────────────────────────────────
        # Indexed by the MAC counter k = 0..N-1. With Const elements
        # this is a hard-wired ROM; Vivado will pick distributed RAM
        # or LUT logic as appropriate for the size.
        coeffs = Array([
            Const(c, signed(self.cw)) for c in self.taps_q
        ])

        # ── MAC counter + state ────────────────────────────────────
        # k counts 0..N-1 during the MAC sequence; one extra value
        # (N) is the "all done, latch output" terminal state.
        k = Signal(range(N + 2), reset_less=True)
        running = Signal(init=0)

        re_acc = Signal(signed(self.acc_width), reset_less=True)
        im_acc = Signal(signed(self.acc_width), reset_less=True)

        # Default: no output strobe.
        m.d.sync += self.strobe_out.eq(0)

        with m.If(self.strobe_in):
            # Shift the sample buffers by one position and insert
            # the new sample at index 0. This shift takes effect at
            # the next clock edge, so the MAC sequence (which starts
            # on that same next edge) sees the new sample.
            for i in range(N - 1, 0, -1):
                m.d.sync += [
                    buf_re[i].eq(buf_re[i - 1]),
                    buf_im[i].eq(buf_im[i - 1]),
                ]
            m.d.sync += [
                buf_re[0].eq(self.re_in),
                buf_im[0].eq(self.im_in),
                # Reset accumulators and start the MAC sequence.
                re_acc.eq(0),
                im_acc.eq(0),
                k.eq(0),
                running.eq(1),
            ]
        with m.Elif(running):
            with m.If(k < N):
                # MAC step: y[n] = sum_{k=0..N-1} h[k] * x[n-k].
                # After the shift above, buf_re[k] is the k-th most
                # recent sample (buf_re[0] is x[n], buf_re[1] is
                # x[n-1], etc.), and coeffs[k] is h[k]. So the
                # product `buf_re[k] * coeffs[k]` is exactly the
                # k-th term of the convolution.
                m.d.sync += [
                    re_acc.eq(re_acc + buf_re[k] * coeffs[k]),
                    im_acc.eq(im_acc + buf_im[k] * coeffs[k]),
                    k.eq(k + 1),
                ]
            with m.Else():
                # All N MACs done. Rescale the accumulator back to
                # the output Q-format, saturate to output_width, and
                # latch the result. Strobe the output.
                re_scaled = re_acc >> self.shift
                im_scaled = im_acc >> self.shift
                re_sat = self._saturate(m, re_scaled, self.ow)
                im_sat = self._saturate(m, im_scaled, self.ow)
                m.d.sync += [
                    self.re_out.eq(re_sat),
                    self.im_out.eq(im_sat),
                    self.strobe_out.eq(1),
                    running.eq(0),
                ]

        return m


# ─── Frozen tap arrays imported from the Rust reference ─────────────
#
# These literal lists are exact copies of the constants in
# `p25-httpd/src/lsm/filters.rs`. They are *not* designed at
# elaboration time -- the design lives in `tools/p25_lsm_demod.py`
# (`design_baseband_lpf` and `design_rrc`), the Rust port froze them
# into a `[f32; N]` array, and we mirror the same array here so the
# HDL is bit-comparable to the Rust pipeline at the f32 -> Q1.17
# rounding step.
#
# Regenerate by running `cargo test --bin p25-httpd lsm::golden_dump`
# (which uses the Rust constants) and copy-pasting the new tap
# values from `p25-httpd/src/lsm/filters.rs` into the lists below if
# the design ever changes.

# 83-tap Parks-McClellan equiripple baseband LPF, 31.25 kSPS,
# passband 0..7250 Hz, stopband 8000..15625 Hz.
LPF_TAPS_31250 = [
    1.8599540676e-04, -6.2620649561e-03, -3.6394699086e-04,  2.6395369719e-03,
    5.1450542933e-04, -3.1611807556e-03, -8.8228333582e-04,  3.7203046654e-03,
    1.3594551368e-03, -4.3145217852e-03, -1.9670868006e-03,  4.9395977123e-03,
    2.7230176112e-03, -5.5873223218e-03, -3.6520602689e-03,  6.2505818540e-03,
    4.7839919887e-03, -6.9186670918e-03, -6.1556700757e-03,  7.5850955063e-03,
    7.8156079622e-03, -8.2360150360e-03, -9.8258793891e-03,  8.8683630704e-03,
    1.2289056582e-02, -9.4554414724e-03, -1.5349465703e-02,  1.0012688844e-02,
    1.9241454340e-02, -1.0508576436e-02, -2.4388093939e-02,  1.0943656808e-02,
    3.1569350881e-02, -1.1317206147e-02, -4.2465966533e-02,  1.1612428065e-02,
    6.1485703065e-02, -1.1826637973e-02, -1.0478690800e-01,  1.1952966919e-02,
    3.1786922263e-01,  4.8800390754e-01,  3.1786922263e-01,  1.1952966919e-02,
   -1.0478690800e-01, -1.1826637973e-02,  6.1485703065e-02,  1.1612428065e-02,
   -4.2465966533e-02, -1.1317206147e-02,  3.1569350881e-02,  1.0943656808e-02,
   -2.4388093939e-02, -1.0508576436e-02,  1.9241454340e-02,  1.0012688844e-02,
   -1.5349465703e-02, -9.4554414724e-03,  1.2289056582e-02,  8.8683630704e-03,
   -9.8258793891e-03, -8.2360150360e-03,  7.8156079622e-03,  7.5850955063e-03,
   -6.1556700757e-03, -6.9186670918e-03,  4.7839919887e-03,  6.2505818540e-03,
   -3.6520602689e-03, -5.5873223218e-03,  2.7230176112e-03,  4.9395977123e-03,
   -1.9670868006e-03, -4.3145217852e-03,  1.3594551368e-03,  3.7203046654e-03,
   -8.8228333582e-04, -3.1611807556e-03,  5.1450542933e-04,  2.6395369719e-03,
   -3.6394699086e-04, -6.2620649561e-03,  1.8599540676e-04,
]
assert len(LPF_TAPS_31250) == 83

# 105-tap unit-energy root raised cosine matched filter, sps =
# 31250/4800, alpha=0.2, 16 symbols.
RRC_TAPS_31250 = [
   -1.0273903608e-03,  4.9391790526e-04,  1.9210274331e-03,  2.7859127149e-03,
    2.7780425735e-03,  1.8582775956e-03,  2.9086196446e-04, -1.4235960552e-03,
   -2.6999015827e-03, -3.0612742994e-03, -2.3125547450e-03, -6.3365290407e-04,
    1.4458663063e-03,  3.1944846269e-03,  3.9118854329e-03,  3.1785175670e-03,
    1.0395868449e-03, -1.9460807089e-03, -4.8254416324e-03, -6.5208370797e-03,
   -6.1804908328e-03, -3.5056071356e-03,  1.0560825467e-03,  6.3321157359e-03,
    1.0686622933e-02,  1.2473019771e-02,  1.0563435033e-02,  4.8006759025e-03,
   -3.7790331990e-03, -1.3055616990e-02, -2.0284336060e-02, -2.2820075974e-02,
   -1.8932243809e-02, -8.4851626307e-03,  6.7335492931e-03,  2.3183923215e-02,
    3.6271259189e-02,  4.1456125677e-02,  3.5535667092e-02,  1.7779115587e-02,
   -9.3995966017e-03, -4.0466103703e-02, -6.7569032311e-02, -8.2017973065e-02,
   -7.6154530048e-02, -4.5213922858e-02,  1.1257279664e-02,  8.8789448142e-02,
    1.7838372290e-01,  2.6789104939e-01,  3.4413120151e-01,  3.9532911777e-01,
    4.1336068511e-01,  3.9532911777e-01,  3.4413120151e-01,  2.6789104939e-01,
    1.7838372290e-01,  8.8789448142e-02,  1.1257279664e-02, -4.5213922858e-02,
   -7.6154530048e-02, -8.2017973065e-02, -6.7569032311e-02, -4.0466103703e-02,
   -9.3995966017e-03,  1.7779115587e-02,  3.5535667092e-02,  4.1456125677e-02,
    3.6271259189e-02,  2.3183923215e-02,  6.7335492931e-03, -8.4851626307e-03,
   -1.8932243809e-02, -2.2820075974e-02, -2.0284336060e-02, -1.3055616990e-02,
   -3.7790331990e-03,  4.8006759025e-03,  1.0563435033e-02,  1.2473019771e-02,
    1.0686622933e-02,  6.3321157359e-03,  1.0560825467e-03, -3.5056071356e-03,
   -6.1804908328e-03, -6.5208370797e-03, -4.8254416324e-03, -1.9460807089e-03,
    1.0395868449e-03,  3.1785175670e-03,  3.9118854329e-03,  3.1944846269e-03,
    1.4458663063e-03, -6.3365290407e-04, -2.3125547450e-03, -3.0612742994e-03,
   -2.6999015827e-03, -1.4235960552e-03,  2.9086196446e-04,  1.8582775956e-03,
    2.7780425735e-03,  2.7859127149e-03,  1.9210274331e-03,  4.9391790526e-04,
   -1.0273903608e-03,
]
assert len(RRC_TAPS_31250) == 105

#
# Fishball P25 - Differential Demodulator (C4FM + LSM/CQPSK)
#
# Computes the full complex differential product:
#   z_curr * conj(z_prev) = (re_curr + j*im_curr) * (re_prev - j*im_prev)
#   diff_re = re_curr*re_prev + im_curr*im_prev
#   diff_im = im_curr*re_prev - re_curr*im_prev
#
# diff_im is the classic FM cross-product discriminator and works for C4FM
# (where data = instantaneous frequency).
# (diff_re, diff_im) together is the differential symbol vector and works
# for both C4FM and LSM (CQPSK), matching SDRTrunk's unified approach.
# The dibit is then just the two sign bits of (diff_re, diff_im).
#
# Cost: 4 DSP48E1 (16x16 multiplies)
#
# SPDX-License-Identifier: MIT
#

from amaranth import *


class C4FMDemod(Elaboratable):
    """Differential demodulator for P25 C4FM and LSM (CQPSK)

    Computes the full complex differential product z[n] * conj(z[n-1]).
    The imaginary part is the FM frequency discriminator (works for C4FM).
    The (real, imag) pair is the differential symbol vector — its quadrant
    encodes the dibit for both C4FM and LSM.

    Inputs (sync domain):
        re_in, im_in: 16-bit signed IQ samples from DDC
        strobe_in: sample valid strobe

    Outputs (sync domain):
        diff_re_out, diff_im_out: 18-bit signed differential product
        disc_out: alias for diff_im_out (legacy name for FM discriminator)
        strobe_out: output valid strobe
    """
    def __init__(self):
        # Inputs
        self.re_in = Signal(signed(16))
        self.im_in = Signal(signed(16))
        self.strobe_in = Signal()

        # Outputs (full complex differential product)
        self.diff_re_out = Signal(signed(18))
        self.diff_im_out = Signal(signed(18))
        self.strobe_out = Signal()

        # Legacy alias: disc_out == diff_im_out (FM cross-product)
        self.disc_out = self.diff_im_out

    def elaborate(self, platform):
        m = Module()

        # Previous IQ sample registers
        re_prev = Signal(signed(16), reset_less=True)
        im_prev = Signal(signed(16), reset_less=True)

        # Pipeline stage 1: capture inputs and previous sample
        re_curr_r = Signal(signed(16), reset_less=True)
        im_curr_r = Signal(signed(16), reset_less=True)
        re_prev_r = Signal(signed(16), reset_less=True)
        im_prev_r = Signal(signed(16), reset_less=True)
        strobe_p1 = Signal()

        with m.If(self.strobe_in):
            m.d.sync += [
                re_curr_r.eq(self.re_in),
                im_curr_r.eq(self.im_in),
                re_prev_r.eq(re_prev),
                im_prev_r.eq(im_prev),
                re_prev.eq(self.re_in),
                im_prev.eq(self.im_in),
                strobe_p1.eq(1),
            ]
        with m.Else():
            m.d.sync += strobe_p1.eq(0)

        # Pipeline stage 2: 4 multiplies for full complex differential
        # diff_re = re_curr*re_prev + im_curr*im_prev
        # diff_im = im_curr*re_prev - re_curr*im_prev
        rr = Signal(signed(32), reset_less=True)  # re_curr * re_prev
        ii = Signal(signed(32), reset_less=True)  # im_curr * im_prev
        ir = Signal(signed(32), reset_less=True)  # im_curr * re_prev
        ri = Signal(signed(32), reset_less=True)  # re_curr * im_prev

        with m.If(strobe_p1):
            m.d.sync += [
                rr.eq(re_curr_r * re_prev_r),
                ii.eq(im_curr_r * im_prev_r),
                ir.eq(im_curr_r * re_prev_r),
                ri.eq(re_curr_r * im_prev_r),
                self.strobe_out.eq(1),
            ]
        with m.Else():
            m.d.sync += self.strobe_out.eq(0)

        # Combine: diff_re = rr + ii ; diff_im = ir - ri
        # Each is 33-bit, right-shift by 15 to fit 18-bit output
        diff_re_full = Signal(signed(33), reset_less=True)
        diff_im_full = Signal(signed(33), reset_less=True)
        m.d.comb += [
            diff_re_full.eq(rr + ii),
            diff_im_full.eq(ir - ri),
            self.diff_re_out.eq(diff_re_full >> 15),
            self.diff_im_out.eq(diff_im_full >> 15),
        ]

        return m

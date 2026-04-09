#
# Fishball P25 - C4FM Differential Demodulator
#
# Cross-product FM discriminator:
#   disc[n] = re[n-1]*im[n] - im[n-1]*re[n]
#
# Output is proportional to instantaneous frequency deviation.
# For P25 C4FM (±1800/±600 Hz, 4800 sym/sec), the small modulation
# index means sin(delta_phi) ≈ delta_phi, so the discriminator output
# maps directly to the 4 deviation levels.
#
# Cost: 2 DSP48E1 (two 16x16 multiplies for cross product)
#
# SPDX-License-Identifier: MIT
#

from amaranth import *


class C4FMDemod(Elaboratable):
    """C4FM FM discriminator

    Inputs (sync domain):
        re_in, im_in: 16-bit signed IQ samples from DDC
        strobe_in: sample valid strobe

    Outputs (sync domain):
        disc_out: 18-bit signed discriminator output
        strobe_out: output valid strobe
    """
    def __init__(self):
        # Inputs
        self.re_in = Signal(signed(16))
        self.im_in = Signal(signed(16))
        self.strobe_in = Signal()

        # Outputs
        self.disc_out = Signal(signed(18))
        self.strobe_out = Signal()

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
                # Latch current and previous for multiply stage
                re_curr_r.eq(self.re_in),
                im_curr_r.eq(self.im_in),
                re_prev_r.eq(re_prev),
                im_prev_r.eq(im_prev),
                # Update previous sample
                re_prev.eq(self.re_in),
                im_prev.eq(self.im_in),
                strobe_p1.eq(1),
            ]
        with m.Else():
            m.d.sync += strobe_p1.eq(0)

        # Pipeline stage 2: cross-product multiply and subtract
        # disc = re_prev * im_curr - im_prev * re_curr
        # Each multiply is 16x16 -> 32 bits, difference -> 33 bits
        # Truncate to 18 bits (drop 15 LSBs) for downstream
        prod_a = Signal(signed(32), reset_less=True)  # re_prev * im_curr
        prod_b = Signal(signed(32), reset_less=True)  # im_prev * re_curr

        with m.If(strobe_p1):
            m.d.sync += [
                prod_a.eq(re_prev_r * im_curr_r),
                prod_b.eq(im_prev_r * re_curr_r),
                self.strobe_out.eq(1),
            ]
        with m.Else():
            m.d.sync += self.strobe_out.eq(0)

        # Output: (prod_a - prod_b) >> 15, saturated to 18 bits
        diff = Signal(signed(33), reset_less=True)
        m.d.comb += diff.eq(prod_a - prod_b)
        # Right-shift by 15 to fit 18-bit output (keeps sign + 17 magnitude)
        shifted = Signal(signed(18), reset_less=True)
        m.d.comb += shifted.eq(diff >> 15)
        m.d.comb += self.disc_out.eq(shifted)

        return m

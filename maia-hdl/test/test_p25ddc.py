#
# Fishball P25 -- P25DDC HDL tests
#
# Unit tests for the P25DDC subclass of maia_hdl.ddc.DDC. The
# underlying DDC logic is already exercised by maia-hdl's own FIR
# tests and by the full p25_top.py elaboration path; these tests
# focus on the subclass-specific behaviour:
#
#   1.  macc_trunc default matches the v2 SDRTrunk-faithful profile
#       ([14, 17, 17]), which pairs with the unit-DC-gain coefficient
#       tables produced by tools/p25_ddc_filter_design.py v2.
#
#   2.  The subclass is a strict instanceof DDC, so any downstream
#       code that type-checks against DDC still works.
#
#   3.  Port shape is identical to DDC, i.e. P25DDC is a drop-in
#       replacement at p25_top.py's two DDC instance sites.
#
#   4.  macc_trunc can be overridden at construction time (so the
#       subclass default is just a default, not a lock-in).
#
#   5.  Elaboration of P25DDC succeeds end-to-end with the required
#       clk3x domain wrapper.
#
# SPDX-License-Identifier: MIT
#

import unittest

from amaranth import *
from amaranth.hdl import Fragment

from maia_hdl.ddc import DDC
from p25_hdl.p25ddc import P25DDC


class TestP25DDC(unittest.TestCase):
    def test_default_macc_trunc_is_v2_profile(self):
        """The v2 profile assumes unit-DC-gain coefficient tables
        (sum(taps) ~ 2^17) and gives an 8x amplification at stage 1
        to preserve SNR through the cascade, while stages 2 and 3
        remain unit gain. Total cascaded boost: +18 dB, which fits
        comfortably inside LsmAgc's 54 dB per-symbol headroom."""
        ddc = P25DDC('clk3x')
        self.assertEqual(ddc.macc_trunc, [14, 17, 17])

    def test_maia_ddc_default_is_different(self):
        """Document the difference vs the Maia DDC default so the
        test suite fails loudly if upstream ever changes its
        default away from [17, 18, 18]."""
        maia = DDC('clk3x')
        self.assertEqual(maia.macc_trunc, [17, 18, 18])
        self.assertNotEqual(maia.macc_trunc, P25DDC('clk3x').macc_trunc)

    def test_is_subclass_of_ddc(self):
        """P25DDC must remain a DDC subclass so any isinstance(...,
        DDC) check in the Maia code still works. Avoids a class of
        subtle Python duck-typing bugs if P25DDC is ever used in a
        place that was only typed against DDC."""
        ddc = P25DDC('clk3x')
        self.assertIsInstance(ddc, DDC)

    def test_port_shape_matches_ddc(self):
        """Strict drop-in replacement requires that every Signal
        attribute on DDC also exists on P25DDC (and vice versa).
        The subclass inherits all of them, but a future refactor
        that adds new ports to P25DDC without matching DDC would
        silently break this — catch it here."""
        maia_ports = {
            name for name in dir(DDC('clk3x'))
            if not name.startswith('_')
            and isinstance(getattr(DDC('clk3x'), name, None), Signal)
        }
        p25_ports = {
            name for name in dir(P25DDC('clk3x'))
            if not name.startswith('_')
            and isinstance(getattr(P25DDC('clk3x'), name, None), Signal)
        }
        self.assertEqual(maia_ports, p25_ports)

    def test_macc_trunc_override_at_construction(self):
        """The v2 default is a default, not a lock-in. Callers that
        want to experiment with a different truncation profile
        should be able to pass macc_trunc=... explicitly."""
        override = [16, 16, 16]
        ddc = P25DDC('clk3x', macc_trunc=override)
        self.assertEqual(ddc.macc_trunc, override)

    def test_elaboration(self):
        """Concrete elaboration check: P25DDC must compile into a
        real Amaranth Fragment when wrapped in a module that
        provides the required clk3x domain."""
        m = Module()
        m.domains.clk3x = ClockDomain('clk3x')
        m.submodules.ddc = P25DDC('clk3x')
        # Fragment.get raises if elaboration fails.
        frag = Fragment.get(m, platform=None)
        # Sanity: the fragment has at least one subfragment (the DDC).
        self.assertGreater(len(frag.subfragments), 0)


if __name__ == '__main__':
    unittest.main()

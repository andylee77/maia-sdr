#
# Fishball P25 -- P25DDC fork (v2, SDRTrunk-faithful)
#
# Thin subclass of maia_hdl.ddc.DDC with P25-specific defaults for
# the output truncation (macc_trunc) and clarifying documentation
# about the SDRTrunk-faithful coefficient-design convention in use.
#
# Port-compatible with DDC for drop-in replacement in
# p25_hdl/p25_top.py:
#
#     from .p25ddc import P25DDC as DDC   # one-line change
#
# All port signals, widths, and semantics are inherited unchanged.
# The only difference is the per-stage truncation profile, which
# assumes the coefficients loaded from the PS side are unit-DC-gain
# (sum(taps) ~ 1.0 in float, ~131072 in Q1.17 integer units).
#
# Why the fork exists
# -------------------
# Phase 10-prep's DDC design baked a ~170x cascaded signal
# amplification into the filter shape by rescaling each stage's
# coefficient table so the peak quantised tap landed at Q1.17 max
# (131071). That trick silently coupled filter design and
# macc_trunc: any change to the coefficient set shifted the
# effective per-stage DC gain, and a bake cycle on 2026-04-15 was
# lost when unit-DC-gain coefficients starved the demod chain by
# 44+ dB because the macc_trunc was still tuned for the rescaled
# convention.
#
# The v2 design breaks this coupling. Coefficients are designed for
# unit DC gain (`tools/p25_ddc_filter_design.py` v2), and the
# amplification is explicit in this module's `macc_trunc` default.
# Filter design and gain/scale are now orthogonal -- any future
# coefficient swap is a pure filter-shape change.
#
# The v2 filter design is also much sharper than Phase 10-prep's:
# stage 3 uses the full FIR4DSP 256-tap budget with a 7.25 kHz
# passband (matching SDRTrunk's Remez baseband LPF passband edge),
# which fixes the LsmDecimator2 fold-back bug documented in
# `doc/changes/041_p25ddc_fork.md`.
#
# macc_trunc profile
# ------------------
# Default is [14, 17, 17] (vs maia_hdl.ddc.DDC's [17, 18, 18]):
#
#   * Stage 1 macc_trunc=14 gives an 8x amplification (2^(17-14) = 8)
#     at the stage 1 output. Stage 1 is a /4 decimator with a 12-bit
#     Q1.11 input; 8x amplification brings the stage 1 output to
#     15-bit usable range inside the 16-bit container, which
#     preserves the input SNR without clipping (12-bit peak 2047 x
#     8 = 16376, well under the 16-bit signed max of 32767).
#
#   * Stage 2 macc_trunc=17 is unit gain. Stage 2 input is a 16-bit
#     signed from stage 1 (with 15-bit effective signal), and the
#     unit-gain stage preserves that through the /4 decimation.
#
#   * Stage 3 macc_trunc=17 is also unit gain. The v2 stage 3 filter
#     uses the full 256-tap FIR4DSP budget, so its output has very
#     clean anti-alias behaviour in the 15.625-31.25 kHz band that
#     LsmDecimator2 would otherwise fold onto the P25 channel.
#
# Cascaded gain: 8x = +18 dB vs unit. Phase 10-prep had ~170x = +44
# dB. The downstream LsmAgc (per-symbol, 500x max gain) has 54 dB of
# total headroom, so any value from 0 to ~+45 dB of DDC gain works.
# The +18 dB choice keeps stage 1 well clear of clipping while
# leaving most of LsmAgc's 54 dB headroom available for per-symbol
# normalisation.
#
# Coefficient loading (unchanged from maia_hdl.ddc.DDC)
# -----------------------------------------------------
# The PS loads coefficients into the DDC coefficient RAM via the
# `coeff_waddr / coeff_wren / coeff_wdata` ports. The per-stage
# coefficient tables must be the unit-DC-gain tables emitted by
# `tools/p25_ddc_filter_design.py` v2, not the Phase 10-prep
# peak-rescaled tables. See p25-httpd/src/fpga.rs::configure_ddc().
#
# Port shape
# ----------
# Identical to maia_hdl.ddc.DDC. See that module's docstring for the
# full port list. Drop-in replacement means no changes to p25_top.py
# beyond the import swap.
#
# SPDX-License-Identifier: MIT
#

from maia_hdl.ddc import DDC


class P25DDC(DDC):
    """P25-specific digital down-converter (v2, SDRTrunk-faithful).

    Subclass of maia_hdl.ddc.DDC that changes only the macc_trunc
    default to match the unit-DC-gain coefficient convention used by
    `tools/p25_ddc_filter_design.py` v2.

    All constructor arguments except ``macc_trunc`` default to the
    same values as DDC. Passing ``macc_trunc`` explicitly will
    override this subclass's default and behave identically to DDC.

    Example
    -------
    >>> # In p25_hdl/p25_top.py:
    >>> from .p25ddc import P25DDC
    >>> self.ddc = P25DDC('clk3x')
    >>> self.traffic_ddc = P25DDC('clk3x')

    Attributes
    ----------
    All attributes are inherited from DDC. The only behavioural
    difference is the default macc_trunc profile described in the
    module docstring.
    """

    def __init__(
        self,
        domain_3x: str,
        *,
        in_width: int = 12,
        out_width: list[int] = [16] * 3,
        nco_width: int = 28,
        coeff_width: int = 18,
        decim_width: list[int] = [7, 6, 7],
        oper_width: list[int] = [7, 6, 7],
        # P25-specific default. Assumes unit-DC-gain coefficient
        # tables. See module docstring for the gain budget rationale.
        macc_trunc: list[int] = [14, 17, 17],
    ):
        super().__init__(
            domain_3x,
            in_width=in_width,
            out_width=out_width,
            nco_width=nco_width,
            coeff_width=coeff_width,
            decim_width=decim_width,
            oper_width=oper_width,
            macc_trunc=macc_trunc,
        )

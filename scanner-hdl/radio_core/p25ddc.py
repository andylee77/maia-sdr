#
# Fishball radio core - a lane's DDC: Maia's DDC (mixer and three FIR stages) with the output
# truncation set for unit-DC-gain coefficients.
#
# Ports, widths and behaviour are maia_hdl.ddc.DDC's. Only the default `macc_trunc` differs, and
# it assumes the coefficient convention of `tools/p25_ddc_filter_design.py`: every stage's taps
# sum to unit DC gain (about 131072 in Q1.17), so filter shape and gain stay independent and a new
# coefficient set changes only the shape.
#
# macc_trunc [14, 17, 17] (Maia's default is [17, 18, 18]):
#
#   * Stage 1, 14: an 8x gain (2^(17-14)). The 12-bit input's peak, 2047 x 8 = 16376, uses about
#     15 bits of the 16-bit output without clipping, which keeps the input's SNR.
#   * Stage 2, 17: unit gain.
#   * Stage 3, 17: unit gain. Its filter (7.25 kHz passband, 25 kHz stopband at 50 kSPS, as long
#     as the stage's tap budget allows) is the anti-alias for the receivers' /2 that follows on
#     the PS.
#
# The cascade's 8x (+18 dB) leaves the software receivers' AGC (up to 500x, 54 dB) most of its
# range.
#
# The PS loads the coefficients into the DDC's RAM through `coeff_waddr` / `coeff_wren` /
# `coeff_wdata` (scanner/src/hardware/presets).
#
# SPDX-License-Identifier: MIT
#

from maia_hdl.ddc import DDC


class P25DDC(DDC):
    """A lane's digital down-converter: maia_hdl.ddc.DDC with ``macc_trunc`` [14, 17, 17] for
    unit-DC-gain coefficients (see the module header). Every other argument defaults as in DDC;
    an explicit ``macc_trunc`` behaves as DDC.
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
        # Unit-DC-gain coefficient tables (see the module header).
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

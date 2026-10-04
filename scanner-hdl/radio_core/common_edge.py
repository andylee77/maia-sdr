#
# Fishball radio core - a common-edge pulse for one consumer of an Nx clock.
#
# maia_hdl's ClkNxCommonEdge makes the pulse that marks the Nx cycle after each 1x edge; every
# 3x multiplier's input multiplexers read it. One generator shared by the core drove 368 loads
# across the die, and synthesis merged the consumers' equal copies of it and of the registers
# derived from it, so the core's worst clk3x paths were that pulse's routing. Here each
# consumer (each of the FFT's twiddle multipliers, the integrator, each lane's DDC) has its own
# generator, with `keep` on its registers so that synthesis leaves the copies apart and each is
# placed beside its consumer.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *


class CommonEdge(Elaboratable):
    """ClkNxCommonEdge's pulse, kept apart from every other copy.

    Parameters
    ----------
    domain_1x, domain_nx : str
        The 1x and Nx clock domains.
    n : int
        The Nx clock's ratio to the 1x.
    name : str
        Prefix of the generator's registers (they stay apart under `keep`).

    Attributes
    ----------
    common_edge : Signal(), out
        High in the Nx cycle that begins with a 1x edge, from a register.
    """
    def __init__(self, domain_1x: str, domain_nx: str, n: int, name: str):
        self._1x = domain_1x
        self._nx = domain_nx
        self._n = n
        self._name = name
        self.common_edge = Signal(reset_less=True)

    def elaborate(self, platform):
        m = Module()
        keep = {'keep': 'true'}
        p = self._name
        toggle_1x = Signal(reset_less=True, name=f'{p}_toggle', attrs=keep)
        m.d[self._1x] += toggle_1x.eq(~toggle_1x)
        toggle_1x_q = Signal(reset_less=True, name=f'{p}_toggle_q', attrs=keep)
        m.d[self._nx] += toggle_1x_q.eq(toggle_1x)
        # The edge, combinational; delayed N cycles it comes out of a register in the same phase.
        pulse = toggle_1x ^ toggle_1x_q
        pulse_del = Signal(self._n, reset_less=True, name=f'{p}_pulse', attrs=keep)
        m.d[self._nx] += pulse_del.eq(Cat(pulse, pulse_del[:-1]))
        m.d.comb += self.common_edge.eq(pulse_del[-1])
        return m

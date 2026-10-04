#
# Fishball hwval -- clock census
#
# Block `census` of doc/HW_VALIDATION_SUITE.md section 6.4: measures the
# frequency of every clock that reaches the hwval core (and AD9361
# CLK_OUT, which only arrives as a data pin) by counting edges during a
# gate of known length in the AXI-Lite reference clock.
#

from amaranth import *
from amaranth.lib.cdc import FFSynchronizer


class ClockCensus(Elaboratable):
    """Gated edge counter for a set of clock domains.

    Method: the reference domain raises a gate level for `gate` ref
    cycles. The gate is FFSynchronizer'd into every counted domain, which
    counts its own cycles while its synchronized gate is high (the
    counter restarts at the synchronized gate rising edge). Plain input
    signals listed in `sampled` (e.g. AD9361 CLK_OUT) are synchronized
    into `sampler_domain` and their rising edges are counted during the
    gate synchronized into that domain. Each domain's synchronized gate
    is echoed back to the reference domain: the census waits at least
    `settle_cycles` ref cycles after the gate falls AND until every echo
    has been low for 8 cycles (bounded by `settle_timeout`), so the
    counters are quasi-static when they are sampled. A domain whose echo
    never went high during the measurement (dead clock, or a period
    longer than the gate) reports 0, so a stale count from an earlier
    run is never returned. A clock that stops mid-gate delays completion
    until `settle_timeout` and reports its partial count.

    Accuracy: +-1 count per domain from the gate synchronization
    (+-1 edge for sampled inputs, which also need a high and low time
    longer than one `sampler_domain` period). Frequency is
    ``count / gate_actual * f_ref``. 32-bit counts saturate at
    2**32 - 1.

    Clock domains: the control/status ports are in `ref_domain`; each
    counter runs in its own domain (reset_less, so it keeps working when
    the integrator holds that domain in reset); sampled inputs are
    asynchronous.

    CDC / constraints: the counters are named ``<name>_count_snapshadow``
    and are sampled every ref cycle into ``<name>_count_snapstage``
    registers, which carry the ``amaranth.vivado.false_path = "TRUE"``
    attribute (covered by the maia XDC rule
    ``set_false_path -to [get_cells -hier -filter
    {amaranth.vivado.false_path == "TRUE"}]``). Equivalently
    ``set_false_path -from [get_cells -hier -filter
    {NAME =~ *_snapshadow*}]``. The staged value is only used after the
    settle wait. The gate and echo synchronizers are standard
    FFSynchronizers (same XDC rule).

    Parameters
    ----------
    counted : list of (str, str)
        (name, domain) pairs whose clock cycles are counted in their own
        domain.
    sampled : list of str
        Names of plain input signals whose rising edges are counted by
        sampling them in `sampler_domain`.
    ref_domain : str
        Reference (control) domain; the gate length is in its cycles.
    sampler_domain : str
        Domain used to sample the `sampled` inputs.
    settle_cycles : int
        Minimum ref cycles to wait after the gate falls (>= 16).
    settle_timeout : int
        Maximum ref cycles to wait for all echoes to drop.

    Attributes
    ----------
    start : Signal(), in
        Pulse: start a measurement (ignored while busy).
    gate : Signal(32), in
        Gate length in ref cycles (latched at start; 0 gives all zeros).
    busy : Signal(), out
        Measurement in progress.
    done : Signal(), out
        Measurement complete; stays high until the next start.
    gate_actual : Signal(32), out
        Number of ref cycles the gate was high.
    counts : dict of str -> Signal(32), out
        Edge counts during the gate, for every counted and sampled name.
        Updated when done rises.
    inputs : dict of str -> Signal(), in
        One asynchronous input per sampled name.
    """
    QUIET_CYCLES = 8

    def __init__(self, counted, sampled=(), ref_domain='s_axi_lite',
                 sampler_domain='clk3x', settle_cycles=256,
                 settle_timeout=2**16):
        self.counted = list(counted)
        self.sampled = list(sampled)
        self._ref = ref_domain
        self._sampler = sampler_domain
        if settle_cycles < 16:
            raise ValueError('settle_cycles must be >= 16')
        if settle_timeout < settle_cycles:
            raise ValueError('settle_timeout must be >= settle_cycles')
        self.settle_cycles = settle_cycles
        self.settle_timeout = settle_timeout
        names = [name for name, _ in self.counted] + self.sampled
        if len(set(names)) != len(names):
            raise ValueError('duplicate census names')
        self.names = names

        self.start = Signal()
        self.gate = Signal(32)
        self.busy = Signal()
        self.done = Signal()
        self.gate_actual = Signal(32)
        self.counts = {name: Signal(32, name=f'count_{name}')
                       for name in names}
        self.inputs = {name: Signal(name=f'{name}_in')
                       for name in self.sampled}

    def elaborate(self, platform):
        m = Module()
        ref = self._ref

        gate_level = Signal()
        gate_len = Signal(32)
        gate_cnt = Signal(32)

        # Per-name shadow counter and echo; echo_list has each echo once.
        shadows = {}
        echoes = {}
        echo_list = []

        def gate_sync(tag, domain):
            g = Signal(name=f'{tag}_gate', reset_less=True)
            g_d = Signal(name=f'{tag}_gate_d', reset_less=True)
            m.submodules[f'{tag}_gate_sync'] = FFSynchronizer(
                gate_level, g, o_domain=domain)
            m.d[domain] += g_d.eq(g)
            echo = Signal(name=f'{tag}_echo')
            m.submodules[f'{tag}_echo_sync'] = FFSynchronizer(
                g, echo, o_domain=ref)
            echo_list.append(echo)
            return g, g_d, echo

        for name, domain in self.counted:
            g, g_d, echo = gate_sync(name, domain)
            cnt = Signal(33, name=f'{name}_count_snapshadow',
                         reset_less=True)
            with m.If(g & ~g_d):
                m.d[domain] += cnt.eq(1)
            with m.Elif(g & ~cnt[-1]):
                m.d[domain] += cnt.eq(cnt + 1)
            shadows[name] = cnt
            echoes[name] = echo

        if self.sampled:
            gs, gs_d, echo = gate_sync('sampler', self._sampler)
            for name in self.sampled:
                x = Signal(name=f'{name}_sampled', reset_less=True)
                x_d = Signal(name=f'{name}_sampled_d', reset_less=True)
                m.submodules[f'{name}_input_sync'] = FFSynchronizer(
                    self.inputs[name], x, o_domain=self._sampler)
                m.d[self._sampler] += x_d.eq(x)
                rise = x & ~x_d
                cnt = Signal(33, name=f'{name}_count_snapshadow',
                             reset_less=True)
                with m.If(gs & ~gs_d):
                    m.d[self._sampler] += cnt.eq(rise)
                with m.Elif(gs & rise & ~cnt[-1]):
                    m.d[self._sampler] += cnt.eq(cnt + 1)
                shadows[name] = cnt
                echoes[name] = echo

        # Quasi-static sampling of the counters into the ref domain.
        stages = {}
        for name in self.names:
            stage = Signal(33, name=f'{name}_count_snapstage',
                           reset_less=True,
                           attrs={'amaranth.vivado.false_path': 'TRUE'})
            m.d[ref] += stage.eq(shadows[name])
            stages[name] = stage

        any_echo = Signal()
        m.d.comb += any_echo.eq(Cat(*echo_list).any() if echo_list else 0)
        seen = {name: Signal(name=f'{name}_seen') for name in self.names}

        settle_cnt = Signal(range(self.settle_timeout + 1))
        quiet = Signal(range(self.QUIET_CYCLES + 1))

        with m.FSM(domain=ref):
            with m.State('IDLE'):
                with m.If(self.start):
                    m.d[ref] += [
                        gate_len.eq(self.gate),
                        gate_cnt.eq(0),
                        self.busy.eq(1),
                        self.done.eq(0),
                    ]
                    m.d[ref] += [s.eq(0) for s in seen.values()]
                    m.next = 'GATE'
            with m.State('GATE'):
                with m.If(gate_cnt == gate_len):
                    m.d[ref] += [
                        gate_level.eq(0),
                        settle_cnt.eq(0),
                        quiet.eq(0),
                    ]
                    m.next = 'SETTLE'
                with m.Else():
                    m.d[ref] += [
                        gate_level.eq(1),
                        gate_cnt.eq(gate_cnt + 1),
                    ]
                m.d[ref] += [seen[name].eq(seen[name] | echoes[name])
                             for name in self.names]
            with m.State('SETTLE'):
                m.d[ref] += [seen[name].eq(seen[name] | echoes[name])
                             for name in self.names]
                with m.If(settle_cnt != self.settle_timeout):
                    m.d[ref] += settle_cnt.eq(settle_cnt + 1)
                with m.If(any_echo):
                    m.d[ref] += quiet.eq(0)
                with m.Elif(quiet != self.QUIET_CYCLES):
                    m.d[ref] += quiet.eq(quiet + 1)
                with m.If(((settle_cnt >= self.settle_cycles)
                           & (quiet == self.QUIET_CYCLES))
                          | (settle_cnt == self.settle_timeout)):
                    m.next = 'CAPTURE'
            with m.State('CAPTURE'):
                for name in self.names:
                    stage = stages[name]
                    value = Mux(stage[-1], 2**32 - 1, stage[:32])
                    m.d[ref] += self.counts[name].eq(
                        Mux(seen[name], value, 0))
                m.d[ref] += [
                    self.gate_actual.eq(gate_cnt),
                    self.busy.eq(0),
                    self.done.eq(1),
                ]
                m.next = 'IDLE'

        return m

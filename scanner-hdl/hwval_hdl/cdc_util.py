#
# Fishball hardware validation (hwval) - CDC helpers
#
# All hwval registers live in the AXI-Lite clock domain (see
# doc/HW_VALIDATION_SUITE.md section 6.3). These helpers move values in
# and out of the other clock domains without ever making an AXI-Lite
# transaction wait for another clock: if a source clock is dead, a
# snapshot simply never acknowledges and the PS times out.
#
# XDC: the multi-bit data lanes crossed by these helpers are plain
# registers (not FFSynchronizer chains), qualified by a toggle handshake.
# Their names carry a fixed suffix so the constraints can waive them:
#
#   set_false_path -from [get_cells -hier -filter {NAME =~ *_snapshadow_reg*}]
#   set_false_path -from [get_cells -hier -filter {NAME =~ *_cdchold_reg*}]
#
# (ConfigSync / DomainCrossing hold registers are *_cdchold; Snapshot
# shadows are *_snapshadow.)
#
# The handshake toggles and GrayCounterSync go through FFSynchronizer
# chains, which get ASYNC_REG (and amaranth.vivado.false_path on the
# first stage) from the platform.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *
from amaranth.lib.cdc import FFSynchronizer, PulseSynchronizer
import amaranth.back.verilog

from typing import Dict


class Snapshot(Elaboratable):
    """Snapshot of a group of source-domain signals.

    On a request (a pulse in ``req_domain``) the source domain latches all
    the signals into shadow registers (named ``<key>_snapshadow``) and
    acknowledges through a toggle crossed back to ``req_domain``. The
    shadows are then quasi-static and can be read directly by the register
    multiplexer in ``req_domain`` once ``ack`` is asserted.

    The handshake is level based (request toggle / acknowledge toggle), so
    it works with any ratio of clock frequencies. A request made while a
    previous one is still in flight is queued and issued when the previous
    one completes; ``ack`` only rises once the shadows hold values latched
    after the most recent request. If the source clock is dead, ``ack``
    stays low forever and nothing else is affected.

    The source-side registers are reset-less, so a snapshot works (and the
    shadows keep their values) while the source domain is held in reset.

    Parameters
    ----------
    src_domain : str
        Domain of the signals to snapshot.
    signals : Dict[str, Value]
        Signals to snapshot, keyed by a name used for the shadow register.
    req_domain : str
        Domain of the requester (the AXI-Lite domain).
    latch_delay : int
        Number of source-domain cycles between the ``trigger`` pulse and the
        latching of the shadows. Use it when ``trigger`` makes the source
        update the values to be captured (e.g. a statistics window freeze).
    stages : int
        Synchronizer stages.

    Attributes
    ----------
    req : Signal(), in (req_domain)
        Request pulse.
    hold_off : Signal(), in (req_domain)
        While asserted, a queued request is not issued. Tie it to the
        ``busy`` of the configuration crossings into ``src_domain`` so that
        configuration written before ``req`` (e.g. a histogram bin select)
        has reached the source before the snapshot is taken.
    ack : Signal(), out (req_domain)
        Asserted when a snapshot requested by the last ``req`` completed.
        Cleared by ``req``.
    busy : Signal(), out (req_domain)
        A request is queued or in flight.
    seq : Signal(32), out (req_domain)
        Number of acknowledged snapshots (wraps). Requests merged while
        queued count once.
    trigger : Signal(), out (src_domain)
        One-cycle pulse when the source domain starts serving a request.
    shadow : Dict[str, Signal]
        Shadow registers (src_domain registers, quasi-static).
    """
    def __init__(self, src_domain: str, signals: Dict[str, Value], *,
                 req_domain: str = 's_axi_lite', latch_delay: int = 0,
                 stages: int = 2):
        if latch_delay < 0:
            raise ValueError('latch_delay must be >= 0')
        self._src = src_domain
        self._req = req_domain
        self._latch_delay = latch_delay
        self._stages = stages
        self.signals = {k: Value.cast(v) for k, v in signals.items()}

        self.req = Signal()
        self.hold_off = Signal()
        self.ack = Signal()
        self.busy = Signal()
        self.seq = Signal(32)
        self.trigger = Signal()
        self.shadow = {
            k: Signal(v.shape(), name=f'{k}_snapshadow', reset_less=True)
            for k, v in self.signals.items()}

    def elaborate(self, platform):
        m = Module()
        req = m.d[self._req]
        src = m.d[self._src]

        # ── Request domain ────────────────────────────────────
        req_t = Signal()
        ack_t = Signal(reset_less=True)       # src domain
        ack_t_s = Signal()                    # req domain
        req_t_s = Signal()                    # src domain
        m.submodules.ack_sync = FFSynchronizer(
            ack_t, ack_t_s, o_domain=self._req, stages=self._stages)
        m.submodules.req_sync = FFSynchronizer(
            req_t, req_t_s, o_domain=self._src, stages=self._stages)

        want = Signal()
        outstanding = Signal()
        outstanding_q = Signal()
        issue = Signal()
        m.d.comb += [
            outstanding.eq(req_t != ack_t_s),
            issue.eq(want & ~outstanding & ~self.hold_off),
            self.busy.eq(want | outstanding),
        ]
        req += outstanding_q.eq(outstanding)
        with m.If(issue):
            req += req_t.eq(~req_t)
        with m.If(self.req):
            req += [
                want.eq(1),
                self.ack.eq(0),
            ]
        with m.Else():
            with m.If(issue):
                req += want.eq(0)
            with m.If(outstanding_q & ~outstanding & ~want):
                req += [
                    self.ack.eq(1),
                    self.seq.eq(self.seq + 1),
                ]

        # ── Source domain ─────────────────────────────────────
        pending = Signal()
        m.d.comb += pending.eq(req_t_s != ack_t)
        latch = Signal()
        if self._latch_delay == 0:
            m.d.comb += [
                self.trigger.eq(pending),
                latch.eq(pending),
            ]
        else:
            count = Signal(range(self._latch_delay + 1), reset_less=True)
            serving = Signal(reset_less=True)
            m.d.comb += [
                self.trigger.eq(pending & ~serving),
                latch.eq(serving & (count == 0)),
            ]
            with m.If(self.trigger):
                src += [
                    serving.eq(1),
                    count.eq(self._latch_delay - 1),
                ]
            with m.Elif(serving):
                with m.If(count == 0):
                    src += serving.eq(0)
                with m.Else():
                    src += count.eq(count - 1)
        with m.If(latch):
            src += ack_t.eq(req_t_s)
            for k, v in self.signals.items():
                src += self.shadow[k].eq(v)

        return m


class GrayCounterSync(Elaboratable):
    """Monotonic counter crossing through Gray code.

    The binary counter in ``i_domain`` is registered as Gray code, passed
    through an FFSynchronizer and converted back to binary (registered) in
    ``o_domain``. The input must change by at most one step per
    ``i_domain`` cycle (a counter), which makes every sampled value either
    the old or the new count. Used for ``RINGV2_COMMITTED_BURSTS``, which
    the streaming reader polls without a snapshot.

    Attributes
    ----------
    i : Signal(width), in (i_domain)
        Binary count.
    o : Signal(width), out (o_domain)
        Synchronized binary count (latency: 1 + stages + 1 cycles).
    """
    def __init__(self, i_domain: str, o_domain: str, width: int = 32,
                 stages: int = 2):
        self._i_domain = i_domain
        self._o_domain = o_domain
        self.w = width
        self._stages = stages
        self.i = Signal(width)
        self.o = Signal(width)

    def elaborate(self, platform):
        m = Module()
        gray = Signal(self.w, name='gray_src')
        gray_s = Signal(self.w, name='gray_dst')
        m.d[self._i_domain] += gray.eq(self.i ^ (self.i >> 1))
        m.submodules.gray_sync = FFSynchronizer(
            gray, gray_s, o_domain=self._o_domain, stages=self._stages)
        binary = Signal(self.w)
        for k in range(self.w):
            # b[k] = XOR of g[k:]
            m.d.comb += binary[k].eq(gray_s[k:].xor())
        m.d[self._o_domain] += self.o.eq(binary)
        return m


class ConfigSync(Elaboratable):
    """Coherent multi-bit value crossing (handshake, tracks the input).

    Whenever the handshake is idle and the input differs from the last
    value sent, the value is copied into a hold register (``*_cdchold``)
    and a request toggle is flipped. The output domain copies the hold
    register when it sees the toggle and acknowledges. The output is
    therefore always a value that the input really had (never a mix of old
    and new bits), whatever the timing of the writes; the latest input
    value always wins. All registers are reset-less so resets in either
    domain never make the two sides disagree (the input is re-sent if it
    changed). A dead output clock only leaves the output stale.

    Attributes
    ----------
    i : Signal(width), in (i_domain)
    o : Signal(width), out (o_domain), initialized to ``init``
    hold : Signal(width), (i_domain) last value sent
    busy : Signal(), out (i_domain)
        A transfer is in flight or the input differs from the last value
        sent.
    """
    def __init__(self, i_domain: str, o_domain: str, width: int, *,
                 init: int = 0, name: str = 'cfg', stages: int = 2):
        self._i_domain = i_domain
        self._o_domain = o_domain
        self.w = width
        self._init = init
        self._name = name
        self._stages = stages
        self.i = Signal(width, init=init)
        self.o = Signal(width, init=init, reset_less=True)
        self.hold = Signal(width, init=init, reset_less=True,
                           name=f'{name}_cdchold')
        self.busy = Signal()

    def elaborate(self, platform):
        m = Module()
        hold = self.hold
        req_t = Signal(reset_less=True)
        ack_t = Signal(reset_less=True)
        req_t_s = Signal()
        ack_t_s = Signal()
        m.submodules.req_sync = FFSynchronizer(
            req_t, req_t_s, o_domain=self._o_domain, stages=self._stages)
        m.submodules.ack_sync = FFSynchronizer(
            ack_t, ack_t_s, o_domain=self._i_domain, stages=self._stages)
        idle = Signal()
        m.d.comb += [
            idle.eq(req_t == ack_t_s),
            self.busy.eq(~idle | (self.i != hold)),
        ]
        with m.If(idle & (self.i != hold)):
            m.d[self._i_domain] += [
                hold.eq(self.i),
                req_t.eq(~req_t),
            ]
        with m.If(req_t_s != ack_t):
            m.d[self._o_domain] += [
                self.o.eq(hold),
                ack_t.eq(req_t_s),
            ]
        return m


class DomainCrossing(Elaboratable):
    """Ordered crossing of all the configuration and commands of a domain.

    Every configuration value and every command bit that goes from
    ``i_domain`` to ``o_domain`` is carried by one wide ``ConfigSync``
    (hold register ``<name>_cdchold``). Commands are converted to toggles
    in ``i_domain`` (reset-less, one per command bit) that travel inside
    the same vector; ``o_domain`` turns toggle changes back into one-cycle
    pulses, registered, so they appear one cycle after the configuration
    that travelled with them.

    Because each transfer is a snapshot of the whole source vector taken
    at one instant, a write is never delivered in a transfer earlier than
    one carrying a write that preceded it in the source. Writes made while
    a transfer is in flight are merged into the next transfer; within one
    transfer the configuration is applied first and the commands one
    cycle later. So configuration written before a command (e.g. a memory
    tester setup, then start) is always in place when the command
    arrives, and a command written before a configuration change (e.g.
    clear, then enable) arrives in the same transfer or an earlier one
    (in the same transfer it lands one cycle after the configuration).
    A command bit written again before its previous toggle was sent is
    merged (never lost as an even number of toggles). A dead ``o_domain``
    clock leaves everything pending without affecting ``i_domain``.

    Use ``config()`` and ``command()`` (before elaboration) to add items.

    Attributes
    ----------
    busy : Signal(), out (i_domain)
        Something written in ``i_domain`` has not been acknowledged by
        ``o_domain`` yet. Tie a ``Snapshot.hold_off`` to it so that a
        snapshot reflects every earlier write.
    """
    def __init__(self, i_domain: str, o_domain: str, *, name: str,
                 stages: int = 2):
        self._i_domain = i_domain
        self._o_domain = o_domain
        self._name = name
        self._stages = stages
        self._configs = []
        self._commands = []
        self.busy = Signal(name=f'{name}_busy')

    def config(self, value: Value, *, init: int = 0,
               name: str = 'cfg') -> Signal:
        """Add a configuration value. Returns its ``o_domain`` copy."""
        value = Value.cast(value)
        o = Signal(len(value), init=init, reset_less=True,
                   name=f'{name}_{self._o_domain}')
        self._configs.append((value, o, init))
        return o

    def command(self, value: Value, stb: Value, *,
                name: str = 'cmd') -> Signal:
        """Add a command register: ``stb`` is the write strobe and
        ``value`` the written bits (a bit set = command issued). Returns
        the ``o_domain`` one-cycle pulses."""
        value = Value.cast(value)
        o = Signal(len(value), name=f'{name}_{self._o_domain}')
        self._commands.append((value, Value.cast(stb), o))
        return o

    def elaborate(self, platform):
        m = Module()
        isd = m.d[self._i_domain]
        osd = m.d[self._o_domain]
        i_parts = []
        o_parts = []
        init = 0
        width = 0
        for value, o, init_v in self._configs:
            i_parts.append(value)
            o_parts.append(o)
            init |= init_v << width
            width += len(value)
        toggles = []
        for value, stb, o in self._commands:
            tog = Signal(len(value), reset_less=True)
            toggles.append((tog, width, value, stb, o))
            i_parts.append(tog)
            width += len(value)
        if width == 0:
            return m
        m.submodules.sync = cs = ConfigSync(
            self._i_domain, self._o_domain, width, init=init,
            name=self._name, stages=self._stages)
        m.d.comb += [
            cs.i.eq(Cat(*i_parts)),
            self.busy.eq(cs.busy),
        ]
        offset = 0
        for value, o, _ in self._configs:
            m.d.comb += o.eq(cs.o[offset:offset + len(value)])
            offset += len(value)
        for tog, off, value, stb, o in toggles:
            w = len(value)
            sent = cs.hold[off:off + w]
            # flip only the bits whose previous toggle was already sent
            with m.If(stb):
                isd += tog.eq(tog ^ (value & ~(tog ^ sent)))
            recv = cs.o[off:off + w]
            prev = Signal(w, reset_less=True)
            osd += [
                prev.eq(recv),
                o.eq(recv ^ prev),
            ]
        return m


def ff_sync(m: Module, value: Value, o_domain: str, *, name: str,
            init: int = 0, stages: int = 2) -> Signal:
    """Cross a quasi-static value or independent status bits with an
    FFSynchronizer per bit. Returns the synchronized signal."""
    value = Value.cast(value)
    o = Signal(value.shape(), name=f'{name}_s', init=init)
    m.submodules[f'{name}_ffsync'] = FFSynchronizer(
        value, o, o_domain=o_domain, init=init, stages=stages)
    return o


def pulse_sync(m: Module, pulse: Value, i_domain: str, o_domain: str, *,
               name: str, stages: int = 2) -> Signal:
    """Cross a single-cycle pulse. Returns the o_domain pulse."""
    ps = PulseSynchronizer(i_domain, o_domain, stages=stages)
    m.submodules[f'{name}_psync'] = ps
    m.d.comb += ps.i.eq(pulse)
    return ps.o


def config_sync(m: Module, value: Value, i_domain: str, o_domain: str, *,
                name: str, init: int = 0) -> ConfigSync:
    """Cross a (multi-bit) configuration value coherently with
    ``ConfigSync``. Returns the ConfigSync (use ``.o`` and ``.busy``)."""
    value = Value.cast(value)
    cs = ConfigSync(i_domain, o_domain, len(value), init=init, name=name)
    m.submodules[f'{name}_cfgsync'] = cs
    m.d.comb += cs.i.eq(value)
    return cs


if __name__ == '__main__':
    m = Module()
    m.domains.src = ClockDomain()
    m.domains.dst = ClockDomain()
    a = Signal(32)
    m.submodules.snap = snap = Snapshot('src', {'a': a}, req_domain='dst')
    print(amaranth.back.verilog.convert(
        m, ports=[a, snap.req, snap.ack, snap.shadow['a']]))

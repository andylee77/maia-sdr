#
# Fishball hwval - AXI3 write-subordinate simulation model
#
# Used by the ring writer tests. Implements a memory behind an AXI3 write
# port with randomized AWREADY/WREADY and write-response latency, optional
# BRESP error injection, a record of every burst, and checks of the AXI
# rules that the ring writers must follow.
#
# SPDX-License-Identifier: MIT
#

import random


OKAY = 0
EXOKAY = 1
SLVERR = 2
DECERR = 3


class AxiProtocolError(AssertionError):
    pass


class AxiWriteSlaveModel:
    """AXI3 write subordinate for Amaranth simulations.

    Add ``model.bench`` as a background testbench::

        sim.add_testbench(model.bench, background=True)

    The model samples the manager's signals at every clock edge
    (``ctx.tick().sample(...)``), so handshakes are evaluated exactly as a
    real subordinate sees them regardless of testbench ordering, and then
    drives ``awready``, ``wready``, ``bvalid`` and ``bresp`` for the next
    cycle.

    Behaviour knobs (plain attributes, may be changed while running):

    - ``awready_prob``, ``wready_prob``: probability of READY per cycle.
    - ``awready_fn``, ``wready_fn``: optional ``callable(model) -> bool``
      that overrides the probability; ``model.cycle`` is then the index
      of the edge at which the READY value will be sampled.
    - ``b_latency``: ``(lo, hi)`` uniform, or ``callable(rec, model)``,
      cycles from burst completion (the later of the AW handshake and the
      WLAST handshake) to BVALID. Responses are always returned in order.
    - ``b_hold``: while True no new BVALID is presented (models a stalled
      interconnect).
    - ``bresp``: None (all OKAY), a set of burst indices that get SLVERR,
      or ``callable(index) -> resp``.

    Checks (raise ``AxiProtocolError`` when ``strict``, else append a
    string to ``violations``):

    - AWVALID/WVALID stay high until the handshake and the payload
      (AW: addr, len, size, burst, cache, prot, lock; W: data, strb,
      last) is stable while waiting.
    - WLAST is asserted on beat ``awlen`` of each burst and only there.
    - AWSIZE equals the bus width, AWBURST is INCR, the start address is
      size-aligned and the burst does not cross a 4 KiB boundary.
    - A B handshake only happens for a presented response.

    Records:

    - ``mem``: dict byte address (8-byte aligned) -> 64-bit word.
    - ``bursts``: one dict per completed burst in order, with ``index``,
      ``addr``, ``len``, ``data`` (list of words), ``strb``, ``aw_cycle``,
      ``wlast_cycle``, ``b_due``, ``b_cycle`` (None until responded) and
      ``bresp``.
    - counters ``aw_count``, ``w_beats``, ``b_count``, ``outstanding``
      (AW accepted, no B yet) and ``outstanding_max``.
    """
    def __init__(self, axi, *, seed=0, awready_prob=1.0, wready_prob=1.0,
                 b_latency=(1, 1), bresp=None, strict=True, data_bytes=8):
        self.axi = axi
        self.rng = random.Random(seed)
        self.awready_prob = awready_prob
        self.wready_prob = wready_prob
        self.awready_fn = None
        self.wready_fn = None
        self.b_latency = b_latency
        self.b_hold = False
        self.bresp = bresp
        self.strict = strict
        self.data_bytes = data_bytes
        self.size_log2 = data_bytes.bit_length() - 1

        self.mem = {}
        self.bursts = []
        self.violations = []
        self.cycle = 0
        self.aw_count = 0
        self.w_beats = 0
        self.b_count = 0
        self.outstanding = 0
        self.outstanding_max = 0

        self._aw_queue = []   # accepted AWs waiting for their W burst
        self._w_queue = []    # complete W bursts waiting for their AW
        self._cur_w = []      # beats of the W burst in progress
        self._b_queue = []    # completed bursts waiting for B, in order
        self._b_active = None
        self._aw_hold = None  # payload of an AW waiting for AWREADY
        self._w_hold = None
        self._w_burst_index = 0

    # ---- helpers for tests ----

    def violation(self, msg):
        msg = f'cycle {self.cycle}: {msg}'
        if self.strict:
            raise AxiProtocolError(msg)
        self.violations.append(msg)

    def stream(self):
        """Data of all completed bursts, in burst order."""
        return [w for b in self.bursts for w in b['data']]

    def responded(self):
        """Bursts whose B handshake has happened."""
        return [b for b in self.bursts if b['b_cycle'] is not None]

    def open_w_beats(self):
        """Beats of a W burst that has not seen WLAST yet."""
        return len(self._cur_w)

    # ---- simulation ----

    async def bench(self, ctx):
        a = self.axi
        sampled = (
            a.awvalid, a.awready, a.awaddr, a.awlen, a.awsize, a.awburst,
            a.awcache, a.awprot, a.awlock,
            a.wvalid, a.wready, a.wdata, a.wstrb, a.wlast,
            a.bvalid, a.bready,
        )
        self._drive(ctx)
        async for _clk, rst, *v in ctx.tick().sample(*sampled):
            if not rst:
                self._edge(*v)
            self.cycle += 1
            self._drive(ctx)

    def _edge(self, awvalid, awready, awaddr, awlen, awsize, awburst,
              awcache, awprot, awlock, wvalid, wready, wdata, wstrb, wlast,
              bvalid, bready):
        # AW channel
        aw = (awaddr, awlen, awsize, awburst, awcache, awprot, awlock)
        if self._aw_hold is not None:
            if not awvalid:
                self.violation(
                    f'AWVALID dropped without handshake (awaddr '
                    f'{self._aw_hold[0]:#010x})')
            elif aw != self._aw_hold:
                self.violation(
                    f'AW payload changed while waiting: {self._aw_hold} '
                    f'-> {aw}')
        self._aw_hold = aw if (awvalid and not awready) else None
        if awvalid and awready:
            self._on_aw(aw)

        # W channel
        w = (wdata, wstrb, wlast)
        if self._w_hold is not None:
            if not wvalid:
                self.violation('WVALID dropped without handshake')
            elif w != self._w_hold:
                self.violation(
                    f'W payload changed while waiting: wdata '
                    f'{self._w_hold[0]:#018x} -> {wdata:#018x}')
        self._w_hold = w if (wvalid and not wready) else None
        if wvalid and wready:
            self._on_w(wdata, wstrb, wlast)

        # B channel (driven by us)
        if bvalid and bready:
            if self._b_active is None:
                self.violation('B handshake without a presented response')
            else:
                rec = self._b_active
                rec['b_cycle'] = self.cycle
                self._b_active = None
                self.b_count += 1
                self.outstanding -= 1

    def _on_aw(self, aw):
        addr, alen, size, burst, cache, prot, lock = aw
        rec = dict(index=self.aw_count, addr=addr, len=alen, size=size,
                   burst=burst, cache=cache, prot=prot, lock=lock,
                   aw_cycle=self.cycle, data=None, strb=None,
                   wlast_cycle=None, b_due=None, b_cycle=None, bresp=None)
        if size != self.size_log2:
            self.violation(f'AWSIZE {size} != {self.size_log2}')
        if burst != 1:
            self.violation(f'AWBURST {burst} is not INCR')
        nbytes = (alen + 1) << size
        if addr % (1 << size):
            self.violation(f'unaligned AWADDR {addr:#010x}')
        if (addr & 0xFFF) + nbytes > 0x1000:
            self.violation(
                f'burst at {addr:#010x} len {alen + 1} crosses 4 KiB')
        self.aw_count += 1
        self.outstanding += 1
        self.outstanding_max = max(self.outstanding_max, self.outstanding)
        self._aw_queue.append(rec)
        self._pair()

    def _on_w(self, wdata, wstrb, wlast):
        self.w_beats += 1
        beat = len(self._cur_w)
        self._cur_w.append((wdata, wstrb, self.cycle))
        # WLAST position check when the matching AW is already known
        pending = len(self._w_queue)
        if pending < len(self._aw_queue):
            alen = self._aw_queue[pending]['len']
            if bool(wlast) != (beat == alen):
                self.violation(
                    f'WLAST={wlast} on beat {beat} of a len={alen + 1} '
                    f'burst (AW index {self._aw_queue[pending]["index"]})')
        if len(self._cur_w) > 16:
            self.violation('W burst longer than 16 beats without WLAST')
        if wlast:
            self._w_queue.append(self._cur_w)
            self._cur_w = []
            self._pair()

    def _pair(self):
        while self._aw_queue and self._w_queue:
            rec = self._aw_queue.pop(0)
            beats = self._w_queue.pop(0)
            if len(beats) != rec['len'] + 1:
                self.violation(
                    f'W burst of {len(beats)} beats for AW len '
                    f'{rec["len"] + 1} at {rec["addr"]:#010x}')
            rec['data'] = [b[0] for b in beats]
            rec['strb'] = [b[1] for b in beats]
            rec['wlast_cycle'] = beats[-1][2]
            for i, (data, strb, _) in enumerate(beats):
                addr = rec['addr'] + i * self.data_bytes
                old = self.mem.get(addr, 0)
                new = old
                for byte in range(self.data_bytes):
                    if (strb >> byte) & 1:
                        mask = 0xFF << (8 * byte)
                        new = (new & ~mask) | (data & mask)
                self.mem[addr] = new
            if callable(self.b_latency):
                lat = self.b_latency(rec, self)
            else:
                lat = self.rng.randint(*self.b_latency)
            done = max(rec['aw_cycle'], rec['wlast_cycle'])
            rec['b_due'] = done + max(1, lat)
            if self.bresp is None:
                rec['bresp'] = OKAY
            elif callable(self.bresp):
                rec['bresp'] = self.bresp(rec['index'])
            else:
                rec['bresp'] = SLVERR if rec['index'] in self.bresp else OKAY
            self.bursts.append(rec)
            self._b_queue.append(rec)

    def _drive(self, ctx):
        a = self.axi
        rnd = self.rng.random
        if self.awready_fn is not None:
            awready = self.awready_fn(self)
        else:
            awready = rnd() < self.awready_prob
        if self.wready_fn is not None:
            wready = self.wready_fn(self)
        else:
            wready = rnd() < self.wready_prob
        ctx.set(a.awready, int(awready))
        ctx.set(a.wready, int(wready))
        if self._b_active is None:
            if (self._b_queue and not self.b_hold
                    and self._b_queue[0]['b_due'] <= self.cycle):
                self._b_active = self._b_queue.pop(0)
                ctx.set(a.bresp, self._b_active['bresp'])
                ctx.set(a.bvalid, 1)
            else:
                ctx.set(a.bvalid, 0)

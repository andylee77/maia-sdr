#
# Fishball hwval - AXI3 read/write subordinate model for simulation
#
# Dict-backed byte memory behind an AXI3 subordinate port with randomized
# handshake latencies, multiple outstanding reads (in-order R), optional
# error injection and AXI rule checks on the manager (DUT) side.
#
# SPDX-License-Identifier: MIT
#

import collections
import random


OKAY = 0b00
SLVERR = 0b10


class AxiProtocolError(AssertionError):
    pass


class AxiRWModel:
    """AXI3 read/write subordinate model.

    Run :meth:`process` as a background testbench
    (``sim.add_testbench(model.process, background=True)``). Each cycle the
    model drives its ready/valid signals, samples the manager's signals at
    the clock edge, applies the handshakes and checks:

    - AWVALID/WVALID/ARVALID are held until the handshake with a stable
      payload;
    - AxSIZE = 3, AxBURST = INCR, AxCACHE = ``expect_cache``, 8-byte aligned
      addresses, bursts do not cross 4 KiB and (if ``window`` is given) lie
      inside ``[window[0], window[1])``;
    - WLAST is asserted exactly on the last beat of each burst;
    - at most ``max_outstanding`` bursts per direction (if given).

    Writes honour WSTRB. Write bursts are matched to AW in order (W may
    precede AW). B is returned in order, ``b_delay`` cycles (random range)
    after both the AW and the last W beat were accepted. Reads are accepted
    while fewer than ``max_reads`` are pending and return in order; the
    first beat comes ``r_delay`` cycles after AR and every beat is presented
    with probability ``p_rbeat`` per cycle. With ``zero_latency=True`` all
    ready signals are constantly high, B comes the cycle after WLAST and
    read data starts the cycle after AR with no gaps.

    Error injection: ``read_flips[beat_addr] = mask`` XORs the read data of
    that beat, ``bresp_err_addrs`` holds burst start addresses answered
    with SLVERR (the data is still written), ``rresp_err_addrs`` holds beat
    addresses answered with SLVERR (the data is still returned) and
    ``wstrb_stuck_on`` is ORed into every WSTRB (a stuck-enabled DM line).

    With ``forbid_w_before_aw`` a W beat accepted before the AWVALID of its
    burst was asserted is a failure (W overtaking the AW handshake is legal
    and supported; ``w_early_beats`` counts such beats). ``b_log`` and
    ``rlast_log`` record the cycles of the B and RLAST handshakes
    (``aw_log``/``ar_log`` hold ``(cycle, addr, len)``).
    """
    def __init__(self, axi_if, *, seed=0, zero_latency=False,
                 p_awready=0.5, p_wready=0.7, p_arready=0.5,
                 b_delay=(1, 20), r_delay=(2, 24), p_rbeat=0.7,
                 max_reads=8, window=None, max_outstanding=None,
                 expect_cache=0b0011, fill=None, wstrb_stuck_on=0,
                 forbid_w_before_aw=False):
        self.a = axi_if
        self.rng = random.Random(seed)
        self.zl = zero_latency
        self.p_awready = p_awready
        self.p_wready = p_wready
        self.p_arready = p_arready
        self.b_delay = b_delay
        self.r_delay = r_delay
        self.p_rbeat = p_rbeat
        self.max_reads = max_reads
        self.window = window
        self.max_outstanding = max_outstanding
        self.expect_cache = expect_cache
        self.wstrb_stuck_on = wstrb_stuck_on
        self.forbid_w_before_aw = forbid_w_before_aw
        # fill(byte_address) -> byte for never-written memory
        self.fill = fill if fill is not None else (lambda addr: 0)

        self.mem = {}
        self.read_flips = {}
        self.bresp_err_addrs = set()
        self.rresp_err_addrs = set()

        self.cycle = 0
        self.awq = collections.deque()    # accepted AW waiting for W data
        self.wq = collections.deque()     # accepted W beats not yet matched
        self.bq = collections.deque()     # (ready_cycle, resp)
        self.b_cur = None
        self.rq = collections.deque()     # [addr, len, next_beat, ready]
        self.r_cur = None                 # (data, resp, last)
        self.r_last_ready = 0
        self.b_last_ready = 0

        # statistics
        self.aw_log = []
        self.ar_log = []
        self.w_beats = 0
        self.w_early_beats = 0
        self.r_beats = 0
        self.b_count = 0
        self.b_log = []
        self.rlast_log = []
        self.w_out = 0
        self.r_out = 0
        self.w_out_max = 0
        self.r_out_max = 0

    # -- memory helpers ----------------------------------------------------

    def write_beat(self, addr, data, strb=0xFF):
        for i in range(8):
            if (strb >> i) & 1:
                self.mem[addr + i] = (data >> (8 * i)) & 0xFF

    def read_beat(self, addr):
        v = 0
        for i in range(8):
            b = self.mem.get(addr + i)
            if b is None:
                b = self.fill(addr + i) & 0xFF
            v |= b << (8 * i)
        return v

    def fill_beats(self, base, size, fn):
        """Preload ``fn(beat_addr)`` into ``[base, base + size)``."""
        for addr in range(base, base + size, 8):
            self.write_beat(addr, fn(addr))

    def idle(self):
        """True when no transaction is in progress."""
        return not (self.awq or self.wq or self.bq or self.b_cur
                    or self.rq or self.r_cur)

    # -- checks ------------------------------------------------------------

    def _fail(self, msg):
        raise AxiProtocolError(f'cycle {self.cycle}: {msg}')

    def _check_addr(self, kind, addr, length, size, burst, cache):
        nbytes = (length + 1) * 8
        if size != 3:
            self._fail(f'{kind} size {size} != 3')
        if burst != 0b01:
            self._fail(f'{kind} burst {burst} != INCR')
        if cache != self.expect_cache:
            self._fail(f'{kind} cache {cache:#x} != {self.expect_cache:#x}')
        if addr % 8:
            self._fail(f'{kind} addr {addr:#x} not 8-byte aligned')
        if (addr % 4096) + nbytes > 4096:
            self._fail(f'{kind} burst {addr:#x}+{nbytes} crosses 4 KiB')
        if self.window is not None:
            lo, hi = self.window
            if addr < lo or addr + nbytes > hi:
                self._fail(f'{kind} burst {addr:#x}+{nbytes} outside '
                           f'[{lo:#x}, {hi:#x})')

    # -- transaction handling ---------------------------------------------

    def _delay(self, rng_range):
        return 1 if self.zl else self.rng.randint(*rng_range)

    def _match_writes(self):
        while self.awq and len(self.wq) >= self.awq[0][1] + 1:
            addr, length = self.awq.popleft()
            for i in range(length + 1):
                data, strb, last = self.wq.popleft()
                if last != (i == length):
                    self._fail(f'WLAST={last} on beat {i} of burst '
                               f'{addr:#x} len {length + 1}')
                self.write_beat(addr + 8 * i, data,
                                strb | self.wstrb_stuck_on)
            resp = SLVERR if addr in self.bresp_err_addrs else OKAY
            ready = max(self.cycle + self._delay(self.b_delay),
                        self.b_last_ready)
            self.b_last_ready = ready
            self.bq.append((ready, resp))

    def _next_r_beat(self):
        if self.r_cur is not None or not self.rq:
            return
        burst = self.rq[0]
        addr, length, beat, ready = burst
        if ready > self.cycle:
            return
        if not self.zl and self.rng.random() >= self.p_rbeat:
            return
        baddr = addr + 8 * beat
        data = self.read_beat(baddr) ^ self.read_flips.get(baddr, 0)
        resp = SLVERR if baddr in self.rresp_err_addrs else OKAY
        self.r_cur = (data, resp, int(beat == length))

    def _drive(self, ctx):
        """Drive the subordinate outputs for the current cycle."""
        a = self.a
        if self.zl:
            awready = wready = 1
            arready = int(len(self.rq) < self.max_reads)
        else:
            awready = int(self.rng.random() < self.p_awready)
            wready = int(self.rng.random() < self.p_wready)
            arready = int(self.rng.random() < self.p_arready
                          and len(self.rq) < self.max_reads)
        if self.b_cur is None and self.bq and self.bq[0][0] <= self.cycle:
            self.b_cur = self.bq.popleft()[1]
        self._next_r_beat()
        ctx.set(a.awready, awready)
        ctx.set(a.wready, wready)
        ctx.set(a.arready, arready)
        ctx.set(a.bvalid, int(self.b_cur is not None))
        ctx.set(a.bresp, self.b_cur if self.b_cur is not None else 0)
        if self.r_cur is not None:
            data, resp, last = self.r_cur
            ctx.set(a.rvalid, 1)
            ctx.set(a.rdata, data)
            ctx.set(a.rresp, resp)
            ctx.set(a.rlast, last)
        else:
            ctx.set(a.rvalid, 0)
            ctx.set(a.rlast, 0)
        return awready, wready, arready

    async def process(self, ctx):
        a = self.a
        prev_aw = prev_w = prev_ar = None
        sampled = (
            a.awvalid, a.awaddr, a.awlen, a.awsize, a.awburst, a.awcache,
            a.wvalid, a.wdata, a.wstrb, a.wlast,
            a.arvalid, a.araddr, a.arlen, a.arsize, a.arburst, a.arcache,
            a.bready, a.rready)
        awready, wready, arready = self._drive(ctx)
        # The manager is sampled at the clock edge, so the result does not
        # depend on the order in which testbenches run.
        async for v in ctx.tick().sample(*sampled):
            v = v[2:]
            awvalid, aw = v[0], tuple(v[1:6])
            wvalid, wdata, wstrb, wlast = v[6:10]
            arvalid, ar = v[10], tuple(v[11:16])
            bready, rready = v[16:18]
            aw = aw if awvalid else None
            if prev_aw is not None and aw != prev_aw:
                self._fail(f'AW changed/dropped before handshake: '
                           f'{prev_aw} -> {aw}')
            w = (wdata, wstrb, wlast) if wvalid else None
            if prev_w is not None and w != prev_w:
                self._fail(f'W changed/dropped before handshake: '
                           f'{prev_w} -> {w}')
            ar = ar if arvalid else None
            if prev_ar is not None and ar != prev_ar:
                self._fail(f'AR changed/dropped before handshake: '
                           f'{prev_ar} -> {ar}')

            # Handshakes
            prev_aw = prev_w = prev_ar = None
            if aw is not None:
                if awready:
                    self._check_addr('AW', *aw)
                    self.awq.append((aw[0], aw[1]))
                    self.aw_log.append((self.cycle, aw[0], aw[1]))
                    self.w_out += 1
                    self.w_out_max = max(self.w_out_max, self.w_out)
                    if (self.max_outstanding is not None
                            and self.w_out > self.max_outstanding):
                        self._fail(f'{self.w_out} writes outstanding')
                else:
                    prev_aw = aw
            if w is not None:
                if wready:
                    accepted = sum(n + 1 for _, n in self.awq)
                    if len(self.wq) + 1 > accepted:
                        # W ahead of its AW handshake
                        self.w_early_beats += 1
                        presented = accepted
                        if aw is not None and not awready:
                            presented += aw[1] + 1
                        if (self.forbid_w_before_aw
                                and len(self.wq) + 1 > presented):
                            self._fail('W beat accepted before its AWVALID')
                    self.wq.append(w)
                    self.w_beats += 1
                else:
                    prev_w = w
            if aw is not None and awready or w is not None and wready:
                self._match_writes()
            if ar is not None:
                if arready:
                    self._check_addr('AR', *ar)
                    ready = max(self.cycle + self._delay(self.r_delay),
                                self.r_last_ready)
                    self.r_last_ready = ready
                    self.rq.append([ar[0], ar[1], 0, ready])
                    self.ar_log.append((self.cycle, ar[0], ar[1]))
                    self.r_out += 1
                    self.r_out_max = max(self.r_out_max, self.r_out)
                    if (self.max_outstanding is not None
                            and self.r_out > self.max_outstanding):
                        self._fail(f'{self.r_out} reads outstanding')
                else:
                    prev_ar = ar
            if self.b_cur is not None and bready:
                self.b_cur = None
                self.b_count += 1
                self.b_log.append(self.cycle)
                self.w_out -= 1
            if self.r_cur is not None and rready:
                self.r_beats += 1
                burst = self.rq[0]
                burst[2] += 1
                if self.r_cur[2]:
                    self.rlast_log.append(self.cycle)
                    self.rq.popleft()
                    self.r_out -= 1
                self.r_cur = None

            self.cycle += 1
            awready, wready, arready = self._drive(ctx)

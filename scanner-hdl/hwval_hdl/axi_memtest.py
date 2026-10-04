#
# Fishball hwval - AXI3 memory tester (mt0 / mt1)
#
# PL AXI3 read/write memory tester for the Zynq HP ports. See
# doc/HW_VALIDATION_SUITE.md section 8 ("Memory testing") and the
# mt0/mt1 register block in section 6.4.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *
from amaranth.lib.fifo import SyncFIFOBuffered

from maia_hdl import axi

from .lat_hist import LatencyTracker, latency_bin


__all__ = [
    'AxiMemTester', 'expected_beat', 'prbs_word', 'byte_lane_strobe',
    'byte_lane_expected', 'pattern_pass_index',
    'MODE_WRITE', 'MODE_READ_VERIFY', 'MODE_WRITE_VERIFY', 'MODE_READ',
    'MODE_BYTE_LANE', 'NUM_MODES',
    'PAT_ADDRESS', 'PAT_WALK1', 'PAT_WALK0', 'PAT_CHECKER', 'PAT_PRBS',
    'PAT_ZERO', 'PAT_ONES', 'PAT_TOGGLE', 'NUM_PATTERNS',
    'VALID_BURST_LENS', 'PRBS_PASS_MUL', 'PRBS_ROUNDS', 'PRBS_LOW_XOR',
    'LAT_BINS', 'lat_bin',
]

# Modes (MT_CTRL bits[2:0])
MODE_WRITE = 0          # write the window with the pattern
MODE_READ_VERIFY = 1    # read the window and compare with the pattern
MODE_WRITE_VERIFY = 2   # each pass: write the window, wait all B, verify it
MODE_READ = 3           # read the window, no compare (bandwidth)
MODE_BYTE_LANE = 4      # zeros, then ones with a one-hot WSTRB, then verify
NUM_MODES = 5

# Patterns (MT_CTRL bits[6:3])
PAT_ADDRESS = 0
PAT_WALK1 = 1
PAT_WALK0 = 2
PAT_CHECKER = 3
PAT_PRBS = 4
PAT_ZERO = 5
PAT_ONES = 6
PAT_TOGGLE = 7
NUM_PATTERNS = 8
# Internal generator selector for the byte-lane verify phase
_PAT_LANE = 8

VALID_BURST_LENS = (1, 2, 4, 8, 16)

PRBS_PASS_MUL = 0x9E3779B9
PRBS_ROUNDS = 3
PRBS_LOW_XOR = 0xA5A5A5A5

# Latency histogram: 16 log2 bins (see lat_hist.latency_bin())
LAT_BINS = LatencyTracker.NUM_BINS

_M32 = (1 << 32) - 1
_M64 = (1 << 64) - 1


# ---------------------------------------------------------------------------
# Pure-Python reference (imported by the host tools and the tests)
# ---------------------------------------------------------------------------

def _xorshift32(v):
    v ^= (v << 13) & _M32
    v ^= v >> 17
    v ^= (v << 5) & _M32
    return v


def _rotl32(x, n):
    return ((x << n) | (x >> (32 - n))) & _M32


def prbs_word(addr, pass_index, seed):
    """32-bit PRBS hash used by pattern 4.

    ``k = seed ^ (pass_index * 0x9E3779B9 mod 2**32)``,
    ``x = xorshift32^3(addr ^ k)`` (three xorshift32 steps with shifts
    13/17/5). The hash is linear over GF(2), so the hardware flattens it into
    an XOR tree of at most 18 inputs per bit.
    """
    k = (seed ^ (pass_index * PRBS_PASS_MUL)) & _M32
    v = (addr ^ k) & _M32
    for _ in range(PRBS_ROUNDS):
        v = _xorshift32(v)
    return v


def expected_beat(pattern, addr, pass_index, seed):
    """Data of the 64-bit beat at byte address ``addr``.

    This is the exact function implemented by the hardware generator for
    patterns 0-7. ``pass_index`` is the 0-based pass index within the run
    (see :func:`pattern_pass_index` for mode 1).
    """
    a = addr & _M32
    beat = a >> 3
    if pattern == PAT_ADDRESS:
        return (a << 32) | (~a & _M32)
    if pattern == PAT_WALK1:
        return 1 << ((beat + pass_index) % 64)
    if pattern == PAT_WALK0:
        return ~(1 << ((beat + pass_index) % 64)) & _M64
    if pattern == PAT_CHECKER:
        return (0xAAAAAAAAAAAAAAAA if (beat + pass_index) % 2 == 0
                else 0x5555555555555555)
    if pattern == PAT_PRBS:
        x = prbs_word(a, pass_index, seed)
        return (x << 32) | (_rotl32(x, 16) ^ PRBS_LOW_XOR)
    if pattern == PAT_ZERO:
        return 0
    if pattern == PAT_ONES:
        return _M64
    if pattern == PAT_TOGGLE:
        return _M64 if beat & 1 else 0
    raise ValueError(f'invalid pattern {pattern}')


def byte_lane_strobe(addr):
    """WSTRB used in byte-lane pass B for the beat at ``addr``."""
    return 1 << ((addr >> 3) & 7)


def byte_lane_expected(addr):
    """Expected data in byte-lane pass C for the beat at ``addr``."""
    return 0xFF << (8 * ((addr >> 3) & 7))


def pattern_pass_index(mode, pass_index):
    """Pass index that the generator uses for run pass ``pass_index``.

    Mode 1 (read-verify) always verifies against pass index 0, so repeated
    read-verify passes re-check a single fill.
    """
    return 0 if mode == MODE_READ_VERIFY else pass_index


# Histogram bin of a latency in cycles (0 for lat <= 1, otherwise
# floor(log2(lat)) clamped to 15), shared with lat_hist.LatencyTracker.
lat_bin = latency_bin


# ---------------------------------------------------------------------------
# Hardware
# ---------------------------------------------------------------------------

def _xorshift32_hw(v):
    v = v ^ Cat(Const(0, 13), v[:19])
    v = v ^ v[17:]
    v = v ^ Cat(Const(0, 5), v[:27])
    return v


def _sat_inc(m, counter, cond):
    with m.If(cond & ~counter.all()):
        m.d.sync += counter.eq(counter + 1)


class _PatternGen(Elaboratable):
    """Pipelined beat data generator.

    Computes the pattern data for the 64-bit beat at ``addr_in``. The
    output appears ``LATENCY`` cycles after ``valid_in`` (the pipeline never
    stalls). Stage 1 does the cheap per-address precompute (walking index,
    XOR with the PRBS key), stage 2 the one-hot decodes and the PRBS XOR
    tree, and stage 3 the pattern multiplexer. ``strb_out`` is the one-hot
    byte-lane strobe when ``lane_strb`` is set (all-ones otherwise), and
    ``last_out`` flags the last beat of an aligned ``blen_m1 + 1`` burst.
    """
    LATENCY = 3

    def __init__(self):
        self.valid_in = Signal()
        self.addr_in = Signal(32)
        self.pattern = Signal(4)
        self.lane_strb = Signal()
        self.pass_p = Signal(6)
        self.key = Signal(32)
        self.blen_m1 = Signal(4)

        self.valid_out = Signal()
        self.addr_out = Signal(32, reset_less=True)
        self.data_out = Signal(64, reset_less=True)
        self.strb_out = Signal(8, reset_less=True)
        self.last_out = Signal(reset_less=True)
        self.busy = Signal()

    def elaborate(self, platform):
        m = Module()

        # Stage 1 (only the valid bits are reset)
        v1 = Signal()
        a1 = Signal(32, reset_less=True)
        widx1 = Signal(6, reset_less=True)
        odd1 = Signal(reset_less=True)
        pv1 = Signal(32, reset_less=True)
        last1 = Signal(reset_less=True)
        m.d.sync += [
            v1.eq(self.valid_in),
            a1.eq(self.addr_in),
            widx1.eq(self.addr_in[3:9] + self.pass_p),
            odd1.eq(self.addr_in[3] ^ self.pass_p[0]),
            pv1.eq(self.addr_in ^ self.key),
            last1.eq((self.addr_in[3:7] & self.blen_m1) == self.blen_m1),
        ]

        # Stage 2
        v2 = Signal()
        a2 = Signal(32, reset_less=True)
        walk2 = Signal(64, reset_less=True)
        odd2 = Signal(reset_less=True)
        x2 = Signal(32, reset_less=True)
        lane2 = Signal(8, reset_less=True)
        last2 = Signal(reset_less=True)
        x = pv1
        for _ in range(PRBS_ROUNDS):
            x = _xorshift32_hw(x)
        m.d.sync += [
            v2.eq(v1),
            a2.eq(a1),
            walk2.eq(Cat(widx1 == j for j in range(64))),
            odd2.eq(odd1),
            x2.eq(x),
            lane2.eq(Cat(a1[3:6] == j for j in range(8))),
            last2.eq(last1),
        ]

        # Stage 3
        m.d.sync += [
            self.valid_out.eq(v2),
            self.addr_out.eq(a2),
            self.strb_out.eq(Mux(self.lane_strb, lane2, 0xFF)),
            self.last_out.eq(last2),
        ]
        with m.Switch(self.pattern):
            with m.Case(PAT_ADDRESS):
                m.d.sync += self.data_out.eq(Cat(~a2, a2))
            with m.Case(PAT_WALK1):
                m.d.sync += self.data_out.eq(walk2)
            with m.Case(PAT_WALK0):
                m.d.sync += self.data_out.eq(~walk2)
            with m.Case(PAT_CHECKER):
                m.d.sync += self.data_out.eq(
                    Mux(odd2, 0x5555555555555555, 0xAAAAAAAAAAAAAAAA))
            with m.Case(PAT_PRBS):
                m.d.sync += self.data_out.eq(
                    Cat(Cat(x2[16:], x2[:16]) ^ PRBS_LOW_XOR, x2))
            with m.Case(PAT_ONES):
                m.d.sync += self.data_out.eq(_M64)
            with m.Case(PAT_TOGGLE):
                m.d.sync += self.data_out.eq(Mux(a2[3], _M64, 0))
            with m.Case(_PAT_LANE):
                m.d.sync += self.data_out.eq(
                    Cat(Mux(lane2[j], 0xFF, 0x00) for j in range(8)))
            with m.Default():
                # PAT_ZERO
                m.d.sync += self.data_out.eq(0)

        m.d.comb += self.busy.eq(v1 | v2 | self.valid_out)
        return m


class AxiMemTester(Elaboratable):
    """AXI3 memory tester.

    AXI3 manager (64-bit data, 32-bit address, ID 0, INCR bursts,
    AxSIZE = 3, AxCACHE = 0b0011) that writes and/or reads a memory window
    ``[base, base + size)`` with a data pattern, verifies read data and
    measures bandwidth and latency. Everything runs in the ``sync`` domain
    (the integrator renames it to the 125 MHz ``mem`` domain).

    Configuration is quasi-static: it is latched on ``start`` and must only
    be changed while ``busy`` is low.

    Run sequence. ``start`` (ignored while busy) latches the configuration,
    clears ``done``/``error`` and checks it. The run is refused (no AXI
    traffic, ``guard_blocked`` += 1, ``done`` and ``error`` set, ``irq``
    pulsed) if ``base < guard_lo``, ``base + size > guard_hi`` (33-bit sum),
    ``size == 0``, ``base`` or ``size`` is not a multiple of the burst size
    in bytes, ``burst_len`` is not 1/2/4/8/16, ``mode > 4``, or
    ``pattern > 7`` in modes 0-2. Otherwise the tester runs ``passes``
    passes (0 = until ``abort``). Each pass is a sequence of phases that
    each sweep the whole window in ascending address order:

    - mode 0: write.
    - mode 1: read + verify. The data is always compared against pass
      index 0, so repeated passes re-check a single fill.
    - mode 2: write, wait for every B, read + verify.
    - mode 3: read, no compare.
    - mode 4: write zeros (full WSTRB), write all-ones with
      ``WSTRB = 1 << addr[5:3]``, read + verify against
      ``0xFF << 8 * addr[5:3]``. Each write phase waits for every B before
      the next phase starts.

    Writes and reads are never in flight at the same time. Within a phase
    up to ``max_outstanding_cfg`` bursts (0 is taken as 1, values above
    ``max_outstanding`` are clamped) are kept in flight. An address burst
    is committed (AxVALID set) only while fewer than that many are
    outstanding; it stays outstanding until its B (write) or RLAST (read)
    is received. W data for a burst is generated once its AWVALID is
    asserted, so WVALID never precedes AWVALID but does not wait for
    AWREADY (AXI write dependency rules). ``idle_cycles`` = N > 0 spaces
    successive address commits at least ``burst_len + N`` cycles apart
    (offered load ~ burst_len / (burst_len + N)); 0 = unthrottled. Bursts
    never cross 4 KiB since they are aligned to their own size.

    ``abort`` stops committing new bursts; bursts in flight complete, then
    the run ends with ``done`` set (a pass is counted in ``pass_count`` only
    if every burst of all its phases was issued). With
    ``stop_on_error`` the same happens once any error (data mismatch,
    BRESP or RRESP != OKAY) has been seen; bursts already in flight are
    still verified, so ``err_count`` can exceed 1.

    Data patterns are a pure function of the beat address, the pass index
    and the seed; :func:`expected_beat` is the bit-exact reference.

    Counters and captures (``pass_count``, ``bytes_wr``, ``bytes_rd``,
    ``cycles``, ``err_count``, ``first_err_*``, ``err_lanes``,
    ``bresp_err``, ``rresp_err``, latency max/histograms, ``guard_blocked``)
    accumulate until ``clear``; they are not reset by ``start``. Issue
    ``start`` and ``clear`` in the same cycle to begin a fresh run.
    ``clear`` while busy clears the counters without stopping the run.
    32-bit counters saturate.

    Parameters
    ----------
    name : str
        Name prefix of the AXI3 manager port.
    max_outstanding : int
        Maximum number of bursts in flight per direction (1..15).

    Attributes
    ----------
    axi : AXI3 manager interface (read and write).
    mode : Signal(3), in
    pattern : Signal(4), in
    burst_len : Signal(5), in
        Beats per burst: 1, 2, 4, 8 or 16. Other values refuse the run.
    max_outstanding_cfg : Signal(4), in
    stop_on_error : Signal(), in
    base : Signal(32), in
        Window base, aligned to ``8 * burst_len``.
    size : Signal(32), in
        Window size in bytes, a non-zero multiple of ``8 * burst_len``.
    passes : Signal(16), in
        Number of passes; 0 runs until ``abort``.
    idle_cycles : Signal(16), in
    seed : Signal(32), in
    guard_lo, guard_hi : Signal(32), in
        Allowed address range ``[guard_lo, guard_hi)``.
    start, abort, clear : Signal(), in
        Single-cycle command pulses.
    busy : Signal(), out
    done : Signal(), out
        Set when a run finishes (or is refused); cleared by start/clear.
    error : Signal(), out
        Any error seen in this run (mismatch, BRESP/RRESP != OKAY, refusal).
        Cleared by start/clear.
    pass_count : Signal(32), out
        Completed passes.
    bytes_wr, bytes_rd : Signal(64), out
        8 bytes per W / R handshake (bus bytes, regardless of WSTRB).
    cycles : Signal(64), out
        Cycles with ``busy`` set.
    err_count : Signal(32), out
        Mismatched 64-bit beats.
    first_err_addr : Signal(32), out
    first_err_exp, first_err_act : Signal(64), out
        Address, expected and actual data of the first mismatch since clear.
    err_lanes : Signal(64), out
        OR of ``expected ^ actual`` over all mismatches.
    bresp_err : Signal(32), out
        B responses with BRESP != OKAY.
    rresp_err : Signal(32), out
        R beats with RRESP != OKAY.
    wlat_max, rlat_max : Signal(32), out
        Maximum AW->B and AR->RLAST handshake latency in cycles.
    hist_sel : Signal(5), in
        bit 4: 0 = write latency histogram, 1 = read; bits[3:0]: bin.
    hist_val : Signal(32), out
        Selected histogram bin (see :func:`lat_bin`), 2 cycles behind
        ``hist_sel``.
    guard_blocked : Signal(32), out
        Refused starts.
    irq : Signal(), out
        Single-cycle pulse when a run finishes (with ``done`` rising).
    """
    FIFO_DEPTH = 16

    def __init__(self, *, name='m_axi_mt0', max_outstanding=8):
        if not 1 <= max_outstanding <= 15:
            raise ValueError('max_outstanding must be in 1..15')
        self.max_outstanding = max_outstanding
        self.axi = axi.AxiInterface(
            axi.AxiDevice.MANAGER,
            [axi.AxiChannel(axi.AxiDirection.WRITE, 32, 64),
             axi.AxiChannel(axi.AxiDirection.READ, 32, 64)],
            axi.AxiVersion.AXI3, name=name)

        # Configuration
        self.mode = Signal(3)
        self.pattern = Signal(4)
        self.burst_len = Signal(5, init=16)
        self.max_outstanding_cfg = Signal(4, init=max_outstanding)
        self.stop_on_error = Signal()
        self.base = Signal(32)
        self.size = Signal(32)
        self.passes = Signal(16, init=1)
        self.idle_cycles = Signal(16)
        self.seed = Signal(32)
        self.guard_lo = Signal(32)
        self.guard_hi = Signal(32)
        # Commands
        self.start = Signal()
        self.abort = Signal()
        self.clear = Signal()
        # Status
        self.busy = Signal()
        self.done = Signal()
        self.error = Signal()
        self.pass_count = Signal(32)
        self.bytes_wr = Signal(64)
        self.bytes_rd = Signal(64)
        self.cycles = Signal(64)
        self.err_count = Signal(32)
        self.first_err_addr = Signal(32)
        self.first_err_exp = Signal(64)
        self.first_err_act = Signal(64)
        self.err_lanes = Signal(64)
        self.bresp_err = Signal(32)
        self.rresp_err = Signal(32)
        self.wlat_max = Signal(32)
        self.rlat_max = Signal(32)
        self.hist_sel = Signal(5)
        self.hist_val = Signal(32)
        self.guard_blocked = Signal(32)
        self.irq = Signal()

    def ports(self):
        return self.axi.ports() + [
            self.mode, self.pattern, self.burst_len,
            self.max_outstanding_cfg, self.stop_on_error, self.base,
            self.size, self.passes, self.idle_cycles, self.seed,
            self.guard_lo, self.guard_hi,
            self.start, self.abort, self.clear,
            self.busy, self.done, self.error, self.pass_count,
            self.bytes_wr, self.bytes_rd, self.cycles, self.err_count,
            self.first_err_addr, self.first_err_exp, self.first_err_act,
            self.err_lanes, self.bresp_err, self.rresp_err,
            self.wlat_max, self.rlat_max, self.hist_sel, self.hist_val,
            self.guard_blocked, self.irq,
        ]

    def elaborate(self, platform):
        m = Module()
        a = self.axi
        mo_max = self.max_outstanding

        m.submodules.gen = gen = _PatternGen()
        m.submodules.wfifo = wfifo = SyncFIFOBuffered(
            width=64 + 8 + 1, depth=self.FIFO_DEPTH)
        m.submodules.wlat = wlat = LatencyTracker(max_outstanding=mo_max)
        m.submodules.rlat = rlat = LatencyTracker(max_outstanding=mo_max)

        # Latched configuration
        mode_l = Signal(3)
        pat_l = Signal(4)
        blen_raw = Signal(5)
        mo_raw = Signal(4)
        stop_l = Signal()
        base_l = Signal(32)
        size_l = Signal(32)
        passes_l = Signal(16)
        idle_l = Signal(16)
        seed_l = Signal(32)

        # Checked / derived configuration
        blen_ok = Signal()
        lg = Signal(3)          # log2(burst_len)
        blen_m1 = Signal(4)
        bb = Signal(8)          # bytes per burst
        align_ok = Signal()
        size_nz = Signal()
        lo_ok = Signal()
        end_addr = Signal(33)
        mode_ok = Signal()
        pat_ok = Signal()
        mo = Signal(range(mo_max + 1))
        nbursts = Signal(32)
        gap_load = Signal(17)
        gap_none = Signal()

        # Run state
        run_err = Signal()
        abort_req = Signal()
        halt = Signal()
        m.d.comb += halt.eq(abort_req | (stop_l & run_err))
        run_pass = Signal(16)
        pat_p = Signal(6)
        pmix = Signal(32)
        ph = Signal(2)
        last_ph = Signal(2)

        # Phase configuration
        is_read = Signal()
        cmp_en = Signal()
        gen_pat = Signal(4)
        lane_strb = Signal()
        key = Signal(32)

        # Status defaults
        m.d.sync += self.irq.eq(0)
        m.d.comb += [
            self.error.eq(run_err),
            self.wlat_max.eq(wlat.lat_max),
            self.rlat_max.eq(rlat.lat_max),
        ]
        with m.If(self.busy):
            m.d.sync += self.cycles.eq(self.cycles + 1)

        # Burst length decode
        lg_c = Signal(3)
        ok_c = Signal()
        mask_c = Signal(7)
        with m.Switch(blen_raw):
            for i, n in enumerate(VALID_BURST_LENS):
                with m.Case(n):
                    m.d.comb += [lg_c.eq(i), ok_c.eq(1),
                                 mask_c.eq(8 * n - 1)]

        # Phase table
        # (is_read, compare, generator pattern (None = configured), lane)
        phases = {
            MODE_WRITE: [(0, 0, None, 0)],
            MODE_READ_VERIFY: [(1, 1, None, 0)],
            MODE_WRITE_VERIFY: [(0, 0, None, 0), (1, 1, None, 0)],
            MODE_READ: [(1, 0, None, 0)],
            MODE_BYTE_LANE: [(0, 0, PAT_ZERO, 0), (0, 0, PAT_ONES, 1),
                             (1, 1, _PAT_LANE, 0)],
        }
        with m.Switch(mode_l):
            for mode, plist in phases.items():
                with m.Case(mode):
                    m.d.comb += last_ph.eq(len(plist) - 1)

        # ------------------------------------------------------------------
        # Address issue (shared by AW and AR; only one direction per phase)
        # ------------------------------------------------------------------
        running = Signal()
        iss_valid = Signal()
        iss_addr = Signal(32)
        iss_next = Signal(32)
        iss_left = Signal(32)
        iss_more = Signal()
        gap_cnt = Signal(17)
        gap_ok = Signal()
        out_cnt = Signal(range(mo_max + 1))

        m.d.comb += [
            a.awvalid.eq(iss_valid & ~is_read),
            a.arvalid.eq(iss_valid & is_read),
            a.awaddr.eq(iss_addr),
            a.araddr.eq(iss_addr),
            a.awlen.eq(blen_m1),
            a.arlen.eq(blen_m1),
            a.awsize.eq(3),
            a.arsize.eq(3),
            a.awburst.eq(axi.AxiBurst.INCR),
            a.arburst.eq(axi.AxiBurst.INCR),
            # Normal non-cacheable bufferable memory
            a.awcache.eq(0b0011),
            a.arcache.eq(0b0011),
            a.awprot.eq(0),
            a.arprot.eq(0),
            a.awlock.eq(0),
            a.arlock.eq(0),
        ]
        m.d.sync += [
            a.bready.eq(1),
            a.rready.eq(1),
        ]

        iss_ready = Signal()
        iss_hs = Signal()
        iss_can = Signal()
        commit = Signal()
        m.d.comb += [
            iss_ready.eq(Mux(is_read, a.arready, a.awready)),
            iss_hs.eq(iss_valid & iss_ready),
            iss_can.eq(running & iss_more & ~halt & (out_cnt < mo) & gap_ok),
            commit.eq((~iss_valid | iss_ready) & iss_can),
        ]
        with m.If(~iss_valid | iss_ready):
            m.d.sync += iss_valid.eq(iss_can)
        with m.If(commit):
            m.d.sync += [
                iss_addr.eq(iss_next),
                iss_next.eq(iss_next + bb),
                iss_left.eq(iss_left - 1),
                iss_more.eq(iss_left != 1),
                gap_cnt.eq(gap_load),
                gap_ok.eq(gap_none),
            ]
        with m.Elif(gap_cnt != 0):
            m.d.sync += [
                gap_cnt.eq(gap_cnt - 1),
                gap_ok.eq(gap_cnt == 1),
            ]

        # Registered response channels (bready = rready = 1)
        b_valid_q = Signal()
        b_resp_q = Signal(2, reset_less=True)
        r_valid_q = Signal()
        r_last_q = Signal(reset_less=True)
        r_resp_q = Signal(2, reset_less=True)
        r_data_q = Signal(64, reset_less=True)
        aw_hs_q = Signal()
        ar_hs_q = Signal()
        w_hs_q = Signal()
        m.d.sync += [
            b_valid_q.eq(a.b_handshake()),
            b_resp_q.eq(a.bresp),
            r_valid_q.eq(a.r_handshake()),
            r_last_q.eq(a.rlast),
            r_resp_q.eq(a.rresp),
            r_data_q.eq(a.rdata),
            aw_hs_q.eq(iss_hs & ~is_read),
            ar_hs_q.eq(iss_hs & is_read),
            w_hs_q.eq(a.w_handshake()),
        ]
        rlast_evt = Signal()
        m.d.comb += rlast_evt.eq(r_valid_q & r_last_q)

        with m.If(commit & ~(b_valid_q | rlast_evt)):
            m.d.sync += out_cnt.eq(out_cnt + 1)
        with m.Elif(~commit & (b_valid_q | rlast_evt)):
            m.d.sync += out_cnt.eq(out_cnt - 1)

        m.d.comb += [
            wlat.start.eq(aw_hs_q),
            wlat.done.eq(b_valid_q),
            wlat.clear.eq(self.clear),
            wlat.hist_sel.eq(self.hist_sel[:4]),
            rlat.start.eq(ar_hs_q),
            rlat.done.eq(rlast_evt),
            rlat.clear.eq(self.clear),
            rlat.hist_sel.eq(self.hist_sel[:4]),
        ]
        m.d.sync += self.hist_val.eq(
            Mux(self.hist_sel[4], rlat.hist_val, wlat.hist_val))

        # ------------------------------------------------------------------
        # Data generation (W data, or expected R data)
        # ------------------------------------------------------------------
        gaddr = Signal(32)
        wb_credit = Signal(range(mo_max + 1))
        wcred = Signal(range(self.FIFO_DEPTH + 1), init=self.FIFO_DEPTH)
        w_issue = Signal()
        gen_last = Signal()
        m.d.comb += [
            gen_last.eq((gaddr[3:7] & blen_m1) == blen_m1),
            w_issue.eq(~is_read & (wb_credit != 0) & (wcred != 0)),
            gen.valid_in.eq(w_issue | (r_valid_q & cmp_en & is_read)),
            gen.addr_in.eq(gaddr),
            gen.pattern.eq(gen_pat),
            gen.lane_strb.eq(lane_strb),
            gen.pass_p.eq(pat_p),
            gen.key.eq(key),
            gen.blen_m1.eq(blen_m1),
        ]
        with m.If(gen.valid_in):
            m.d.sync += gaddr.eq(gaddr + 8)

        # W beats may be generated for every committed AW burst
        aw_evt = Signal()
        wb_evt = Signal()
        m.d.comb += [
            aw_evt.eq(commit & ~is_read),
            wb_evt.eq(w_issue & gen_last),
        ]
        with m.If(aw_evt & ~wb_evt):
            m.d.sync += wb_credit.eq(wb_credit + 1)
        with m.Elif(~aw_evt & wb_evt):
            m.d.sync += wb_credit.eq(wb_credit - 1)
        with m.If(w_issue & ~w_hs_q):
            m.d.sync += wcred.eq(wcred - 1)
        with m.Elif(~w_issue & w_hs_q):
            m.d.sync += wcred.eq(wcred + 1)

        # W channel from the FIFO (only holds beats of committed AWs)
        m.d.comb += [
            wfifo.w_en.eq(gen.valid_out & ~is_read),
            wfifo.w_data.eq(Cat(gen.data_out, gen.strb_out, gen.last_out)),
            a.wvalid.eq(wfifo.r_rdy),
            a.wdata.eq(wfifo.r_data[:64]),
            a.wstrb.eq(wfifo.r_data[64:72]),
            a.wlast.eq(wfifo.r_data[72]),
            wfifo.r_en.eq(a.wready),
        ]
        with m.If(w_hs_q):
            m.d.sync += self.bytes_wr.eq(self.bytes_wr + 8)
        with m.If(r_valid_q):
            m.d.sync += self.bytes_rd.eq(self.bytes_rd + 8)

        # ------------------------------------------------------------------
        # Read compare
        # ------------------------------------------------------------------
        # R data delayed to line up with the generator output (no reset,
        # so it maps to SRLs)
        rd_dly = [Signal(64, name=f'rd_dly{i}', reset_less=True)
                  for i in range(_PatternGen.LATENCY)]
        m.d.sync += rd_dly[0].eq(r_data_q)
        for i in range(1, len(rd_dly)):
            m.d.sync += rd_dly[i].eq(rd_dly[i - 1])
        act = rd_dly[-1]
        exp = gen.data_out
        cv = Signal()
        m.d.comb += cv.eq(gen.valid_out & is_read)

        c1v = Signal()
        c1_diff = Signal(64)
        c1_bnz = Signal(8)
        c1_exp = Signal(64, reset_less=True)
        c1_addr = Signal(32, reset_less=True)
        m.d.sync += [
            c1v.eq(cv),
            c1_diff.eq(Mux(cv, exp ^ act, 0)),
            c1_bnz.eq(Cat(cv & (exp[8 * j:8 * j + 8] != act[8 * j:8 * j + 8])
                          for j in range(8))),
            c1_exp.eq(exp),
            c1_addr.eq(gen.addr_out),
        ]
        have_first = Signal()
        mism = Signal()
        m.d.comb += mism.eq(c1_bnz.any())
        m.d.sync += self.err_lanes.eq(self.err_lanes | c1_diff)
        _sat_inc(m, self.err_count, mism)
        with m.If(mism):
            m.d.sync += run_err.eq(1)
            with m.If(~have_first):
                m.d.sync += [
                    have_first.eq(1),
                    self.first_err_addr.eq(c1_addr),
                    self.first_err_exp.eq(c1_exp),
                    self.first_err_act.eq(c1_exp ^ c1_diff),
                ]

        # Response errors
        bresp_bad = Signal()
        rresp_bad = Signal()
        m.d.comb += [
            bresp_bad.eq(b_valid_q & (b_resp_q != axi.AxiResp.OKAY.value)),
            rresp_bad.eq(r_valid_q & (r_resp_q != axi.AxiResp.OKAY.value)),
        ]
        _sat_inc(m, self.bresp_err, bresp_bad)
        _sat_inc(m, self.rresp_err, rresp_bad)
        with m.If(bresp_bad | rresp_bad):
            m.d.sync += run_err.eq(1)

        # Phase drained: nothing left to commit, nothing in flight
        drained = Signal()
        m.d.comb += drained.eq(
            (~iss_more | halt) & ~iss_valid & (out_cnt == 0)
            & (wb_credit == 0) & ~gen.busy & (wfifo.level == 0)
            & ~r_valid_q & ~b_valid_q & ~c1v)

        # ------------------------------------------------------------------
        # Control FSM
        # ------------------------------------------------------------------
        with m.FSM(name='mt_fsm'):
            with m.State('IDLE'):
                with m.If(self.start):
                    m.d.sync += [
                        mode_l.eq(self.mode),
                        pat_l.eq(self.pattern),
                        blen_raw.eq(self.burst_len),
                        mo_raw.eq(self.max_outstanding_cfg),
                        stop_l.eq(self.stop_on_error),
                        base_l.eq(self.base),
                        size_l.eq(self.size),
                        passes_l.eq(self.passes),
                        idle_l.eq(self.idle_cycles),
                        seed_l.eq(self.seed),
                        self.busy.eq(1),
                        self.done.eq(0),
                        run_err.eq(0),
                        abort_req.eq(0),
                    ]
                    m.next = 'CHECK'
            with m.State('CHECK'):
                m.d.sync += [
                    blen_ok.eq(ok_c),
                    lg.eq(lg_c),
                    blen_m1.eq((mask_c >> 3)[:4]),
                    bb.eq(mask_c + 1),
                    align_ok.eq(((base_l[:7] | size_l[:7]) & mask_c) == 0),
                    size_nz.eq(size_l != 0),
                    lo_ok.eq(base_l >= self.guard_lo),
                    end_addr.eq(base_l + size_l),
                    mode_ok.eq(mode_l < NUM_MODES),
                    pat_ok.eq((pat_l < NUM_PATTERNS)
                              | (mode_l == MODE_READ)
                              | (mode_l == MODE_BYTE_LANE)),
                    mo.eq(Mux(mo_raw == 0, 1,
                              Mux(mo_raw > mo_max, mo_max, mo_raw))),
                ]
                m.next = 'DECIDE'
            with m.State('DECIDE'):
                with m.Switch(lg):
                    for i in range(len(VALID_BURST_LENS)):
                        with m.Case(i):
                            m.d.sync += nbursts.eq(size_l[3 + i:])
                m.d.sync += [
                    gap_load.eq(Mux(idle_l != 0, idle_l + blen_m1, 0)),
                    gap_none.eq(idle_l == 0),
                    run_pass.eq(0),
                    pat_p.eq(0),
                    pmix.eq(0),
                    ph.eq(0),
                ]
                with m.If(blen_ok & align_ok & size_nz & lo_ok
                          & (end_addr <= self.guard_hi) & mode_ok & pat_ok):
                    m.next = 'SETUP'
                with m.Else():
                    _sat_inc(m, self.guard_blocked, 1)
                    m.d.sync += run_err.eq(1)
                    m.next = 'FINISH'
            with m.State('SETUP'):
                m.d.sync += [
                    iss_next.eq(base_l),
                    iss_left.eq(nbursts),
                    iss_more.eq(1),
                    gap_cnt.eq(0),
                    gap_ok.eq(1),
                    gaddr.eq(base_l),
                    key.eq(seed_l ^ pmix),
                    gen_pat.eq(pat_l),
                    lane_strb.eq(0),
                ]
                with m.Switch(mode_l):
                    for mode, plist in phases.items():
                        with m.Case(mode):
                            with m.Switch(ph):
                                for i, (rd, cmp, pat, lane) in enumerate(
                                        plist):
                                    with m.Case(i):
                                        m.d.sync += [
                                            is_read.eq(rd),
                                            cmp_en.eq(cmp),
                                            lane_strb.eq(lane),
                                        ]
                                        if pat is not None:
                                            m.d.sync += gen_pat.eq(pat)
                m.next = 'RUN'
            with m.State('RUN'):
                m.d.comb += running.eq(1)
                with m.If(drained):
                    m.next = 'PHASE_END'
            with m.State('PHASE_END'):
                # A halted pass still counts if its last phase issued every
                # burst (iss_more is cleared by the last commit)
                with m.If(halt & ~((ph == last_ph) & ~iss_more)):
                    m.next = 'FINISH'
                with m.Elif(ph != last_ph):
                    m.d.sync += ph.eq(ph + 1)
                    m.next = 'SETUP'
                with m.Else():
                    _sat_inc(m, self.pass_count, 1)
                    m.d.sync += [
                        run_pass.eq(run_pass + 1),
                        ph.eq(0),
                    ]
                    with m.If(mode_l != MODE_READ_VERIFY):
                        m.d.sync += [
                            pat_p.eq(pat_p + 1),
                            pmix.eq(pmix + PRBS_PASS_MUL),
                        ]
                    with m.If(halt | ((passes_l != 0)
                                      & ((run_pass + 1)[:16] == passes_l))):
                        m.next = 'FINISH'
                    with m.Else():
                        m.next = 'SETUP'
            with m.State('FINISH'):
                m.d.sync += [
                    self.busy.eq(0),
                    self.done.eq(1),
                    self.irq.eq(1),
                ]
                m.next = 'IDLE'

        with m.If(self.abort & self.busy):
            m.d.sync += abort_req.eq(1)

        # Clear (wins over the updates above)
        with m.If(self.clear):
            m.d.sync += [
                self.done.eq(0),
                run_err.eq(0),
                self.pass_count.eq(0),
                self.bytes_wr.eq(0),
                self.bytes_rd.eq(0),
                self.cycles.eq(0),
                self.err_count.eq(0),
                self.first_err_addr.eq(0),
                self.first_err_exp.eq(0),
                self.first_err_act.eq(0),
                self.err_lanes.eq(0),
                have_first.eq(0),
                self.bresp_err.eq(0),
                self.rresp_err.eq(0),
                self.guard_blocked.eq(0),
            ]

        return m

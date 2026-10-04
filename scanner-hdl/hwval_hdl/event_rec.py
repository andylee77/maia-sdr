#
# Fishball hwval -- AD9361 CTRL_OUT event recorder
#
# Block `evt` of doc/HW_VALIDATION_SUITE.md section 6.4: timestamps every
# transition of the AD9361 CTRL_OUT[7:0] status pins (AGC/gain-lock,
# ENSM state, calibration flags, depending on the CTRL_OUT mux setting)
# into a FIFO that the PS drains through AXI-Lite.
#

from amaranth import *
from amaranth.lib.cdc import FFSynchronizer

from .ingest import AsyncFifo36, sat_inc


class EventRecorder(Elaboratable):
    """CTRL_OUT transition recorder.

    Record format (36 bits)::

        [35]    heartbeat (1 = heartbeat record, 0 = transition)
        [34:27] CTRL_OUT value (8 bits, full synchronized value)
        [26:0]  timestamp, low 27 bits of an i_domain cycle counter

    Operation (i_domain): `ctrl_in` is FFSynchronized per bit into
    i_domain (bits that change together may therefore be recorded as two
    records one cycle apart). While `enable` is high, every cycle in
    which a bit selected by `mask` differs from its value on the previous
    cycle produces a transition record with the new value. A heartbeat
    record (current value) is produced on the rising edge of `enable`
    (initial state and timestamp origin) and 2**heartbeat_log2 cycles
    after the previous record attempt if nothing happened in between, so
    consecutive records are never more than 2**heartbeat_log2 cycles
    apart and the host can unwrap the 27-bit timestamp (requires
    heartbeat_log2 <= 26; unwrapping is only ambiguous across
    overflows, which `overflows` reports). Records are written to a
    512-deep FIFO; a record that finds the FIFO full is dropped and
    counted in `overflows`.

    The timestamp counter is free-running from the i_domain reset, so it
    equals the low 27 bits of any other i_domain counter released from
    the same reset (e.g. the core TS_LO/TS_HI).

    Read side (o_domain): the head record is prefetched into `data`.
    `level` is the number of records available (head included); `data`
    is valid when ``level != 0``. `level` is exact for the head and for
    local pops, and may lag new writes by a few cycles (gray-coded write
    count through an FFSynchronizer); it never overstates what is
    available. `pop` discards the head (ignored when level == 0); the
    next record appears 2 o_domain cycles later.

    Clock domains:

    * asynchronous: `ctrl_in`, `reset`
    * i_domain (``sync``, 62.5 MHz): `enable`, `mask` (quasi-static),
      `clear`, `current`, `overflows`
    * o_domain (``s_axi_lite``): `level`, `data`, `pop`

    Integrator notes: `overflows` is an i_domain counter; read it through
    the core snapshot (SNAP_REQ bit0 sync). `current` is live i_domain
    data; register it through a (per-bit) FFSynchronizer for EVT_CURRENT.
    `reset` must be driven (e.g. from CORE_RESET) and pulsed once the
    clocks run; it resets the FIFO18E1, the write count (through an
    i_domain synchronizer) and the read side. XDC:
    ``set_false_path -to [get_pins <path>/fifo/fifo/fifo18e1/RST]``;
    all other crossings are FFSynchronizers (amaranth.vivado.false_path
    attribute rule).

    Parameters
    ----------
    i_domain : str
        Recording domain.
    o_domain : str
        Read-out domain.
    heartbeat_log2 : int
        log2 of the heartbeat interval in i_domain cycles.
    sim : bool
        Use the simulation FIFO model (AsyncFIFO) instead of FIFO18E1.

    Attributes
    ----------
    ctrl_in : Signal(8), in (asynchronous)
        AD9361 CTRL_OUT[7:0].
    enable : Signal(), in (i_domain)
        Enable recording.
    mask : Signal(8), in (i_domain)
        Bits whose transitions produce records.
    clear : Signal(), in (i_domain)
        Pulse: clear `overflows` (tie to 0 if unused).
    current : Signal(8), out (i_domain)
        Synchronized current CTRL_OUT value.
    overflows : Signal(32), out (i_domain)
        Records lost because the FIFO was full (saturating).
    reset : Signal(), in (asynchronous)
        FIFO and recorder read-side reset.
    level : Signal(10), out (o_domain)
        Records available (0 = empty; see above).
    data : Signal(36), out (o_domain)
        Head record.
    pop : Signal(), in (o_domain)
        Pulse: discard the head record.
    """
    def __init__(self, i_domain='sync', o_domain='s_axi_lite',
                 heartbeat_log2=26, sim=False):
        if not 2 <= heartbeat_log2 <= 26:
            raise ValueError('heartbeat_log2 must be in [2, 26]')
        self._i_domain = i_domain
        self._o_domain = o_domain
        self.heartbeat_log2 = heartbeat_log2
        self._sim = sim
        self.ts_width = 27
        # Write/pop counters: FIFO (512) + head register fit in 10 bits;
        # one extra bit keeps the difference unambiguous.
        self._cnt_width = 11

        self.ctrl_in = Signal(8)
        self.enable = Signal()
        self.mask = Signal(8)
        self.clear = Signal()
        self.current = Signal(8)
        self.overflows = Signal(32)

        self.reset = Signal()
        self.level = Signal(10)
        self.data = Signal(36)
        self.pop = Signal()

    def elaborate(self, platform):
        m = Module()
        i = self._i_domain
        o = self._o_domain
        cw = self._cnt_width

        m.submodules.fifo = fifo = AsyncFifo36(
            r_domain=o, w_domain=i, sim=self._sim)
        m.d.comb += fifo.reset.eq(self.reset)

        # ── i_domain: detection ──────────────────────────────────────
        reset_i = Signal()
        m.submodules.sync_reset_i = FFSynchronizer(
            self.reset, reset_i, o_domain=i, init=1)
        ctrl = Signal(8)
        m.submodules.sync_ctrl = FFSynchronizer(
            self.ctrl_in, ctrl, o_domain=i)
        m.d.comb += self.current.eq(ctrl)
        ctrl_q = Signal(8)
        enable_q = Signal()
        m.d[i] += [
            ctrl_q.eq(ctrl),
            enable_q.eq(self.enable),
        ]
        ts = Signal(self.ts_width)
        m.d[i] += ts.eq(ts + 1)

        hb_cnt = Signal(self.heartbeat_log2)
        event = Signal()
        heartbeat = Signal()
        m.d.comb += [
            event.eq(self.enable & ((ctrl ^ ctrl_q) & self.mask).any()),
            heartbeat.eq(self.enable & (~enable_q | hb_cnt.all())),
        ]
        with m.If(event | heartbeat):
            m.d[i] += hb_cnt.eq(0)
        with m.Else():
            m.d[i] += hb_cnt.eq(hb_cnt + 1)

        # Registered write stage
        wr_valid = Signal()
        wr_data = Signal(36)
        m.d[i] += [
            wr_valid.eq(event | heartbeat),
            wr_data.eq(Cat(ts, ctrl, heartbeat & ~event)),
        ]
        wren = Signal()
        m.d.comb += [
            wren.eq(wr_valid & ~reset_i),
            fifo.wren.eq(wren & ~fifo.full),
            fifo.data_in.eq(wr_data),
        ]
        sat_inc(m, i, self.overflows, wren & fifo.full)
        with m.If(self.clear):
            m.d[i] += self.overflows.eq(0)

        # Successful-write count, gray coded for the o_domain level.
        wr_count = Signal(cw)
        wr_gray = Signal(cw)
        wr_count_next = Signal(cw)
        m.d.comb += wr_count_next.eq(wr_count + fifo.wren)
        m.d[i] += [
            wr_count.eq(wr_count_next),
            wr_gray.eq(wr_count_next ^ (wr_count_next >> 1)),
        ]
        with m.If(reset_i):
            m.d[i] += [
                wr_count.eq(0),
                wr_gray.eq(0),
            ]

        # ── o_domain: head prefetch and level ────────────────────────
        reset_o = Signal()
        m.submodules.sync_reset_o = FFSynchronizer(
            self.reset, reset_o, o_domain=o, init=1)
        wr_gray_o = Signal(cw)
        m.submodules.sync_wr_gray = FFSynchronizer(
            wr_gray, wr_gray_o, o_domain=o)
        wr_count_o = Signal(cw)
        # Gray decode: b[k] = XOR of g[k:]
        m.d[o] += wr_count_o.eq(
            Cat(*[wr_gray_o[k:].xor() for k in range(cw)]))

        head_valid = Signal()
        fetching = Signal()
        pops = Signal(cw)
        pop_accept = Signal()
        m.d.comb += [
            pop_accept.eq(self.pop & head_valid),
            fifo.rden.eq(~fifo.empty & ~reset_o & ~fetching
                         & (~head_valid | pop_accept)),
        ]
        m.d[o] += fetching.eq(fifo.rden)
        with m.If(fetching):
            m.d[o] += [
                self.data.eq(fifo.data_out),
                head_valid.eq(1),
            ]
        with m.Elif(pop_accept):
            m.d[o] += head_valid.eq(0)
        with m.If(pop_accept):
            m.d[o] += pops.eq(pops + 1)
        with m.If(reset_o):
            m.d[o] += [
                head_valid.eq(0),
                fetching.eq(0),
                pops.eq(0),
            ]

        level_raw = Signal(cw)
        m.d.comb += level_raw.eq(wr_count_o - pops)
        with m.If(~head_valid):
            m.d.comb += self.level.eq(0)
        with m.Elif(level_raw == 0):
            m.d.comb += self.level.eq(1)
        with m.Elif(level_raw[-1]):
            m.d.comb += self.level.eq(2**len(self.level) - 1)
        with m.Else():
            m.d.comb += self.level.eq(level_raw)

        return m

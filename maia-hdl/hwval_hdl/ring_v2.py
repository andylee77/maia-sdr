#
# Fishball hwval - ring buffer writer v2
#
# Redesigned stream -> DDR ring writer (doc/HW_VALIDATION_SUITE.md
# section 7). Replaces DmaStreamRingWrite + packer holding registers:
# BRAM FIFO with drop counters, store-and-forward 16-beat bursts,
# registered AXI VALIDs, drain-on-disable, flush padding, optional
# sub-buffer headers, protect mode, committed-burst pointer and IRQ
# coalescing.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *
from amaranth.lib.fifo import SyncFIFO, SyncFIFOBuffered

from maia_hdl import axi
from .lat_hist import LatencyTracker


PAD_MAGIC = 0xF1B1
HDR_MAGIC = 0xF1B0


def _sat_inc(sig):
    """Saturating +1 of an unsigned signal."""
    return Mux(sig.all(), sig, sig + 1)


def ring_marker(magic, drop_count, word_count):
    """64-bit header/pad word (Python reference)."""
    return ((magic << 48) | ((drop_count & 0xFFFF) << 32)
            | (word_count & 0xFFFF_FFFF))


class RingWriterV2(Elaboratable):
    """64-bit stream -> AXI3 ring buffer writer (ring v2).

    **Data path.** Input words (``in_data``/``in_valid``, at most one per
    cycle, never stalled) are accepted only while the writer is enabled
    (state RUN). They go through a 16-word elastic FIFO (LUTRAM) into a
    merge stage that inserts header and pad words, and then into the main
    FIFO (``SyncFIFOBuffered``, block RAM, ``fifo_depth`` words). When the
    elastic FIFO is full (because the main FIFO is full) the word is
    dropped and counted: ``drop_protect`` if the protect limit has
    blocked the ring during the current overflow episode (from the
    blocking until a word is accepted again), ``drop_full`` otherwise.
    Every word written into the main FIFO is later written to memory in
    order (or, only in the protect-blocked drain case below, discarded
    and counted), so ``words_in`` always equals the data words written
    plus ``drop_full`` plus ``drop_protect`` once the writer is idle.

    **Bursts.** The write side of the main FIFO counts complete 16-word
    groups. An AW is issued only when a whole group is in the FIFO
    (store-and-forward), so a W burst never waits for data. Bursts are
    INCR, 16 beats of 64 bits (128 B), AWCACHE 0b0011, WSTRB all ones,
    ID 0. Burst k (k counted from ``soft_reset``) goes to
    ``base + (k mod size_bursts) * 128``; the ring position wraps by
    compare, so ``size_bursts`` can be any value >= 2. ``base`` must be
    4 KiB aligned, so bursts never cross 4 KiB. AWVALID and WVALID are
    registers that stay high with a stable payload until their handshake.
    W beats of a burst are sent after its AW handshake. At most
    ``max_outstanding_cfg`` bursts are in flight (AW issued, B not yet
    received); 0 or a value above ``max_outstanding`` selects
    ``max_outstanding``. BREADY is always high.

    **Committed pointer.** ``committed_bursts`` counts write responses. It
    advances on every B, including error responses, so that burst k of
    the stream always sits at ring slot ``k mod size_bursts`` from the
    reader's point of view; responses other than OKAY are counted in
    ``bresp_err`` (the reader must treat the ring as suspect when that
    counter moves). ``issued_bursts`` counts AW handshakes. Both are
    32-bit monotonic (wrapping) pointers that are only reset by
    ``soft_reset``.

    **Enable / disable.** In IDLE, a rising edge of ``enable`` (``enable``
    must have been observed low since the previous attempt) starts a
    3-cycle check of the quasi-static configuration: the ring
    ``[base, base + size_bursts * 128)`` must lie within
    ``[guard_lo, guard_hi)``, ``size_bursts >= 2`` and ``base`` must be
    4 KiB aligned. On failure the writer stays in IDLE and
    ``guard_blocked`` counts the refusal (toggle ``enable`` to retry). On
    success ``epoch`` increments and the writer enters RUN
    (``enabled = 1``). When ``enable`` falls, the writer stops accepting
    input, finishes merging what it has, pads the partial burst (a final
    flush, counted in ``flushes``/``pad_words``), writes all remaining
    groups, waits for every B and returns to IDLE, emptying the FIFOs.
    Nothing is ever cut mid-burst and AXI VALIDs are never withdrawn. The
    ring position is kept, so the next session continues at the next
    burst; readers detect the session change through ``epoch`` and the
    header/pad word counts.

    **Flush.** With a partial burst buffered, ``flush_timeout`` cycles
    without an input word (0 = never) or a ``flush`` pulse pad the burst
    to 16 words with ``{0xF1B1, drop_count[15:0], word_count[31:0]}``
    where ``drop_count`` is the low 16 bits of ``drop_full +
    drop_protect`` and ``word_count`` the low 32 bits of ``words_in``,
    both latched when the flush starts, so all pads of one flush are
    identical and ``word_count`` is the index of the next data word. The
    flush starts at the first cycle in which the elastic FIFO is empty,
    so the pads follow every word received before it (with a word on
    every cycle the burst simply fills with data first). Words arriving
    during padding wait in the elastic FIFO.

    **Headers.** With ``header_enable`` and ``subbuf_bursts != 0``, the
    first word of every burst whose index k (from ``soft_reset``) is a
    multiple of ``subbuf_bursts`` is ``{0xF1B0, drop_count[15:0],
    word_count[31:0]}``, where ``word_count`` is the ``words_in`` index
    of the data word that follows the header. Headers are inserted lazily
    when that data word is merged, and shift the data by one word per
    sub-buffer.

    **Protect mode.** With ``protect``, a burst is only issued while
    ``issued - consumer_bursts < size_bursts - 1`` (32-bit modular
    difference, counting a presented AW as issued), i.e. the producer is
    never more than ``size_bursts - 1`` bursts ahead of the PS-written
    consumed count and unconsumed data is never overwritten. While
    blocked, the FIFO fills and further words are dropped into
    ``drop_protect``. If the ring is blocked during a disable once nothing
    is in flight, the unissued FIFO contents (and any partial burst) are
    discarded, data words counted in ``drop_protect``, so that the writer
    always reaches IDLE.

    **IRQ.** ``irq`` is a one-cycle pulse when ``irq_every != 0`` bursts
    have been committed since the last IRQ, or when ``irq_timeout != 0``
    cycles have elapsed since the last IRQ and at least one burst has
    been committed since. Either IRQ restarts both conditions. The timer
    runs in every state, so the final bursts of a drain also raise an
    IRQ.

    **Commands.** ``soft_reset`` is accepted only in IDLE (ignored
    otherwise) and resets the ring position: ``issued_bursts``,
    ``committed_bursts``, the ring slot, the sub-buffer phase and the IRQ
    coalescing state. ``clear`` resets the statistics: ``words_in``,
    drop/pad/flush/header/error counters, ``fifo_hwm``, the latency
    tracker, ``epoch`` and ``guard_blocked``. Statistics counters are
    32-bit saturating (``words_in`` is a 64-bit counter).

    Configuration inputs are quasi-static: change them only in IDLE,
    except ``consumer_bursts`` (live).

    Parameters
    ----------
    name : str
        Name prefix of the AXI3 manager port.
    fifo_depth : int
        Main FIFO depth in 64-bit words (block RAM).
    max_outstanding : int
        Maximum bursts in flight (1..15).

    Attributes
    ----------
    axi : AXI3 manager interface (write channel only, 32-bit address,
        64-bit data, no IDs)
    in_data : Signal(64), in
    in_valid : Signal(), in
    enable, protect, header_enable : Signal(), in
    base, size_bursts : Signal(32), in
    subbuf_bursts, irq_every : Signal(16), in
    irq_timeout, flush_timeout : Signal(32), in
    max_outstanding_cfg : Signal(4), in
    consumer_bursts, guard_lo, guard_hi : Signal(32), in
    soft_reset, flush, clear : Signal(), in (pulses)
    committed_bursts : Signal(32), out
        B responses received (OKAY or not) since soft_reset.
    issued_bursts : Signal(32), out
        AW handshakes since soft_reset.
    words_in : Signal(64), out
        Input words received while enabled (written or dropped).
    drop_full, drop_protect : Signal(32), out
        Input words dropped (see Data path, Protect mode).
    pad_words : Signal(32), out
        Pad words written to memory.
    flushes : Signal(32), out
        Flushes that padded a partial burst (timeout, pulse or disable).
    bresp_err : Signal(32), out
        B responses other than OKAY.
    fifo_hwm : Signal(16), out
        Main FIFO level high-water mark (words).
    max_outstanding_seen : Signal(4), out
        High-water mark of bursts between AW and B handshakes.
    lat_max : Signal(32), out
        Largest AW-handshake -> B latency in cycles.
    hist_sel : Signal(4), in
    hist_val : Signal(32), out
        AW -> B latency histogram bin (see ``LatencyTracker``).
    epoch : Signal(32), out
        Successful enables (RUN entries).
    headers : Signal(32), out
        Header words written to memory.
    guard_blocked : Signal(32), out
        Enables refused by the configuration check.
    idle : Signal(), out
        IDLE state: nothing buffered or in flight.
    enabled : Signal(), out
        RUN state (input accepted).
    fifo_empty : Signal(), out
    irq : Signal(), out
    """
    BURST_BEATS = 16
    BURST_BYTES = 128

    def __init__(self, *, name='m_axi_ringv2', fifo_depth=2048,
                 max_outstanding=8):
        if not 1 <= max_outstanding <= 15:
            raise ValueError('max_outstanding must be in 1..15')
        if fifo_depth < 2 * self.BURST_BEATS:
            raise ValueError('fifo_depth must be >= 32')
        self.fifo_depth = fifo_depth
        self.max_outstanding = max_outstanding
        self.elastic_depth = 16

        self.axi = axi.AxiInterface(
            axi.AxiDevice.MANAGER,
            [axi.AxiChannel(axi.AxiDirection.WRITE, 32, 64)],
            axi.AxiVersion.AXI3, name=name)

        self.in_data = Signal(64)
        self.in_valid = Signal()

        self.enable = Signal()
        self.protect = Signal()
        self.header_enable = Signal()
        self.base = Signal(32)
        self.size_bursts = Signal(32)
        self.subbuf_bursts = Signal(16)
        self.irq_every = Signal(16)
        self.irq_timeout = Signal(32)
        self.flush_timeout = Signal(32)
        self.max_outstanding_cfg = Signal(4)
        self.consumer_bursts = Signal(32)
        self.guard_lo = Signal(32)
        self.guard_hi = Signal(32)

        self.soft_reset = Signal()
        self.flush = Signal()
        self.clear = Signal()

        self.committed_bursts = Signal(32)
        self.issued_bursts = Signal(32)
        self.words_in = Signal(64)
        self.drop_full = Signal(32)
        self.drop_protect = Signal(32)
        self.pad_words = Signal(32)
        self.flushes = Signal(32)
        self.bresp_err = Signal(32)
        self.fifo_hwm = Signal(16)
        self.max_outstanding_seen = Signal(4)
        self.lat_max = Signal(32)
        self.hist_sel = Signal(4)
        self.hist_val = Signal(32)
        self.epoch = Signal(32)
        self.headers = Signal(32)
        self.guard_blocked = Signal(32)
        self.idle = Signal()
        self.enabled = Signal()
        self.fifo_empty = Signal()
        self.irq = Signal()

    def elaborate(self, platform):
        m = Module()
        a = self.axi
        n_out = self.max_outstanding

        m.submodules.elastic = elastic = SyncFIFO(
            width=96, depth=self.elastic_depth)
        m.submodules.fifo = fifo = SyncFIFOBuffered(
            width=66, depth=self.fifo_depth)
        m.submodules.lat = lat = LatencyTracker(max_outstanding=n_out)

        in_run = Signal()
        in_drain = Signal()
        in_discard = Signal()
        merging = Signal()
        m.d.comb += merging.eq(in_run | in_drain)

        # ---- Registered views of the quasi-static configuration
        size_m1 = Signal(32)
        subbuf_m1 = Signal(16)
        max_eff = Signal(range(n_out + 1))
        guard_end = Signal(40)
        guard_ok = Signal()
        m.d.sync += [
            size_m1.eq(self.size_bursts - 1),
            subbuf_m1.eq(self.subbuf_bursts - 1),
            max_eff.eq(Mux((self.max_outstanding_cfg == 0)
                           | (self.max_outstanding_cfg > n_out),
                           n_out, self.max_outstanding_cfg)),
            guard_end.eq(self.base + (self.size_bursts << 7)),
            guard_ok.eq((self.base >= self.guard_lo)
                        & (guard_end <= self.guard_hi)
                        & (self.size_bursts >= 2)
                        & (self.base[:12] == 0)),
        ]

        # ---- Ring position (issue side)
        claimed = Signal(32)   # AWs presented (issued + pending AWVALID)
        slot = Signal(32)      # claimed mod size_bursts
        isub = Signal(16)      # claimed mod subbuf_bursts
        blocked = Signal()
        m.d.sync += blocked.eq(
            self.protect
            & ((claimed - self.consumer_bursts)[:32] >= size_m1))

        # ---- Drop accounting
        drop_lo16 = Signal(16)

        # ---- Input stage -> elastic FIFO
        accept = Signal()
        m.d.comb += [
            accept.eq(in_run & self.in_valid),
            elastic.w_data.eq(Cat(self.in_data, self.words_in[:32])),
            elastic.w_en.eq(accept),
        ]
        # A FIFO overflow episode in which the protect limit blocked the
        # ring is attributed to protect until a word gets in again (the
        # FIFO is still full for a while after consumer_bursts advances).
        protect_episode = Signal()
        with m.If(blocked):
            m.d.sync += protect_episode.eq(1)
        with m.Elif(accept & elastic.w_rdy):
            m.d.sync += protect_episode.eq(0)
        with m.If(accept):
            m.d.sync += self.words_in.eq(self.words_in + 1)
            with m.If(~elastic.w_rdy):
                m.d.sync += drop_lo16.eq(drop_lo16 + 1)
                with m.If(blocked | protect_episode):
                    m.d.sync += self.drop_protect.eq(
                        _sat_inc(self.drop_protect))
                with m.Else():
                    m.d.sync += self.drop_full.eq(_sat_inc(self.drop_full))

        # ---- Merge stage: elastic FIFO + header/pad words -> main FIFO
        wpos = Signal(4)       # word position in the burst being filled
        wsub = Signal(16)      # burst index (write side) mod subbuf_bursts
        padding = Signal()
        flush_req = Signal()
        groups_ready = Signal(range(self.fifo_depth // 16 + 2))

        hdr_needed = Signal()
        m.d.comb += hdr_needed.eq(
            self.header_enable & (self.subbuf_bursts != 0)
            & (wpos == 0) & (wsub == 0))
        pad_fields = Signal(48)  # {drop_count, word_count} at flush start
        pad_word = Cat(pad_fields, C(PAD_MAGIC, 16))
        hdr_word = Cat(elastic.r_data[64:96], drop_lo16, C(HDR_MAGIC, 16))

        # Main FIFO words carry their kind in bits 65:64 (free in the
        # 72-bit wide block RAM): 0b01 data, 0b10 pad, 0b00 header.
        with m.If(merging & fifo.w_rdy):
            with m.If(padding):
                m.d.comb += [
                    fifo.w_data.eq(Cat(pad_word, C(0b10, 2))),
                    fifo.w_en.eq(1),
                ]
            with m.Elif(elastic.r_rdy):
                with m.If(hdr_needed):
                    m.d.comb += [
                        fifo.w_data.eq(Cat(hdr_word, C(0b00, 2))),
                        fifo.w_en.eq(1),
                    ]
                with m.Else():
                    m.d.comb += [
                        fifo.w_data.eq(Cat(elastic.r_data[:64], C(0b01, 2))),
                        fifo.w_en.eq(1),
                        elastic.r_en.eq(1),
                    ]

        burst_done = Signal()
        m.d.comb += burst_done.eq(fifo.w_en & (wpos == 15))
        with m.If(fifo.w_en):
            m.d.sync += wpos.eq(wpos + 1)
        with m.If(burst_done):
            m.d.sync += [
                wsub.eq(Mux(wsub >= subbuf_m1, 0, wsub + 1)),
                padding.eq(0),
            ]

        # A flush starts once every word received before it has been
        # merged (elastic FIFO empty). It cannot coincide with a merge
        # write, since those need padding or a non-empty elastic FIFO.
        with m.If(merging & flush_req & ~padding & ~elastic.r_rdy):
            m.d.sync += flush_req.eq(0)
            with m.If(wpos != 0):
                m.d.sync += [
                    padding.eq(1),
                    pad_fields.eq(Cat(self.words_in[:32], drop_lo16)),
                    self.flushes.eq(_sat_inc(self.flushes)),
                ]

        idle_cnt = Signal(32)
        with m.If(in_run & self.in_valid):
            m.d.sync += idle_cnt.eq(0)
        with m.Elif(~idle_cnt.all()):
            m.d.sync += idle_cnt.eq(idle_cnt + 1)
        flush_timed_out = Signal()
        m.d.comb += flush_timed_out.eq(
            (self.flush_timeout != 0) & (idle_cnt == self.flush_timeout))
        with m.If(in_run & (self.flush | flush_timed_out)):
            m.d.sync += flush_req.eq(1)

        with m.If(fifo.level > self.fifo_hwm):
            m.d.sync += self.fifo_hwm.eq(fifo.level)

        # ---- AW channel
        # An AW is loaded only while AWVALID is low, so two loads are at
        # least 2 cycles apart and the registered ``blocked``/``inflight``
        # already account for the previous load.
        inflight = Signal(range(n_out + 1))
        aw_load = Signal()
        m.d.comb += aw_load.eq(
            merging & ~a.awvalid & (groups_ready != 0)
            & (inflight < max_eff) & ~blocked)
        m.d.comb += [
            a.awlen.eq(self.BURST_BEATS - 1),
            a.awsize.eq(3),
            a.awburst.eq(axi.AxiBurst.INCR.value),
            # Normal non-cacheable bufferable memory
            a.awcache.eq(0b0011),
            a.awprot.eq(0),
            a.awlock.eq(0),
            a.wstrb.eq(0xFF),
            a.bready.eq(1),
        ]
        with m.If(aw_load):
            m.d.sync += [
                a.awvalid.eq(1),
                a.awaddr.eq((self.base + (slot << 7))[:32]),
                slot.eq(Mux(slot >= size_m1, 0, slot + 1)),
                isub.eq(Mux(isub >= subbuf_m1, 0, isub + 1)),
                claimed.eq(claimed + 1),
            ]
        with m.Elif(a.aw_handshake()):
            m.d.sync += a.awvalid.eq(0)
        with m.If(a.aw_handshake()):
            m.d.sync += self.issued_bursts.eq(self.issued_bursts + 1)

        with m.If(burst_done & ~aw_load):
            m.d.sync += groups_ready.eq(groups_ready + 1)
        with m.Elif(aw_load & ~burst_done):
            m.d.sync += groups_ready.eq(groups_ready - 1)

        # ---- W channel
        w_avail = Signal(range(n_out + 1))  # AW done, last beat not loaded
        lbeat = Signal(4)
        w_load = Signal()
        discard_pop = Signal()
        discard_pop_el = Signal()
        m.d.comb += [
            w_load.eq((~a.wvalid | a.w_handshake()) & (w_avail != 0)
                      & fifo.r_rdy),
            # DISCARD empties the main FIFO first, then the elastic FIFO
            discard_pop.eq(in_discard & fifo.r_rdy),
            discard_pop_el.eq(in_discard & ~fifo.r_rdy & elastic.r_rdy),
            fifo.r_en.eq(w_load | discard_pop),
        ]
        with m.If(discard_pop_el):
            m.d.comb += elastic.r_en.eq(1)
        w_last_load = Signal()
        m.d.comb += w_last_load.eq(w_load & (lbeat == 15))
        with m.If(w_load):
            m.d.sync += [
                a.wdata.eq(fifo.r_data[:64]),
                a.wlast.eq(lbeat == 15),
                a.wvalid.eq(1),
                lbeat.eq(lbeat + 1),
            ]
            # pads/headers are counted when they go to memory, so that
            # words discarded by DISCARD are not counted
            with m.If(fifo.r_data[65]):
                m.d.sync += self.pad_words.eq(_sat_inc(self.pad_words))
            with m.Elif(~fifo.r_data[64]):
                m.d.sync += self.headers.eq(_sat_inc(self.headers))
        with m.Elif(a.w_handshake()):
            m.d.sync += a.wvalid.eq(0)
        with m.If(a.aw_handshake() & ~w_last_load):
            m.d.sync += w_avail.eq(w_avail + 1)
        with m.Elif(w_last_load & ~a.aw_handshake()):
            m.d.sync += w_avail.eq(w_avail - 1)

        # ---- B channel
        b_hs = Signal()
        m.d.comb += b_hs.eq(a.b_handshake())
        with m.If(b_hs):
            m.d.sync += self.committed_bursts.eq(self.committed_bursts + 1)
            with m.If(a.bresp != axi.AxiResp.OKAY.value):
                m.d.sync += self.bresp_err.eq(_sat_inc(self.bresp_err))
        with m.If(aw_load & ~b_hs):
            m.d.sync += inflight.eq(inflight + 1)
        with m.Elif(b_hs & ~aw_load):
            m.d.sync += inflight.eq(inflight - 1)

        m.d.comb += [
            lat.start.eq(a.aw_handshake()),
            lat.done.eq(b_hs),
            lat.clear.eq(self.clear),
            lat.hist_sel.eq(self.hist_sel),
            self.hist_val.eq(lat.hist_val),
            self.lat_max.eq(lat.lat_max),
            self.max_outstanding_seen.eq(lat.outstanding_max),
        ]

        # ---- IRQ coalescing
        irq_cnt = Signal(16)
        irq_pending = Signal()
        irq_timer = Signal(32)
        fire = Signal()
        m.d.comb += fire.eq(
            ((self.irq_every != 0) & (irq_cnt >= self.irq_every))
            | ((self.irq_timeout != 0) & irq_pending
               & (irq_timer >= self.irq_timeout)))
        m.d.sync += self.irq.eq(fire)
        with m.If(fire):
            m.d.sync += [
                irq_cnt.eq(b_hs),
                irq_pending.eq(b_hs),
                irq_timer.eq(0),
            ]
        with m.Else():
            with m.If(b_hs):
                m.d.sync += [
                    irq_cnt.eq(irq_cnt + 1),
                    irq_pending.eq(1),
                ]
            with m.If(~irq_timer.all()):
                m.d.sync += irq_timer.eq(irq_timer + 1)

        # ---- Control state machine
        pipe_empty = Signal()
        axi_quiet = Signal()
        m.d.comb += [
            pipe_empty.eq(~elastic.r_rdy & ~padding & ~flush_req
                          & (wpos == 0)),
            axi_quiet.eq(~a.awvalid & (w_avail == 0) & ~a.wvalid
                         & (inflight == 0)),
        ]
        en_seen_low = Signal(init=1)
        chk_cnt = Signal(2)
        with m.If(~self.enable):
            m.d.sync += en_seen_low.eq(1)

        with m.FSM():
            with m.State('IDLE'):
                m.d.comb += self.idle.eq(1)
                with m.If(self.enable & en_seen_low):
                    m.d.sync += [
                        en_seen_low.eq(0),
                        chk_cnt.eq(0),
                    ]
                    m.next = 'CHECK'
                with m.Elif(self.soft_reset):
                    m.d.sync += [
                        self.issued_bursts.eq(0),
                        self.committed_bursts.eq(0),
                        claimed.eq(0),
                        slot.eq(0),
                        isub.eq(0),
                        wsub.eq(0),
                        irq_cnt.eq(0),
                        irq_pending.eq(0),
                    ]
            with m.State('CHECK'):
                # wait for the registered config views to settle
                m.d.sync += chk_cnt.eq(chk_cnt + 1)
                with m.If(~self.enable):
                    m.next = 'IDLE'
                with m.Elif(chk_cnt == 3):
                    with m.If(guard_ok):
                        m.d.sync += [
                            self.epoch.eq(_sat_inc(self.epoch)),
                            wpos.eq(0),
                            wsub.eq(isub),
                        ]
                        with m.If(slot > size_m1):
                            m.d.sync += slot.eq(0)
                        m.next = 'RUN'
                    with m.Else():
                        m.d.sync += self.guard_blocked.eq(
                            _sat_inc(self.guard_blocked))
                        m.next = 'IDLE'
            with m.State('RUN'):
                m.d.comb += [
                    in_run.eq(1),
                    self.enabled.eq(1),
                ]
                with m.If(~self.enable):
                    # final flush of the partial burst
                    m.d.sync += flush_req.eq(1)
                    m.next = 'DRAIN'
            with m.State('DRAIN'):
                m.d.comb += in_drain.eq(1)
                with m.If(pipe_empty & axi_quiet & (groups_ready == 0)
                          & (fifo.level == 0)):
                    m.next = 'IDLE'
                with m.Elif(axi_quiet & blocked):
                    # nothing in flight and the protect limit forbids the
                    # next burst: whatever is left can never be written
                    m.next = 'DISCARD'
            with m.State('DISCARD'):
                # Drop the protect-blocked leftovers so that the writer
                # always reaches IDLE. Data words (kind bit 64 in the main
                # FIFO, every elastic entry) count as drop_protect.
                m.d.comb += in_discard.eq(1)
                with m.If((discard_pop & fifo.r_data[64]) | discard_pop_el):
                    m.d.sync += [
                        self.drop_protect.eq(_sat_inc(self.drop_protect)),
                        drop_lo16.eq(drop_lo16 + 1),
                    ]
                with m.If((fifo.level == 0) & ~elastic.r_rdy):
                    m.d.sync += [
                        groups_ready.eq(0),
                        wpos.eq(0),
                        padding.eq(0),
                        flush_req.eq(0),
                    ]
                    m.next = 'IDLE'

        m.d.comb += self.fifo_empty.eq((fifo.level == 0) & ~elastic.r_rdy)

        # ---- Statistics clear (wins over increments)
        with m.If(self.clear):
            m.d.sync += [
                self.words_in.eq(0),
                self.drop_full.eq(0),
                self.drop_protect.eq(0),
                self.pad_words.eq(0),
                self.flushes.eq(0),
                self.bresp_err.eq(0),
                self.fifo_hwm.eq(0),
                self.epoch.eq(0),
                self.headers.eq(0),
                self.guard_blocked.eq(0),
                drop_lo16.eq(0),
            ]

        return m

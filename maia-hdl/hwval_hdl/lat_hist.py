#
# Fishball hwval - request -> response latency tracker
#
# Shared by the ring writers (AW handshake -> B handshake) and the AXI
# memory testers (AW -> B, AR -> RLAST). See doc/HW_VALIDATION_SUITE.md
# sections 6-8.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *
from amaranth.lib.memory import Memory


def _sat_inc(sig):
    """Saturating +1 of an unsigned signal."""
    return Mux(sig.all(), sig, sig + 1)


def latency_bin(lat):
    """Histogram bin of a latency in cycles (Python reference).

    Bin 0 holds latencies 0..1, bin k (1 <= k <= 14) holds
    ``[2**k, 2**(k+1))`` and bin 15 holds everything ``>= 2**15``.
    """
    if lat < 2:
        return 0
    return min(lat.bit_length() - 1, LatencyTracker.NUM_BINS - 1)


class LatencyTracker(Elaboratable):
    """Latency statistics for in-order request/response pairs.

    A free-running 32-bit cycle counter is captured into a small
    timestamp FIFO (LUTRAM) on every ``start`` pulse and popped on every
    ``done`` pulse. Responses must come back in request order, which
    holds for AXI transactions that all use the same ID. The latency of
    a transaction is the number of clock edges between its ``start``
    cycle and its ``done`` cycle (a response in the cycle after the
    request is latency 1).

    Each latency updates ``lat_max`` and one of 16 saturating 32-bit
    histogram bins: bin 0 counts latencies 0..1, bin k counts
    ``[2**k, 2**(k+1))`` cycles and bin 15 counts ``>= 2**15`` cycles.
    The update is pipelined (3 cycles after ``done``), so a ``clear``
    pulse drops any update still in the pipeline.

    ``clear`` resets the statistics (``lat_max``, the histogram, the
    ``error`` flag and ``outstanding_max``, which restarts from the
    current ``outstanding``). It does not flush the timestamp FIFO, so
    transactions in flight across a clear are still measured correctly.

    If more than ``max_outstanding`` requests are in flight, the extra
    timestamp is not stored and ``error`` is set; a ``done`` with nothing
    in flight also sets ``error``. Latencies measured after an error
    are unreliable until the tracker is reset.

    Parameters
    ----------
    max_outstanding : int
        Maximum number of requests in flight (timestamp FIFO depth).

    Attributes
    ----------
    start : Signal(), in
        Request accepted (e.g. AW handshake).
    done : Signal(), in
        Response accepted (e.g. B handshake).
    clear : Signal(), in
        Pulse: clear the statistics.
    lat_max : Signal(32), out
        Largest latency seen since clear, in cycles.
    outstanding : Signal(range(max_outstanding + 1)), out
        Requests currently in flight.
    outstanding_max : Signal(range(max_outstanding + 1)), out
        High-water mark of ``outstanding`` since clear.
    hist_sel : Signal(4), in
        Histogram bin to read.
    hist_val : Signal(32), out
        Count in bin ``hist_sel`` (registered, 1 cycle behind ``hist_sel``).
    error : Signal(), out
        Sticky: timestamp FIFO overflow or response without a request.
    """
    NUM_BINS = 16

    def __init__(self, max_outstanding=16):
        if max_outstanding < 1:
            raise ValueError('max_outstanding must be >= 1')
        self.max_outstanding = max_outstanding

        self.start = Signal()
        self.done = Signal()
        self.clear = Signal()
        self.lat_max = Signal(32)
        self.outstanding = Signal(range(max_outstanding + 1))
        self.outstanding_max = Signal(range(max_outstanding + 1))
        self.hist_sel = Signal(4)
        self.hist_val = Signal(32)
        self.error = Signal()

    def elaborate(self, platform):
        m = Module()
        n = self.max_outstanding

        now = Signal(32)
        m.d.sync += now.eq(now + 1)

        # Timestamp FIFO. The read port is asynchronous (LUTRAM) so that
        # the head timestamp is available in the cycle of ``done``. When
        # the FIFO is full the write and read pointers are equal; a
        # simultaneous pop + push then reads the old head before the
        # write lands, which is the required order.
        m.submodules.ts = ts = Memory(shape=32, depth=n, init=[])
        wp = ts.write_port()
        rp = ts.read_port(domain='comb')
        wr_ptr = Signal(range(n))
        rd_ptr = Signal(range(n))
        level = self.outstanding

        pop = Signal()
        push = Signal()
        m.d.comb += [
            pop.eq(self.done & (level != 0)),
            push.eq(self.start & ((level != n) | pop)),
            wp.addr.eq(wr_ptr),
            wp.data.eq(now),
            wp.en.eq(push),
            rp.addr.eq(rd_ptr),
        ]
        with m.If(push):
            m.d.sync += wr_ptr.eq(Mux(wr_ptr == n - 1, 0, wr_ptr + 1))
        with m.If(pop):
            m.d.sync += rd_ptr.eq(Mux(rd_ptr == n - 1, 0, rd_ptr + 1))
        with m.If(push & ~pop):
            m.d.sync += level.eq(level + 1)
        with m.Elif(pop & ~push):
            m.d.sync += level.eq(level - 1)

        with m.If((self.start & ~push) | (self.done & ~pop)):
            m.d.sync += self.error.eq(1)

        # Stage 1: latency of the popped transaction
        lat1 = Signal(32)
        lat1_valid = Signal()
        m.d.sync += [
            lat1.eq(now - rp.data),
            lat1_valid.eq(pop),
        ]

        # Stage 2: histogram bin (priority encoder, clamped to 15) and max
        bin1 = Signal(4)
        m.d.comb += bin1.eq(0)
        for k in range(1, self.NUM_BINS):
            with m.If(lat1[k:] != 0):
                m.d.comb += bin1.eq(k)
        bin2 = Signal(4)
        bin2_valid = Signal()
        m.d.sync += [
            bin2.eq(bin1),
            bin2_valid.eq(lat1_valid),
        ]
        with m.If(lat1_valid & (lat1 > self.lat_max)):
            m.d.sync += self.lat_max.eq(lat1)

        # Stage 3: single read-modify-write of the selected bin
        bins = Array(Signal(32, name=f'bin{k}') for k in range(self.NUM_BINS))
        with m.If(bin2_valid):
            m.d.sync += bins[bin2].eq(_sat_inc(bins[bin2]))
        m.d.sync += self.hist_val.eq(bins[self.hist_sel])

        with m.If(level > self.outstanding_max):
            m.d.sync += self.outstanding_max.eq(level)

        with m.If(self.clear):
            m.d.sync += [
                self.lat_max.eq(0),
                self.outstanding_max.eq(level),
                self.error.eq(0),
                lat1_valid.eq(0),
                bin2_valid.eq(0),
            ]
            m.d.sync += [b.eq(0) for b in bins]

        return m

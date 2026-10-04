#
# Fishball radio core - a continuous ring DMA: a stream written round a ring of sub-buffers in DDR
# (the lane ring and the raw IQ capture; hwval's replica of the capture ring).
#
# SPDX-License-Identifier: MIT
#

from math import log2

from amaranth import *

from maia_hdl import axi


class DmaStreamRingWrite(Elaboratable):
    """DMA streaming input -> AXI3, continuous ring buffer.

    Combines ``DmaStreamWrite``'s streaming handshake with ``DmaBRAMWrite``'s
    cyclic address wrapping. Writes continuously, wrapping around
    *num_buffers* sub-buffers. Never stops. Fires ``interrupt`` and updates
    ``last_buffer`` on each sub-buffer completion (from the B-channel side,
    i.e., after the write response is received -- not at the AW issue).

    Parameters
    ----------
    base_address : int
        DDR base address. Must be aligned to the total ring size
        ``(1 << num_buffers_log2) * buffer_size``.
    num_buffers_log2 : int
        log2 of number of sub-buffers (e.g. 3 -> 8 sub-buffers).
    buffer_size : int
        Bytes per sub-buffer. Must produce a power-of-two number of
        128-byte bursts (16 beats x 8 B at width=64).
    width : int
        AXI data width in bits. Default: 64.
    axi_awidth : int
        AXI address width. Default: 32.
    name : Optional[str]
        Name prefix for the AXI3 manager port.

    Attributes
    ----------
    axi : AXI3 Manager interface (write channel only)
    enable : Signal(), in
        When deasserted, the AW channel is gated. Outstanding bursts
        (already accepted by the slave) will complete normally.
    stream_data, stream_valid, stream_ready : AXI4-Stream-like input
    last_buffer : Signal(num_buffers_log2), out
        Index of the most recently *completed* sub-buffer (from B-channel).
        Initialised to -1 (all bits 1) so software's get_new_buffers
        returns empty until at least one sub-buffer has been written.
    interrupt : Signal(), out
        Pulses high for one clock cycle when a sub-buffer completes.
    """

    def __init__(self, base_address, num_buffers_log2, buffer_size,
                 width=64, axi_awidth=32, name=None):
        bytes_per_word_log2 = int(log2(width // 8))
        burst_len_log2 = 4
        addr_shift = burst_len_log2 + bytes_per_word_log2

        num_buffers = 1 << num_buffers_log2
        words_per_buffer = buffer_size >> bytes_per_word_log2
        bursts_per_buffer = words_per_buffer >> burst_len_log2
        if bursts_per_buffer <= 0 or (bursts_per_buffer & (bursts_per_buffer - 1)):
            raise ValueError(
                f'bursts_per_buffer ({bursts_per_buffer}) must be a '
                'positive power of 2')
        bpb_log2 = int(log2(bursts_per_buffer))

        total_size = num_buffers * buffer_size
        if base_address & (total_size - 1):
            raise ValueError(
                f'base_address {base_address:#010x} is not aligned to '
                f'ring size {total_size:#010x}')

        self._base_address = base_address
        self._total_bursts = num_buffers * bursts_per_buffer
        self._addr_shift = addr_shift
        self._bytes_per_word_log2 = bytes_per_word_log2
        self._num_buffers_log2 = num_buffers_log2
        self._bpb_log2 = bpb_log2
        self._axi_awidth = axi_awidth
        self._width = width

        self.axi = axi.AxiInterface(
            axi.AxiDevice.MANAGER,
            [axi.AxiChannel(axi.AxiDirection.WRITE, axi_awidth, width)],
            axi.AxiVersion.AXI3, name=name)
        self.enable = Signal()
        self.stream_data = Signal(width)
        self.stream_valid = Signal()
        self.stream_ready = Signal()
        self.last_buffer = Signal(num_buffers_log2, init=-1)
        self.interrupt = Signal()

    def ports(self):
        return self.axi.ports() + [
            self.enable,
            self.stream_data, self.stream_valid, self.stream_ready,
            self.last_buffer, self.interrupt,
        ]

    def elaborate(self, platform):
        m = Module()

        total_bursts = self._total_bursts
        addr_shift = self._addr_shift
        bpwl = self._bytes_per_word_log2
        burst_len_log2 = 4
        bpb_log2 = self._bpb_log2

        # AW channel: cyclic counter over all bursts in the ring
        aw_counter = Signal(range(total_bursts))
        m.d.comb += [
            self.axi.awaddr.eq(
                (self._base_address + (aw_counter << addr_shift))
                [:self._axi_awidth]),
            self.axi.awlen.eq(2**burst_len_log2 - 1),
            self.axi.awburst.eq(axi.AxiBurst.INCR),
            self.axi.awcache.eq(0b0011),
            self.axi.awprot.eq(0b0000),
            self.axi.awlock.eq(0),
            self.axi.awsize.eq(bpwl),
            self.axi.wstrb.eq(-1),
        ]

        one_outstanding_burst = Signal()
        two_outstanding_bursts = Signal()
        m.d.comb += self.axi.awvalid.eq(
            self.enable & ~two_outstanding_bursts)

        with m.If(self.axi.aw_handshake()):
            m.d.sync += aw_counter.eq(
                Mux(aw_counter == total_bursts - 1, 0, aw_counter + 1))
            with m.If(~(self.axi.w_handshake() & self.axi.wlast)):
                m.d.sync += [
                    one_outstanding_burst.eq(~one_outstanding_burst),
                    two_outstanding_bursts.eq(one_outstanding_burst),
                ]

        # W channel
        beat_counter = Signal(burst_len_log2)
        beat_counter_next = Signal(len(beat_counter) + 1)
        last_beat = beat_counter_next[-1]
        m.d.comb += [
            beat_counter_next.eq(beat_counter + 1),
            self.axi.wlast.eq(last_beat),
            self.axi.wdata.eq(self.stream_data),
        ]
        m.d.sync += self.axi.bready.eq(1)

        # Track outstanding write responses
        max_outstanding_b_log2 = 2
        outstanding_b = Signal(max_outstanding_b_log2 + 2, init=-1)
        no_outstanding_b = outstanding_b[-1]
        full_outstanding_b = ~outstanding_b[-1] & outstanding_b[-2]
        with m.If(self.axi.wlast & self.axi.w_handshake() & ~self.axi.bvalid):
            m.d.sync += outstanding_b.eq(outstanding_b + 1)
        with m.If(self.axi.bvalid & ~(self.axi.wlast & self.axi.w_handshake())):
            m.d.sync += outstanding_b.eq(outstanding_b - 1)

        enable_w = Signal()
        m.d.comb += [
            enable_w.eq((one_outstanding_burst | two_outstanding_bursts)
                        & ~full_outstanding_b),
            self.axi.wvalid.eq(enable_w & self.stream_valid),
            self.stream_ready.eq(enable_w & self.axi.wready),
        ]
        with m.If(self.axi.w_handshake()):
            m.d.sync += beat_counter.eq(beat_counter_next)
            with m.If(self.axi.wlast & ~self.axi.aw_handshake()):
                m.d.sync += [
                    one_outstanding_burst.eq(two_outstanding_bursts),
                    two_outstanding_bursts.eq(0),
                ]

        # B-channel: sub-buffer completion tracking
        b_counter = Signal(range(total_bursts))
        b_at_end = Signal()
        m.d.comb += b_at_end.eq(
            b_counter[:bpb_log2] == ((1 << bpb_log2) - 1))
        m.d.comb += self.interrupt.eq(0)
        with m.If(self.axi.bvalid):
            m.d.sync += b_counter.eq(
                Mux(b_counter == total_bursts - 1, 0, b_counter + 1))
            with m.If(b_at_end):
                m.d.sync += self.last_buffer.eq(self.last_buffer + 1)
                m.d.comb += self.interrupt.eq(1)

        return m
